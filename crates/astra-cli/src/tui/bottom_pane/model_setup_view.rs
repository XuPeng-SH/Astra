//! Native Runner-local model setup. Provider secrets remain masked and cross
//! the view boundary only in a redacted typed wrapper.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use unicode_width::UnicodeWidthStr;

use super::view::{
    BottomPaneView, CancellationEvent, ModelSetupCredentialDraft, ModelSetupDraft, SecretInput,
    ViewCompletion, ViewResult,
};

const LABELS: [&str; 5] = [
    "Name       ",
    "API base   ",
    "Model      ",
    "Credential ",
    "Value      ",
];

pub(crate) struct ModelSetupView {
    values: [String; 5],
    focus: usize,
    error: Option<String>,
    submitted: Option<ViewResult>,
    cancelled: bool,
}

impl ModelSetupView {
    pub(crate) fn new() -> Self {
        Self {
            values: [
                String::new(),
                "https://api.openai.com/v1".to_string(),
                String::new(),
                "environment".to_string(),
                "OPENAI_API_KEY".to_string(),
            ],
            focus: 0,
            error: None,
            submitted: None,
            cancelled: false,
        }
    }

    fn credential_source(&self) -> &str {
        self.values[3].trim()
    }

    fn rendered_value(&self, index: usize) -> String {
        if index == 4 && matches!(self.credential_source(), "stored" | "file") {
            "•".repeat(self.values[index].chars().count())
        } else if index == 4 && matches!(self.credential_source(), "none" | "keyless") {
            "—".to_string()
        } else {
            self.values[index].clone()
        }
    }

    fn submit(&mut self) {
        for index in 0..3 {
            if self.values[index].trim().is_empty() {
                self.error = Some(format!("{} cannot be empty", LABELS[index].trim()));
                self.focus = index;
                return;
            }
        }
        let credential = match self.credential_source().to_ascii_lowercase().as_str() {
            "environment" | "env" if !self.values[4].trim().is_empty() => {
                ModelSetupCredentialDraft::Environment {
                    name: self.values[4].trim().to_string(),
                }
            }
            "stored" | "file" if !self.values[4].is_empty() => ModelSetupCredentialDraft::Stored {
                secret: SecretInput::new(self.values[4].clone()),
            },
            "none" | "keyless" => ModelSetupCredentialDraft::None,
            "environment" | "env" => {
                self.error = Some("Environment variable cannot be empty".to_string());
                self.focus = 4;
                return;
            }
            "stored" | "file" => {
                self.error = Some("Provider API key cannot be empty".to_string());
                self.focus = 4;
                return;
            }
            _ => {
                self.error = Some("Credential must be environment, stored, or none".to_string());
                self.focus = 3;
                return;
            }
        };
        self.submitted = Some(ViewResult::ModelSetup(ModelSetupDraft {
            name: self.values[0].trim().to_string(),
            base_url: self.values[1].trim().to_string(),
            provider_model: self.values[2].trim().to_string(),
            credential,
        }));
    }

    fn cycle_credential(&mut self, backwards: bool) {
        let next = match (self.credential_source(), backwards) {
            ("environment" | "env", false) | ("none" | "keyless", true) => "stored",
            ("stored" | "file", false) => "none",
            ("stored" | "file", true) | ("none" | "keyless", false) => "environment",
            _ => "environment",
        };
        self.values[3] = next.to_string();
        self.values[4] = if next == "environment" {
            "OPENAI_API_KEY".to_string()
        } else {
            String::new()
        };
        self.error = None;
    }
}

impl BottomPaneView for ModelSetupView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Line::from(Span::styled(
                " /model add · Runner-local ",
                Style::default()
                    .fg(crate::tui::theme::current().accent)
                    .add_modifier(Modifier::BOLD),
            )));
        let inner = outer.inner(area);
        outer.render(area, buf);
        let mut lines = vec![Line::from(Span::styled(
            "  Your key stays on this machine and is never sent to Astra Server.",
            Style::default().fg(Color::Gray),
        ))];
        for (index, label) in LABELS.iter().enumerate() {
            let focused = index == self.focus;
            lines.push(Line::from(vec![
                Span::styled(
                    if focused { "  ▸ " } else { "    " },
                    Style::default().fg(crate::tui::theme::current().accent),
                ),
                Span::styled(*label, Style::default().fg(Color::Gray)),
                Span::raw("  "),
                Span::styled(
                    self.rendered_value(index),
                    Style::default().fg(if focused { Color::White } else { Color::Gray }),
                ),
            ]));
        }
        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("  {error}"),
                Style::default().fg(Color::Red),
            )));
        }
        lines.push(Line::from(Span::styled(
            "  Tab / ↑↓ field · ←→ credential source · Enter save · Esc cancel",
            Style::default().fg(Color::DarkGray),
        )));
        Paragraph::new(lines).render(inner, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        9 + u16::from(self.error.is_some())
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => self.cancelled = true,
            (KeyCode::Tab | KeyCode::Down, _) => {
                self.focus = (self.focus + 1).min(LABELS.len() - 1);
                self.error = None;
            }
            (KeyCode::BackTab | KeyCode::Up, _) => {
                self.focus = self.focus.saturating_sub(1);
                self.error = None;
            }
            (KeyCode::Left, _) if self.focus == 3 => self.cycle_credential(true),
            (KeyCode::Right, _) if self.focus == 3 => self.cycle_credential(false),
            (KeyCode::Enter, _) => self.submit(),
            (KeyCode::Backspace, _) if self.focus != 3 => {
                self.values[self.focus].pop();
                self.error = None;
            }
            (KeyCode::Char('u'), KeyModifiers::CONTROL) if self.focus != 3 => {
                self.values[self.focus].clear();
                self.error = None;
            }
            (KeyCode::Char(character), modifiers)
                if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
            {
                if self.focus != 3
                    && !(self.focus == 4 && matches!(self.credential_source(), "none" | "keyless"))
                {
                    self.values[self.focus].push(character);
                }
                self.error = None;
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        if self.cancelled || self.submitted.is_some() {
            return None;
        }
        let value_width = self.rendered_value(self.focus).width() as u16;
        Some((
            area.x
                .saturating_add(6 + LABELS[self.focus].width() as u16 + value_width),
            area.y.saturating_add(2 + self.focus as u16),
        ))
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.cancelled = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.cancelled || self.submitted.is_some()
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.cancelled {
            Some(ViewCompletion {
                result: None,
                reopen: None,
            })
        } else {
            self.submitted.clone().map(|result| ViewCompletion {
                result: Some(result),
                reopen: None,
            })
        }
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }

    fn hint_keys(&self) -> Option<String> {
        None
    }

    fn reserve_status_footer(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};

    struct Widget<'a>(&'a ModelSetupView);
    impl ratatui::widgets::Widget for Widget<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            self.0.render(area, buf);
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn stored_secret_is_masked_in_render_and_debug_result() {
        let mut view = ModelSetupView::new();
        view.values = [
            "work".into(),
            "https://provider.example/v1".into(),
            "coding-model".into(),
            "stored".into(),
            "provider-secret-canary".into(),
        ];
        view.focus = 4;
        let rendered = buffer_to_string(&draw_widget(Widget(&view), 100, 12));
        assert!(!rendered.contains("provider-secret-canary"));
        assert!(rendered.contains("••••"));
        view.handle_key(key(KeyCode::Enter));
        let result = view.completion().unwrap().result.unwrap();
        assert!(!format!("{result:?}").contains("provider-secret-canary"));
    }

    #[test]
    fn escape_cancels_without_emitting_partial_secret() {
        let mut view = ModelSetupView::new();
        view.values[4] = "provider-secret-canary".into();
        view.handle_key(key(KeyCode::Esc));
        assert!(view.completion().unwrap().result.is_none());
    }

    #[test]
    fn credential_picker_never_carries_one_sources_value_into_another() {
        let mut view = ModelSetupView::new();
        view.focus = 3;
        view.handle_key(key(KeyCode::Right));
        assert_eq!(view.credential_source(), "stored");
        assert!(view.values[4].is_empty());
        view.values[4] = "secret-canary".into();
        view.handle_key(key(KeyCode::Right));
        assert_eq!(view.credential_source(), "none");
        assert!(view.values[4].is_empty());
        view.handle_key(key(KeyCode::Right));
        assert_eq!(view.credential_source(), "environment");
        assert_eq!(view.values[4], "OPENAI_API_KEY");
    }
}
