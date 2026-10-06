//! Realistic team HTTP API integration tests — complex lifecycle scenarios.
//!
//! Exercises the full team management surface through HTTP requests:
//! CRUD and draft persistence, validation edge-cases,
//! concurrent Team revision updates, immutable identities, and large-team handling.
//!
//! Uses Tower oneshot (no network), InMemoryTeamStore (no DB required).

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Router,
    body::{self, Body},
    http::{HeaderMap, Request, StatusCode},
};
use serde_json::{Value, json};
use tower::util::ServiceExt;

use astra_runtime::{
    AppState, AuthLoginRequestData, AuthRefreshRequestData, AuthRegisterRequestData, AuthService,
    AuthTokenRecord, AuthUserRecord, ErrorResponse, HealthChecker, ServiceInfo, build_app,
};
use astra_services::team_persistence::InMemoryTeamStore;

// ─── Stubs ──────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct StubHealth;

#[async_trait]
impl HealthChecker for StubHealth {
    async fn database_healthy(&self) -> bool {
        true
    }
}

struct StubAuth;

#[async_trait]
impl AuthService for StubAuth {
    async fn register(
        &self,
        _r: AuthRegisterRequestData,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn login(
        &self,
        _r: AuthLoginRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn refresh(
        &self,
        _r: AuthRefreshRequestData,
    ) -> Result<AuthTokenRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn logout(
        &self,
        _r: AuthRefreshRequestData,
    ) -> Result<(), (StatusCode, axum::Json<ErrorResponse>)> {
        unreachable!()
    }
    async fn current_user(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthUserRecord, (StatusCode, axum::Json<ErrorResponse>)> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some(h) if h.starts_with("Bearer ") => {
                let user_id = h.trim_start_matches("Bearer ");
                Ok(AuthUserRecord {
                    user_id: user_id.to_string(),
                    username: format!("user-{user_id}"),
                    email: format!("{user_id}@test.local"),
                    display_name: None,
                })
            }
            _ => Err((
                StatusCode::UNAUTHORIZED,
                axum::Json(ErrorResponse::new("Not authenticated")),
            )),
        }
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn build_test_app() -> Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth))
        .with_team_store(Arc::new(InMemoryTeamStore::new()));
    build_app(state)
}

fn build_test_app_without_team_store() -> Router {
    let state = AppState::new(ServiceInfo::default(), Arc::new(StubHealth))
        .with_auth_service(Arc::new(StubAuth));
    build_app(state)
}

fn auth(user: &str) -> Vec<(&str, String)> {
    vec![("authorization", format!("Bearer {user}"))]
}

async fn get(app: Router, path: &str, user: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("GET").uri(path);
    for (k, v) in auth(user) {
        builder = builder.header(k, v);
    }
    let response = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn post(app: Router, path: &str, user: &str, payload: Value) -> (StatusCode, Value) {
    write_json(app, "POST", path, user, payload).await
}

async fn put(app: Router, path: &str, user: &str, payload: Value) -> (StatusCode, Value) {
    write_json(app, "PUT", path, user, payload).await
}

fn update_payload(mut payload: Value, expected_revision: u64) -> Value {
    payload.as_object_mut().unwrap().remove("team_id");
    payload["expected_revision"] = json!(expected_revision);
    payload
}

async fn write_json(
    app: Router,
    method: &str,
    path: &str,
    user: &str,
    payload: Value,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    for (k, v) in auth(user) {
        builder = builder.header(k, v);
    }
    let response = app
        .oneshot(builder.body(Body::from(payload.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn post_raw(app: Router, path: &str, user: &str, body: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    for (k, v) in auth(user) {
        builder = builder.header(k, v);
    }
    let response = app
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn delete(app: Router, path: &str, user: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("DELETE").uri(path);
    for (k, v) in auth(user) {
        builder = builder.header(k, v);
    }
    let response = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

// ─── Payloads ───────────────────────────────────────────────────────────────

fn dev_team_payload() -> Value {
    json!({
        "team_id": "id-dev-cycle",
        "name": "dev-cycle",
        "description": "Full dev cycle: plan, implement, test, review",
        "members": [
            {
                "role": "planner",
                "agent_id": "member-planner",
                "system_prompt": "Decompose the task into subtasks with acceptance criteria.",
                "skills": ["plan-decompose"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "implementer",
                "agent_id": "fast-coder",
                "system_prompt": "Implement code changes following the plan.",
                "skills": ["edit", "shell"],
                "model_selection": { "offering_id": "offer-dev-implementer" },
                "mcp_servers": ["filesystem"],
                "can_delegate": true,
                "max_delegation_depth": 2
            },
            {
                "role": "tester",
                "agent_id": "member-tester",
                "system_prompt": "Write and run tests, verifying acceptance criteria.",
                "skills": ["verify-task", "shell"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "reviewer",
                "agent_id": "member-reviewer",
                "system_prompt": "Review code changes for correctness, style, and security.",
                "skills": ["review-changes"],
                "model_selection": { "offering_id": "offer-dev-reviewer" },
                "mcp_servers": ["github"],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "context": {
            "repo": "mo-dev-agent",
            "language": "rust",
            "test_cmd": "cargo test --workspace"
        },
    })
}

fn ordered_review_payload() -> Value {
    json!({
        "team_id": "id-ordered-review",
        "name": "ordered-review",
        "description": "Produce an output and review it in order",
        "members": [
            {
                "role": "producer",
                "agent_id": "member-producer",
                "system_prompt": "Write high-quality code.",
                "skills": ["edit", "shell"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "reviewer",
                "agent_id": "member-reviewer",
                "system_prompt": "Find bugs, security issues, and performance problems.",
                "skills": ["review-changes"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
    })
}

fn fanout_research_payload() -> Value {
    json!({
        "team_id": "id-parallel-research",
        "name": "parallel-research",
        "description": "Fan-out: 3 researchers investigate in parallel, results merged",
        "members": [
            {
                "role": "researcher-api",
                "agent_id": "member-researcher-api",
                "system_prompt": "Research REST API best practices.",
                "skills": ["web-search"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "researcher-perf",
                "agent_id": "member-researcher-perf",
                "system_prompt": "Research performance optimization techniques.",
                "skills": ["web-search"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "researcher-sec",
                "agent_id": "member-researcher-sec",
                "system_prompt": "Research security hardening strategies.",
                "skills": ["web-search"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
    })
}

fn sequential_migration_payload() -> Value {
    json!({
        "team_id": "id-db-migration",
        "name": "db-migration",
        "description": "Sequential: analyze schema, write migration, test, deploy",
        "members": [
            {
                "role": "schema-analyst",
                "agent_id": "member-schema-analyst",
                "system_prompt": "Analyze current schema and propose migration plan.",
                "skills": [],
                "mcp_servers": ["database"],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "migration-writer",
                "agent_id": "member-migration-writer",
                "system_prompt": "Write backward-compatible SQL migration.",
                "skills": ["edit"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ]
    })
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 1: Full lifecycle — create 4 teams, list, get, update, delete
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_full_team_lifecycle() {
    let app = build_test_app();
    let user = "lifecycle-user";

    // ── Create 4 teams with distinct member rosters ──
    let teams = [
        ("dev-cycle", dev_team_payload()),
        ("ordered-review", ordered_review_payload()),
        ("parallel-research", fanout_research_payload()),
        ("db-migration", sequential_migration_payload()),
    ];

    for (name, payload) in &teams {
        let (status, body) = post(app.clone(), "/teams", user, payload.clone()).await;
        assert_eq!(status, StatusCode::OK, "create {name} failed: {body}");
        assert_eq!(body["name"], *name);
        assert!(!body["team_id"].as_str().unwrap().is_empty());
    }

    // ── List: owner-scoped builtins plus all 4 custom teams ──
    let (status, body) = get(app.clone(), "/teams", user).await;
    assert_eq!(status, StatusCode::OK);
    let team_list = body["teams"].as_array().unwrap();
    assert_eq!(team_list.len(), 7);
    let names: Vec<&str> = team_list
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"dev-cycle"));
    assert!(names.contains(&"ordered-review"));
    assert!(names.contains(&"parallel-research"));
    assert!(names.contains(&"db-migration"));
    assert!(names.contains(&"review"));
    assert!(names.contains(&"research"));
    assert!(names.contains(&"dev"));

    // ── Get detail for dev-cycle: verify members ──
    let (status, body) = get(app.clone(), "/teams/name/dev-cycle", user).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"], "dev-cycle");
    assert_eq!(body["members"].as_array().unwrap().len(), 4);

    // ── Get detail for ordered-review ──
    let (status, body) = get(app.clone(), "/teams/name/ordered-review", user).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["members"].as_array().unwrap().len(), 2);

    // ── Update dev-cycle: change description ──
    let mut updated = dev_team_payload();
    updated["description"] = json!("Updated: full dev cycle v2");
    let (status, body) = put(
        app.clone(),
        "/teams/id-dev-cycle",
        user,
        update_payload(updated, 1),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["description"], "Updated: full dev cycle v2");

    // Re-fetch to confirm persistence
    let (status, body) = get(app.clone(), "/teams/name/dev-cycle", user).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["description"], "Updated: full dev cycle v2");

    // ── Delete db-migration ──
    let (status, body) = delete(app.clone(), "/teams/id-db-migration", user).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["deleted"].as_bool().unwrap());

    // Confirm gone
    let (status, _) = get(app.clone(), "/teams/name/db-migration", user).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // List should now have 3 custom teams plus the 3 owner-scoped builtins.
    let (status, body) = get(app.clone(), "/teams", user).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["teams"].as_array().unwrap().len(), 6);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 2: Multi-user isolation — teams are scoped per user
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_multi_user_isolation() {
    let app = build_test_app();

    // User A creates a team
    let (s, _) = post(app.clone(), "/teams", "alice", dev_team_payload()).await;
    assert_eq!(s, StatusCode::OK);

    // User B creates a team with the same name
    let mut bob = dev_team_payload();
    bob["team_id"] = json!("bob-dev-cycle");
    let (s, _) = post(app.clone(), "/teams", "bob", bob).await;
    assert_eq!(s, StatusCode::OK);

    // Each user sees only their own
    let (_, body_a) = get(app.clone(), "/teams", "alice").await;
    let (_, body_b) = get(app.clone(), "/teams", "bob").await;
    assert_eq!(body_a["teams"].as_array().unwrap().len(), 4);
    assert_eq!(body_b["teams"].as_array().unwrap().len(), 4);

    // Different team_ids
    let id_a = body_a["teams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|team| team["name"] == "dev-cycle")
        .and_then(|team| team["team_id"].as_str())
        .unwrap();
    let id_b = body_b["teams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|team| team["name"] == "dev-cycle")
        .and_then(|team| team["team_id"].as_str())
        .unwrap();
    assert_ne!(id_a, id_b);

    // User A cannot see user B's team by name (scoped)
    let (s, _) = get(app.clone(), "/teams/name/dev-cycle", "charlie").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn scenario_fresh_owner_lazily_receives_isolated_builtin_teams() {
    let app = build_test_app();

    let (status, alice) = get(app.clone(), "/teams", "alice-new").await;
    assert_eq!(status, StatusCode::OK);
    let alice_teams = alice["teams"].as_array().unwrap();
    assert_eq!(alice_teams.len(), 3);
    assert_eq!(
        alice_teams
            .iter()
            .map(|team| team["name"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>(),
        std::collections::BTreeSet::from(["dev", "research", "review"])
    );

    let (status, review) = get(app.clone(), "/teams/name/review", "alice-new").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(review["user_id"], "alice-new");

    let (status, bob_review) = get(app, "/teams/name/review", "bob-new").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bob_review["user_id"], "bob-new");
    assert_ne!(review["team_id"], bob_review["team_id"]);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 3: Validation edge cases
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_draft_persistence_and_validation() {
    let app = build_test_app();
    let user = "validator";

    // Empty rosters are persisted drafts.
    let (status, body) = post(
        app.clone(),
        "/teams",
        user,
        json!({
            "team_id": "id-empty-team",
            "name": "empty-team",
            "description": "no members",
            "members": []
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "empty roster draft: {body}");
    assert_eq!(body["members"], json!([]));
    let (status, saved) = get(app.clone(), "/teams/name/empty-team", user).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved, body);

    // Duplicate roles
    let (status, body) = post(
        app.clone(),
        "/teams",
        user,
        json!({
            "team_id": "id-dup-roles",
            "name": "dup-roles",
            "description": "duplicate role names",
            "members": [
                { "role": "coder", "agent_id": "member-coder", "skills": [], "mcp_servers": [] },
                { "role": "coder", "agent_id": "second-coder", "skills": [], "mcp_servers": [] }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "duplicate roles: {body}");
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 4: One writer wins each revision; names cannot redirect an ID write.
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_cas_preserves_identity_and_rejects_stale_or_foreign_writes() {
    let app = build_test_app();
    let user = "cas-user";
    let original = sequential_migration_payload();
    let (status, body1) = post(app.clone(), "/teams", user, original.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body1["revision"], 1);
    assert!(body1.get("created_at").is_none());
    assert!(body1.get("updated_at").is_none());
    let team_id = body1["team_id"].as_str().unwrap();
    let path = format!("/teams/{team_id}");
    let (status, conflict) = post(app.clone(), "/teams", user, original.clone()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(conflict["error_code"], "team_conflict_or_missing");

    let mut updated = update_payload(original, 1);
    updated["name"] = json!("renamed");
    updated["members"][0]["role"] = json!("renamed-role");
    let (a, b) = tokio::join!(
        put(app.clone(), &path, user, updated.clone()),
        put(app.clone(), &path, user, updated.clone()),
    );
    let (winner, loser) = if a.0 == StatusCode::OK {
        (a, b)
    } else {
        (b, a)
    };
    assert_eq!(winner.0, StatusCode::OK);
    assert_eq!(loser.0, StatusCode::CONFLICT);
    assert_eq!(winner.1["team_id"], body1["team_id"]);
    assert_eq!(winner.1["revision"], 2);
    assert_eq!(
        winner.1["members"][0]["agent_id"],
        body1["members"][0]["agent_id"]
    );
    assert_eq!(get(app.clone(), &path, user).await.1, winner.1);
    assert_eq!(
        get(app.clone(), "/teams/name/db-migration", user).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(app.clone(), "/teams/renamed", user).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(app.clone(), "/teams/name/renamed", user).await.0,
        StatusCode::OK
    );
    assert_eq!(
        get(app.clone(), &format!("/teams/name/{team_id}"), user)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    for (owner, target) in [
        (user, path.as_str()),
        ("other-owner", path.as_str()),
        (user, "/teams/renamed"),
    ] {
        let (status, body) = put(app.clone(), target, owner, updated.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error_code"], "team_conflict_or_missing");
    }
    assert_eq!(
        delete(app.clone(), &path, "other-owner").await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get(app, &path, user).await.1, winner.1);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 6: Delete non-existent → 404
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_delete_nonexistent_returns_404() {
    let app = build_test_app();
    let (status, _) = delete(app, "/teams/ghost-team", "anyone").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 7: No auth → 401
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_no_auth_returns_401() {
    let app = build_test_app();
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/teams")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 8: Complex team with all fields populated — round-trip fidelity
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_complex_team_full_roundtrip() {
    let app = build_test_app();
    let user = "complex-user";

    let payload = json!({
        "team_id": "id-mega-team",
        "name": "mega-team",
        "description": "Complex team exercising every field",
        "members": [
            {
                "role": "lead",
                "agent_id": "agent-lead-001",
                "system_prompt": "You are the tech lead. Coordinate and review.",
                "skills": ["review-changes", "plan-decompose"],
                "model_selection": { "offering_id": "offer-mega-lead" },
                "mcp_servers": ["github", "jira"],
                "can_delegate": true,
                "max_delegation_depth": 3
            },
            {
                "role": "backend",
                "agent_id": "member-backend",
                "system_prompt": "Implement backend features in Rust.",
                "skills": ["edit", "shell", "review-changes"],
                "model_selection": { "offering_id": "offer-mega-backend" },
                "mcp_servers": ["filesystem", "database"],
                "can_delegate": true,
                "max_delegation_depth": 1
            },
            {
                "role": "frontend",
                "agent_id": "agent-frontend-react",
                "system_prompt": "Implement React components with TypeScript.",
                "skills": ["edit", "shell"],
                "mcp_servers": ["filesystem", "browser"],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "devops",
                "agent_id": "member-devops",
                "system_prompt": "Handle CI/CD, Docker, and deployment.",
                "skills": ["shell"],
                "model_selection": { "offering_id": "offer-mega-devops" },
                "mcp_servers": ["kubernetes", "docker"],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "security",
                "agent_id": "member-security",
                "system_prompt": "Audit for vulnerabilities and compliance.",
                "skills": ["review-changes", "web-search"],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "context": {
            "repo": "mo-dev-agent",
            "language": "rust,typescript",
            "ci": "github-actions",
            "deploy_target": "kubernetes",
            "branch": "feature/team-system"
        },
    });

    // Create
    let (status, body) = post(app.clone(), "/teams", user, payload.clone()).await;
    assert_eq!(status, StatusCode::OK, "create mega-team: {body}");

    // Fetch and verify every field
    let (status, team) = get(app.clone(), "/teams/name/mega-team", user).await;
    assert_eq!(status, StatusCode::OK);

    // Top-level
    assert_eq!(team["name"], "mega-team");
    assert_eq!(team["description"], "Complex team exercising every field");
    assert_eq!(team["user_id"], user);

    // Members
    let members = team["members"].as_array().unwrap();
    assert_eq!(members.len(), 5);

    // Verify lead member
    let lead = &members[0];
    assert_eq!(lead["role"], "lead");
    assert_eq!(lead["agent_id"], "agent-lead-001");
    assert_eq!(lead["model_selection"]["offering_id"], "offer-mega-lead");
    assert!(lead["can_delegate"].as_bool().unwrap());
    assert_eq!(lead["max_delegation_depth"], 3);
    let lead_skills: Vec<&str> = lead["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(lead_skills.contains(&"review-changes"));
    assert!(lead_skills.contains(&"plan-decompose"));
    let lead_mcp: Vec<&str> = lead["mcp_servers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(lead_mcp.contains(&"github"));
    assert!(lead_mcp.contains(&"jira"));

    // Verify frontend member has explicit agent_id
    let frontend = &members[2];
    assert_eq!(frontend["role"], "frontend");
    assert_eq!(frontend["agent_id"], "agent-frontend-react");
    assert!(!frontend["can_delegate"].as_bool().unwrap());

    // Context map
    assert_eq!(team["context"]["repo"], "mo-dev-agent");
    assert_eq!(team["context"]["language"], "rust,typescript");
    assert_eq!(team["context"]["ci"], "github-actions");
    assert_eq!(team["context"]["deploy_target"], "kubernetes");
    assert_eq!(team["context"]["branch"], "feature/team-system");
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 9: Team without optional fields
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_minimal_team_defaults() {
    let app = build_test_app();
    let user = "minimal-user";

    let (status, body) = post(
        app.clone(),
        "/teams",
        user,
        json!({
            "team_id": "id-bare-minimum",
            "name": "bare-minimum",
            "description": "Minimal member configuration",
            "members": [
                { "role": "worker", "agent_id": "member-worker", "skills": [], "mcp_servers": [] }
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "minimal team: {body}");

    let (status, saved) = get(app, "/teams/name/bare-minimum", user).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved, body);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 11: Team store not configured → 503
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_team_routes_without_store_return_503() {
    let app = build_test_app_without_team_store();
    let (status, body) = get(app.clone(), "/teams", "u1").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        body["detail"]
            .as_str()
            .unwrap()
            .contains("team service not configured"),
        "body={body}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 12: Malformed JSON and unknown fields on POST /teams
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_post_teams_invalid_payload_is_4xx() {
    let app = build_test_app();
    let (status, _) = post_raw(app.clone(), "/teams", "u1", "{not valid json").await;
    assert!(
        status.is_client_error(),
        "expected 4xx for invalid JSON, got {status}"
    );

    let mut payload = dev_team_payload();
    payload["unexpected"] = json!(true);
    let (status, _) = post(app.clone(), "/teams", "u1", payload).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = get(app, "/teams/name/dev-cycle", "u1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn scenario_team_identity_and_revision_are_required() {
    let app = build_test_app();
    for field in ["team_id", "agent_id"] {
        for invalid in [None, Some(Value::Null), Some(json!(""))] {
            let mut payload = dev_team_payload();
            let object = if field == "team_id" {
                payload.as_object_mut().unwrap()
            } else {
                payload["members"][0].as_object_mut().unwrap()
            };
            match invalid {
                Some(value) => {
                    object.insert(field.to_string(), value);
                }
                None => {
                    object.remove(field);
                }
            }
            let (status, body) = post(app.clone(), "/teams", "owner", payload).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["error_code"], "team_validation_failed");
        }
    }
    let (status, original) = post(app.clone(), "/teams", "owner", dev_team_payload()).await;
    assert_eq!(status, StatusCode::OK);
    for invalid in [
        None,
        Some(Value::Null),
        Some(json!(0)),
        Some(json!(u64::MAX)),
    ] {
        let mut payload = update_payload(dev_team_payload(), 1);
        match invalid {
            Some(value) => {
                payload["expected_revision"] = value;
            }
            None => {
                payload.as_object_mut().unwrap().remove("expected_revision");
            }
        }
        let (status, body) = put(app.clone(), "/teams/id-dev-cycle", "owner", payload).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error_code"], "team_validation_failed");
    }
    assert_eq!(get(app, "/teams/id-dev-cycle", "owner").await.1, original);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 14: Snapshot CRUD via API
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_snapshot_crud() {
    let app = build_test_app();
    let user = "snap-user";

    // Create a team first
    let (s, _) = post(app.clone(), "/teams", user, sequential_migration_payload()).await;
    assert_eq!(s, StatusCode::OK);

    // Create snapshot
    let (s, snap) = post(
        app.clone(),
        "/teams/id-db-migration/snapshots",
        user,
        json!({ "label": "before refactor", "git_commit": "abc123" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "create snapshot: {snap}");
    assert!(!snap["snapshot_id"].as_str().unwrap().is_empty());
    assert_eq!(snap["team_name"], "db-migration");
    assert_eq!(snap["team_id"], "id-db-migration");
    assert_eq!(snap["label"], "before refactor");
    assert_eq!(snap["git_commit"], "abc123");
    assert!(snap["team_definition_json"].as_str().is_some());

    let snap_id = snap["snapshot_id"].as_str().unwrap().to_string();

    // List snapshots
    let (s, body) = get(app.clone(), "/teams/id-db-migration/snapshots", user).await;
    assert_eq!(s, StatusCode::OK);
    let snaps = body["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0]["snapshot_id"], snap_id);

    // A rename retains the history; a new Team with the old name does not inherit it.
    let mut renamed = update_payload(sequential_migration_payload(), 1);
    renamed["name"] = json!("renamed-migration");
    assert_eq!(
        put(app.clone(), "/teams/id-db-migration", user, renamed)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        get(app.clone(), "/teams/id-db-migration/snapshots", user)
            .await
            .1["snapshots"][0]["team_id"],
        "id-db-migration"
    );
    let mut recreated = sequential_migration_payload();
    recreated["team_id"] = json!("recreated-migration");
    assert_eq!(
        post(app.clone(), "/teams", user, recreated).await.0,
        StatusCode::OK
    );
    assert_eq!(
        get(app.clone(), "/teams/recreated-migration/snapshots", user)
            .await
            .1["snapshots"],
        json!([])
    );
    assert_eq!(
        get(app.clone(), &format!("/teams/snapshots/{snap_id}"), user)
            .await
            .1["team_id"],
        "id-db-migration"
    );

    // Delete snapshot
    let (s, body) = delete(app.clone(), &format!("/teams/snapshots/{snap_id}"), user).await;
    assert_eq!(s, StatusCode::OK);
    assert!(body["deleted"].as_bool().unwrap());

    // Confirm gone
    let (s, body) = get(app.clone(), "/teams/id-db-migration/snapshots", user).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["snapshots"].as_array().unwrap().len(), 0);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 15: Snapshot user isolation
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_snapshot_user_isolation() {
    let app = build_test_app();

    // Alice creates a team and snapshot
    let (s, _) = post(
        app.clone(),
        "/teams",
        "alice",
        sequential_migration_payload(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, snap) = post(
        app.clone(),
        "/teams/id-db-migration/snapshots",
        "alice",
        json!({ "label": "alice snap" }),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let snap_id = snap["snapshot_id"].as_str().unwrap().to_string();

    // Bob creates same-named team
    let mut bob = sequential_migration_payload();
    bob["team_id"] = json!("bob-db-migration");
    let (s, _) = post(app.clone(), "/teams", "bob", bob).await;
    assert_eq!(s, StatusCode::OK);

    // Bob sees no snapshots for his team
    let (s, body) = get(app.clone(), "/teams/bob-db-migration/snapshots", "bob").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["snapshots"].as_array().unwrap().len(), 0);

    // Bob cannot delete Alice's snapshot
    let (s, _) = delete(app.clone(), &format!("/teams/snapshots/{snap_id}"), "bob").await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Alice still sees her snapshot
    let (s, body) = get(app.clone(), "/teams/id-db-migration/snapshots", "alice").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body["snapshots"].as_array().unwrap().len(), 1);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 16: Snapshot for non-existent team → 404
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_snapshot_nonexistent_team_404() {
    let app = build_test_app();
    let (s, _) = get(app.clone(), "/teams/ghost/snapshots", "u1").await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (s, _) = post(
        app,
        "/teams/ghost/snapshots",
        "u1",
        json!({ "label": "nope" }),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ═══════════════════════════════════════════════════════════════════════════
// Scenario 17: Delete non-existent snapshot → 404
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn scenario_delete_nonexistent_snapshot_404() {
    let app = build_test_app();
    let (s, _) = delete(app, "/teams/snapshots/no-such-snap", "u1").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
