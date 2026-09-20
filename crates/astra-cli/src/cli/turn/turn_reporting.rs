//! Final turn reporting, status lines, and summary rendering.

use std::time::Instant;

use crate::cli::session::session_state::SessionState;
use crate::cli::stream::streaming_types::StreamResult;
use astra_services::session_journal;
use astra_turn_core::evaluation::TurnEvaluation;
use crossterm::style::Stylize;

pub(crate) fn compact_token_count(tokens: u64) -> String {
    if tokens > 1000 {
        format!("{:.1}k", tokens as f64 / 1000.0)
    } else {
        format!("{tokens}")
    }
}

pub(crate) fn cache_hit_percentage(
    prompt_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> f64 {
    let total_input = astra_turn_types::NormalizedPromptCacheUsage::new(
        prompt_tokens,
        cache_read_tokens,
        cache_creation_tokens,
    )
    .total_input_tokens();
    cache_read_tokens as f64 / total_input.max(1) as f64 * 100.0
}

/// Build a compact tool-call summary for cross-turn context continuity.
///
/// Appended to the assistant text in history so the next turn's prompt
/// contains file paths and tool outcomes from the previous turn — without
/// storing the full tool_call / tool_result messages.
pub(crate) fn build_turn_tool_summary(records: &[session_journal::ToolCallRecord]) -> String {
    if records.is_empty() {
        return String::new();
    }

    let mut files = Vec::new();
    let mut failed = Vec::new();
    for record in records {
        if !record.was_executed() {
            continue;
        }
        if record.ok
            && let Some(file_path) = record.file_path.as_deref()
            && !files.contains(&file_path)
        {
            files.push(file_path);
        }
        if !record.ok && !failed.contains(&record.name.as_str()) {
            failed.push(record.name.as_str());
        }
    }

    let mut parts = Vec::new();
    if !files.is_empty() {
        if files.len() <= 15 {
            parts.push(format!("files: {}", files.join(", ")));
        } else {
            parts.push(format!(
                "files: {} (+{} more)",
                files[..15].join(", "),
                files.len() - 15
            ));
        }
    }
    if !failed.is_empty() {
        parts.push(format!("failed: {}", failed.join(", ")));
    }
    parts.push(format!(
        "tool_calls: {}",
        records
            .iter()
            .filter(|record| record.was_executed())
            .count()
    ));

    format!("\n\n[Turn context: {}]", parts.join(" | "))
}

/// Build the text stored in history: assistant response + optional tool summary.
pub(crate) fn build_history_text(
    full_text: &str,
    records: &[session_journal::ToolCallRecord],
) -> String {
    let summary = build_turn_tool_summary(records);
    if summary.is_empty() {
        return full_text.to_string();
    }
    format!("{full_text}{summary}")
}

pub(crate) fn print_turn_status_line(
    state: &SessionState,
    result: &StreamResult,
    evaluation: Option<&TurnEvaluation>,
    turn_start: Instant,
) {
    if state.tui_render_policy.is_some() {
        return;
    }
    let elapsed = turn_start.elapsed();
    let elapsed_str = if elapsed.as_secs() >= 60 {
        format!("{}m{:.0}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    };

    let turn_cost = crate::cli::slash::slash_stats::cost_for_tokens(
        result.prompt_tokens,
        result.completion_tokens,
        result.cache_read_tokens,
        result.cache_creation_tokens,
        &state.cached_pricing,
    );

    let mut parts = Vec::new();
    if let Some(model) = result
        .usage_attribution
        .primary_model
        .as_ref()
        .or(state.model.as_ref())
    {
        parts.push(format!("model:{model}"));
    }
    parts.extend(usage_status_parts(result));
    if result.usage_attribution.has_auxiliary() {
        parts.push(format!(
            "aux:{}",
            result
                .usage_attribution
                .auxiliary_summary()
                .unwrap_or_else(|| "usage unavailable".to_string())
        ));
    }
    if turn_cost > 0.0 {
        let cost = crate::cli::slash::slash_stats::format_cost(turn_cost);
        if result.usage_attribution.has_auxiliary()
            || result.usage_attribution.primary.is_none()
                && (result.prompt_tokens > 0
                    || result.completion_tokens > 0
                    || result.cache_read_tokens > 0
                    || result.cache_creation_tokens > 0)
        {
            parts.push(format!("overall {cost}"));
        } else {
            parts.push(cost);
        }
    }
    parts.push(elapsed_str);
    if let Some(ttft) = result.ttft_ms
        && ttft > 0
    {
        parts.push(format!("ttft:{ttft}ms"));
    }
    if result.tool_calls_count > 0 {
        parts.push(format!(
            "{} tool{}",
            result.tool_calls_count,
            if result.tool_calls_count == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    eprintln!("{}", format!("  ─ {} ─", parts.join(" │ ")).dim());

    let session_cost = state.total_session_cost + turn_cost;
    if session_cost > 0.0 && state.turn > 0 {
        eprintln!(
            "{}",
            format!(
                "  session: {}",
                crate::cli::slash::slash_stats::format_cost(session_cost)
            )
            .dim()
        );
    }
    if let Some(error) = state
        .session_persistence_error
        .as_deref()
        .map(str::trim)
        .filter(|error| !error.is_empty())
    {
        eprintln!(
            "{}",
            format!("  ⚠ Session persistence degraded: {error}").yellow()
        );
    }

    if let Some(notice) = interruption_status_notice(result) {
        eprintln!("{}", format!("  ⚠ {notice}").yellow());
    }
    if let Some(notice) =
        evaluation.and_then(|evaluation| evaluation_status_notice_for_result(result, evaluation))
    {
        eprintln!("{}", format!("  ⚠ {notice}").yellow());
    }
    print_context_window_warning(result.budget_pressure);

    let width = crossterm::terminal::size()
        .map(|(columns, _)| columns as usize)
        .unwrap_or(80);
    eprintln!("{}", "─".repeat(width.min(72)).dim());
}

fn usage_status_parts(result: &StreamResult) -> Vec<String> {
    let auxiliary_present = result.usage_attribution.has_auxiliary();
    let primary_usage = result.usage_attribution.primary;
    let has_overall_values = result.prompt_tokens > 0
        || result.completion_tokens > 0
        || result.cache_read_tokens > 0
        || result.cache_creation_tokens > 0;
    let unclassified_overall = primary_usage.is_none() && !auxiliary_present && has_overall_values;
    let display_usage = primary_usage.or_else(|| {
        unclassified_overall.then_some(crate::cli::stream::streaming_types::AttributedTokenUsage {
            fresh_input_tokens: Some(result.prompt_tokens),
            cache_read_tokens: Some(result.cache_read_tokens),
            cache_creation_tokens: Some(result.cache_creation_tokens),
            output_tokens: Some(result.completion_tokens),
        })
    });
    let usage_partial = unclassified_overall
        || primary_usage.is_some_and(|_| !result.usage_attribution.primary_complete)
        || auxiliary_present && primary_usage.is_none();
    let usage_label = if primary_usage.is_some() {
        "main"
    } else if unclassified_overall {
        "overall"
    } else {
        "main"
    };
    let known_input = display_usage.map(|usage| {
        usage
            .fresh_input_tokens
            .unwrap_or(0)
            .saturating_add(usage.cache_read_tokens.unwrap_or(0))
            .saturating_add(usage.cache_creation_tokens.unwrap_or(0))
    });
    let known_total = display_usage.map(|usage| usage.known_total_tokens());
    let tokens_str = known_total
        .map(compact_token_count)
        .unwrap_or_else(|| "unavailable".to_string());
    let prompt_str = format_known_lane(
        known_input,
        display_usage.is_some_and(|usage| {
            usage.fresh_input_tokens.is_some()
                && usage.cache_read_tokens.is_some()
                && usage.cache_creation_tokens.is_some()
        }),
    );
    let completion_str = format_known_lane(
        display_usage.and_then(|usage| usage.output_tokens),
        display_usage.is_some_and(|usage| usage.output_tokens.is_some()),
    );

    let mut parts = vec![format!(
        "{usage_label} tokens:{tokens_str} (↑{prompt_str} ↓{completion_str})"
    )];
    if usage_partial {
        parts.push("usage not fully attributed".to_string());
    }

    // A cache percentage is an exact ratio only when Explain Analyze proved
    // that the primary attempt set and all input lanes were captured.
    if result.usage_attribution.primary_complete
        && let Some(usage) = primary_usage
        && let (Some(fresh_input), Some(cache_read), Some(cache_creation)) = (
            usage.fresh_input_tokens,
            usage.cache_read_tokens,
            usage.cache_creation_tokens,
        )
        && cache_read > 0
    {
        let cache_pct = cache_hit_percentage(fresh_input, cache_read, cache_creation);
        parts.push(format!("cache:{cache_pct:.0}%"));
    }

    parts
}

fn format_known_lane(value: Option<u64>, exact: bool) -> String {
    match (value, exact) {
        (Some(value), true) => compact_token_count(value),
        (Some(value), false) => format!("{} known", compact_token_count(value)),
        (None, _) => "?".to_string(),
    }
}

/// A typed terminal result owns the user-visible completion state.  The raw
/// tool ledger remains useful evidence, but it must not reopen a completed
/// assessment after runtime reconciliation has already accepted it.  When a
/// turn is interrupted or explicitly marked unverified, use the semantic
/// record-aware evaluator to explain the remaining obligation.
fn evaluation_status_notice_for_result(
    result: &StreamResult,
    evaluation: &TurnEvaluation,
) -> Option<String> {
    if result.final_state == "completed" && !result.server_terminal_unverified {
        return None;
    }
    astra_turn_core::evaluation::turn_evaluation_status_notice_for_records(
        evaluation,
        &result.tool_call_records,
    )
}

pub(crate) fn interruption_status_notice(result: &StreamResult) -> Option<String> {
    let interruption = result.interruption.as_ref()?;
    if let Some(user_message) = interruption
        .get("user_message")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        let has_distinct_answer =
            !result.full_text.trim().is_empty() && result.full_text.trim() != user_message;
        return Some(
            if result.interruption_kind.as_deref() == Some("execution_incomplete")
                && has_distinct_answer
            {
                format!("Partial answer shown. {user_message}")
            } else {
                user_message.to_string()
            },
        );
    }

    let kind = result
        .interruption_kind
        .as_deref()
        .or_else(|| interruption.get("kind").and_then(serde_json::Value::as_str))?;
    let resumable = interruption
        .get("resumable")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let suffix = if resumable {
        " You can continue in the next message."
    } else {
        ""
    };
    Some(format!("[{kind}] Turn interrupted.{suffix}"))
}

/// Print a context window warning when budget pressure exceeds thresholds.
pub(crate) fn print_context_window_warning(budget_pressure: f64) {
    const WARNING_THRESHOLD: f64 = 0.70;
    const CRITICAL_THRESHOLD: f64 = 0.85;

    if budget_pressure >= CRITICAL_THRESHOLD {
        let remaining = ((1.0 - budget_pressure) * 100.0).max(0.0);
        eprintln!(
            "{}",
            format!(
                "  🔴 Context window {:.0}% full ({:.0}% remaining) — consider /compact or starting a new session",
                budget_pressure * 100.0,
                remaining
            )
            .red()
        );
    } else if budget_pressure >= WARNING_THRESHOLD {
        let remaining = ((1.0 - budget_pressure) * 100.0).max(0.0);
        eprintln!(
            "{}",
            format!(
                "  🟡 Context window {:.0}% used ({:.0}% remaining) — use /stats context for details",
                budget_pressure * 100.0,
                remaining
            )
            .yellow()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_history_text, build_turn_tool_summary, cache_hit_percentage, compact_token_count,
        evaluation_status_notice_for_result, interruption_status_notice, usage_status_parts,
    };
    use crate::cli::stream::streaming_types::{AttributedTokenUsage, UsageAttribution};
    use astra_services::session_journal;
    use astra_turn_core::evaluation::{
        EvalSignal, EvaluationThresholds, TurnEvaluation, turn_evaluation_status_notice,
    };

    fn make_record(
        name: &str,
        ok: bool,
        file_path: Option<&str>,
    ) -> session_journal::ToolCallRecord {
        session_journal::ToolCallRecord {
            name: name.into(),
            ok,
            file_path: file_path.map(|path| path.into()),
            ..Default::default()
        }
    }

    #[test]
    fn turn_evaluation_notice_reports_unresolved_outcome_failure() {
        let eval = TurnEvaluation {
            success: false,
            quality: 0.2,
            confidence: 0.9,
            signals: vec![EvalSignal::ToolOutcomeFailure {
                class: "test_failure".to_string(),
                count: 1,
            }],
            thresholds: EvaluationThresholds::default(),
        };

        let notice = turn_evaluation_status_notice(&eval).expect("notice");
        assert!(notice.contains("test_failure x1"));
        assert!(notice.contains("incomplete"));
    }

    #[test]
    fn turn_evaluation_notice_ignores_successful_turn() {
        let eval = TurnEvaluation {
            success: true,
            quality: 0.8,
            confidence: 0.7,
            signals: vec![EvalSignal::AllToolsHealthy],
            thresholds: EvaluationThresholds::default(),
        };

        assert!(turn_evaluation_status_notice(&eval).is_none());
    }

    #[test]
    fn completed_typed_terminal_does_not_reopen_assessment_failure() {
        let mut result = crate::tests::stub_stream_result("");
        result.final_state = "completed".into();
        result.server_terminal_unverified = false;
        let mut failed = make_record("bash", false, None);
        failed.args_full =
            Some(serde_json::json!({"command": "cargo test --test artifact"}).to_string());
        failed.result_class = Some("test_failure".into());
        result.tool_call_records = vec![failed];
        let eval = TurnEvaluation {
            success: false,
            quality: 0.2,
            confidence: 0.9,
            signals: vec![EvalSignal::ToolOutcomeFailure {
                class: "test_failure".to_string(),
                count: 1,
            }],
            thresholds: EvaluationThresholds::default(),
        };

        assert!(
            evaluation_status_notice_for_result(&result, &eval).is_none(),
            "accepted runtime settlement must not be reclassified from raw evidence"
        );
    }

    #[test]
    fn interrupted_unverified_terminal_keeps_validation_failure_visible() {
        let mut result = crate::tests::stub_stream_result("");
        result.final_state = "interrupted".into();
        result.server_terminal_unverified = true;
        let mut failed = make_record("bash", false, None);
        failed.args_full =
            Some(serde_json::json!({"command": "cargo test --test artifact"}).to_string());
        failed.result_class = Some("test_failure".into());
        result.tool_call_records = vec![failed];
        let eval = TurnEvaluation {
            success: false,
            quality: 0.2,
            confidence: 0.9,
            signals: vec![EvalSignal::ToolOutcomeFailure {
                class: "test_failure".to_string(),
                count: 1,
            }],
            thresholds: EvaluationThresholds::default(),
        };

        let notice = evaluation_status_notice_for_result(&result, &eval).expect("notice");
        assert!(notice.contains("test_failure x1"));
    }

    #[test]
    fn interruption_status_notice_prefers_user_message() {
        let mut result = crate::tests::stub_stream_result("");
        result.interruption = Some(serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "user_message": "[budget_exhausted] 2 tool call(s) completed. Continue next turn."
        }));
        assert_eq!(
            interruption_status_notice(&result).as_deref(),
            Some("[budget_exhausted] 2 tool call(s) completed. Continue next turn.")
        );
    }

    #[test]
    fn interruption_status_notice_falls_back_to_kind_and_resumable_hint() {
        let mut result = crate::tests::stub_stream_result("");
        result.interruption_kind = Some("context_budget".into());
        result.interruption = Some(serde_json::json!({
            "kind": "context_budget",
            "resumable": true
        }));
        assert_eq!(
            interruption_status_notice(&result).as_deref(),
            Some("[context_budget] Turn interrupted. You can continue in the next message.")
        );
    }

    #[test]
    fn interruption_status_notice_labels_answer_separately_from_unverified_execution() {
        let mut result = crate::tests::stub_stream_result("partial answer");
        result.interruption_kind = Some("execution_incomplete".into());
        result.interruption = Some(serde_json::json!({
            "kind": "execution_incomplete",
            "resumable": true,
            "user_message": "Execution did not reach a verified terminal state. Review the available execution evidence, then continue to reconcile the unfinished work."
        }));

        let notice = interruption_status_notice(&result).expect("notice");
        assert!(notice.starts_with("Partial answer shown."));
        assert!(notice.contains("verified terminal state"));
        assert_eq!(result.full_text, "partial answer");
    }

    #[test]
    fn tool_summary_empty_when_no_tools() {
        let summary = build_turn_tool_summary(&[]);
        assert!(summary.is_empty());
    }

    #[test]
    fn tool_summary_lists_successful_files_touched() {
        let records = vec![
            make_record("read_file", true, Some("src/main.rs")),
            make_record("str_replace", true, Some("src/lib.rs")),
            make_record("read_file", true, Some("src/main.rs")),
        ];

        let summary = build_turn_tool_summary(&records);
        assert!(summary.contains("files: src/main.rs, src/lib.rs"));
        assert!(summary.contains("tool_calls: 3"));
        assert!(!summary.contains("failed:"));
    }

    #[test]
    fn tool_summary_includes_failures_without_failed_paths() {
        let records = vec![
            make_record("read_file", false, Some("rust/astra/src/bridge/mod.rs")),
            make_record(
                "str_replace",
                true,
                Some("crates/runtime/src/bridge/mod.rs"),
            ),
        ];

        let summary = build_turn_tool_summary(&records);
        assert!(summary.contains("files: crates/runtime/src/bridge/mod.rs"));
        assert!(summary.contains("failed: read_file"));
        assert!(
            !summary.contains("rust/astra"),
            "failed file paths must not be persisted into prompt-facing history: {summary}"
        );
    }

    #[test]
    fn tool_summary_caps_successful_files() {
        let mut records = Vec::new();
        for idx in 0..18 {
            records.push(make_record(
                "read_file",
                true,
                Some(&format!("src/file_{idx}.rs")),
            ));
        }
        records.push(make_record("edit", false, Some("rust/old/path.rs")));

        let summary = build_turn_tool_summary(&records);
        assert!(summary.contains("failed: edit"));
        assert!(summary.contains("(+3 more)"));
        assert!(!summary.contains("rust/old/path.rs"));
    }

    #[test]
    fn tool_summary_stays_compact_under_heavy_load() {
        let mut records = Vec::new();
        for idx in 0..50 {
            let file = format!("src/module_{}/file_{}.rs", idx / 5, idx % 5);
            records.push(make_record(
                if idx % 3 == 0 {
                    "read_file"
                } else {
                    "str_replace"
                },
                idx % 7 != 0,
                Some(&file),
            ));
        }

        let summary = build_turn_tool_summary(&records);
        assert!(
            summary.len() < 2048,
            "summary should be compact, got {} bytes: {summary}",
            summary.len()
        );
        assert!(summary.contains("src/module_0/file_1.rs"));
        assert!(
            !summary.contains("src/module_0/file_0.rs"),
            "failed file paths must not be retained: {summary}"
        );
        assert!(
            summary.contains("more)"),
            "should truncate beyond 15 files: {summary}"
        );
    }

    #[test]
    fn history_text_appends_tool_summary() {
        let full_text = "Updated three files.";
        let records = vec![
            make_record("read_file", true, Some("src/main.rs")),
            make_record("edit", false, Some("src/lib.rs")),
        ];
        let history_text = build_history_text(full_text, &records);
        assert!(history_text.starts_with(full_text));
        assert!(history_text.contains("[Turn context:"));
        assert!(history_text.contains("failed: edit"));
    }

    #[test]
    fn history_text_noop_without_tool_summary() {
        let full_text = "No tools used.";
        assert_eq!(build_history_text(full_text, &[]), full_text);
    }

    #[test]
    fn cache_hit_percentage_formula() {
        let cache_pct = cache_hit_percentage(200, 800, 0);
        assert!((cache_pct - 80.0).abs() < 0.01);
    }

    #[test]
    fn cache_hit_percentage_zero_when_no_cache() {
        let cache_pct = cache_hit_percentage(1000, 0, 0);
        assert!((cache_pct - 0.0).abs() < 0.01);
    }

    #[test]
    fn cache_hit_percentage_with_heavy_cache_creation() {
        let cache_pct = cache_hit_percentage(12, 29_816, 38_788);
        assert!(
            (cache_pct - 43.5).abs() < 1.0,
            "expected ~43.5%, got {cache_pct:.1}%"
        );
    }

    #[test]
    fn cache_hit_percentage_100_only_when_all_input_was_cache_read() {
        let cache_pct = cache_hit_percentage(0, 5000, 0);
        assert!((cache_pct - 100.0).abs() < 0.01);
    }

    #[test]
    fn status_line_keeps_unclassified_overall_out_of_main_usage() {
        let mut result = crate::tests::stub_stream_result("answer");
        result.prompt_tokens = 100;
        result.completion_tokens = 20;
        result.cache_read_tokens = 900;
        let parts = usage_status_parts(&result);

        assert!(parts.iter().any(|part| part.starts_with("overall tokens:")));
        assert!(!parts.iter().any(|part| part.starts_with("main tokens:")));
        assert!(
            parts
                .iter()
                .any(|part| part == "usage not fully attributed")
        );
        assert!(!parts.iter().any(|part| part.starts_with("cache:")));
    }

    #[test]
    fn status_line_hides_cache_rate_for_partial_primary_usage() {
        let mut result = crate::tests::stub_stream_result("answer");
        result.usage_attribution = UsageAttribution {
            primary: Some(AttributedTokenUsage {
                fresh_input_tokens: Some(100),
                cache_read_tokens: Some(900),
                cache_creation_tokens: Some(0),
                output_tokens: Some(20),
            }),
            primary_complete: false,
            ..UsageAttribution::default()
        };
        let parts = usage_status_parts(&result);

        assert!(parts.iter().any(|part| part.starts_with("main tokens:")));
        assert!(
            parts
                .iter()
                .any(|part| part == "usage not fully attributed")
        );
        assert!(!parts.iter().any(|part| part.starts_with("cache:")));
    }

    #[test]
    fn compact_token_count_below_1k() {
        assert_eq!(compact_token_count(999), "999");
    }

    #[test]
    fn compact_token_count_above_1k() {
        assert_eq!(compact_token_count(12_500), "12.5k");
    }
}
