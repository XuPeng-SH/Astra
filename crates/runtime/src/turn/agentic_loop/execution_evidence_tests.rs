use super::*;
use crate::turn::agentic_loop::host::run_agentic_loop_with_host;
use crate::turn::agentic_loop::host::tests::{
    MockHost, edge_tool_result, make_edge_tool_with_args, make_state, text_result,
};
use astra_config::user_profile::{MutationCompletionScope, TurnIntent};
use astra_services::session_journal::{ToolCallDisposition, ToolCallRecord};
use serde_json::json;

async fn run_written_go_verification(
    command: &str,
    result_class: &str,
    exit_semantics: &str,
) -> (AgenticLoopState, MockHost) {
    let write = make_edge_tool_with_args(
        "write_file",
        json!({"path": "/workspace/main.go", "content": "package main\n"}),
        "written",
    );
    let mut checked =
        make_edge_tool_with_args("bash", json!({"command": command}), "observed result");
    checked.tool_result_fields.as_mut().unwrap().extend([
        (
            "exit_code".into(),
            json!(if exit_semantics == "success" { 0 } else { 1 }),
        ),
        ("result_class".into(), json!(result_class)),
        ("exit_semantics".into(), json!(exit_semantics)),
    ]);
    let mut host = MockHost::new(vec![
        edge_tool_result(vec![write], 10, 2, None),
        edge_tool_result(vec![checked], 10, 2, None),
        text_result("Implemented and checked the change.", 10, 2, None),
        text_result("Implemented and checked the change.", 10, 2, None),
        text_result("Implemented and checked the change.", 10, 2, None),
    ])
    .with_valid_tools(&["write_file", "bash", "read_file"]);
    let mut state = make_state();
    state.hooks.workspace_root_hint = Some("/workspace".into());
    state.turn_intent =
        Some(TurnIntent::default().with_workspace_mutation(WorkspaceMutationIntent::MustMutate));
    run_agentic_loop_with_host(&mut host, &mut state)
        .await
        .unwrap();
    (state, host)
}

#[tokio::test]
async fn go_verification_after_write_finishes_without_redundant_observation() {
    for command in [
        "go test ./...",
        "mkdir -p /tmp/astra-cache && go test ./...",
        "mkdir -p /tmp/astra-cache && GOWORK=off go test ./... 2>&1 | tail -10",
        "GOWORK=off go test ./...",
        "GOWORK=off go test -skip TestIntegration -covermode=atomic -coverpkg=./... ./...",
        "GOWORK=off go build ./... && GOWORK=off go vet ./... && echo BUILD_VET_OK",
        "cd /workspace && GOWORK=off go build -mod=readonly ./pkg/one/ ./pkg/two/ && GOWORK=off go vet -mod=readonly ./pkg/one/ ./pkg/two/ && echo BUILD_VET_OK",
    ] {
        let (state, host) = run_written_go_verification(command, "success", "success").await;
        assert!(
            state.interruption.is_none(),
            "{command}: {:?}",
            state.interruption
        );
        assert_eq!(
            host.turn_count(),
            3,
            "{command}: no redundant recovery rounds"
        );
        assert_eq!(state.stall.tool_call_records.len(), 2);
        assert_eq!(
            host.final_output_ready,
            vec!["Implemented and checked the change."]
        );
    }
}

#[tokio::test]
async fn go_verification_does_not_complete_when_a_predecessor_failed_or_changed_configuration() {
    for (command, class, semantics) in [
        (
            "sh -c 'exit 1' && GOWORK=off go test ./...",
            "inconclusive",
            "domain_negative",
        ),
        (
            "GOWORK=off go test ./... | head -1",
            "success",
            "pipeline_truncated",
        ),
        (
            "export GOFLAGS=-n && GOWORK=off go test ./...",
            "success",
            "success",
        ),
        ("true || GOWORK=off go test ./...", "success", "success"),
        (
            "mkdir -p /tmp/astra-cache && go test ./...",
            "inconclusive",
            "domain_negative",
        ),
        (
            "mkdir -p /tmp/astra-cache && go test ./...",
            "test_failure",
            "domain_negative",
        ),
        (
            "mkdir -p /tmp/astra-cache | cat && go test ./...",
            "success",
            "success",
        ),
        (
            "GOFLAGS=$(cat /tmp/flags) && mkdir -p /tmp/astra-cache && go test ./...",
            "success",
            "success",
        ),
        (
            "go env -w GOFLAGS=-n && mkdir -p /tmp/astra-cache && go test ./...",
            "success",
            "success",
        ),
        (
            "go generate ./... && mkdir -p /tmp/astra-cache && go test ./...",
            "success",
            "success",
        ),
        (
            "printf -v PATH /tmp/fakebin && mkdir -p /tmp/astra-cache && go test ./...",
            "success",
            "success",
        ),
        (
            "printf '%n' PATH && mkdir -p /tmp/astra-cache && go test ./...",
            "success",
            "success",
        ),
        (
            "mkdir -p /tmp/astra-cache && export PATH=/tmp/fakebin && go test ./... && cargo test",
            "success",
            "success",
        ),
    ] {
        let (state, host) = run_written_go_verification(command, class, semantics).await;
        assert_eq!(
            state.interruption.as_ref().map(|record| record.kind),
            Some(InterruptionKind::ExecutionIncomplete),
            "{command}"
        );
        assert_eq!(host.turn_count(), 5);
        assert_eq!(
            state.stall.tool_call_records.len(),
            2,
            "no invented recovery tool"
        );
    }
}

fn external_request() -> AgenticLoopState {
    let mut state = make_state();
    state.task_profile =
        astra_turn_core::chat_turn_heuristics::TaskExecutionProfile::from_structured_intent(
            true,
            false,
            astra_turn_core::chat_turn_heuristics::TaskComplexity::Standard,
        );
    state.turn_intent = Some(
        TurnIntent::default()
            .with_workspace_mutation(WorkspaceMutationIntent::MustMutate)
            .with_mutation_completion_scope(MutationCompletionScope::External),
    );
    state
}

#[tokio::test]
async fn external_result_and_already_satisfied_state_finish_without_another_mutation() {
    for command in [
        "git worktree remove --force /tmp/review",
        "test ! -e /tmp/review",
    ] {
        let mut result = make_edge_tool_with_args("bash", json!({"command": command}), "absent");
        astra_tools::execution_outcome::insert_started_execution_fact(
            result.tool_result_fields.as_mut().unwrap(),
        );
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![result], 10, 2, None),
            text_result("The tool reports that the checkout is absent.", 10, 2, None),
        ])
        .with_valid_tools(&["bash"]);
        let mut state = external_request();
        let budget = state.max_turns;
        let outcome = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        assert!(matches!(outcome, AgenticLoopOutcome::Completed));
        assert_eq!(
            host.turn_count(),
            2,
            "no extra mutation/recovery round: {command}"
        );
        assert_eq!(state.max_turns, budget);
        assert!(state.interruption.is_none(), "{:?}", state.interruption);
        assert_eq!(state.stall.tool_call_records.len(), 1);
        assert_eq!(
            state.stall.tool_call_records[0].disposition,
            Some(ToolCallDisposition::Executed)
        );
        assert!(state.final_text.contains("checkout is absent"));
    }
}

#[test]
fn mixed_request_retains_only_its_workspace_completion_owner() {
    let mut state = external_request();
    assert_eq!(pending_completion_action(&state).unwrap(), None);
    state
        .turn_intent
        .as_mut()
        .unwrap()
        .mutation_completion_scope = MutationCompletionScope::Mixed;
    assert_eq!(
        pending_completion_action(&state).unwrap(),
        Some(CompletionAction::RequiredWorkspaceMutation)
    );
}

#[test]
fn unrelated_success_cannot_clear_started_cancellation() {
    for disposition in [ToolCallDisposition::Rejected, ToolCallDisposition::Executed] {
        let mut state = external_request();
        state.final_text = "Operation result remains uncertain.".into();
        let args = json!({"command": "perform-external-operation"}).to_string();
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: false,
            runtime_args_full: Some(args),
            error_kind: Some(astra_core::ErrorKind::Cancelled),
            disposition: Some(disposition),
            ..Default::default()
        });
        state.stall.tool_call_records.push(ToolCallRecord {
            name: "bash".into(),
            ok: true,
            runtime_args_full: Some(json!({"command": "test -e /tmp/other"}).to_string()),
            disposition: Some(ToolCallDisposition::Executed),
            ..Default::default()
        });
        assert!(!enforce_workspace_completion_before_text_completion(
            &mut state
        ));
        assert_eq!(
            state.interruption.is_some(),
            disposition == ToolCallDisposition::Executed
        );
        if disposition == ToolCallDisposition::Executed {
            assert_eq!(
                state.interruption.as_ref().unwrap().kind,
                InterruptionKind::ExecutionIncomplete
            );
            assert!(state.final_text.contains("effects remain unverified"));
        }
    }
}

#[tokio::test]
async fn callback_outcome_facts_survive_journaling_and_control_completion() {
    // Use the shared producer, not hand-written rejection metadata. This is
    // the same callback body/metadata the CLI sends after cancellation.
    for (started, unknown) in [(false, false), (true, false), (false, true)] {
        let mut result =
            make_edge_tool_with_args("bash", json!({"command": "perform-external-operation"}), "");
        let cancelled = astra_tools::cancelled_tool_result("bash", started);
        result.output = cancelled.output;
        result
            .tool_result_fields
            .as_mut()
            .unwrap()
            .extend(cancelled.metadata.unwrap());
        if unknown {
            let fields = result.tool_result_fields.as_mut().unwrap();
            // A lost worker/result cannot assert that dispatch never occurred.
            for key in [
                "status",
                "cancelled",
                "reason",
                "execution_started",
                "side_effects_maybe",
            ] {
                fields.remove(key);
            }
            fields.remove("disposition");
            fields.insert("error_kind".into(), json!("tool_outcome_unknown"));
            fields.insert("execution_fact".into(), json!("unknown"));
            fields.insert("side_effects_maybe".into(), json!(true));
            result.output = "Error: shell acknowledgement lost; reconcile outcome".into();
        }
        result.status = "failed".into();
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![result], 10, 2, None),
            text_result("The operation succeeded.", 10, 2, None),
        ])
        .with_valid_tools(&["bash"]);
        let mut state = external_request();
        let _ = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        let record = &state.stall.tool_call_records[0];
        assert_eq!(
            record.error_kind,
            Some(if unknown {
                astra_core::ErrorKind::ToolOutcomeUnknown
            } else {
                astra_core::ErrorKind::Cancelled
            })
        );
        assert_eq!(record.was_executed(), started || unknown);
        assert_eq!(
            state.interruption.is_some(),
            started || unknown,
            "{record:?}"
        );
        assert_eq!(state.stall.tool_call_records.len(), 1, "no recovery write");
        if started || unknown {
            let interruption = state.interruption.as_ref().unwrap();
            assert_eq!(interruption.kind, InterruptionKind::ExecutionIncomplete);
            assert!(
                interruption
                    .error_detail
                    .as_deref()
                    .unwrap()
                    .contains("interrupted or unknown outcome")
            );
            assert!(!record.ok);
            // Interruption is lifecycle truth. Provider content stays a
            // partial answer; it cannot promote the operation to success.
            assert_eq!(state.final_text, "The operation succeeded.");
            assert_eq!(host.final_output_ready, vec![state.final_text.clone()]);
        }
    }
}

#[tokio::test]
async fn timeout_callback_requires_reconciliation_only_for_started_side_effects() {
    // The callback and journal must preserve the executor's timeout and startup
    // facts. A later successful observation cannot settle the earlier operation.
    for (command, started, incomplete) in [
        ("perform-external-operation", true, true),
        ("perform-external-operation", false, false),
        ("pwd", true, false),
    ] {
        let mut result = make_edge_tool_with_args("bash", json!({"command": command}), "");
        let fields = result.tool_result_fields.as_mut().unwrap();
        fields.insert("error_kind".into(), json!("tool_timeout"));
        if started {
            astra_tools::execution_outcome::insert_started_execution_fact(fields);
        } else {
            astra_tools::execution_outcome::insert_not_executed_fact(fields);
        }
        result.status = "failed".into();
        result.output = "Error: command timed out".into();
        let mut observed =
            make_edge_tool_with_args("bash", json!({"command": "pwd"}), "/workspace");
        observed.request_id = "req-bash-probe".into();
        let mut host = MockHost::new(vec![
            edge_tool_result(vec![result], 10, 2, None),
            edge_tool_result(vec![observed], 10, 2, None),
            text_result("Preserved partial report.", 10, 2, None),
        ])
        .with_valid_tools(&["bash"]);
        let mut state = external_request();
        let budget = state.max_turns;
        let _ = run_agentic_loop_with_host(&mut host, &mut state)
            .await
            .unwrap();
        let record = &state.stall.tool_call_records[0];
        assert_eq!(record.error_kind, Some(astra_core::ErrorKind::ToolTimeout));
        assert_eq!(record.was_executed(), started);
        assert!(!record.ok);
        assert!(
            state.stall.tool_call_records[1].ok,
            "the unrelated probe succeeded"
        );
        assert_eq!(state.interruption.is_some(), incomplete, "{record:?}");
        if incomplete {
            assert_eq!(
                state.interruption.as_ref().unwrap().kind,
                InterruptionKind::ExecutionIncomplete
            );
        }
        assert_eq!(
            host.turn_count(),
            3,
            "no automatic replay or recovery round"
        );
        assert_eq!(state.max_turns, budget);
        assert_eq!(state.stall.tool_call_records.len(), 2);
        assert_eq!(state.final_text, "Preserved partial report.");
        assert_eq!(host.final_output_ready, vec![state.final_text.clone()]);
    }
}
