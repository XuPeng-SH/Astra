//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services --test runner_inference_db_it -- --ignored --test-threads=1
mod common;

use astra_services::inference_execution::runner::*;
use astra_services::inference_execution::*;
use astra_services::models::{
    AdmittedModelExecution, ModelAccessKind, ModelExecutionMaterial, ModelExecutionPlacement,
};
use astra_services::runner_model_bindings::*;
use astra_turn_types::runner_inference::*;
use astra_turn_types::{InferenceInvocationScope, InferencePurpose};
use serial_test::serial;
use sqlx::Row;

const REQUEST: &[u8] =
    br#"{ "model":"model", "messages":[{"role":"user","content":"private-request-canary"}] }"#;
const RESPONSE: &[u8] = br#"{"content":"private-response-canary","complete":true}"#;

struct Fixture {
    pool: astra_core::SharedPool,
    connection: AuthenticatedRunnerConnection,
    binding: ResolvedRunnerModelBinding,
    input: InferenceInvocationInput,
}

fn id(value: &str) -> RunnerInferenceId {
    RunnerInferenceId::new(value).unwrap()
}

impl Fixture {
    async fn new() -> Self {
        let pool = common::setup_pool().await;
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let user = format!("runner-user-{suffix}");
        let session = format!("session-{suffix}");
        let run = format!("run-{suffix}");
        sqlx::query("INSERT INTO agent_sessions (session_id, user_id, status, event_count, project_retention_policy,
            created_at, updated_at, last_active_at) VALUES (?, ?, 'active', 0, 'session', NOW(6), NOW(6), NOW(6))")
            .bind(&session).bind(&user).execute(pool.get()).await.unwrap();
        sqlx::query("INSERT INTO agent_runs
            (run_id, user_id, session_id, root_run_id, ancestor_path, depth, retry_scope, status, execution_mode,
             owner_pod_id, owner_lease_expires_at, run_generation, last_event_idx, retry_count,
             total_prompt_tokens, total_completion_tokens, total_tool_calls, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, 0, 'node', 'running', 'web_agent', 'run-owner', DATE_ADD(NOW(6), INTERVAL 5 MINUTE),
             0, -1, 0, 0, 0, 0, NOW(6), NOW(6))")
            .bind(&run).bind(&user).bind(&session).bind(&run).bind(&run).execute(pool.get()).await.unwrap();
        sqlx::query("INSERT INTO edge_agent_registry (user_id, registry_id, edge_agent_id, edge_id, registration_state)
            VALUES (?, 'registry', 'runner', 'socket-1', 1)")
            .bind(&user).execute(pool.get()).await.unwrap();
        let connection = AuthenticatedRunnerConnection {
            user_id: user.clone(),
            runner_id: id("runner"),
            edge_id: "socket-1".into(),
        };
        enroll_runner_inference(&pool, &connection, 1, &id("journal"), &id("boot-1"))
            .await
            .unwrap();
        let publication: RunnerInferenceBindingPublication = serde_json::from_value(serde_json::json!({
            "protocol_version":1,"operation_id":"publish","expected_publication_revision":0,
            "change":{"action":"publish","definition":{
                "identity":{"runner_id":"runner","journal_id":"journal","binding_id":"model","binding_revision":1,"profile_revision":1},
                "model_name":"model","protocol":"openai_chat_completions","context_window":8192,"max_output_tokens":1024
            }}
        })).unwrap();
        publish_runner_binding(&pool, &connection, &publication)
            .await
            .unwrap();
        let binding = resolve_runner_model_binding(&pool, &user, publication.change.identity())
            .await
            .unwrap();
        let input = InferenceInvocationInput {
            user_id: user.clone(),
            scope: InferenceInvocationScope::Run {
                session_id: session,
                run_id: run,
                turn: 0,
                round: 0,
                operation_id: "primary".into(),
                logical_attempt: 0,
            },
            run_authority: Some(InferenceRunAdmissionAuthority {
                expected_owner_generation: 0,
                expected_owner_pod_id: "run-owner".into(),
                expected_control_epoch: -1,
            }),
            offering_id: runner_offering_id(&user, &binding.definition.identity),
            resolved_model_name: "model".into(),
            upstream_model_name: "model".into(),
            provider: "openai".into(),
            purpose: InferencePurpose::PrimaryAgent,
            execution_placement: ModelExecutionPlacement::Edge,
            access_kind: ModelAccessKind::ThisDevice,
        };
        Self {
            pool,
            connection,
            binding,
            input,
        }
    }

    fn plan(&self) -> RunnerInferenceDispatchPlan {
        plan_runner_inference_dispatch(
            self.input.clone(),
            &self.binding,
            REQUEST,
            (chrono::Utc::now().timestamp_millis() + 120_000) as u64,
        )
        .unwrap()
    }

    async fn admit(&self) -> (RunnerInferenceDispatchPlan, RunnerInferenceDispatchGrant) {
        let plan = self.plan();
        let grant = admit_runner_inference_dispatch(&self.pool, &plan)
            .await
            .unwrap();
        (plan, grant)
    }
}

fn terminal() -> InferenceInvocationTerminal {
    InferenceInvocationTerminal {
        status: InferenceTerminalStatus::Succeeded,
        usage: InferenceUsage {
            input: astra_turn_types::NormalizedPromptCacheUsage::new(7, 3, 0),
            output_tokens: 5,
        },
        usage_status: InferenceUsageStatus::ProviderExact,
        provider_response_id: Some("response-id".into()),
        error_kind: None,
        error_message: None,
    }
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
#[serial]
async fn runner_grants_pin_exact_private_request_and_fence_owner_session_boot_and_start() {
    let f = Fixture::new().await;
    let (plan, grant) = f.admit().await;
    assert!(!format!("{plan:?}").contains("private-request-canary"));
    assert!(
        !serde_json::to_string(&grant)
            .unwrap()
            .contains("private-request-canary")
    );
    assert_eq!(
        admit_runner_inference_dispatch(&f.pool, &plan)
            .await
            .unwrap(),
        grant
    );
    let bytes = load_runner_request_custody(&f.pool, &f.connection, &grant)
        .await
        .unwrap();
    assert_eq!(
        bytes.as_bytes(),
        REQUEST,
        "preserve exact whitespace and object ordering"
    );
    assert!(!format!("{bytes:?}").contains("private-request-canary"));
    let mut foreign = f.connection.clone();
    foreign.user_id = "another-owner".into();
    assert!(
        load_runner_request_custody(&f.pool, &foreign, &grant)
            .await
            .is_err()
    );
    let mut forged = grant.clone();
    if let InferenceInvocationScope::Run { session_id, .. } = &mut forged.attempt.scope {
        *session_id = "other-session".into();
    }
    assert!(
        load_runner_request_custody(&f.pool, &f.connection, &forged)
            .await
            .is_err()
    );
    let claim = claim_runner_delivery(&f.pool, &f.connection, &grant.attempt)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claim.action, RunnerDeliveryAction::Dispatch(grant.clone()));
    assert!(
        claim_runner_delivery(&f.pool, &f.connection, &grant.attempt)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        record_runner_start_evidence(
            &f.pool,
            &f.connection,
            &grant,
            RunnerInferenceStartEvidence::ProviderStarted
        )
        .await
        .is_err()
    );
    assert!(
        record_runner_start_evidence(
            &f.pool,
            &f.connection,
            &grant,
            RunnerInferenceStartEvidence::ExpiredWithoutFence
        )
        .await
        .is_err()
    );
    record_runner_start_evidence(
        &f.pool,
        &f.connection,
        &grant,
        RunnerInferenceStartEvidence::FenceCommitted,
    )
    .await
    .unwrap();
    record_runner_start_evidence(
        &f.pool,
        &f.connection,
        &grant,
        RunnerInferenceStartEvidence::ProviderStarted,
    )
    .await
    .unwrap();
    request_runner_cancellation(&f.pool, &f.connection.user_id, &grant.attempt)
        .await
        .unwrap();
    assert!(
        record_runner_start_evidence(
            &f.pool,
            &f.connection,
            &grant,
            RunnerInferenceStartEvidence::CancelledWithoutFence
        )
        .await
        .is_err()
    );
    assert_eq!(
        claim_runner_delivery(&f.pool, &f.connection, &grant.attempt)
            .await
            .unwrap()
            .unwrap()
            .action,
        RunnerDeliveryAction::Cancel(grant)
    );
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
#[serial]
async fn runner_late_custody_survives_owner_loss_and_reconnect_then_consumes_once_atomically() {
    let mut f = Fixture::new().await;
    let (plan, grant) = f.admit().await;
    sqlx::query("UPDATE inference_invocations SET owner_lease_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND)
        WHERE user_id = ? AND invocation_id = ?")
        .bind(&f.connection.user_id).bind(grant.attempt.invocation_id.as_str()).execute(f.pool.get()).await.unwrap();
    reconcile_inference_settlements(&f.pool, 32).await.unwrap();
    let status: String = sqlx::query_scalar(
        "SELECT status FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ?",
    )
    .bind(&f.connection.user_id)
    .bind(grant.attempt.attempt_id.as_str())
    .fetch_one(f.pool.get())
    .await
    .unwrap();
    assert_eq!(
        status, "started",
        "Server lease expiry is not remote evidence"
    );
    sqlx::query("UPDATE edge_agent_registry SET edge_id = 'socket-2' WHERE user_id = ?")
        .bind(&f.connection.user_id)
        .execute(f.pool.get())
        .await
        .unwrap();
    let old = f.connection.clone();
    f.connection.edge_id = "socket-2".into();
    enroll_runner_inference(&f.pool, &f.connection, 1, &id("journal"), &id("boot-2"))
        .await
        .unwrap();
    let terminal = terminal();
    let hash = runner_terminal_digest(&terminal, RESPONSE).unwrap();
    assert!(
        take_runner_terminal_custody(&f.pool, &old, &grant.attempt, &terminal, RESPONSE, &hash)
            .await
            .is_err()
    );
    let ack = take_runner_terminal_custody(
        &f.pool,
        &f.connection,
        &grant.attempt,
        &terminal,
        RESPONSE,
        &hash,
    )
    .await
    .unwrap();
    assert_eq!(
        take_runner_terminal_custody(
            &f.pool,
            &f.connection,
            &grant.attempt,
            &terminal,
            RESPONSE,
            &hash
        )
        .await
        .unwrap(),
        ack
    );
    let pending = list_pending_runner_continuations(&f.pool, 128)
        .await
        .unwrap();
    assert!(pending.contains(&grant.attempt));
    sqlx::query("UPDATE agent_runs SET run_generation = 1, owner_pod_id = 'recovered-owner' WHERE user_id = ? AND run_id = ?")
        .bind(&f.connection.user_id).bind(f.input.scope.run_id()).execute(f.pool.get()).await.unwrap();
    assert!(
        claim_runner_continuation(&f.pool, f.input.clone(), &grant.attempt, None)
            .await
            .is_err(),
        "stale run generation cannot consume late custody"
    );
    f.input.run_authority = Some(InferenceRunAdmissionAuthority {
        expected_owner_generation: 1,
        expected_owner_pod_id: "recovered-owner".into(),
        expected_control_epoch: -1,
    });
    let claim = claim_runner_continuation(&f.pool, f.input.clone(), &grant.attempt, None)
        .await
        .unwrap();
    assert!(claim.invocation().owner_generation() > plan.invocation().owner_generation());
    assert_eq!(
        load_runner_response_custody(&f.pool, &claim)
            .await
            .unwrap()
            .as_bytes(),
        RESPONSE
    );
    assert!(
        claim_runner_continuation(
            &f.pool,
            f.input.clone(),
            &grant.attempt,
            Some(plan.invocation().owner_token())
        )
        .await
        .is_err()
    );
    let mut tx = f.pool.get().begin().await.unwrap();
    consume_runner_continuation_tx(&mut tx, &claim)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert!(
        list_pending_runner_continuations(&f.pool, 128)
            .await
            .unwrap()
            .contains(&grant.attempt),
        "rollback retains continuation obligation"
    );
    let mut tx = f.pool.get().begin().await.unwrap();
    consume_runner_continuation_tx(&mut tx, &claim)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tx = f.pool.get().begin().await.unwrap();
    assert!(
        consume_runner_continuation_tx(&mut tx, &claim)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    assert!(
        !list_pending_runner_continuations(&f.pool, 128)
            .await
            .unwrap()
            .contains(&grant.attempt)
    );
    assert_eq!(
        take_runner_terminal_custody(
            &f.pool,
            &f.connection,
            &grant.attempt,
            &terminal,
            RESPONSE,
            &hash
        )
        .await
        .unwrap(),
        ack,
        "ACK replay does not recreate continuation"
    );
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
#[serial]
async fn runner_cancelled_run_keeps_real_usage_and_response_without_resuming() {
    let f = Fixture::new().await;
    let (_, grant) = f.admit().await;
    sqlx::query(
        "UPDATE agent_runs SET status = 'cancelled', cancellation_requested_at = NOW(6)
        WHERE user_id = ? AND run_id = ?",
    )
    .bind(&f.connection.user_id)
    .bind(f.input.scope.run_id())
    .execute(f.pool.get())
    .await
    .unwrap();
    let terminal = terminal();
    let hash = runner_terminal_digest(&terminal, RESPONSE).unwrap();
    take_runner_terminal_custody(
        &f.pool,
        &f.connection,
        &grant.attempt,
        &terminal,
        RESPONSE,
        &hash,
    )
    .await
    .unwrap();
    assert!(
        claim_runner_continuation(&f.pool, f.input.clone(), &grant.attempt, None)
            .await
            .is_err()
    );
    discard_cancelled_runner_continuation(&f.pool, &f.connection.user_id, &grant.attempt)
        .await
        .unwrap();
    let row = sqlx::query("SELECT status, input_tokens, output_tokens FROM inference_invocations WHERE user_id = ? AND invocation_id = ?")
        .bind(&f.connection.user_id).bind(grant.attempt.invocation_id.as_str()).fetch_one(f.pool.get()).await.unwrap();
    assert_eq!(row.get::<String, _>("status"), "succeeded");
    assert_eq!(row.get::<i64, _>("input_tokens"), 7);
    assert_eq!(row.get::<i64, _>("output_tokens"), 5);
    let run_status: String =
        sqlx::query_scalar("SELECT status FROM agent_runs WHERE user_id = ? AND run_id = ?")
            .bind(&f.connection.user_id)
            .bind(f.input.scope.run_id())
            .fetch_one(f.pool.get())
            .await
            .unwrap();
    assert_eq!(run_status, "cancelled");
    assert!(
        !list_pending_runner_continuations(&f.pool, 128)
            .await
            .unwrap()
            .contains(&grant.attempt)
    );
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
#[serial]
async fn runner_expiry_never_regrants_and_conflicting_terminal_cannot_overwrite_custody() {
    let f = Fixture::new().await;
    let (_, grant) = f.admit().await;
    sqlx::query("UPDATE inference_provider_attempts SET runner_grant_expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND)
        WHERE user_id = ? AND attempt_id = ?")
        .bind(&f.connection.user_id).bind(grant.attempt.attempt_id.as_str()).execute(f.pool.get()).await.unwrap();
    assert_eq!(
        claim_runner_delivery(&f.pool, &f.connection, &grant.attempt)
            .await
            .unwrap()
            .unwrap()
            .action,
        RunnerDeliveryAction::Reconcile(grant.clone())
    );
    assert!(
        list_runner_reconciliation(&f.pool, &f.connection, 10)
            .await
            .unwrap()
            .contains(&grant)
    );
    let terminal = terminal();
    let hash = runner_terminal_digest(&terminal, RESPONSE).unwrap();
    take_runner_terminal_custody(
        &f.pool,
        &f.connection,
        &grant.attempt,
        &terminal,
        RESPONSE,
        &hash,
    )
    .await
    .unwrap();
    let conflicting = br#"{"content":"different"}"#;
    let conflicting_hash = runner_terminal_digest(&terminal, conflicting).unwrap();
    assert!(
        take_runner_terminal_custody(
            &f.pool,
            &f.connection,
            &grant.attempt,
            &terminal,
            conflicting,
            &conflicting_hash
        )
        .await
        .is_err()
    );
    let row = sqlx::query("SELECT runner_terminal_hash, runner_terminal_conflict FROM inference_provider_attempts WHERE user_id = ? AND attempt_id = ?")
        .bind(&f.connection.user_id).bind(grant.attempt.attempt_id.as_str()).fetch_one(f.pool.get()).await.unwrap();
    assert_eq!(row.get::<String, _>("runner_terminal_hash"), hash.as_str());
    assert!(row.get::<bool, _>("runner_terminal_conflict"));
    assert!(
        !list_pending_runner_continuations(&f.pool, 128)
            .await
            .unwrap()
            .contains(&grant.attempt)
    );
    let material = AdmittedModelExecution::from_runner_binding(f.binding.clone());
    assert!(material.server_material().is_err());
    assert!(matches!(
        material.execution_material,
        ModelExecutionMaterial::Runner(_)
    ));
}
