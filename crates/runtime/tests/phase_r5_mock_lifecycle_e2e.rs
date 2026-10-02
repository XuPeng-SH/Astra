//! Real Server tool execution and subsequent provider wire history.
#![cfg(feature = "e2e-hooks")]

use astra_runtime::server::provider_test_support::{
    InferenceLedgerFixture, ProviderGateway, ProviderResponse, ProviderScript,
    bind_server_workspace, loop_state, server_host_builder,
};
use astra_runtime::server::tool_transport::{
    ExecutionBindingSnapshot, ExecutorBinding, WorkspaceBinding,
};
use astra_runtime::turn::agentic_loop::finalization::run_agentic_loop_with_host;
use serde_json::{Value, json};

#[tokio::test]
async fn server_lifecycle_tool_use_preserves_actual_tool_result_pairing() {
    let workspace = tempfile::TempDir::new().unwrap();
    std::fs::write(
        workspace.path().join("evidence.txt"),
        "fixture evidence from the executor\n",
    )
    .unwrap();
    let gateway = ProviderGateway::start(vec![ProviderScript::new(
        "primary read then explain", |r| r.path == "/v1/chat/completions" && r.body["model"] == "provider-fixture-model",
        vec![
            ProviderResponse::OpenAi(json!({"id":"read-request","model":"provider-fixture-model",
                "choices":[{"index":0,"message":{"role":"assistant","tool_calls":[{"id":"call-read-evidence","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"evidence.txt\"}"}}]},"finish_reason":"tool_calls"}],
                "usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}})),
            ProviderResponse::OpenAi(json!({"id":"read-answer","model":"provider-fixture-model",
                "choices":[{"index":0,"message":{"role":"assistant","content":"The file contains fixture evidence from the executor."},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":52,"completion_tokens":11,"total_tokens":63}})),
        ]
    )]).await;
    let ledger = InferenceLedgerFixture::default();
    let session = format!("tool-fixture-{}", uuid::Uuid::new_v4());
    let mut host = server_host_builder(
        &gateway,
        &ledger,
        &session,
        "openai",
        "provider-fixture-model",
        None,
    )
    .with_static_tool_catalog_admissible(true)
    .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
        WorkspaceBinding::server_sandbox(workspace.path()),
        ExecutorBinding::server_local(),
    ))
    .build();
    let mut state = loop_state(
        &session,
        Vec::new(),
        "Read evidence.txt and explain its contents without making changes.",
    );
    bind_server_workspace(&mut state, workspace.path()).await;
    state.skills.request_constraints.allowed_tools =
        Some(["read_file".to_owned()].into_iter().collect());
    run_agentic_loop_with_host(&mut host, &mut state)
        .await
        .unwrap();
    assert_eq!(
        state.final_text,
        "The file contains fixture evidence from the executor."
    );
    assert_eq!(state.llm_rounds_completed, 2);
    assert_eq!(state.total_tool_calls, 1);
    assert_eq!((state.total_prompt, state.total_completion), (94, 18));
    assert_eq!(ledger.attempt_count(), 2);
    ledger.assert_quiescent();
    gateway.assert_complete();
    let emitted: Vec<_> = host
        .take_emitted_events()
        .into_iter()
        .filter(|event| event["type"] == "tool_call")
        .collect();
    assert_eq!(
        emitted.len(),
        1,
        "one actual invocation emits one canonical call"
    );
    let call = &emitted[0]["tool_call"];
    assert_eq!(call["id"], "call-read-evidence");
    assert_eq!(call["function"]["name"], "read_file");
    assert_eq!(call["function"]["arguments"], "{\"path\":\"evidence.txt\"}");
    assert!(call.get("name").is_none());
    assert!(call.get("arguments").is_none());
    let requests = gateway.requests.lock().await;
    assert_eq!(requests.len(), 2);
    let messages = requests[1].body["messages"].as_array().unwrap();
    let ids: Vec<_> = messages
        .iter()
        .filter_map(|m| m["tool_calls"].as_array())
        .flatten()
        .map(|call| call["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["call-read-evidence"]);
    let assistant = messages
        .iter()
        .find(|m| {
            m.pointer("/tool_calls/0/id").and_then(Value::as_str) == Some("call-read-evidence")
        })
        .unwrap();
    let results: Vec<_> = messages
        .iter()
        .filter(|m| m["role"] == "tool" && m["tool_call_id"] == "call-read-evidence")
        .collect();
    assert_eq!(results.len(), 1);
    assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 1);
    let output = results[0]["content"].as_str().unwrap();
    assert!(
        output.contains("fixture evidence from the executor"),
        "actual executor output: {output}"
    );
    assert!(
        messages
            .iter()
            .all(|m| m.get("reasoning_content").is_none())
    );
}
