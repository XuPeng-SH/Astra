//! Real MatrixOne contracts for bounded identity-collision diagnostics.

use astra_services::observation_capture::{
    ObservationCollisionReceipt, ObservationPayloadDomain, canonical_observation_payload_hash,
    record_observation_collision,
};
use sqlx::Row;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires MatrixOne; creates and removes one randomly named isolated test database"]
async fn schema_rejects_missing_capture_columns_in_existing_table() {
    use sqlx::Connection;
    let mut settings = common::require_db_it_env();
    let catalog =
        std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
    let mut admin_settings = settings.clone();
    admin_settings.database = catalog.clone();
    let mut admin = sqlx::MySqlConnection::connect(&admin_settings.database_url_with_password())
        .await
        .expect("connect bootstrap catalog");
    settings.database = format!("astra_test_probe_capture_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE `{}`", settings.database))
        .execute(&mut admin)
        .await
        .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE `{}`.agent_events (user_id VARCHAR(128) NOT NULL, event_id VARCHAR(128) NOT NULL, PRIMARY KEY(user_id, event_id))",
        settings.database,
    )).execute(&mut admin).await.unwrap();
    let bootstrap = astra_services::storage::ensure_core_schema(&settings, &catalog).await;
    // The only destructive target is the unique database this test created.
    sqlx::query(&format!("DROP DATABASE `{}`", settings.database))
        .execute(&mut admin)
        .await
        .expect("remove isolated schema probe");
    let error = bootstrap.expect_err("missing capture columns must prevent startup");
    assert!(
        error.to_string().contains("payload_hash"),
        "unexpected schema rejection: {error}"
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1 and a fresh ASTRA_DATABASE"]
async fn event_api_replays_exact_request_and_rejects_changed_agent_or_lineage() {
    use astra_services::events::{
        DatabaseEventService, EventCreateRequestData, EventIngestionSource, EventService,
    };
    let (pool, settings) = common::setup_pool_and_settings().await;
    let owner = Uuid::new_v4().to_string();
    let session = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agent_sessions (session_id, user_id, status, event_count) VALUES (?, ?, 'active', 0)")
        .bind(&session).bind(&owner).execute(pool.get()).await.unwrap();
    let service = DatabaseEventService::new(settings).with_pool(pool.clone());
    let request = EventCreateRequestData {
        ingestion_source: EventIngestionSource::Client,
        event_id: Some(Uuid::new_v4().to_string()),
        session_id: session.clone(),
        event_type: "capture-test".into(),
        content: "immutable content".into(),
        agent_id: Some("original-agent".into()),
        agent_version: None,
        parent_event_id: None,
        parent_event_ids: None,
        causal_chain_id: None,
        metadata: None,
    };
    assert!(
        !service
            .create_event(owner.clone(), request.clone())
            .await
            .unwrap()
            .idempotent_replay
    );
    assert!(
        service
            .create_event(owner.clone(), request.clone())
            .await
            .unwrap()
            .idempotent_replay
    );
    let mut changed_agent = request.clone();
    changed_agent.agent_id = Some("different-agent".into());
    assert_eq!(
        service
            .create_event(owner.clone(), changed_agent)
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::CONFLICT
    );
    let mut changed_parent = request.clone();
    changed_parent.parent_event_id = Some("different-parent".into());
    assert_eq!(
        service
            .create_event(owner.clone(), changed_parent)
            .await
            .unwrap_err()
            .0,
        axum::http::StatusCode::CONFLICT
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT event_count FROM agent_sessions WHERE user_id = ? AND session_id = ?",
    )
    .bind(&owner)
    .bind(&session)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(count, 1);
    let collisions: u64 = sqlx::query_scalar("SELECT collision_count FROM observation_identity_collisions WHERE user_id = ? AND identity_kind = 'agent_event' AND identity_id = ?")
        .bind(&owner).bind(&request.event_id).fetch_one(pool.get()).await.unwrap();
    assert_eq!(collisions, 2);
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1 and a fresh ASTRA_DATABASE"]
async fn collision_receipts_bound_distinct_hashes_and_isolate_owners() {
    let pool = common::setup_pool().await;
    let owner = format!("collision-{}", Uuid::new_v4());
    let other_owner = format!("collision-other-{}", Uuid::new_v4());
    let stored_hash = canonical_observation_payload_hash(
        ObservationPayloadDomain::AgentEvent,
        &serde_json::json!({"original": true}),
    );
    // Separate connections contend on one durable identity. Distinct attacker
    // payloads must increase a counter, never create one receipt per payload.
    let mut tasks = tokio::task::JoinSet::new();
    for worker in 0..4 {
        let pool = pool.clone();
        let owner = owner.clone();
        let stored_hash = stored_hash.clone();
        tasks.spawn(async move {
            for sequence in 0..256 {
                let hash = canonical_observation_payload_hash(
                    ObservationPayloadDomain::AgentEvent,
                    &serde_json::json!({"worker": worker, "sequence": sequence}),
                );
                let mut tx = pool.get().begin().await.unwrap();
                record_observation_collision(
                    &mut tx,
                    ObservationCollisionReceipt {
                        user_id: &owner,
                        domain: ObservationPayloadDomain::AgentEvent,
                        identity_id: "shared-id",
                        session_id: "session",
                        stored_payload_hash: &stored_hash,
                        attempted_payload_hash: &hash,
                        source: "observation_capture_db_it",
                    },
                )
                .await
                .unwrap();
                tx.commit().await.unwrap();
            }
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    let mut tx = pool.get().begin().await.unwrap();
    record_observation_collision(
        &mut tx,
        ObservationCollisionReceipt {
            user_id: &other_owner,
            domain: ObservationPayloadDomain::AgentEvent,
            identity_id: "shared-id",
            session_id: "session",
            stored_payload_hash: &stored_hash,
            attempted_payload_hash: &stored_hash,
            source: "observation_capture_db_it",
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let rows = sqlx::query(
        "SELECT collision_count, stored_payload_hash,
                TIMESTAMPDIFF(SECOND, first_seen_at, expires_at) AS retention_seconds
         FROM observation_identity_collisions WHERE user_id = ?",
    )
    .bind(&owner)
    .fetch_all(pool.get())
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "1024 distinct hashes must occupy one row");
    assert_eq!(rows[0].get::<u64, _>("collision_count"), 1024);
    assert_eq!(rows[0].get::<String, _>("stored_payload_hash"), stored_hash);
    assert_eq!(rows[0].get::<i64, _>("retention_seconds"), 7 * 24 * 60 * 60);
    let other_count: u64 = sqlx::query_scalar(
        "SELECT collision_count FROM observation_identity_collisions WHERE user_id = ?",
    )
    .bind(&other_owner)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(other_count, 1);

    sqlx::query("UPDATE observation_identity_collisions SET expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE user_id = ?")
        .bind(&owner).execute(pool.get()).await.unwrap();
    let maintenance = astra_services::runtime_maintenance::maintain_runtime_storage(
        &pool,
        None,
        &astra_services::runtime_maintenance::RuntimeMaintenancePolicy {
            batch_limit: 1,
            ..Default::default()
        },
    )
    .await;
    assert_eq!(maintenance.observation_collision_receipts_expired, 1);
    assert!(
        maintenance.cleanup_errors.is_empty(),
        "maintenance errors: {:?}",
        maintenance.cleanup_errors
    );
    let retained: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM observation_identity_collisions WHERE user_id = ?",
    )
    .bind(&other_owner)
    .fetch_one(pool.get())
    .await
    .unwrap();
    assert_eq!(retained, 1, "expiry must preserve unexpired receipts");

    sqlx::query("DELETE FROM observation_identity_collisions WHERE user_id IN (?, ?)")
        .bind(&owner)
        .bind(&other_owner)
        .execute(pool.get())
        .await
        .unwrap();
}
