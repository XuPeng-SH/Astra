//! Provider HTTP construction. One send must not hide another inference request.
//!
//! The caller owns admitted retries and per-attempt deadlines. This module owns
//! connection reuse and disables HTTP retries and redirects beneath that caller.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use astra_core::{ClassifiedError, ErrorKind};

use super::client::{llm_connect_timeout, llm_total_budget};

#[derive(Default)]
struct ProviderClientCache {
    client: OnceLock<reqwest::Client>,
    initialization: Mutex<()>,
}

impl ProviderClientCache {
    fn get_or_try_init(
        &self,
        initialize: impl FnOnce() -> Result<reqwest::Client, ClassifiedError>,
    ) -> Result<&reqwest::Client, ClassifiedError> {
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        // Initialization never spans provider I/O. Cache only a successful
        // construction so a subsequent call can recover after local repair.
        let _guard = self
            .initialization
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(client) = self.client.get() {
            return Ok(client);
        }
        let client = initialize()?;
        Ok(self.client.get_or_init(|| client))
    }
}

fn build_provider_client(
    builder: reqwest::ClientBuilder,
    connect_timeout: Duration,
    total_timeout: Duration,
    pool_idle: usize,
) -> Result<reqwest::Client, ClassifiedError> {
    builder
        .connect_timeout(connect_timeout)
        .timeout(total_timeout)
        .pool_max_idle_per_host(pool_idle)
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| {
            // Builder errors can contain local configuration. Return a typed,
            // content-free diagnostic without replacing the configured client.
            ClassifiedError::new(
                ErrorKind::Unknown,
                "Provider HTTP transport initialization failed; repair local transport configuration and retry",
            )
            .with_details_json(
                serde_json::json!({
                    "code": "provider_transport_initialization_failed",
                    "stage": "transport_initialization",
                    "retry_safety": "none",
                })
                .to_string(),
            )
        })
}

pub(crate) fn global_llm_client() -> Result<&'static reqwest::Client, ClassifiedError> {
    static CACHE: ProviderClientCache = ProviderClientCache {
        client: OnceLock::new(),
        initialization: Mutex::new(()),
    };
    CACHE.get_or_try_init(|| {
        let connect = llm_connect_timeout();
        // This backstop remains above the logical call budget. Request-level
        // timeouts and the coordinator's settlement reserve stay authoritative.
        let total = llm_total_budget().saturating_add(Duration::from_secs(60));
        let pool_idle = std::env::var("ASTRA_LLM_POOL_MAX_IDLE_PER_HOST")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4);
        let builder = astra_core::net::apply_env_proxy(reqwest::Client::builder());
        let client = build_provider_client(builder, connect, total, pool_idle)?;
        tracing::info!(
            target: "astra_runtime::llm_client",
            pool_max_idle_per_host = pool_idle,
            connect_timeout_s = connect.as_secs(),
            total_timeout_s = total.as_secs(),
            "global LLM HTTP client built"
        );
        Ok(client)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::body::Bytes;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::post;

    use super::*;

    struct LocalProvider {
        url: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl LocalProvider {
        async fn start(app: Router) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind local provider");
            let address = listener.local_addr().expect("provider address");
            Self {
                url: format!("http://{address}"),
                task: tokio::spawn(async move {
                    axum::serve(listener, app).await.expect("serve provider");
                }),
            }
        }
    }

    impl Drop for LocalProvider {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn local_client(timeout: Duration) -> reqwest::Client {
        build_provider_client(
            reqwest::Client::builder().no_proxy(),
            Duration::from_secs(1),
            timeout,
            4,
        )
        .expect("build local provider client")
    }

    #[tokio::test]
    async fn inference_redirects_never_resend_body_or_authorization() {
        for status in [
            StatusCode::TEMPORARY_REDIRECT,
            StatusCode::PERMANENT_REDIRECT,
        ] {
            let original_hits = Arc::new(AtomicUsize::new(0));
            let redirected_hits = Arc::new(AtomicUsize::new(0));
            let forwarded_auth = Arc::new(AtomicUsize::new(0));
            let app = Router::new()
                .route(
                    "/inference",
                    post({
                        let hits = original_hits.clone();
                        move || {
                            hits.fetch_add(1, Ordering::SeqCst);
                            async move { (status, [("location", "/redirected")]) }
                        }
                    }),
                )
                .route(
                    "/redirected",
                    post({
                        let hits = redirected_hits.clone();
                        let auth = forwarded_auth.clone();
                        move |headers: HeaderMap| {
                            hits.fetch_add(1, Ordering::SeqCst);
                            if headers.contains_key("authorization") {
                                auth.fetch_add(1, Ordering::SeqCst);
                            }
                            async { StatusCode::OK }
                        }
                    }),
                );
            let provider = LocalProvider::start(app).await;
            let response = local_client(Duration::from_secs(2))
                .post(format!("{}/inference", provider.url))
                .bearer_auth("test-canary-credential")
                .body(r#"{"model":"test","messages":[]}"#)
                .send()
                .await
                .expect("receive redirect response");
            assert_eq!(response.status(), status);
            assert_eq!(original_hits.load(Ordering::SeqCst), 1);
            assert_eq!(redirected_hits.load(Ordering::SeqCst), 0);
            assert_eq!(forwarded_auth.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn ordinary_inference_preserves_exact_bytes_across_reused_client() {
        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/echo",
            post({
                let hits = hits.clone();
                move |body: Bytes| {
                    hits.fetch_add(1, Ordering::SeqCst);
                    async move { body }
                }
            }),
        );
        let provider = LocalProvider::start(app).await;
        let client = local_client(Duration::from_secs(2));
        let exact_body = Bytes::from_static(b"{ \"messages\": [], \"model\": \"test\" }\n");
        for _ in 0..2 {
            let response = client
                .post(format!("{}/echo", provider.url))
                .body(exact_body.clone())
                .send()
                .await
                .expect("send provider request")
                .bytes()
                .await
                .expect("read provider response");
            assert_eq!(response, exact_body);
        }
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn request_deadline_and_client_backstop_bound_stalled_provider() {
        let deadline = Duration::from_secs(120);
        for (case, client_timeout, request_timeout) in [
            ("request deadline", Duration::from_secs(600), Some(deadline)),
            ("client backstop", deadline, None),
        ] {
            let (accepted, mut acceptance) = tokio::sync::mpsc::unbounded_channel();
            let app = Router::new().route(
                "/pending",
                post(move || {
                    accepted.send(()).expect("acceptance observer is present");
                    async {
                        std::future::pending::<()>().await;
                        StatusCode::OK
                    }
                }),
            );
            let provider = LocalProvider::start(app).await;
            let client = build_provider_client(
                reqwest::Client::builder().no_proxy(),
                Duration::from_secs(30),
                client_timeout,
                4,
            )
            .expect("build provider client");
            let mut request = client.post(format!("{}/pending", provider.url));
            if let Some(timeout) = request_timeout {
                request = request.timeout(timeout);
            }
            let mut sending = tokio::spawn(async move { request.send().await });
            // Establish actual provider acceptance with the real clock before
            // advancing timers. A connect failure cannot satisfy this test.
            tokio::select! {
                result = tokio::time::timeout(Duration::from_secs(30), acceptance.recv()) => {
                    assert_eq!(result.expect("provider must accept request"), Some(()));
                }
                result = &mut sending => {
                    panic!("{case} completed before provider acceptance: {result:?}");
                }
            }
            assert!(!sending.is_finished(), "provider must still be stalled");
            tokio::time::pause();
            tokio::time::advance(deadline + Duration::from_secs(1)).await;
            // Bound the observation too: otherwise paused-time auto-advance
            // could let the 600-second backstop hide a broken request override.
            let result = tokio::time::timeout(Duration::from_secs(1), &mut sending).await;
            tokio::time::resume();
            let error = result
                .expect("configured deadline must have elapsed")
                .expect("provider request task completes")
                .expect_err("stalled provider must hit its configured deadline");
            assert!(error.is_timeout(), "{case}: {error}");
        }
    }

    #[test]
    fn initialization_failure_is_typed_and_repairable_without_replacing_success() {
        let cache = ProviderClientCache::default();
        let sensitive_configuration =
            "invalid\nhttps://canary-user:canary-secret@canary.invalid/private-ca.pem";
        let error = cache
            .get_or_try_init(|| {
                build_provider_client(
                    reqwest::Client::builder()
                        .no_proxy()
                        .user_agent(sensitive_configuration),
                    Duration::from_secs(1),
                    Duration::from_secs(2),
                    4,
                )
            })
            .expect_err("invalid configuration must fail closed");
        assert_eq!(error.kind, ErrorKind::Unknown);
        assert!(!error.kind.is_retryable());
        let details: serde_json::Value = serde_json::from_str(
            error
                .details_json
                .as_deref()
                .expect("initialization details"),
        )
        .expect("structured initialization failure");
        assert_eq!(details["code"], "provider_transport_initialization_failed");
        assert_eq!(details["stage"], "transport_initialization");
        assert_eq!(details["retry_safety"], "none");
        for private_value in [
            "canary-user",
            "canary-secret",
            "canary.invalid",
            "private-ca.pem",
        ] {
            assert!(!error.to_string().contains(private_value));
            assert!(!format!("{error:?}").contains(private_value));
        }
        assert!(cache.client.get().is_none());
        let repaired = cache
            .get_or_try_init(|| Ok(local_client(Duration::from_secs(2))))
            .expect("repaired configuration initializes");
        let reused = cache
            .get_or_try_init(|| panic!("successful initialization must be reused"))
            .expect("reuse client");
        assert!(std::ptr::eq(repaired, reused));
    }
}
