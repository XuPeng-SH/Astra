//! HTTP-backed team persistence for CLI clients.
//!
//! Team definitions and snapshots belong to the server's cloud authority. The
//! CLI reads and writes that owner directly through the runtime HTTP API, not a
//! second configuration registry or direct MatrixOne access.

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fmt;

use astra_services::team_persistence::{
    CreateTeam, TeamDefinition, TeamPersistenceService, TeamSnapshotRecord, TeamWriteError,
    UpdateTeam,
};

const TEAM_HTTP_TIMEOUT_SECS: u64 = 15;

#[derive(Debug, Deserialize)]
struct TeamListResponse {
    teams: Vec<TeamDefinition>,
}

#[derive(Debug, Deserialize)]
struct SnapshotListResponse {
    snapshots: Vec<SnapshotWire>,
}

#[derive(Debug, Deserialize)]
struct SnapshotWire {
    snapshot_id: String,
    team_id: String,
    team_name: String,
    label: String,
    git_commit: Option<String>,
    session_id: Option<String>,
    team_definition_json: Option<String>,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct DeleteResponse {
    deleted: bool,
}

#[derive(Debug, Serialize)]
struct CreateSnapshotRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
}

pub(crate) struct HttpTeamStore {
    api: astra_thin_client::ThinClient,
    owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
}

#[derive(Debug)]
enum TeamHttpError {
    AuthenticationRequired,
    Network {
        method: &'static str,
        path: String,
        error: String,
    },
    Http {
        method: &'static str,
        path: String,
        status: reqwest::StatusCode,
        body: String,
    },
    Decode {
        method: &'static str,
        path: String,
        error: String,
    },
}

impl TeamHttpError {
    fn write_outcome(self) -> TeamWriteError {
        match self {
            Self::AuthenticationRequired => TeamWriteError::Rejected,
            Self::Http { status, body, .. }
                if status == reqwest::StatusCode::SERVICE_UNAVAILABLE
                    && serde_json::from_str::<astra_core::ErrorResponse>(&body).is_ok_and(
                        |body| body.error_code.as_deref() == Some("team_write_rejected"),
                    ) =>
            {
                TeamWriteError::Rejected
            }
            Self::Http { status, .. } if status == reqwest::StatusCode::CONFLICT => {
                TeamWriteError::ConflictOrMissing
            }
            Self::Http { status, .. }
                if status.is_client_error() && status != reqwest::StatusCode::REQUEST_TIMEOUT =>
            {
                TeamWriteError::Rejected
            }
            Self::Network { .. } | Self::Http { .. } | Self::Decode { .. } => {
                TeamWriteError::Unconfirmed
            }
        }
    }

    fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Http {
                status: reqwest::StatusCode::NOT_FOUND,
                ..
            }
        )
    }
}

impl fmt::Display for TeamHttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AuthenticationRequired => write!(f, "team API requires authentication"),
            Self::Network {
                method,
                path,
                error,
            } => write!(f, "network {method} {path}: {error}"),
            Self::Http {
                method,
                path,
                status,
                body,
            } => write!(f, "team API {method} {path} -> {status}: {body}"),
            Self::Decode {
                method,
                path,
                error,
            } => write!(f, "decode {method} {path}: {error}"),
        }
    }
}

impl HttpTeamStore {
    pub(crate) fn new(api: &astra_thin_client::ThinClient, profile: Option<&str>) -> Self {
        let mut owner = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
        if profile.is_some_and(|profile| owner.profile_name.as_deref() != Some(profile)) {
            owner.server_account_id = None;
        }
        Self {
            api: api.clone(),
            owner,
        }
    }

    pub(crate) fn owner_account_id(&self) -> Option<&str> {
        self.owner
            .server_account_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
    }

    /// UI attachment check only; credentials still come exclusively from the
    /// captured owner at dispatch. This never loads or re-captures a profile.
    pub(crate) fn is_attached_owner(&self) -> bool {
        self.owner_account_id().is_some()
            && self.owner.owner_scope == astra_services::local_owner_scope()
            && match (
                &self.owner.native_binding,
                crate::cli::native_auth::active(),
            ) {
                (Some(bound), Some(active)) => std::sync::Arc::ptr_eq(bound, &active),
                (None, None) => true,
                _ => false,
            }
    }

    async fn access_token(&self) -> Result<String, TeamHttpError> {
        tokio::time::timeout(
            std::time::Duration::from_secs(TEAM_HTTP_TIMEOUT_SECS),
            crate::cli::session::session_runtime::owner_access_token(&self.api, &self.owner),
        )
        .await
        .ok()
        .flatten()
        .ok_or(TeamHttpError::AuthenticationRequired)
    }

    pub(crate) async fn model_catalog(
        &self,
    ) -> Result<Vec<astra_services::ModelListItemResponse>, String> {
        let token = self
            .access_token()
            .await
            .map_err(|error| error.to_string())?;
        tokio::time::timeout(
            std::time::Duration::from_secs(TEAM_HTTP_TIMEOUT_SECS),
            crate::cli::session::session_runtime::fetch_model_catalog(&self.api, Some(&token)),
        )
        .await
        .map_err(|_| "Model catalog request timed out".to_string())?
        .map_err(|error| error.to_string())
    }

    fn require_owner(&self, requested: &str) -> Result<&str, TeamHttpError> {
        self.owner_account_id()
            .filter(|owner| requested.is_empty() || requested == *owner)
            .ok_or(TeamHttpError::AuthenticationRequired)
    }

    async fn request_json<T: DeserializeOwned>(
        &self,
        method: &'static str,
        path: &str,
        request: reqwest::RequestBuilder,
    ) -> Result<T, TeamHttpError> {
        // This bounds Team pre-delivery waiting, not the auth owner's in-flight
        // rotation settlement. No Team request is sent if credential wait ends.
        let token = self.access_token().await?;
        let response = request
            .bearer_auth(token)
            .timeout(std::time::Duration::from_secs(TEAM_HTTP_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|error| TeamHttpError::Network {
                method,
                path: path.to_string(),
                error: error.to_string(),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(TeamHttpError::Http {
                method,
                path: path.to_string(),
                status,
                body: response.text().await.unwrap_or_default(),
            });
        }
        response
            .json::<T>()
            .await
            .map_err(|error| TeamHttpError::Decode {
                method,
                path: path.to_string(),
                error: error.to_string(),
            })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.api.api_origin(), path)
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, TeamHttpError> {
        self.request_json(
            "GET",
            path,
            self.api.http_client().get(self.url(path)).query(query),
        )
        .await
    }

    async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, TeamHttpError> {
        self.request_json(
            "POST",
            path,
            self.api.http_client().post(self.url(path)).json(body),
        )
        .await
    }

    async fn delete_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, TeamHttpError> {
        self.request_json(
            "DELETE",
            path,
            self.api.http_client().delete(self.url(path)),
        )
        .await
    }

    fn team_path_segment(value: &str) -> String {
        urlencoding::encode(value).into_owned()
    }
}

#[async_trait]
impl TeamPersistenceService for HttpTeamStore {
    async fn ensure_builtins(&self, user_id: &str) -> Result<(), String> {
        // The authenticated server endpoint owns idempotent materialization.
        // Reading the collection exercises that owner-scoped boundary without
        // duplicating builtin definitions in the CLI.
        self.list_teams(user_id).await.map(|_| ())
    }

    async fn create_team(
        &self,
        user_id: &str,
        input: &CreateTeam,
    ) -> Result<TeamDefinition, TeamWriteError> {
        let owner = self
            .require_owner(user_id)
            .map_err(TeamHttpError::write_outcome)?;
        let accepted: TeamDefinition = self
            .post_json("/teams", input)
            .await
            .map_err(TeamHttpError::write_outcome)?;
        if accepted.team_id != input.team_id
            || accepted.revision != 1
            || accepted.user_id != owner
            || accepted.name != input.name
            || accepted.description != input.description
            || accepted.members != input.members
            || accepted.context != input.context
        {
            return Err(TeamWriteError::Unconfirmed);
        }
        Ok(accepted)
    }

    async fn update_team(
        &self,
        user_id: &str,
        team_id: &str,
        input: &UpdateTeam,
    ) -> Result<TeamDefinition, TeamWriteError> {
        let owner = self
            .require_owner(user_id)
            .map_err(TeamHttpError::write_outcome)?;
        let path = format!("/teams/{}", Self::team_path_segment(team_id));
        let accepted: TeamDefinition = self
            .request_json(
                "PUT",
                &path,
                self.api.http_client().put(self.url(&path)).json(input),
            )
            .await
            .map_err(TeamHttpError::write_outcome)?;
        if accepted.team_id != team_id
            || Some(accepted.revision) != input.expected_revision.checked_add(1)
            || accepted.user_id != owner
            || accepted.name != input.name
            || accepted.description != input.description
            || accepted.members != input.members
            || accepted.context != input.context
        {
            return Err(TeamWriteError::Unconfirmed);
        }
        Ok(accepted)
    }

    async fn load_team(&self, user_id: &str, name: &str) -> Result<Option<TeamDefinition>, String> {
        self.require_owner(user_id)
            .map_err(|error| error.to_string())?;
        let name = Self::team_path_segment(name);
        match self
            .get_json::<TeamDefinition>(&format!("/teams/name/{name}"), &[])
            .await
        {
            Ok(team) => Ok(Some(team)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn load_team_by_id(
        &self,
        user_id: &str,
        team_id: &str,
    ) -> Result<Option<TeamDefinition>, String> {
        self.require_owner(user_id)
            .map_err(|error| error.to_string())?;
        let team_id = Self::team_path_segment(team_id);
        match self
            .get_json::<TeamDefinition>(&format!("/teams/{team_id}"), &[])
            .await
        {
            Ok(team) => Ok(Some(team)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn list_teams(&self, user_id: &str) -> Result<Vec<TeamDefinition>, String> {
        self.require_owner(user_id)
            .map_err(|error| error.to_string())?;
        let list: TeamListResponse = self
            .get_json("/teams", &[])
            .await
            .map_err(|e| e.to_string())?;
        let mut teams = list.teams;
        teams.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(teams)
    }

    async fn delete_team(&self, user_id: &str, team_id: &str) -> Result<bool, String> {
        self.require_owner(user_id)
            .map_err(|error| error.to_string())?;
        let team_id = Self::team_path_segment(team_id);
        match self
            .delete_json::<DeleteResponse>(&format!("/teams/{team_id}"))
            .await
        {
            Ok(response) => Ok(response.deleted),
            Err(error) if error.is_not_found() => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn save_snapshot(
        &self,
        snapshot: &TeamSnapshotRecord,
    ) -> Result<TeamSnapshotRecord, String> {
        let owner = self
            .require_owner(&snapshot.user_id)
            .map_err(|error| error.to_string())?;
        let body = CreateSnapshotRequest {
            label: (!snapshot.label.is_empty()).then_some(snapshot.label.as_str()),
            git_commit: snapshot.git_commit.as_deref(),
            session_id: snapshot.session_id.as_deref(),
        };
        let accepted: SnapshotWire = self
            .post_json(
                &format!(
                    "/teams/{}/snapshots",
                    Self::team_path_segment(&snapshot.team_id)
                ),
                &body,
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(TeamSnapshotRecord {
            snapshot_id: accepted.snapshot_id,
            team_id: accepted.team_id,
            team_name: accepted.team_name,
            user_id: owner.to_string(),
            label: accepted.label,
            git_commit: accepted.git_commit,
            session_id: accepted.session_id,
            team_definition_json: accepted.team_definition_json,
            created_at: accepted.created_at,
        })
    }

    async fn list_snapshots(
        &self,
        team_id: &str,
        user_id: &str,
        _limit: u32,
    ) -> Result<Vec<TeamSnapshotRecord>, String> {
        let owner = self
            .require_owner(user_id)
            .map_err(|error| error.to_string())?;
        let response: SnapshotListResponse = self
            .get_json(
                &format!("/teams/{}/snapshots", Self::team_path_segment(team_id)),
                &[],
            )
            .await
            .map_err(|e| e.to_string())?;
        Ok(response
            .snapshots
            .into_iter()
            .map(|snapshot| TeamSnapshotRecord {
                snapshot_id: snapshot.snapshot_id,
                team_id: snapshot.team_id,
                team_name: snapshot.team_name,
                user_id: owner.to_string(),
                label: snapshot.label,
                git_commit: snapshot.git_commit,
                session_id: snapshot.session_id,
                team_definition_json: snapshot.team_definition_json,
                created_at: snapshot.created_at,
            })
            .collect())
    }

    async fn find_snapshot(
        &self,
        snapshot_id: &str,
        user_id: &str,
    ) -> Result<Option<TeamSnapshotRecord>, String> {
        let owner = self
            .require_owner(user_id)
            .map_err(|error| error.to_string())?;
        match self
            .get_json::<SnapshotWire>(
                &format!("/teams/snapshots/{}", Self::team_path_segment(snapshot_id)),
                &[],
            )
            .await
        {
            Ok(snapshot) => Ok(Some(TeamSnapshotRecord {
                snapshot_id: snapshot.snapshot_id,
                team_id: snapshot.team_id,
                team_name: snapshot.team_name,
                user_id: owner.to_string(),
                label: snapshot.label,
                git_commit: snapshot.git_commit,
                session_id: snapshot.session_id,
                team_definition_json: snapshot.team_definition_json,
                created_at: snapshot.created_at,
            })),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    async fn delete_snapshot(&self, snapshot_id: &str, user_id: &str) -> Result<bool, String> {
        self.require_owner(user_id)
            .map_err(|error| error.to_string())?;
        match self
            .delete_json::<DeleteResponse>(&format!("/teams/snapshots/{snapshot_id}"))
            .await
        {
            Ok(response) => Ok(response.deleted),
            Err(error) if error.is_not_found() => Ok(false),
            Err(error) => Err(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::HttpTeamStore;
    use astra_credentials::{CredentialsFile, Profile};
    use astra_services::team_persistence::{
        CreateTeam, TeamDefinition, TeamPersistenceService, TeamSnapshotRecord, TeamWriteError,
        UpdateTeam,
    };
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_test_profile() -> crate::cli::cli_config::cli_utils::TestCliProfileIdentityGuard {
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                account_id: Some("user-1".into()),
                access_token: Some("test-token".into()),
                ..Default::default()
            },
        );
        crate::cli::cli_config::cli_utils::save_credentials(&creds).unwrap();
        crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
            "default",
            Some("user-1"),
        )
        .unwrap()
    }

    fn create_input(team: &TeamDefinition) -> CreateTeam {
        CreateTeam {
            team_id: team.team_id.clone(),
            name: team.name.clone(),
            description: team.description.clone(),
            members: team.members.clone(),
            context: team.context.clone(),
        }
    }

    async fn write_team(
        store: &HttpTeamStore,
        team: &TeamDefinition,
        updating: bool,
    ) -> Result<TeamDefinition, TeamWriteError> {
        if updating {
            store
                .update_team(&team.user_id, &team.team_id, &UpdateTeam::from(team))
                .await
        } else {
            store.create_team(&team.user_id, &create_input(team)).await
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn team_writes_preserve_identity_and_exact_payload_without_an_extra_read() {
        let _creds_guard = crate::tests::isolate_credentials();
        let _env_guard = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let _identity = write_test_profile();
        let server = MockServer::start().await;
        let mut requested = astra_services::team_persistence::builtin_teams("user-1").remove(0);
        requested.team_id = "stable-team-id".into();
        requested.name = "name differs from ID".into();
        requested.members[0].can_delegate = true;
        requested.members[0].max_delegation_depth = 2;
        requested.members[0].allow_tools = Some(vec!["read_file".into()]);
        requested.members[0].read_only = true;
        requested.members[0].initial_turns = Some(2);
        requested.members[0].max_turns = Some(5);
        requested.members[0].model_selection = Some(astra_turn_types::ModelSelection {
            offering_id: "fixture-offering".into(),
        });
        requested.members[0].mcp_servers = vec!["fixture-mcp".into()];
        requested
            .context
            .insert("literal key".into(), "Don't change --flags".into());
        let store = HttpTeamStore::new(
            &astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
            None,
        );
        for updating in [false, true] {
            requested.revision = if updating { 7 } else { 1 };
            let mut accepted = requested.clone();
            if updating {
                accepted.revision += 1;
            }
            let (verb, endpoint, body) = if updating {
                (
                    "PUT",
                    "/teams/stable-team-id",
                    serde_json::to_value(UpdateTeam::from(&requested)).unwrap(),
                )
            } else {
                (
                    "POST",
                    "/teams",
                    serde_json::to_value(create_input(&requested)).unwrap(),
                )
            };
            Mock::given(method(verb))
                .and(path(endpoint))
                .and(header("authorization", "Bearer test-token"))
                .and(wiremock::matchers::body_json(body))
                .respond_with(ResponseTemplate::new(200).set_body_json(&accepted))
                .expect(1)
                .mount(&server)
                .await;
            assert_eq!(
                write_team(&store, &requested, updating).await.unwrap(),
                accepted
            );
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                1,
                "one write, no hydration/readback"
            );
            server.verify().await;
            server.reset().await;
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn team_write_errors_and_invalid_acknowledgments_do_not_retry() {
        let _creds_guard = crate::tests::isolate_credentials();
        let _env_guard = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let _identity = write_test_profile();
        let server = MockServer::start().await;
        let store = HttpTeamStore::new(
            &astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
            None,
        );
        let requested = astra_services::team_persistence::builtin_teams("user-1").remove(0);
        for updating in [false, true] {
            let mut accepted = requested.clone();
            if updating {
                accepted.revision += 1;
            }
            let mut cases = vec![
                (ResponseTemplate::new(401), TeamWriteError::Rejected),
                (ResponseTemplate::new(403), TeamWriteError::Rejected),
                (
                    ResponseTemplate::new(409),
                    TeamWriteError::ConflictOrMissing,
                ),
                (ResponseTemplate::new(408), TeamWriteError::Unconfirmed),
                (ResponseTemplate::new(503), TeamWriteError::Unconfirmed),
                (
                    ResponseTemplate::new(503).set_body_json(serde_json::json!({
                        "error_code": "team_write_rejected", "detail": "unavailable"
                    })),
                    TeamWriteError::Rejected,
                ),
                (
                    ResponseTemplate::new(503).set_body_json(serde_json::json!({
                        "error_code": "team_write_unconfirmed", "detail": "unavailable"
                    })),
                    TeamWriteError::Unconfirmed,
                ),
                (
                    ResponseTemplate::new(200).set_body_string("{"),
                    TeamWriteError::Unconfirmed,
                ),
            ];
            for (pointer, value) in [
                ("/team_id", serde_json::json!("replacement-id")),
                ("/revision", serde_json::json!(99)),
                ("/user_id", serde_json::json!("")),
                ("/user_id", serde_json::json!("other-owner")),
                ("/name", serde_json::json!("other-name")),
                ("/description", serde_json::json!("other description")),
                ("/members/0/read_only", serde_json::json!(true)),
                (
                    "/members/0/agent_id",
                    serde_json::json!("replacement-member"),
                ),
                (
                    "/members/0/model_selection",
                    serde_json::json!({"offering_id": "other-offering"}),
                ),
                ("/context", serde_json::json!({"unexpected": "value"})),
            ] {
                let mut body = serde_json::to_value(&accepted).unwrap();
                *body.pointer_mut(pointer).unwrap() = value;
                cases.push((
                    ResponseTemplate::new(200).set_body_json(body),
                    TeamWriteError::Unconfirmed,
                ));
            }
            for (response, expected) in cases {
                let endpoint = if updating {
                    format!("/teams/{}", requested.team_id)
                } else {
                    "/teams".into()
                };
                Mock::given(method(if updating { "PUT" } else { "POST" }))
                    .and(path(&endpoint))
                    .respond_with(response)
                    .expect(1)
                    .mount(&server)
                    .await;
                assert_eq!(
                    write_team(&store, &requested, updating).await,
                    Err(expected)
                );
                assert_eq!(server.received_requests().await.unwrap().len(), 1);
                server.verify().await;
                server.reset().await;
            }
        }
        crate::cli::cli_config::cli_utils::save_credentials(&CredentialsFile::default()).unwrap();
        for updating in [false, true] {
            assert_eq!(
                write_team(&store, &requested, updating).await,
                Err(TeamWriteError::Rejected)
            );
        }
        assert!(
            server.received_requests().await.unwrap().is_empty(),
            "no credentials means no write"
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn interrupted_team_write_is_unconfirmed_without_reconnecting() {
        use tokio::io::AsyncReadExt;
        let _creds_guard = crate::tests::isolate_credentials();
        let _env_guard = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let _identity = write_test_profile();
        let requested = astra_services::team_persistence::builtin_teams("user-1").remove(0);
        for updating in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let api = astra_thin_client::ThinClient::new(
                &format!("http://{}", listener.local_addr().unwrap()),
                None,
            )
            .unwrap();
            let store = HttpTeamStore::new(&api, None);
            let write = write_team(&store, &requested, updating);
            tokio::pin!(write);
            let mut connections = 0;
            let mut peers = tokio::task::JoinSet::new();
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    tokio::select! {
                        result = &mut write => break result,
                        accepted = listener.accept() => {
                            let (mut socket, _) = accepted.unwrap();
                            connections += 1;
                            peers.spawn(async move {
                                let mut buffer = [0; 1024];
                                assert!(socket.read(&mut buffer).await.unwrap() > 0);
                                // Observe the write, then lose the acknowledgment.
                                drop(socket);
                            });
                        }
                    }
                }
            })
            .await
            .expect("transport loss must settle");
            assert_eq!(outcome, Err(TeamWriteError::Unconfirmed));
            assert_eq!(connections, 1, "uncertain writes must not be repeated");
            while let Some(result) = peers.join_next().await {
                result.unwrap();
            }
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn load_team_returns_none_on_404() {
        let _creds_guard = crate::tests::isolate_credentials();
        let _identity = write_test_profile();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/teams/name/missing-team"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .expect(1)
            .mount(&server)
            .await;

        let store = HttpTeamStore::new(
            &astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
            None,
        );
        let team = store.load_team("user-1", "missing-team").await.unwrap();
        assert!(team.is_none());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn load_team_refreshes_credentials_before_dispatch_and_fails_closed() {
        let _creds_guard = crate::tests::isolate_credentials();
        let _identity = crate::cli::cli_config::cli_utils::install_cli_profile_identity_for_test(
            "team-profile",
            Some("user-1"),
        )
        .unwrap();
        let _env_guard = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let expired = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJleHAiOjF9.sig";
        let valid = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJleHAiOjQxMDAwMDAwMDB9.sig";
        let team = astra_services::team_persistence::builtin_teams("user-1").remove(0);
        let team_path = format!("/teams/name/{}", team.name);

        for (access_token, refresh_status, expected_token, operation) in [
            (Some(valid), None, Some(valid)),
            (Some(expired), Some(200), Some("fresh-access")),
            (None, Some(200), Some("fresh-access")),
            (Some(expired), Some(503), None),
            (None, Some(503), None),
        ]
        .into_iter()
        .flat_map(|(access, status, token)| {
            ["snapshot", "context"].map(move |operation| (access, status, token, operation))
        }) {
            let mut credentials = CredentialsFile::default();
            credentials.profiles.insert(
                "default".into(),
                Profile {
                    access_token: Some("other-profile-token".into()),
                    ..Default::default()
                },
            );
            credentials.profiles.insert(
                "team-profile".into(),
                Profile {
                    account_id: Some("user-1".into()),
                    access_token: access_token.map(str::to_string),
                    refresh_token: Some("refresh-old".into()),
                    ..Default::default()
                },
            );
            crate::cli::cli_config::cli_utils::save_credentials(&credentials).unwrap();

            let server = MockServer::start().await;
            let refresh_count = u64::from(refresh_status.is_some());
            let team_count = u64::from(expected_token.is_some());
            Mock::given(method("POST"))
                .and(path("/auth/refresh"))
                .and(wiremock::matchers::body_json(serde_json::json!({
                    "refresh_token": "refresh-old"
                })))
                .respond_with(
                    ResponseTemplate::new(refresh_status.unwrap_or(503)).set_body_json(
                        serde_json::json!({
                            "user_id": "user-1",
                            "access_token": "fresh-access",
                            "refresh_token": "fresh-refresh"
                        }),
                    ),
                )
                .expect(refresh_count)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path(&team_path))
                .and(header(
                    "authorization",
                    format!("Bearer {}", expected_token.unwrap_or("fresh-access")),
                ))
                .respond_with(ResponseTemplate::new(200).set_body_json(&team))
                .expect(team_count)
                .mount(&server)
                .await;

            let mut updated = team.clone();
            updated
                .context
                .insert("responsibility".into(), "coordinate".into());
            let update = UpdateTeam::from(&updated);
            updated.revision += 1;
            let snapshot = TeamSnapshotRecord {
                snapshot_id: "saved-snapshot".into(),
                team_id: team.team_id.clone(),
                team_name: team.name.clone(),
                user_id: team.user_id.clone(),
                label: "saved".into(),
                git_commit: None,
                session_id: None,
                team_definition_json: Some(serde_json::to_string(&team).unwrap()),
                created_at: "2026-10-05T00:00:00Z".into(),
            };
            let (verb, write_path, body, accepted) = if operation == "snapshot" {
                (
                    "POST",
                    format!("/teams/{}/snapshots", team.team_id),
                    serde_json::json!({"label":"saved"}),
                    serde_json::to_value(&snapshot).unwrap(),
                )
            } else {
                (
                    "PUT",
                    format!("/teams/{}", team.team_id),
                    serde_json::to_value(&update).unwrap(),
                    serde_json::to_value(&updated).unwrap(),
                )
            };
            Mock::given(method(verb))
                .and(path(&write_path))
                .and(header(
                    "authorization",
                    format!("Bearer {}", expected_token.unwrap_or("fresh-access")),
                ))
                .and(wiremock::matchers::body_json(body))
                .respond_with(ResponseTemplate::new(200).set_body_json(accepted))
                .expect(team_count)
                .mount(&server)
                .await;
            let store = HttpTeamStore::new(
                &astra_thin_client::ThinClient::new(&server.uri(), None).unwrap(),
                Some("team-profile"),
            );
            let result = store.load_team("user-1", &team.name).await;
            if expected_token.is_some() {
                assert_eq!(
                    serde_json::to_value(result.unwrap().expect("authorized team")).unwrap(),
                    serde_json::to_value(&team).unwrap()
                );
                // One captured owner across the real read/write operation;
                // never construct another store to borrow rotated credentials.
                if operation == "snapshot" {
                    assert_eq!(
                        serde_json::to_value(store.save_snapshot(&snapshot).await.unwrap())
                            .unwrap(),
                        serde_json::to_value(&snapshot).unwrap()
                    );
                } else {
                    assert_eq!(
                        store
                            .update_team("user-1", &team.team_id, &update)
                            .await
                            .unwrap(),
                        updated
                    );
                }
            } else {
                assert_eq!(result.unwrap_err(), "team API requires authentication");
            }
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len() as u64, refresh_count + 2 * team_count);
            if refresh_status.is_some() {
                assert_eq!(requests[0].url.path(), "/auth/refresh");
            }
            if expected_token.is_some() {
                assert_eq!(requests[refresh_count as usize].url.path(), team_path);
                assert_eq!(requests.last().unwrap().url.path(), write_path);
            }
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request.url.path() == team_path)
                    .count() as u64,
                team_count
            );
            let saved = crate::cli::cli_config::cli_utils::load_credentials();
            assert_eq!(
                saved.profiles["team-profile"].access_token.as_deref(),
                expected_token.or(access_token)
            );
            assert_eq!(
                saved.profiles["default"].access_token.as_deref(),
                Some("other-profile-token")
            );
        }
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn delete_snapshot_refreshes_credentials_on_the_shared_transport() {
        let _creds_guard = crate::tests::isolate_credentials();
        let _identity = write_test_profile();
        let server = MockServer::start().await;
        let api =
            astra_thin_client::ThinClient::new(&server.uri(), Some("stale-default-token".into()))
                .unwrap();
        let store = HttpTeamStore::new(&api, None);
        for token in ["first-token", "rotated-token"] {
            let mut creds = CredentialsFile::default();
            creds.profiles.insert(
                "default".into(),
                Profile {
                    account_id: Some("user-1".into()),
                    access_token: Some(token.into()),
                    ..Default::default()
                },
            );
            crate::cli::cli_config::cli_utils::save_credentials(&creds).unwrap();
            assert!(
                store
                    .delete_snapshot("missing-snapshot", "user-1")
                    .await
                    .is_err()
            );
            // A new operation captures the new source; the old operation must
            // never silently borrow a replacement, even for the same account.
            let current = HttpTeamStore::new(&api, None);
            Mock::given(method("DELETE"))
                .and(path("/teams/snapshots/missing-snapshot"))
                .and(header("authorization", format!("Bearer {token}")))
                .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
                .expect(1)
                .mount(&server)
                .await;
            assert!(
                !current
                    .delete_snapshot("missing-snapshot", "user-1")
                    .await
                    .unwrap()
            );
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn queued_team_write_stays_with_captured_profile_and_account() {
        use crate::cli::cli_config::cli_utils::{
            credential_store, install_cli_profile_identity_for_test, save_credentials,
        };
        let _credentials = crate::tests::isolate_credentials();
        let _identity = write_test_profile();
        let _env = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        unsafe {
            std::env::set_var("ASTRA_ACCESS_TOKEN", "other-owner-token");
        }
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let store = HttpTeamStore::new(&api, None);
        let requested = astra_services::team_persistence::builtin_teams("user-1").remove(0);
        let mut credentials = credential_store().load().unwrap();
        credentials.current_profile = Some("other".into());
        credentials.profiles.insert(
            "other".into(),
            Profile {
                account_id: Some("other-owner".into()),
                access_token: Some("other-owner-token".into()),
                ..Default::default()
            },
        );
        save_credentials(&credentials).unwrap();
        let _other = install_cli_profile_identity_for_test("other", Some("other-owner")).unwrap();
        Mock::given(method("POST"))
            .and(path("/teams"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&requested))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            write_team(&store, &requested, false).await.unwrap(),
            requested
        );
        server.verify().await;
        server.reset().await;

        // Login replacement with unchanged profile/account labels still
        // invalidates the queued source, whether access or refresh changed.
        for (access, refresh) in [
            ("replacement-access", None),
            ("test-token", Some("replacement-refresh")),
        ] {
            let profile = credentials.profiles.get_mut("default").unwrap();
            profile.access_token = Some(access.into());
            profile.refresh_token = refresh.map(str::to_owned);
            save_credentials(&credentials).unwrap();
            assert_eq!(
                write_team(&store, &requested, false).await,
                Err(TeamWriteError::Rejected)
            );
            assert!(server.received_requests().await.unwrap().is_empty());
        }

        // Owner arguments, a replaced account, and an unbound identity must all
        // reject before delivery, not borrow current/env credentials or retry.
        assert_eq!(
            store
                .create_team("other-owner", &create_input(&requested))
                .await,
            Err(TeamWriteError::Rejected)
        );
        credentials
            .profiles
            .insert("default".into(), credentials.profiles["other"].clone());
        save_credentials(&credentials).unwrap();
        assert_eq!(
            write_team(&store, &requested, false).await,
            Err(TeamWriteError::Rejected)
        );
        let _unbound = install_cli_profile_identity_for_test("default", None).unwrap();
        let unbound = HttpTeamStore::new(&api, None);
        assert!(unbound.owner_account_id().is_none());
        assert_eq!(
            write_team(&unbound, &requested, false).await,
            Err(TeamWriteError::Rejected)
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn team_refresh_barrier_never_borrows_or_overwrites_replacement_credentials() {
        use crate::cli::cli_config::cli_utils::{
            credential_store, install_cli_profile_identity_for_test, save_credentials,
        };
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let _credentials = crate::tests::isolate_credentials();
        let _env = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        // Switching selection is harmless; replacing the source pair or a
        // foreign refresh response is not. All cases cross a real HTTP await.
        for scenario in [
            "selection",
            "account",
            "same_account_login",
            "foreign_response",
            "timeout",
        ] {
            let _identity = write_test_profile();
            let mut credentials = credential_store().load().unwrap();
            credentials
                .profiles
                .get_mut("default")
                .unwrap()
                .access_token = Some("eyJhbGciOiJub25lIn0.eyJleHAiOjF9.sig".into());
            credentials
                .profiles
                .get_mut("default")
                .unwrap()
                .refresh_token = Some("refresh-a".into());
            credentials.profiles.insert(
                "other".into(),
                Profile {
                    account_id: Some("other-owner".into()),
                    access_token: Some("access-b".into()),
                    refresh_token: Some("refresh-b".into()),
                    ..Default::default()
                },
            );
            save_credentials(&credentials).unwrap();
            let requested = astra_services::team_persistence::builtin_teams("user-1").remove(0);
            let (entered, release) = (
                Arc::new(tokio::sync::Notify::new()),
                Arc::new(tokio::sync::Notify::new()),
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let writes = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(AtomicUsize::new(0));
            let requests_seen = requests.clone();
            let (started, proceed, refresh_calls) =
                (entered.clone(), release.clone(), calls.clone());
            let team_calls = writes.clone();
            let accepted = requested.clone();
            let app = axum::Router::new()
                    .route("/auth/refresh", axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                        let (started, proceed, calls) = (started.clone(), proceed.clone(), refresh_calls.clone());
                        async move {
                            assert_eq!(body, serde_json::json!({"refresh_token":"refresh-a"}));
                            calls.fetch_add(1, Ordering::SeqCst);
                            started.notify_one();
                            proceed.notified().await;
                            axum::Json(serde_json::json!({"user_id": if scenario == "foreign_response" {"other-owner"} else {"user-1"}, "access_token":"fresh-a", "refresh_token":"rotated-a"}))
                        }
                    }))
                    .route("/teams", axum::routing::post(move |headers: axum::http::HeaderMap| {
                        let (calls, accepted) = (team_calls.clone(), accepted.clone());
                        async move {
                            assert_eq!(headers["authorization"], "Bearer fresh-a");
                            calls.fetch_add(1, Ordering::SeqCst);
                            axum::Json(accepted)
                        }
                    }))
                    .layer(axum::middleware::from_fn(move |request: axum::extract::Request, next: axum::middleware::Next| {
                        let requests = requests_seen.clone();
                        async move {
                            requests.fetch_add(1, Ordering::SeqCst);
                            next.run(request).await
                        }
                    }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let api = astra_thin_client::ThinClient::new(
                &format!("http://{}", listener.local_addr().unwrap()),
                None,
            )
            .unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let store = HttpTeamStore::new(&api, None);
            let write = write_team(&store, &requested, false);
            tokio::pin!(write);
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::select! {
                    result = &mut write => panic!("write settled before refresh barrier: {result:?}"),
                    () = entered.notified() => {}
                }
            })
            .await
            .expect("refresh entered");
            credentials.current_profile = Some("other".into());
            if scenario == "account" {
                credentials
                    .profiles
                    .insert("default".into(), credentials.profiles["other"].clone());
            } else if scenario == "same_account_login" {
                let source = credentials.profiles.get_mut("default").unwrap();
                source.access_token = Some("replacement-a".into());
                source.refresh_token = Some("replacement-refresh-a".into());
            }
            save_credentials(&credentials).unwrap();
            let _other =
                install_cli_profile_identity_for_test("other", Some("other-owner")).unwrap();
            let outcome = if scenario == "timeout" {
                // The actual HTTP refresh is already blocked at the barrier.
                // Expire only the Team wait, then let authentication settle.
                tokio::time::pause();
                tokio::time::advance(std::time::Duration::from_secs(
                    super::TEAM_HTTP_TIMEOUT_SECS + 1,
                ))
                .await;
                let outcome = (&mut write).await;
                tokio::time::resume();
                outcome
            } else {
                release.notify_one();
                tokio::time::timeout(std::time::Duration::from_secs(2), &mut write)
                    .await
                    .expect("write settles")
            };
            if scenario == "selection" {
                assert_eq!(outcome, Ok(requested.clone()));
            } else {
                assert_eq!(outcome, Err(TeamWriteError::Rejected));
            }
            if scenario == "timeout" {
                assert_eq!(
                    writes.load(Ordering::SeqCst),
                    0,
                    "timed-out request was never posted"
                );
                assert!(
                    store.owner.legacy_pair.try_lock().is_err(),
                    "settlement retains the auth lock"
                );
                // An explicit new request shares the same owner and must wait
                // for its already-running rotation, not retry the old token.
                let next = write_team(&store, &requested, false);
                tokio::pin!(next);
                tokio::select! {
                    biased;
                    result = &mut next => panic!("second request escaped pending rotation: {result:?}"),
                    () = tokio::task::yield_now() => {}
                }
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                release.notify_one();
                assert_eq!(
                    tokio::time::timeout(std::time::Duration::from_secs(2), &mut next)
                        .await
                        .expect("same-owner request resumes after rotation"),
                    Ok(requested.clone())
                );
            }
            let saved = credential_store().load().unwrap();
            let rotated = matches!(scenario, "selection" | "timeout");
            let pair = store
                .owner
                .legacy_pair
                .try_lock()
                .expect("auth lock ends before Team completion");
            assert_eq!(
                pair.0.as_deref(),
                Some(if rotated {
                    "fresh-a"
                } else {
                    "eyJhbGciOiJub25lIn0.eyJleHAiOjF9.sig"
                })
            );
            assert_eq!(
                pair.1.as_deref(),
                Some(if rotated { "rotated-a" } else { "refresh-a" })
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(writes.load(Ordering::SeqCst), usize::from(rotated));
            assert_eq!(
                requests.load(Ordering::SeqCst),
                1 + usize::from(rotated),
                "no auth probes, readback, or retries"
            );
            for name in ["default", "other"] {
                let expected = &credentials.profiles[name];
                let actual = &saved.profiles[name];
                assert_eq!(actual.account_id, expected.account_id);
                assert_eq!(
                    actual.access_token.as_deref(),
                    if matches!(scenario, "selection" | "timeout") && name == "default" {
                        Some("fresh-a")
                    } else {
                        expected.access_token.as_deref()
                    }
                );
                assert_eq!(
                    actual.refresh_token.as_deref(),
                    if matches!(scenario, "selection" | "timeout") && name == "default" {
                        Some("rotated-a")
                    } else {
                        expected.refresh_token.as_deref()
                    }
                );
            }
            server.abort();
        }
    }
}
