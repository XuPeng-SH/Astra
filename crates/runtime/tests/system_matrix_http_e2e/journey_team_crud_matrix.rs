//! Team CRUD Matrix E2E: `POST/GET/DELETE /teams`, `team_definitions` rows.
//!
//! Runs against real [`astra_services::team_persistence::MatrixOneTeamStore`] via `build_server_state`.

use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::Row;

use super::harness::{bootstrap, delete_json, get_json, post_json, put_json};

fn minimal_team_payload(name: &str, description: &str) -> Value {
    json!({
        "team_id": uuid::Uuid::new_v4().to_string(),
        "name": name,
        "description": description,
        "members": [
            {
                "role": "coder",
                "agent_id": "coder",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "reviewer",
                "agent_id": "reviewer",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "context": { "suite": "matrix_team_crud" },
    })
}

pub async fn run_team_crud_db() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth = &b.auth_header;

    let team_name = format!("e2e_mx_team_{}", ctx.suffix);

    let payload_v1 = minimal_team_payload(&team_name, "matrix e2e team crud v1");
    let (st, body) = post_json(&ctx.app, "/teams", Some(auth), payload_v1.clone()).await;
    assert_eq!(st, StatusCode::OK, "POST /teams create: {body}");
    let team_id = body["team_id"].as_str().expect("team_id").to_string();
    assert_eq!(body["user_id"].as_str(), Some(ctx.user_id.as_str()));
    assert_eq!(body["name"].as_str(), Some(team_name.as_str()));

    let (st_list, list_j) = get_json(&ctx.app, "/teams", Some(auth), &[]).await;
    assert_eq!(st_list, StatusCode::OK, "GET /teams: {list_j}");
    let teams = list_j["teams"].as_array().expect("teams array");
    assert!(
        teams
            .iter()
            .any(|t| t["name"].as_str() == Some(team_name.as_str())),
        "list should include our team: {list_j}"
    );

    let path_detail = format!("/teams/{team_id}");
    let (st_get, get_j) = get_json(&ctx.app, &path_detail, Some(auth), &[]).await;
    assert_eq!(st_get, StatusCode::OK, "GET team: {get_j}");
    assert_eq!(get_j["team_id"].as_str(), Some(team_id.as_str()));

    let row = sqlx::query(
        "SELECT team_id, user_id, name FROM team_definitions WHERE user_id = ? AND name = ?",
    )
    .bind(&ctx.user_id)
    .bind(&team_name)
    .fetch_optional(&ctx.pool)
    .await
    .expect("team_definitions SELECT");
    let row = row.expect("team_definitions row after POST");
    assert_eq!(row.get::<String, _>("team_id"), team_id);
    assert_eq!(row.get::<String, _>("user_id"), ctx.user_id);

    let mut payload_v2 = payload_v1;
    payload_v2.as_object_mut().unwrap().remove("team_id");
    payload_v2["expected_revision"] = body["revision"].clone();
    payload_v2["description"] = json!("matrix e2e team crud v2 CAS");
    let (st2, body2) = put_json(&ctx.app, &path_detail, Some(auth), payload_v2).await;
    assert_eq!(st2, StatusCode::OK, "PUT Team CAS: {body2}");
    assert_eq!(
        body2["team_id"].as_str(),
        Some(team_id.as_str()),
        "CAS preserves immutable team identity"
    );
    assert_eq!(
        body2["description"].as_str(),
        Some("matrix e2e team crud v2 CAS")
    );

    let desc_db: String = sqlx::query_scalar(
        "SELECT description FROM team_definitions WHERE user_id = ? AND name = ?",
    )
    .bind(&ctx.user_id)
    .bind(&team_name)
    .fetch_one(&ctx.pool)
    .await
    .expect("description after CAS");
    assert_eq!(desc_db, "matrix e2e team crud v2 CAS");

    let (st_del, del_j) = delete_json(&ctx.app, &path_detail, Some(auth)).await;
    assert_eq!(st_del, StatusCode::OK, "DELETE team: {del_j}");
    assert_eq!(del_j["deleted"].as_bool(), Some(true));

    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM team_definitions WHERE user_id = ? AND name = ?")
            .bind(&ctx.user_id)
            .bind(&team_name)
            .fetch_one(&ctx.pool)
            .await
            .expect("count after delete");
    assert_eq!(n, 0, "team_definitions row removed");

    b.ctx.close().await;
}
