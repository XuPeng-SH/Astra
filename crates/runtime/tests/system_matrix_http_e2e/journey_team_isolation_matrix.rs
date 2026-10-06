//! Cross-user isolation, including protection of B's uninitialized builtin namespace.

use astra_services::team_persistence::builtin_teams;
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use super::harness::{E2E_PASSWORD, bootstrap, delete_json, get_json, post_json};

pub async fn run_team_cross_user_isolation() {
    let b = bootstrap().await;
    let ctx = &b.ctx;
    let auth_a = &b.auth_header;

    let team_name = format!("e2e_mx_iso_{}", ctx.suffix);

    let payload = json!({
        "team_id": Uuid::new_v4().to_string(),
        "name": team_name,
        "description": "owner A only",
        "members": [
            {
                "role": "a1",
                "agent_id": "a1",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            },
            {
                "role": "a2",
                "agent_id": "a2",
                "skills": [],
                "mcp_servers": [],
                "can_delegate": false,
                "max_delegation_depth": 0
            }
        ],
        "context": {},
    });

    let (st_up, created) = post_json(&ctx.app, "/teams", Some(auth_a), payload.clone()).await;
    assert_eq!(st_up, StatusCode::OK);

    let b_suffix = Uuid::new_v4().simple().to_string();
    let b_username = format!("prod_matrix_iso_{b_suffix}");
    let b_email = format!("prod_matrix_iso_{b_suffix}@e2e.test");

    let (st_reg, reg_b) = post_json(
        &ctx.app,
        "/auth/register",
        None,
        json!({
            "username": b_username,
            "email": b_email,
            "password": E2E_PASSWORD,
            "display_name": "Team isolation B"
        }),
    )
    .await;
    assert_eq!(st_reg, StatusCode::CREATED, "register B: {reg_b}");
    let user_b = reg_b["user_id"].as_str().expect("B user_id");
    let builtins_b = builtin_teams(user_b);
    let builtin_b = builtins_b.first().expect("builtin Team");
    let mut claim = payload;
    claim["team_id"] = json!(builtin_b.team_id);
    claim["name"] = json!(format!("claim_builtin_{}", ctx.suffix));
    let (st_claim, rejected) = post_json(&ctx.app, "/teams", Some(auth_a), claim).await;
    assert_eq!(
        st_claim,
        StatusCode::BAD_REQUEST,
        "reserved Team ID: {rejected}"
    );
    assert_eq!(rejected["error_code"], "team_validation_failed");

    let (st_login, login_j) = post_json(
        &ctx.app,
        "/auth/login",
        None,
        json!({ "username": b_username, "password": E2E_PASSWORD }),
    )
    .await;
    assert_eq!(st_login, StatusCode::OK, "login B: {login_j}");
    let access_b = login_j["access_token"].as_str().expect("B access_token");
    let auth_b = format!("Bearer {access_b}");

    // This must be B's first Team initialization, after A's attempted ID reservation.
    let (st_list_b, list_b) = get_json(&ctx.app, "/teams", Some(&auth_b), &[]).await;
    assert_eq!(
        st_list_b,
        StatusCode::OK,
        "B builtin initialization: {list_b}"
    );
    let teams_b = list_b["teams"].as_array().expect("teams B");
    assert_eq!(teams_b.len(), builtins_b.len());
    for builtin in &builtins_b {
        assert!(
            teams_b.contains(&serde_json::to_value(builtin).unwrap()),
            "B must receive its own unmodified builtin {}: {list_b}",
            builtin.team_id
        );
    }
    assert!(
        !teams_b
            .iter()
            .any(|t| t["name"].as_str() == Some(team_name.as_str())),
        "B list must not contain A team: {list_b}"
    );

    let team_id = created["team_id"].as_str().expect("team_id");
    let path_t = format!("/teams/{team_id}");
    let (st_g, _) = get_json(&ctx.app, &path_t, Some(&auth_b), &[]).await;
    assert_eq!(st_g, StatusCode::NOT_FOUND, "B must not see A team by ID");

    let (st_d, _) = delete_json(&ctx.app, &path_t, Some(&auth_b)).await;
    assert_eq!(st_d, StatusCode::NOT_FOUND, "B must not delete A team");

    let (st_del_a, _) = delete_json(&ctx.app, &path_t, Some(auth_a)).await;
    assert_eq!(st_del_a, StatusCode::OK);

    b.ctx.close().await;
}
