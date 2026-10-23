//! Public chat persistence budget, including work after the response ends.
#![cfg(feature = "e2e-hooks")]

#[allow(dead_code, unused_imports)]
#[path = "system_matrix_http_e2e/harness.rs"]
mod harness;
mod test_support;

use harness::*;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tracing_subscriber::prelude::*;

#[derive(Default)]
struct Statement(String);

impl tracing::field::Visit for Statement {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "db.statement" && !value.trim().is_empty() {
            self.0 = value.split_whitespace().collect::<Vec<_>>().join(" ");
        } else if field.name() == "summary" && self.0.is_empty() {
            self.0 = value.to_owned();
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

#[derive(Clone, Default)]
struct SqlCapture(Arc<Mutex<Option<Vec<String>>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SqlCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let mut guard = self.0.lock().unwrap();
        if let Some(statements) = guard.as_mut() {
            let mut statement = Statement::default();
            event.record(&mut statement);
            statements.push(statement.0);
        }
    }
}

fn answer(message: Value, reason: &str) -> ProviderResponse {
    ProviderResponse::OpenAi(json!({
        "choices": [{"index": 0, "message": message, "finish_reason": reason}],
        "usage": {"prompt_tokens": 42, "completion_tokens": 7, "total_tokens": 49}
    }))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1; provider is scripted localhost HTTP"]
async fn ordinary_chat_and_readonly_rounds_have_bounded_total_sql() {
    require_system_e2e_env();
    let capture = SqlCapture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))
        .expect("this integration binary owns its SQL subscriber");

    let mut budgets = Vec::new();
    for readonly_rounds in [0, 1, 3] {
        let b = bootstrap().await;
        let model = format!("mock-{}", b.ctx.suffix);
        let mut responses = (0..readonly_rounds)
            .map(|round| {
                answer(
                    json!({"role": "assistant", "content": "", "tool_calls": [{
                        "id": format!("discover-{round}"), "type": "function",
                        "function": {"name": "tool_search", "arguments": "{\"query\":\"select:agent\"}"}
                    }]}),
                    "tool_calls",
                )
            })
            .collect::<Vec<_>>();
        responses.push(answer(
            json!({"role": "assistant", "content": "Discovery complete."}),
            "stop",
        ));
        b.ctx
            .install_native_provider(
                &b.auth_header,
                vec![ProviderScript::new(
                    "public ordinary conversation and readonly discovery",
                    move |request| request.body["model"] == model && request.body["stream"] == true,
                    responses,
                )],
            )
            .await;
        *capture.0.lock().unwrap() = Some(Vec::new());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/chat/stream")
            .header("authorization", &b.auth_header)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(json!({
                "message": if readonly_rounds == 0 { "hi" } else { "Discover the agent tool without invoking it." },
                "session_id": b.ctx.session_id,
                "model_selection": seeded_model_selection(&b.ctx)
            }).to_string()))
            .unwrap();
        let (status, body) = collect_full_sse_stream(&b.ctx.app, request, 30).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let events = parse_sse_events(&body);
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "run_finished" && event["status"] == "completed"),
            "{body}"
        );
        assert!(body.contains("Discovery complete."), "{body}");
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "tool_call_end")
                .count(),
            readonly_rounds,
            "tool outcomes must be delivered exactly once: {body}"
        );
        assert!(
            !events.iter().any(|event| event["type"] == "error"),
            "{body}"
        );
        assert!(
            b.ctx
                .app_state
                .drain_background_runs(std::time::Duration::from_secs(5))
                .await
        );
        let statements = capture.0.lock().unwrap().take().unwrap();
        let diagnostic_rows: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type IN ('trace_span', 'llm_round')",
        )
        .bind(&b.ctx.user_id)
        .bind(&b.ctx.session_id)
        .fetch_one(&b.ctx.pool)
        .await
        .unwrap();
        assert_eq!(
            diagnostic_rows.0, 0,
            "ordinary chat must not persist optional diagnostic copies"
        );
        assert!(
            !statements.is_empty(),
            "the SQL observer must capture real database work"
        );
        let requests = b.ctx.native_provider_requests().await;
        assert_eq!(requests.len(), readonly_rounds + 1);
        if readonly_rounds > 0 {
            assert!(
                requests.last().unwrap().body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "tool")
            );
        }
        b.ctx.close().await;
        eprintln!(
            "chat_sql_budget readonly_rounds={readonly_rounds} total_sql={}",
            statements.len()
        );
        if let Some(path) = std::env::var_os("ASTRA_TEST_SQL_BUDGET_REPORT") {
            let record = json!({"readonly_rounds": readonly_rounds, "total_sql": statements.len(), "statements": statements});
            use std::io::Write as _;
            let mut report = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(report, "{record}").unwrap();
        }
        budgets.push((readonly_rounds, statements));
    }
    let baseline = budgets[0].1.len();
    for (rounds, statements) in &budgets {
        assert_eq!(
            statements.len(),
            baseline,
            "{rounds} readonly rounds must add no framework SQL; base={baseline} total={}",
            statements.len()
        );
    }
    assert!(
        baseline <= 20,
        "ordinary chat used {baseline} SQL statements including transactions and drained background work:\n{}",
        budgets[0].1.join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1; provider is scripted localhost HTTP"]
async fn explicit_explain_keeps_diagnostic_ingestion_after_complete_drain() {
    require_system_e2e_env();
    let b = bootstrap().await;
    let model = format!("mock-{}", b.ctx.suffix);
    b.ctx
        .install_native_provider(
            &b.auth_header,
            vec![ProviderScript::new(
                "explicit diagnostic recording",
                move |request| request.body["model"] == model && request.body["stream"] == true,
                vec![answer(
                    json!({"role": "assistant", "content": "Observed."}),
                    "stop",
                )],
            )],
        )
        .await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", &b.auth_header)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({
                "message": "Reply briefly.",
                "session_id": b.ctx.session_id,
                "model_selection": seeded_model_selection(&b.ctx),
                "explain": true
            })
            .to_string(),
        ))
        .unwrap();
    let (status, body) = collect_full_sse_stream(&b.ctx.app, request, 30).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert!(
        parse_sse_events(&body)
            .iter()
            .any(|event| { event["type"] == "run_finished" && event["status"] == "completed" }),
        "{body}"
    );
    assert!(
        b.ctx
            .app_state
            .drain_background_runs(std::time::Duration::from_secs(5))
            .await
    );
    let recorded: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ? AND event_type IN ('trace_span', 'llm_round')",
    )
    .bind(&b.ctx.user_id)
    .bind(&b.ctx.session_id)
    .fetch_one(&b.ctx.pool)
    .await
    .unwrap();
    assert!(recorded.0 > 0, "explicit diagnostics must remain available");
    assert_eq!(b.ctx.native_provider_requests().await.len(), 1);
    b.ctx.close().await;
}
