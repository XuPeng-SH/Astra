//! Product harness HTTP handlers.
//!
//! These endpoints are distinct from `/sessions/{session_id}/harness/*`, which
//! exposes diagnostic harness snapshots for agent-loop observability.

use super::*;
use async_trait::async_trait;

pub(crate) struct AppStateSkillCreatorToolService {
    state: AppState,
}

impl AppStateSkillCreatorToolService {
    pub(crate) fn new(state: &AppState) -> Self {
        Self {
            state: state.clone(),
        }
    }
}

#[async_trait]
impl crate::server::runtime_tool_executor::SkillCreatorToolService
    for AppStateSkillCreatorToolService
{
    async fn create_skill(
        &self,
        user_id: &str,
        session_id: &str,
        request: astra_services::AuthoringIntentRequest,
    ) -> Result<astra_services::AuthoringIntentRecord, String> {
        run_authoring_intent(
            &self.state,
            user_id.to_string(),
            session_id.to_string(),
            request,
        )
        .await
        .map_err(|(_, body)| body.0.detail)
    }
}

async fn persist_authoring_evaluation(
    state: &AppState,
    user_id: &str,
    record: &mut AuthoringIntentRecord,
    expected_candidate_revision_id: &str,
    evaluation: AuthoringEvaluationSummary,
    plan: Option<astra_services::evaluation::EvaluationExperimentPrepareResponse>,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let harness_run_id = record.harness_run.harness_run_id.clone();
    record.harness_run = state
        .harness_service
        .persist_authoring_evaluation(
            user_id.to_string(),
            harness_run_id,
            expected_candidate_revision_id.to_string(),
            evaluation.clone(),
            plan.clone(),
        )
        .await?;
    let durable_candidate_revision_id = record
        .harness_run
        .output_json
        .pointer("/authoring/candidate_revision_id")
        .and_then(serde_json::Value::as_str);
    if durable_candidate_revision_id != Some(expected_candidate_revision_id) {
        return Err(error_response(
            StatusCode::CONFLICT,
            "authoring candidate changed while Evaluation was being persisted; retry the request",
        ));
    }
    record.evaluation = record
        .harness_run
        .output_json
        .pointer("/authoring/evaluation")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_else(|| AuthoringEvaluationSummary {
            status: "unavailable".to_string(),
            reason: "the durable authoring Evaluation result was missing after persistence"
                .to_string(),
            experiment_id: None,
        });
    record.evaluation_plan = record
        .harness_run
        .output_json
        .pointer("/authoring/evaluation_plan")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    Ok(())
}

async fn prepare_authoring_evaluation(
    state: &AppState,
    user_id: String,
    mut record: AuthoringIntentRecord,
) -> Result<AuthoringIntentRecord, (StatusCode, Json<ErrorResponse>)> {
    if record.evaluation_plan.is_some() {
        return Ok(record);
    }
    let Some(input) = record.evaluation_input.take() else {
        return Ok(record);
    };
    let expected_candidate_revision_id = input.target.candidate.revision_id.clone();

    let unavailable = |reason: String| AuthoringEvaluationSummary {
        status: "unavailable".to_string(),
        reason,
        experiment_id: None,
    };
    let model_offering_id = match state
        .model_service
        .default_user_model_offering_id(user_id.clone())
        .await
    {
        Ok(Some(offering_id)) => offering_id,
        Ok(None) => {
            let evaluation = unavailable(
                "a default model offering is required before the server-owned Evaluation can run"
                    .to_string(),
            );
            persist_authoring_evaluation(
                state,
                &user_id,
                &mut record,
                &expected_candidate_revision_id,
                evaluation,
                None,
            )
            .await?;
            return Ok(record);
        }
        Err((_, body)) => {
            let evaluation = unavailable(format!(
                "the default model offering could not be resolved: {}",
                body.0.detail
            ));
            persist_authoring_evaluation(
                state,
                &user_id,
                &mut record,
                &expected_candidate_revision_id,
                evaluation,
                None,
            )
            .await?;
            return Ok(record);
        }
    };

    let request = crate::evaluation::api::EvaluationExperimentPrepareRequest {
        submission_idempotency_key: input.submission_idempotency_key,
        target: input.target,
        case: input.case,
        model_offering_id,
        workspace: None,
        judgment_model_offering_id: None,
        max_concurrency: 1,
        max_wall_time_secs: 300,
    };
    match crate::evaluation::prepare::prepare_experiment(state, &user_id, request).await {
        Ok(plan) => {
            let evaluation = AuthoringEvaluationSummary {
                status: "prepared".to_string(),
                reason: "the shared Evaluation is prepared; the CLI/Web authoring flow can run its frozen trials and show the evidence report"
                    .to_string(),
                experiment_id: Some(plan.experiment.experiment_id.clone()),
            };
            persist_authoring_evaluation(
                state,
                &user_id,
                &mut record,
                &expected_candidate_revision_id,
                evaluation,
                Some(plan),
            )
            .await?;
            Ok(record)
        }
        Err((_, body)) => {
            let evaluation = unavailable(format!(
                "the shared Evaluation could not be prepared: {}",
                body.0.detail
            ));
            persist_authoring_evaluation(
                state,
                &user_id,
                &mut record,
                &expected_candidate_revision_id,
                evaluation,
                None,
            )
            .await?;
            Ok(record)
        }
    }
}

pub(crate) async fn run_authoring_intent(
    state: &AppState,
    user_id: String,
    session_id: String,
    request: AuthoringIntentRequest,
) -> Result<AuthoringIntentRecord, (StatusCode, Json<ErrorResponse>)> {
    let record = state
        .harness_service
        .create_authoring_intent(user_id.clone(), session_id, request)
        .await?;
    prepare_authoring_evaluation(state, user_id, record).await
}

pub async fn list_harness_templates_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<HarnessTemplateRecord>>, (StatusCode, Json<ErrorResponse>)> {
    state.auth_service.current_user(&headers).await?;
    state.harness_service.list_templates().await.map(Json)
}

pub async fn list_harness_node_catalog_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<HarnessNodeCatalogRecord>>, (StatusCode, Json<ErrorResponse>)> {
    state.auth_service.current_user(&headers).await?;
    state.harness_service.list_node_catalog().await.map(Json)
}

pub async fn create_skillify_harness_run_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SkillifyRunRequest>,
) -> Result<(StatusCode, Json<HarnessRunRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .create_skillify_run(user.user_id, request)
        .await
        .map(|run| (StatusCode::CREATED, Json(run)))
}

/// High-level authoring entrypoint. The session is an authenticated route
/// boundary; the request body contains only the user's natural-language goal.
pub async fn create_authoring_intent_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(request): Json<AuthoringIntentRequest>,
) -> Result<(StatusCode, Json<AuthoringIntentRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    run_authoring_intent(&state, user.user_id, session_id, request)
        .await
        .map(|record| (StatusCode::CREATED, Json(record)))
}

pub async fn create_standalone_authoring_intent_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<AuthoringIntentRequest>,
) -> Result<(StatusCode, Json<AuthoringIntentRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    run_authoring_intent(&state, user.user_id, String::new(), request)
        .await
        .map(|record| (StatusCode::CREATED, Json(record)))
}

pub async fn get_harness_run_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(harness_run_id): Path<String>,
) -> Result<Json<HarnessRunRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .get_run(user.user_id, harness_run_id)
        .await
        .map(Json)
}

pub async fn list_harness_run_items_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(harness_run_id): Path<String>,
) -> Result<Json<Vec<HarnessItemRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .list_run_items(user.user_id, harness_run_id)
        .await
        .map(Json)
}

pub async fn decide_harness_item_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((harness_run_id, item_id)): Path<(String, String)>,
    Json(request): Json<HarnessDecisionRequest>,
) -> Result<Json<HarnessItemRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .decide_item(user.user_id, harness_run_id, item_id, request)
        .await
        .map(Json)
}

pub async fn list_skill_drafts_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(harness_run_id): Path<String>,
) -> Result<Json<Vec<HarnessSkillDraftRecord>>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .list_skill_drafts(user.user_id, harness_run_id)
        .await
        .map(Json)
}

pub async fn get_skill_draft_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((harness_run_id, skill_draft_id)): Path<(String, String)>,
) -> Result<Json<HarnessSkillDraftRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .get_skill_draft(user.user_id, harness_run_id, skill_draft_id)
        .await
        .map(Json)
}

pub async fn decide_skill_draft_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((harness_run_id, skill_draft_id)): Path<(String, String)>,
    Json(request): Json<HarnessDecisionRequest>,
) -> Result<Json<HarnessSkillDraftRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .decide_skill_draft(user.user_id, harness_run_id, skill_draft_id, request)
        .await
        .map(Json)
}

pub async fn decide_skill_rule_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((harness_run_id, skill_draft_id, skill_rule_id)): Path<(String, String, String)>,
    Json(request): Json<HarnessDecisionRequest>,
) -> Result<Json<HarnessSkillDraftRecord>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .decide_skill_rule(
            user.user_id,
            harness_run_id,
            skill_draft_id,
            skill_rule_id,
            request,
        )
        .await
        .map(Json)
}

pub async fn publish_skill_draft_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((harness_run_id, skill_draft_id)): Path<(String, String)>,
    Json(request): Json<SkillifyPublishRequest>,
) -> Result<(StatusCode, Json<SkillifyPublishRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .publish_skill_draft(user.user_id, harness_run_id, skill_draft_id, request)
        .await
        .map(|record| (StatusCode::CREATED, Json(record)))
}

pub async fn create_skillify_draft_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(harness_run_id): Path<String>,
    Json(request): Json<SkillifyDraftRequest>,
) -> Result<(StatusCode, Json<SkillifyDraftRecord>), (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    state
        .harness_service
        .create_skillify_draft(user.user_id, harness_run_id, request)
        .await
        .map(|draft| (StatusCode::CREATED, Json(draft)))
}
