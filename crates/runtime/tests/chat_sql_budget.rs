//! Public chat persistence budget, including work after the response ends.
#![cfg(feature = "e2e-hooks")]

#[allow(dead_code, unused_imports)]
#[path = "system_matrix_http_e2e/harness.rs"]
mod harness;
mod test_support;

use harness::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use tracing_subscriber::prelude::*;

#[derive(Default)]
struct Statement(String, f64);

impl tracing::field::Visit for Statement {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "db.statement" && !value.trim().is_empty() {
            self.0 = value.split_whitespace().collect::<Vec<_>>().join(" ");
        } else if field.name() == "summary" && self.0.is_empty() {
            self.0 = value.to_owned();
        }
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if field.name() == "elapsed_secs" {
            self.1 = value;
        }
    }

    fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
}

struct ObservedSql {
    sql: String,
    elapsed: std::time::Duration,
    finished_at: std::time::Instant,
}

#[derive(Clone, Default)]
struct SqlCapture(Arc<Mutex<Option<Vec<ObservedSql>>>>);

// This ordinary-chat probe owns sequential opening/terminal transactions.
// If a scenario introduces overlapping transactions, report the ambiguity
// instead of attributing concurrent connection events to the wrong owner.
fn transaction_windows(samples: &[ObservedSql]) -> Value {
    let mut active = None;
    let mut windows = Vec::new();
    for (index, sample) in samples.iter().enumerate() {
        let started_at = sample.finished_at - sample.elapsed;
        if sample.sql == "BEGIN" {
            if active.is_some() {
                return json!({"overlapping_transactions":true});
            }
            active = Some((index, started_at, None));
        }
        if let Some((_, _, ref mut lock_start)) = active
            && lock_start.is_none()
            && sample.sql.contains("FOR UPDATE")
        {
            // Includes waiting to acquire the first lock, therefore an
            // upper bound on holding that transaction's first lock.
            *lock_start = Some(started_at);
        }
        if matches!(sample.sql.as_str(), "COMMIT" | "ROLLBACK") {
            let Some((first, tx_start, lock_start)) = active.take() else {
                return json!({"unmatched_transaction_boundary":true});
            };
            let sql_ms = samples[first..=index]
                .iter()
                .map(|sample| sample.elapsed.as_secs_f64() * 1000.0)
                .sum::<f64>();
            let total_ms = sample.finished_at.duration_since(tx_start).as_secs_f64() * 1000.0;
            windows.push(json!({
                "statement_count": index - first + 1,
                "transaction_ms": total_ms,
                "sql_ms": sql_ms,
                "between_statements_ms": (total_ms - sql_ms).max(0.0),
                "lock_wait_and_hold_upper_bound_ms": lock_start.map(|start| sample.finished_at.duration_since(start).as_secs_f64() * 1000.0),
            }));
        }
    }
    json!({"windows":windows,"open_transaction":active.is_some()})
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for SqlCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        let mut guard = self.0.lock().unwrap();
        if let Some(statements) = guard.as_mut() {
            let mut statement = Statement::default();
            event.record(&mut statement);
            statements.push(ObservedSql {
                sql: statement.0,
                elapsed: std::time::Duration::from_secs_f64(statement.1),
                finished_at: std::time::Instant::now(),
            });
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
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn unavailable_sessions_reject_both_chat_entrypoints_before_execution() {
    let b = bootstrap().await;
    let model = format!("mock-{}", b.ctx.suffix);
    b.ctx
        .install_native_provider(
            &b.auth_header,
            vec![ProviderScript::new(
                "no provider request is admitted",
                move |request| request.body["model"] == model,
                Vec::new(),
            )],
        )
        .await;
    let missing = uuid::Uuid::new_v4().to_string();
    let foreign = uuid::Uuid::new_v4().to_string();
    let closed = uuid::Uuid::new_v4().to_string();
    let deleting = uuid::Uuid::new_v4().to_string();
    for (session, owner, status) in [
        (&foreign, "unrelated-owner", "active"),
        (&closed, b.ctx.user_id.as_str(), "closed"),
        (&deleting, b.ctx.user_id.as_str(), "deleting"),
    ] {
        sqlx::query("INSERT INTO agent_sessions (session_id, user_id, status) VALUES (?, ?, ?)")
            .bind(session)
            .bind(owner)
            .bind(status)
            .execute(&b.ctx.pool)
            .await
            .unwrap();
    }
    for session in [&missing, &foreign, &closed, &deleting] {
        let payload = json!({"message":"must not execute", "session_id":session,
            "model_selection":seeded_model_selection(&b.ctx)});
        let (status, error) =
            post_json(&b.ctx.app, "/chat", Some(&b.auth_header), payload.clone()).await;
        assert_eq!(status, axum::http::StatusCode::NOT_FOUND, "{error}");
        assert_eq!(error["error_code"], "session_unavailable", "{error}");
        assert_eq!(error["metadata"]["admission_state"], "rejected", "{error}");
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/chat/stream")
            .header("authorization", &b.auth_header)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(payload.to_string()))
            .unwrap();
        let (_, body) = collect_full_sse_stream(&b.ctx.app, request, 5).await;
        let events = parse_sse_events(&body);
        let error = events
            .iter()
            .find(|event| event["type"] == "error")
            .expect(&body);
        assert_eq!(error["error_code"], "session_unavailable", "{body}");
        assert_eq!(error["metadata"]["admission_state"], "rejected", "{body}");
        assert!(
            !events.iter().any(|event| event["type"] == "run_started"),
            "{body}"
        );
    }
    assert!(b.ctx.native_provider_requests().await.is_empty());
    let runs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_runs WHERE user_id = ?")
        .bind(&b.ctx.user_id)
        .fetch_one(&b.ctx.pool)
        .await
        .unwrap();
    assert_eq!(
        runs, 0,
        "rejection must not create an executable or failed Run"
    );
    assert!(
        b.ctx
            .app_state
            .drain_background_runs(std::time::Duration::from_secs(5))
            .await
    );
    b.ctx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
async fn ordinary_chat_releases_session_locks_before_waiting_for_the_provider() {
    use tokio::sync::Notify;
    let b = bootstrap().await;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let signal = entered.clone();
    let model = format!("mock-{}", b.ctx.suffix);
    let response = format!("data: {}\n\n", json!({
        "id":"held-provider", "object":"chat.completion.chunk", "model":model,
        "choices":[{"index":0,"delta":{"role":"assistant","content":"Locks were released."},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":42,"completion_tokens":7,"total_tokens":49}
    })).into_bytes();
    b.ctx
        .install_native_provider(
            &b.auth_header,
            vec![ProviderScript::new(
                "provider wait after committed opening",
                move |request| {
                    let matches = request.body["model"] == model && request.body["stream"] == true;
                    if matches {
                        signal.notify_one();
                    }
                    matches
                },
                vec![ProviderResponse::Stream {
                    content_type: "text/event-stream",
                    chunks: vec![response, b"data: [DONE]\n\n".to_vec()],
                    release_before_chunk: Some((0, release.clone())),
                }],
            )],
        )
        .await;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/chat/stream")
        .header("authorization", &b.auth_header)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            json!({"message":"hi", "session_id":b.ctx.session_id,
            "model_selection":seeded_model_selection(&b.ctx)})
            .to_string(),
        ))
        .unwrap();
    let app = b.ctx.app.clone();
    let chat = tokio::spawn(async move { collect_full_sse_stream(&app, request, 30).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    // An independent connection must be able to lock both admission owners
    // while the provider response is deliberately blocked. Close on drop also
    // makes a failed timeout unable to return a busy connection to the pool.
    let mut connection = b.ctx.pool.acquire().await.unwrap();
    connection.close_on_drop();
    let available = tokio::time::timeout(std::time::Duration::from_secs(2), sqlx::query(
        "SELECT session.status FROM agent_session_lifecycle_fences fence
         JOIN agent_sessions session ON session.user_id = fence.user_id AND session.session_id = fence.session_id
         WHERE fence.user_id = ? AND fence.session_id = ? FOR UPDATE",
    ).bind(&b.ctx.user_id).bind(&b.ctx.session_id).fetch_one(&mut *connection)).await;
    drop(connection);
    release.notify_one();
    let (status, body) = chat.await.unwrap();
    assert!(
        available.is_ok(),
        "a Session transaction spanned the provider wait"
    );
    available.unwrap().unwrap();
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert!(
        parse_sse_events(&body)
            .iter()
            .any(|event| event["type"] == "run_finished" && event["status"] == "completed"),
        "{body}"
    );
    assert!(body.contains("Locks were released."), "{body}");
    assert!(
        b.ctx
            .app_state
            .drain_background_runs(std::time::Duration::from_secs(5))
            .await
    );
    b.ctx.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1; provider is scripted localhost HTTP"]
async fn ordinary_chat_and_readonly_rounds_have_bounded_total_sql() {
    require_system_e2e_env();
    let capture = SqlCapture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))
        .expect("this integration binary owns its SQL subscriber");

    let mut budgets = Vec::new();
    for (scenario, readonly_rounds, warm_context) in [
        ("ordinary", 0, false),
        ("discovery", 1, false),
        ("discovery", 3, false),
        ("agent_list", 1, false),
        ("agent_list", 3, false),
        ("ordinary", 0, true),
        ("agent_list", 3, true),
    ] {
        let executes_agent = scenario == "agent_list";
        let tool_call_count = readonly_rounds + usize::from(executes_agent);
        let b = bootstrap().await;
        let model = format!("mock-{}", b.ctx.suffix);
        let mut responses = (0..readonly_rounds)
            .map(|round| {
                answer(
                    json!({"role": "assistant", "content": "", "tool_calls": [{
                        "id": format!("discover-{round}"), "type": "function",
                        "function": if executes_agent {
                            json!({"name":"invoke_tool", "arguments":json!({"name":"agent","arguments":{"action":"list"}}).to_string()})
                        } else {
                            json!({"name":"tool_search", "arguments":"{\"query\":\"select:agent\"}"})
                        }
                    }]}),
                    "tool_calls",
                )
            })
            .collect::<Vec<_>>();
        if executes_agent {
            responses.insert(
                0,
                answer(
                    json!({"role":"assistant","content":"", "tool_calls":[{
                        "id":"select-agent", "type":"function",
                        "function":{"name":"tool_search","arguments":"{\"query\":\"select:agent\"}"}
                    }]}),
                    "tool_calls",
                ),
            );
        }
        responses.push(answer(
            json!({"role": "assistant", "content": "Discovery complete."}),
            "stop",
        ));
        if warm_context {
            responses.insert(
                0,
                answer(
                    json!({"role":"assistant", "content":"Warm context ready."}),
                    "stop",
                ),
            );
        }
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
        // Raw history can contain an imported row without a canonical head.
        // It stays visible to history readers, never an alternate prompt source.
        let imported = "Imported transcript is not canonical model context.";
        let mut hash = Sha256::new();
        hash.update(b"assistant\0");
        hash.update(imported.as_bytes());
        hash.update([0]);
        sqlx::query(
            "INSERT INTO session_transcript_items (session_id, item_seq, user_id, role, content, source_event_id, content_hash, created_at) VALUES (?, 1, ?, 'assistant', ?, 'imported-history', ?, NOW(6))",
        ).bind(&b.ctx.session_id).bind(&b.ctx.user_id).bind(imported)
            .bind(format!("{:x}", hash.finalize())).execute(&b.ctx.pool).await.unwrap();
        if warm_context {
            let warm_request = axum::http::Request::builder()
                .method("POST")
                .uri("/chat/stream")
                .header("authorization", &b.auth_header)
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    json!({
                        "message":"Remember the first turn.", "session_id":b.ctx.session_id,
                        "model_selection":seeded_model_selection(&b.ctx)
                    })
                    .to_string(),
                ))
                .unwrap();
            let (status, body) = collect_full_sse_stream(&b.ctx.app, warm_request, 30).await;
            assert_eq!(status, axum::http::StatusCode::OK, "{body}");
            assert!(
                parse_sse_events(&body)
                    .iter()
                    .any(|event| event["type"] == "run_finished" && event["status"] == "completed"),
                "{body}"
            );
            assert!(body.contains("Warm context ready."), "{body}");
        }
        let message = if executes_agent {
            "List agent status."
        } else if readonly_rounds == 0 {
            "hi"
        } else {
            "Discover the agent tool without invoking it."
        };
        *capture.0.lock().unwrap() = Some(Vec::new());
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/chat/stream")
            .header("authorization", &b.auth_header)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                json!({
                    "message": message,
                    "session_id": b.ctx.session_id,
                    "model_selection": seeded_model_selection(&b.ctx)
                })
                .to_string(),
            ))
            .unwrap();
        let (status, body) = collect_full_sse_stream(&b.ctx.app, request, 30).await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        let response_finished_at = std::time::Instant::now();
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
            tool_call_count,
            "tool outcomes must be delivered exactly once: {body}"
        );
        if executes_agent {
            let endings = events
                .iter()
                .filter(|event| event["type"] == "tool_call_end" && event["tool"] == "agent")
                .collect::<Vec<_>>();
            assert_eq!(
                endings.len(),
                readonly_rounds,
                "each real handler call needs its own ending: {body}"
            );
            assert!(
                endings
                    .iter()
                    .all(|event| event["success"] == true || event["status"] == "completed"),
                "every actual readonly call must succeed, not merely be discovered or skipped: {body}"
            );
        }
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
        let samples = capture.0.lock().unwrap().take().unwrap();
        let transactions = transaction_windows(&samples);
        let after_response_sql = samples
            .iter()
            .filter(|sample| sample.finished_at > response_finished_at)
            .count();
        let statements = samples
            .iter()
            .map(|sample| sample.sql.clone())
            .collect::<Vec<_>>();
        assert!(
            !statements
                .iter()
                .any(|sql| sql.contains("FROM conversation_log")),
            "ordinary chat must use its admitted canonical history, without legacy CSL probes"
        );
        assert!(
            !statements
                .iter()
                .any(|sql| sql.contains("run_display_projections")),
            "ordinary chat must not maintain a separate display store"
        );
        let diagnostic_rows: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM agent_events WHERE user_id = ? AND session_id = ?",
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
        let (history_status, history) = get_json(
            &b.ctx.app,
            &format!(
                "/sessions/{}/transcript?scope=root_conversation&limit=20",
                b.ctx.session_id
            ),
            Some(b.auth_header.as_str()),
            &[],
        )
        .await;
        assert_eq!(history_status, axum::http::StatusCode::OK, "{history}");
        let items = history["items"]
            .as_array()
            .expect("refreshed history items");
        assert!(
            items
                .iter()
                .any(|item| item["role"] == "user" && item["content"] == message),
            "{history}"
        );
        assert!(items.iter().any(|item| item["role"] == "assistant" && item["content"] == "Discovery complete."), "{history}");
        assert!(
            statements
                .iter()
                .all(|sql| !sql.contains("resource_usage") && !sql.contains("resource_limits")),
            "ordinary Run usage belongs to its canonical terminal, not a synchronous daily quota mirror"
        );
        assert!(
            !statements.is_empty(),
            "the SQL observer must capture real database work"
        );
        let requests = b.ctx.native_provider_requests().await;
        assert_eq!(
            requests.len(),
            tool_call_count + 1 + usize::from(warm_context)
        );
        assert!(
            requests
                .iter()
                .all(|request| !request.body.to_string().contains(imported)),
            "raw transcript without canonical Context must not be injected into provider requests"
        );
        if warm_context {
            assert!(
                requests.last().unwrap().body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "assistant"
                        && message["content"] == "Warm context ready."),
                "next turn must consume the committed canonical history"
            );
        }
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
            "chat_sql_budget scenario={scenario} warm_context={warm_context} readonly_rounds={readonly_rounds} tool_calls={tool_call_count} total_sql={}",
            statements.len()
        );
        if let Some(path) = std::env::var_os("ASTRA_TEST_SQL_BUDGET_REPORT") {
            let statement_timings = samples
                .iter()
                .map(|sample| {
                    json!({
                        "elapsed_ms": sample.elapsed.as_secs_f64() * 1000.0,
                        "sql": sample.sql,
                    })
                })
                .collect::<Vec<_>>();
            let record = json!({"scenario":scenario, "warm_context":warm_context, "readonly_rounds": readonly_rounds, "tool_calls":tool_call_count, "total_sql": statements.len(), "statements": statements, "statement_timings":statement_timings, "transactions":transactions, "after_response_sql":after_response_sql});
            use std::io::Write as _;
            let mut report = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            writeln!(report, "{record}").unwrap();
        }
        budgets.push((scenario, readonly_rounds, warm_context, statements));
    }
    for (scenario, rounds, warm_context, statements) in &budgets {
        let baseline = budgets
            .iter()
            .find(|(name, _, warm, _)| *name == "ordinary" && warm == warm_context)
            .unwrap()
            .3
            .len();
        assert_eq!(
            statements.len(),
            baseline,
            "{scenario} {rounds} readonly rounds (warm={warm_context}) must add no framework SQL; base={baseline} total={}",
            statements.len()
        );
        assert!(
            baseline <= 20,
            "ordinary chat (warm={warm_context}) used {baseline} SQL statements including transactions and drained background work:\n{}",
            statements.join("\n")
        );
    }
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
