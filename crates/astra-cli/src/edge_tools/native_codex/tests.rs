use super::*;
use astra_tools::tool_engine::{ToolInvocationAdmissionSource, ToolInvocationMetadata};

// Fixture-owned admission, not proof of the production server grant producer.
fn fixture_execution_ceiling(
    executor: &super::super::ToolExecutor,
) -> astra_server_types::edge_ws_protocol::EdgeExecutionCeiling {
    astra_server_types::edge_ws_protocol::EdgeExecutionCeiling {
        workspace_root: executor
            .effective_project_root()
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned(),
        workspace_id: None,
        materialization_id: None,
        execution_binding_generation: 1,
        runtime_read_paths: Vec::new(),
        workspace_write_allowed: false,
        network_allowed: false,
    }
}

fn stage() -> Stage {
    Stage::parse(&json!({"task": "Review stage", "anchor_run_id": "anchor"})).unwrap()
}

fn test_profile() -> Value {
    permission_profile("/workspace", false, false, &test_requirements()).unwrap()
}

fn test_requirements() -> astra_turn_types::ProviderRuntimeRequirements {
    astra_turn_types::ProviderRuntimeRequirements {
        executable: "/workspace/codex".into(),
        read_paths: vec!["/workspace/codex".into()],
    }
}

#[test]
fn provider_declaration_carries_bounded_runtime_requirements_in_the_existing_extension() {
    let requirements = test_requirements();
    let declaration = provider_declaration(requirements.clone()).unwrap();
    let roundtrip = astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(
        &declaration.extension_fields,
    )
    .unwrap()
    .unwrap();
    assert_eq!(roundtrip, requirements);
    assert_eq!(declaration.native_tool_name, TOOL_NAME);
    assert!(declaration.is_collaborator_stage());
    assert_eq!(
        declaration
            .claims
            .read_only
            .as_ref()
            .map(|claim| claim.value),
        Some(true)
    );
    assert!(
        declaration.input_schema["properties"]
            .get("runtime_read_paths")
            .is_none()
    );
    assert!(
        declaration.input_schema["properties"]
            .get("permissions")
            .is_none()
    );
}

#[test]
fn runtime_grant_requires_exact_installation_local_authority_and_portable_bounds() {
    let workspace = tempfile::tempdir().unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let executable = root.join("codex");
    std::fs::write(&executable, b"fake installed executable").unwrap();
    let text = executable.to_str().unwrap().to_owned();
    let expected = astra_turn_types::ProviderRuntimeRequirements {
        executable: text.clone(),
        read_paths: vec![text.clone()],
    };
    let policy = astra_sandbox::SandboxPolicy::for_project(&root);
    validate_runtime_grant(&expected.read_paths, &expected, Some(&policy)).unwrap();
    assert!(validate_runtime_grant(&[], &expected, Some(&policy)).is_err());
    assert!(validate_runtime_grant(&expected.read_paths, &expected, None).is_err());
    let mut extra = expected.read_paths.clone();
    extra.push("/usr".into());
    assert!(validate_runtime_grant(&extra, &expected, Some(&policy)).is_err());
    let mut too_many = expected.read_paths.clone();
    too_many.resize(33, text.clone());
    assert!(validate_runtime_grant(&too_many, &expected, Some(&policy)).is_err());
    for bad in [
        "/",
        "/tmp/*",
        "~/bin/codex",
        "/tmp/$HOME/codex",
        "/tmp/../bin/codex",
        "relative/codex",
    ] {
        let malformed = astra_turn_types::ProviderRuntimeRequirements {
            executable: bad.into(),
            read_paths: vec![bad.into()],
        };
        assert!(
            validate_runtime_grant(&malformed.read_paths, &malformed, Some(&policy)).is_err(),
            "{bad}"
        );
    }
    #[cfg(unix)]
    {
        let link = root.join("codex-link");
        std::os::unix::fs::symlink(&executable, &link).unwrap();
        let text = link.to_str().unwrap().to_owned();
        let alias = astra_turn_types::ProviderRuntimeRequirements {
            executable: text.clone(),
            read_paths: vec![text],
        };
        assert!(validate_runtime_grant(&alias.read_paths, &alias, Some(&policy)).is_err());
    }
    let other = tempfile::tempdir().unwrap();
    let mut outside_policy = astra_sandbox::SandboxPolicy::for_project(other.path());
    // Temporary directories are normally authorized by the project policy.
    // Exercise an actually disjoint authority, independent of TMPDIR.
    outside_policy.allowed_paths.clear();
    assert!(
        validate_runtime_grant(&expected.read_paths, &expected, Some(&outside_policy)).is_err()
    );
}

#[test]
fn native_profile_is_rootless_and_tracks_admitted_workspace_authority() {
    let read_only = test_profile();
    let config = requested_profile_config(&read_only).unwrap();
    assert!(config.get("extends").is_none());
    let filesystem = config["filesystem"].as_object().unwrap();
    assert!(!filesystem.contains_key(":minimal"));
    assert!(!filesystem.contains_key("/etc"));
    assert_eq!(filesystem["/workspace/*.kube/config*"], "deny");
    assert_eq!(filesystem["/workspace/**/*.kube/config*"], "deny");
    assert_eq!(filesystem["/workspace/**/*.env*/**"], "deny");
    assert_eq!(filesystem["/workspace/.[aA][wW][sS]/**"], "deny");
    assert_eq!(filesystem["/workspace/**/.[aA][wW][sS]/**"], "deny");
    assert!(!filesystem.contains_key("/workspace/*config*"));
    assert_eq!(
        expected_profile_sandbox(&read_only, "/workspace").unwrap(),
        ("readOnly", false)
    );

    let writable = permission_profile("/workspace", true, true, &test_requirements()).unwrap();
    assert_eq!(
        expected_profile_sandbox(&writable, "/workspace").unwrap(),
        ("workspaceWrite", true)
    );
}

#[test]
fn native_stage_budget_requires_immutable_work_deadline_not_command_cap() {
    let metadata = ToolInvocationMetadata {
        command_timeout_cap_ms: Some(86_400_000),
        ..Default::default()
    };
    assert!(native_stage_remaining(metadata).is_err());
    let deadline = std::time::Instant::now() + Duration::from_secs(43_200);
    for command_timeout_cap_ms in [None, Some(1), Some(86_400_000)] {
        let remaining = native_stage_remaining(ToolInvocationMetadata {
            admission_deadline: Some(deadline),
            command_timeout_cap_ms,
            ..Default::default()
        })
        .unwrap();
        assert!(remaining > Duration::from_secs(43_199));
        assert!(remaining <= Duration::from_secs(43_200));
    }
    assert!(
        native_stage_remaining(ToolInvocationMetadata {
            admission_deadline: Some(std::time::Instant::now()),
            ..metadata
        })
        .is_err()
    );
}

fn questionnaire() -> ProviderInteractionRequest {
    ProviderInteractionRequest {
        request_id: "rpc-question".into(),
        timeout_ms: Some(10_000),
        payload: json!({"provider":"codex", "method":"item/tool/requestUserInput", "params":{"questions":[
            {"id":"native-first", "header":"First", "question":"Choose first?", "isOther":true,
             "options":[{"label":"A", "description":"First option"},{"label":"B", "description":"Second option"}]},
            {"id":"native-second", "header":"Second", "question":"Choose second?", "isOther":false,
             "options":[{"label":"C", "description":"Third option"},{"label":"D", "description":"Fourth option"}]}
        ]}}),
    }
}

#[test]
fn native_question_projection_preserves_ids_options_and_normalized_order() {
    let request = questionnaire();
    let mut prompt = question_prompt(&request).unwrap();
    prompt.context = Some("Codex · originating session".into());
    assert_eq!(prompt.questions[0].header, "First");
    assert_eq!(
        prompt.questions[0].options[0].description.as_deref(),
        Some("First option")
    );
    assert!(prompt.questions[0].allow_freeform);
    assert!(!prompt.questions[1].allow_freeform);
    let answers = astra_tools::AskUserAnswers {
        answers: vec![
            astra_tools::AskUserQuestionAnswer {
                question: "Choose second?".into(),
                answers: vec![" D ".into()],
                multi_select: false,
                annotation: None,
            },
            astra_tools::AskUserQuestionAnswer {
                question: "Choose first?".into(),
                answers: vec!["custom answer".into()],
                multi_select: false,
                annotation: None,
            },
        ],
    };
    assert_eq!(
        question_response(&request, &prompt, &answers).unwrap(),
        json!({"answers":{
            "native-first":{"answers":["custom answer"]}, "native-second":{"answers":["D"]}
        }})
    );
    let mut wrong = prompt.clone();
    wrong.questions.swap(0, 1);
    assert!(question_response(&request, &wrong, &answers).is_err());
}

#[test]
fn native_question_projection_rejects_secret_duplicates_and_nonquestions() {
    let mut request = questionnaire();
    request.payload["params"]["questions"][0]["isSecret"] = json!(true);
    request.payload["params"]["questions"][0]["question"] =
        json!("secret-content-not-for-rendering");
    let error = question_prompt(&request).unwrap_err();
    assert!(error.contains("secret"));
    assert!(!error.contains("secret-content-not-for-rendering"));
    let mut request = questionnaire();
    request.payload["params"]["questions"][1]["id"] = json!("native-first");
    assert!(question_prompt(&request).unwrap_err().contains("unique"));
    let mut request = questionnaire();
    request.payload["params"]["questions"][1]["question"] = json!("Choose first?");
    assert!(question_prompt(&request).unwrap_err().contains("duplicate"));
    request.payload["method"] = json!("item/permissions/requestApproval");
    assert!(
        question_prompt(&request)
            .unwrap_err()
            .contains("not supported")
    );
}

fn active() -> Evidence {
    Evidence {
        thread: Some("thread".into()),
        turn: Some("turn".into()),
        ..Evidence::default()
    }
}

fn terminal(thread: &str, turn: &str) -> Value {
    json!({"method": "turn/completed", "params": {"threadId": thread, "turn": {"id": turn, "status": "completed"}}})
}

#[test]
fn stage_arguments_cannot_carry_execution_controls() {
    for field in [
        "run_id",
        "expected_control_epoch",
        "timeout",
        "command",
        "cwd",
        "approvalPolicy",
    ] {
        let mut args = json!({"task": "Review", "anchor_run_id": "anchor"});
        args[field] = json!("injected");
        assert!(Stage::parse(&args).is_err(), "{field}");
    }
    assert!(Stage::parse(&json!({"task": "Review", "anchor_run_id": " anchor"})).is_err());
}

#[test]
fn exact_thread_resume_and_native_turn_input() {
    let mut stage = stage();
    stage.native_session_id = Some("native-thread".into());
    stage.model = Some("chosen-model".into());
    stage.effort = Some("xhigh".into());
    let profile = test_profile();
    let resume = thread_request(&stage, "/workspace", &profile);
    assert_eq!(resume["method"], "thread/resume");
    assert_eq!(resume["params"]["threadId"], "native-thread");
    assert_eq!(resume["params"]["excludeTurns"], true);
    assert!(resume["params"].get("path").is_none());
    let turn = turn_request(&stage, "native-thread", "/workspace", &profile);
    assert_eq!(
        turn["params"]["input"][0],
        json!({"type": "text", "text": "Review stage", "text_elements": []})
    );
    assert_eq!(turn["params"]["permissions"], profile["profileId"]);
    assert!(turn["params"].get("sandboxPolicy").is_none());
    assert!(resume["params"].get("sandbox").is_none());
    assert_eq!(resume["params"]["config"], profile["config"]);
    assert_eq!(turn["params"]["effort"], "xhigh");
}

#[test]
fn acknowledgements_never_guess_thread_or_join_old_work() {
    let mut stage = stage();
    stage.native_session_id = Some("expected".into());
    let mut evidence = Evidence::default();
    assert!(
        acknowledge_thread(
            &stage,
            &json!({"thread": {"id": "other", "status": {"type": "idle"}}}),
            &mut evidence
        )
        .is_err()
    );
    assert!(evidence.thread.is_none());
    assert!(
        acknowledge_thread(
            &stage,
            &json!({"thread": {"id": "expected", "status": {"type": "active"}}}),
            &mut evidence
        )
        .is_err()
    );
    assert_eq!(evidence.thread.as_deref(), Some("expected"));
    assert!(evidence.turn.is_none());
}

#[test]
fn terminal_requires_both_exact_identities_and_typed_terminal_status() {
    for (thread, turn) in [("other", "turn"), ("thread", "other")] {
        let mut evidence = active();
        assert!(
            evidence
                .notification(
                    "turn/completed",
                    &terminal(thread, turn)["params"],
                    OUTPUT_BYTES
                )
                .is_err()
        );
        assert!(evidence.terminal.is_none());
    }
    let mut evidence = active();
    assert!(
        evidence
            .notification(
                "turn/completed",
                &json!({"threadId": "thread", "turn": {"id": "turn", "status": "inProgress"}}),
                OUTPUT_BYTES
            )
            .is_err()
    );
    evidence
        .notification(
            "turn/completed",
            &terminal("thread", "turn")["params"],
            OUTPUT_BYTES,
        )
        .unwrap();
    assert_eq!(evidence.terminal.as_deref(), Some("completed"));
}

#[test]
fn native_sandbox_ack_cannot_broaden_selected_authority() {
    let requested = test_profile();
    let mut response = json!({"cwd": "/workspace", "approvalPolicy": "never", "approvalsReviewer": "user", "activePermissionProfile": {"id":requested["profileId"]}, "sandbox": {"type":"readOnly", "networkAccess":false}});
    verify_sandbox(&response, &requested, "/workspace").unwrap();
    response["activePermissionProfile"]["id"] = json!(":workspace");
    assert!(verify_sandbox(&response, &requested, "/workspace").is_err());
    response["activePermissionProfile"]["id"] = requested["profileId"].clone();
    response["cwd"] = json!("/other");
    assert!(verify_sandbox(&response, &requested, "/workspace").is_err());
    response["cwd"] = json!("/workspace");
    response["approvalPolicy"] = json!("on-request");
    assert!(verify_sandbox(&response, &requested, "/workspace").is_err());
    response["approvalPolicy"] = json!("never");
    response["activePermissionProfile"]["extends"] = json!("broad_profile");
    assert!(verify_sandbox(&response, &requested, "/workspace").is_err());
    response
        .as_object_mut()
        .unwrap()
        .remove("activePermissionProfile");
    assert!(verify_sandbox(&response, &requested, "/workspace").is_err());
}

#[test]
fn usage_is_last_snapshot_not_sum_and_absence_is_unknown() {
    let mut evidence = active();
    assert!(evidence.usage.is_none());
    assert!(evidence.stage_usage().unwrap().is_none());
    let counters = json!({"inputTokens": 30, "cachedInputTokens": 10, "outputTokens": 5, "reasoningOutputTokens": 2, "totalTokens": 35});
    let event = json!({"threadId": "thread", "turnId": "turn", "tokenUsage": {"total": counters, "last": counters, "modelContextWindow": null}});
    for _ in 0..2 {
        evidence
            .notification("thread/tokenUsage/updated", &event, OUTPUT_BYTES)
            .unwrap();
    }
    assert_eq!(evidence.usage.as_ref().unwrap()["total"]["totalTokens"], 35);
    // Missing cache-write evidence cannot be filled with an invented zero.
    assert_eq!(
        evidence.stage_usage().unwrap().unwrap().to_json(),
        json!({"cached_input_tokens": 10, "output_tokens": 5})
    );
}

fn usage_event(turn: &str, input: u64, cached: u64, creation: u64, output: u64) -> Value {
    let counters = json!({"inputTokens": input, "cachedInputTokens": cached, "cacheWriteInputTokens": creation, "outputTokens": output, "reasoningOutputTokens": 2, "totalTokens": input + output});
    json!({"threadId": "thread", "turnId": turn, "tokenUsage": {"total": counters, "last": counters}})
}

#[test]
fn usage_receipt_is_disjoint_stage_delta_not_thread_total_or_last_response() {
    let mut evidence = active();
    evidence.resumed = true;
    evidence
        .notification(
            "thread/tokenUsage/updated",
            &usage_event("old-turn", 100, 40, 20, 10),
            OUTPUT_BYTES,
        )
        .unwrap();
    let mut event = usage_event("turn", 150, 60, 30, 20);
    event["tokenUsage"]["last"] = usage_event("turn", 10, 4, 2, 3)["tokenUsage"]["last"].clone();
    for _ in 0..2 {
        evidence
            .notification("thread/tokenUsage/updated", &event, OUTPUT_BYTES)
            .unwrap();
    }
    assert_eq!(
        evidence.stage_usage().unwrap().unwrap().to_json(),
        json!({"input_tokens": 20, "cached_input_tokens": 20, "cache_creation_tokens": 10, "output_tokens": 10, "total_tokens": 60})
    );
    assert!(
        evidence
            .notification(
                "thread/tokenUsage/updated",
                &usage_event("old-turn", 100, 40, 20, 10),
                OUTPUT_BYTES
            )
            .is_err()
    );
}

#[test]
fn resumed_usage_without_baseline_is_unknown_and_invalid_counters_do_not_become_receipts() {
    let mut evidence = active();
    evidence.resumed = true;
    evidence
        .notification(
            "thread/tokenUsage/updated",
            &usage_event("turn", 150, 60, 30, 20),
            OUTPUT_BYTES,
        )
        .unwrap();
    assert!(evidence.stage_usage().unwrap().is_none());
    assert!(
        evidence
            .notification(
                "thread/tokenUsage/updated",
                &usage_event("turn", 149, 60, 30, 20),
                OUTPUT_BYTES
            )
            .is_err()
    );
    let mut evidence = active();
    assert!(
        evidence
            .notification(
                "thread/tokenUsage/updated",
                &usage_event("turn", 30, 20, 11, 10),
                OUTPUT_BYTES
            )
            .is_err()
    );
    assert!(evidence.usage.is_none());
    assert!(
        evidence
            .notification(
                "thread/tokenUsage/updated",
                &usage_event("old-turn", 100, 40, 20, 10),
                OUTPUT_BYTES
            )
            .is_err()
    );
}

#[test]
fn output_is_utf8_bounded_and_not_terminal_evidence() {
    let mut evidence = active();
    evidence
        .notification(
            "item/agentMessage/delta",
            &json!({"threadId": "thread", "turnId": "turn", "delta": "你好completed"}),
            4,
        )
        .unwrap();
    assert_eq!(evidence.output, "你");
    assert!(evidence.output_capped);
    assert!(evidence.terminal.is_none());
}

#[test]
fn completed_agent_message_is_authoritative_over_truncated_progress() {
    let mut evidence = active();
    evidence
        .notification(
            "item/agentMessage/delta",
            &json!({"threadId": "thread", "turnId": "turn", "delta": "x".repeat(OUTPUT_BYTES + 1)}),
            OUTPUT_BYTES,
        )
        .unwrap();
    evidence
        .notification(
            "item/completed",
            &json!({
                "threadId": "thread",
                "turnId": "turn",
                "item": {"type": "agentMessage", "id": "answer", "text": "final answer"}
            }),
            OUTPUT_BYTES,
        )
        .unwrap();
    assert_eq!(evidence.final_output.as_deref(), Some("final answer"));
    assert!(evidence.output_capped);
}

#[test]
fn original_native_questions_and_rpc_id_type_are_preserved() {
    let envelope = json!({"id": "native-question", "method": "item/tool/requestUserInput", "params": {"threadId": "thread", "turnId": "turn", "itemId": "item", "questions": [{"id": "q", "question": "Which?"}], "isBlocking": true}});
    let request = interaction_request(&envelope, &active()).unwrap();
    assert_eq!(request.payload["native_request_id"], envelope["id"]);
    assert_eq!(request.payload["params"], envelope["params"]);
    assert_eq!(request.request_id, "\"native-question\"");
    let mut wrong = envelope;
    wrong["params"]["turnId"] = json!("other");
    assert!(interaction_request(&wrong, &active()).is_err());
}

#[tokio::test]
async fn selected_cli_entrypoint_requires_binding_policy_and_admitted_budget() {
    struct NoTaskGate;
    #[async_trait::async_trait]
    impl ProviderInteractionGate for NoTaskGate {
        async fn request_interaction(
            &self,
            _: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            panic!("preflight must not execute native work");
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(directory.path());
    let args = json!({"task": "Review", "anchor_run_id": "anchor"});
    let denied = executor
        .execute_with_invocation_metadata_cancelable(
            TOOL_NAME,
            &args,
            ToolInvocationMetadata::default(),
            None,
        )
        .await;
    assert!(denied.is_error);
    executor.set_cli_local_provider_schemas(vec![schema()]);
    executor.set_current_visible_tool_schemas(&[schema()]);
    let invocation = ToolInvocationMetadata {
        admission_deadline: Some(std::time::Instant::now() + Duration::from_secs(86_400)),
        run_id: Some("run"),
        tool_call_id: Some("call"),
        admission_source: Some(ToolInvocationAdmissionSource::Policy),
        command_timeout_cap_ms: Some(86_400_000),
        ..ToolInvocationMetadata::default()
    };
    let denied = executor
        .execute_native_provider_invocation(
            astra_turn_core::provider_resolution::NativeCollaboratorProtocol::CodexAppServer,
            TOOL_NAME,
            &args,
            invocation,
            None,
            &NoTaskGate,
            &fixture_execution_ceiling(&executor),
            None,
        )
        .await;
    assert!(denied.is_error);
    assert_eq!(
        denied.tool_result_fields.unwrap()["native_collaborator"]["dispatch_state"],
        "not_dispatched"
    );
    // An explicit local sandbox bypass still cannot supply execution time.
    // This reaches the selected entrypoint, not just the argument parser.
    *astra_core::sync_poison::recover_rwlock_write(&executor.sandbox_policy) = None;
    let denied = executor
        .execute_native_provider_invocation(
            astra_turn_core::provider_resolution::NativeCollaboratorProtocol::CodexAppServer,
            TOOL_NAME,
            &args,
            ToolInvocationMetadata {
                admission_deadline: None,
                ..invocation
            },
            None,
            &NoTaskGate,
            &fixture_execution_ceiling(&executor),
            None,
        )
        .await;
    assert!(denied.is_error);
    assert!(denied.output.contains("requires an admitted stage budget"));
    let fields = denied.tool_result_fields.unwrap();
    assert_eq!(fields["native_collaborator"]["target_released"], false);
    assert!(fields.get("collaborator_usage").is_none());
}

/// Opt-in paid-provider evidence for the selected ToolExecutor adapter only.
/// These are fixture-owned invocation identities and explicit test admission,
/// not a forged provider principal or proof of durable Server admission/UX.
/// Provider credentials are consumed by Codex itself; this test never opens
/// credentials/configuration or prints native output/errors/configuration.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "live Codex task; requires explicit opt-in, model, auth, native sandbox and fresh supervisor binary"]
async fn live_native_codex_two_stages_same_session() {
    assert_eq!(
        std::env::var("ASTRA_NATIVE_CODEX_HARNESS").as_deref(),
        Ok("1"),
        "explicit paid-task opt-in is required"
    );
    let model = std::env::var("ASTRA_NATIVE_CODEX_HARNESS_MODEL")
        .expect("ASTRA_NATIVE_CODEX_HARNESS_MODEL must select the requested model");
    assert!(valid_id(&model), "invalid harness model identity");
    let helper = std::path::PathBuf::from(
        std::env::var_os("ASTRA_NATIVE_CODEX_HARNESS_SUPERVISOR_BIN")
            .expect("set the absolute path of a freshly built Astra supervisor binary"),
    );
    assert!(
        helper.is_absolute() && helper.is_file(),
        "supervisor binary unavailable"
    );
    // Use the caller-selected disk-backed temporary root. Keeping the live
    // workspace outside the checkout prevents Codex from discovering the
    // checkout's parent AGENTS.md while the fixture grants only this root.
    let base = std::env::temp_dir().join("astra-native-codex-harness");
    std::fs::create_dir_all(&base).expect("create disk-backed harness parent");
    let workspace = tempfile::Builder::new()
        .prefix("two-stage-")
        .tempdir_in(base)
        .expect("create isolated disk workspace");
    let root = workspace
        .path()
        .canonicalize()
        .expect("canonical workspace");
    let mut executor = ToolExecutor::new(&root);
    // Declare a representable local read policy, retain a real policy object
    // and impose the actual Codex readOnly OS sandbox. No Bypass/None,
    // dangerFullAccess, externalSandbox, auto-approval or sandbox retries.
    let mut policy = astra_sandbox::SandboxPolicy::permissive(&root);
    // The live provider task is explicitly opt-in and needs the configured
    // Codex provider network. Production execution still takes this only from
    // the immutable admission ceiling; offline contract tests keep network
    // disabled.
    policy.network_allowed = true;
    *astra_core::sync_poison::recover_rwlock_write(&executor.sandbox_policy) = Some(policy);
    executor.set_read_only_execution();
    executor.set_cli_local_provider_schemas(vec![schema()]);
    executor.set_current_visible_tool_schemas(&[schema()]);

    struct RejectUnexpectedInteraction(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl ProviderInteractionGate for RejectUnexpectedInteraction {
        async fn request_interaction(
            &self,
            request: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            request
                .validate()
                .expect("canonical native question envelope");
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            ProviderInteractionDecision::Cancelled
        }
    }
    let gate = RejectUnexpectedInteraction(std::sync::atomic::AtomicUsize::new(0));
    let cancel = CancellationToken::new();
    let session = format!("native-live-session-{}", uuid::Uuid::now_v7());
    let first_run = format!("native-live-run-{}", uuid::Uuid::now_v7());
    let second_run = format!("native-live-run-{}", uuid::Uuid::now_v7());
    let token = format!("native-history-{}", uuid::Uuid::now_v7().simple());
    let tasks = [
        format!("Remember this opaque token for our next stage: {token}. Reply with that token only. Do not use tools, access files, or ask questions."),
        "Repeat only the opaque token I asked you to remember in the previous stage. Do not use tools, access files, or ask questions.".to_string(),
    ];
    let mut native_session: Option<String> = None;
    let mut first_turn: Option<String> = None;
    for (index, (run, task)) in [&first_run, &second_run].into_iter().zip(tasks).enumerate() {
        let identity = astra_turn_types::ToolInvocationIdentity::new(
            "native-adapter-live-harness",
            &session,
            run,
            run,
            "native-stage",
        )
        .expect("canonical fixture invocation identity");
        let mut args = json!({"task": task, "anchor_run_id": first_run, "model": model});
        if let Some(id) = &native_session {
            args["native_session_id"] = json!(id);
        }
        // This short live fixture explicitly admits three minutes per stage;
        // it does not test or alter the product's hours/day budget transport.
        let invocation = ToolInvocationMetadata {
            admission_deadline: Some(std::time::Instant::now() + Duration::from_secs(180)),
            run_id: Some(&identity.run_id),
            turn_chain_id: Some(&identity.turn_chain_id),
            tool_call_id: Some(&identity.invocation_id),
            admission_source: Some(ToolInvocationAdmissionSource::ParentApproval),
            command_timeout_cap_ms: None,
            ..ToolInvocationMetadata::default()
        };
        let started = std::time::Instant::now();
        let mut ceiling = fixture_execution_ceiling(&executor);
        ceiling.network_allowed = true;
        // Fixture-owned approval of the captured installed dependency set,
        // not evidence that the production descriptor/admission gate is wired.
        ceiling.runtime_read_paths = installed_runtime_requirements()
            .expect("installed native requirements")
            .read_paths;
        let outcome = executor
            .execute_native_provider_invocation(
                astra_turn_core::provider_resolution::NativeCollaboratorProtocol::CodexAppServer,
                TOOL_NAME,
                &args,
                invocation,
                Some(&cancel),
                &gate,
                &ceiling,
                None,
            )
            .await;
        let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let exact_output = outcome.output.trim() == token;
        let fields = outcome.tool_result_fields.as_ref();
        let native = fields
            .and_then(|fields| fields.get("native_collaborator"))
            .cloned()
            .unwrap_or(Value::Null);
        let usage = fields
            .and_then(|fields| fields.get("collaborator_usage"))
            .and_then(|usage| astra_turn_types::CanonicalTokenUsage::from_json(usage).ok());
        let cached = usage.and_then(|usage| usage.cached_input_tokens());
        let inclusive_input = usage
            .and_then(|usage| usage.input_column())
            .map(|input| input as u64);
        let cache_read_percent = cached
            .zip(inclusive_input.filter(|input| *input > 0))
            .map(|(cached, input)| 100.0 * cached as f64 / input as f64);
        let same_session = native_session.as_deref().and_then(|expected| {
            native["native_session_id"]
                .as_str()
                .map(|actual| actual == expected)
        });
        let different_turn = first_turn.as_deref().and_then(|previous| {
            native["native_turn_id"]
                .as_str()
                .map(|actual| actual != previous)
        });
        // A closed field projection, printed even on failure. Never serialize
        // the whole receipt: it may contain private native error/output data.
        println!(
            "{}",
            json!({
                "event": "native_codex_adapter_live_stage",
                "stage": index + 1,
                "elapsed_ms": elapsed_ms,
                "requested_model": model,
                "is_error": outcome.is_error,
                "failure_reason": outcome.output.rsplit_once("Error: ").map(|(_, reason)| reason),
                "output_trim_exact": exact_output,
                "session_acknowledged": native["session_acknowledged"].as_bool(),
                "turn_acknowledged": native["turn_acknowledged"].as_bool(),
                "same_acknowledged_session": same_session,
                "different_acknowledged_turn": different_turn,
                "native_terminal": native["native_terminal"].as_str()
                    .filter(|status| matches!(*status, "completed" | "failed" | "interrupted")),
                "usage_scope": "stage_delta",
                "usage_observed": usage.is_some(),
                "usage": usage.map(astra_turn_types::CanonicalTokenUsage::to_json),
                "cache_read": {
                    "coverage": cached.is_some() && inclusive_input.is_some(),
                    "cached_input_tokens": cached,
                    "inclusive_input_tokens": inclusive_input,
                    "percent": cache_read_percent
                },
                "cost_usd": null,
                "cost_observed": false,
                "cleanup": {
                    "target_released": native["target_released"].as_bool(),
                    "settlement_authoritative": native["settlement_authoritative"].as_bool(),
                    "transport_settled_after_terminal": native["transport_settled_after_terminal"].as_bool(),
                    "cleanup_cancelled": native["cleanup_cancelled"].as_bool(),
                    "workspace_effect_settled": fields.and_then(|fields| fields.get("workspace_effect_settled")).and_then(Value::as_bool)
                }
            })
        );
        // Do not expose native error text, output, credentials or config in a
        // failed test. Cleanup has been awaited before either assertion.
        assert!(
            !outcome.is_error,
            "native stage {} failed; inspect sanitized owner evidence",
            index + 1
        );
        assert!(
            exact_output,
            "stage {} output was not exactly the remembered token",
            index + 1
        );
        let fields = outcome
            .tool_result_fields
            .expect("native structured receipt");
        let native = &fields["native_collaborator"];
        assert_eq!(native["session_acknowledged"], true);
        assert_eq!(native["turn_acknowledged"], true);
        assert_eq!(native["dispatch_state"], "acknowledged");
        assert_eq!(native["native_terminal"], "completed");
        assert_eq!(native["output_capped"], false);
        assert_eq!(native["target_released"], true);
        assert_eq!(native["settlement_authoritative"], true);
        assert_eq!(native["transport_settled_after_terminal"], true);
        assert_eq!(fields["workspace_effect_settled"], true);
        let acknowledged: astra_services::runs::CollaboratorNativeSession = serde_json::from_value(
            fields[astra_services::runs::COLLABORATOR_NATIVE_SESSION_METADATA_KEY].clone(),
        )
        .expect("actual native session ACK metadata");
        assert_eq!(
            acknowledged.provider,
            astra_services::runs::CollaboratorProvider::Codex
        );
        assert_eq!(acknowledged.anchor_run_id, first_run);
        assert_eq!(
            native["native_session_id"].as_str(),
            Some(acknowledged.native_session_id.as_str())
        );
        let turn = native["native_turn_id"]
            .as_str()
            .filter(|id| valid_id(id))
            .expect("actual native turn ACK")
            .to_owned();
        if let Some(session) = &native_session {
            assert_eq!(
                &acknowledged.native_session_id, session,
                "resume changed the native session"
            );
            assert_ne!(
                Some(&turn),
                first_turn.as_ref(),
                "second stage reused the native turn"
            );
        } else {
            native_session = Some(acknowledged.native_session_id);
            first_turn = Some(turn);
        }
    }
    assert_eq!(
        gate.0.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "unexpected native interaction was rejected, not auto-approved"
    );
    drop(executor);
    workspace
        .close()
        .expect("remove only the isolated harness workspace");
    assert!(!root.exists(), "workspace cleanup incomplete");
    println!(
        "{}",
        json!({"event": "native_codex_adapter_live_complete", "stages": 2, "same_acknowledged_session": true, "different_acknowledged_turn": true, "workspace_removed": true})
    );
}

#[cfg(target_os = "linux")]
mod transport {
    use super::*;

    #[test]
    fn supervisor_helper() {
        if astra_sandbox::invocation_supervisor_is_requested()
            && let Some(code) = astra_sandbox::run_invocation_supervisor_if_requested()
        {
            std::process::exit(code);
        }
    }

    async fn process(script: &str, token: CancellationToken) -> FramedProcess {
        let (command, owner) = BashInvocationOwner::prepare_with_supervisor_helper(
            std::env::current_exe().unwrap(),
            [
                "--exact".into(),
                "edge_tools::native_codex::tests::transport::supervisor_helper".into(),
                "--nocapture".into(),
                "--quiet".into(),
            ],
            "python3",
            &["-u".into(), "-c".into(), script.into()],
        )
        .unwrap();
        let mut process = owner
            .spawn_framed(
                command,
                FramedProcessLimits {
                    max_frame_bytes: FRAME_BYTES,
                    max_queued_frames: 4,
                    max_stderr_bytes: 128,
                    timeout: Duration::from_secs(5),
                },
                token,
            )
            .unwrap();
        // Only the re-exec libtest fixture emits this prelude. Production
        // never strips text or accepts a non-JSON native frame.
        loop {
            let frame = process.recv_frame().await.expect("helper prelude");
            if frame == b"running 1 test" {
                break;
            }
            assert!(frame.is_empty(), "unexpected test helper prelude");
        }
        process
    }

    const PREFIX: &str = r#"
import json,sys
def recv(): return json.loads(sys.stdin.readline())
def emit(v): print(json.dumps(v),flush=True)
assert recv()['method']=='initialize'
emit({'id':1,'result':{}})
assert recv()['method']=='initialized'
request=recv()
assert request['method']=='thread/start'
assert request['params']['approvalPolicy']=='never'
assert request['params']['permissions'].startswith('astra_admitted_')
assert 'config' in request['params']
profile=request['params']['permissions']
assert request['params']['config']['default_permissions']==profile
config=request['params']['config']['permissions'][profile]
assert 'extends' not in config
assert config['filesystem']['/workspace']=='read'
emit({'id':2,'result':{'thread':{'id':'thread','status':{'type':'idle'}},'cwd':'/workspace','approvalPolicy':'never','approvalsReviewer':'user','activePermissionProfile':{'id':profile},'sandbox':{'type':'readOnly','networkAccess':False}}})
request=recv()
assert request['method']=='turn/start'
assert request['params']['approvalPolicy']=='never'
assert request['params']['permissions']==profile
assert 'sandboxPolicy' not in request['params']
"#;

    #[tokio::test]
    async fn real_owner_preserves_terminal_and_delta_before_turn_ack() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'method':'item/agentMessage/delta','params':{'threadId':'thread','turnId':'turn','delta':'review complete'}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut evidence = Evidence::default();
        drive(
            &mut process,
            &stage(),
            "/workspace",
            &test_profile(),
            &mut evidence,
            OUTPUT_BYTES,
            None,
            &token,
        )
        .await
        .unwrap();
        assert_eq!(evidence.output, "review complete");
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        let outcome = process.wait().await.unwrap();
        assert!(outcome.status.unwrap().success());
        assert!(outcome.settlement.unwrap().ownership.is_authoritative());
    }

    #[tokio::test]
    async fn real_owner_resume_replay_and_usage_receipt_are_fenced_to_current_stage() {
        let script = format!(
            "{}{}",
            PREFIX.replace("=='thread/start'", "=='thread/resume'"),
            r#"
# Resume usage replay arrives after its ACK but before the new turn ACK.
before={'inputTokens':100,'cachedInputTokens':40,'cacheWriteInputTokens':20,'outputTokens':10,'reasoningOutputTokens':2,'totalTokens':110}
emit({'method':'thread/tokenUsage/updated','params':{'threadId':'thread','turnId':'old-turn','tokenUsage':{'total':before,'last':before}}})
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
after={'inputTokens':150,'cachedInputTokens':60,'cacheWriteInputTokens':30,'outputTokens':20,'reasoningOutputTokens':4,'totalTokens':170}
for i in range(2): emit({'method':'thread/tokenUsage/updated','params':{'threadId':'thread','turnId':'turn','tokenUsage':{'total':after,'last':before}}})
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut stage = stage();
        stage.native_session_id = Some("thread".into());
        let mut evidence = Evidence::default();
        drive(
            &mut process,
            &stage,
            "/workspace",
            &test_profile(),
            &mut evidence,
            OUTPUT_BYTES,
            None,
            &token,
        )
        .await
        .unwrap();
        assert_eq!(
            evidence.stage_usage().unwrap().unwrap().to_json(),
            json!({"input_tokens":20,"cached_input_tokens":20,"cache_creation_tokens":10,"output_tokens":10,"total_tokens":60})
        );
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        assert!(
            process
                .wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    #[tokio::test]
    async fn real_owner_eof_is_not_terminal_or_zero_usage() {
        let script = format!(
            "{PREFIX}{}",
            "emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})"
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut evidence = Evidence::default();
        assert!(
            drive(
                &mut process,
                &stage(),
                "/workspace",
                &test_profile(),
                &mut evidence,
                OUTPUT_BYTES,
                None,
                &token
            )
            .await
            .is_err()
        );
        assert_eq!(evidence.turn.as_deref(), Some("turn"));
        assert!(evidence.terminal.is_none());
        assert!(evidence.usage.is_none());
        assert!(
            process
                .cancel_and_wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    struct AnswerGate;
    #[async_trait::async_trait]
    impl ProviderInteractionGate for AnswerGate {
        async fn request_interaction(
            &self,
            request: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            assert_eq!(request.payload["native_request_id"], "native-question");
            assert_eq!(request.payload["params"]["questions"][0]["id"], "q");
            ProviderInteractionDecision::Submitted(
                json!({"answers": {"q": {"answers": ["Proceed"]}}}),
            )
        }
    }

    #[tokio::test]
    async fn real_owner_question_before_ack_preserves_request_and_reply() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'id':'native-question','method':'item/tool/requestUserInput','params':{'threadId':'thread','turnId':'turn','itemId':'item','isBlocking':True,'questions':[{'id':'q','header':'Choice','question':'Proceed?'}]}})
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
answer=recv()
assert answer=={'id':'native-question','result':{'answers':{'q':{'answers':['Proceed']}}}}
emit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut evidence = Evidence::default();
        drive(
            &mut process,
            &stage(),
            "/workspace",
            &test_profile(),
            &mut evidence,
            OUTPUT_BYTES,
            Some(&AnswerGate),
            &token,
        )
        .await
        .unwrap();
        assert_eq!(evidence.terminal.as_deref(), Some("completed"));
        assert!(
            process
                .wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }

    struct ApprovalReplyGate(Value);

    #[async_trait::async_trait]
    impl ProviderInteractionGate for ApprovalReplyGate {
        async fn request_interaction(
            &self,
            _: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            ProviderInteractionDecision::Submitted(self.0.clone())
        }
    }

    #[tokio::test]
    async fn real_owner_workspace_approval_cannot_expand_immutable_ceiling() {
        let sandbox = test_profile();
        let prefix = PREFIX;
        for (method, payload, allowed) in [
            (
                "item/commandExecution/requestApproval",
                json!({"decision":"accept"}),
                false,
            ),
            (
                "item/commandExecution/requestApproval",
                json!({"decision":"acceptForSession"}),
                false,
            ),
            (
                "item/fileChange/requestApproval",
                json!({"decision":"accept"}),
                false,
            ),
            (
                "item/permissions/requestApproval",
                json!({"permissions":{"network":{"enabled":true}}}),
                false,
            ),
            (
                "item/permissions/requestApproval",
                json!({"permissions":{"fileSystem":{"write":["/outside"]}},"scope":"session"}),
                false,
            ),
            // Even an apparently in-root additive grant is unsupported: the
            // adapter cannot prove its native execution/symlink scope is bounded.
            (
                "item/permissions/requestApproval",
                json!({"permissions":{"fileSystem":{"write":["/workspace"]}}}),
                false,
            ),
            (
                "item/commandExecution/requestApproval",
                json!({"decision":"decline"}),
                true,
            ),
            (
                "item/permissions/requestApproval",
                json!({"permissions":{},"scope":"turn"}),
                true,
            ),
        ] {
            let script = format!(
                "{prefix}\nemit({{'id':3,'result':{{'turn':{{'id':'turn','status':'inProgress'}}}}}})\nemit({{'id':'approval','method':'{method}','params':{{'threadId':'thread','turnId':'turn','itemId':'item'}}}})\n{}",
                if allowed {
                    "reply=recv()\nassert reply['id']=='approval'\nassert reply['result'].get('decision')=='decline' or reply['result'].get('permissions')=={}\nemit({'method':'turn/completed','params':{'threadId':'thread','turn':{'id':'turn','status':'completed'}}})\nfor line in sys.stdin: pass\n"
                } else {
                    // wait() closes input after the rejected reply. A forbidden
                    // grant reaching the actual wire makes the process fail.
                    "assert sys.stdin.readline()==''\n"
                }
            );
            let token = CancellationToken::new();
            let mut process = process(&script, token.clone()).await;
            let mut evidence = Evidence::default();
            let result = drive(
                &mut process,
                &stage(),
                "/workspace",
                &sandbox,
                &mut evidence,
                OUTPUT_BYTES,
                Some(&ApprovalReplyGate(payload)),
                &token,
            )
            .await;
            assert_eq!(result.is_ok(), allowed, "{method}");
            assert_eq!(evidence.terminal.as_deref(), allowed.then_some("completed"));
            let outcome = process.wait().await.unwrap();
            assert!(
                outcome.status.unwrap().success(),
                "forbidden reply reached native wire"
            );
            assert!(outcome.settlement.unwrap().ownership.is_authoritative());
        }
    }

    struct CancelGate(CancellationToken);
    #[async_trait::async_trait]
    impl ProviderInteractionGate for CancelGate {
        async fn request_interaction(
            &self,
            _: &ProviderInteractionRequest,
        ) -> ProviderInteractionDecision {
            self.0.cancel();
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn real_owner_cancel_during_question_does_not_invent_terminal() {
        let script = format!(
            "{PREFIX}{}",
            r#"
emit({'id':3,'result':{'turn':{'id':'turn','status':'inProgress'}}})
emit({'id':'question','method':'item/tool/requestUserInput','params':{'threadId':'thread','turnId':'turn','itemId':'item','isBlocking':True,'questions':[{'id':'q','header':'Choice','question':'Proceed?'}]}})
for line in sys.stdin: pass
"#
        );
        let token = CancellationToken::new();
        let mut process = process(&script, token.clone()).await;
        let mut evidence = Evidence::default();
        assert!(
            drive(
                &mut process,
                &stage(),
                "/workspace",
                &test_profile(),
                &mut evidence,
                OUTPUT_BYTES,
                Some(&CancelGate(token.clone())),
                &token
            )
            .await
            .is_err()
        );
        assert_eq!(evidence.turn.as_deref(), Some("turn"));
        assert!(evidence.terminal.is_none());
        assert!(
            process
                .cancel_and_wait()
                .await
                .unwrap()
                .settlement
                .unwrap()
                .ownership
                .is_authoritative()
        );
    }
}
