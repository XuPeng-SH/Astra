//! Composable mid-loop guards extracted from `execute_turn_and_ingest_phase`.
//!
//! Each guard is a self-contained fn that checks a condition on
//! `AgenticLoopState`, optionally sets a state flag, and returns a
//! `GuardOutcome`. Guards were previously inlined as ~30-line blocks
//! inside a 1100-line function; extracting them here gives each a
//! documented home, makes them independently testable, and reduces the
//! orchestration function to a readable pipeline.
//!
//! # Adding a new guard
//!
//! 1. Add a `check_<name>(state, cfg) -> GuardOutcome` function below.
//! 2. Register it in `default_guards()`.
//! 3. Write a unit test using a minimal `AgenticLoopState` fixture.
//! 4. Remove the corresponding inline block from
//!    `execute_turn_and_ingest_phase`.

use std::collections::HashSet;

use super::execution_phase::{
    cache_waste_advisory_message, cache_wasteful_tools, parallel_batching_advisory_message,
    should_emit_cache_waste_advisory, should_emit_parallel_batching_advisory,
};
use super::host::{AgenticLoopState, VolatileKind};
use astra_turn_core::headless::body_preview::HeadlessStderrStyle;

// ── Pipeline types ─────────────────────────────────────────────────────

/// Outcome of a single guard evaluation.
#[must_use]
pub(crate) enum GuardOutcome {
    /// Guard did not fire; continue to next guard.
    Pass,
    /// Guard fired: push this signal as volatile advisory evidence, optionally
    /// emit `hint` as a yellow stderr line.
    Advisory {
        message: String,
        kind: VolatileKind,
        hint: Option<String>,
    },
}

/// Shared configuration for all guards in a turn.
#[derive(Clone)]
pub(crate) struct GuardConfig {
    pub parallel_batching_force_streak: usize,
    pub cache_waste_threshold: usize,
}

type GuardFn = fn(&mut AgenticLoopState, &GuardConfig) -> GuardOutcome;

/// Ordered list of guards to run before each LLM call.
///
/// Ordering matters: earlier guards set state flags that later guards
/// check to defer when a stronger intervention is already active
/// (e.g. redundant_reads defers to round_budget_phase1).
pub(crate) fn default_guards() -> Vec<(&'static str, GuardFn)> {
    vec![
        ("observation_reuse", check_observation_reuse),
        ("work_evidence_sufficiency", check_work_evidence_sufficiency),
        (
            "parallel_batching_advisory",
            check_parallel_batching_advisory,
        ),
        ("cache_waste", check_cache_waste),
    ]
}

/// A normalized, typed identity for a self-diagnosis request.  This is
/// deliberately separate from the provider's raw JSON/signature: aliases and
/// omitted defaults that resolve to the same observation are one request, but
/// different facets, horizons, sources, or questions remain distinct.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ObservationRequestKey {
    Introspect {
        topic: String,
        facet: String,
        depth: String,
        horizon: String,
        source_policy: String,
        include_context: bool,
        format_json: bool,
        run_id: Option<String>,
    },
    Reflect {
        topic: String,
        facet: String,
        depth: String,
        horizon: String,
        source_policy: String,
        include_context: bool,
        last_n: i32,
        question: String,
    },
}

impl ObservationRequestKey {
    fn label(&self) -> &'static str {
        match self {
            Self::Introspect { .. } => "introspect",
            Self::Reflect { .. } => "reflect",
        }
    }
}

/// Parse only the canonical request fields that the observation tools
/// themselves use.  Failed, rejected, artifact-recovery, and unknown calls
/// are not classified; a self-diagnosis guard must never infer intent from
/// display text or a truncated argument preview.
fn observation_request_key(
    record: &astra_services::session_journal::ToolCallRecord,
) -> Option<ObservationRequestKey> {
    if !record.ok || !record.was_executed() {
        return None;
    }
    let args = serde_json::from_str::<serde_json::Value>(record.authoritative_args_full()?).ok()?;
    match record.name.as_str() {
        "introspect" if args.get("artifact").is_none() => {
            let request = astra_turn_core::introspect::IntrospectRequest::from_args(&args);
            let run_id = args
                .get("_run_id")
                .or_else(|| args.get("run_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string);
            Some(ObservationRequestKey::Introspect {
                topic: request.topic.as_str().to_string(),
                facet: request.facet.as_str().to_string(),
                depth: request.depth.as_str().to_string(),
                horizon: request.horizon.as_str().to_string(),
                source_policy: request.source_policy.as_str().to_string(),
                include_context: request.include_context,
                format_json: request.format.is_json(),
                run_id,
            })
        }
        "reflect" => {
            let text_arg = |name: &str| {
                args.get(name)
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            };
            let include_context = args
                .get("include_context")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let last_n = args
                .get("last_n")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(20)
                .clamp(1, 100) as i32;
            let request =
                astra_services::reflect::ReflectRequest::from_observation_params_with_source(
                    text_arg("topic"),
                    text_arg("facet"),
                    text_arg("depth"),
                    text_arg("horizon"),
                    text_arg("source_policy"),
                    include_context,
                    last_n,
                    args.get("question")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default(),
                );
            Some(ObservationRequestKey::Reflect {
                topic: request.topic.as_str().to_string(),
                facet: request.facet.as_str().to_string(),
                depth: request.depth.as_str().to_string(),
                horizon: request.horizon.as_str().to_string(),
                source_policy: request.source_policy.as_str().to_string(),
                include_context: request.include_context,
                last_n: request.last_n,
                question: request.question,
            })
        }
        _ => None,
    }
}

/// Find duplicate observation requests in the contiguous tail of successful
/// observation calls.  Any intervening tool call is a state-transition
/// boundary and makes a fresh observation potentially meaningful.  The scan is
/// bounded because this is a pre-provider hot-path guard.
fn repeated_observation_request(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Option<ObservationRequestKey> {
    const MAX_OBSERVATION_TAIL: usize = 8;
    let mut seen = HashSet::new();
    for record in records.iter().rev().take(MAX_OBSERVATION_TAIL) {
        let Some(key) = observation_request_key(record) else {
            break;
        };
        if !seen.insert(key.clone()) {
            return Some(key);
        }
    }
    None
}

fn check_observation_reuse(state: &mut AgenticLoopState, _cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.observation_reuse_advisory_emitted || state.stall.any_behavior_advisory_emitted()
    {
        return GuardOutcome::Pass;
    }
    let records = state
        .stall
        .tool_call_records
        .get(state.stall.observation_reuse_record_floor..)
        .unwrap_or_default();
    let Some(request) = repeated_observation_request(records) else {
        return GuardOutcome::Pass;
    };

    state.stall.observation_reuse_advisory_emitted = true;
    tracing::info!(
        target: "astra::loop_guard",
        tool = request.label(),
        round = state.llm_rounds_completed,
        "typed observation reuse advisory observed"
    );
    GuardOutcome::Advisory {
        message: format!(
            "Observation reuse: the same typed {} request already succeeded without an intervening tool state transition; reuse its evidence or choose one explicitly missing facet. This is advisory only.",
            request.label()
        ),
        kind: VolatileKind::BehaviorAdvisory,
        hint: None,
    }
}

/// Run all registered guards. Model-facing advisory evidence is independent
/// from presentation mode; the caller decides whether returned status hints
/// should be shown to the user.
pub(crate) fn evaluate_guards(
    guards: &[(&str, GuardFn)],
    state: &mut AgenticLoopState,
    cfg: &GuardConfig,
) -> Vec<(HeadlessStderrStyle, String)> {
    let mut hints = Vec::new();
    for (name, guard_fn) in guards {
        match guard_fn(state, cfg) {
            GuardOutcome::Pass => {}
            GuardOutcome::Advisory {
                message,
                kind,
                hint,
            } => {
                state.push_volatile(kind, message.clone());
                tracing::info!(
                    target: "astra::loop_guard",
                    guard = name,
                    round = state.llm_rounds_completed,
                    "guard fired"
                );
                if let Some(hint_text) = hint {
                    hints.push((HeadlessStderrStyle::Yellow, hint_text));
                }
            }
        }
    }
    hints
}

// ═══════════════════════════════════════════════════════════════════════
// Individual guard implementations
// ═══════════════════════════════════════════════════════════════════════

pub(crate) struct WorkDirectionSnapshot {
    pub key: String,
    pub evidence: astra_services::work_direction_judgment::WorkDirectionEvidence,
    /// Whether the displayed evidence can be assessed for support, not proof
    /// of complete attempt coverage or permission to settle.
    pub support_basis_available: bool,
}

fn direction_hash(value: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

/// Preserve all current guidance, or abstain. Check byte/count bounds before
/// trimming, cloning or traversing an arbitrarily large input/history.
fn direction_current_guidance(state: &AgenticLoopState) -> Option<Option<String>> {
    let applied = state.user_intents.applied_user_intents();
    if applied.len() > 16 {
        return None;
    }
    // Assignment objective/expected_result already carry the original task.
    // Applied directives have no reconciliation watermark, so retain all of
    // them conservatively; never drop a newer constraint to fit the original.
    let mut guidance = String::new();
    for intent in applied {
        if intent.content.len() > 4096 {
            return None;
        }
        let content = intent.content.trim();
        if !content.is_empty() {
            if !guidance.is_empty() {
                guidance.push_str("\n\n");
            }
            guidance.push_str(content);
        }
        if guidance.chars().take(1025).count() > 1024 {
            return None;
        }
    }
    if guidance.chars().take(1025).count() > 1024 {
        return None;
    }
    Some((!guidance.is_empty()).then_some(guidance))
}

fn direction_assignment(state: &AgenticLoopState) -> Option<(String, String, String)> {
    let (binding, objective, expected) = state
        .runtime_tool_executor
        .as_deref()?
        .work_direction_assignment(
            state.context_manifest_user_id.as_deref()?,
            state.current_session_id.as_deref()?,
            state.current_run_id.as_deref()?,
        )?;
    let key = direction_assignment_key(
        &binding,
        state.current_run_id.as_deref(),
        &objective,
        &expected,
    );
    Some((key, objective, expected))
}

fn direction_assignment_key(
    binding: &astra_services::runs::WorkRuntimeBindingRequest,
    run: Option<&str>,
    objective: &str,
    expected: &str,
) -> String {
    // Evidence custody follows the exact attempt, not the worker holding its
    // lease. Ownership still fences advice via the snapshot key below.
    direction_hash(&serde_json::json!({
        "binding": binding, "run": run, "objective": objective, "expected": expected,
    }))
}

/// Only observations produced after this loop saw the exact assignment are
/// eligible. A missing round identity never manufactures attribution.
pub(crate) fn current_work_direction_snapshot(
    state: &AgenticLoopState,
) -> Option<WorkDirectionSnapshot> {
    let gate = &state.provider_adaptation.work_direction;
    if gate.disabled || (gate.attempted.len() >= 3 && gate.cached.is_none()) {
        return None;
    }
    work_direction_snapshot_for_assignment(state, direction_assignment(state)?)
}

fn work_direction_snapshot_for_assignment(
    state: &AgenticLoopState,
    assignment: (String, String, String),
) -> Option<WorkDirectionSnapshot> {
    use astra_services::work_direction_judgment::{
        WorkDirectionEvidence, WorkDirectionObservation,
    };
    if state.remaining_turns == 0
        || state.provider_adaptation.work_direction.disabled
        || (state.provider_adaptation.work_direction.attempted.len() >= 3
            && state.provider_adaptation.work_direction.cached.is_none())
        || state
            .cancellation
            .flag
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
        || state
            .cancellation
            .token
            .as_ref()
            .is_some_and(|token| token.is_cancelled())
        || state.hooks.completion_settlement.text_only
        || state.hooks.completion_settlement.work_settlement_only
        || state
            .hooks
            .completion_settlement
            .completion_action_window
            .is_some()
    {
        return None;
    }
    let (binding, objective, expected_result) = assignment;
    let gate = &state.provider_adaptation.work_direction;
    if gate.binding_key.as_ref() != Some(&binding) {
        return None;
    }
    let current_guidance = direction_current_guidance(state)?;
    let mut observations = Vec::new();
    let records = &state.stall.tool_call_records;
    // Rounds are turn-local. The journal retains older turns for audit, so
    // compare rounds only inside the existing runtime-owned turn boundary.
    let turn_floor = state.stall.observation_reuse_record_floor;
    if turn_floor > records.len() {
        return None;
    }
    // The validated assignment and its round floor fence this projection.
    // Journal tool names are not committed transitions: even a successful
    // run_next_work_item may resume the same attempt. No journal field proves
    // complete attempt custody. Omission tracks projection loss, not settlement
    // readiness; the advisory only assesses the supplied results.
    let mut omitted = false;
    let mut unattributed = false;
    let mut displayed_complete = Vec::new();
    let mut revisions = Vec::new();
    // Select from canonical metadata before truncation. An append-position
    // window lets old replays evict a newer failure. Keep bounded candidates,
    // not another persisted index or checkpoint authority.
    let mut ordered = std::collections::BTreeMap::<
        (u32, &str),
        (&astra_services::session_journal::ToolCallRecord, bool),
    >::new();
    let mut latest_round = None;
    let mut latest_call_id = None;
    let mut latest_round_ambiguous = false;
    for record in &records[turn_floor..] {
        if !record.was_executed() {
            continue;
        }
        if matches!(
            record.name.as_str(),
            "tool_search"
                | "start_work"
                | "run_next_work_item"
                | "settle_work_item"
                | "inspect_work_plan"
                | "propose_work_plan"
                | "inspect_work_criteria"
                | "propose_work_criteria"
                | "introspect"
                | "reflect"
        ) {
            continue;
        }
        let Some(round) = record.round else {
            omitted = true;
            unattributed = true;
            continue;
        };
        if round < gate.first_round {
            continue;
        }
        if record.name.len() > 128 {
            return None;
        }
        let call_id = record.tool_call_id.as_deref().filter(|id| !id.is_empty())?;
        if call_id.len() > 128 {
            return None;
        }
        if latest_round.is_none_or(|latest| round > latest) {
            latest_round = Some(round);
            latest_call_id = Some(call_id);
            latest_round_ambiguous = false;
        } else if latest_round == Some(round) && latest_call_id != Some(call_id) {
            latest_round_ambiguous = true;
        }
        let key = (round, call_id);
        if let Some((previous, conflicting)) = ordered.get_mut(&key) {
            if *conflicting {
                continue;
            }
            // A reused identity with inconsistent bounded content is not a
            // new execution or a trustworthy replacement. Opaque documents
            // cannot supply closure support in either case. Never compare
            // arbitrarily large strings while detecting duplicate revisions.
            let same_document = |left: Option<&str>, right: Option<&str>| match (left, right) {
                (Some(left), Some(right)) if left.len() <= 16_384 && right.len() <= 16_384 => {
                    left == right
                }
                // Both are opaque in this projection. Unknown equivalence is
                // not evidence of a conflict, nor of complete result support.
                (Some(left), Some(right)) if left.len() > 16_384 && right.len() > 16_384 => true,
                (None, None) => true,
                _ => false,
            };
            let same_artifact = match (&previous.result_artifact, &record.result_artifact) {
                (None, None) => true,
                (Some(left), Some(right)) => {
                    [left, right].into_iter().all(|artifact| {
                        artifact.call_id.len() <= 128
                            && artifact.run_id.len() <= 128
                            && artifact.content_sha256.len() <= 64
                    }) && left == right
                }
                _ => false,
            };
            *conflicting |= previous.name != record.name
                || previous.ok != record.ok
                || !same_artifact
                || !same_document(
                    previous.result_full.as_deref(),
                    record.result_full.as_deref(),
                )
                || !same_document(
                    previous.result_preview.as_deref(),
                    record.result_preview.as_deref(),
                )
                || !same_document(previous.args_full.as_deref(), record.args_full.as_deref());
            continue;
        }
        if ordered.len() == 32 {
            omitted = true;
            if ordered
                .first_key_value()
                .is_some_and(|(oldest, _)| key <= *oldest)
            {
                continue;
            }
            ordered.pop_first();
        }
        ordered.insert(key, (record, false));
    }
    for ((round, call_id), (record, conflicting)) in ordered {
        let raw = (!conflicting).then_some(record).and_then(|record| {
            record
                .result_full
                .as_deref()
                .or(record.result_preview.as_deref())
        });
        let args = record.args_full.as_deref().unwrap_or("");
        use sha2::{Digest, Sha256};
        // Hash only complete, bounded documents; never hash a prefix as if it
        // identified the full result. Large/missing documents remain an opaque
        // outcome linked to the existing invocation, not evidence of coverage.
        // An out-of-line document's immutable digest can still invalidate the
        // projection without fetching or traversing its contents.
        let bounded_raw = raw.filter(|raw| raw.len() <= 16_384);
        let bounded_args = (args.len() <= 16_384).then_some(args);
        omitted |= bounded_args.is_none();
        let artifact_digest = record.result_artifact.as_ref().and_then(|artifact| {
            (artifact.call_id == call_id
                && Some(artifact.run_id.as_str()) == state.current_run_id.as_deref()
                && artifact.content_sha256.len() == 64
                && artifact
                    .content_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit()))
            .then_some(artifact.content_sha256.as_str())
        });
        let revision = (
            record.name.clone(),
            record.ok,
            conflicting,
            bounded_raw.map(|raw| format!("{:x}", Sha256::digest(raw.as_bytes()))),
            bounded_args.map(|args| format!("{:x}", Sha256::digest(args.as_bytes()))),
            artifact_digest,
            raw.map(str::len),
            args.len(),
        );
        revisions.push((round, call_id, revision));
        // Do not sanitize an unbounded document or expose an unsanitized
        // prefix. The execution outcome itself remains useful for recovery.
        let (excerpt, complete) = if let Some(raw) = bounded_raw {
            let safe = astra_turn_core::safety_middleware::sanitize_tool_output_for_llm(raw);
            let complete = safe.content.chars().count() <= 768
                && record.result_full.is_some()
                && bounded_args.is_some()
                && record.result_artifact.is_none();
            omitted |= !complete;
            (safe.content.chars().take(768).collect(), complete)
        } else {
            omitted = true;
            (
                "Result content unavailable in this bounded projection; execution outcome only."
                    .into(),
                false,
            )
        };
        if observations.len() == 4 {
            omitted = true;
            observations.remove(0);
            displayed_complete.remove(0);
        }
        displayed_complete.push((complete, record.ok));
        observations.push(WorkDirectionObservation {
            tool: record.name.chars().take(128).collect(),
            disposition: if record.ok {
                "executed_success"
            } else {
                "executed_failure"
            }
            .into(),
            result_excerpt: excerpt,
        });
    }
    // Exact replays do not count as new evidence. Distinct executions do.
    if observations.len() < 2 {
        return None;
    }
    // The latest execution must supply a complete successful result, not just
    // a status or prefix. Earlier failed/opaque observations remain explicit
    // context; they cannot permanently veto a later repair and verification.
    // This only admits a semantic support judgment: the judge still must find
    // direct support for every material expected-result requirement.
    let support_basis_available = !unattributed
        && !latest_round_ambiguous
        && displayed_complete
            .last()
            .is_some_and(|(complete, ok)| *complete && *ok);
    let evidence = WorkDirectionEvidence {
        objective,
        expected_result,
        current_guidance,
        observations,
        omitted_observations: omitted,
    };
    let request =
        astra_services::work_direction_judgment::work_direction_judgment_request(&evidence);
    let key = direction_hash(&serde_json::json!({
        "version": 5, "binding": binding, "request": request, "evidence_revisions": revisions,
        "generation": state.current_run_owner_generation, "turn": state.session_turn,
        "support_basis_available": support_basis_available,
        "guidance_cursor": state.user_intents.applied_user_intents().last().map(|intent| intent.event_index),
    }));
    Some(WorkDirectionSnapshot {
        key,
        evidence,
        support_basis_available,
    })
}

fn direction_assignment_first_round(state: &AgenticLoopState) -> Option<u32> {
    // Match the projection's runtime-owned turn boundary: retained audit
    // rounds from earlier turns are not comparable to the current counter.
    let current_turn_records = state
        .stall
        .tool_call_records
        .get(state.stall.observation_reuse_record_floor..)?;
    Some(
        current_turn_records
            .iter()
            .filter_map(|record| record.round)
            .max()
            .map_or(state.llm_rounds_completed, |round| round.saturating_add(1)),
    )
}

/// Reserve before awaiting, including unavailable/abstaining outcomes. The
/// caller polls durable user guidance again before publishing the result.
pub(crate) async fn judge_work_direction<H: super::host::AgenticLoopHost>(
    host: &mut H,
    state: &mut AgenticLoopState,
) {
    state.clear_volatile(VolatileKind::WorkDirection);
    let gate = &state.provider_adaptation.work_direction;
    if gate.disabled || gate.attempted.len() >= 3 {
        return;
    }
    let Some((binding, _, _)) = direction_assignment(state) else {
        state.provider_adaptation.work_direction.cached = None;
        state.provider_adaptation.work_direction.binding_key = None;
        return;
    };
    if state
        .provider_adaptation
        .work_direction
        .binding_key
        .as_ref()
        != Some(&binding)
    {
        let Some(first_round) = direction_assignment_first_round(state) else {
            state.provider_adaptation.work_direction.cached = None;
            return;
        };
        let gate = &mut state.provider_adaptation.work_direction;
        gate.binding_key = Some(binding);
        gate.first_round = first_round;
        gate.cached = None;
        return;
    }
    let Some(snapshot) = current_work_direction_snapshot(state) else {
        tracing::debug!(operation = "work_direction", status = "ineligible");
        return;
    };
    tracing::debug!(operation = "work_direction", status = "eligible", snapshot_key = %snapshot.key);
    if !state
        .provider_adaptation
        .work_direction
        .reserve(&snapshot.key)
    {
        tracing::debug!(
            operation = "work_direction",
            status = "skipped",
            reason = "reserved_disabled_or_budget"
        );
        return;
    }
    tracing::debug!(operation = "work_direction", status = "attempted", snapshot_key = %snapshot.key);
    let outcome = host.judge_work_direction(state, &snapshot.evidence).await;
    retain_work_direction_outcome(state, snapshot, outcome);
}

fn retain_work_direction_outcome(
    state: &mut AgenticLoopState,
    snapshot: WorkDirectionSnapshot,
    outcome: super::host::WorkDirectionOutcome,
) {
    use astra_services::work_direction_judgment::WorkDirectionSemanticResult;
    let gate = &mut state.provider_adaptation.work_direction;
    gate.cached = None;
    match outcome {
        super::host::WorkDirectionOutcome::Disabled => {
            gate.disabled = true;
            tracing::debug!(
                operation = "work_direction",
                status = "disabled",
                reason = "no_judge"
            );
        }
        super::host::WorkDirectionOutcome::Unavailable => {
            tracing::debug!(operation = "work_direction", status = "unavailable");
        }
        super::host::WorkDirectionOutcome::Evaluated(WorkDirectionSemanticResult::Abstained {
            reason,
            assessment,
        }) => {
            tracing::debug!(operation = "work_direction", status = "abstained", ?reason,
                supported = assessment.supported, verify = assessment.verify, provenance = ?assessment.provenance);
        }
        super::host::WorkDirectionOutcome::Evaluated(WorkDirectionSemanticResult::Invalid {
            reason,
        }) => {
            tracing::debug!(operation = "work_direction", status = "invalid", ?reason);
        }
        super::host::WorkDirectionOutcome::Evaluated(WorkDirectionSemanticResult::Decision {
            decision,
            assessment,
        }) => {
            use astra_services::work_direction_judgment::WorkDirection;
            if !snapshot.support_basis_available
                && decision.direction == WorkDirection::PrepareSettlement
            {
                tracing::debug!(
                    operation = "work_direction",
                    status = "abstained",
                    reason = "incomplete_evidence"
                );
                return;
            }
            let direction = match decision.direction {
                WorkDirection::FocusedVerification => "focused_verification",
                WorkDirection::ContinueInvestigation => "continue_investigation",
                WorkDirection::PrepareSettlement => "prepare_settlement",
            };
            tracing::debug!(operation = "work_direction", status = "answered", direction,
                supported = assessment.supported, verify = assessment.verify, provenance = ?assessment.provenance);
            gate.cached = Some((
                snapshot.key.clone(),
                serde_json::json!({
                    "schema": "work_direction.v1", "snapshot_key": snapshot.key,
                    "direction": direction, "provenance": decision.provenance,
                    "authority": "advisory_only",
                    "evidence_scope": "displayed_results_for_current_assignment",
                    "omitted_observations": snapshot.evidence.omitted_observations,
                    "settlement_readiness": "unknown",
                    "instruction": "Consider this direction against the current assignment and direct results. It grants no tool, mutation, verification, or settlement authority. Existing runtime requirements still apply.",
                }),
            ));
        }
    }
}

pub(crate) fn publish_work_direction(state: &mut AgenticLoopState) {
    let current = current_work_direction_snapshot(state).map(|snapshot| snapshot.key);
    publish_work_direction_for_key(state, current);
}

fn publish_work_direction_for_key(state: &mut AgenticLoopState, current: Option<String>) {
    state.clear_volatile(VolatileKind::WorkDirection);
    let cached = state.provider_adaptation.work_direction.cached.clone();
    if let Some((key, payload)) = cached
        && current.as_ref() == Some(&key)
    {
        tracing::debug!(operation = "work_direction", status = "applied", snapshot_key = %key);
        state.push_volatile_payload(VolatileKind::WorkDirection, payload);
    } else {
        if state.provider_adaptation.work_direction.cached.is_some() {
            tracing::debug!(operation = "work_direction", status = "stale");
        }
        state.provider_adaptation.work_direction.cached = None;
    }
}

/// Project existing facts, independently of the one-shot behavior guards and
/// optional semantic judges. Do not duplicate the server's settlement gate:
/// journal records do not bind a result to an exact Work attempt, and success
/// alone cannot prove that its expected result is supported.
pub(crate) fn refresh_work_evidence_context(state: &mut AgenticLoopState) {
    state.clear_volatile(VolatileKind::WorkEvidenceContext);
    let (Some(executor), Some(user), Some(session), Some(run)) = (
        state.runtime_tool_executor.as_deref(),
        state.context_manifest_user_id.as_deref(),
        state.current_session_id.as_deref(),
        state.current_run_id.as_deref(),
    ) else {
        return;
    };
    if let Some(payload) = work_evidence_context(
        executor.primary_work_handoff(user, session, run),
        &state.stall.tool_call_records,
    ) {
        state.push_volatile_payload(VolatileKind::WorkEvidenceContext, payload);
    }
}

fn work_evidence_context(
    handoff: crate::server::runtime_tool_executor::PrimaryWorkHandoff,
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Option<serde_json::Value> {
    let crate::server::runtime_tool_executor::PrimaryWorkHandoff::Active { binding } = handoff
    else {
        return None;
    };
    Some(serde_json::json!({
        "schema": "work_evidence_context.v1",
        "binding": binding,
        "recent_observations": bounded_work_observations(records),
        "settlement_readiness": "unknown",
        "authority": "observation_only",
        "delivered_requirement": "A delivered settlement requires a successful non-lifecycle executable result for the assigned attempt; discovery and Work planning/inspection alone do not satisfy this requirement. Success alone does not prove expected_result.",
        "next_action": "Reuse relevant results already available for this assignment. If expected_result is supported, request settle_work_item alone; otherwise obtain the specific missing evidence or truthfully settle blocked/failed. Recent observations do not establish attempt attribution or semantic coverage.",
    }))
}

/// A bounded suffix, not an evidence ledger or a readiness classifier. Stop
/// even at failed lifecycle calls: their disposition cannot establish whether
/// the active assignment changed. Missing history never means no work happened.
fn bounded_work_observations(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> serde_json::Value {
    const WINDOW: usize = 8;
    const FIELD_CHARS: usize = 128;
    let mut observations = Vec::new();
    let mut boundary_seen = false;
    for record in records.iter().rev().take(WINDOW) {
        if matches!(
            record.name.as_str(),
            "start_work" | "run_next_work_item" | "settle_work_item"
        ) {
            boundary_seen = true;
            break;
        }
        // Omit oversized identities rather than manufacture a truncated ID.
        let call_id = record
            .tool_call_id
            .as_deref()
            .filter(|id| id.chars().take(FIELD_CHARS + 1).count() <= FIELD_CHARS);
        observations.push(serde_json::json!({
            "tool": record.name.chars().take(FIELD_CHARS).collect::<String>(),
            "tool_call_id": call_id,
            "disposition": record.effective_disposition(),
            "ok": record.ok,
        }));
    }
    observations.reverse();
    serde_json::json!({
        "scope": "recent_journal_suffix_not_attempt_proof",
        "lifecycle_boundary_seen": boundary_seen,
        "window_truncated": !boundary_seen && records.len() > WINDOW,
        "calls": observations,
    })
}

/// Number of successful, non-mutating tool executions inside one owned
/// WorkItem after which the model should explicitly reassess whether its typed
/// expected result is already supported. The threshold is deliberately above
/// the ordinary small investigation path and never removes tool authority.
const WORK_EVIDENCE_REASSESS_CALLS: usize = astra_turn_core::evaluation::LLM_ROUND_CHURN_THRESHOLD;

/// Count the current WorkItem's evidence path using only typed execution
/// records. The reverse scan is strictly bounded, stops at canonical Work
/// lifecycle boundaries, and declines to classify a path that has mutated the
/// workspace. That keeps this hot-loop check O(1) per model boundary and avoids
/// prompt-text or scenario matching.
fn bounded_read_only_work_evidence_calls(
    records: &[astra_services::session_journal::ToolCallRecord],
) -> Option<usize> {
    let mut successful = 0_usize;
    for record in records
        .iter()
        .rev()
        .take(WORK_EVIDENCE_REASSESS_CALLS.saturating_mul(2))
    {
        if matches!(
            record.name.as_str(),
            "start_work" | "run_next_work_item" | "settle_work_item"
        ) {
            break;
        }
        if !record.was_executed() {
            continue;
        }
        if super::lifecycle::tool_record_is_workspace_mutation(record) {
            return None;
        }
        if record.ok {
            successful = successful.saturating_add(1);
            if successful >= WORK_EVIDENCE_REASSESS_CALLS {
                return Some(successful);
            }
        }
    }
    Some(successful)
}

fn check_work_evidence_sufficiency(
    state: &mut AgenticLoopState,
    _cfg: &GuardConfig,
) -> GuardOutcome {
    if state.stall.any_behavior_advisory_emitted()
        || state
            .runtime_tool_executor
            .as_deref()
            .is_none_or(|executor| !executor.has_active_primary_work_attempt())
    {
        return GuardOutcome::Pass;
    }
    let Some(calls) = bounded_read_only_work_evidence_calls(&state.stall.tool_call_records) else {
        return GuardOutcome::Pass;
    };
    if calls < WORK_EVIDENCE_REASSESS_CALLS {
        return GuardOutcome::Pass;
    }

    state.stall.work_evidence_advisory_emitted = true;
    tracing::info!(
        target: "astra::loop_guard",
        calls,
        round = state.llm_rounds_completed,
        "owned WorkItem evidence-sufficiency advisory observed"
    );
    GuardOutcome::Advisory {
        // Keep decision feedback compact: CurrentUserOnly providers place it
        // on the uncached tail for one request. The count remains in tracing;
        // the model needs only the decision boundary on wire.
        message: "Owned WorkItem: settle_work_item if expected_result is supported; otherwise pursue one specific missing fact.".to_string(),
        kind: VolatileKind::BehaviorAdvisory,
        hint: None,
    }
}

/// Surface batching evidence when the model has produced a long streak of
/// single-tool rounds despite prompt-layer guidance. Catches the
/// "exploratory churn" failure mode (sessions 6566d6a8, bbae8641, 6da9cf8f).
fn check_parallel_batching_advisory(
    state: &mut AgenticLoopState,
    cfg: &GuardConfig,
) -> GuardOutcome {
    if state.stall.parallel_batching_advisory_emitted
        || !should_emit_parallel_batching_advisory(state, cfg.parallel_batching_force_streak)
    {
        return GuardOutcome::Pass;
    }
    state.stall.parallel_batching_advisory_emitted = true;
    let streak = crate::prompts::trailing_single_tool_round_streak(&state.messages);
    let msg = parallel_batching_advisory_message(streak, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "parallel_batching_advisory",
        streak,
        round = state.llm_rounds_completed,
        "behavior advisory observed"
    );
    GuardOutcome::Advisory {
        message: msg,
        kind: VolatileKind::BehaviorAdvisory,
        hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::session_journal::ToolCallRecord;

    fn successful(name: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ..Default::default()
        }
    }

    fn direction_state() -> AgenticLoopState {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.provider_adaptation.work_direction.binding_key = Some("assignment".into());
        state.provider_adaptation.work_direction.first_round = 2;
        state.stall.tool_call_records = ["report generated", "validation pending"]
            .into_iter()
            .enumerate()
            .map(|(i, text)| ToolCallRecord {
                round: Some(2 + i as u32),
                result_full: Some(text.into()),
                tool_call_id: Some(format!("call-{i}")),
                ..successful("read_file")
            })
            .collect();
        state
    }

    fn direction_snapshot(state: &AgenticLoopState) -> Option<WorkDirectionSnapshot> {
        work_direction_snapshot_for_assignment(
            state,
            ("assignment".into(), "objective".into(), "expected".into()),
        )
    }

    fn apply_direction_guidance(state: &mut AgenticLoopState, content: &str) {
        let event_index = state.user_intents.applied_user_intents().len() + 1;
        state
            .user_intents
            .record_applied_user_intents(&[super::super::host::AppliedUserIntent {
                intent_id: format!("guidance-{event_index}"),
                delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
                status: astra_turn_types::UserIntentStatus::Applied,
                event_index,
                content: content.into(),
            }]);
    }

    fn prepare_direction(state: &mut AgenticLoopState, snapshot: WorkDirectionSnapshot) {
        use astra_services::work_direction_judgment::{
            parse_work_direction_judgment, work_direction_judgment_request,
        };
        let request = work_direction_judgment_request(&snapshot.evidence);
        let decision =
            parse_work_direction_judgment(&request, r#"{"true":["supported"],"uncertain":[]}"#);
        assert!(matches!(
            decision,
            astra_services::work_direction_judgment::WorkDirectionSemanticResult::Decision { .. }
        ));
        retain_work_direction_outcome(
            state,
            snapshot,
            super::super::host::WorkDirectionOutcome::Evaluated(decision),
        );
    }

    #[test]
    fn work_direction_snapshot_deduplicates_rounds_and_rejects_stale_intent() {
        let mut state = direction_state();
        let first = direction_snapshot(&state).unwrap();
        state.llm_rounds_completed += 5;
        state
            .stall
            .tool_call_records
            .push(state.stall.tool_call_records[0].clone());
        assert_eq!(direction_snapshot(&state).unwrap().key, first.key);
        state.provider_adaptation.work_direction.cached = Some((
            first.key.clone(),
            serde_json::json!({"direction":"focused_verification"}),
        ));
        publish_work_direction_for_key(&mut state, Some(first.key));
        assert!(
            state
                .volatile_pending
                .iter()
                .any(|item| item.kind == VolatileKind::WorkDirection)
        );
        apply_direction_guidance(&mut state, "Stop investigating; give the current status");
        let snapshot = direction_snapshot(&state).unwrap();
        assert_eq!(
            snapshot.evidence.current_guidance.as_deref(),
            Some("Stop investigating; give the current status")
        );
        let current = snapshot.key;
        publish_work_direction_for_key(&mut state, Some(current));
        assert!(state.provider_adaptation.work_direction.cached.is_none());
        assert!(
            !state
                .volatile_pending
                .iter()
                .any(|item| item.kind == VolatileKind::WorkDirection)
        );
        assert!(state.restricted_tools.is_empty());
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
    }

    #[test]
    fn work_direction_snapshot_excludes_old_unexecuted_and_changed_assignments() {
        let mut state = direction_state();
        state.stall.tool_call_records[0].round = Some(1);
        assert!(direction_snapshot(&state).is_none());
        state.stall.tool_call_records[0].round = Some(2);
        state.stall.tool_call_records[0].disposition =
            Some(astra_services::session_journal::ToolCallDisposition::Rejected);
        assert!(direction_snapshot(&state).is_none());
        state = direction_state();
        state.provider_adaptation.work_direction.binding_key = Some("successor".into());
        assert!(direction_snapshot(&state).is_none());
        state = direction_state();
        state.hooks.completion_settlement.text_only = true;
        assert!(direction_snapshot(&state).is_none());
    }

    #[test]
    fn work_direction_bounds_excerpts_and_preserves_run_budget_on_restore() {
        let mut state = direction_state();
        state.stall.tool_call_records[0].result_full = Some("界".repeat(1000));
        let snapshot = direction_snapshot(&state).unwrap();
        assert!(snapshot.evidence.omitted_observations);
        assert!(
            snapshot
                .evidence
                .observations
                .iter()
                .all(|item| item.result_excerpt.chars().count() <= 768)
        );
        let gate = &mut state.provider_adaptation.work_direction;
        assert!(gate.reserve(&snapshot.key));
        assert!(!gate.reserve(&snapshot.key));
        assert!(gate.reserve("second"));
        let encoded = serde_json::to_value(&state.provider_adaptation).unwrap();
        let mut restored: super::super::host::ProviderAdaptationState =
            serde_json::from_value(encoded).unwrap();
        assert!(!restored.work_direction.reserve(&snapshot.key));
        assert!(restored.work_direction.reserve("third"));
        assert!(!restored.work_direction.reserve("fourth"));
        let mut disabled = super::super::host::WorkDirectionState {
            disabled: true,
            ..Default::default()
        };
        assert!(!disabled.reserve("first"));
    }

    #[test]
    fn work_direction_full_digest_detects_changes_beyond_prompt_excerpt() {
        let mut state = direction_state();
        let prefix = "x".repeat(800);
        state.stall.tool_call_records[0].result_full = Some(format!("{prefix}old"));
        let first = direction_snapshot(&state).unwrap();
        state.stall.tool_call_records[0].result_full = Some(format!("{prefix}new"));
        let changed = direction_snapshot(&state).unwrap();
        assert_eq!(first.evidence, changed.evidence);
        assert_ne!(first.key, changed.key);
        state
            .stall
            .tool_call_records
            .push(state.stall.tool_call_records[0].clone());
        assert_eq!(direction_snapshot(&state).unwrap().key, changed.key);
        state.stall.tool_call_records[0].result_full = Some("x".repeat(20_000));
        state.stall.tool_call_records[0].result_preview =
            Some("large report: verification pending".into());
        let oversized = direction_snapshot(&state).unwrap();
        assert!(oversized.evidence.omitted_observations);
        assert!(
            oversized.evidence.observations[0]
                .result_excerpt
                .contains("outcome only")
        );
        state = direction_state();
        state.stall.tool_call_records[0].args_full = Some("x".repeat(20_000));
        assert!(
            direction_snapshot(&state)
                .unwrap()
                .evidence
                .omitted_observations
        );
    }

    fn direction_execution(round: u32, id: &str, ok: bool, result: &str) -> ToolCallRecord {
        ToolCallRecord {
            round: Some(round),
            tool_call_id: Some(id.into()),
            name: "run_script".into(),
            disposition: Some(astra_services::session_journal::ToolCallDisposition::Executed),
            ok,
            result_full: Some(result.into()),
            ..Default::default()
        }
    }

    #[test]
    fn work_direction_old_success_replay_outside_suffix_cannot_supersede_failure() {
        let mut state = direction_state();
        let old = direction_execution(2, "old-verification", true, "old verification passed");
        state.stall.tool_call_records = vec![old.clone()];
        for round in 3..=36 {
            state.stall.tool_call_records.push(direction_execution(
                round,
                &format!("failure-{round}"),
                false,
                "newer verification failed",
            ));
        }
        state.stall.tool_call_records.push(old);
        for restored in [false, true] {
            if restored {
                // Exercise the durable journal representation, including the
                // persisted round; live-only completion references are absent.
                state.stall.tool_call_records = serde_json::from_value(
                    serde_json::to_value(&state.stall.tool_call_records).unwrap(),
                )
                .unwrap();
            }
            let snapshot = direction_snapshot(&state).unwrap();
            assert!(!snapshot.support_basis_available);
            assert_eq!(
                snapshot
                    .evidence
                    .observations
                    .last()
                    .unwrap()
                    .result_excerpt,
                "newer verification failed"
            );
            prepare_direction(&mut state, snapshot);
            assert!(state.provider_adaptation.work_direction.cached.is_none());
        }
        // A genuinely later verification recovers without an append-order
        // anomaly permanently poisoning otherwise usable evidence.
        state.stall.tool_call_records.push(direction_execution(
            37,
            "fresh",
            true,
            "new verification passed",
        ));
        let snapshot = direction_snapshot(&state).unwrap();
        prepare_direction(&mut state, snapshot);
        assert!(state.provider_adaptation.work_direction.cached.is_some());
    }

    #[test]
    fn work_direction_replay_flood_cannot_evict_newer_failure_before_first_judgment() {
        let mut state = direction_state();
        let old = state.stall.tool_call_records.clone();
        state.stall.tool_call_records.push(direction_execution(
            4,
            "failure",
            false,
            "verification failed",
        ));
        for _ in 0..32 {
            state.stall.tool_call_records.extend(old.clone());
        }
        let snapshot = direction_snapshot(&state).unwrap();
        assert!(!snapshot.support_basis_available);
        assert_eq!(
            snapshot
                .evidence
                .observations
                .last()
                .unwrap()
                .result_excerpt,
            "verification failed"
        );
        let key = snapshot.key.clone();
        state.stall.tool_call_records.extend(old);
        assert_eq!(key, direction_snapshot(&state).unwrap().key);
        prepare_direction(&mut state, snapshot);
        assert!(state.provider_adaptation.work_direction.cached.is_none());
        state.stall.tool_call_records.push(direction_execution(
            5,
            "recovered",
            true,
            "new verification passed",
        ));
        let snapshot = direction_snapshot(&state).unwrap();
        prepare_direction(&mut state, snapshot);
        assert!(state.provider_adaptation.work_direction.cached.is_some());
    }

    #[test]
    fn work_direction_same_round_ambiguity_survives_candidate_eviction() {
        let mut state = direction_state();
        state.stall.tool_call_records.push(direction_execution(
            4,
            "a-failure",
            false,
            "verification failed",
        ));
        for index in 0..40 {
            state.stall.tool_call_records.push(direction_execution(
                4,
                &format!("z-success-{index:02}"),
                true,
                "observation succeeded",
            ));
        }
        let snapshot = direction_snapshot(&state).unwrap();
        assert!(
            snapshot
                .evidence
                .observations
                .iter()
                .all(|observation| observation.disposition == "executed_success")
        );
        assert!(!snapshot.support_basis_available);
        state.stall.tool_call_records.push(direction_execution(
            5,
            "verified",
            true,
            "verification passed after batch",
        ));
        assert!(direction_snapshot(&state).unwrap().support_basis_available);
    }

    #[test]
    fn work_direction_conflicting_replay_is_opaque_until_new_execution() {
        let mut state = direction_state();
        let before = direction_snapshot(&state).unwrap();
        let mut conflicting = state.stall.tool_call_records.last().unwrap().clone();
        conflicting.result_full =
            Some("different result under the same invocation identity".into());
        state.stall.tool_call_records.push(conflicting);
        let after = direction_snapshot(&state).unwrap();
        assert_ne!(before.key, after.key);
        assert!(!after.support_basis_available);
        state.stall.tool_call_records.push(direction_execution(
            4,
            "fresh",
            true,
            "fresh verification passed",
        ));
        assert!(direction_snapshot(&state).unwrap().support_basis_available);
    }

    #[test]
    #[ignore = "manual metadata-scan scaling measurement; no live services"]
    fn work_direction_metadata_scan_scaling() {
        for count in [100_u32, 10_000, 100_000] {
            let mut state = direction_state();
            state.stall.tool_call_records = (2..count + 2)
                .map(|round| direction_execution(round, &format!("call-{round}"), true, "verified"))
                .collect();
            let started = std::time::Instant::now();
            for _ in 0..10 {
                let snapshot = std::hint::black_box(direction_snapshot(&state).unwrap());
                assert_eq!(snapshot.evidence.observations.len(), 4);
            }
            eprintln!(
                "work_direction metadata records={count} mean_us={}",
                started.elapsed().as_micros() / 10
            );
        }
    }

    #[test]
    fn work_direction_opaque_replay_does_not_manufacture_conflict() {
        let mut state = direction_state();
        state.stall.tool_call_records[1].result_full = Some("x".repeat(20_000));
        let before = direction_snapshot(&state).unwrap();
        state
            .stall
            .tool_call_records
            .push(state.stall.tool_call_records[1].clone());
        let after = direction_snapshot(&state).unwrap();
        assert_eq!(before.key, after.key);
        assert!(!after.support_basis_available);
    }

    #[test]
    fn work_direction_artifact_custody_conflict_invalidates_cached_support() {
        use astra_services::session_journal::{
            ToolResultArtifactDescriptor, ToolResultDocumentKind,
        };
        for reverse in [false, true] {
            let mut state = direction_state();
            state.current_run_id = Some("run".into());
            let before = direction_snapshot(&state).unwrap();
            prepare_direction(&mut state, before);
            assert!(state.provider_adaptation.work_direction.cached.is_some());
            let original = state.stall.tool_call_records[1].clone();
            let mut artifact = original.clone();
            artifact.result_artifact = Some(ToolResultArtifactDescriptor {
                document_kind: ToolResultDocumentKind::Result,
                version: 1,
                call_id: "call-1".into(),
                run_id: "run".into(),
                byte_len: 20_000,
                content_sha256: "a".repeat(64),
            });
            state.stall.tool_call_records.truncate(1);
            state.stall.tool_call_records.extend(if reverse {
                [artifact, original]
            } else {
                [original, artifact]
            });
            let snapshot = direction_snapshot(&state).unwrap();
            assert!(!snapshot.support_basis_available);
            publish_work_direction_for_key(&mut state, Some(snapshot.key));
            assert!(state.provider_adaptation.work_direction.cached.is_none());
            state.stall.tool_call_records.push(direction_execution(
                4,
                "fresh",
                true,
                "new verification passed",
            ));
            assert!(direction_snapshot(&state).unwrap().support_basis_available);
        }
    }

    #[test]
    fn work_direction_same_round_order_is_unknown_even_without_parallel_flag() {
        for reverse in [false, true] {
            let mut state = direction_state();
            let mut batch = vec![
                direction_execution(4, "failure", false, "verification failed"),
                direction_execution(4, "success", true, "verification passed"),
            ];
            if reverse {
                batch.reverse();
            }
            state.stall.tool_call_records.extend(batch);
            let snapshot = direction_snapshot(&state).unwrap();
            assert!(!snapshot.support_basis_available);
            prepare_direction(&mut state, snapshot);
            assert!(state.provider_adaptation.work_direction.cached.is_none());
            state.stall.tool_call_records.push(direction_execution(
                5,
                "later",
                true,
                "verified after batch",
            ));
            let snapshot = direction_snapshot(&state).unwrap();
            assert!(snapshot.support_basis_available);
            // An exact duplicate does not manufacture a second execution.
            state
                .stall
                .tool_call_records
                .push(state.stall.tool_call_records.last().unwrap().clone());
            assert_eq!(snapshot.key, direction_snapshot(&state).unwrap().key);
        }
    }

    #[test]
    fn work_direction_hidden_same_round_failure_still_blocks_closure() {
        let mut state = direction_state();
        state.stall.tool_call_records.push(direction_execution(
            4,
            "failure",
            false,
            "verification failed",
        ));
        for index in 0..5 {
            state.stall.tool_call_records.push(direction_execution(
                4,
                &format!("success-{index}"),
                true,
                "observation succeeded",
            ));
        }
        let snapshot = direction_snapshot(&state).unwrap();
        assert!(
            snapshot
                .evidence
                .observations
                .iter()
                .all(|observation| observation.disposition == "executed_success")
        );
        assert!(snapshot.evidence.omitted_observations);
        assert!(!snapshot.support_basis_available);
        prepare_direction(&mut state, snapshot);
        assert!(state.provider_adaptation.work_direction.cached.is_none());
    }

    #[test]
    fn work_direction_assignment_floor_initialization_excludes_prior_turn() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.llm_rounds_completed = 0;
        state.stall.tool_call_records.push(direction_execution(
            100,
            "prior-turn",
            true,
            "old result",
        ));
        state.stall.begin_fresh_user_turn();
        assert_eq!(direction_assignment_first_round(&state), Some(0));

        state.stall.tool_call_records.push(direction_execution(
            1,
            "current-turn",
            true,
            "current result",
        ));
        assert_eq!(direction_assignment_first_round(&state), Some(2));

        state.stall.tool_call_records.push(direction_execution(
            11,
            "newest",
            true,
            "newest result",
        ));
        for _ in 0..40 {
            state.stall.tool_call_records.push(direction_execution(
                2,
                "old-replay",
                true,
                "old result",
            ));
        }
        assert_eq!(direction_assignment_first_round(&state), Some(12));

        // An invalid/restored boundary must abstain, not fall back to old rounds.
        state.stall.observation_reuse_record_floor = state.stall.tool_call_records.len() + 1;
        assert_eq!(direction_assignment_first_round(&state), None);
    }

    #[test]
    fn work_direction_round_order_respects_runtime_user_turn_boundary() {
        let mut state = direction_state();
        state.stall.tool_call_records.push(direction_execution(
            100,
            "prior-turn",
            true,
            "old verification passed",
        ));
        state.stall.begin_fresh_user_turn();
        state.stall.tool_call_records.push(direction_execution(
            2,
            "repair",
            true,
            "repair applied",
        ));
        state.stall.tool_call_records.push(direction_execution(
            3,
            "failure",
            false,
            "new verification failed",
        ));
        let snapshot = direction_snapshot(&state).unwrap();
        assert_eq!(snapshot.evidence.observations.len(), 2);
        assert!(!snapshot.evidence.omitted_observations);
        assert!(!snapshot.support_basis_available);
        assert_eq!(
            snapshot
                .evidence
                .observations
                .last()
                .unwrap()
                .result_excerpt,
            "new verification failed"
        );
        prepare_direction(&mut state, snapshot);
        assert!(state.provider_adaptation.work_direction.cached.is_none());
        state.stall.tool_call_records.push(direction_execution(
            4,
            "fresh",
            true,
            "new verification passed",
        ));
        assert!(direction_snapshot(&state).unwrap().support_basis_available);
    }

    #[test]
    fn work_direction_original_continuation_capture_does_not_restore_missing_journal() {
        use super::super::host::OriginalLoopExecutionFacts;
        let mut state = direction_state();
        let snapshot = direction_snapshot(&state).unwrap();
        prepare_direction(&mut state, snapshot);
        let captured = OriginalLoopExecutionFacts::capture(&state).unwrap();
        let restored: OriginalLoopExecutionFacts =
            serde_json::from_value(serde_json::to_value(captured).unwrap()).unwrap();
        let mut resumed = crate::turn::agentic_loop::host::make_test_loop_state();
        resumed.provider_adaptation = restored.provider_adaptation;
        resumed.current_run_owner_generation = Some(2);
        assert_eq!(resumed.provider_adaptation.work_direction.first_round, 2);
        assert!(resumed.stall.tool_call_records.is_empty());
        assert!(direction_snapshot(&resumed).is_none());
        publish_work_direction_for_key(&mut resumed, None);
        assert!(resumed.provider_adaptation.work_direction.cached.is_none());
    }

    #[test]
    fn work_direction_latest_observations_preserve_recovery_order() {
        let mut state = direction_state();
        for index in 0..4 {
            state.stall.tool_call_records.push(direction_execution(
                4 + index,
                &format!("earlier-{index}"),
                true,
                "earlier observation",
            ));
        }
        let before = direction_snapshot(&state).unwrap().key;
        for (round, id, ok, text) in [
            (8, "failure", false, "validation failed"),
            (9, "repair", true, "repair applied"),
            (10, "verification", true, "verification passed"),
        ] {
            state
                .stall
                .tool_call_records
                .push(direction_execution(round, id, ok, text));
        }
        let snapshot = direction_snapshot(&state).unwrap();
        assert_ne!(snapshot.key, before);
        assert_eq!(
            snapshot
                .evidence
                .observations
                .iter()
                .map(|o| o.result_excerpt.as_str())
                .collect::<Vec<_>>(),
            [
                "earlier observation",
                "validation failed",
                "repair applied",
                "verification passed"
            ]
        );
        assert_eq!(
            snapshot.evidence.observations[1].disposition,
            "executed_failure"
        );
        assert!(snapshot.evidence.omitted_observations);
        // Replayed older invocations must not displace current recovery facts.
        state
            .stall
            .tool_call_records
            .push(state.stall.tool_call_records[2].clone());
        assert_eq!(direction_snapshot(&state).unwrap().key, snapshot.key);
        prepare_direction(&mut state, snapshot);
        let (_, advice) = state
            .provider_adaptation
            .work_direction
            .cached
            .as_ref()
            .unwrap();
        assert_eq!(advice["direction"], "prepare_settlement");
        assert_eq!(advice["omitted_observations"], true);
        assert_eq!(advice["settlement_readiness"], "unknown");
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
    }

    #[test]
    fn work_direction_large_documents_do_not_poison_recovery() {
        let mut state = direction_state();
        let mut failure = direction_execution(4, "failure", false, &"界".repeat(20_000));
        failure.args_full = Some("x".repeat(20_000));
        failure.result_preview = Some("a prefix must not stand in for complete evidence".into());
        state.stall.tool_call_records.push(failure);
        state.stall.tool_call_records.push(direction_execution(
            5,
            "repair",
            true,
            "repair applied",
        ));
        state.stall.tool_call_records.push(direction_execution(
            6,
            "verification",
            true,
            "verification passed",
        ));
        let snapshot = direction_snapshot(&state).unwrap();
        assert_eq!(snapshot.evidence.observations.len(), 4);
        assert_eq!(
            snapshot.evidence.observations[1].disposition,
            "executed_failure"
        );
        assert!(
            snapshot.evidence.observations[1]
                .result_excerpt
                .contains("outcome only")
        );
        assert_eq!(
            snapshot.evidence.observations[2].result_excerpt,
            "repair applied"
        );
        assert_eq!(
            snapshot.evidence.observations[3].result_excerpt,
            "verification passed"
        );
        assert!(snapshot.evidence.omitted_observations);
        assert!(snapshot.support_basis_available);
        prepare_direction(&mut state, snapshot);
        assert!(state.provider_adaptation.work_direction.cached.is_some());
        // No filler calls were needed to age out the opaque failure. But a
        // newest result with unavailable content must invalidate that advice.
        state.stall.tool_call_records.push(direction_execution(
            7,
            "newest",
            true,
            &"x".repeat(20_000),
        ));
        let opaque = direction_snapshot(&state).unwrap();
        assert!(!opaque.support_basis_available);
        prepare_direction(&mut state, opaque);
        assert!(state.provider_adaptation.work_direction.cached.is_none());
        state
            .stall
            .tool_call_records
            .last_mut()
            .unwrap()
            .result_full = Some("validation failed again".into());
        state.stall.tool_call_records.last_mut().unwrap().ok = false;
        let failed = direction_snapshot(&state).unwrap();
        assert!(!failed.support_basis_available);
        prepare_direction(&mut state, failed);
        assert!(state.provider_adaptation.work_direction.cached.is_none());
    }

    #[test]
    fn work_direction_display_prefixes_and_artifacts_do_not_mint_support() {
        use astra_services::session_journal::{
            ToolResultArtifactDescriptor, ToolResultDocumentKind,
        };
        let mut state = direction_state();
        state.current_run_id = Some("run".into());
        state.stall.tool_call_records[1].result_full = None;
        state.stall.tool_call_records[1].result_preview = Some("verification passed".into());
        let preview = direction_snapshot(&state).unwrap();
        assert!(preview.evidence.omitted_observations);
        assert!(!preview.support_basis_available);
        prepare_direction(&mut state, preview);
        assert!(state.provider_adaptation.work_direction.cached.is_none());
        state.stall.tool_call_records[1].result_full = Some("x".repeat(769));
        assert!(!direction_snapshot(&state).unwrap().support_basis_available);
        state.stall.tool_call_records[1].result_full = Some("artifact display envelope".into());
        state.stall.tool_call_records[1].result_artifact = Some(ToolResultArtifactDescriptor {
            document_kind: ToolResultDocumentKind::Result,
            version: 1,
            call_id: "call-1".into(),
            run_id: "run".into(),
            byte_len: 20_000,
            content_sha256: "a".repeat(64),
        });
        let artifact = direction_snapshot(&state).unwrap();
        assert!(!artifact.support_basis_available);
        state.stall.tool_call_records[1]
            .result_artifact
            .as_mut()
            .unwrap()
            .content_sha256 = "b".repeat(64);
        let changed = direction_snapshot(&state).unwrap();
        assert_eq!(artifact.evidence, changed.evidence);
        assert_ne!(artifact.key, changed.key);
        prepare_direction(&mut state, changed);
        assert!(state.provider_adaptation.work_direction.cached.is_none());
    }

    #[test]
    fn work_direction_lifecycle_requests_do_not_erase_attempt_observations() {
        use astra_services::session_journal::ToolCallDisposition;
        for disposition in [ToolCallDisposition::Rejected, ToolCallDisposition::Executed] {
            let mut state = direction_state();
            state.stall.tool_call_records.push(direction_execution(
                4,
                "failure",
                false,
                "validation failed",
            ));
            let before = direction_snapshot(&state).unwrap();
            state.stall.tool_call_records.push(ToolCallRecord {
                name: "settle_work_item".into(),
                disposition: Some(disposition),
                ..direction_execution(5, "lifecycle", false, "settlement rejected")
            });
            // Even a successful lifecycle call may only resume the same attempt.
            state.stall.tool_call_records.push(ToolCallRecord {
                name: "run_next_work_item".into(),
                ..direction_execution(6, "resume", true, "assigned existing attempt")
            });
            assert_eq!(direction_snapshot(&state).unwrap().key, before.key);
            state.stall.tool_call_records.push(direction_execution(
                7,
                "repair",
                true,
                "repair applied",
            ));
            state.stall.tool_call_records.push(direction_execution(
                8,
                "verification",
                true,
                "verification passed",
            ));
            let snapshot = direction_snapshot(&state).unwrap();
            assert_eq!(
                snapshot.evidence.observations[1].result_excerpt,
                "validation failed"
            );
            assert_eq!(
                snapshot.evidence.observations[2].result_excerpt,
                "repair applied"
            );
            assert_eq!(
                snapshot.evidence.observations[3].result_excerpt,
                "verification passed"
            );
            assert!(snapshot.evidence.omitted_observations);
            prepare_direction(&mut state, snapshot);
            assert!(state.provider_adaptation.work_direction.cached.is_some());
        }
    }

    #[test]
    fn work_direction_owner_change_retains_available_same_attempt_evidence_but_fences_advice() {
        let binding = astra_services::runs::WorkRuntimeBindingRequest {
            work_id: "work".into(),
            branch_id: "branch".into(),
            item: Some(astra_services::runs::WorkItemRuntimeBindingRequest {
                item_id: "item".into(),
                item_revision: 2,
                attempt_id: "attempt".into(),
            }),
        };
        let assignment = direction_assignment_key(&binding, Some("run"), "objective", "expected");
        let snapshot = |state: &AgenticLoopState, key: &str| {
            work_direction_snapshot_for_assignment(
                state,
                (key.into(), "objective".into(), "expected".into()),
            )
        };
        let mut state = direction_state();
        state.current_run_id = Some("run".into());
        state.current_run_owner_generation = Some(1);
        state.provider_adaptation.work_direction.binding_key = Some(assignment.clone());
        let before = snapshot(&state, &assignment).unwrap();
        state.provider_adaptation.work_direction.cached = Some((
            before.key.clone(),
            serde_json::json!({"direction": "focused_verification"}),
        ));
        // This tests ownership fencing when evidence is available, not a
        // promise that the continuation protocol restores the journal.
        state.current_run_owner_generation = Some(2);
        let after = snapshot(&state, &assignment).unwrap();
        assert_eq!(before.evidence, after.evidence);
        assert_ne!(before.key, after.key);
        assert_eq!(state.provider_adaptation.work_direction.first_round, 2);
        publish_work_direction_for_key(&mut state, Some(after.key));
        assert!(state.provider_adaptation.work_direction.cached.is_none());

        // Neither a successor attempt, an item revision, nor another executor
        // run may reuse this floor/evidence even when task text is unchanged.
        let mut successor = binding.clone();
        successor.item.as_mut().unwrap().attempt_id = "successor".into();
        let mut revised = binding.clone();
        revised.item.as_mut().unwrap().item_revision += 1;
        for changed in [
            direction_assignment_key(&successor, Some("run"), "objective", "expected"),
            direction_assignment_key(&revised, Some("run"), "objective", "expected"),
            direction_assignment_key(&binding, Some("another-run"), "objective", "expected"),
        ] {
            assert_ne!(assignment, changed);
            assert!(snapshot(&state, &changed).is_none());
        }
    }

    #[test]
    fn work_direction_repeated_mutation_invalidates_verified_snapshot() {
        let mut state = direction_state();
        state.stall.tool_call_records[0].name = "write_file".into();
        state.stall.tool_call_records[1].result_full = Some("verified".into());
        state.stall.tool_call_records.insert(
            0,
            ToolCallRecord {
                round: Some(1),
                ..successful("start_work")
            },
        );
        let first = direction_snapshot(&state).unwrap();
        let key = first.key.clone();
        prepare_direction(&mut state, first);
        assert!(state.provider_adaptation.work_direction.cached.is_some());
        let mut mutation = state.stall.tool_call_records[1].clone();
        mutation.round = Some(4);
        mutation.tool_call_id = Some("second-mutation".into());
        state.stall.tool_call_records.push(mutation.clone());
        let next = direction_snapshot(&state).unwrap();
        assert_ne!(next.key, key);
        assert_eq!(
            next.evidence
                .observations
                .iter()
                .map(|o| o.tool.as_str())
                .collect::<Vec<_>>(),
            ["write_file", "read_file", "write_file"]
        );
        publish_work_direction_for_key(&mut state, Some(next.key.clone()));
        assert!(state.provider_adaptation.work_direction.cached.is_none());
        state.stall.tool_call_records.push(mutation);
        assert_eq!(direction_snapshot(&state).unwrap().key, next.key);
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
    }

    #[test]
    fn work_direction_new_guidance_is_bounded_without_original_task() {
        let mut state = direction_state();
        let original_key = direction_snapshot(&state).unwrap().key;
        state.user_intent = "long original task".repeat(10_000);
        state.message = state.user_intent.clone();
        let snapshot = direction_snapshot(&state).unwrap();
        assert_eq!(snapshot.key, original_key);
        assert_eq!(snapshot.evidence.current_guidance, None);
        apply_direction_guidance(&mut state, "Do not mutate");
        let snapshot = direction_snapshot(&state).unwrap();
        assert_ne!(snapshot.key, original_key);
        let request = astra_services::work_direction_judgment::work_direction_judgment_request(
            &snapshot.evidence,
        );
        assert!(
            serde_json::to_string(&request)
                .unwrap()
                .contains("Do not mutate")
        );
        apply_direction_guidance(&mut state, &"界".repeat(1024));
        assert!(direction_snapshot(&state).is_none());
        state = direction_state();
        apply_direction_guidance(&mut state, &"界".repeat(1024));
        assert!(direction_snapshot(&state).is_some());
        apply_direction_guidance(&mut state, "x");
        assert!(direction_snapshot(&state).is_none());
        state = direction_state();
        for _ in 0..17 {
            apply_direction_guidance(&mut state, "x");
        }
        assert!(direction_snapshot(&state).is_none());
    }

    #[test]
    fn work_direction_missing_round_prevents_support_but_lifecycle_name_is_not_custody() {
        let mut state = direction_state();
        let snapshot = direction_snapshot(&state).unwrap();
        assert!(!snapshot.evidence.omitted_observations);
        prepare_direction(&mut state, snapshot);
        assert!(state.provider_adaptation.work_direction.cached.is_some());
        state.stall.tool_call_records.insert(
            0,
            ToolCallRecord {
                round: Some(1),
                ..successful("start_work")
            },
        );
        let snapshot = direction_snapshot(&state).unwrap();
        // A successful lifecycle request is not a typed custody boundary.
        assert!(!snapshot.evidence.omitted_observations);
        prepare_direction(&mut state, snapshot);
        assert!(state.provider_adaptation.work_direction.cached.is_some());
        state.stall.tool_call_records.push(ToolCallRecord {
            result_full: Some("unattributed mutation".into()),
            ..successful("write_file")
        });
        let snapshot = direction_snapshot(&state).unwrap();
        assert!(snapshot.evidence.omitted_observations);
        prepare_direction(&mut state, snapshot);
        assert!(state.provider_adaptation.work_direction.cached.is_none());
    }

    #[test]
    fn work_direction_round_floor_excludes_old_evidence_without_claiming_custody() {
        let mut state = direction_state();
        let current = std::mem::take(&mut state.stall.tool_call_records);
        state.stall.tool_call_records = vec![
            ToolCallRecord {
                round: Some(0),
                result_full: Some("old assignment".into()),
                ..successful("read_file")
            };
            40
        ];
        state.stall.tool_call_records.push(ToolCallRecord {
            round: Some(1),
            ..successful("settle_work_item")
        });
        state.stall.tool_call_records.extend(current);
        let snapshot = direction_snapshot(&state).unwrap();
        assert_eq!(snapshot.evidence.observations.len(), 2);
        assert!(!snapshot.evidence.omitted_observations);
        let request = astra_services::work_direction_judgment::work_direction_judgment_request(
            &snapshot.evidence,
        );
        assert!(matches!(
            astra_services::work_direction_judgment::parse_work_direction_judgment(
                &request,
                r#"{"true":["supported"],"uncertain":[]}"#,
            ),
            astra_services::work_direction_judgment::WorkDirectionSemanticResult::Decision {
                decision: astra_services::work_direction_judgment::WorkDirectionDecision {
                    direction:
                        astra_services::work_direction_judgment::WorkDirection::PrepareSettlement,
                    ..
                },
                ..
            }
        ));

        // Exact replay introduces no projection loss: selection occurs before
        // truncation. Tool names still do not establish evidence custody.
        let repeated = state.stall.tool_call_records.last().unwrap().clone();
        state
            .stall
            .tool_call_records
            .splice(40..41, std::iter::repeat_n(repeated, 32));
        assert!(
            !direction_snapshot(&state)
                .unwrap()
                .evidence
                .omitted_observations
        );
    }

    #[tokio::test]
    async fn work_direction_no_judge_preserves_primary_path() {
        use super::super::host::{AgenticLoopHost, WorkDirectionOutcome};
        let mut host = super::super::host::tests::MockHost::new(vec![]);
        let mut state = direction_state();
        let evidence = direction_snapshot(&state).unwrap().evidence;
        assert!(matches!(
            host.judge_work_direction(&state, &evidence).await,
            WorkDirectionOutcome::Disabled
        ));
        let snapshot = direction_snapshot(&state).unwrap();
        retain_work_direction_outcome(&mut state, snapshot, WorkDirectionOutcome::Disabled);
        assert!(state.provider_adaptation.work_direction.disabled);
        assert!(direction_snapshot(&state).is_none());
        judge_work_direction(&mut host, &mut state).await;
        assert!(
            state
                .provider_adaptation
                .work_direction
                .attempted
                .is_empty()
        );
        assert!(state.restricted_tools.is_empty());
        assert!(
            state
                .hooks
                .completion_settlement
                .completion_action_window
                .is_none()
        );
    }

    fn successful_observation(name: &str, args: serde_json::Value) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            args_full: Some(args.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn work_evidence_context_projects_owned_attempt_without_readiness_authority() {
        use crate::server::runtime_tool_executor::{PrimaryWorkHandoff, PrimaryWorkUnavailable};
        let binding = astra_services::runs::WorkRuntimeBindingRequest {
            work_id: "work".into(),
            branch_id: "branch".into(),
            item: Some(astra_services::runs::WorkItemRuntimeBindingRequest {
                item_id: "item".into(),
                item_revision: 2,
                attempt_id: "attempt".into(),
            }),
        };
        // This pure projection receives the executor's validated handoff and
        // needs neither a database nor any semantic judge.
        let snapshot = work_evidence_context(
            PrimaryWorkHandoff::Active { binding },
            &[successful("read_file")],
        )
        .unwrap();
        assert_eq!(snapshot["binding"]["item"]["attempt_id"], "attempt");
        assert_eq!(snapshot["binding"]["item"]["item_revision"], 2);
        assert_eq!(snapshot["settlement_readiness"], "unknown");
        assert_eq!(snapshot["authority"], "observation_only");
        for handoff in [
            PrimaryWorkHandoff::NoBinding,
            PrimaryWorkHandoff::BindingOnly {
                binding: astra_services::runs::WorkRuntimeBindingRequest {
                    work_id: "work".into(),
                    branch_id: "branch".into(),
                    item: None,
                },
            },
            PrimaryWorkHandoff::Unavailable {
                reason: PrimaryWorkUnavailable::BindingMismatch,
            },
        ] {
            assert!(work_evidence_context(handoff, &[successful("read_file")]).is_none());
        }
    }

    #[test]
    fn work_observations_exclude_previous_assignment_and_result_text() {
        let records = vec![
            successful("old_evidence"),
            successful("settle_work_item"),
            ToolCallRecord {
                tool_call_id: Some("current-call".into()),
                result_full: Some("untrusted result: ready to settle".into()),
                ..successful("read_file")
            },
        ];
        let snapshot = bounded_work_observations(&records);
        assert_eq!(snapshot["calls"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["calls"][0]["tool_call_id"], "current-call");
        assert_eq!(snapshot["lifecycle_boundary_seen"], true);
        assert!(!snapshot.to_string().contains("ready to settle"));
    }

    #[test]
    fn work_observations_retain_failures_and_nonexecution_without_claiming_evidence() {
        use astra_services::session_journal::ToolCallDisposition;
        let records = vec![
            ToolCallRecord {
                ok: false,
                ..successful("read_file")
            },
            ToolCallRecord {
                disposition: Some(ToolCallDisposition::Rejected),
                ..successful("bash")
            },
            successful("inspect_work_plan"),
        ];
        let snapshot = bounded_work_observations(&records);
        assert_eq!(snapshot["calls"][0]["ok"], false);
        assert_eq!(snapshot["calls"][1]["disposition"], "rejected");
        assert_eq!(snapshot["calls"][2]["tool"], "inspect_work_plan");
        assert_eq!(snapshot["scope"], "recent_journal_suffix_not_attempt_proof");
        assert_eq!(snapshot["lifecycle_boundary_seen"], false);
    }

    #[test]
    fn work_observations_bound_window_and_never_truncate_call_identity() {
        let mut records = vec![successful("read_file"); 100];
        records.last_mut().unwrap().tool_call_id = Some("x".repeat(129));
        let snapshot = bounded_work_observations(&records);
        assert_eq!(snapshot["calls"].as_array().unwrap().len(), 8);
        assert_eq!(snapshot["window_truncated"], true);
        assert!(snapshot["calls"][7]["tool_call_id"].is_null());
        let empty = bounded_work_observations(&[]);
        assert_eq!(empty["lifecycle_boundary_seen"], false);
        assert_eq!(empty["window_truncated"], false);
    }

    #[test]
    fn work_evidence_context_clears_stale_snapshot_without_executor_or_judge() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.push_volatile_payload(
            VolatileKind::WorkEvidenceContext,
            serde_json::json!({"binding": "old-attempt"}),
        );
        state.push_volatile(VolatileKind::BehaviorAdvisory, "unrelated advisory");
        refresh_work_evidence_context(&mut state);
        assert_eq!(state.volatile_pending.len(), 1);
        assert_eq!(
            state.volatile_pending[0].kind,
            VolatileKind::BehaviorAdvisory
        );
    }

    #[test]
    fn work_evidence_counter_is_bounded_to_current_lifecycle_item() {
        let mut records = (0..20).map(|_| successful("read_file")).collect::<Vec<_>>();
        records.push(successful("settle_work_item"));
        records.extend((0..3).map(|_| successful("grep")));

        assert_eq!(bounded_read_only_work_evidence_calls(&records), Some(3));
    }

    #[test]
    fn work_evidence_counter_reaches_threshold_without_text_classification() {
        let records = (0..WORK_EVIDENCE_REASSESS_CALLS)
            .map(|index| successful(if index % 2 == 0 { "grep" } else { "read_file" }))
            .collect::<Vec<_>>();

        assert_eq!(
            bounded_read_only_work_evidence_calls(&records),
            Some(WORK_EVIDENCE_REASSESS_CALLS)
        );
    }

    #[test]
    fn work_evidence_counter_defers_to_mutating_execution_paths() {
        let mut records = (0..WORK_EVIDENCE_REASSESS_CALLS)
            .map(|_| successful("read_file"))
            .collect::<Vec<_>>();
        records.push(ToolCallRecord {
            name: "apply_patch".to_string(),
            ok: true,
            args_full: Some(r#"{"patch":"*** Begin Patch"}"#.to_string()),
            ..Default::default()
        });

        assert_eq!(bounded_read_only_work_evidence_calls(&records), None);
    }

    #[test]
    fn observation_reuse_normalizes_typed_defaults_and_aliases() {
        let records = vec![
            successful_observation(
                "introspect",
                serde_json::json!({"topic": "runtime", "facet": "session"}),
            ),
            successful_observation("introspect", serde_json::json!({})),
        ];

        assert!(matches!(
            repeated_observation_request(&records),
            Some(ObservationRequestKey::Introspect { .. })
        ));
    }

    #[test]
    fn observation_reuse_stops_at_a_tool_state_transition() {
        let records = vec![
            successful_observation("introspect", serde_json::json!({})),
            successful("read_file"),
            successful_observation("introspect", serde_json::json!({})),
        ];

        assert_eq!(repeated_observation_request(&records), None);
    }

    #[test]
    fn observation_reuse_keeps_distinct_facets_and_reflection_questions() {
        let records = vec![
            successful_observation("introspect", serde_json::json!({"facet": "errors"})),
            successful_observation("introspect", serde_json::json!({"facet": "recent"})),
            successful_observation(
                "reflect",
                serde_json::json!({"facet": "errors", "question": "what changed?"}),
            ),
        ];

        assert_eq!(repeated_observation_request(&records), None);
    }

    #[test]
    fn observation_artifact_recovery_is_not_a_diagnostic_reuse() {
        let records = vec![
            successful_observation("introspect", serde_json::json!({"artifact": "artifact-1"})),
            successful_observation("introspect", serde_json::json!({"artifact": "artifact-1"})),
        ];

        assert_eq!(repeated_observation_request(&records), None);
    }

    #[test]
    fn observation_reuse_is_one_shot_advisory_not_tool_restriction() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.stall.tool_call_records = vec![
            successful_observation("introspect", serde_json::json!({})),
            successful_observation("introspect", serde_json::json!({})),
        ];
        state.restricted_tools.insert("bash".to_string());
        let cfg = GuardConfig {
            parallel_batching_force_streak: 8,
            cache_waste_threshold: 3,
        };

        assert!(matches!(
            check_observation_reuse(&mut state, &cfg),
            GuardOutcome::Advisory { .. }
        ));
        assert!(state.stall.observation_reuse_advisory_emitted);
        assert_eq!(state.restricted_tools, HashSet::from(["bash".to_string()]));
        assert!(matches!(
            check_observation_reuse(&mut state, &cfg),
            GuardOutcome::Pass
        ));
    }

    #[test]
    fn observation_reuse_does_not_cross_a_user_turn_boundary() {
        let mut state = crate::turn::agentic_loop::host::make_test_loop_state();
        state.stall.tool_call_records = vec![
            successful_observation("introspect", serde_json::json!({})),
            successful_observation("introspect", serde_json::json!({})),
        ];
        let cfg = GuardConfig {
            parallel_batching_force_streak: 8,
            cache_waste_threshold: 3,
        };
        assert!(matches!(
            check_observation_reuse(&mut state, &cfg),
            GuardOutcome::Advisory { .. }
        ));

        state.stall.begin_fresh_user_turn();
        assert!(matches!(
            check_observation_reuse(&mut state, &cfg),
            GuardOutcome::Pass
        ));
    }
}

/// Detect wasteful cache reads (repeated reads hitting the stale-cache
/// guard without follow-up writes) and surface advisory evidence.
///
/// Defers to redundant_reads when both would fire on the same round, and
/// to the same stronger interventions as `check_redundant_reads`.
fn check_cache_waste(state: &mut AgenticLoopState, cfg: &GuardConfig) -> GuardOutcome {
    if state.stall.cache_waste_advisory_emitted
        || !should_emit_cache_waste_advisory(state, cfg.cache_waste_threshold)
    {
        return GuardOutcome::Pass;
    }
    let wasteful = cache_wasteful_tools(state, cfg.cache_waste_threshold);
    if wasteful.is_empty() {
        return GuardOutcome::Pass;
    }
    state.stall.cache_waste_advisory_emitted = true;
    let msg = cache_waste_advisory_message(&wasteful, &state.message);
    tracing::warn!(
        target: "astra::loop_guard",
        tier = "cache_waste_advisory",
        round = state.llm_rounds_completed,
        tools = ?wasteful,
        threshold = cfg.cache_waste_threshold,
        "behavior advisory observed"
    );
    let tool_list = wasteful
        .iter()
        .map(|(tool, count)| format!("{tool} ({count}x)"))
        .collect::<Vec<_>>()
        .join(", ");
    GuardOutcome::Advisory {
        message: msg,
        kind: VolatileKind::BehaviorAdvisory,
        hint: Some(format!(
            "↻ repeated cached tool calls on [{tool_list}]; adding reuse advisory…"
        )),
    }
}
