//! CLI hosting of the shared Edge delivery owner. Bootstrap permission is
//! resolved once against a frozen declaration, not inferred from discovery.

use super::provider_interaction::NativeInvocationInteractionGate;
use crate::cli::{
    chat_stream,
    permission_manager::{GateOutcome, PermissionManager},
};
use crate::edge_tools::{
    ToolExecutor,
    native_codex::{self, ApprovedNativeRuntime},
};
use astra_edge::{EdgeConnectionContext, EdgeInvocation, EdgeInvocationExecutor};
use astra_server_types::edge_ws_protocol::EdgeClientMessage;
use astra_tools::{
    ToolResult,
    tool_engine::{ToolInvocationAdmissionSource, ToolInvocationMetadata},
};
use astra_turn_types::{
    ProviderBindingRef, ProviderDiscoverySnapshot, ProviderIdentity, ProviderProtocolId,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;

/// Held by the existing CLI lifecycle, never by a root-turn future. Dropping
/// the handle requests cancellation; the task still owns settlement/join.
pub(crate) struct NativeDeliveryHandle {
    cancellation: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for NativeDeliveryHandle {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl NativeDeliveryHandle {
    pub(crate) async fn shutdown(mut self) {
        self.cancellation.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// All identities come from the selected authenticated CLI boundary. The
/// journal path is allocated by the existing local state owner, not the model.
pub(crate) struct NativeDeliveryConfig {
    pub(crate) websocket_url: String,
    pub(crate) api: astra_thin_client::ThinClient,
    pub(crate) auth: String,
    pub(crate) account_id: String,
    pub(crate) edge_agent_id: String,
    pub(crate) edge_transport_id: String,
    pub(crate) workspace_id: Option<String>,
    pub(crate) materialization_id: String,
    pub(crate) journal_path: PathBuf,
    pub(crate) executor: Arc<ToolExecutor>,
    pub(crate) ask_user_request_tx: Option<chat_stream::AskUserRequestTx>,
}

fn bootstrap_args(snapshot: &ProviderDiscoverySnapshot, path: &str) -> Value {
    json!({"directory": path, "provider_snapshot_hash": snapshot.content_hash,
        "provider_binding": snapshot.binding_ref, "access": "native_runtime_read"})
}

fn discovery_snapshot(
    edge_agent_id: &str,
    materialization_id: &str,
    canonical_root: &str,
    declaration: astra_turn_types::ProviderToolDeclaration,
) -> Result<ProviderDiscoverySnapshot, String> {
    ProviderDiscoverySnapshot::new(
        ProviderIdentity::new(edge_agent_id).map_err(|_| "invalid provider identity")?,
        ProviderBindingRef::new(
            astra_services::SessionExecutionBindingV1::edge_materialization_physical_identity(
                materialization_id,
                canonical_root,
            ),
        )
        .map_err(|_| "invalid provider binding")?,
        ProviderProtocolId::new("cli-local").map_err(|_| "invalid provider protocol")?,
        vec![declaration],
    )
    .map_err(|_| "invalid native declaration".into())
}

/// One UI request for the complete dependency set. Each constituent path still
/// goes through the existing evaluator/audit owner; hard denies win over UI.
async fn approve_bootstrap(
    executor: &ToolExecutor,
    snapshot: ProviderDiscoverySnapshot,
    pm: &mut PermissionManager,
    approval_tx: Option<&chat_stream::ApprovalRequestTx>,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<ApprovedNativeRuntime, String> {
    if cancellation.is_cancelled() || Instant::now() >= deadline {
        return Err("native bootstrap admission cancelled or expired".into());
    }
    let declaration = snapshot
        .tool_declarations
        .first()
        .ok_or("missing native declaration")?;
    let requirements = astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(
        &declaration.extension_fields,
    )
    .map_err(|_| "invalid native runtime requirements")?
    .ok_or("missing native runtime requirements")?;
    let root = executor
        .effective_project_root()
        .canonicalize()
        .map_err(|_| "native workspace is unavailable")?;
    let name = "sandbox_expand:native_codex";
    let mut needs_approval = false;
    for path in &requirements.read_paths {
        if astra_sandbox::is_never_readable_path(std::path::Path::new(path)) {
            return Err("native bootstrap contains a forbidden path".into());
        }
        match crate::tool_safety_guard::ToolSafetyGuard::check_request(
            Some(pm),
            name,
            &bootstrap_args(&snapshot, path),
        ) {
            GateOutcome::Allow => {}
            GateOutcome::Deny(_) => return Err("native bootstrap denied by local policy".into()),
            GateOutcome::NeedApproval { .. } => needs_approval = true,
        }
    }
    let source = if needs_approval {
        if cancellation.is_cancelled() || Instant::now() >= deadline {
            return Err("native bootstrap admission cancelled or expired".into());
        }
        let tx = approval_tx.ok_or("native bootstrap requires an approval consumer")?;
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        let args = json!({"provider_snapshot_hash": snapshot.content_hash,
            "provider_binding": snapshot.binding_ref, "executable": requirements.executable,
            "read_paths": requirements.read_paths, "access": "native_runtime_read"});
        chat_stream::enqueue_interactive_request(tx, chat_stream::ApprovalRequest::bare(
            name.into(), "Allow native Codex runtime reads?".into(),
            Some(requirements.read_paths.join("\n")),
            "These exact executable/platform paths become readable by the native collaborator; workspace, network and sensitive-path restrictions remain in force.".into(),
            args, response_tx)).map_err(|_| "native bootstrap approval consumer unavailable")?;
        let approved = tokio::select! {
            biased;
            _ = cancellation.cancelled() => false,
            result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), response_rx) =>
                result.ok().and_then(Result::ok).is_some_and(|response| response.is_approved()),
        };
        for path in &requirements.read_paths {
            pm.record_approval(name, Some(&bootstrap_args(&snapshot, path)), approved);
        }
        if !approved {
            return Err("native bootstrap approval denied, cancelled or expired".into());
        }
        ToolInvocationAdmissionSource::ParentApproval
    } else {
        ToolInvocationAdmissionSource::Policy
    };
    if cancellation.is_cancelled() || Instant::now() >= deadline {
        return Err("native bootstrap admission cancelled or expired".into());
    }
    Ok(ApprovedNativeRuntime {
        snapshot,
        requirements,
        workspace_root: root,
        admission_source: source,
    })
}

struct CliNativeExecutor {
    config: Arc<NativeDeliveryConfig>,
    approval: Arc<ApprovedNativeRuntime>,
}

fn rejected(reason: &str) -> ToolResult {
    let mut metadata = serde_json::Map::new();
    astra_tools::execution_outcome::insert_not_executed_fact(&mut metadata);
    metadata.insert("workspace_effect_settled".into(), json!(true));
    ToolResult {
        output: reason.into(),
        is_error: true,
        metadata: Some(metadata),
        exit_semantics: None,
    }
}

impl EdgeInvocationExecutor for CliNativeExecutor {
    fn execute(
        &self,
        invocation: EdgeInvocation,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, ToolResult> {
        Box::pin(async move {
            let config = &self.config;
            let Some(ceiling) = invocation.execution_ceiling.as_deref() else {
                return rejected("CLI native delivery requires a frozen execution grant");
            };
            if invocation.tool != native_codex::TOOL_NAME
                || invocation.identity.user_id != config.account_id
                || ceiling.workspace_root != self.approval.workspace_root.to_string_lossy()
                || ceiling.workspace_id != config.workspace_id
                || ceiling.materialization_id.as_deref() != Some(config.materialization_id.as_str())
                || ceiling.execution_binding_generation == 0
                || invocation.runtime_process_authorization.is_some()
            {
                return rejected("CLI native delivery does not match the selected boundary");
            }
            let gate = NativeInvocationInteractionGate {
                api: config.api.clone(),
                auth: config.auth.clone(),
                edge_transport_id: config.edge_transport_id.clone(),
                edge_agent_id: config.edge_agent_id.clone(),
                identity: invocation.identity.clone(),
                deadline: invocation.execution_deadline,
                ask_user_request_tx: config.ask_user_request_tx.clone(),
            };
            let metadata = ToolInvocationMetadata {
                admission_deadline: Some(invocation.execution_deadline),
                run_id: Some(&invocation.identity.run_id),
                turn_chain_id: Some(&invocation.identity.turn_chain_id),
                tool_call_id: Some(&invocation.identity.invocation_id),
                admission_source: Some(self.approval.admission_source),
                command_timeout_cap_ms: invocation.command_timeout_cap_ms,
                ..ToolInvocationMetadata::default()
            };
            let outcome = config
                .executor
                .execute_native_codex_provider_invocation(
                    &invocation.args,
                    metadata,
                    Some(&cancel),
                    &gate,
                    ceiling,
                    Some(&self.approval),
                )
                .await;
            ToolResult {
                output: outcome.output,
                is_error: outcome.is_error,
                metadata: outcome.tool_result_fields,
                exit_semantics: None,
            }
        })
    }
}

fn capabilities(
    config: &NativeDeliveryConfig,
    snapshot: Option<&ProviderDiscoverySnapshot>,
) -> Value {
    let mut value = astra_thin_client::edge_runtime_environment_capabilities(
        &config.edge_agent_id,
        config.executor.effective_project_root().to_string_lossy(),
    );
    value["provider_discovery"] = json!(snapshot.into_iter().collect::<Vec<_>>());
    value
}

async fn publish(
    config: &NativeDeliveryConfig,
    snapshot: Option<&ProviderDiscoverySnapshot>,
) -> Result<(), String> {
    let mut body = astra_thin_client::EdgeRegisterRequest::new(&config.edge_agent_id);
    body.worktree_path = Some(
        config
            .executor
            .effective_project_root()
            .to_string_lossy()
            .into_owned(),
    );
    body.materialization_id = Some(config.materialization_id.clone());
    body.capabilities = Some(capabilities(config, snapshot));
    config
        .api
        .post_agents_edge_register(Some(&config.auth), Some(&config.edge_transport_id), &body)
        .await
        .map_err(|_| "native capacity registration failed".to_string())?;
    Ok(())
}

/// Called by the process-scoped CLI lifecycle after constructing the real
/// executor and UI channels. No standalone model-tool projection is installed.
pub(crate) async fn install_native_delivery(
    config: NativeDeliveryConfig,
    pm: &mut PermissionManager,
    approval_tx: Option<&chat_stream::ApprovalRequestTx>,
    admission_deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<NativeDeliveryHandle, String> {
    if config.account_id.is_empty()
        || config.edge_agent_id.is_empty()
        || config.edge_transport_id.is_empty()
    {
        return Err("native delivery requires authenticated CLI identities".into());
    }
    let declaration = config
        .executor
        .native_codex_declaration_if_available(Some(cancellation))
        .await
        .ok_or("installed native provider is unavailable")?;
    let root = config
        .executor
        .effective_project_root()
        .canonicalize()
        .map_err(|_| "native workspace is unavailable")?;
    let root_text = root.to_str().ok_or("native workspace is not UTF-8")?;
    let snapshot = discovery_snapshot(
        &config.edge_agent_id,
        &config.materialization_id,
        root_text,
        declaration,
    )?;
    let approval = Arc::new(
        approve_bootstrap(
            &config.executor,
            snapshot,
            pm,
            approval_tx,
            admission_deadline,
            cancellation,
        )
        .await?,
    );
    connect_admitted_delivery(config, approval, admission_deadline, cancellation).await
}

// The admitted installation path is shared by production and real transport
// tests; tests replace only the external peer, not Astra auth/custody/dispatch.
async fn connect_admitted_delivery(
    config: NativeDeliveryConfig,
    approval: Arc<ApprovedNativeRuntime>,
    admission_deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<NativeDeliveryHandle, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let config = Arc::new(config);
    let mut request = config
        .websocket_url
        .as_str()
        .into_client_request()
        .map_err(|_| "invalid Edge WebSocket endpoint")?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", config.auth)
            .parse()
            .map_err(|_| "invalid Edge authentication header")?,
    );
    let connect = tokio_tungstenite::connect_async(request);
    let (socket, _) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err("native delivery cancelled".into()),
        result = tokio::time::timeout_at(tokio::time::Instant::from_std(admission_deadline), connect) =>
            result.map_err(|_| "native delivery connection deadline expired")?
                .map_err(|_| "native delivery connection failed")?,
    };
    // Auth does not claim capacity: only recovery/installed consumer readiness
    // below permits the later authenticated REST advertisement.
    let auth = EdgeClientMessage::Auth {
        edge_agent_id: config.edge_agent_id.clone(),
        materialization_id: config.materialization_id.clone(),
        interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
        hostname: None,
        workspace_dir: Some(approval.workspace_root.to_string_lossy().into_owned()),
        capabilities: Some(capabilities(&config, None)),
    };
    let (socket, account_id) =
        astra_edge::authenticate_connection(socket, auth, Some(&config.account_id), cancellation)
            .await
            .map_err(|_| "native delivery authentication failed")?;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let context = EdgeConnectionContext {
        account_id,
        edge_agent_id: config.edge_agent_id.clone(),
        workspace_dir: approval.workspace_root.clone(),
        journal_path: config.journal_path.clone(),
        ready: Some(ready_tx),
    };
    let owner_cancel = cancellation.child_token();
    let callback = Arc::new(CliNativeExecutor {
        config: config.clone(),
        approval: approval.clone(),
    });
    let task_cancel = owner_cancel.clone();
    let task_config = config.clone();
    let (installed_tx, installed_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let owner = astra_edge::serve_connection(socket, context, callback, task_cancel.clone());
        tokio::pin!(owner);
        let mut ended_before_ready = false;
        let ready = tokio::select! {
            biased;
            _ = &mut owner => { ended_before_ready = true; false },
            result = ready_rx => result.is_ok(),
        };
        if ready {
            // The same task owns publication and withdrawal ordering. An
            // initializer cannot publish after an already-settled owner has
            // withdrawn its capacity.
            let result = tokio::time::timeout_at(
                tokio::time::Instant::from_std(admission_deadline),
                publish(&task_config, Some(&approval.snapshot)),
            );
            tokio::pin!(result);
            let mut owner_ended = false;
            let published = tokio::select! {
                biased;
                _ = &mut owner => {
                    owner_ended = true;
                    // If publication was dispatched, settle it before the
                    // withdrawal; never issue these writes concurrently.
                    let _ = result.await;
                    false
                },
                result = &mut result => result.is_ok_and(|result| result.is_ok()),
            };
            let _ = installed_tx.send(published);
            if !owner_ended {
                if !published {
                    task_cancel.cancel();
                }
                let _ = owner.await;
            }
        } else {
            let _ = installed_tx.send(false);
            if !ended_before_ready {
                task_cancel.cancel();
                let _ = owner.await;
            }
        }
        // Withdraw capacity when admission/transport ends. No native work can
        // be dispatched through a disconnected consumer.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            publish(&task_config, None),
        )
        .await;
    });
    let handle = NativeDeliveryHandle {
        cancellation: owner_cancel,
        task: Some(task),
    };
    let installed = tokio::select! {
        biased;
        _ = cancellation.cancelled() => false,
        result = tokio::time::timeout_at(tokio::time::Instant::from_std(admission_deadline), installed_rx) =>
            result.ok().and_then(Result::ok).unwrap_or(false),
    };
    if !installed {
        handle.shutdown().await;
        return Err("native delivery recovery or capacity publication failed".into());
    }
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::permission_manager::PermissionMode;
    use astra_turn_types::{NativeToolId, ProviderRuntimeRequirements, ProviderToolDeclaration};
    use std::time::Duration;

    fn snapshot(
        requirements: &ProviderRuntimeRequirements,
        root: &std::path::Path,
    ) -> ProviderDiscoverySnapshot {
        let mut extension_fields = serde_json::Map::new();
        extension_fields.insert(
            astra_turn_types::PROVIDER_RUNTIME_REQUIREMENTS_KEY.into(),
            json!(requirements),
        );
        let declaration = ProviderToolDeclaration {
            native_tool_id: NativeToolId::new(native_codex::TOOL_NAME).unwrap(),
            native_tool_name: native_codex::TOOL_NAME.into(),
            stable_tool_alias: None,
            title: None,
            description: None,
            input_schema: native_codex::schema()["function"]["parameters"].clone(),
            output_schema: None,
            claims: Default::default(),
            task_support: Default::default(),
            extension_fields,
        };
        discovery_snapshot(
            "edge-test",
            "materialization-test",
            root.to_str().unwrap(),
            declaration,
        )
        .unwrap()
    }

    fn requirements(root: &std::path::Path) -> ProviderRuntimeRequirements {
        ProviderRuntimeRequirements {
            executable: root.join("codex").to_str().unwrap().into(),
            read_paths: vec![
                root.join("codex").to_str().unwrap().into(),
                root.join("platform").to_str().unwrap().into(),
            ],
        }
    }

    #[tokio::test]
    async fn bootstrap_uses_one_prompt_and_does_not_expand_general_file_authority() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(workspace.path());
        let before = executor
            .sandbox_policy
            .read()
            .unwrap()
            .clone()
            .unwrap()
            .allowed_paths;
        let req = requirements(runtime.path());
        let frozen = snapshot(&req, workspace.path());
        let hash = frozen.content_hash.clone();
        let mut pm = PermissionManager::with_project_mode(PermissionMode::Prompt, workspace.path());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            assert_eq!(prompt.args["provider_snapshot_hash"], hash);
            assert_eq!(prompt.args["read_paths"], json!(req.read_paths));
            prompt
                .response_tx
                .send(chat_stream::ApprovalResponse::AllowOnce)
                .unwrap();
        };
        let cancel = CancellationToken::new();
        let (receipt, ()) = tokio::join!(
            approve_bootstrap(
                &executor,
                frozen,
                &mut pm,
                Some(&tx),
                Instant::now() + Duration::from_secs(5),
                &cancel
            ),
            ui
        );
        let receipt = receipt.unwrap();
        assert_eq!(
            receipt.admission_source,
            ToolInvocationAdmissionSource::ParentApproval
        );
        assert_eq!(
            executor
                .sandbox_policy
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .allowed_paths,
            before
        );
        assert!(rx.try_recv().is_err(), "only one complete-set prompt");
        // The existing remembered approval is exact-descriptor/path scoped.
        let mut changed = req.clone();
        changed
            .read_paths
            .push(runtime.path().join("new-runtime").to_str().unwrap().into());
        let changed = snapshot(&changed, workspace.path());
        assert!(matches!(
            crate::tool_safety_guard::ToolSafetyGuard::check_request(
                Some(&mut pm),
                "sandbox_expand:native_codex",
                &bootstrap_args(&changed, &req.read_paths[0])
            ),
            GateOutcome::NeedApproval { .. }
        ));
    }

    #[tokio::test]
    async fn bootstrap_cancellation_and_forbidden_paths_never_enqueue_approval() {
        let workspace = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(workspace.path());
        let mut pm = PermissionManager::with_project_mode(PermissionMode::Auto, workspace.path());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            approve_bootstrap(
                &executor,
                snapshot(&requirements(workspace.path()), workspace.path()),
                &mut pm,
                Some(&tx),
                Instant::now() + Duration::from_secs(5),
                &cancel
            )
            .await
            .is_err()
        );
        let mut forbidden = requirements(workspace.path());
        forbidden
            .read_paths
            .push(workspace.path().join(".env.local").to_str().unwrap().into());
        assert!(
            approve_bootstrap(
                &executor,
                snapshot(&forbidden, workspace.path()),
                &mut pm,
                Some(&tx),
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new()
            )
            .await
            .is_err()
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn authenticated_shared_ws_reaches_cli_native_entrypoint_and_withdraws_capacity() {
        use astra_server_types::edge_ws_protocol::{
            EdgeExecutionCeiling, EdgeServerMessage, ToolInvocationIdentity,
        };
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let http = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
            .mount(&http)
            .await;
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let executor = Arc::new(ToolExecutor::new(workspace.path()));
        let req = requirements(runtime.path());
        let approval = Arc::new(ApprovedNativeRuntime {
            snapshot: snapshot(&req, workspace.path()),
            requirements: req.clone(),
            workspace_root: workspace.path().canonicalize().unwrap(),
            admission_source: ToolInvocationAdmissionSource::Policy,
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/edge/ws", listener.local_addr().unwrap());
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let root = approval.workspace_root.to_str().unwrap().to_owned();
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let auth: EdgeClientMessage =
                serde_json::from_slice(&ws.next().await.unwrap().unwrap().into_data()).unwrap();
            let EdgeClientMessage::Auth {
                capabilities: Some(capabilities),
                ..
            } = auth
            else {
                panic!("Auth required")
            };
            assert_eq!(capabilities["provider_discovery"], json!([]));
            ws.send(Message::Text(
                serde_json::to_string(&EdgeServerMessage::AuthOk {
                    user_id: "account-test".into(),
                    interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
            let identity = ToolInvocationIdentity::new(
                "account-test",
                "session-test",
                "run-test",
                "chain-test",
                "call-test",
            )
            .unwrap();
            let unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            let request = EdgeServerMessage::ToolRequest {
                request_id: identity.storage_key(),
                identity: Box::new(identity.clone()),
                delivery_generation: 1,
                tool: native_codex::TOOL_NAME.into(),
                args: json!({"task":"No paid invocation", "anchor_run_id":"anchor-test"}),
                execution_ceiling: Some(Box::new(EdgeExecutionCeiling {
                    workspace_root: root,
                    workspace_id: None,
                    materialization_id: Some("materialization-test".into()),
                    execution_binding_generation: 1,
                    runtime_read_paths: req.read_paths,
                    workspace_write_allowed: false,
                    network_allowed: false,
                })),
                runtime_process_authorization: None,
                runtime_process_authorization_required: false,
                timeout_secs: 120,
                execution_deadline_unix_ms: Some(unix + 5000),
                execution_timeout_ms: Some(5000),
                command_timeout_cap_ms: Some(120_000),
            };
            ws.send(Message::Text(
                serde_json::to_string(&request).unwrap().into(),
            ))
            .await
            .unwrap();
            loop {
                let frame = ws.next().await.unwrap().unwrap();
                if !frame.is_text() {
                    continue;
                }
                let message: EdgeClientMessage =
                    serde_json::from_slice(&frame.into_data()).unwrap();
                if let EdgeClientMessage::ToolResult {
                    identity: actual,
                    is_error,
                    tool_result_fields,
                    ..
                } = message
                {
                    assert_eq!(actual, identity);
                    assert!(is_error);
                    // This rejected installed-provider mismatch is produced by
                    // the real native leaf, not a standalone transport parser.
                    let fields = tool_result_fields.unwrap();
                    assert_eq!(
                        fields["native_collaborator"]["dispatch_state"],
                        "not_dispatched"
                    );
                    assert_eq!(fields["native_collaborator"]["target_released"], false);
                    let _ = result_tx.send(());
                    break;
                }
            }
            while let Some(frame) = ws.next().await {
                if frame.is_err() || frame.is_ok_and(|frame| frame.is_close()) {
                    break;
                }
            }
        });
        let config = NativeDeliveryConfig {
            websocket_url: endpoint,
            api: astra_thin_client::ThinClient::new(&http.uri(), None).unwrap(),
            auth: "test-only-token".into(),
            account_id: "account-test".into(),
            edge_agent_id: "edge-test".into(),
            edge_transport_id: "transport-test".into(),
            workspace_id: None,
            materialization_id: "materialization-test".into(),
            journal_path: workspace.path().join("journal.json"),
            executor,
            ask_user_request_tx: None,
        };
        let handle = connect_admitted_delivery(
            config,
            approval,
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), result_rx)
            .await
            .unwrap()
            .unwrap();
        handle.shutdown().await;
        tokio::time::timeout(Duration::from_secs(5), peer)
            .await
            .unwrap()
            .unwrap();
        let requests = http.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let last: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(
            first["capabilities"]["provider_discovery"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(last["capabilities"]["provider_discovery"], json!([]));
    }
}
