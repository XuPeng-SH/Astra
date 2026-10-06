//! MySQL / MatrixOne integration tests for [`astra_services::team_persistence`].
//!
//! ```text
//! ASTRA_TEST_DB_IT=1 cargo test -p astra-services team_persistence_integration -- --ignored
//! ```
//!
//! Uses `MATRIXONE_*` env vars (after `dotenvy`) with the same defaults as local dev.
//! Effective database name includes optional `ASTRA_DATABASE_PREFIX` (same as `AppSettings`).

use astra_core::SharedPool;
use astra_services::team_persistence::{
    CreateTeam, MatrixOneTeamStore, TeamDefinition, TeamMemberDef, TeamPersistenceService,
    TeamSnapshotRecord, TeamWriteError, UpdateTeam,
};
use serial_test::serial;
use std::collections::HashMap;
use uuid::Uuid;

mod common;

async fn setup_pool() -> SharedPool {
    common::setup_pool().await
}

async fn cleanup_team(pool: &sqlx::Pool<sqlx::MySql>, team_id: &str) {
    let _ = sqlx::query("DELETE FROM team_definitions WHERE team_id = ?")
        .bind(team_id)
        .execute(pool)
        .await;
}

async fn cleanup_snapshot(pool: &sqlx::Pool<sqlx::MySql>, snapshot_id: &str) {
    let _ = sqlx::query("DELETE FROM team_snapshots WHERE snapshot_id = ?")
        .bind(snapshot_id)
        .execute(pool)
        .await;
}

fn test_team(suffix: &str) -> TeamDefinition {
    let team_id = format!("it-team-{suffix}-{}", Uuid::new_v4());
    let user_id = format!("it-user-{suffix}-{}", Uuid::new_v4());
    TeamDefinition {
        team_id,
        user_id,
        name: format!("it-{suffix}"),
        description: format!("Integration test team: {suffix}"),
        members: vec![
            TeamMemberDef {
                role: "coder".into(),
                agent_id: "coder".into(),
                system_prompt: Some("Implement code".into()),
                skills: vec!["review-changes".into()],
                model_selection: None,
                mcp_servers: vec![],
                can_delegate: false,
                max_delegation_depth: 0,
                ..Default::default()
            },
            TeamMemberDef {
                role: "tester".into(),
                agent_id: "custom-tester".into(),
                system_prompt: None,
                skills: vec![],
                model_selection: Some(astra_turn_types::ModelSelection {
                    offering_id: "offer-fast".into(),
                }),
                mcp_servers: vec![],
                can_delegate: true,
                max_delegation_depth: 2,
                ..Default::default()
            },
        ],
        context: HashMap::from([("repo".into(), "test-repo".into())]),
        revision: 1,
    }
}

fn create_input(team: &TeamDefinition) -> CreateTeam {
    CreateTeam {
        team_id: team.team_id.clone(),
        name: team.name.clone(),
        description: team.description.clone(),
        members: team.members.clone(),
        context: team.context.clone(),
    }
}

// ─── CRUD Tests ─────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn team_crud_roundtrip() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let store = MatrixOneTeamStore::new(pool.clone());
    let team = test_team("crud");
    cleanup_team(&pool, &team.team_id).await;

    // Save
    store
        .create_team(&team.user_id, &create_input(&team))
        .await
        .expect("save_team");

    // Load by user_id + name
    let loaded = store
        .load_team(&team.user_id, &team.name)
        .await
        .expect("load_team")
        .expect("team should exist");
    assert_eq!(loaded.team_id, team.team_id);
    assert_eq!(loaded.members.len(), 2);
    assert_eq!(loaded.members[0].role, "coder");
    assert_eq!(loaded.members[1].agent_id, "custom-tester");

    // List
    let list = store.list_teams(&team.user_id).await.expect("list_teams");
    assert!(list.iter().any(|t| t.team_id == team.team_id));

    let snapshot = TeamSnapshotRecord {
        snapshot_id: Uuid::new_v4().to_string(),
        team_id: team.team_id.clone(),
        team_name: team.name.clone(),
        user_id: team.user_id.clone(),
        label: "before rename".to_string(),
        git_commit: None,
        session_id: None,
        team_definition_json: Some(serde_json::to_string(&team).unwrap()),
        created_at: String::new(),
    };
    store.save_snapshot(&snapshot).await.unwrap();

    // CAS update, with no readback needed to obtain accepted contents
    let mut updated = team.clone();
    updated.description = "Updated description".into();
    let accepted = store
        .update_team(
            &updated.user_id,
            &updated.team_id,
            &UpdateTeam::from(&updated),
        )
        .await
        .expect("CAS update");
    assert_eq!(accepted.revision, 2);
    let reloaded = store
        .load_team(&team.user_id, &team.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reloaded.description, "Updated description");
    assert_eq!(reloaded.team_id, team.team_id);
    assert_eq!(reloaded.revision, 2);

    // One revision admits exactly one of two writers, including a rename.
    let mut next = UpdateTeam::from(&accepted);
    next.name = "renamed-crud".to_string();
    next.members[0].role = "renamed-coder".to_string();
    let (a, b) = tokio::join!(
        store.update_team(&team.user_id, &team.team_id, &next),
        store.update_team(&team.user_id, &team.team_id, &next),
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let (winner, loser) = if a.is_ok() { (a, b) } else { (b, a) };
    assert_eq!(loser.unwrap_err(), TeamWriteError::ConflictOrMissing);
    let winner = winner.unwrap();
    assert_eq!(winner.revision, 3);
    assert_eq!(winner.team_id, team.team_id);
    assert_eq!(winner.members[0].agent_id, team.members[0].agent_id);
    assert!(
        store
            .load_team(&team.user_id, &team.name)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store
            .load_team_by_id(&team.user_id, &team.team_id)
            .await
            .unwrap(),
        Some(winner.clone())
    );
    for (owner, id) in [
        ("other-owner", team.team_id.as_str()),
        (team.user_id.as_str(), "missing-team"),
    ] {
        assert_eq!(
            store
                .update_team(owner, id, &UpdateTeam::from(&winner))
                .await,
            Err(TeamWriteError::ConflictOrMissing)
        );
    }
    assert!(
        !store
            .delete_team("other-owner", &team.team_id)
            .await
            .unwrap()
    );
    let snapshots = store
        .list_snapshots_page(&team.team_id, &team.user_id, 1, None)
        .await
        .unwrap();
    assert_eq!(snapshots.snapshots.len(), 1);
    assert_eq!(snapshots.snapshots[0].team_id, winner.team_id);
    assert_eq!(snapshots.snapshots[0].team_name, team.name);
    assert!(!snapshots.snapshots[0].created_at.is_empty());
    assert!(
        store
            .find_snapshot(&snapshot.snapshot_id, "other-owner")
            .await
            .unwrap()
            .is_none()
    );

    // Delete
    assert!(
        store
            .delete_team(&team.user_id, &team.team_id)
            .await
            .unwrap()
    );
    assert!(
        store
            .load_team(&team.user_id, &team.name)
            .await
            .unwrap()
            .is_none()
    );

    cleanup_team(&pool, &team.team_id).await;
    let mut recreated = team.clone();
    recreated.team_id = Uuid::new_v4().to_string();
    store
        .create_team(&recreated.user_id, &create_input(&recreated))
        .await
        .unwrap();
    assert!(
        store
            .list_snapshots(&recreated.team_id, &recreated.user_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    cleanup_team(&pool, &recreated.team_id).await;
    cleanup_snapshot(&pool, &snapshot.snapshot_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn create_team_rejects_primary_key_collision_with_different_logical_team() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let store = MatrixOneTeamStore::new(pool.clone());

    let original = test_team("pk-conflict-a");
    cleanup_team(&pool, &original.team_id).await;
    store
        .create_team(&original.user_id, &create_input(&original))
        .await
        .expect("save original team");

    let mut conflicting = test_team("pk-conflict-b");
    conflicting.team_id = original.team_id.clone();

    let err = store
        .create_team(&conflicting.user_id, &create_input(&conflicting))
        .await
        .expect_err("primary-key collision must not overwrite another logical team");
    assert_eq!(err, TeamWriteError::ConflictOrMissing);
    let mut same_name = original.clone();
    same_name.team_id = Uuid::new_v4().to_string();
    assert_eq!(
        store
            .create_team(&same_name.user_id, &create_input(&same_name))
            .await,
        Err(TeamWriteError::ConflictOrMissing)
    );

    let reloaded = store
        .load_team(&original.user_id, &original.name)
        .await
        .expect("load original after conflict")
        .expect("original team should remain");
    assert_eq!(reloaded.team_id, original.team_id);
    assert_eq!(reloaded.user_id, original.user_id);
    assert_eq!(reloaded.name, original.name);
    assert_eq!(reloaded.description, original.description);

    assert!(
        store
            .load_team(&conflicting.user_id, &conflicting.name)
            .await
            .expect("load conflicting team")
            .is_none(),
        "conflicting logical team should not be created"
    );

    cleanup_team(&pool, &original.team_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn load_team_rejects_corrupt_context_json_on_live_matrixone() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let store = MatrixOneTeamStore::new(pool.clone());
    let team = test_team("badctx");
    cleanup_team(&pool, &team.team_id).await;

    store
        .create_team(&team.user_id, &create_input(&team))
        .await
        .expect("save_team");
    sqlx::query("UPDATE team_definitions SET context_json = 'not-json' WHERE team_id = ?")
        .bind(&team.team_id)
        .execute(&pool)
        .await
        .expect("corrupt context_json");

    let err = store
        .load_team(&team.user_id, &team.name)
        .await
        .expect_err("corrupt persisted context_json must fail loudly");
    assert!(
        err.contains("team_definitions row decode column `context_json`"),
        "unexpected error: {err}"
    );

    cleanup_team(&pool, &team.team_id).await;
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn list_snapshots_rejects_null_required_label_on_live_matrixone() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let store = MatrixOneTeamStore::new(pool.clone());
    let snapshot_id = format!("it-snap-{}", Uuid::new_v4());
    let team_name = format!("it-snap-team-{}", Uuid::new_v4().simple());
    let user_id = format!("it-snap-user-{}", Uuid::new_v4());
    cleanup_snapshot(&pool, &snapshot_id).await;

    store
        .save_snapshot(&TeamSnapshotRecord {
            snapshot_id: snapshot_id.clone(),
            team_id: team_name.clone(),
            team_name: team_name.clone(),
            user_id: user_id.clone(),
            label: "before refactor".to_string(),
            git_commit: None,
            session_id: None,
            team_definition_json: None,
            created_at: String::new(),
        })
        .await
        .expect("save snapshot");

    sqlx::query("UPDATE team_snapshots SET label = NULL WHERE snapshot_id = ?")
        .bind(&snapshot_id)
        .execute(&pool)
        .await
        .expect("corrupt snapshot label");

    let err = store
        .list_snapshots(&team_name, &user_id, 10)
        .await
        .expect_err("null persisted snapshot label must fail loudly");
    assert!(
        err.contains("team_snapshots row decode column `label`"),
        "unexpected error: {err}"
    );

    cleanup_snapshot(&pool, &snapshot_id).await;
}

// ─── Builtins Seeding ───────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn ensure_builtins_idempotent() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let user_id = format!("it-builtins-{}", Uuid::new_v4());
    let store = MatrixOneTeamStore::new(pool.clone());

    // Concurrent first requests must converge without overwriting or errors.
    let (first, concurrent) = tokio::join!(
        store.ensure_builtins(&user_id),
        store.ensure_builtins(&user_id)
    );
    first.expect("ensure_builtins");
    concurrent.expect("concurrent ensure_builtins");
    let list1 = store.list_teams(&user_id).await.unwrap();
    assert_eq!(list1.len(), 3, "should have review, research, dev once");

    // Second call is idempotent
    store
        .ensure_builtins(&user_id)
        .await
        .expect("ensure_builtins (2)");
    let list2 = store.list_teams(&user_id).await.unwrap();
    assert_eq!(list1.len(), list2.len());

    // A new store models restart: initialization must not recreate or reject a renamed builtin.
    let dev = list2.iter().find(|team| team.name == "dev").unwrap();
    let mut update = UpdateTeam::from(dev);
    update.name = "my-dev".to_string();
    let accepted = store
        .update_team(&user_id, &dev.team_id, &update)
        .await
        .unwrap();
    let restarted = MatrixOneTeamStore::new(pool.clone());
    restarted.ensure_builtins(&user_id).await.unwrap();
    assert_eq!(
        restarted
            .load_team_by_id(&user_id, &dev.team_id)
            .await
            .unwrap(),
        Some(accepted)
    );
    assert!(
        restarted
            .load_team(&user_id, "dev")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(restarted.list_teams(&user_id).await.unwrap().len(), 3);

    // Cleanup
    for t in &list2 {
        cleanup_team(&pool, &t.team_id).await;
    }
}

#[tokio::test]
#[ignore = "ASTRA_TEST_DB_IT=1 and live MatrixOne"]
#[serial]
async fn ensure_builtins_preserves_existing_owner_customization() {
    let shared = setup_pool().await;
    let pool = shared.get().clone();
    let store = MatrixOneTeamStore::new(pool.clone());
    let user_id = format!("it-builtins-custom-{}", Uuid::new_v4());
    let mut customized = test_team("builtin-custom");
    customized.user_id = user_id.clone();
    customized.name = "review".to_string();
    customized.description = "owner-defined review workflow".to_string();
    cleanup_team(&pool, &customized.team_id).await;
    store
        .create_team(&customized.user_id, &create_input(&customized))
        .await
        .expect("save owner customization");

    store
        .ensure_builtins(&user_id)
        .await
        .expect("materialize missing builtins");

    let review = store
        .load_team(&user_id, "review")
        .await
        .expect("load customized review")
        .expect("customized review remains present");
    assert_eq!(review.team_id, customized.team_id);
    assert_eq!(review.description, "owner-defined review workflow");
    let teams = store.list_teams(&user_id).await.expect("list teams");
    assert_eq!(teams.len(), 3);

    for team in teams {
        cleanup_team(&pool, &team.team_id).await;
    }
}
