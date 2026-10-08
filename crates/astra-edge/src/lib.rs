//! The existing Edge delivery owner, shared by the Edge binary and CLI host.
//! Authentication/configuration stay in the host; this owner alone handles
//! journal custody, generation fencing, cancellation, ACK/replay and settlement.

mod invocation_journal;

use astra_server_types::edge_ws_protocol::{
    EDGE_HEARTBEAT_INTERVAL_SECS, EdgeClientMessage, EdgeServerMessage,
};
use futures_util::{SinkExt, StreamExt, future::BoxFuture};
use invocation_journal::{DurableEdgeResult, EdgeInvocationJournal, JournalError, PrepareOutcome};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinSet,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use tokio_util::sync::CancellationToken;

/// Convert an API/server base URL into the WebSocket endpoint used by Edge
/// owners. CLI hosts use this same conversion so HTTP configuration and native
/// delivery cannot silently diverge.
pub fn edge_ws_url(server_url: &str) -> Result<String, String> {
    let trimmed = server_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("server URL must not be empty".to_string());
    }
    let with_ws_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else if trimmed.contains("://") {
        return Err(format!(
            "unsupported server URL scheme in '{trimmed}'; use http(s):// or ws(s)://"
        ));
    } else {
        format!("ws://{trimmed}")
    };

    let mut url = reqwest::Url::parse(&with_ws_scheme)
        .map_err(|_| format!("invalid server URL '{server_url}'"))?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(format!(
            "unsupported edge WebSocket URL scheme '{}'; use ws:// or wss://",
            url.scheme()
        ));
    }
    url.set_path(&normalized_edge_ws_path(url.path()));
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.to_string())
}

fn normalized_edge_ws_path(path: &str) -> String {
    let segments = path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();

    if segments.is_empty() {
        return "/edge/ws".to_string();
    }

    if let Some(index) = segments
        .windows(2)
        .position(|window| window == ["edge", "ws"])
    {
        return format!("/{}", segments[..index + 2].join("/"));
    }

    format!("/{}/edge/ws", segments.join("/"))
}

pub type EdgeConnectionError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum EdgeAuthenticationError {
    #[error("Server rejected Edge authentication")]
    Rejected,
    #[error("Server interaction contract is incompatible")]
    IncompatibleContract,
    #[error("Server returned an invalid authenticated account")]
    InvalidAccount,
    #[error("Server authenticated a different account")]
    AccountMismatch,
    #[error("Unexpected Edge authentication response")]
    Protocol,
    #[error("Edge connection closed before authentication completed")]
    ClosedBeforeAuthentication,
    #[error("Edge authentication deadline expired")]
    Timeout,
    #[error("Edge authentication cancelled")]
    Cancelled,
    #[error("Edge authentication transport failed")]
    Transport(#[source] tokio_tungstenite::tungstenite::Error),
    #[error("Edge authentication envelope is invalid")]
    Envelope(#[source] serde_json::Error),
}

impl EdgeAuthenticationError {
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            Self::Rejected
                | Self::IncompatibleContract
                | Self::InvalidAccount
                | Self::AccountMismatch
                | Self::Protocol
                | Self::Envelope(_)
        )
    }
}

/// One authentication exchange for both hosts. A failed exchange consumes the
/// socket: callers cannot accidentally continue with a half-authenticated peer.
/// Success establishes identity, not consumer/capacity readiness.
pub async fn authenticate_connection(
    mut socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    auth: EdgeClientMessage,
    expected_account: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<
    (
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        String,
    ),
    EdgeAuthenticationError,
> {
    if !matches!(auth, EdgeClientMessage::Auth { .. }) {
        return Err(EdgeAuthenticationError::Protocol);
    }
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(astra_server_types::edge_ws_protocol::EDGE_AUTH_TIMEOUT_SECS);
    let exchange = async {
        socket
            .send(Message::Text(
                serde_json::to_string(&auth)
                    .map_err(EdgeAuthenticationError::Envelope)?
                    .into(),
            ))
            .await
            .map_err(EdgeAuthenticationError::Transport)?;
        let response = match socket.next().await {
            Some(Ok(Message::Text(text))) => serde_json::from_str::<EdgeServerMessage>(&text)
                .map_err(|_| EdgeAuthenticationError::Protocol)?,
            Some(Err(error)) => return Err(EdgeAuthenticationError::Transport(error)),
            None | Some(Ok(Message::Close(_))) => {
                return Err(EdgeAuthenticationError::ClosedBeforeAuthentication);
            }
            _ => return Err(EdgeAuthenticationError::Protocol),
        };
        match response {
            EdgeServerMessage::AuthOk {
                user_id,
                interaction_api_major,
            } => {
                if interaction_api_major != astra_server_types::AGENT_INTERACTION_API_MAJOR {
                    return Err(EdgeAuthenticationError::IncompatibleContract);
                }
                if user_id.trim().is_empty() {
                    return Err(EdgeAuthenticationError::InvalidAccount);
                }
                if expected_account.is_some_and(|expected| expected != user_id) {
                    return Err(EdgeAuthenticationError::AccountMismatch);
                }
                Ok(user_id)
            }
            EdgeServerMessage::AuthError { .. } => Err(EdgeAuthenticationError::Rejected),
            _ => Err(EdgeAuthenticationError::Protocol),
        }
    };
    let account = tokio::select! {
        biased;
        _ = cancellation.cancelled() => return Err(EdgeAuthenticationError::Cancelled),
        result = tokio::time::timeout_at(deadline, exchange) =>
            result.map_err(|_| EdgeAuthenticationError::Timeout)??,
    };
    Ok((socket, account))
}

/// Account is the actual AuthOk identity, not a caller-local fallback.
pub struct EdgeConnectionContext {
    pub account_id: String,
    pub edge_agent_id: String,
    pub workspace_dir: PathBuf,
    pub journal_path: PathBuf,
    /// Signals installed consumer readiness after journal recovery/replay.
    pub ready: Option<oneshot::Sender<()>>,
}

/// Exact transport envelope; a host must not reinterpret IDs or renew budget.
pub struct EdgeInvocation {
    pub identity: astra_server_types::edge_ws_protocol::ToolInvocationIdentity,
    pub delivery_generation: u64,
    pub tool: String,
    pub args: Value,
    pub execution_deadline: Instant,
    pub command_timeout_cap_ms: Option<u64>,
    pub execution_ceiling: Option<Box<astra_server_types::edge_ws_protocol::EdgeExecutionCeiling>>,
    pub runtime_process_authorization:
        Option<Box<astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext>>,
}

pub trait EdgeInvocationExecutor: Send + Sync {
    fn execute(
        &self,
        invocation: EdgeInvocation,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, astra_tools::ToolResult>;
}

const MAX_CONCURRENT_TOOL_EXECUTIONS: usize = 128;

fn command_deadline(
    deadline: Instant,
    timeout_secs: u64,
    cap_ms: Option<u64>,
    native: bool,
) -> Instant {
    if native {
        deadline
    } else {
        let cap = Duration::from_millis(cap_ms.unwrap_or(u64::MAX))
            .min(Duration::from_secs(timeout_secs));
        deadline.min(Instant::now() + cap)
    }
}

/// Anchor the immutable server work budget at receipt, before queueing or
/// journal I/O. Relative remaining time must never renew an absolute cutoff.
fn edge_execution_deadline(
    timeout_secs: u64,
    execution_deadline_unix_ms: Option<u64>,
    execution_timeout_ms: Option<u64>,
) -> Result<Instant, &'static str> {
    let now = Instant::now();
    let duration = match (execution_deadline_unix_ms, execution_timeout_ms) {
        (Some(deadline), Some(remaining)) => {
            let unix_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| "Edge clock cannot validate execution deadline")?
                .as_millis()
                .min(u128::from(u64::MAX)) as u64;
            Duration::from_millis(remaining.min(deadline.saturating_sub(unix_ms)))
        }
        (None, None) => Duration::from_secs(timeout_secs),
        _ => return Err("Incomplete server execution budget"),
    };
    if duration.is_zero() {
        return Err("Server-issued execution deadline expired before dispatch");
    }
    now.checked_add(duration)
        .ok_or("Server execution deadline is outside clock range")
}

#[derive(Clone)]
struct EdgeExecutionBudget {
    permits: Arc<Semaphore>,
}

impl EdgeExecutionBudget {
    fn new() -> Self {
        Self {
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_TOOL_EXECUTIONS)),
        }
    }

    fn try_acquire(&self) -> Option<OwnedSemaphorePermit> {
        self.permits.clone().try_acquire_owned().ok()
    }
}

struct CompletedEdgeInvocation {
    request_id: String,
    generation: u64,
    result: astra_tools::ToolResult,
    duration_ms: u64,
}

fn rejected_tool_message(
    request_id: String,
    identity: astra_turn_types::ToolInvocationIdentity,
    delivery_generation: u64,
    message: impl Into<String>,
) -> EdgeClientMessage {
    DurableEdgeResult::from_tool_result(astra_tools::ToolResult::error(message.into()), 0)
        .client_message(request_id, identity, delivery_generation)
}

fn valid_runtime_process_authorization(
    tool: &str,
    required: bool,
    context: Option<&astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext>,
) -> bool {
    required == context.is_some()
        && context.is_none_or(|context| {
            astra_server_types::edge_ws_protocol::runtime_process_authorization_applies_to_tool(
                tool,
            ) && !context.authorization.trim().is_empty()
        })
}

struct InFlightEdgeInvocation {
    generation: u64,
    cancel: CancellationToken,
}

#[derive(Default)]
struct EdgeInvocationTracker {
    in_flight: HashMap<String, InFlightEdgeInvocation>,
}

impl EdgeInvocationTracker {
    fn begin(&mut self, request_id: &str, generation: u64) -> Result<CancellationToken, u64> {
        if let Some(active) = self.in_flight.get(request_id) {
            return Err(active.generation);
        }
        let cancel = CancellationToken::new();
        self.in_flight.insert(
            request_id.to_string(),
            InFlightEdgeInvocation {
                generation,
                cancel: cancel.clone(),
            },
        );
        Ok(cancel)
    }

    fn cancel_if_current(&self, request_id: &str, generation: u64) -> bool {
        let Some(active) = self.in_flight.get(request_id) else {
            return false;
        };
        if active.generation != generation {
            return false;
        }
        active.cancel.cancel();
        true
    }

    fn finish_if_current(&mut self, request_id: &str, generation: u64) -> bool {
        if self
            .in_flight
            .get(request_id)
            .is_none_or(|active| active.generation != generation)
        {
            return false;
        }
        self.in_flight.remove(request_id);
        true
    }

    fn cancel_all(self) {
        for active in self.in_flight.into_values() {
            active.cancel.cancel();
        }
    }
}

/// Serve an already authenticated socket. Shutdown stops admission, cancels
/// active invocations, persists their actual results and joins owned work.
pub async fn serve_connection(
    socket: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    mut context: EdgeConnectionContext,
    executor: Arc<dyn EdgeInvocationExecutor>,
    shutdown: CancellationToken,
) -> Result<(), EdgeConnectionError> {
    if shutdown.is_cancelled() {
        return Ok(());
    }
    let (mut write, mut read) = socket.split();
    let (completed_tx, mut completed_rx) = mpsc::channel::<CompletedEdgeInvocation>(1_024);
    let execution_budget = EdgeExecutionBudget::new();
    let mut invocations = EdgeInvocationTracker::default();
    let mut tasks = JoinSet::new();
    let mut journal = EdgeInvocationJournal::open(context.journal_path.clone()).await?;
    let journal_status = journal.status();
    tracing::info!(
        target: "astra.edge.invocation_journal",
        records = journal_status.records,
        running = journal_status.running,
        awaiting_ack = journal_status.awaiting_ack,
        state_bytes = journal_status.state_bytes,
        wal_entries = journal_status.wal_entries,
        wal_bytes = journal_status.wal_bytes,
        "edge invocation journal restored"
    );

    // Results remain in the durable outbox until the server acknowledges the
    // exact delivery generation. Reconnect therefore starts by replaying them.
    for pending in journal.pending_results()? {
        let message = pending.result.client_message(
            pending.request_id,
            pending.identity,
            pending.delivery_generation,
        );
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Ok(()),
            result = write.send(Message::Text(serde_json::to_string(&message)?.into())) => result?,
        }
    }

    if shutdown.is_cancelled() {
        return Ok(());
    }
    if let Some(ready) = context.ready.take() {
        let _ = ready.send(());
    }

    // Heartbeat ticker
    let mut heartbeat = tokio::time::interval(Duration::from_secs(EDGE_HEARTBEAT_INTERVAL_SECS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    tracing::info!(
        workspace = %context.workspace_dir.display(),
        "Edge agent ready — waiting for tool calls"
    );

    let connection_result = async {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = joined {
                    return Err(Box::new(error) as EdgeConnectionError);
                }
            }
            msg = read.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        tracing::debug!(
                            frame_len = text.len(),
                            "edge received server text frame"
                        );
                        match serde_json::from_str::<EdgeServerMessage>(&text) {
                            Ok(EdgeServerMessage::ToolRequest {
                                request_id,
                                identity,
                                delivery_generation,
                                tool,
                                args: tool_args,
                                runtime_process_authorization,
                                runtime_process_authorization_required,
                                timeout_secs,
                                execution_deadline_unix_ms,
                                execution_timeout_ms,
                                command_timeout_cap_ms,
                                execution_ceiling,
                            }) => {
                                let execution_deadline = edge_execution_deadline(timeout_secs, execution_deadline_unix_ms, execution_timeout_ms).map(|deadline| {
                                    command_deadline(deadline, timeout_secs, command_timeout_cap_ms, execution_ceiling.is_some())
                                });
                                if !valid_runtime_process_authorization(
                                    &tool,
                                    runtime_process_authorization_required,
                                    runtime_process_authorization.as_deref(),
                                ) {
                                    let message = rejected_tool_message(
                                        request_id,
                                        *identity,
                                        delivery_generation,
                                        "Runtime process authorization is invalid",
                                    );
                                    write
                                        .send(Message::Text(
                                            serde_json::to_string(&message)?.into(),
                                        ))
                                        .await?;
                                    continue;
                                }
                                let execution_permit = execution_budget.try_acquire();
                                match journal
                                    .prepare(
                                        &request_id,
                                        &identity,
                                        delivery_generation,
                                        &tool,
                                        &tool_args,
                                        execution_permit.is_some(),
                                        execution_ceiling.as_deref(),
                                    )
                                    .await
                                {
                                    Ok(PrepareOutcome::Replay(result)) => {
                                        let message = result.client_message(
                                            request_id,
                                            *identity,
                                            delivery_generation,
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Ok(PrepareOutcome::Active) => {
                                        tracing::warn!(
                                            request_id = %request_id,
                                            delivery_generation,
                                            "Duplicate edge delivery joined the existing invocation"
                                        );
                                        continue;
                                    }
                                    Ok(PrepareOutcome::Execute) => {}
                                    Err(error @ (JournalError::Full | JournalError::WalFull)) => {
                                        let journal_status = journal.status();
                                        tracing::warn!(
                                            target: "astra.edge.invocation_journal",
                                            %error,
                                            records = journal_status.records,
                                            running = journal_status.running,
                                            awaiting_ack = journal_status.awaiting_ack,
                                            state_bytes = journal_status.state_bytes,
                                            wal_entries = journal_status.wal_entries,
                                            wal_bytes = journal_status.wal_bytes,
                                            "edge invocation admission rejected by durable journal capacity"
                                        );
                                        let result = DurableEdgeResult::not_dispatched_rejection(
                                            format!("Edge invocation admission is temporarily saturated: {error}"),
                                        )
                                        .with_journal_status(&journal_status);
                                        let message = result.client_message(
                                            request_id,
                                            *identity,
                                            delivery_generation,
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Err(error @ JournalError::IdentityConflict { .. }) => {
                                        let message = rejected_tool_message(
                                            request_id,
                                            *identity,
                                            delivery_generation,
                                            format!(
                                                "Edge invocation identity conflict before dispatch: {error}"
                                            ),
                                        );
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                    Err(error) => return Err(error.into()),
                                }
                                // Replay returns the durable original result even after
                                // expiry. Only a newly admitted execution is rejected.
                                let execution_deadline = match execution_deadline {
                                    Ok(deadline) => deadline,
                                    Err(reason) => {
                                        let pending = journal.complete(&request_id, delivery_generation, DurableEdgeResult::not_dispatched_rejection(reason)).await?;
                                        let message = pending.result.client_message(request_id, *identity, delivery_generation);
                                        write.send(Message::Text(serde_json::to_string(&message)?.into())).await?;
                                        continue;
                                    }
                                };
                                let execution_permit = execution_permit.ok_or_else(|| {
                                    format!(
                                        "edge invocation journal admitted {request_id} without execution capacity"
                                    )
                                })?;
                                let cancel = match invocations.begin(&request_id, delivery_generation) {
                                    Ok(cancel) => cancel,
                                    Err(active_generation) => {
                                        return Err(format!(
                                            "edge invocation tracker conflicts with durable journal for {request_id}: active generation {active_generation}, incoming {delivery_generation}"
                                        ).into());
                                    }
                                };
                                let executor = executor.clone();
                                let completed_tx = completed_tx.clone();
                                tracing::info!(tool = %tool, request_id = %request_id, generation = delivery_generation, "Executing tool");
                                tasks.spawn(async move {
                                    let _execution_permit = execution_permit;
                                    let start = Instant::now();
                                    let execution = async {
                                        if Instant::now() >= execution_deadline || cancel.is_cancelled() {
                                            return astra_tools::ToolResult::error("Server-issued execution deadline expired before dispatch".into());
                                        }
                                        executor.execute(EdgeInvocation {
                                            identity: *identity,
                                            delivery_generation,
                                            tool: tool.clone(),
                                            args: tool_args,
                                            execution_deadline,
                                            command_timeout_cap_ms,
                                            execution_ceiling,
                                            runtime_process_authorization,
                                        }, cancel.clone()).await
                                    };
                                    // The executor owns asynchronous subprocess cleanup.
                                    // Dropping its future on cancellation strands children.
                                    tokio::pin!(execution);
                                    let result = tokio::select! {
                                        result = &mut execution => result,
                                        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(execution_deadline)) => {
                                            cancel.cancel();
                                            let _ = execution.await;
                                            astra_tools::ToolResult::error(
                                                format!("Tool '{tool}' exceeded its server-issued execution deadline")
                                            )
                                        }
                                    };
                                    let completion = CompletedEdgeInvocation {
                                        request_id,
                                        generation: delivery_generation,
                                        result,
                                        duration_ms: start.elapsed().as_millis() as u64,
                                    };
                                    let _ = completed_tx.send(completion).await;
                                });
                            }
                            Ok(EdgeServerMessage::Pong {}) => {
                                // heartbeat ack
                            }
                            Ok(EdgeServerMessage::ToolCancel { request_id, delivery_generation }) => {
                                let execution_generation = journal
                                    .running_execution_generation(&request_id, delivery_generation);
                                if execution_generation.is_some_and(|generation| {
                                    invocations.cancel_if_current(&request_id, generation)
                                }) {
                                    tracing::info!(
                                        request_id = %request_id,
                                        delivery_generation,
                                        "Cancelled in-flight edge invocation"
                                    );
                                } else {
                                    tracing::debug!(request_id = %request_id, "Ignoring cancellation for non-active edge invocation");
                                }
                            }
                            Ok(EdgeServerMessage::ToolResultAck { request_id, delivery_generation }) => {
                                if !journal.acknowledge(&request_id, delivery_generation).await? {
                                    tracing::warn!(
                                        request_id = %request_id,
                                        delivery_generation,
                                        "Ignoring stale or unknown edge result acknowledgement"
                                    );
                                }
                            }
                            Ok(EdgeServerMessage::Closing { reason }) => {
                                tracing::info!(reason = %reason, "Server closing connection");
                                break;
                            }
                            Ok(EdgeServerMessage::AuthOk { .. } | EdgeServerMessage::AuthError { .. }) => {
                                // ignore duplicate auth
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "Failed to parse server message");
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        let _ = write.send(Message::Pong(data)).await;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("Connection closed");
                        break;
                    }
                    Some(Err(error)) => return Err(error.into()),
                    _ => {}
                }
            }
            Some(completed) = completed_rx.recv() => {
                if !invocations.finish_if_current(&completed.request_id, completed.generation) {
                    tracing::warn!(
                        request_id = %completed.request_id,
                        generation = completed.generation,
                        "Discarding stale edge invocation completion"
                    );
                    continue;
                }
                let pending = persist_completion(&mut journal, completed).await?;
                let result_msg = pending.result.client_message(
                    pending.request_id,
                    pending.identity,
                    pending.delivery_generation,
                );
                write.send(Message::Text(serde_json::to_string(&result_msg)?.into())).await?;
            }
            _ = heartbeat.tick() => {
                let ping = EdgeClientMessage::Ping {};
                if write.send(Message::Text(serde_json::to_string(&ping)?.into())).await.is_err() {
                    tracing::warn!("Failed to send heartbeat");
                    break;
                }
            }
        }
    }

    Ok(())
    }.await;

    // Every exit (including journal and socket errors) settles this connection's
    // executions before reconnect can acquire the journal and dispatch again.
    invocations.cancel_all();
    // A failed append may have left a partial WAL record. Do not append again
    // until open() has validated/recovered it. Other connection failures do
    // not prevent preserving the results produced during cancellation.
    let journal_writable = !matches!(
        connection_result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<JournalError>()),
        Some(JournalError::Io { .. } | JournalError::Corrupt { .. })
    );
    drop(completed_tx);
    let cleanup_result = settle_invocations(
        &mut tasks,
        &mut completed_rx,
        &mut journal,
        journal_writable,
    )
    .await;
    if let Err(error) = &cleanup_result {
        tracing::error!(component = "edge", operation = "settle_invocations", stage = "cleanup", error = %error, "Edge invocation cleanup failed");
    }
    connection_result.and(cleanup_result)
}

async fn persist_completion(
    journal: &mut EdgeInvocationJournal,
    completed: CompletedEdgeInvocation,
) -> Result<invocation_journal::PendingResult, JournalError> {
    let pending = journal
        .complete(
            &completed.request_id,
            completed.generation,
            DurableEdgeResult::from_tool_result(completed.result, completed.duration_ms),
        )
        .await?;
    tracing::info!(
        request_id = %completed.request_id,
        generation = completed.generation,
        duration_ms = completed.duration_ms,
        is_error = pending.result.is_error,
        output_len = pending.result.output.len(),
        "Tool execution complete"
    );
    Ok(pending)
}

async fn settle_invocations(
    tasks: &mut JoinSet<()>,
    completed_rx: &mut mpsc::Receiver<CompletedEdgeInvocation>,
    journal: &mut EdgeInvocationJournal,
    mut journal_writable: bool,
) -> Result<(), EdgeConnectionError> {
    let mut failure: Option<EdgeConnectionError> = None;
    // Spawned tasks continue running while we receive. Drain before joining:
    // queued completions have already released their execution permits, so
    // even a queue larger than the concurrency budget can fill up.
    // The connection must drop its sender before calling this function.
    while let Some(completed) = completed_rx.recv().await {
        if journal_writable {
            let request_id = completed.request_id.clone();
            if let Err(error) = persist_completion(journal, completed).await {
                tracing::error!(component = "edge", operation = "settle_invocations", stage = "persist_result", request_id = %request_id, error = %error, "Failed to persist completion during connection cleanup");
                journal_writable = false;
                failure = Some(Box::new(error));
            }
        }
    }
    while let Some(joined) = tasks.join_next().await {
        if let Err(error) = joined {
            tracing::error!(component = "edge", operation = "settle_invocations", stage = "join", error = %error, "Edge invocation task failed during cleanup");
            if failure.is_none() {
                failure = Some(Box::new(error));
            }
        }
    }
    failure.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoDispatchExecutor;
    impl EdgeInvocationExecutor for NoDispatchExecutor {
        fn execute(
            &self,
            _: EdgeInvocation,
            _: CancellationToken,
        ) -> BoxFuture<'_, astra_tools::ToolResult> {
            Box::pin(async { panic!("readiness failure must not dispatch a tool") })
        }
    }

    #[tokio::test]
    async fn failed_recovery_and_cancelled_installation_never_publish_ready() {
        for cancelled in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let journal_path = directory.path().join("journal.json");
            tokio::fs::write(&journal_path, b"invalid journal")
                .await
                .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let _ = socket.next().await;
            });
            let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
                .await
                .unwrap();
            let (ready, receiver) = oneshot::channel();
            let shutdown = CancellationToken::new();
            if cancelled {
                shutdown.cancel();
            }
            let result = serve_connection(
                socket,
                EdgeConnectionContext {
                    account_id: "account".into(),
                    edge_agent_id: "edge".into(),
                    workspace_dir: directory.path().to_owned(),
                    journal_path,
                    ready: Some(ready),
                },
                Arc::new(NoDispatchExecutor),
                shutdown,
            )
            .await;
            assert_eq!(result.is_ok(), cancelled);
            assert!(
                receiver.await.is_err(),
                "failure/cancellation cannot install capacity"
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_authentication_wait_without_returning_a_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (received, request_received) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            assert!(socket.next().await.unwrap().is_ok());
            received.send(()).unwrap();
            let _ = socket.next().await;
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let client = tokio::spawn(async move {
            authenticate_connection(
                socket,
                EdgeClientMessage::Auth {
                    edge_agent_id: "edge".into(),
                    materialization_id: "materialization".into(),
                    interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                    hostname: None,
                    workspace_dir: None,
                    capabilities: None,
                },
                Some("account"),
                &task_cancellation,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), request_received)
            .await
            .unwrap()
            .unwrap();
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(3), client)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(EdgeAuthenticationError::Cancelled)));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn authentication_uses_server_identity_and_rejects_untrusted_acknowledgements() {
        let cases = [
            (
                serde_json::json!({"type":"edge_auth_ok","user_id":"account","interaction_api_major":astra_server_types::AGENT_INTERACTION_API_MAJOR}),
                None,
            ),
            (
                serde_json::json!({"type":"edge_auth_ok","user_id":"other","interaction_api_major":astra_server_types::AGENT_INTERACTION_API_MAJOR}),
                Some("mismatch"),
            ),
            (
                serde_json::json!({"type":"edge_auth_ok","user_id":" ","interaction_api_major":astra_server_types::AGENT_INTERACTION_API_MAJOR}),
                Some("account"),
            ),
            (
                serde_json::json!({"type":"edge_auth_ok","user_id":"account","interaction_api_major":"invalid"}),
                Some("contract"),
            ),
            (
                serde_json::json!({"type":"edge_auth_error","message":"private server details"}),
                Some("rejected"),
            ),
            (Value::Null, Some("closed")),
        ];
        for (response, failure) in cases {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let request = socket.next().await.unwrap().unwrap();
                let Message::Text(text) = request else {
                    panic!("expected authentication envelope")
                };
                assert!(matches!(
                    serde_json::from_str::<EdgeClientMessage>(&text).unwrap(),
                    EdgeClientMessage::Auth { .. }
                ));
                if response.is_null() {
                    socket.send(Message::Close(None)).await.unwrap();
                } else {
                    socket
                        .send(Message::Text(response.to_string().into()))
                        .await
                        .unwrap();
                }
            });
            let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
                .await
                .unwrap();
            let auth = EdgeClientMessage::Auth {
                edge_agent_id: "edge".into(),
                materialization_id: "materialization".into(),
                interaction_api_major: astra_server_types::AGENT_INTERACTION_API_MAJOR.into(),
                hostname: None,
                workspace_dir: None,
                capabilities: None,
            };
            let result =
                authenticate_connection(socket, auth, Some("account"), &CancellationToken::new())
                    .await;
            match failure {
                None => assert_eq!(result.unwrap().1, "account"),
                Some(kind) => {
                    let error = result.unwrap_err();
                    assert_eq!(error.is_permanent(), kind != "closed");
                    assert!(!error.to_string().contains("private server details"));
                    assert!(matches!(
                        (kind, error),
                        ("mismatch", EdgeAuthenticationError::AccountMismatch)
                            | ("account", EdgeAuthenticationError::InvalidAccount)
                            | ("contract", EdgeAuthenticationError::IncompatibleContract)
                            | ("rejected", EdgeAuthenticationError::Rejected)
                            | (
                                "closed",
                                EdgeAuthenticationError::ClosedBeforeAuthentication
                            )
                    ));
                }
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn cleanup_drains_a_full_completion_queue_and_preserves_results() {
        assert_cleanup_drains(false).await;
    }

    #[tokio::test]
    async fn cleanup_drains_senders_even_when_persistence_fails() {
        assert_cleanup_drains(true).await;
    }

    async fn assert_cleanup_drains(fail_first_completion: bool) {
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("journal.json");
        let mut journal = EdgeInvocationJournal::open(path.clone()).await.unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        let mut tasks = JoinSet::new();
        for i in 0..4 {
            let identity = astra_server_types::edge_ws_protocol::ToolInvocationIdentity::new(
                "user",
                "session",
                "run",
                "turn",
                format!("completion-{i}"),
            )
            .unwrap();
            let request_id = identity.storage_key();
            journal
                .prepare(
                    &request_id,
                    &identity,
                    1,
                    "bash",
                    &serde_json::json!({}),
                    true,
                    None,
                )
                .await
                .unwrap();
            let completion = CompletedEdgeInvocation {
                request_id: if fail_first_completion && i == 0 {
                    "missing-record".into()
                } else {
                    request_id
                },
                generation: 1,
                result: astra_tools::ToolResult::text(format!("finished-{i}")),
                duration_ms: i,
            };
            if i == 0 {
                tx.send(completion).await.unwrap();
            } else {
                let tx = tx.clone();
                tasks.spawn(async move {
                    tx.send(completion).await.unwrap();
                });
            }
        }
        assert_eq!(rx.len(), 1);
        drop(tx);
        let settled = tokio::time::timeout(
            Duration::from_secs(5),
            settle_invocations(&mut tasks, &mut rx, &mut journal, true),
        )
        .await
        .unwrap();
        assert!(tasks.is_empty());
        if fail_first_completion {
            let error = settled.unwrap_err();
            assert!(matches!(
                error.downcast_ref::<JournalError>(),
                Some(JournalError::Corrupt { .. })
            ));
            assert_eq!(
                journal.status().running,
                4,
                "stop appending after integrity failure"
            );
            drop(journal);
            let restored = EdgeInvocationJournal::open(path).await.unwrap();
            let pending = restored.pending_results().unwrap();
            assert_eq!(pending.len(), 4);
            assert!(pending.iter().all(
                |result| result.result.tool_result_fields.as_ref().unwrap()["outcome_certainty"]
                    == "unknown"
            ));
            return;
        }
        settled.unwrap();
        drop(journal);
        let restored = EdgeInvocationJournal::open(path).await.unwrap();
        let mut outputs = restored
            .pending_results()
            .unwrap()
            .into_iter()
            .map(|pending| {
                assert!(!pending.result.is_error);
                pending.result.output
            })
            .collect::<Vec<_>>();
        outputs.sort();
        assert_eq!(
            outputs,
            ["finished-0", "finished-1", "finished-2", "finished-3"]
        );
    }

    #[test]
    fn edge_work_deadline_is_absolute_and_not_the_generic_command_timeout() {
        let work_deadline = Instant::now() + Duration::from_secs(600);
        assert!(
            command_deadline(work_deadline, 60, Some(10_000), false)
                .saturating_duration_since(Instant::now())
                <= Duration::from_secs(10)
        );
        assert_eq!(
            command_deadline(work_deadline, 60, Some(10_000), true),
            work_deadline
        );
        let unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(edge_execution_deadline(30, Some(unix_ms - 1), Some(60_000)).is_err());
        assert!(edge_execution_deadline(30, Some(unix_ms + 60_000), None).is_err());
        assert!(edge_execution_deadline(30, None, Some(60_000)).is_err());
        assert!(edge_execution_deadline(30, Some(unix_ms + 60_000), Some(0)).is_err());
        let deadline =
            edge_execution_deadline(30, Some(unix_ms + 86_400_000), Some(43_200_000)).unwrap();
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(remaining > Duration::from_secs(43_199));
        assert!(remaining <= Duration::from_secs(43_200));
        let absolute =
            edge_execution_deadline(30, Some(unix_ms + 1_000), Some(86_400_000)).unwrap();
        assert!(absolute.saturating_duration_since(Instant::now()) <= Duration::from_secs(1));
    }

    #[test]
    fn process_authorization_fails_closed_without_live_credential() {
        use astra_server_types::edge_ws_protocol::RuntimeProcessAuthorizationContext;

        let context = RuntimeProcessAuthorizationContext {
            authorization: "Bearer runtime-grant".to_string(),
        };
        assert!(valid_runtime_process_authorization(
            "bash",
            true,
            Some(&context)
        ));
        assert!(valid_runtime_process_authorization("bash", false, None));
        assert!(!valid_runtime_process_authorization("bash", true, None));
        assert!(!valid_runtime_process_authorization(
            "read_file",
            true,
            Some(&context)
        ));
    }

    #[test]
    fn invocation_tracker_deduplicates_and_fences_stale_completions() {
        let mut tracker = EdgeInvocationTracker::default();
        let generation = 7;
        tracker.begin("request-1", generation).unwrap();
        assert_eq!(tracker.begin("request-1", 8).unwrap_err(), generation);
        assert!(!tracker.finish_if_current("request-1", generation + 1));
        assert_eq!(tracker.begin("request-1", 8).unwrap_err(), generation);
        assert!(tracker.finish_if_current("request-1", generation));

        let next_generation = generation + 1;
        tracker.begin("request-1", next_generation).unwrap();
        assert!(!tracker.finish_if_current("request-1", generation));
        assert!(tracker.finish_if_current("request-1", next_generation));
    }

    #[test]
    fn invocation_tracker_routes_cancellation_to_the_exact_active_request() {
        let mut tracker = EdgeInvocationTracker::default();
        let first_generation = 1;
        let first_cancel = tracker.begin("request-1", first_generation).unwrap();
        let second_cancel = tracker.begin("request-2", 2).unwrap();

        assert!(!tracker.cancel_if_current("request-1", first_generation + 1));
        assert!(!first_cancel.is_cancelled());
        assert!(tracker.cancel_if_current("request-1", first_generation));
        assert!(first_cancel.is_cancelled());
        assert!(!second_cancel.is_cancelled());
        assert!(!tracker.cancel_if_current("missing", first_generation));
    }

    #[test]
    fn execution_budget_admits_exactly_the_configured_concurrency() {
        let budget = EdgeExecutionBudget::new();
        let permits = (0..MAX_CONCURRENT_TOOL_EXECUTIONS)
            .map(|_| budget.try_acquire().expect("configured execution permit"))
            .collect::<Vec<_>>();
        assert!(
            budget.try_acquire().is_none(),
            "the first invocation beyond the execution budget must be rejected before dispatch"
        );
        drop(permits);
        assert!(
            budget.try_acquire().is_some(),
            "completed executions must release capacity"
        );
    }
}
