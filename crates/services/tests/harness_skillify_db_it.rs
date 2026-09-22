mod common;

use std::sync::{Arc, Mutex};

use astra_services::{
    DatabaseHarnessService, HarnessDecisionRequest, HarnessService, SkillifyAgentCitation,
    SkillifyAgentDraft, SkillifyAgentExecutor, SkillifyAgentOutput, SkillifyAgentRequest,
    SkillifyAgentRule, SkillifyRunRequest, SkillifySourceFile,
};
use async_trait::async_trait;
use axum::http::StatusCode;
use serde_json::json;
use serial_test::serial;
use sqlx::Row;
use uuid::Uuid;

fn agent_event_fixture_payload_hash(payload: serde_json::Value) -> String {
    astra_services::observation_capture::canonical_observation_payload_hash(
        astra_services::observation_capture::ObservationPayloadDomain::AgentEvent,
        &payload,
    )
}

#[derive(Default)]
struct CapturingSkillifyExecutor {
    request: Mutex<Option<SkillifyAgentRequest>>,
    output: Mutex<Option<SkillifyAgentOutput>>,
    pool: Mutex<Option<sqlx::Pool<sqlx::MySql>>>,
    owner_observed_running: Mutex<bool>,
    failure: Mutex<Option<String>>,
}

#[async_trait]
impl SkillifyAgentExecutor for CapturingSkillifyExecutor {
    async fn synthesize_skill_drafts(
        &self,
        request: SkillifyAgentRequest,
    ) -> Result<SkillifyAgentOutput, String> {
        let pool = self.pool.lock().expect("pool lock").clone();
        if let Some(pool) = pool {
            let row = sqlx::query(
                "SELECT user_id, status FROM harness_runs WHERE harness_run_id = ? LIMIT 1",
            )
            .bind(&request.harness_run_id)
            .fetch_optional(&pool)
            .await
            .map_err(|error| format!("load durable harness owner: {error}"))?
            .ok_or_else(|| "durable harness owner was not created before inference".to_string())?;
            if row.get::<String, _>("user_id") != request.user_id
                || row.get::<String, _>("status") != "running"
            {
                return Err("durable harness owner was not running for this user".to_string());
            }
            *self
                .owner_observed_running
                .lock()
                .expect("owner observation lock") = true;
        }
        *self.request.lock().expect("capture lock") = Some(request);
        if let Some(error) = self.failure.lock().expect("failure lock").clone() {
            return Err(error);
        }
        Ok(self
            .output
            .lock()
            .expect("output lock")
            .clone()
            .unwrap_or_else(|| SkillifyAgentOutput {
                extractor: "capturing-test-executor".to_string(),
                subagent_strategy: json!({"mode": "capture"}),
                drafts: Vec::new(),
            }))
    }
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_skillify_uses_event_level_sources_and_rejects_corrupt_events() {
    let shared_pool = common::setup_pool().await;
    let pool = shared_pool.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();

    sqlx::query("INSERT INTO agent_sessions (session_id, user_id, title) VALUES (?, ?, 'skillify integration session')")
        .bind(&session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("insert agent session");
    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
         payload_hash, ingestion_write_id) \
         VALUES (?, ?, ?, 'assistant_message', 'Prefer conclusion-first answers.', ?, ?)",
    )
    .bind(&event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(agent_event_fixture_payload_hash(json!({
        "event_id": &event_id,
        "session_id": &session_id,
        "user_id": &user_id,
        "event_type": "assistant_message",
        "content": "Prefer conclusion-first answers.",
    })))
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert agent event");

    let executor = Arc::new(CapturingSkillifyExecutor::default());
    *executor.pool.lock().expect("pool lock") = Some(pool.clone());
    let service = DatabaseHarnessService::new(shared_pool.clone())
        .with_skillify_agent_executor(executor.clone());
    let run = service
        .create_skillify_run(
            user_id.clone(),
            SkillifyRunRequest {
                session_ids: vec![session_id.clone()],
                source_files: None,
                skill_name: None,
                topic: None,
                target_scope: Some("personal".to_string()),
                idempotency_key: None,
            },
        )
        .await
        .expect("create skillify run from valid session event");

    let captured = executor
        .request
        .lock()
        .expect("capture lock")
        .take()
        .expect("skillify executor request captured");
    assert_eq!(captured.harness_run_id, run.harness_run_id);
    assert!(
        *executor
            .owner_observed_running
            .lock()
            .expect("owner observation lock"),
        "harness owner must be durable and running before model execution"
    );
    assert_eq!(captured.source_packets.len(), 1);
    let packet = &captured.source_packets[0];
    assert_eq!(packet.event_id, event_id);
    assert_eq!(packet.session_id, session_id);
    assert_eq!(packet.source_id, event_id);
    assert_eq!(packet.source_type, "session_event");
    assert_eq!(packet.title, format!("assistant_message ({session_id})"));

    sqlx::query("UPDATE agent_events SET event_type = '' WHERE event_id = ?")
        .bind(&event_id)
        .execute(&pool)
        .await
        .expect("corrupt event_type");

    let err = service
        .create_skillify_run(
            user_id.clone(),
            SkillifyRunRequest {
                session_ids: vec![session_id.clone()],
                source_files: None,
                skill_name: None,
                topic: None,
                target_scope: Some("personal".to_string()),
                idempotency_key: None,
            },
        )
        .await
        .expect_err("empty persisted event_type must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("agent_events.event_type"),
        "unexpected error detail: {}",
        err.1.detail
    );

    let _ = sqlx::query("DELETE FROM harness_runs WHERE harness_run_id = ?")
        .bind(&run.harness_run_id)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ?")
        .bind(&event_id)
        .execute(&pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
        .bind(&session_id)
        .execute(&pool)
        .await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_skillify_citations_point_to_review_items() {
    let shared_pool = common::setup_pool().await;
    let pool = shared_pool.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();

    sqlx::query("INSERT INTO agent_sessions (session_id, user_id, title) VALUES (?, ?, 'skillify citation session')")
        .bind(&session_id)
        .bind(&user_id)
        .execute(&pool)
        .await
        .expect("insert agent session");
    sqlx::query(
        "INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, \
         payload_hash, ingestion_write_id) \
         VALUES (?, ?, ?, 'user_message', 'Always cite the event-level source.', ?, ?)",
    )
    .bind(&event_id)
    .bind(&session_id)
    .bind(&user_id)
    .bind(agent_event_fixture_payload_hash(json!({
        "event_id": &event_id,
        "session_id": &session_id,
        "user_id": &user_id,
        "event_type": "user_message",
        "content": "Always cite the event-level source.",
    })))
    .bind(Uuid::new_v4().to_string())
    .execute(&pool)
    .await
    .expect("insert agent event");

    let executor = Arc::new(CapturingSkillifyExecutor::default());
    *executor.output.lock().expect("output lock") = Some(SkillifyAgentOutput {
        extractor: "capturing-test-executor".to_string(),
        subagent_strategy: json!({"mode": "citation"}),
        drafts: vec![SkillifyAgentDraft {
            candidate_name: "citation-skill".to_string(),
            description: "Citation Skill".to_string(),
            target_scope: "personal".to_string(),
            publish_visibility: "private".to_string(),
            content_markdown: "# Citation Skill\n\n## When to use\nFor reviews.\n\n- Cite event-level evidence.\n\n## Example\nPreserve this example.".to_string(),
            source_summary_json: json!({"source_count": 1}),
            confidence: Some(0.9),
            rules: vec![SkillifyAgentRule {
                rule_type: "preference".to_string(),
                statement: "Cite event-level evidence.".to_string(),
                rationale: "The source states that event-level source identity matters."
                    .to_string(),
                confidence: Some(0.9),
                citations: vec![SkillifyAgentCitation {
                    source_id: event_id.clone(),
                    source_excerpt: "cite the event-level source".to_string(),
                    source_locator_json: json!({"event_id": "forged", "start_byte": 999}),
                }],
            }],
        }],
    });
    let service = DatabaseHarnessService::new(shared_pool.clone())
        .with_skillify_agent_executor(executor.clone());
    let run = service
        .create_skillify_run(
            user_id.clone(),
            SkillifyRunRequest {
                session_ids: vec![session_id.clone()],
                source_files: None,
                skill_name: None,
                topic: None,
                target_scope: Some("personal".to_string()),
                idempotency_key: None,
            },
        )
        .await
        .expect("create skillify run with cited rule");

    let items = service
        .list_run_items(user_id.clone(), run.harness_run_id.clone())
        .await
        .expect("list generated review items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].item_type, "skill_rule");
    assert_eq!(items[0].status, "pending_review");

    let drafts = service
        .list_skill_drafts(user_id.clone(), run.harness_run_id.clone())
        .await
        .expect("list generated skill drafts");
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].rules.len(), 1);
    let rule = &drafts[0].rules[0];
    assert_eq!(rule.citations.len(), 1);
    let citation = &rule.citations[0];
    assert_eq!(
        citation.skill_rule_id.as_deref(),
        Some(rule.skill_rule_id.as_str())
    );
    assert_eq!(citation.item_id, items[0].item_id);
    assert_eq!(citation.source_locator_json["event_id"], event_id);
    assert_ne!(citation.item_id, rule.skill_rule_id);
    assert_eq!(
        items[0]
            .locator_json
            .get("skill_rule_id")
            .and_then(|v| v.as_str()),
        Some(rule.skill_rule_id.as_str())
    );

    let approved_item = service
        .decide_item(
            user_id.clone(),
            run.harness_run_id.clone(),
            items[0].item_id.clone(),
            HarnessDecisionRequest {
                decision: "approve".to_string(),
                after_json: None,
                reason: Some("approve through item API".to_string()),
                idempotency_key: None,
            },
        )
        .await
        .expect("approve generated review item");
    assert_eq!(approved_item.status, "approved");

    let drafts_after_item_decision = service
        .list_skill_drafts(user_id.clone(), run.harness_run_id.clone())
        .await
        .expect("list drafts after item decision");
    let approved_rule = &drafts_after_item_decision[0].rules[0];
    assert_eq!(approved_rule.status, "approved");
    assert_eq!(drafts_after_item_decision[0].status, "ready_to_publish");
    assert_eq!(
        drafts_after_item_decision[0].content_markdown, drafts[0].content_markdown,
        "approving a rule must preserve full generated text"
    );
    assert_eq!(drafts_after_item_decision[0].revision, drafts[0].revision);
    assert_eq!(
        citation.source_locator_json["validation"],
        "exact_source_match"
    );
    let start = citation.source_locator_json["start_byte"].as_u64().unwrap() as usize;
    let end = citation.source_locator_json["end_byte"].as_u64().unwrap() as usize;
    assert_eq!(
        &"Always cite the event-level source."[start..end],
        citation.evidence_text_preview.as_deref().unwrap()
    );

    let draft_after_rule_decision = service
        .decide_skill_rule(
            user_id.clone(),
            run.harness_run_id.clone(),
            drafts_after_item_decision[0].skill_draft_id.clone(),
            approved_rule.skill_rule_id.clone(),
            HarnessDecisionRequest {
                decision: "request_revision".to_string(),
                after_json: None,
                reason: Some("request revision through rule API".to_string()),
                idempotency_key: None,
            },
        )
        .await
        .expect("request rule revision");
    assert_eq!(draft_after_rule_decision.rules[0].status, "needs_revision");
    assert_eq!(draft_after_rule_decision.status, "pending_rule_review");

    let revised_body = format!(
        "{}\n\n## Additional constraint\nKeep examples.",
        draft_after_rule_decision.content_markdown
    );
    sqlx::query("UPDATE harness_runs SET output_json = JSON_SET(output_json, '$.authoring', CAST(? AS JSON)) WHERE harness_run_id = ?")
        .bind(json!({"evaluation": {"status": "prepared"}, "candidate_revision_id": "before-review", "baseline": {"version_id":"old-version"}}).to_string())
        .bind(&run.harness_run_id).execute(&pool).await.unwrap();
    let edited = service.decide_skill_rule(user_id.clone(), run.harness_run_id.clone(),
        draft_after_rule_decision.skill_draft_id.clone(), approved_rule.skill_rule_id.clone(),
        HarnessDecisionRequest { decision: "edit".into(), after_json: Some(json!({
            "statement": "Cite sources and keep examples.", "content_markdown": &revised_body,
        })), reason: None, idempotency_key: None }).await.unwrap();
    assert_eq!(edited.content_markdown, revised_body);
    assert_eq!(edited.revision, drafts[0].revision + 1);
    let updated_run = service
        .get_run(user_id.clone(), run.harness_run_id.clone())
        .await
        .unwrap();
    assert_eq!(
        updated_run.output_json["authoring"]["evaluation"]["status"],
        "stale"
    );
    assert_eq!(
        updated_run.output_json["authoring"]["baseline"]["version_id"],
        "old-version"
    );
    assert!(updated_run.output_json["authoring"]["candidate_revision_id"].is_null());
    let late = service
        .persist_authoring_evaluation(
            user_id.clone(),
            run.harness_run_id.clone(),
            "before-review".into(),
            astra_services::AuthoringEvaluationSummary {
                status: "prepared".into(),
                reason: "Late preparation".into(),
                experiment_id: None,
            },
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(late.0, StatusCode::CONFLICT);
    let items_after_rule_decision = service
        .list_run_items(user_id.clone(), run.harness_run_id.clone())
        .await
        .expect("list items after rule decision");
    assert_eq!(items_after_rule_decision[0].status, "approved");

    sqlx::query("UPDATE harness_items SET locator_json = 'not-json' WHERE item_id = ?")
        .bind(&items[0].item_id)
        .execute(&pool)
        .await
        .expect("corrupt persisted item locator");
    let err = service
        .list_run_items(user_id.clone(), run.harness_run_id.clone())
        .await
        .expect_err("corrupt item locator_json must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("harness_items.locator_json"),
        "unexpected error detail: {}",
        err.1.detail
    );

    sqlx::query("UPDATE harness_skill_drafts SET candidate_name = '' WHERE skill_draft_id = ?")
        .bind(&drafts[0].skill_draft_id)
        .execute(&pool)
        .await
        .expect("corrupt persisted draft candidate name");
    let err = service
        .list_skill_drafts(user_id.clone(), run.harness_run_id.clone())
        .await
        .expect_err("empty persisted draft candidate_name must fail loudly");
    assert_eq!(err.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        err.1.detail.contains("harness_skill_drafts.candidate_name"),
        "unexpected error detail: {}",
        err.1.detail
    );

    cleanup_skillify_run(&pool, &run.harness_run_id, &event_id, &session_id).await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn database_skillify_model_failure_persists_a_recoverable_terminal_run() {
    let shared_pool = common::setup_pool().await;
    let pool = shared_pool.get().clone();
    let user_id = Uuid::new_v4().to_string();
    let executor = Arc::new(CapturingSkillifyExecutor::default());
    *executor.pool.lock().expect("pool lock") = Some(pool.clone());
    *executor.failure.lock().expect("failure lock") = Some("provider unavailable".to_string());
    let service =
        DatabaseHarnessService::new(shared_pool).with_skillify_agent_executor(executor.clone());

    let error = service
        .create_skillify_run(
            user_id.clone(),
            SkillifyRunRequest {
                session_ids: Vec::new(),
                source_files: Some(vec![SkillifySourceFile {
                    file_name: "notes.md".to_string(),
                    mime_type: Some("text/markdown".to_string()),
                    content: "Prefer concise conclusions with cited evidence.".to_string(),
                }]),
                skill_name: Some("concise-review".to_string()),
                topic: None,
                target_scope: Some("personal".to_string()),
                idempotency_key: None,
            },
        )
        .await
        .expect_err("model failure must be surfaced");
    assert_eq!(
        error.0,
        StatusCode::BAD_GATEWAY,
        "unexpected failure response: {:?}",
        error.1
    );
    let metadata = error.1.metadata.as_ref().expect("failure metadata");
    let harness_run_id = metadata["harness_run_id"]
        .as_str()
        .expect("durable failed run id");
    assert_eq!(metadata["status"], "failed");

    let row = sqlx::query(
        "SELECT status, error FROM harness_runs WHERE user_id = ? AND harness_run_id = ?",
    )
    .bind(&user_id)
    .bind(harness_run_id)
    .fetch_one(&pool)
    .await
    .expect("load failed harness run");
    assert_eq!(row.get::<String, _>("status"), "failed");
    assert!(
        row.get::<String, _>("error")
            .contains("provider unavailable")
    );
    assert!(
        *executor
            .owner_observed_running
            .lock()
            .expect("owner observation lock")
    );

    sqlx::query("DELETE FROM harness_runs WHERE user_id = ? AND harness_run_id = ?")
        .bind(&user_id)
        .bind(harness_run_id)
        .execute(&pool)
        .await
        .expect("cleanup failed harness run");
}

async fn cleanup_skillify_run(
    pool: &sqlx::Pool<sqlx::MySql>,
    harness_run_id: &str,
    event_id: &str,
    session_id: &str,
) {
    let _ = sqlx::query(
        "DELETE debt
         FROM inference_invocation_settlement_debts AS debt
         INNER JOIN inference_invocations AS invocation
           ON invocation.user_id = debt.user_id
          AND invocation.invocation_id = debt.invocation_id
         WHERE invocation.harness_run_id = ?",
    )
    .bind(harness_run_id)
    .execute(pool)
    .await;
    for table in [
        "model_request_context_events",
        "inference_provider_attempts",
        "inference_invocations",
        "inference_routes",
    ] {
        let sql = format!("DELETE FROM {table} WHERE harness_run_id = ?");
        let _ = sqlx::query(&sql).bind(harness_run_id).execute(pool).await;
    }
    for table in [
        "harness_citations",
        "harness_items",
        "harness_skill_rules",
        "harness_skill_drafts",
        "harness_runs",
    ] {
        let sql = format!("DELETE FROM {table} WHERE harness_run_id = ?");
        let _ = sqlx::query(&sql).bind(harness_run_id).execute(pool).await;
    }
    let _ = sqlx::query("DELETE FROM agent_events WHERE event_id = ?")
        .bind(event_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM agent_sessions WHERE session_id = ?")
        .bind(session_id)
        .execute(pool)
        .await;
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn authoring_retry_uses_fresh_attempt_and_preserves_explicit_replay() {
    let shared_pool = common::setup_pool().await;
    let pool = shared_pool.get().clone();
    let owner = Uuid::new_v4().to_string();
    let executor = Arc::new(CapturingSkillifyExecutor::default());
    *executor.failure.lock().unwrap() = Some("transient provider failure".into());
    let service =
        DatabaseHarnessService::new(shared_pool).with_skillify_agent_executor(executor.clone());
    let request = |key: Option<&str>| astra_services::AuthoringIntentRequest {
        create_new: false,
        target_skill: None,
        validation_task: None,
        goal: "Create a concise review skill".into(),
        idempotency_key: key.map(str::to_string),
    };
    assert!(
        service
            .create_authoring_intent(owner.clone(), String::new(), request(Some("attempt-1")))
            .await
            .is_err()
    );
    *executor.failure.lock().unwrap() = None;
    let failure = service
        .create_authoring_intent(owner.clone(), String::new(), request(Some("attempt-1")))
        .await
        .unwrap_err();
    assert_eq!(failure.0, StatusCode::CONFLICT);
    let recovered = service
        .create_authoring_intent(owner.clone(), String::new(), request(Some("attempt-2")))
        .await
        .unwrap();
    let replay = service
        .create_authoring_intent(owner.clone(), String::new(), request(Some("attempt-2")))
        .await
        .unwrap();
    assert_eq!(
        recovered.harness_run.harness_run_id,
        replay.harness_run.harness_run_id
    );
    let fresh = service
        .create_authoring_intent(owner.clone(), String::new(), request(None))
        .await
        .unwrap();
    assert_ne!(
        recovered.harness_run.harness_run_id,
        fresh.harness_run.harness_run_id
    );
    for run in [
        recovered.harness_run.harness_run_id,
        fresh.harness_run.harness_run_id,
    ] {
        cleanup_skillify_run(&pool, &run, "", "").await;
    }
    sqlx::query("DELETE FROM harness_runs WHERE user_id = ?")
        .bind(&owner)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires live DB: run with ASTRA_TEST_DB_IT=1"]
#[serial]
async fn authoring_pins_old_body_before_generation_and_preserves_identity_on_retry() {
    use astra_services::harness::AuthoringSkillTarget;
    use astra_services::personal_skills::{
        CreateUserSkillSource, DatabasePersonalSkillStore, SubmitUserSkillVersion,
    };
    let shared = common::setup_pool().await;
    let pool = shared.get().clone();
    let owner = Uuid::new_v4().to_string();
    let session_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO agent_sessions (session_id, user_id, title) VALUES (?, ?, 'authoring task')",
    )
    .bind(&session_id)
    .bind(&owner)
    .execute(&pool)
    .await
    .unwrap();
    let payload = json!({"event_id": &event_id, "session_id": &session_id, "user_id": &owner,
        "event_type":"user_query", "content":"Return {\"ok\":true} as JSON."});
    sqlx::query("INSERT INTO agent_events (event_id, session_id, user_id, event_type, content, payload_hash, ingestion_write_id) VALUES (?, ?, ?, 'user_query', ?, ?, ?)")
        .bind(&event_id).bind(&session_id).bind(&owner).bind(payload["content"].as_str().unwrap())
        .bind(agent_event_fixture_payload_hash(payload.clone())).bind(Uuid::new_v4().to_string())
        .execute(&pool).await.unwrap();
    let store = DatabasePersonalSkillStore::new(shared.clone());
    store
        .ensure_source(
            &owner,
            CreateUserSkillSource {
                skill_name: "review".into(),
                visibility: Some("public".into()),
            },
        )
        .await
        .unwrap();
    let baseline = store
        .submit_version(
            &owner,
            "review",
            SubmitUserSkillVersion {
                version: "1.0.0".into(),
                manifest_json: json!({"name":"review","version":"1.0.0","description":"Review"}),
                content_markdown: "# Review\n\n## Steps\nPreserve existing examples.".into(),
                status: Some("draft".into()),
            },
        )
        .await
        .unwrap();
    let executor = Arc::new(CapturingSkillifyExecutor::default());
    *executor.output.lock().unwrap() = Some(SkillifyAgentOutput {
        extractor: "test".into(),
        subagent_strategy: json!({}),
        drafts: vec![SkillifyAgentDraft {
            candidate_name: "review".into(),
            description: "Improved review".into(),
            target_scope: "personal".into(),
            publish_visibility: "private".into(),
            content_markdown: format!("{}\nUse citations.", baseline.content_markdown),
            source_summary_json: json!({}),
            confidence: None,
            rules: vec![SkillifyAgentRule {
                rule_type: "workflow".into(),
                statement: "Preserve existing examples.".into(),
                rationale: "Keep the old capability.".into(),
                confidence: None,
                citations: vec![SkillifyAgentCitation {
                    source_id: format!("skill-version:{}", baseline.version_id),
                    source_excerpt: "Preserve existing examples.".into(),
                    source_locator_json: json!({}),
                }],
            }],
        }],
    });
    let service =
        DatabaseHarnessService::new(shared).with_skillify_agent_executor(executor.clone());
    let request = astra_services::AuthoringIntentRequest {
        goal: "Improve review with citations".into(),
        create_new: false,
        target_skill: Some(AuthoringSkillTarget {
            skill_name: "review".into(),
            version_id: baseline.version_id.clone(),
        }),
        validation_task: None,
        idempotency_key: Some("frozen-attempt".into()),
    };
    let result = service
        .create_authoring_intent(owner.clone(), session_id.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(result.operation, "improve");
    assert_eq!(
        store.list_sources(&owner, Some("review")).await.unwrap()[0].visibility,
        "public",
        "candidate materialization must preserve an existing source's visibility"
    );
    assert_eq!(
        result.harness_run.output_json["authoring"]["baseline"]["version_id"],
        baseline.version_id
    );
    let captured = executor.request.lock().unwrap().take().unwrap();
    assert_eq!(captured.skill_name.as_deref(), Some("review"));
    assert!(
        captured
            .source_packets
            .iter()
            .any(|source| source.event_type == "skill_baseline"
                && source.content == baseline.content_markdown)
    );
    let replay = service
        .create_authoring_intent(owner.clone(), session_id.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(
        result.harness_run.harness_run_id,
        replay.harness_run.harness_run_id
    );
    assert!(
        executor.request.lock().unwrap().is_none(),
        "retry must not regenerate"
    );
    let task_source = captured
        .source_packets
        .iter()
        .find(|source| source.event_type == "user_query")
        .unwrap();
    let mut validation = request.clone();
    validation.validation_task = Some(astra_services::harness::AuthoringValidationTask {
        source_id: task_source.source_id.clone(),
        expected_result: json!({"ok":true}),
    });
    let selected = service
        .create_authoring_intent(owner.clone(), session_id.clone(), validation.clone())
        .await
        .unwrap();
    let input = selected.evaluation_input.as_ref().unwrap();
    assert_eq!(input.case.message, task_source.content);
    assert_eq!(input.target.baseline.revision_id, baseline.version_id);
    validation.validation_task.as_mut().unwrap().expected_result = json!({"ok":false});
    let conflict = service
        .create_authoring_intent(owner.clone(), session_id.clone(), validation)
        .await
        .unwrap_err();
    assert_eq!(
        conflict.0,
        StatusCode::CONFLICT,
        "task identity must freeze before a plan exists"
    );
    let resumed = service
        .create_authoring_intent(owner.clone(), session_id.clone(), request)
        .await
        .unwrap();
    assert_eq!(resumed.evaluation_input, selected.evaluation_input);
    cleanup_skillify_run(
        &pool,
        &result.harness_run.harness_run_id,
        &event_id,
        &session_id,
    )
    .await;
    for table in ["user_skill_versions", "user_skill_sources"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE owner_user_id = ?"))
            .bind(&owner)
            .execute(&pool)
            .await
            .unwrap();
    }
}
