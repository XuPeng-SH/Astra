//! CLI hosting of the shared Edge delivery owner. Bootstrap permission is
//! resolved on actual invocation against a frozen declaration, never at
//! installation or inferred from discovery.

use super::provider_interaction::NativeInvocationInteractionGate;
use crate::cli::{
    chat_stream,
    permission_manager::{GateOutcome, PermissionPolicySubscription},
};
use crate::edge_tools::{ApprovedNativeRuntime, ToolExecutor, native_codex};
use astra_edge::{EdgeConnectionContext, EdgeInvocation, EdgeInvocationExecutor};
use astra_server_types::edge_ws_protocol::EdgeClientMessage;
use astra_tools::{
    ToolResult,
    tool_engine::{ToolInvocationAdmissionSource, ToolInvocationMetadata},
};
use astra_turn_core::provider_resolution::NativeCollaboratorProtocol;
use astra_turn_types::{
    ProviderBindingRef, ProviderDiscoverySnapshot, ProviderIdentity, ProviderProtocolId,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const NATIVE_DELIVERY_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const NATIVE_DELIVERY_STARTUP_WAIT: std::time::Duration = std::time::Duration::from_secs(6);

#[derive(Debug)]
enum NativeDeliveryError {
    Authentication(String),
    Other(String),
}

impl NativeDeliveryError {
    fn other(message: impl Into<String>) -> Self {
        Self::Other(message.into())
    }

    fn is_authentication(&self) -> bool {
        matches!(self, Self::Authentication(_))
    }
}

impl std::fmt::Display for NativeDeliveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authentication(message) | Self::Other(message) => formatter.write_str(message),
        }
    }
}

fn native_delivery_authentication_error(
    error: astra_edge::EdgeAuthenticationError,
) -> NativeDeliveryError {
    use astra_edge::EdgeAuthenticationError;
    match error {
        EdgeAuthenticationError::Rejected
        | EdgeAuthenticationError::InvalidAccount
        | EdgeAuthenticationError::AccountMismatch => {
            NativeDeliveryError::Authentication("native delivery credentials were rejected".into())
        }
        EdgeAuthenticationError::IncompatibleContract
        | EdgeAuthenticationError::Protocol
        | EdgeAuthenticationError::Envelope(_) => {
            NativeDeliveryError::other("native delivery authentication protocol is incompatible")
        }
        EdgeAuthenticationError::ClosedBeforeAuthentication
        | EdgeAuthenticationError::Timeout
        | EdgeAuthenticationError::Cancelled
        | EdgeAuthenticationError::Transport(_) => {
            NativeDeliveryError::other("native delivery authentication transport failed")
        }
    }
}

fn native_delivery_connection_error(
    error: tokio_tungstenite::tungstenite::Error,
) -> NativeDeliveryError {
    if let tokio_tungstenite::tungstenite::Error::Http(response) = &error
        && matches!(response.status().as_u16(), 401 | 403)
    {
        return NativeDeliveryError::Authentication(
            "native delivery server rejected credentials".into(),
        );
    }
    NativeDeliveryError::other("native delivery connection failed")
}

/// Held by the existing CLI lifecycle, never by a root-turn future. Dropping
/// the handle requests cancellation; the task still owns settlement/join.
pub(crate) struct NativeDeliveryHandle {
    cancellation: CancellationToken,
    withdrawal: CancellationToken,
    refresh_tx: mpsc::Sender<()>,
    discovered_executables: Arc<std::sync::Mutex<Vec<PathBuf>>>,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Set after the first discovery/handshake/publication attempt completes.
    /// `true` means the optional capability is settled for this turn; it does
    /// not claim that a provider was available.
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

    fn withdraw(&self) {
        self.withdrawal.cancel();
    }

    fn request_refresh(&self) {
        // Capacity one coalesces repeated environment changes. The existing
        // supervisor remains the single owner of discovery and reconnection.
        let current = native_codex::native_executable_candidates();
        let changed = self
            .discovered_executables
            .lock()
            .ok()
            .is_some_and(|selected| *selected != current);
        if changed {
            if let Ok(mut selected) = self.discovered_executables.lock() {
                *selected = current;
            }
            let _ = self.refresh_tx.try_send(());
        }
    }

    fn executable_changed(&self) -> bool {
        let current = native_codex::native_executable_candidates();
        self.discovered_executables
            .lock()
            .ok()
            .is_none_or(|selected| *selected != current)
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
        if let Some(task) = self.task.as_mut() {
            let _ = task.await;
        }
        self.task.take();
    }
}

async fn wait_for_native_delivery_ready(handle: &NativeDeliveryHandle) {
    if *handle.ready.borrow() {
        return;
    }
    let mut ready = handle.ready.clone();
    let _ = tokio::time::timeout(NATIVE_DELIVERY_STARTUP_WAIT, async move {
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
#[derive(Clone)]
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
    let declaration = snapshot
        .tool_declarations
        .first()
        .ok_or_else(|| "missing native collaborator declaration".to_string())?;
    let protocol = NativeCollaboratorProtocol::from_extension_fields(&declaration.extension_fields)
        .ok_or_else(|| "native collaborator protocol is not declared".to_string())?;
    let mut needed = false;
    for path in &requirements.read_paths {
        if astra_sandbox::is_never_readable_path(std::path::Path::new(path)) {
            return Err("native bootstrap contains a forbidden path".into());
        }
        match current
            .check_sandbox_expansion(protocol.permission_scope(), &bootstrap_args(snapshot, path))
        {
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
        let protocol =
            NativeCollaboratorProtocol::from_extension_fields(&declaration.extension_fields)
                .ok_or("native collaborator protocol is not declared")?;
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
            protocol.permission_scope().into(), format!("Allow native {} runtime reads?", protocol.display_name()),
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
            protocol,
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
    tool_name: String,
    protocol: NativeCollaboratorProtocol,
    expected_session_id: String,
    expected_attachment_epoch: u64,
    // Production installation always supplies this identity. The optional
    // test-only value keeps admission tests focused on permission facts
    // without manufacturing an executable artifact.
    expected_executable_identity: Option<native_codex::NativeExecutableIdentity>,
    invalidation: CancellationToken,
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

struct UnavailableNativeExecutor;

impl EdgeInvocationExecutor for UnavailableNativeExecutor {
    fn execute(&self, _: EdgeInvocation, _: CancellationToken) -> BoxFuture<'_, ToolResult> {
        Box::pin(async {
            rejected("native provider capability is unavailable; no work was dispatched")
        })
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
            if invocation.tool != self.tool_name
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
                physical_workspace_id:
                    astra_services::SessionExecutionBindingV1::edge_materialization_physical_identity(
                        &config.materialization_id,
                        &self.workspace_root.to_string_lossy(),
                    ),
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
            // Revalidate immediately before dispatch, after any user approval
            // wait. A capability snapshot is valid only while the exact
            // installed executable it described remains usable. A changed or
            // removed client withdraws this owner; the session supervisor will
            // probe the environment again instead of repeatedly rejecting a
            // stale published snapshot.
            let current_requirements =
                crate::edge_tools::native_codex::runtime_requirements_for_executable(
                    std::path::Path::new(&self.requirements.executable),
                );
            let identity_matches =
                self.expected_executable_identity
                    .as_ref()
                    .is_none_or(|expected| {
                        native_codex::native_executable_identity(std::path::Path::new(
                            &self.requirements.executable,
                        ))
                        .is_ok_and(|current| current == *expected)
                    });
            if current_requirements.as_ref().ok() != Some(&self.requirements) || !identity_matches {
                self.invalidation.cancel();
                return rejected("native provider capability changed; rediscovery is required");
            }
            let native_input_rx = invocation.input_rx;
            let outcome = config
                .executor
                .execute_native_provider_invocation(
                    self.protocol,
                    &self.tool_name,
                    &invocation.args,
                    metadata,
                    Some(&cancel),
                    &gate,
                    ceiling,
                    Some(&approval),
                    native_input_rx,
                )
                .await;
            if outcome
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("native_capability_unavailable"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                // The result is returned through the normal Edge completion
                // path first. The shared owner observes this token only as a
                // drain signal, so already-admitted sibling invocations are
                // not cancelled or lost.
                self.invalidation.cancel();
            }
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
async fn install_native_delivery(
    config: NativeDeliveryConfig,
    admission_deadline: Instant,
    cancellation: &CancellationToken,
    withdrawal: CancellationToken,
    refresh_tx: mpsc::Sender<()>,
    discovered_executables: Arc<std::sync::Mutex<Vec<PathBuf>>>,
) -> Result<NativeDeliveryHandle, NativeDeliveryError> {
    if config.account_id.is_empty()
        || config.edge_agent_id.is_empty()
        || config.edge_transport_id.is_empty()
    {
        return Err(NativeDeliveryError::other(
            "native delivery requires authenticated CLI identities",
        ));
    }
    let root = config
        .executor
        .effective_project_root()
        .canonicalize()
        .map_err(|_| NativeDeliveryError::other("native workspace is unavailable"))?;
    let root_text = root
        .to_str()
        .ok_or_else(|| NativeDeliveryError::other("native workspace is not UTF-8"))?;
    let snapshot = match config
        .executor
        .native_collaborator_declaration_if_available(Some(cancellation), admission_deadline)
        .await
    {
        Some(declaration) => Some(Arc::new(
            discovery_snapshot(
                &config.edge_agent_id,
                &config.materialization_id,
                root_text,
                declaration,
            )
            .map_err(NativeDeliveryError::other)?,
        )),
        None if astra_edge::has_pending_invocation_results(config.journal_path.clone()).await => {
            // Provider capability is optional, but an authenticated Edge owner
            // must still be able to replay an already durable result. It runs
            // withdrawn until discovery succeeds, so this recovery path cannot
            // admit new provider work.
            None
        }
        None => {
            return Err(NativeDeliveryError::other(
                "installed native provider is unavailable",
            ));
        }
    };
    connect_native_delivery(
        config,
        snapshot,
        admission_deadline,
        cancellation,
        withdrawal,
        refresh_tx,
        discovered_executables,
    )
    .await
}

// The capability-only installation path is shared by production and real transport
// tests; tests replace only the external peer, not Astra auth/custody/dispatch.
async fn connect_native_delivery(
    config: NativeDeliveryConfig,
    snapshot: Option<Arc<ProviderDiscoverySnapshot>>,
    admission_deadline: Instant,
    cancellation: &CancellationToken,
    withdrawal: CancellationToken,
    refresh_tx: mpsc::Sender<()>,
    discovered_executables: Arc<std::sync::Mutex<Vec<PathBuf>>>,
) -> Result<NativeDeliveryHandle, NativeDeliveryError> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let workspace_root = config
        .executor
        .effective_project_root()
        .canonicalize()
        .map_err(|_| NativeDeliveryError::other("native workspace is unavailable"))?;
    if withdrawal.is_cancelled() {
        return Err(NativeDeliveryError::other(
            "native delivery refresh requested before connection",
        ));
    }
    let provider = snapshot
        .as_ref()
        .map(|snapshot| {
            let declaration = snapshot
                .tool_declarations
                .first()
                .ok_or_else(|| NativeDeliveryError::other("missing native declaration"))?;
            let protocol =
                NativeCollaboratorProtocol::from_extension_fields(&declaration.extension_fields)
                    .ok_or_else(|| {
                        NativeDeliveryError::other("native collaborator protocol is not declared")
                    })?;
            let requirements =
                astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(
                    &declaration.extension_fields,
                )
                .map_err(|_| NativeDeliveryError::other("invalid native runtime requirements"))?
                .ok_or_else(|| NativeDeliveryError::other("missing native runtime requirements"))?;
            let attachment = config.permission_policy.current().ok_or_else(|| {
                NativeDeliveryError::other("native delivery requires a bound permission attachment")
            })?;
            let executable_identity = native_codex::native_executable_identity(
                std::path::Path::new(&requirements.executable),
            )
            .map_err(NativeDeliveryError::other)?;
            Ok::<_, NativeDeliveryError>((
                declaration.native_tool_name.clone(),
                protocol,
                requirements,
                executable_identity,
                attachment.session_id().to_owned(),
                attachment.attachment_epoch(),
            ))
        })
        .transpose()?;
    let mut request = config
        .websocket_url
        .as_str()
        .into_client_request()
        .map_err(|_| NativeDeliveryError::other("invalid Edge WebSocket endpoint"))?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {}", config.auth)
            .parse()
            .map_err(|_| NativeDeliveryError::other("invalid Edge authentication header"))?,
    );
    let connect = tokio_tungstenite::connect_async(request);
    let (socket, _) = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(NativeDeliveryError::other("native delivery cancelled")),
        result = tokio::time::timeout_at(tokio::time::Instant::from_std(admission_deadline), connect) =>
            result.map_err(|_| NativeDeliveryError::other("native delivery connection deadline expired"))?
                .map_err(native_delivery_connection_error)?,
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
    let authenticated = tokio::time::timeout_at(
        tokio::time::Instant::from_std(admission_deadline),
        astra_edge::authenticate_connection(socket, auth, Some(&config.account_id), cancellation),
    )
    .await
    .map_err(|_| NativeDeliveryError::other("native delivery authentication deadline expired"))?
    .map_err(native_delivery_authentication_error)?;
    let (socket, account_id, edge_transport_id) = authenticated;
    // The server owns the transport identity. The local agent label is only
    // an authenticated capability selector and must never be reused as the
    // REST callback identity.
    let config = Arc::new(NativeDeliveryConfig {
        edge_transport_id,
        ..config
    });
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let context = EdgeConnectionContext {
        account_id,
        edge_agent_id: config.edge_agent_id.clone(),
        workspace_dir: workspace_root.clone(),
        journal_path: config.journal_path.clone(),
        ready: Some(ready_tx),
    };
    let owner_cancel = cancellation.child_token();
    let capability_withdrawal = CancellationToken::new();
    let callback: Arc<dyn EdgeInvocationExecutor> = if let Some((
        tool_name,
        protocol,
        requirements,
        executable_identity,
        expected_session_id,
        expected_attachment_epoch,
    )) = provider
    {
        Arc::new(CliNativeExecutor {
            config: config.clone(),
            snapshot: snapshot.clone().expect("provider snapshot exists"),
            workspace_root,
            requirements,
            tool_name,
            protocol,
            expected_session_id,
            expected_attachment_epoch,
            expected_executable_identity: Some(executable_identity),
            invalidation: capability_withdrawal.clone(),
        })
    } else {
        // Recovery can connect without a currently usable provider. The Edge
        // owner replays durable receipts, while every new request is denied
        // and the capability remains withdrawn.
        withdrawal.cancel();
        Arc::new(UnavailableNativeExecutor)
    };
    let task_cancel = owner_cancel.clone();
    let task_withdrawal = capability_withdrawal.clone();
    let task_external_withdrawal = withdrawal.clone();
    let bridge_withdrawal = task_withdrawal.clone();
    let task_config = config.clone();
    let (installed_tx, installed_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let withdrawal_bridge = tokio::spawn(async move {
            task_external_withdrawal.cancelled().await;
            bridge_withdrawal.cancel();
        });
        let owner = astra_edge::serve_connection_with_drain(
            socket,
            context,
            callback,
            task_cancel.clone(),
            Some(task_withdrawal),
        );
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
                publish(&task_config, snapshot.as_deref()),
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
        withdrawal_bridge.abort();
    });
    let handle = NativeDeliveryHandle {
        cancellation: owner_cancel,
        withdrawal,
        refresh_tx,
        discovered_executables,
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
        return Err(NativeDeliveryError::other(
            "native delivery recovery or capacity publication failed",
        ));
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
        if handle.executable_changed() {
            // A PATH/launcher change is an explicit capability invalidation,
            // not a reason to create a second owner. The current owner drains
            // admitted work; its supervisor coalesces this request and probes
            // the current environment before advertising again.
            handle.request_refresh();
        }
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
    let supervisor_cancel = task_cancel.clone();
    let (refresh_tx, mut refresh_rx) = mpsc::channel(1);
    let discovered_executables = Arc::new(std::sync::Mutex::new(
        native_codex::native_executable_candidates(),
    ));
    let task_discovered_executables = discovered_executables.clone();
    let task_refresh_tx = refresh_tx.clone();
    let (ready_tx, ready_rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(async move {
        let mut startup_tx = Some(ready_tx);
        let mut was_available = false;
        let mut reconnect_failures = 0_u32;
        loop {
            if supervisor_cancel.is_cancelled() {
                break;
            }
            let result = install_native_delivery(
                config.clone(),
                Instant::now() + NATIVE_DELIVERY_STARTUP_TIMEOUT,
                &supervisor_cancel,
                CancellationToken::new(),
                task_refresh_tx.clone(),
                task_discovered_executables.clone(),
            )
            .await;
            match result {
                Ok(mut delivery) => {
                    // The first successful provider binding settles the
                    // startup wait. Later disconnects are handled by this
                    // same session owner, never by replaying an invocation.
                    let _ = startup_tx.take().map(|tx| tx.send(true));
                    was_available = true;
                    let mut refresh_requested = false;
                    tokio::select! {
                        _ = supervisor_cancel.cancelled() => {
                            delivery.shutdown().await;
                            break;
                        }
                        refresh = refresh_rx.recv() => {
                            if refresh.is_none() {
                                delivery.shutdown().await;
                                break;
                            }
                            refresh_requested = true;
                            delivery.withdraw();
                            delivery.wait().await;
                        }
                        _ = delivery.wait() => {}
                    }
                    if supervisor_cancel.is_cancelled() {
                        break;
                    }
                    if refresh_requested {
                        reconnect_failures = 0;
                        continue;
                    }
                    reconnect_failures = 1;
                    tracing::warn!(
                        session_id = %task_session_id,
                        "native collaborator transport ended; retrying capability discovery"
                    );
                }
                Err(error) if !was_available => {
                    // Initial discovery is optional capacity. Do not keep a
                    // missing client in a hot retry loop; the next user turn
                    // performs a fresh probe and can bind it if it appears.
                    let _ = startup_tx.take().map(|tx| tx.send(true));
                    tracing::warn!(session_id = %task_session_id, %error, "native collaborator delivery unavailable");
                    break;
                }
                Err(error) => {
                    if error.is_authentication() {
                        // The supervisor captured the credentials for its
                        // session owner. Once the server rejects them, keep
                        // no live owner around: the next turn will rebuild
                        // this optional capability with the current account
                        // snapshot instead of retrying a stale bearer token.
                        tracing::warn!(
                            session_id = %task_session_id,
                            %error,
                            "native collaborator credentials need a fresh session probe"
                        );
                        break;
                    }
                    if !astra_edge::has_pending_invocation_results(config.journal_path.clone())
                        .await
                    {
                        // A disconnected optional provider with no durable
                        // receipt left to replay has no work for a background
                        // reconnect loop. The next user turn owns the next
                        // capability probe, so an installation appearing
                        // later is still discovered without a hot retry.
                        tracing::debug!(
                            session_id = %task_session_id,
                            %error,
                            "native collaborator unavailable with no pending receipt"
                        );
                        break;
                    }
                    reconnect_failures = reconnect_failures.saturating_add(1);
                    tracing::warn!(
                        session_id = %task_session_id,
                        attempt = reconnect_failures,
                        %error,
                        "native collaborator reconnect unavailable"
                    );
                }
            }

            let delay_secs = match reconnect_failures.min(5) {
                0 => 1,
                1 => 1,
                2 => 2,
                3 => 4,
                4 => 8,
                _ => 16,
            };
            tokio::select! {
                _ = supervisor_cancel.cancelled() => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(delay_secs)) => {}
            }
        }
    });
    state.native_delivery = Some(NativeDeliveryHandle {
        cancellation: task_cancel,
        withdrawal: CancellationToken::new(),
        refresh_tx,
        discovered_executables,
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
    use crate::edge_tools::native_codex;

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
    fn authentication_failure_ends_the_owner_for_credential_refresh() {
        assert!(NativeDeliveryError::Authentication("rejected".into()).is_authentication());
        assert!(!NativeDeliveryError::other("deadline").is_authentication());
        assert!(!NativeDeliveryError::other("connection failed").is_authentication());
        assert!(
            !native_delivery_authentication_error(astra_edge::EdgeAuthenticationError::Timeout)
                .is_authentication()
        );
        assert!(
            native_delivery_authentication_error(astra_edge::EdgeAuthenticationError::Rejected)
                .is_authentication()
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
        extension_fields.insert(
            NativeCollaboratorProtocol::EXTENSION_KEY.into(),
            json!(NativeCollaboratorProtocol::CodexAppServer.extension_value()),
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
            tool_name: native_codex::TOOL_NAME.into(),
            protocol: NativeCollaboratorProtocol::CodexAppServer,
            expected_session_id: "session-test".into(),
            expected_attachment_epoch: owner.session_attachment_epoch,
            expected_executable_identity: None,
            invalidation: CancellationToken::new(),
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
            input_rx: tokio::sync::mpsc::channel(1).1,
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
            assert_eq!(result.metadata.unwrap()["execution_fact"], "not_executed");
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
    async fn native_authentication_has_a_bounded_startup_deadline() {
        use futures_util::StreamExt;

        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let executable = runtime.path().join("codex");
        std::fs::write(&executable, b"provider").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&executable, permissions).unwrap();
        }
        let (consumer, _owner) = consumer(workspace.path(), runtime.path(), None);
        let discovery = Arc::new(snapshot(&consumer.requirements, workspace.path()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/edge/ws", listener.local_addr().unwrap());
        let peer = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let _ = ws.next().await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });

        let mut config = (*consumer.config).clone();
        config.websocket_url = endpoint;
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            connect_native_delivery(
                config,
                Some(discovery),
                Instant::now() + Duration::from_millis(100),
                &CancellationToken::new(),
                CancellationToken::new(),
                mpsc::channel(1).0,
                Arc::new(std::sync::Mutex::new(Vec::new())),
            ),
        )
        .await
        .expect("authentication must not hang past the test guard");
        let error = match error {
            Ok(_) => panic!("an unauthenticated peer cannot publish native capacity"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("deadline"));
        peer.abort();
        let _ = peer.await;
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
            matchers::{header, method, path},
        };
        let http = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/agents/edge"))
            .and(header("X-Astra-Edge-Id", "ws-native-test"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok":true})))
            .mount(&http)
            .await;
        let workspace = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let executable = runtime.path().join("codex");
        std::fs::write(&executable, b"provider").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&executable, permissions).unwrap();
        }
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
                    edge_id: "ws-native-test".into(),
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
            // The callback detects that the executable captured by the
            // published capability has been replaced. It must not dispatch
            // through a stale snapshot. The rejected result is delivered
            // before the owner withdraws, so the server has a truthful
            // not-executed fact and can rediscover on the next turn.
            loop {
                match ws.next().await {
                    Some(Ok(frame)) if frame.is_text() => {
                        let message: EdgeClientMessage =
                            serde_json::from_slice(&frame.into_data()).unwrap();
                        if matches!(message, EdgeClientMessage::Ping {}) {
                            continue;
                        }
                        let EdgeClientMessage::ToolResult {
                            identity: actual,
                            is_error,
                            tool_result_fields,
                            ..
                        } = message.clone()
                        else {
                            panic!(
                                "stale native capability returned an unexpected message: {message:?}"
                            )
                        };
                        assert_eq!(actual, identity);
                        assert!(is_error);
                        let fields = tool_result_fields.unwrap();
                        assert_eq!(fields["execution_fact"], "not_executed");
                        assert_eq!(fields["workspace_effect_settled"], true);
                        break;
                    }
                    Some(Ok(frame)) if frame.is_close() => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            let _ = result_tx.send(());
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
            Some(discovery),
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            CancellationToken::new(),
            mpsc::channel(1).0,
            Arc::new(std::sync::Mutex::new(Vec::new())),
        )
        .await
        .unwrap();
        assert!(
            approval_rx.try_recv().is_err(),
            "installation/discovery cannot prompt"
        );
        // Replace the discovered artifact before dispatch. The provider path
        // is still present, but its capability identity is no longer the one
        // that was probed and published.
        std::fs::write(&executable, b"replacement-provider").unwrap();
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
