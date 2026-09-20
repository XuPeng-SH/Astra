//! Turn-summary history cell — the compact post-turn metrics band.
//!
//! Shape:
//!
//! ```text
//!   8.5s · with deepseek-flash · 1 tool
//! ```
//!
//! The summary is a user-facing completion marker, not a telemetry report.
//! It answers how long the turn took, which primary model answered, and
//! whether material tools ran. Token lanes, cache ratios, auxiliary judgment,
//! and session totals remain structured evidence for Explain Analyze rather
//! than being dumped into every chat turn.
//!
//! Persists as [`TurnEvent::TurnSummary`]. Never live.

use std::any::Any;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::tui::turn_event::TurnEvent;

#[derive(Debug, Clone, Default)]
pub(crate) struct TurnSummaryCell {
    pub elapsed_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    /// Structured provider metrics retained for diagnostics/replay. They are
    /// intentionally not rendered in the default completion marker.
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    /// Cached input alongside the fresh `tokens_in`.
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub model_name: Option<String>,
    pub auxiliary_summary: Option<String>,
    pub usage_partial: bool,
    pub tools: u32,
    pub cumulative_tokens: Option<u64>,
    pub cumulative_cost_usd: Option<f64>,
    pub ts: Option<String>,
}

impl TurnSummaryCell {
    pub fn from_persist(ev: TurnEvent) -> Option<Self> {
        match ev {
            TurnEvent::TurnSummary {
                ts,
                elapsed_ms,
                ttft_ms,
                tokens_in,
                tokens_out,
                cache_read_tokens,
                cache_creation_tokens,
                model_name,
                auxiliary_summary,
                usage_partial,
                tools,
                cumulative_tokens,
                cumulative_cost_usd,
            } => Some(Self {
                elapsed_ms,
                ttft_ms,
                tokens_in,
                tokens_out,
                cache_read_tokens,
                cache_creation_tokens,
                model_name,
                auxiliary_summary,
                usage_partial,
                tools,
                cumulative_tokens,
                cumulative_cost_usd,
                ts,
            }),
            _ => None,
        }
    }
}

impl HistoryCell for TurnSummaryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let theme = crate::tui::theme::current();
        let label = Style::default().fg(theme.dim);
        let value = Style::default().fg(theme.fg);
        let sections = self.sections(label, value);
        if sections.is_empty() {
            return Vec::new();
        }

        pack_sections_into_lines(sections, width)
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn to_persist(&self) -> Option<TurnEvent> {
        Some(TurnEvent::TurnSummary {
            ts: self.ts.clone(),
            elapsed_ms: self.elapsed_ms,
            ttft_ms: self.ttft_ms,
            tokens_in: self.tokens_in,
            tokens_out: self.tokens_out,
            cache_read_tokens: self.cache_read_tokens,
            cache_creation_tokens: self.cache_creation_tokens,
            model_name: self.model_name.clone(),
            auxiliary_summary: self.auxiliary_summary.clone(),
            usage_partial: self.usage_partial,
            tools: self.tools,
            cumulative_tokens: self.cumulative_tokens,
            cumulative_cost_usd: self.cumulative_cost_usd,
        })
    }
}

impl TurnSummaryCell {
    fn sections(&self, label: Style, value: Style) -> Vec<Section> {
        let mut sections: Vec<Section> = Vec::new();
        if let Some(elapsed) = self.elapsed_ms {
            sections.push(Section::primary(vec![Span::styled(
                fmt_duration_ms(elapsed),
                value,
            )]));
        }

        if let Some(model) = self.model_name.as_deref() {
            sections.push(Section::primary(vec![
                Span::styled("with ", label),
                Span::styled(model.to_string(), value),
            ]));
        }

        if self.tools > 0 {
            sections.push(Section::primary(vec![
                Span::styled(self.tools.to_string(), value),
                Span::styled(if self.tools == 1 { " tool" } else { " tools" }, label),
            ]));
        }

        sections
    }
}

// ── Formatting helpers ──────────────────────────────────────────
//
// Kept local so the TurnSummaryCell renders deterministically without coupling
// its persisted projection to unrelated status-line formatting.

/// Elapsed duration in ms. Whole seconds below a minute, `Nm Ss`
/// above — matches the coarse format we use elsewhere so the
/// band doesn't jitter sub-second.
fn fmt_duration_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let sub = ms % 1000;
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs >= 10 || sub == 0 {
        // ≥ 10 s or an exact-second tick — drop the decimal so the
        // band doesn't jitter on long turns or exact multiples.
        format!("{secs}s")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

#[derive(Debug, Clone)]
struct Section {
    spans: Vec<Span<'static>>,
}

impl Section {
    fn primary(spans: Vec<Span<'static>>) -> Self {
        Self { spans }
    }
}

fn pack_sections_into_lines(sections: Vec<Section>, width: u16) -> Vec<Line<'static>> {
    let available = usize::from(width.max(24));
    let indent = Span::styled("  ", Style::default().fg(crate::tui::theme::current().dim));
    let indent_width = unicode_width::UnicodeWidthStr::width(indent.content.as_ref());
    let sep = Span::styled(" · ", Style::default().fg(crate::tui::theme::current().dim));
    let sep_width = unicode_width::UnicodeWidthStr::width(sep.content.as_ref());
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = vec![indent.clone()];
    let mut current_width = indent_width;

    for section in sections {
        let section_width = spans_width(&section.spans);
        let extra = if current_width == indent_width {
            0
        } else {
            sep_width
        };
        if current_width > indent_width && current_width + extra + section_width > available {
            lines.push(Line::from(current));
            current = vec![indent.clone()];
            current_width = indent_width;
        }
        if current_width > indent_width {
            current.push(sep.clone());
            current_width += sep_width;
        }
        current_width += section_width;
        current.extend(section.spans);
    }

    if current_width > indent_width {
        lines.push(Line::from(current));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    fn render(cell: &TurnSummaryCell, width: u16) -> String {
        let lines = cell.display_lines(width);
        let p =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        buffer_to_string(&draw_widget(p, width, 3))
    }

    fn mk_full() -> TurnSummaryCell {
        TurnSummaryCell {
            elapsed_ms: Some(16_600),
            ttft_ms: Some(1_757),
            tokens_in: Some(23_200),
            tokens_out: Some(408),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            model_name: None,
            auxiliary_summary: None,
            usage_partial: false,
            tools: 2,
            cumulative_tokens: Some(145_000),
            cumulative_cost_usd: Some(0.014),
            ts: None,
        }
    }

    // ── Render ───────────────────────────────────────────────────

    #[test]
    fn full_summary_contains_only_user_facing_sections() {
        let out = render(&mk_full(), 120);
        for seg in ["16s", "tools"] {
            assert!(out.contains(seg), "missing section {seg:?} in {out}");
        }
        for diagnostic in ["ttft", "tokens", "cached", "overall", "spent"] {
            assert!(
                !out.contains(diagnostic),
                "diagnostic section {diagnostic:?} leaked into {out}"
            );
        }
    }

    #[test]
    fn tools_zero_is_elided() {
        let mut c = mk_full();
        c.tools = 0;
        let out = render(&c, 120);
        assert!(!out.contains(" tools"), "tools=0 should not render: {out}");
    }

    #[test]
    fn ttft_zero_is_elided_from_time_segment() {
        let mut c = mk_full();
        c.ttft_ms = Some(0);
        let out = render(&c, 120);
        assert!(!out.contains("ttft"), "ttft=0 must not render: {out}");
        assert!(out.contains("16s"));
    }

    #[test]
    fn model_name_is_compact_and_user_facing() {
        let mut c = mk_full();
        c.model_name = Some("deepseek-flash".into());
        let out = render(&c, 120);
        assert!(out.contains("with deepseek-flash"), "model missing: {out}");
    }

    #[test]
    fn usage_details_are_not_dumped_into_default_summary() {
        let c = TurnSummaryCell {
            tokens_in: Some(1_200),
            tokens_out: Some(300),
            cache_read_tokens: Some(98_800),
            cache_creation_tokens: Some(0),
            auxiliary_summary: Some("Jev (jev-1.13.0) · request_judgment".into()),
            usage_partial: true,
            cumulative_tokens: Some(145_000),
            ..Default::default()
        };
        let out = render(&c, 120);
        for diagnostic in [
            "tokens",
            "cached",
            "Jev",
            "request_judgment",
            "usage not fully attributed",
            "overall",
        ] {
            assert!(
                !out.contains(diagnostic),
                "diagnostic {diagnostic:?} leaked into {out}"
            );
        }
    }

    #[test]
    fn overall_is_elided_when_it_only_repeats_the_current_turn() {
        let c = TurnSummaryCell {
            tokens_in: Some(1_200),
            tokens_out: Some(300),
            cache_read_tokens: Some(98_800),
            cumulative_tokens: Some(100_300),
            ..Default::default()
        };
        let out = render(&c, 120);
        assert!(
            !out.contains("overall"),
            "duplicate overall is noise: {out}"
        );
    }

    #[test]
    fn usage_summary_is_empty_when_only_diagnostics_are_present() {
        let mut c = mk_full();
        c.cache_read_tokens = None;
        c.elapsed_ms = None;
        c.tools = 0;
        let out = render(&c, 120);
        assert!(out.trim().is_empty(), "diagnostic-only cell leaked: {out}");
    }

    #[test]
    fn narrow_width_wraps_summary_into_multiple_lines() {
        let mut cell = mk_full();
        cell.model_name = Some("deepseek-flash".into());
        let out = render(&cell, 32);
        let non_empty: Vec<&str> = out.lines().filter(|line| !line.trim().is_empty()).collect();
        assert!(
            non_empty.len() >= 2,
            "narrow summaries should wrap cleanly; got {out:?}"
        );
    }

    #[test]
    fn empty_cell_renders_nothing() {
        let c = TurnSummaryCell::default();
        let lines = c.display_lines(80);
        assert!(lines.is_empty(), "default cell must not produce output");
    }

    // ── Formatting helpers ───────────────────────────────────────

    #[test]
    fn fmt_duration_boundaries() {
        assert_eq!(fmt_duration_ms(400), "0.4s");
        assert_eq!(fmt_duration_ms(1_500), "1.5s");
        assert_eq!(fmt_duration_ms(16_600), "16s"); // >= 10s drops decimal
        assert_eq!(fmt_duration_ms(60_000), "1m 0s");
        assert_eq!(fmt_duration_ms(125_000), "2m 5s");
    }

    // ── Persistence ──────────────────────────────────────────────

    #[test]
    fn persist_roundtrip_keeps_every_field() {
        let mut orig = mk_full();
        orig.cache_read_tokens = Some(18_000);
        orig.cache_creation_tokens = Some(7);
        orig.model_name = Some("deepseek-flash".into());
        orig.auxiliary_summary = Some("Jev (deepseek-judgement) · request_judgment".into());
        orig.usage_partial = true;
        orig.ts = Some("2026-09-20T00:00:00Z".into());
        let ev = orig.to_persist().unwrap();
        let back = TurnSummaryCell::from_persist(ev).unwrap();
        assert_eq!(back.elapsed_ms, orig.elapsed_ms);
        assert_eq!(back.ttft_ms, orig.ttft_ms);
        assert_eq!(back.tokens_in, orig.tokens_in);
        assert_eq!(back.tokens_out, orig.tokens_out);
        assert_eq!(back.cache_read_tokens, orig.cache_read_tokens);
        assert_eq!(back.cache_creation_tokens, orig.cache_creation_tokens);
        assert_eq!(back.model_name, orig.model_name);
        assert_eq!(back.auxiliary_summary, orig.auxiliary_summary);
        assert_eq!(back.usage_partial, orig.usage_partial);
        assert_eq!(back.tools, orig.tools);
        assert_eq!(back.cumulative_tokens, orig.cumulative_tokens);
        assert_eq!(back.cumulative_cost_usd, orig.cumulative_cost_usd);
        assert_eq!(back.ts, orig.ts);
    }

    #[test]
    fn from_persist_rejects_wrong_variant() {
        let wrong = TurnEvent::User {
            ts: None,
            text: "x".into(),
        };
        assert!(TurnSummaryCell::from_persist(wrong).is_none());
    }

    // ── Snapshot ─────────────────────────────────────────────────

    #[test]
    fn snapshot_full_band_120() {
        crate::tui::testing::assert_tui_snapshot!("turn_summary_full_120", render(&mk_full(), 120));
    }
}
