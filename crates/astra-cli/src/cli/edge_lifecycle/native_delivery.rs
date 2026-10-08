//! CLI hosting of the shared Edge delivery owner. Bootstrap permission is
//! resolved on actual invocation against a frozen declaration, never at
//! installation or inferred from discovery.

use super::provider_interaction::NativeInvocationInteractionGate;
use crate::cli::{
    chat_stream,
    permission_manager::{GateOutcome, PermissionPolicySubscription},
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
    ready: tokio::sync::watch::Receiver<bool>,
}

impl Drop for NativeDeliveryHandle {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl NativeDeliveryHandle {
    pub(crate) fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.task
            .as_ref()
            .is_none_or(tokio::task::JoinHandle::is_finished)
    }

    pub(crate) async fn shutdown(mut self) {
        self.cancel();
        self.wait().await;
    }

    async fn wait(&mut self) {
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

async fn wait_for_native_delivery_ready(handle: &NativeDeliveryHandle) {
    if *handle.ready.borrow() {
        return;
    }
    let mut ready = handle.ready.clone();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async move {
        loop {
            if *ready.borrow() {
                break;
            }
            if ready.changed().await.is_err() {
                break;
            }
        }
    })
    .await;
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
    /// Read-only publication from the selected SessionState permission writer.
    pub(crate) permission_policy: PermissionPolicySubscription,
    pub(crate) approval_request_tx: Option<chat_stream::ApprovalRequestTx>,
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

/// Verify current attachment authority, not merely a retained policy Arc.
fn current_policy(
    policy: &PermissionPolicySubscription,
    session_id: &str,
    attachment_epoch: u64,
) -> Result<Arc<crate::cli::permission_manager::PermissionPolicySnapshot>, String> {
    let current = policy
        .current()
        .ok_or("native permission attachment is unbound")?;
    if current.session_id() != session_id || current.attachment_epoch() != attachment_epoch {
        return Err("native permission attachment changed".into());
    }
    Ok(current)
}

fn dependencies_need_approval(
    policy: &PermissionPolicySubscription,
    session_id: &str,
    attachment_epoch: u64,
    snapshot: &ProviderDiscoverySnapshot,
    requirements: &astra_turn_types::ProviderRuntimeRequirements,
) -> Result<bool, String> {
    let current = current_policy(policy, session_id, attachment_epoch)?;
    let mut needed = false;
    for path in &requirements.read_paths {
        if astra_sandbox::is_never_readable_path(std::path::Path::new(path)) {
            return Err("native bootstrap contains a forbidden path".into());
        }
        match current.check_sandbox_expansion(
            "sandbox_expand:native_codex",
            &bootstrap_args(snapshot, path),
        ) {
            GateOutcome::Allow => {}
            GateOutcome::Deny(_) => return Err("native bootstrap denied by local policy".into()),
            GateOutcome::NeedApproval { .. } => needed = true,
        }
    }
    Ok(needed)
}

/// One complete-set prompt, with no permission writes in the consumer.
impl CliNativeExecutor {
    async fn approve_bootstrap(
        &self,
        invocation: &EdgeInvocation,
        cancellation: &CancellationToken,
    ) -> Result<ApprovedNativeRuntime, String> {
        let executor = &self.config.executor;
        let snapshot = self.snapshot.clone();
        let permission_policy = &self.config.permission_policy;
        let expected_session_id = self.expected_session_id.as_str();
        let expected_attachment_epoch = self.expected_attachment_epoch;
        let approval_tx = self.config.approval_request_tx.as_ref();
        let deadline = invocation.execution_deadline;
        if cancellation.is_cancelled() || Instant::now() >= deadline {
            return Err("native bootstrap admission cancelled or expired".into());
        }
        if invocation.identity.session_id != expected_session_id {
            return Err("native invocation does not match permission attachment".into());
        }
        let binding_generation = invocation
            .execution_ceiling
            .as_ref()
            .ok_or("native bootstrap requires an execution ceiling")?
            .execution_binding_generation;
        if binding_generation == 0 {
            return Err("native bootstrap requires a binding generation".into());
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
        let mut policy = permission_policy.clone();
        let source = if dependencies_need_approval(
            &policy,
            expected_session_id,
            expected_attachment_epoch,
            &snapshot,
            &requirements,
        )? {
            let tx = approval_tx.ok_or("native bootstrap requires an approval consumer")?;
            let (response_tx, mut response_rx) = tokio::sync::oneshot::channel();
            let args = json!({"provider_snapshot_hash": snapshot.content_hash,
            "provider_binding": snapshot.binding_ref, "executable": requirements.executable,
            "read_paths": requirements.read_paths, "access": "native_runtime_read"});
            let mut request = chat_stream::ApprovalRequest::bare(
            "sandbox_expand:native_codex".into(), "Allow native Codex runtime reads?".into(),
            Some(requirements.read_paths.join("\n")),
            "These exact executable/platform paths become readable by the native collaborator; workspace, network and sensitive-path restrictions remain in force.".into(),
            args, response_tx,
        );
            request.metadata = Some(Box::new(crate::tui::approval::queue::ApprovalMetadata {
                runtime_dependencies: Some(
                    crate::tui::approval::queue::RuntimeDependencyApprovalContext {
                        invocation: invocation.identity.clone(),
                        attachment_epoch: expected_attachment_epoch,
                        execution_binding_generation: binding_generation,
                        deadline,
                        cancel: cancellation.clone(),
                    },
                ),
                ..Default::default()
            }));
            chat_stream::enqueue_interactive_request(tx, request)
                .map_err(|_| "native bootstrap approval consumer unavailable")?;
            let cutoff = tokio::time::Instant::from_std(deadline);
            loop {
                if cancellation.is_cancelled() || tokio::time::Instant::now() >= cutoff {
                    return Err("native bootstrap admission cancelled or expired".into());
                }
                let needs_approval = dependencies_need_approval(
                    &policy,
                    expected_session_id,
                    expected_attachment_epoch,
                    &snapshot,
                    &requirements,
                )?;
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() =>
                        return Err("native bootstrap admission cancelled".into()),
                    _ = tokio::time::sleep_until(cutoff) =>
                        return Err("native bootstrap admission expired".into()),
                    response = &mut response_rx => {
                        // The latest whole-set decision always wins over a stale answer.
                        let needed = dependencies_need_approval(
                            &policy, expected_session_id, expected_attachment_epoch, &snapshot, &requirements,
                        )?;
                        match response.map_err(|_| "native bootstrap approval consumer closed")? {
                            chat_stream::ApprovalResponse::AllowOnce => break if needed {
                                ToolInvocationAdmissionSource::ParentApproval
                            } else {
                                ToolInvocationAdmissionSource::Policy
                            },
                            chat_stream::ApprovalResponse::AlwaysAllow =>
                                return Err("native dependency persistent approval is not supported".into()),
                            chat_stream::ApprovalResponse::Deny =>
                                return Err("native bootstrap approval denied".into()),
                        }
                    }
                    changed = policy.changed() => {
                        changed.map_err(|_| "native permission writer closed")?;
                    }
                    _ = std::future::ready(()), if !needs_approval =>
                        break ToolInvocationAdmissionSource::Policy,
                }
            }
        } else {
            ToolInvocationAdmissionSource::Policy
        };
        if cancellation.is_cancelled() || Instant::now() >= deadline {
            return Err("native bootstrap admission cancelled or expired".into());
        }
        let still_needs_approval = dependencies_need_approval(
            &policy,
            expected_session_id,
            expected_attachment_epoch,
            &snapshot,
            &requirements,
        )?;
        if still_needs_approval && source == ToolInvocationAdmissionSource::Policy {
            return Err("native bootstrap policy approval was revoked".into());
        }
        let source = if still_needs_approval {
            source
        } else {
            ToolInvocationAdmissionSource::Policy
        };
        Ok(ApprovedNativeRuntime {
            snapshot: (*snapshot).clone(),
            requirements,
            workspace_root: root,
            admission_source: source,
        })
    }
}

struct CliNativeExecutor {
    config: Arc<NativeDeliveryConfig>,
    snapshot: Arc<ProviderDiscoverySnapshot>,
    workspace_root: PathBuf,
    requirements: astra_turn_types::ProviderRuntimeRequirements,
    expected_session_id: String,
    expected_attachment_epoch: u64,
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
                || ceiling.workspace_root != self.workspace_root.to_string_lossy()
                || ceiling.workspace_id != config.workspace_id
                || ceiling.materialization_id.as_deref() != Some(config.materialization_id.as_str())
                || ceiling.execution_binding_generation == 0
                || invocation.runtime_process_authorization.is_some()
                || ceiling.runtime_read_paths != self.requirements.read_paths
            {
                return rejected("CLI native delivery does not match the selected boundary");
            }
            // Discovery is capability only. Local approval is acquired on a
            // real, fenced invocation using its original work deadline. The
            // UI wait and native execution share that cutoff.
            if config
                .executor
                .effective_project_root()
                .canonicalize()
                .ok()
                .as_ref()
                != Some(&self.workspace_root)
            {
                return rejected("native bootstrap workspace no longer matches discovery");
            }
            let approval = match self.approve_bootstrap(&invocation, &cancel).await {
                Ok(approval) => approval,
                Err(reason) => return rejected(&reason),
            };
            // A worktree change while the approval was pending must not turn
            // the frozen grant into authority over the newly selected root.
            if approval.workspace_root != self.workspace_root {
                return rejected("native bootstrap workspace changed during approval");
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
                admission_source: Some(approval.admission_source),
                command_timeout_cap_ms: invocation.command_timeout_cap_ms,
                ..ToolInvocationMetadata::default()
            };
            let needed = match dependencies_need_approval(
                &config.permission_policy,
                &self.expected_session_id,
                self.expected_attachment_epoch,
                &self.snapshot,
                &self.requirements,
            ) {
                Ok(needed) => needed,
                Err(reason) => return rejected(&reason),
            };
            if needed && approval.admission_source == ToolInvocationAdmissionSource::Policy {
                return rejected("native bootstrap policy approval was revoked before dispatch");
            }
            if cancel.is_cancelled() || Instant::now() >= invocation.execution_deadline {
                return rejected("native invocation cancelled or expired before dispatch");
            }
            let outcome = config
                .executor
                .execute_native_codex_provider_invocation(
                    &invocation.args,
                    metadata,
                    Some(&cancel),
                    &gate,
                    ceiling,
                    Some(&approval),
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
    // This socket is a provider-stage executor, not the ordinary CLI tool
    // boundary. Advertising the builtin surface would let server admission
    // route `bash`/file tools here even though this executor intentionally
    // accepts only the native provider invocation.
    if let Some(surface) = value
        .get_mut("binding")
        .and_then(Value::as_object_mut)
        .and_then(|binding| binding.get_mut("tool_surface"))
        .and_then(Value::as_object_mut)
    {
        surface.insert("tool_names".into(), json!([]));
        surface.insert("admissions".into(), json!([]));
        surface.insert("denials".into(), json!([]));
    }
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
/// Installation neither evaluates local permission nor asks for bootstrap
/// approval: discovery becomes a grant only inside a fenced invocation.
pub(crate) async fn install_native_delivery(
    config: NativeDeliveryConfig,
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
    connect_native_delivery(config, Arc::new(snapshot), admission_deadline, cancellation).await
}

// The capability-only installation path is shared by production and real transport
// tests; tests replace only the external peer, not Astra auth/custody/dispatch.
async fn connect_native_delivery(
    config: NativeDeliveryConfig,
    snapshot: Arc<ProviderDiscoverySnapshot>,
    admission_deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<NativeDeliveryHandle, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let workspace_root = config
        .executor
        .effective_project_root()
        .canonicalize()
        .map_err(|_| "native workspace is unavailable")?;
    let declaration = snapshot
        .tool_declarations
        .first()
        .ok_or("missing native declaration")?;
    let requirements = astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(
        &declaration.extension_fields,
    )
    .map_err(|_| "invalid native runtime requirements")?
    .ok_or("missing native runtime requirements")?;
    let attachment = config
        .permission_policy
        .current()
        .ok_or("native delivery requires a bound permission attachment")?;
    let expected_session_id = attachment.session_id().to_owned();
    let expected_attachment_epoch = attachment.attachment_epoch();
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
        workspace_dir: Some(workspace_root.to_string_lossy().into_owned()),
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
        workspace_dir: workspace_root.clone(),
        journal_path: config.journal_path.clone(),
        ready: Some(ready_tx),
    };
    let owner_cancel = cancellation.child_token();
    let callback = Arc::new(CliNativeExecutor {
        config: config.clone(),
        snapshot: snapshot.clone(),
        workspace_root,
        requirements,
        expected_session_id,
        expected_attachment_epoch,
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
                publish(&task_config, Some(&snapshot)),
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
        ready: tokio::sync::watch::channel(true).1,
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

fn native_journal_path(session_id: &str) -> PathBuf {
    astra_services::session_journal::journal_file_path(session_id)
        .with_extension("native-edge.jsonl")
}

/// Install the selected CLI capacity only after the canonical interactive
/// session identity exists. The turn boundary is the existing owner for this
/// one-time preparation; the handle itself remains owned by SessionState until
/// the session attachment changes or the TUI shuts down.
pub(crate) async fn ensure_session_native_delivery(
    state: &mut crate::cli::session::session_state::SessionState,
    api: &astra_thin_client::ThinClient,
    token: &str,
    session_id: &str,
) {
    let attachment_epoch = state.session_attachment_epoch;
    if let Some(handle) = state.native_delivery.as_ref()
        && state.native_delivery_session_id.as_deref() == Some(session_id)
        && state.native_delivery_attachment_epoch == Some(attachment_epoch)
        && !handle.is_finished()
    {
        wait_for_native_delivery_ready(handle).await;
        // Capacity is optional. A slow or unavailable provider must not block
        // the ordinary turn; the next turn reuses the same readiness watch.
        return;
    }

    if let Some(handle) = state.native_delivery.take() {
        handle.shutdown().await;
    }
    state.native_delivery_session_id = None;
    state.native_delivery_attachment_epoch = None;

    let Some(shutdown) = state.native_delivery_shutdown.clone() else {
        // Headless/line callers have no interactive delivery owner. They
        // continue through the ordinary canonical turn path unchanged.
        return;
    };
    if shutdown.is_cancelled() {
        return;
    }

    let account_id = state
        .ingestion_user_id
        .clone()
        .or_else(crate::cli::cli_config::cli_utils::cli_account_id);
    let Some(account_id) = account_id.filter(|value| !value.trim().is_empty()) else {
        tracing::debug!("native collaborator delivery skipped: account identity unavailable");
        return;
    };
    let root = match std::env::current_dir()
        .ok()
        .and_then(|path| std::fs::canonicalize(path).ok())
    {
        Some(root) => root,
        None => {
            tracing::warn!("native collaborator delivery skipped: workspace is unavailable");
            return;
        }
    };
    let materialization_id = match astra_runtime_env::load_or_create_materialization_id(&root) {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!(%error, "native collaborator delivery skipped: materialization is unavailable");
            return;
        }
    };
    let edge_agent_id = match crate::cli::chat_stream::try_edge_executor_instance_id() {
        Ok(id) => id.to_owned(),
        Err(error) => {
            tracing::warn!(%error, "native collaborator delivery skipped: Edge identity unavailable");
            return;
        }
    };

    let mut executor = ToolExecutor::new(root.clone())
        .with_active_session_id(session_id.to_owned())
        .with_cloud(api.api_origin(), token.to_owned())
        .with_shared_file_journal(state.file_journal.clone())
        .with_shared_file_state(state.file_state.clone())
        .with_shared_database_snapshot_journal(state.database_snapshot_journal.clone())
        .with_shared_git_worktree_journal(state.git_worktree_journal.clone())
        .with_shared_session_state_journal(state.session_state_journal.clone())
        .with_bg_task_commands(state.bg_task_commands.clone())
        .with_bg_task_list_cache(state.bg_task_list_cache.clone())
        .with_bash_detach_slot(state.bash_detach_slot.clone());
    if let Some(observability) = state.observability_session.clone() {
        executor = executor.with_observability_session(observability);
    }

    let config = NativeDeliveryConfig {
        websocket_url: match astra_edge::edge_ws_url(&api.api_origin()) {
            Ok(url) => url,
            Err(error) => {
                tracing::warn!(%error, "native collaborator delivery skipped: invalid WebSocket endpoint");
                return;
            }
        },
        api: api.clone(),
        auth: token.to_owned(),
        account_id,
        edge_agent_id: edge_agent_id.clone(),
        edge_transport_id: edge_agent_id,
        workspace_id: None,
        materialization_id,
        journal_path: native_journal_path(session_id),
        executor: Arc::new(executor),
        ask_user_request_tx: state.tui_ask_user_request_tx.clone(),
        permission_policy: state.perm_manager.subscribe_permission_policy(),
        approval_request_tx: state.tui_approval_request_tx.clone(),
    };

    let task_cancel = shutdown.child_token();
    let task_session_id = session_id.to_owned();
    let install_cancel = task_cancel.clone();
    let supervisor_cancel = task_cancel.clone();
    let (ready_tx, ready_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        let result = install_native_delivery(
            config,
            Instant::now() + std::time::Duration::from_secs(10),
            &install_cancel,
        )
        .await;
        let _ = ready_tx.send(true);
        match result {
            Ok(mut delivery) => {
                // Keep the actual Edge owner alive inside the session-owned
                // supervisor. Dropping the returned handle would cancel its
                // connection immediately after successful publication.
                tokio::select! {
                    _ = supervisor_cancel.cancelled() => delivery.shutdown().await,
                    _ = delivery.wait() => {}
                }
            }
            Err(error) => {
                // Discovery is optional capacity. A failed native install must
                // not turn an ordinary Astra turn into a false failure; the
                // server simply cannot select this capacity until a later turn.
                tracing::warn!(session_id = %task_session_id, %error, "native collaborator delivery unavailable");
            }
        }
    });
    state.native_delivery = Some(NativeDeliveryHandle {
        cancellation: task_cancel,
        task: Some(task),
        ready: ready_rx,
    });
    state.native_delivery_session_id = Some(session_id.to_owned());
    state.native_delivery_attachment_epoch = Some(attachment_epoch);
    if let Some(handle) = state.native_delivery.as_ref() {
        wait_for_native_delivery_ready(handle).await;
    }
    tracing::info!(
        session_id,
        attachment_epoch,
        "native collaborator delivery startup scheduled"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_journal_is_a_session_sibling_file() {
        let session_id = "session-test";
        let journal = native_journal_path(session_id);
        assert_eq!(
            journal.parent(),
            astra_services::session_journal::journal_file_path(session_id).parent()
        );
        assert!(
            journal
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".native-edge.jsonl"))
        );
    }

    #[test]
    fn native_registration_advertises_only_the_provider_stage_surface() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (consumer, _owner) = consumer(workspace.path(), runtime.path(), None);
        let value = capabilities(&consumer.config, Some(&consumer.snapshot));
        let surface = &value["binding"]["tool_surface"];
        assert_eq!(surface["tool_names"], json!([]));
        assert_eq!(surface["admissions"], json!([]));
        assert_eq!(surface["denials"], json!([]));
        assert_eq!(
            value["provider_discovery"][0]["tool_declarations"][0]["native_tool_name"],
            native_codex::TOOL_NAME
        );
    }

    use crate::cli::permission_manager::{PermissionManager, PermissionMode};
    use crate::cli::session::session_state::SessionState;
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

    fn consumer(
        workspace: &std::path::Path,
        runtime: &std::path::Path,
        approval_tx: Option<chat_stream::ApprovalRequestTx>,
    ) -> (CliNativeExecutor, SessionState) {
        let mut owner = SessionState {
            perm_manager: PermissionManager::with_project_mode(PermissionMode::Prompt, workspace),
            ..SessionState::default()
        };
        owner.set_session_id("session-test");
        let requirements = requirements(runtime);
        let consumer = CliNativeExecutor {
            snapshot: Arc::new(snapshot(&requirements, workspace)),
            workspace_root: workspace.canonicalize().unwrap(),
            requirements,
            expected_session_id: "session-test".into(),
            expected_attachment_epoch: owner.session_attachment_epoch,
            config: Arc::new(NativeDeliveryConfig {
                websocket_url: "ws://127.0.0.1:1/edge/ws".into(),
                api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap(),
                auth: "test-only-token".into(),
                account_id: "account-test".into(),
                edge_agent_id: "edge-test".into(),
                edge_transport_id: "transport-test".into(),
                workspace_id: None,
                materialization_id: "materialization-test".into(),
                journal_path: workspace.join("journal.json"),
                executor: Arc::new(ToolExecutor::new(workspace)),
                ask_user_request_tx: None,
                permission_policy: owner.perm_manager.subscribe_permission_policy(),
                approval_request_tx: approval_tx,
            }),
        };
        (consumer, owner)
    }

    async fn admission(
        consumer: &CliNativeExecutor,
        invocation: &EdgeInvocation,
        cancel: &CancellationToken,
    ) -> Result<ApprovedNativeRuntime, String> {
        consumer.approve_bootstrap(invocation, cancel).await
    }

    fn invocation(consumer: &CliNativeExecutor) -> EdgeInvocation {
        EdgeInvocation {
            identity: astra_turn_types::ToolInvocationIdentity::new(
                "account-test",
                "session-test",
                "run-test",
                "chain-test",
                "call-test",
            )
            .unwrap(),
            delivery_generation: 1,
            tool: native_codex::TOOL_NAME.into(),
            args: json!({"task": "No paid invocation", "anchor_run_id": "anchor-test"}),
            execution_deadline: Instant::now() + Duration::from_secs(5),
            command_timeout_cap_ms: Some(120_000),
            runtime_process_authorization: None,
            execution_ceiling: Some(Box::new(
                astra_server_types::edge_ws_protocol::EdgeExecutionCeiling {
                    workspace_root: consumer.workspace_root.to_str().unwrap().into(),
                    workspace_id: None,
                    materialization_id: Some("materialization-test".into()),
                    execution_binding_generation: 1,
                    runtime_read_paths: consumer.requirements.read_paths.clone(),
                    workspace_write_allowed: false,
                    network_allowed: false,
                },
            )),
        }
    }

    #[tokio::test]
    async fn actual_invocation_allow_once_does_not_write_permission_owner() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        assert!(rx.try_recv().is_err(), "discovery has no approval effect");
        for _ in 0..2 {
            let call = invocation(&consumer);
            let identity = call.identity.clone();
            let deadline = call.execution_deadline;
            let ui = async {
                let prompt = rx.recv().await.unwrap();
                let context = prompt
                    .metadata
                    .as_ref()
                    .unwrap()
                    .runtime_dependencies
                    .as_ref()
                    .unwrap();
                assert_eq!(context.invocation, identity);
                assert_eq!(context.attachment_epoch, owner.session_attachment_epoch);
                assert_eq!(context.execution_binding_generation, 1);
                assert_eq!(
                    prompt.args["provider_snapshot_hash"].as_str().unwrap(),
                    consumer.snapshot.content_hash
                );
                assert_eq!(context.deadline, deadline);
                prompt
                    .response_tx
                    .send(chat_stream::ApprovalResponse::AllowOnce)
                    .unwrap();
            };
            let (result, ()) = tokio::join!(consumer.execute(call, CancellationToken::new()), ui);
            assert_eq!(
                result.metadata.unwrap()["native_collaborator"]["dispatch_state"],
                "not_dispatched"
            );
            assert!(
                matches!(
                    owner
                        .perm_manager
                        .subscribe_permission_policy()
                        .current()
                        .unwrap()
                        .check_sandbox_expansion(
                            "sandbox_expand:native_codex",
                            &bootstrap_args(
                                &consumer.snapshot,
                                &consumer.requirements.read_paths[0]
                            )
                        ),
                    GateOutcome::NeedApproval { .. }
                ),
                "allow once must not persist an override"
            );
        }
    }

    #[tokio::test]
    async fn pending_approval_observes_hard_revocation_without_ui_response() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            owner.perm_manager.set_mode(PermissionMode::Deny);
            prompt
        };
        let (result, prompt) = tokio::join!(
            consumer.execute(invocation(&consumer), CancellationToken::new()),
            ui
        );
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        assert!(prompt.response_tx.is_closed());
    }

    #[tokio::test]
    async fn queued_user_deny_wins_simultaneously_ready_policy_allow() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            // No await between these operations: both watch and response are
            // ready before the admission future can be polled again.
            owner.perm_manager.set_mode(PermissionMode::Bypass);
            assert!(
                !dependencies_need_approval(
                    &consumer.config.permission_policy,
                    &consumer.expected_session_id,
                    consumer.expected_attachment_epoch,
                    &consumer.snapshot,
                    &consumer.requirements,
                )
                .unwrap()
            );
            prompt
                .response_tx
                .send(chat_stream::ApprovalResponse::Deny)
                .unwrap();
        };
        let (result, ()) = tokio::join!(
            biased;
            consumer.execute(invocation(&consumer), CancellationToken::new()),
            ui
        );
        assert!(result.is_error);
        assert_eq!(result.output, "native bootstrap approval denied");
        let metadata = result.metadata.unwrap();
        assert_eq!(metadata["execution_fact"], "not_executed");
        assert!(
            metadata.get("native_collaborator").is_none(),
            "native leaf must not dispatch"
        );
    }

    #[tokio::test]
    async fn policy_change_rechecks_all_dependencies_and_returns_policy_not_user_approval() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let call = invocation(&consumer);
        let cancel = CancellationToken::new();
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            owner.perm_manager.record_approval(
                "sandbox_expand:native_codex",
                Some(&bootstrap_args(
                    &consumer.snapshot,
                    &consumer.requirements.read_paths[0],
                )),
                true,
            );
            tokio::task::yield_now().await;
            assert!(
                !prompt.response_tx.is_closed(),
                "one allowed path is not the complete set"
            );
            owner.perm_manager.set_mode(PermissionMode::Bypass);
            prompt
        };
        let (result, prompt) = tokio::join!(admission(&consumer, &call, &cancel), ui);
        assert_eq!(
            result.unwrap().admission_source,
            ToolInvocationAdmissionSource::Policy
        );
        assert!(prompt.response_tx.is_closed());
    }

    #[tokio::test]
    async fn coalesced_same_session_rebind_revokes_pending_and_future_dispatch() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            owner.clear_session_id();
            owner.set_session_id("session-test");
            prompt
                .response_tx
                .send(chat_stream::ApprovalResponse::AllowOnce)
                .unwrap();
        };
        let (result, ()) = tokio::join!(
            consumer.execute(invocation(&consumer), CancellationToken::new()),
            ui
        );
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        owner.perm_manager.set_mode(PermissionMode::Bypass);
        let result = consumer
            .execute(invocation(&consumer), CancellationToken::new())
            .await;
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn always_allow_is_rejected_without_permission_writeback() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let call = invocation(&consumer);
        let cancel = CancellationToken::new();
        let ui = async {
            rx.recv()
                .await
                .unwrap()
                .response_tx
                .send(chat_stream::ApprovalResponse::AlwaysAllow)
                .unwrap();
        };
        let (result, ()) = tokio::join!(admission(&consumer, &call, &cancel), ui);
        assert!(result.is_err());
        assert!(matches!(
            owner
                .perm_manager
                .subscribe_permission_policy()
                .current()
                .unwrap()
                .check_sandbox_expansion(
                    "sandbox_expand:native_codex",
                    &bootstrap_args(&consumer.snapshot, &consumer.requirements.read_paths[0])
                ),
            GateOutcome::NeedApproval { .. }
        ));
    }

    #[tokio::test]
    async fn same_id_reset_and_closed_writer_reject_without_prompt() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        owner.perm_manager.set_mode(PermissionMode::Bypass);
        assert!(
            current_policy(
                &consumer.config.permission_policy,
                &consumer.expected_session_id,
                consumer.expected_attachment_epoch,
            )
            .is_ok()
        );
        owner.reset_for_new_session();
        let result = consumer
            .execute(invocation(&consumer), CancellationToken::new())
            .await;
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        drop(owner);
        let result = consumer
            .execute(invocation(&consumer), CancellationToken::new())
            .await;
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn mismatched_ceiling_cancel_unbound_and_forbidden_paths_never_prompt() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, mut owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let mut call = invocation(&consumer);
        call.execution_ceiling
            .as_mut()
            .unwrap()
            .runtime_read_paths
            .push("/unapproved".into());
        assert_eq!(
            consumer
                .execute(call, CancellationToken::new())
                .await
                .metadata
                .unwrap()["execution_fact"],
            "not_executed"
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            admission(&consumer, &invocation(&consumer), &cancel)
                .await
                .is_err()
        );
        let mut forbidden = consumer.requirements.clone();
        forbidden
            .read_paths
            .push(workspace.path().join(".env.local").to_str().unwrap().into());
        assert!(
            dependencies_need_approval(
                &consumer.config.permission_policy,
                &consumer.expected_session_id,
                consumer.expected_attachment_epoch,
                &snapshot(&forbidden, workspace.path()),
                &forbidden,
            )
            .is_err()
        );
        owner.clear_session_id();
        owner.perm_manager.set_active_session_id("session-test");
        owner.perm_manager.set_mode(PermissionMode::Bypass);
        assert!(
            admission(&consumer, &invocation(&consumer), &CancellationToken::new())
                .await
                .is_err()
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn pending_approval_cancellation_and_deadline_close_response_without_dispatch() {
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let (consumer, _owner) = consumer(workspace.path(), runtime.path(), Some(tx));
        let cancel = CancellationToken::new();
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            cancel.cancel();
            prompt
        };
        let (result, prompt) =
            tokio::join!(consumer.execute(invocation(&consumer), cancel.clone()), ui);
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        assert!(prompt.response_tx.is_closed());
        let ui = async {
            let prompt = rx.recv().await.unwrap();
            tokio::time::sleep(Duration::from_secs(6)).await;
            prompt
        };
        let (result, prompt) = tokio::join!(
            consumer.execute(invocation(&consumer), CancellationToken::new()),
            ui
        );
        assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
        assert!(prompt.response_tx.is_closed());
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
        let (approval_tx, mut approval_rx) =
            tokio::sync::mpsc::channel::<chat_stream::ApprovalRequest>(1);
        let req = requirements(runtime.path());
        let discovery = Arc::new(snapshot(&req, workspace.path()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/edge/ws", listener.local_addr().unwrap());
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let root = workspace
            .path()
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let (dispatch_tx, dispatch_rx) = tokio::sync::oneshot::channel();
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
            dispatch_rx.await.unwrap();
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
        let mut permission_owner = SessionState {
            perm_manager: PermissionManager::with_project_mode(
                PermissionMode::Prompt,
                workspace.path(),
            ),
            ..SessionState::default()
        };
        permission_owner.set_session_id("session-test");
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
            permission_policy: permission_owner.perm_manager.subscribe_permission_policy(),
            approval_request_tx: Some(approval_tx),
        };
        let handle = connect_native_delivery(
            config,
            discovery,
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            approval_rx.try_recv().is_err(),
            "installation/discovery cannot prompt"
        );
        dispatch_tx.send(()).unwrap();
        let prompt = tokio::time::timeout(Duration::from_secs(5), approval_rx.recv())
            .await
            .unwrap()
            .unwrap();
        prompt
            .response_tx
            .send(chat_stream::ApprovalResponse::AllowOnce)
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
