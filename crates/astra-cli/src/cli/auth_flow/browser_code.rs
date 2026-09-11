//! One-time authorization codes delivered exclusively through the local browser.
//! There is no remote approval polling or public login-ticket retrieval path.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

pub(super) fn verifier() -> String {
    let mut bytes = [0_u8; 32];
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(super) fn append_capability(url: &str, secret: &str) -> Result<String, String> {
    let mut url = url::Url::parse(url).map_err(|_| "Invalid website login URL")?;
    url.query_pairs_mut()
        .append_pair("callback_transport", "authorization_code_v1")
        .append_pair("code_challenge_method", "S256")
        .append_pair(
            "code_challenge",
            &URL_SAFE_NO_PAD.encode(Sha256::digest(secret.as_bytes())),
        );
    Ok(url.into())
}

pub(super) fn callback_code(target: &str, expected_state: &str) -> Result<String, String> {
    // Only an origin-form request target on our fixed callback path is accepted.
    if !target.starts_with("/callback?") || target.contains('#') {
        return Err("Invalid callback".into());
    }
    let url =
        url::Url::parse(&format!("http://127.0.0.1{target}")).map_err(|_| "Invalid callback")?;
    let mut state = None;
    let mut code = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" if state.is_none() => state = Some(value.into_owned()),
            "code" if code.is_none() => code = Some(value.into_owned()),
            _ => return Err("Invalid callback parameters".into()),
        }
    }
    if !super::constant_time_eq(
        state.as_deref().unwrap_or_default().as_bytes(),
        expected_state.as_bytes(),
    ) {
        return Err("Invalid callback state".into());
    }
    let code = code.ok_or("Missing authorization code")?;
    if code.len() != 43 || !URL_SAFE_NO_PAD.decode(&code).is_ok_and(|v| v.len() == 32) {
        return Err("Invalid authorization code".into());
    }
    Ok(code)
}

pub(super) async fn redeem(
    website: &str,
    code: &str,
    secret: &str,
    port: u16,
    state: &str,
) -> Result<String, String> {
    super::validate_login_website(website)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| "Could not initialize login code exchange")?;
    let mut response = client
        .post(format!(
            "{}/api/auth/astra/browser-login/redeem",
            website.trim_end_matches('/')
        ))
        .json(
            &serde_json::json!({"authorization_code":code,"code_verifier":secret,
            "redirect_uri":format!("http://127.0.0.1:{port}/callback"),"state":state}),
        )
        .send()
        .await
        .map_err(|_| "Login code exchange failed; run astra login again")?;
    if !response.status().is_success() {
        return Err(match response.status().as_u16() {
            403 => "Login code is invalid, expired, or no longer authorized; run astra login again",
            409 => "Login code has already been used; run astra login again",
            429 => "Login is rate limited; wait before running astra login again",
            _ => "Login code exchange is unavailable; run astra login again",
        }
        .into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "Login response was interrupted; run astra login again")?
    {
        if bytes.len() + chunk.len() > 8192 {
            return Err("Login response is too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    #[derive(Deserialize)]
    struct Redeemed {
        connection_key: String,
    }
    let result: Redeemed =
        serde_json::from_slice(&bytes).map_err(|_| "Invalid login code response")?;
    if result.connection_key.is_empty() || result.connection_key.len() > 4096 {
        return Err("Invalid login credential".into());
    }
    Ok(result.connection_key)
}

pub(super) async fn write_result(stream: &mut tokio::net::TcpStream, success: bool) {
    use tokio::io::AsyncWriteExt;
    let (status, body) = if success {
        (
            "200 OK",
            "You are signed in to Astra. You can close this tab and return to your terminal.",
        )
    } else {
        (
            "400 Bad Request",
            "Astra could not complete this login. Return to your terminal and run astra login again.",
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'none'; frame-ancestors 'none'\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[test]
    fn capability_is_explicit_and_never_exposes_verifier() {
        let secret = verifier();
        let url = append_capability(
            "https://example.com/connect/astra?port=1234&state=state",
            &secret,
        )
        .unwrap();
        assert!(!url.contains(&secret));
        let url = url::Url::parse(&url).unwrap();
        let fields: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(fields["code_challenge_method"], "S256");
        assert_eq!(fields["callback_transport"], "authorization_code_v1");
        assert_eq!(
            fields["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(secret.as_bytes()))
        );
    }

    #[test]
    fn callback_requires_matching_state_and_unambiguous_code() {
        let code = verifier();
        assert_eq!(
            callback_code(&format!("/callback?state=expected&code={code}"), "expected").unwrap(),
            code
        );
        for target in [
            format!("/callback?state=wrong&code={code}"),
            format!("/callback?state=expected&state=expected&code={code}"),
            format!("/callback?state=expected&code={code}&code={code}"),
            format!("http://evil.invalid/callback?state=expected&code={code}"),
            "/callback?state=expected&code=short".into(),
            format!("/callback?state=expected&code={code}#fragment"),
        ] {
            assert!(callback_code(&target, "expected").is_err());
        }
    }

    #[tokio::test]
    async fn exchange_rejects_malformed_and_oversized_success_without_retry() {
        for body in [
            "<html>gateway fallback</html>".to_string(),
            "{}".into(),
            serde_json::json!({"connection_key":""}).to_string(),
            serde_json::json!({"connection_key":"x".repeat(4097)}).to_string(),
            serde_json::json!({"connection_key":"test-key","padding":"x".repeat(8192)}).to_string(),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/auth/astra/browser-login/redeem"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .expect(1)
                .mount(&server)
                .await;
            assert!(
                redeem(&server.uri(), "code", "secret", 1234, "state")
                    .await
                    .is_err()
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn exchange_is_bounded_non_redirecting_and_never_replayed() {
        for status in [200, 302, 403, 409, 429, 503] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/api/auth/astra/browser-login/redeem"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .set_body_json(serde_json::json!({"connection_key":"test-key"}))
                        .insert_header("Location", format!("{}/redirect", server.uri())),
                )
                .expect(1)
                .mount(&server)
                .await;
            let result = redeem(&server.uri(), "code", "secret", 1234, "nonce").await;
            assert_eq!(result.is_ok(), status == 200);
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(
                requests[0].body_json::<serde_json::Value>().unwrap(),
                serde_json::json!({"authorization_code":"code","code_verifier":"secret","redirect_uri":"http://127.0.0.1:1234/callback","state":"nonce"})
            );
        }
    }
}
