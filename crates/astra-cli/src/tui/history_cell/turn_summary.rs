//! Turn-summary history cell — the compact post-turn metrics band.
//!
//! Shape:
//!
//! ```text
//!   16s total · ttft 1.8s · 23.6k tokens · 2 tools (145.0k overall · main deepseek · Jev ...)
//! ```
//!
//! The summary intentionally stays in product language rather than
//! exposing raw telemetry grammar. Lower-value details such as
//! per-direction token arrows are omitted so the band reads like a
//! calm recap instead of an engineering dashboard. Sections that
//! don't apply to this turn are elided.
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
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    /// Cached input alongside the fresh `tokens_in`. Drives both total provider
    /// traffic and the cache-rate band.
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
        let mut secondary_parts: Vec<Vec<Span<'static>>> = Vec::new();

        if let Some(elapsed) = self.elapsed_ms {
            sections.push(Section::primary(vec![
                Span::styled(fmt_duration_ms(elapsed), value),
                Span::styled(" total", label),
            ]));
        }

        if let Some(ttft) = self.ttft_ms
            && ttft > 0
        {
            sections.push(Section::primary(vec![
                Span::styled("ttft ", label),
                Span::styled(fmt_ms(ttft), value),
            ]));
        }

        let has_known_provider_lane = self.tokens_in.is_some()
            || self.tokens_out.is_some()
            || self.cache_read_tokens.is_some()
            || self.cache_creation_tokens.is_some();
        if has_known_provider_lane {
            let provider_tokens = self
                .tokens_in
                .unwrap_or(0)
                .saturating_add(self.cache_read_tokens.unwrap_or(0))
                .saturating_add(self.cache_creation_tokens.unwrap_or(0))
                .saturating_add(self.tokens_out.unwrap_or(0));
            sections.push(Section::primary(vec![
                Span::styled(fmt_tokens(provider_tokens), value),
                Span::styled(
                    if self.usage_partial {
                        " tokens known"
                    } else {
                        " tokens"
                    },
                    label,
                ),
            ]));
        }

        if !self.usage_partial
            && let (Some(cache_read), Some(fresh_input)) = (self.cache_read_tokens, self.tokens_in)
            && let Some(cache_creation) = self.cache_creation_tokens.or(Some(0))
            && cache_read > 0
        {
            let total_input = cache_read
                .saturating_add(fresh_input)
                .saturating_add(cache_creation);
            let pct = if total_input == 0 {
                0
            } else {
                ((cache_read as f64 / total_input as f64) * 100.0).round() as u32
            };
            sections.push(Section::primary(vec![
                Span::styled(format!("{pct}%"), value),
                Span::styled(" cached", label),
            ]));
        }

        if self.tools > 0 {
            sections.push(Section::primary(vec![
                Span::styled(self.tools.to_string(), value),
                Span::styled(if self.tools == 1 { " tool" } else { " tools" }, label),
            ]));
        }

        if let Some(model) = self.model_name.as_deref() {
            secondary_parts.push(vec![
                Span::styled("main ", label),
                Span::styled(model.to_string(), value),
            ]);
        }
        if let Some(auxiliary) = self.auxiliary_summary.as_deref() {
            secondary_parts.push(vec![Span::styled(auxiliary.to_string(), value)]);
        }
        if self.usage_partial {
            secondary_parts.push(vec![Span::styled("usage not fully attributed", label)]);
        }

        let current_provider_tokens = self
            .tokens_in
            .unwrap_or(0)
            .saturating_add(self.cache_read_tokens.unwrap_or(0))
            .saturating_add(self.cache_creation_tokens.unwrap_or(0))
            .saturating_add(self.tokens_out.unwrap_or(0));
        let cumulative_tokens = self
            .cumulative_tokens
            .filter(|c| *c > current_provider_tokens)
            .map(|c| Span::styled(fmt_tokens(c), value));

        if let Some(cost) = self.cumulative_cost_usd
            && cost > 0.0
        {
            secondary_parts.push(vec![
                Span::styled(fmt_cost(cost), value),
                Span::styled(" spent", label),
            ]);
        }

        if cumulative_tokens.is_some() || !secondary_parts.is_empty() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::styled("(", label));
            if let Some(tokens) = cumulative_tokens {
                spans.push(tokens);
                spans.push(Span::styled(" overall", label));
            } else {
                spans.push(Span::styled("Overall", label));
            }
            for part in secondary_parts {
                spans.push(Span::styled(" · ", label));
                spans.extend(part);
            }
            spans.push(Span::styled(")", label));
            sections.push(Section::secondary(spans));
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

/// Sub-turn ttft: ms below 1s, decimal seconds above.
fn fmt_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn fmt_cost(usd: f64) -> String {
    if usd >= 1.0 {
        format!("${usd:.2}")
    } else if usd >= 0.01 {
        format!("${usd:.3}")
    } else {
        format!("${usd:.4}")
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
    secondary: bool,
}

impl Section {
    fn primary(spans: Vec<Span<'static>>) -> Self {
        Self {
            spans,
            secondary: false,
        }
    }

    fn secondary(spans: Vec<Span<'static>>) -> Self {
        Self {
            spans,
            secondary: true,
        }
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
    fn full_summary_contains_all_sections() {
        let out = render(&mk_full(), 120);
        for seg in ["total", "ttft", "tokens", "tools", "overall", "spent"] {
            assert!(out.contains(seg), "missing section {seg:?} in {out}");
        }
        assert!(out.contains("145.0k"));
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
        assert!(out.contains("total"));
    }

    #[test]
    fn cache_segment_shows_hit_rate_percentage() {
        let mut c = mk_full();
        // 23.2k fresh + 18k cached input → ~44% cache hit rate.
        c.cache_read_tokens = Some(18_000);
        let out = render(&c, 120);
        assert!(out.contains("cached"), "cache label missing: {out}");
        assert!(out.contains("44%"), "expected ~44% hit rate in: {out}");
    }

    #[test]
    fn headline_tokens_include_provider_cache_traffic() {
        let c = TurnSummaryCell {
            tokens_in: Some(1_200),
            tokens_out: Some(300),
            cache_read_tokens: Some(98_800),
            ..Default::default()
        };
        let out = render(&c, 120);
        assert!(
            out.contains("100.3k tokens"),
            "provider traffic should headline: {out}"
        );
        assert!(
            out.contains("99% cached"),
            "cache traffic should stay visible: {out}"
        );
        assert!(
            !out.contains("1.5k tokens"),
            "fresh-only billing tokens must not masquerade as provider traffic: {out}"
        );
    }

    #[test]
    fn partial_usage_keeps_known_tokens_but_hides_exact_cache_rate() {
        let c = TurnSummaryCell {
            tokens_in: Some(1_200),
            tokens_out: Some(300),
            cache_read_tokens: Some(98_800),
            cache_creation_tokens: None,
            usage_partial: true,
            ..Default::default()
        };
        let out = render(&c, 120);
        assert!(
            out.contains("100.3k tokens known"),
            "known lanes missing: {out}"
        );
        assert!(
            !out.contains("cached"),
            "cache rate must be unavailable: {out}"
        );
        assert!(
            out.contains("usage not fully attributed"),
            "coverage missing: {out}"
        );
    }

    #[test]
    fn partial_usage_hides_cache_rate_even_when_all_input_lanes_are_known() {
        let c = TurnSummaryCell {
            tokens_in: Some(1_200),
            tokens_out: Some(300),
            cache_read_tokens: Some(98_800),
            cache_creation_tokens: Some(0),
            usage_partial: true,
            ..Default::default()
        };
        let out = render(&c, 120);
        assert!(!out.contains("cached"), "partial cache rate leaked: {out}");
        assert!(out.contains("usage not fully attributed"));
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
        assert!(out.contains("100.3k tokens"));
        assert!(
            !out.contains("overall"),
            "duplicate overall is noise: {out}"
        );
    }

    #[test]
    fn cache_segment_elided_when_unreported() {
        // Many providers don't surface cache reads on first turn
        // or when caching is disabled — absence should not render
        // a misleading "0%" chip.
        let mut c = mk_full();
        c.cache_read_tokens = None;
        let out = render(&c, 120);
        assert!(!out.contains("cached"), "cache chip must be elided: {out}");
    }

    #[test]
    fn cache_segment_elided_when_read_is_zero() {
        // Provider reported but hit rate genuinely zero — still
        // elide rather than show "0%", which reads as noise.
        let mut c = mk_full();
        c.cache_read_tokens = Some(0);
        let out = render(&c, 120);
        assert!(
            !out.contains("cached"),
            "zero-hit cache chip must elide: {out}"
        );
    }

    #[test]
    fn sigma_section_elided_when_cumulative_zero() {
        let mut c = mk_full();
        c.cumulative_tokens = Some(0);
        c.cumulative_cost_usd = Some(0.0);
        let out = render(&c, 120);
        assert!(!out.contains("overall"));
        assert!(!out.contains("spent"));
    }

    #[test]
    fn narrow_width_wraps_summary_into_multiple_lines() {
        let out = render(&mk_full(), 46);
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

    #[test]
    fn fmt_tokens_scales() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_234), "1.2k");
        assert_eq!(fmt_tokens(23_200), "23.2k");
        assert_eq!(fmt_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn fmt_cost_precision_scales_with_magnitude() {
        assert_eq!(fmt_cost(0.0042), "$0.0042");
        assert_eq!(fmt_cost(0.014), "$0.014");
        assert_eq!(fmt_cost(2.5), "$2.50");
    }

    // ── Persistence ──────────────────────────────────────────────

    #[test]
    fn persist_roundtrip_keeps_every_field() {
        let orig = mk_full();
        let ev = orig.to_persist().unwrap();
        let back = TurnSummaryCell::from_persist(ev).unwrap();
        assert_eq!(back.elapsed_ms, orig.elapsed_ms);
        assert_eq!(back.ttft_ms, orig.ttft_ms);
        assert_eq!(back.tokens_in, orig.tokens_in);
        assert_eq!(back.tokens_out, orig.tokens_out);
        assert_eq!(back.tools, orig.tools);
        assert_eq!(back.cumulative_tokens, orig.cumulative_tokens);
        assert_eq!(back.cumulative_cost_usd, orig.cumulative_cost_usd);
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
