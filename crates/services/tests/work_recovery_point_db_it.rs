mod common;

use astra_services::work::{
    DatabaseWorkBranchDeletionService, DatabaseWorkRepository, NewWorkRecoveryPoint,
    WorkBranchDeletionRequest, WorkBranchId, WorkBranchRevision, WorkChangeRef,
    WorkConflictResource, WorkId, WorkOwnerId, WorkRecoveryPointQuery, WorkRecoveryPointStatus,
    WorkRepository, WorkRepositoryError, WorkRevision,
};
use astra_turn_types::{
    RECOVERY_POINT_MANIFEST_SCHEMA_VERSION, RecoveryPointEnvironmentRequirementsV1,
    RecoveryPointExecutionBindingV1, RecoveryPointExecutorKindV1, RecoveryPointManifestV1,
    RecoveryPointReasonV1, SessionContextHeadV1, SessionCursorV1, SessionKeyV1,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;

fn id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}

async fn cleanup_owner(pool: &astra_core::SharedPool, owner_id: &str) {
    for (table, owner_column) in [
        ("work_recovery_points", "owner_id"),
        ("work_branch_deletion_operations", "owner_id"),
        ("work_branches", "owner_id"),
        ("works", "owner_id"),
        ("agent_sessions", "user_id"),
    ] {
        let statement = format!("DELETE FROM {table} WHERE {owner_column} = ?");
        sqlx::query(&statement)
            .bind(owner_id)
            .execute(pool.get())
            .await
            .unwrap_or_else(|error| panic!("clean {table}: {error}"));
    }
}

async fn add_non_delivery_branch(
    pool: &astra_core::SharedPool,
    owner_id: &str,
    work_id: &str,
    delivery_branch_id: &str,
    branch_id: &str,
    session_id: &str,
) {
    sqlx::query(
        "INSERT INTO work_branches
         (owner_id, work_id, branch_id, branch_revision, session_id, origin_branch_id,
          fork_cursor, goal_revision_ref, criteria_set_revision_ref, basis_graph_revision,
          current_graph_revision, created_at, updated_at, archived_at)
         SELECT owner_id, work_id, ?, 1, ?, branch_id, ?, goal_revision_ref,
                criteria_set_revision_ref, basis_graph_revision, current_graph_revision,
                NOW(6), NOW(6), NULL
         FROM work_branches
         WHERE owner_id = ? AND work_id = ? AND branch_id = ?",
    )
    .bind(branch_id)
    .bind(session_id)
    .bind(format!("fork-{branch_id}"))
    .bind(owner_id)
    .bind(work_id)
    .bind(delivery_branch_id)
    .execute(pool.get())
    .await
    .expect("add non-delivery branch");
}

fn manifest(
    owner_id: &str,
    work_id: &str,
    branch_id: &str,
    session_id: &str,
) -> RecoveryPointManifestV1 {
    let session_key = SessionKeyV1::owner_session("tenant", owner_id, session_id, "main");
    let session_cursor = SessionCursorV1 {
        schema_version: 1,
        owner_id: owner_id.to_owned(),
        session_id: session_id.to_owned(),
        branch_id: "main".to_owned(),
        completed_turn: 1,
        journal_event_seq: 1,
        conversation_seq: 1,
        canonical_root_hash: "a".repeat(64),
        projection_schema: 1,
        compaction_generation: 0,
        config_version_id: None,
    };
    let mut manifest = RecoveryPointManifestV1 {
        schema_version: RECOVERY_POINT_MANIFEST_SCHEMA_VERSION,
        recovery_point_id: id("recovery-point"),
        owner_id: owner_id.to_owned(),
        work_id: work_id.to_owned(),
        branch_id: branch_id.to_owned(),
        work_revision: 1,
        branch_revision: 1,
        graph_revision: 1,
        goal_revision: 1,
        criteria_set_revision: 1,
        session_key: session_key.clone(),
        session_cursor: session_cursor.clone(),
        context_head: SessionContextHeadV1 {
            schema_version: 1,
            key: session_key.clone(),
            cursor: session_cursor,
            latest_manifest_root: "a".repeat(64),
            total_canonical_bytes: 1,
            total_message_count: 1,
            writer_epoch: 1,
        },
        run: None,
        execution: RecoveryPointExecutionBindingV1 {
            binding_generation: 1,
            binding_state: astra_turn_types::RecoveryPointBindingStateV1::Ready,
            logical_workspace_id: id("workspace"),
            executor_kind: RecoveryPointExecutorKindV1::Server,
            executor_id: id("server"),
            binding_hash: String::new(),
            physical_workspace_id: None,
        },
        workspace: None,
        artifacts: vec![],
        environment: RecoveryPointEnvironmentRequirementsV1::default(),
        reason: RecoveryPointReasonV1::UserRequested,
        created_at: "2026-09-16T00:00:00Z".to_owned(),
    };
    manifest.execution.binding_hash = manifest.execution.content_hash();
    manifest
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn recovery_point_capture_is_preparing_and_owner_scoped() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let other_owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;
    cleanup_owner(&pool, &other_owner_id).await;

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Persist one owner-scoped recovery boundary.",
        ))
        .await
        .expect("create Work");

    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");
    let request = NewWorkRecoveryPoint {
        owner_id: owner.clone(),
        work_id: work.clone(),
        branch_id: branch,
        request_id: WorkChangeRef::parse(id("request")).expect("request"),
        manifest: manifest(&owner_id, &work_id, &branch_id, &session_id),
    };
    let record = repository
        .recovery_points()
        .record_preparing(request.clone())
        .await
        .expect("record recovery capture");
    assert_eq!(record.status, WorkRecoveryPointStatus::Preparing);
    assert!(record.ready_at.is_none());

    // A retry of the exact admitted request must return the same durable row,
    // while reusing the request identity for a different manifest is a typed
    // conflict rather than a second capture.
    let replay = repository
        .recovery_points()
        .record_preparing(request.clone())
        .await
        .expect("replay recovery capture");
    assert_eq!(replay, record);
    let mut changed_request = request.clone();
    changed_request.manifest.created_at = "2026-09-16T00:00:01Z".to_owned();
    assert!(matches!(
        repository
            .recovery_points()
            .record_preparing(changed_request)
            .await,
        Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::RecoveryPointRequest
        })
    ));

    let replay_repository = repository.recovery_points();
    let (left, right) = tokio::join!(
        replay_repository.record_preparing(request.clone()),
        replay_repository.record_preparing(request.clone()),
    );
    let left = left.expect("concurrent left replay");
    let right = right.expect("concurrent right replay");
    assert_eq!(left, record);
    assert_eq!(right, record);
    let row_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM work_recovery_points
         WHERE owner_id = ? AND work_id = ? AND recovery_point_id = ?",
    )
    .bind(&owner_id)
    .bind(&work_id)
    .bind(&record.recovery_point_id)
    .fetch_one(pool.get())
    .await
    .expect("count replayed recovery rows");
    assert_eq!(row_count, 1);

    let loaded = repository
        .recovery_points()
        .load(&owner, &work, &record.recovery_point_id)
        .await
        .expect("load owner recovery point")
        .expect("recovery point exists");
    assert_eq!(loaded.recovery_point_id, record.recovery_point_id);

    let other_owner = WorkOwnerId::parse(&other_owner_id).expect("other owner");
    let other_work_id = id("work");
    let other_session_id = id("session");
    repository
        .create_genesis(common::work_genesis(
            &other_owner_id,
            &other_work_id,
            &branch_id,
            &other_session_id,
            &id("intent"),
            "A second owner may use the same opaque recovery-point identifier in another Work.",
        ))
        .await
        .expect("create second owner Work");
    let mut other_manifest = manifest(
        &other_owner_id,
        &other_work_id,
        &branch_id,
        &other_session_id,
    );
    other_manifest.recovery_point_id = record.recovery_point_id.clone();
    other_manifest.execution.binding_hash = other_manifest.execution.content_hash();
    let other_record = repository
        .recovery_points()
        .record_preparing(NewWorkRecoveryPoint {
            owner_id: other_owner.clone(),
            work_id: WorkId::parse(&other_work_id).expect("other work"),
            branch_id: WorkBranchId::parse(&branch_id).expect("other branch"),
            request_id: WorkChangeRef::parse(id("request")).expect("other request"),
            manifest: other_manifest,
        })
        .await
        .expect("record second owner recovery capture");
    assert_eq!(other_record.recovery_point_id, record.recovery_point_id);
    let other_loaded = repository
        .recovery_points()
        .load(
            &other_owner,
            &WorkId::parse(&other_work_id).expect("other work"),
            &record.recovery_point_id,
        )
        .await
        .expect("load second owner recovery point")
        .expect("second owner recovery point exists");
    assert_eq!(other_loaded.owner_id, other_owner);
    assert_eq!(other_loaded.branch_id.as_str(), branch_id);
    let unauthorized_owner = WorkOwnerId::parse(id("owner")).expect("unauthorized owner");
    assert!(
        repository
            .recovery_points()
            .load(&unauthorized_owner, &work, &record.recovery_point_id)
            .await
            .expect("load unauthorized recovery point")
            .is_none()
    );
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn recovery_point_capture_rejects_a_criterion_set_with_a_missing_member() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &branch_id,
            &session_id,
            &id("intent"),
            "Reject a recovery boundary when its criterion member disappeared.",
        ))
        .await
        .expect("create Work");

    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");
    let request = NewWorkRecoveryPoint {
        owner_id: owner.clone(),
        work_id: work.clone(),
        branch_id: branch.clone(),
        request_id: WorkChangeRef::parse(id("request")).expect("request"),
        manifest: manifest(&owner_id, &work_id, &branch_id, &session_id),
    };
    let record = repository
        .recovery_points()
        .record_preparing(request)
        .await
        .expect("record recovery capture");

    // Keep the set envelope internally self-consistent, but point it at a
    // revision that is absent. Capture must validate the complete immutable
    // member set in the same transaction; checking only revision/count/hash
    // would incorrectly publish this boundary as usable.
    let manifest_json =
        r#"{"schema_version":1,"members":[{"criterion_id":"missing-criterion","revision":1}]}"#;
    let manifest_hash = format!("sha256:{:x}", Sha256::digest(manifest_json.as_bytes()));
    sqlx::query(
        "UPDATE work_criterion_sets
         SET member_manifest_json = ?, member_manifest_hash = ?, member_count = 1
         WHERE owner_id = ? AND work_id = ? AND revision = 1",
    )
    .bind(manifest_json)
    .bind(manifest_hash)
    .bind(&owner_id)
    .bind(&work_id)
    .execute(pool.get())
    .await
    .expect("corrupt criterion-set member manifest");

    assert!(matches!(
        repository
            .recovery_points()
            .mark_captured(&owner, &work, &branch, &record.recovery_point_id)
            .await,
        Err(WorkRepositoryError::Corrupt { entity, .. }) if entity == "criterion definition"
    ));
    let loaded = repository
        .recovery_points()
        .load(&owner, &work, &record.recovery_point_id)
        .await
        .expect("load rejected recovery point")
        .expect("recovery point remains durable");
    assert_eq!(loaded.status, WorkRecoveryPointStatus::Preparing);
}

#[tokio::test]
#[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
async fn branch_deletion_removes_branch_recovery_points() {
    let pool = common::setup_pool().await;
    let repository = DatabaseWorkRepository::new(pool.clone());
    let deletion = DatabaseWorkBranchDeletionService::new(pool.clone());
    let owner_id = id("owner");
    let work_id = id("work");
    let delivery_branch_id = id("delivery");
    let delivery_session_id = id("session");
    let branch_id = id("branch");
    let session_id = id("session");
    cleanup_owner(&pool, &owner_id).await;

    repository
        .create_genesis(common::work_genesis(
            &owner_id,
            &work_id,
            &delivery_branch_id,
            &delivery_session_id,
            &id("intent"),
            "Delete branch-owned recovery state with the branch.",
        ))
        .await
        .expect("create Work");
    add_non_delivery_branch(
        &pool,
        &owner_id,
        &work_id,
        &delivery_branch_id,
        &branch_id,
        &session_id,
    )
    .await;
    // Branch deletion fences the canonical Session before it removes the
    // branch-owned recovery point. Keep this fixture on that production
    // lifecycle instead of creating an orphaned work_branches row.
    sqlx::query(
        "INSERT INTO agent_sessions
         (user_id, session_id, status, created_at, updated_at, last_active_at)
         VALUES (?, ?, 'active', NOW(6), NOW(6), NOW(6))",
    )
    .bind(&owner_id)
    .bind(&session_id)
    .execute(pool.get())
    .await
    .expect("create branch session");

    let owner = WorkOwnerId::parse(&owner_id).expect("owner");
    let work = WorkId::parse(&work_id).expect("work");
    let branch = WorkBranchId::parse(&branch_id).expect("branch");
    let record = repository
        .recovery_points()
        .record_preparing(NewWorkRecoveryPoint {
            owner_id: owner.clone(),
            work_id: work.clone(),
            branch_id: branch.clone(),
            request_id: WorkChangeRef::parse(id("request")).expect("request"),
            manifest: manifest(&owner_id, &work_id, &branch_id, &session_id),
        })
        .await
        .expect("record recovery point");

    let admission = deletion
        .admit(&WorkBranchDeletionRequest {
            request_id: id("delete"),
            owner_id: owner.clone(),
            work_id: work.clone(),
            branch_id: branch.clone(),
            expected_work_revision: WorkRevision::new(1).expect("work revision"),
            expected_branch_revision: WorkBranchRevision::new(1).expect("branch revision"),
        })
        .await
        .expect("admit branch deletion");
    let token = deletion
        .claim_execution(&owner, &work, &branch, &admission.operation.operation_id)
        .await
        .expect("claim deletion executor")
        .expect("claim token");
    deletion
        .fence_session(
            &owner,
            &work,
            &branch,
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("fence branch session");
    deletion
        .cleanup_session(
            &owner,
            &work,
            &branch,
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("clean branch session");
    deletion
        .reconcile_lineage(
            &owner,
            &work,
            &branch,
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("reconcile branch lineage");
    deletion
        .complete_branch_cleanup(
            &owner,
            &work,
            &branch,
            &admission.operation.operation_id,
            &token,
        )
        .await
        .expect("delete branch");

    assert!(
        repository
            .recovery_points()
            .load(&owner, &work, &record.recovery_point_id)
            .await
            .expect("load deleted recovery point")
            .is_none()
    );
    assert!(
        repository
            .recovery_points()
            .list(WorkRecoveryPointQuery::new(owner, work).branch(branch))
            .await
            .expect("list deleted branch recovery points")
            .is_empty()
    );
}
