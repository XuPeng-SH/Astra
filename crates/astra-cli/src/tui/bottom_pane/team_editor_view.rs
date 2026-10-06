//! A local roster draft, not another Team configuration or execution owner.

use std::sync::Arc;

use astra_services::team_persistence::{TeamDefinition, TeamMemberDef, TeamWriteError};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Paragraph, Widget, Wrap},
};

use super::info_view::InfoView;
use super::list_selection_view::{ListSelectionView, SelectionItem};
use super::textarea::{TextArea, TextAreaAction};
use super::view::{
    BottomPaneView, BottomPaneViewAction, CancellationEvent, ViewActionDisposition,
    ViewActionRequest, ViewCompletion, ViewResult,
};
use crate::cli::http_team_store::HttpTeamStore;

/// The same captured transport travels from the browser into its editor and
/// background operations. Equality is source identity, never an account label.
#[derive(Clone)]
pub(crate) struct TeamEditorOwner(pub Arc<HttpTeamStore>);

impl std::fmt::Debug for TeamEditorOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TeamEditorOwner")
    }
}

impl PartialEq for TeamEditorOwner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for TeamEditorOwner {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TeamEditorTarget {
    pub editor_id: uuid::Uuid,
    pub attachment_epoch: u64,
    pub owner: TeamEditorOwner,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TeamEditorOperation {
    Save {
        definition: Box<TeamDefinition>,
        create: bool,
    },
    Refresh {
        team_id: String,
    },
    Models {
        agent_id: String,
        current: Option<astra_turn_types::ModelSelection>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TeamEditorRequest {
    pub target: TeamEditorTarget,
    pub operation_id: u64,
    pub operation: TeamEditorOperation,
}

#[derive(Clone, Debug)]
pub(crate) enum TeamEditorResponse {
    Saved(Result<TeamDefinition, TeamWriteError>),
    Refreshed(Result<Option<TeamDefinition>, String>),
    Models(Result<Vec<astra_services::ModelListItemResponse>, String>),
}

#[derive(Clone, Debug)]
pub(crate) struct TeamEditorUpdate {
    pub request: TeamEditorRequest,
    pub response: TeamEditorResponse,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Row {
    Name,
    Description,
    MemberName(usize),
    Responsibility(usize),
    Model(usize),
    Coordinator(usize),
    RemoveMember(usize),
    AddMember,
    ContextKey(String),
    ContextValue(String),
    RemoveContext(String),
    AddContext,
    Save,
    Refresh,
    UseLatest,
    KeepDraft,
    Observed,
    Cancel,
}

/// Recovery belongs to this draft. An uncertain write retains exactly what
/// was sent; absence or an older read is not evidence that it cannot commit.
enum SaveStatus {
    Ready,
    Review(Option<Box<TeamDefinition>>),
    Unconfirmed {
        sent: Box<TeamDefinition>,
        create: bool,
    },
}

pub(crate) struct TeamEditorView {
    target: TeamEditorTarget,
    baseline: Option<TeamDefinition>,
    working: TeamDefinition,
    save_status: SaveStatus,
    editing: bool,
    selection_available: bool,
    selected: usize,
    input: Option<(Row, TextArea)>,
    message: String,
    next_operation: u64,
    pending: Option<TeamEditorRequest>,
    model_pick: Option<(u64, String)>,
    // Presentation metadata only, populated by the member's exact catalog row.
    model_labels: std::collections::HashMap<String, String>,
    inspection: Option<InfoView>,
    width: std::cell::Cell<u16>,
    action: Option<ViewActionRequest>,
    completed: bool,
    confirm_close: bool,
    result: Option<ViewResult>,
    reopen: Option<String>,
}

impl TeamEditorView {
    pub(crate) fn with_selection_available(mut self, available: bool) -> Self {
        self.selection_available = available;
        self
    }

    pub(crate) fn set_initial_name(&mut self, name: String, description: String) {
        self.working.name = name;
        self.working.description = description;
    }

    pub(crate) fn with_reopen(mut self, command: &str) -> Self {
        self.reopen = Some(command.into());
        self
    }

    pub(crate) fn new(
        team: Option<TeamDefinition>,
        owner: TeamEditorOwner,
        attachment_epoch: u64,
    ) -> Self {
        let working = team.clone().unwrap_or_else(|| TeamDefinition {
            team_id: uuid::Uuid::new_v4().to_string(),
            user_id: owner.0.owner_account_id().unwrap_or_default().to_owned(),
            name: String::new(),
            description: String::new(),
            members: Vec::new(),
            context: Default::default(),
            revision: 1,
        });
        Self {
            target: TeamEditorTarget {
                editor_id: uuid::Uuid::new_v4(),
                attachment_epoch,
                owner,
            },
            editing: team.is_none(),
            selection_available: true,
            baseline: team,
            working,
            save_status: SaveStatus::Ready,
            selected: 0,
            input: None,
            message: "Saving changes future work only; running members keep their configuration."
                .into(),
            next_operation: 0,
            pending: None,
            model_pick: None,
            model_labels: Default::default(),
            inspection: None,
            width: std::cell::Cell::new(80),
            action: None,
            completed: false,
            confirm_close: false,
            result: None,
            reopen: None,
        }
    }

    fn model_label(
        &self,
        selection: Option<&astra_turn_types::ModelSelection>,
        inspect: bool,
    ) -> String {
        let Some(selection) = selection else {
            return "Inherited from parent".into();
        };
        let label = self
            .model_labels
            .get(&selection.offering_id)
            .map(String::as_str)
            .unwrap_or("Configured model (exact)");
        if inspect {
            format!("{label} · {}", selection.offering_id)
        } else {
            label.to_owned()
        }
    }

    fn rows(&self) -> Vec<(Row, String)> {
        self.display_rows(false)
    }

    fn display_rows(&self, inspect: bool) -> Vec<(Row, String)> {
        let mut rows = vec![
            (Row::Name, format!("Name: {}", self.working.name)),
            (
                Row::Description,
                format!("Purpose: {}", self.working.description),
            ),
        ];
        for (index, member) in self.working.members.iter().enumerate() {
            let model = self.model_label(member.model_selection.as_ref(), inspect);
            rows.extend([
                (
                    Row::MemberName(index),
                    format!("{} · {}", index + 1, member.role),
                ),
                (
                    Row::Responsibility(index),
                    format!(
                        "  Responsibility: {}",
                        member.system_prompt.as_deref().unwrap_or("Not set")
                    ),
                ),
                (Row::Model(index), format!("  Model: {model}")),
                (
                    Row::Coordinator(index),
                    format!(
                        "  Can coordinate: {}",
                        if member.can_delegate { "yes" } else { "no" }
                    ),
                ),
            ]);
            if inspect {
                rows.push((
                    Row::Observed,
                    format!(
                        "  Read-only: {} · tools: {} · skills: {:?} · MCP: {:?}",
                        member.read_only,
                        match &member.allow_tools {
                            None => "inherited".into(),
                            Some(tools) if tools.is_empty() => "none (deny all)".into(),
                            Some(tools) => tools.join(", "),
                        },
                        member.skills,
                        member.mcp_servers,
                    ),
                ));
            }
            if self.editing {
                rows.push((
                    Row::RemoveMember(index),
                    "  Remove member from draft".into(),
                ));
            }
        }
        if self.editing {
            rows.push((Row::AddMember, "+ Add member".into()));
        }
        let mut context: Vec<_> = self.working.context.iter().collect();
        context.sort_by_key(|(key, _)| *key);
        for (key, value) in context {
            rows.push((Row::ContextKey(key.clone()), format!("Context · {key}")));
            rows.push((Row::ContextValue(key.clone()), format!("  {value}")));
            if self.editing {
                rows.push((
                    Row::RemoveContext(key.clone()),
                    "  Remove context entry".into(),
                ));
            }
        }
        if self.editing {
            rows.push((Row::AddContext, "+ Add shared context".into()));
            rows.push((Row::Save, "Save".into()));
            rows.push((Row::Refresh, "Refresh for review (keeps draft)".into()));
            if let SaveStatus::Review(Some(latest)) = &self.save_status {
                rows.push((
                    Row::Observed,
                    format!("Server revision {} · {}", latest.revision, latest.name),
                ));
                rows.push((Row::Observed, format!("Purpose: {}", latest.description)));
                for member in &latest.members {
                    rows.push((
                        Row::Observed,
                        format!(
                            "{} · {}",
                            member.role,
                            member.system_prompt.as_deref().unwrap_or("Not set")
                        ),
                    ));
                    rows.push((
                        Row::Observed,
                        format!(
                            "  Model: {} · coordinator: {}",
                            self.model_label(member.model_selection.as_ref(), inspect),
                            member.can_delegate
                        ),
                    ));
                    rows.push((
                        Row::Observed,
                        format!(
                            "  Tools: {:?} · skills: {:?} · read-only: {}",
                            member.allow_tools, member.skills, member.read_only
                        ),
                    ));
                    rows.push((
                        Row::Observed,
                        format!(
                            "  Turns: {:?}/{:?} · depth: {} · MCP: {:?}",
                            member.initial_turns,
                            member.max_turns,
                            member.max_delegation_depth,
                            member.mcp_servers
                        ),
                    ));
                }
                let mut entries: Vec<_> = latest.context.iter().collect();
                entries.sort_by_key(|(key, _)| *key);
                for (key, value) in entries {
                    rows.push((Row::Observed, format!("Context · {key}: {value}")));
                }
                rows.push((
                    Row::UseLatest,
                    format!(
                        "Use server revision {} · discard local edits",
                        latest.revision
                    ),
                ));
                rows.push((
                    Row::KeepDraft,
                    format!(
                        "Keep my complete draft · next Save replaces revision {}",
                        latest.revision
                    ),
                ));
            }
            rows.push((Row::Cancel, "Cancel · discard local edits".into()));
        }
        rows
    }

    fn begin(&mut self, operation: TeamEditorOperation) {
        if self.pending.is_some() {
            return;
        }
        self.next_operation += 1;
        self.model_pick = None;
        let request = TeamEditorRequest {
            target: self.target.clone(),
            operation_id: self.next_operation,
            operation,
        };
        self.message = match &request.operation {
            TeamEditorOperation::Save { .. } => {
                "Saving… Waiting for the server acknowledgement.".into()
            }
            TeamEditorOperation::Refresh { .. } => {
                "Reading current revision; your draft is retained.".into()
            }
            TeamEditorOperation::Models { .. } => "Loading available models…".into(),
        };
        self.pending = Some(request.clone());
        self.action = Some(ViewActionRequest {
            action: BottomPaneViewAction::TeamEditor(request),
            disposition: ViewActionDisposition::KeepOpen,
        });
    }

    fn activate(&mut self, row: Row) {
        if self.pending.is_some() {
            return;
        }
        match row {
            Row::Save => {
                if !matches!(self.save_status, SaveStatus::Ready) {
                    self.message = match self.save_status {
                        SaveStatus::Unconfirmed { .. } => "Save remains unconfirmed. Only an exact matching revision can confirm it; no write was replayed.",
                        _ => "Save is not replayed. Refresh and review the server state first.",
                    }.into();
                } else if let Err(errors) =
                    astra_services::team_persistence::validate_team(&self.working)
                {
                    self.message = errors
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("; ");
                } else {
                    self.begin(TeamEditorOperation::Save {
                        definition: Box::new(self.working.clone()),
                        create: self.baseline.is_none(),
                    });
                }
            }
            Row::Refresh => self.begin(TeamEditorOperation::Refresh {
                team_id: self.working.team_id.clone(),
            }),
            Row::Model(index) => {
                let member = &self.working.members[index];
                self.begin(TeamEditorOperation::Models {
                    agent_id: member.agent_id.clone(),
                    current: member.model_selection.clone(),
                });
            }
            Row::AddMember => {
                self.working.members.push(TeamMemberDef {
                    agent_id: uuid::Uuid::new_v4().to_string(),
                    ..Default::default()
                });
                let index = self.working.members.len() - 1;
                self.activate(Row::MemberName(index));
            }
            Row::RemoveMember(index) => {
                self.working.members.remove(index);
            }
            Row::Coordinator(index) => {
                let member = &mut self.working.members[index];
                member.can_delegate = !member.can_delegate;
                if member.can_delegate && member.max_delegation_depth == 0 {
                    member.max_delegation_depth = 1;
                }
            }
            Row::AddContext => {
                let key = (1..)
                    .map(|index| format!("Context {index}"))
                    .find(|key| !self.working.context.contains_key(key))
                    .expect("available context key");
                self.working.context.insert(key.clone(), String::new());
                self.activate(Row::ContextKey(key));
            }
            Row::RemoveContext(key) => {
                self.working.context.remove(&key);
            }
            Row::UseLatest | Row::KeepDraft => {
                if let SaveStatus::Review(Some(latest)) = &self.save_status {
                    let latest = latest.as_ref().clone();
                    if row == Row::UseLatest {
                        self.working = latest.clone();
                    } else {
                        self.working.revision = latest.revision;
                    }
                    self.baseline = Some(latest);
                    self.save_status = SaveStatus::Ready;
                    self.message =
                        "Review complete. Nothing was written; Save remains explicit.".into();
                }
            }
            Row::Cancel => {
                self.reopen = None;
                self.completed = true;
            }
            row => {
                let value = match &row {
                    Row::Name => &self.working.name,
                    Row::Description => &self.working.description,
                    Row::MemberName(index) => &self.working.members[*index].role,
                    Row::Responsibility(index) => self.working.members[*index]
                        .system_prompt
                        .as_deref()
                        .unwrap_or(""),
                    Row::ContextKey(key) => key,
                    Row::ContextValue(key) => &self.working.context[key],
                    _ => return,
                };
                let mut input = TextArea::new();
                input.set_text(value);
                self.input = Some((row, input));
            }
        }
        self.selected = self.selected.min(self.rows().len().saturating_sub(1));
    }

    fn finish_input(&mut self) {
        let Some((row, input)) = self.input.take() else {
            return;
        };
        let text = input.text().to_owned();
        match row {
            Row::Name => self.working.name = text.trim().to_owned(),
            Row::Description => self.working.description = text,
            Row::MemberName(index) => self.working.members[index].role = text.trim().to_owned(),
            Row::Responsibility(index) => {
                self.working.members[index].system_prompt =
                    (!text.trim().is_empty()).then_some(text)
            }
            Row::ContextKey(key) => {
                let text = text.trim().to_owned();
                if text.is_empty() || (text != key && self.working.context.contains_key(&text)) {
                    self.message = "Choose a nonempty, distinct context name.".into();
                    self.input = Some((Row::ContextKey(key), input));
                    return;
                }
                if let Some(value) = self.working.context.remove(&key) {
                    self.working.context.insert(text, value);
                }
            }
            Row::ContextValue(key) => {
                self.working.context.insert(key, text);
            }
            _ => {}
        }
        self.model_pick = None;
    }

    fn accept_saved(&mut self, accepted: &TeamDefinition, sent: &TeamDefinition) {
        let changed = self.working != *sent;
        if changed {
            // A read can confirm an earlier write after the user has kept
            // editing. Advance its base, never discard those later edits.
            self.working.revision = accepted.revision;
        } else {
            self.working = accepted.clone();
        }
        self.baseline = Some(accepted.clone());
        self.save_status = SaveStatus::Ready;
        self.editing = changed;
        self.selected = 0;
        self.message = if changed {
            "Earlier save confirmed. Your later edits remain unsaved; Save is explicit."
        } else {
            "Saved. Future admissions use this definition; running work is unchanged."
        }
        .into();
    }

    fn input_area(area: Rect) -> Rect {
        Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(2),
            area.width.saturating_sub(2),
            area.height.saturating_sub(6),
        )
    }

    fn model_picker(
        &mut self,
        request: &TeamEditorRequest,
        catalog: &[astra_services::ModelListItemResponse],
    ) -> ListSelectionView {
        let TeamEditorOperation::Models { agent_id, current } = &request.operation else {
            unreachable!()
        };
        self.model_pick = Some((request.operation_id, agent_id.clone()));
        let mut items = vec![SelectionItem {
            name: "Inherit parent model".into(),
            description: None,
            is_current: current.is_none(),
        }];
        let result = |selection| ViewResult::TeamMemberModel {
            target: request.target.clone(),
            operation_id: request.operation_id,
            agent_id: agent_id.clone(),
            selection,
        };
        let mut results = vec![result(None)];
        for entry in catalog {
            if !crate::cli::session::session_runtime::model_list_entry_is_active(entry) {
                continue;
            }
            let Some(model) =
                crate::cli::session::session_runtime::model_selection_from_list_entry(entry)
            else {
                continue;
            };
            self.model_labels
                .insert(model.offering_id.clone(), model.name.clone());
            items.push(SelectionItem {
                name: model.name,
                description: Some(format!("{} · {}", entry.provider, entry.access_label)),
                is_current: current
                    .as_ref()
                    .is_some_and(|selected| selected.offering_id == model.offering_id),
            });
            results.push(result(Some(astra_turn_types::ModelSelection {
                offering_id: model.offering_id,
            })));
        }
        ListSelectionView::new(
            items,
            Some("Member model · conversation model unchanged".into()),
        )
        .with_results(results)
        .with_footer_hint("Type to filter · Enter choose · Esc keep draft")
    }
}

impl BottomPaneView for TeamEditorView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.width.set(area.width);
        if let Some(inspection) = &self.inspection {
            inspection.render(area, buf);
            return;
        }
        if area.width == 0 || area.height == 0 {
            return;
        }
        let theme = crate::tui::theme::current();
        let title = if self.working.name.is_empty() {
            "New team"
        } else {
            &self.working.name
        };
        let heading = format!(
            "{title} · {}",
            if self.baseline.is_none() {
                "Draft".into()
            } else {
                format!("Revision {}", self.working.revision)
            }
        );
        Line::from(heading)
            .style(Style::default().fg(theme.accent))
            .render(Rect::new(area.x, area.y, area.width, 1), buf);
        if area.height < 2 {
            return;
        }
        let subtitle = if let Some((row, _)) = &self.input {
            match row {
                Row::Name => "Edit team name",
                Row::Description => "Edit team purpose",
                Row::MemberName(_) => "Edit member name",
                Row::Responsibility(_) => "Edit member responsibility",
                Row::ContextKey(_) => "Edit shared context name",
                Row::ContextValue(_) => "Edit shared context value",
                _ => "Edit field",
            }
        } else if self.working.members.is_empty() {
            "Draft · add members before starting work"
        } else {
            "Team configuration · not live execution status"
        };
        Line::from(subtitle).render(Rect::new(area.x, area.y + 1, area.width, 1), buf);
        let body = Self::input_area(area);
        if let Some((_, input)) = &self.input {
            input.render(body, buf);
        } else {
            let rows = self.rows();
            let start = self
                .selected
                .saturating_sub(body.height.saturating_sub(1) as usize);
            for (index, (_, label)) in rows
                .iter()
                .enumerate()
                .skip(start)
                .take(body.height as usize)
            {
                let marker = if self.editing && index == self.selected {
                    "› "
                } else {
                    "  "
                };
                Line::from(format!("{marker}{}", label.replace('\n', " ")))
                    .style(if self.editing && index == self.selected {
                        Style::default().fg(theme.selected_fg).bg(theme.selected_bg)
                    } else {
                        Style::default().fg(theme.fg)
                    })
                    .render(
                        Rect::new(body.x, body.y + (index - start) as u16, body.width, 1),
                        buf,
                    );
            }
        }
        let note = match &self.save_status {
            SaveStatus::Review(Some(latest)) => format!(
                "Server revision {}: {} · {}. Review member/context differences before choosing a base.",
                latest.revision, latest.name, latest.description
            ),
            _ => self.message.clone(),
        };
        let foot_y = area.bottom().saturating_sub(4).max(area.y + 2);
        Paragraph::new(note)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(theme.dim))
            .render(
                Rect::new(
                    area.x,
                    foot_y,
                    area.width,
                    area.bottom().saturating_sub(foot_y).min(2),
                ),
                buf,
            );
        let hint = self.hint_keys().unwrap_or_default();
        let hint_y = area.bottom().saturating_sub(2).max(area.y + 2);
        Paragraph::new(hint).wrap(Wrap { trim: false }).render(
            Rect::new(
                area.x,
                hint_y,
                area.width,
                area.bottom().saturating_sub(hint_y),
            ),
            buf,
        );
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.width.set(width);
        if let Some(inspection) = &self.inspection {
            return inspection.desired_height(width);
        }
        (self.rows().len() as u16).saturating_add(6).clamp(10, 24)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if let Some(inspection) = &mut self.inspection {
            inspection.handle_key(key);
            if inspection.is_complete() || key.code == KeyCode::Char('i') {
                self.inspection = None;
            }
            return;
        }
        if let Some((_, input)) = &mut self.input {
            if key.code == KeyCode::Esc {
                self.input = None;
                return;
            }
            match input.handle_key(key) {
                TextAreaAction::Submit => self.finish_input(),
                TextAreaAction::Cancel | TextAreaAction::Quit => self.input = None,
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Esc {
            self.on_ctrl_c();
            return;
        }
        if self.pending.is_some() {
            return;
        }
        if key.code == KeyCode::Char('i') && key.modifiers.is_empty() {
            let lines = self
                .display_rows(true)
                .into_iter()
                .flat_map(|(_, label)| {
                    label
                        .split('\n')
                        .flat_map(|line| textwrap::wrap(line, usize::from(self.width.get().max(1))))
                        .map(|line| line.into_owned())
                        .collect::<Vec<_>>()
                })
                .collect();
            self.inspection = Some(InfoView::from_plain("Team inspection · read-only", lines));
            return;
        }
        if !self.editing {
            match key.code {
                KeyCode::Char('e') => self.editing = true,
                KeyCode::Char('n') => {
                    self.editing = true;
                    self.activate(Row::AddMember);
                }
                KeyCode::Enter if !self.working.members.is_empty() => {
                    if !self.selection_available {
                        self.message =
                            "After this turn ends, reopen /team to choose a lead.".into();
                        return;
                    }
                    self.result = Some(ViewResult::UseTeam {
                        team: Arc::new(self.working.clone()),
                        attachment_epoch: self.target.attachment_epoch,
                        owner: self.target.owner.clone(),
                    });
                    self.completed = true;
                }
                KeyCode::Down => {
                    self.selected = (self.selected + 1).min(self.rows().len().saturating_sub(1))
                }
                KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                _ => {}
            }
            return;
        }
        self.model_pick = None;
        match key.code {
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.activate(Row::Save)
            }
            KeyCode::Down | KeyCode::Tab => self.selected = (self.selected + 1) % self.rows().len(),
            KeyCode::Up | KeyCode::BackTab => {
                self.selected = self
                    .selected
                    .checked_sub(1)
                    .unwrap_or(self.rows().len() - 1)
            }
            KeyCode::Enter => self.activate(self.rows()[self.selected].0.clone()),
            _ => {}
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.input
            .as_ref()?
            .1
            .cursor_position(Self::input_area(area))
    }
    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }
    fn on_ctrl_c(&mut self) -> CancellationEvent {
        if self.inspection.take().is_some() {
            return CancellationEvent::Consumed;
        }
        if !self.confirm_close
            && self.pending.as_ref().is_some_and(|pending| {
                matches!(&pending.operation, TeamEditorOperation::Save { .. })
            })
        {
            self.confirm_close = true;
            self.message = "Save may still complete. Esc again discards this local draft, not the server write.".into();
        } else {
            self.completed = true;
        }
        CancellationEvent::Consumed
    }
    fn is_complete(&self) -> bool {
        self.completed
    }
    fn completion(&self) -> Option<ViewCompletion> {
        self.completed.then(|| ViewCompletion {
            result: self.result.clone(),
            reopen: self.result.is_none().then(|| self.reopen.clone()).flatten(),
        })
    }
    fn take_action_request(&mut self) -> Option<ViewActionRequest> {
        self.action.take()
    }
    fn handle_paste(&mut self, text: &str) -> bool {
        if let Some((_, input)) = &mut self.input {
            input.insert_str(text);
        }
        true
    }
    fn is_in_paste_burst(&self) -> bool {
        self.input
            .as_ref()
            .is_some_and(|(_, input)| input.paste_burst_active())
    }
    fn pre_draw_tick(&mut self, _now: std::time::Instant) {
        if let Some((_, input)) = &mut self.input {
            input.flush_paste_burst();
        }
    }
    fn hint_keys(&self) -> Option<String> {
        if self.inspection.is_some() {
            return Some("↑↓ scroll · I / Esc back · draft unchanged".into());
        }
        Some(
            if self.input.is_some() {
                "Enter apply field · Shift+Enter newline · Esc back"
            } else if self.pending.as_ref().is_some_and(|pending| {
                matches!(&pending.operation, TeamEditorOperation::Save { .. })
            }) {
                "Saving… Draft retained until acknowledgement or timeout"
            } else if self.pending.is_some() {
                "Loading… Esc cancel observation"
            } else if self.editing && self.reopen.is_some() {
                "↑↓ select · Enter edit · Ctrl+S save · I inspect · Esc back"
            } else if self.editing {
                "↑↓ select · Enter edit · Ctrl+S save · I inspect · Esc cancel"
            } else if self.working.members.is_empty() && self.reopen.is_some() {
                "N add member · E edit roster · Esc back"
            } else if self.working.members.is_empty() {
                "N add member · E edit roster · Esc close"
            } else if !self.selection_available {
                "Selection unavailable during a turn · E edit roster · I inspect · Esc back"
            } else if self.reopen.is_none() {
                "Enter use team · E edit roster · I inspect · Esc close"
            } else {
                "Enter use team · E edit roster · I inspect · Esc back"
            }
            .into(),
        )
    }

    fn team_editor_pending(&self, request: &TeamEditorRequest) -> bool {
        self.pending.as_ref() == Some(request) && !self.completed
    }

    fn update_team_editor(&mut self, update: &TeamEditorUpdate) -> Option<Box<dyn BottomPaneView>> {
        if !self.team_editor_pending(&update.request) {
            return None;
        }
        self.pending = None;
        self.confirm_close = false;
        match &update.response {
            TeamEditorResponse::Saved(Ok(accepted)) => {
                if let TeamEditorOperation::Save { definition, .. } = &update.request.operation {
                    self.accept_saved(accepted, definition);
                }
            }
            TeamEditorResponse::Saved(Err(error)) => {
                if let TeamEditorOperation::Save { definition, create } = &update.request.operation
                {
                    self.save_status = match error {
                        TeamWriteError::ConflictOrMissing => SaveStatus::Review(None),
                        TeamWriteError::Unconfirmed => SaveStatus::Unconfirmed {
                            sent: definition.clone(),
                            create: *create,
                        },
                        _ => SaveStatus::Ready,
                    };
                }
                self.message = format!("{error}. Your draft is retained; no write was replayed.");
            }
            TeamEditorResponse::Refreshed(Ok(Some(latest))) => {
                if latest.team_id != self.working.team_id || latest.user_id != self.working.user_id
                {
                    self.message =
                        "Refresh returned a different owner or Team. Draft unchanged.".into();
                } else if let SaveStatus::Unconfirmed { sent, create } = &self.save_status {
                    let mut expected = sent.as_ref().clone();
                    let revision = if *create {
                        Some(1)
                    } else {
                        sent.revision.checked_add(1)
                    };
                    if let Some(revision) = revision {
                        expected.revision = revision;
                    }
                    if revision.is_some() && latest == &expected {
                        let sent = sent.clone();
                        self.accept_saved(latest, &sent);
                    } else {
                        self.message = "Read does not confirm the exact sent definition and revision. Save remains unconfirmed; draft retained.".into();
                    }
                } else {
                    self.save_status = SaveStatus::Review(Some(Box::new(latest.clone())));
                }
            }
            TeamEditorResponse::Refreshed(Ok(None)) => {
                self.message = if matches!(self.save_status, SaveStatus::Unconfirmed { .. }) {
                    "Team not observed. The save may still commit; draft retained and replay blocked."
                } else {
                    "This Team is not present on the server. Your draft is retained."
                }.into();
            }
            TeamEditorResponse::Refreshed(Err(error)) => {
                self.message = format!("Refresh failed: {error}. Draft unchanged.")
            }
            TeamEditorResponse::Models(Ok(catalog)) => {
                self.message = "Model selection changes only this member's draft.".into();
                return Some(Box::new(self.model_picker(&update.request, catalog)));
            }
            TeamEditorResponse::Models(Err(error)) => {
                self.message = format!("Model catalog unavailable: {error}")
            }
        }
        None
    }

    fn select_team_member_model(
        &mut self,
        target: &TeamEditorTarget,
        operation_id: u64,
        agent_id: &str,
        selection: Option<astra_turn_types::ModelSelection>,
    ) -> bool {
        if &self.target != target
            || !self
                .model_pick
                .as_ref()
                .is_some_and(|(id, member)| *id == operation_id && member == agent_id)
            || self.completed
        {
            return false;
        }
        self.model_pick = None;
        if let Some(member) = self
            .working
            .members
            .iter_mut()
            .find(|member| member.agent_id == agent_id)
        {
            member.model_selection = selection;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> TeamEditorView {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None).unwrap();
        TeamEditorView::new(
            Some(TeamDefinition {
                team_id: "stable-team".into(),
                user_id: "owner".into(),
                name: "研发团队".into(),
                description: "Ship carefully".into(),
                revision: 7,
                context: Default::default(),
                members: vec![TeamMemberDef {
                    agent_id: "stable-member".into(),
                    role: "Reviewer".into(),
                    system_prompt: Some("Review changes".into()),
                    read_only: true,
                    allow_tools: Some(Vec::new()),
                    skills: vec!["review".into()],
                    max_turns: Some(8),
                    ..Default::default()
                }],
            }),
            TeamEditorOwner(Arc::new(HttpTeamStore::new(&api, None))),
            3,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn roster_fields_keep_identity_and_authority_with_unicode_and_narrow_layout() {
        let mut view = fixture();
        let original = view.working.clone();
        view.handle_key(key(KeyCode::Char('e')));
        view.handle_key(key(KeyCode::Enter));
        view.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        view.handle_paste("  质量 🦀 团队  ");
        view.handle_key(key(KeyCode::Enter));
        view.activate(Row::Responsibility(0));
        view.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let responsibility = "  审查并报告风险\nKeep evidence.  \n";
        view.handle_paste(responsibility);
        view.handle_key(key(KeyCode::Enter));
        assert_eq!(view.working.name, "质量 🦀 团队");
        assert_eq!(
            view.working.members[0].system_prompt.as_deref(),
            Some(responsibility)
        );
        let mut expected = original.clone();
        expected.name.clone_from(&view.working.name);
        expected.members[0]
            .system_prompt
            .clone_from(&view.working.members[0].system_prompt);
        assert_eq!(
            view.working, expected,
            "all hidden capabilities and stable identities survive editing"
        );
        assert_eq!(view.baseline.as_ref(), Some(&original));
        view.activate(Row::AddMember);
        view.handle_paste("Writer");
        view.handle_key(key(KeyCode::Enter));
        assert_ne!(
            view.working.members[1].agent_id,
            view.working.members[0].agent_id
        );
        view.activate(Row::RemoveMember(1));
        view.activate(Row::AddContext);
        view.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        view.handle_paste("  约束  ");
        view.handle_key(key(KeyCode::Enter));
        view.activate(Row::ContextValue("约束".into()));
        let context = " \n  Preserve 用户 data\t\n ";
        view.handle_paste(context);
        view.handle_key(key(KeyCode::Enter));
        assert_eq!(view.working.context["约束"].as_bytes(), context.as_bytes());
        view.activate(Row::RemoveContext("约束".into()));
        assert_eq!(view.working, expected);
        for (width, height) in [(1, 1), (20, 3), (32, 14), (80, 24)] {
            let area = Rect::new(0, 0, width, height);
            let mut buffer = Buffer::empty(area);
            view.render(area, &mut buffer);
            if width >= 32 {
                let text = crate::tui::testing::render::buffer_to_string(&buffer);
                // Wide glyphs occupy a symbol cell plus continuation cells;
                // the ASCII snapshot helper includes those padding spaces.
                for (x, symbol) in [(0, "质"), (2, "量"), (5, "🦀"), (8, "团"), (10, "队")] {
                    assert_eq!(buffer[(x, 0)].symbol(), symbol, "{text}");
                }
                assert!(text.contains("save") && text.contains("cancel"), "{text}");
            }
        }
        view.handle_key(key(KeyCode::Esc));
        assert!(view.is_complete());
        assert!(
            view.take_action_request().is_none(),
            "cancel has no network effect"
        );
    }

    #[test]
    fn roster_save_conflict_unknown_and_refresh_do_not_replay_or_replace_the_draft() {
        for (create, error) in [
            (false, TeamWriteError::ConflictOrMissing),
            (false, TeamWriteError::Unconfirmed),
            (true, TeamWriteError::Unconfirmed),
        ] {
            let mut view = fixture();
            if create {
                view.baseline = None;
                view.working.revision = 1;
            }
            view.editing = true;
            view.working.name = "Local rename".into();
            let draft = view.working.clone();
            view.activate(Row::Save);
            let request = view.pending.clone().unwrap();
            assert!(matches!(
                view.take_action_request().unwrap().disposition,
                ViewActionDisposition::KeepOpen
            ));
            view.handle_key(key(KeyCode::Esc));
            assert!(
                !view.is_complete(),
                "an in-flight write retains draft custody"
            );
            let mut stale = request.clone();
            stale.operation_id += 1;
            view.update_team_editor(&TeamEditorUpdate {
                request: stale,
                response: TeamEditorResponse::Saved(Err(error.clone())),
            });
            assert!(view.team_editor_pending(&request));
            view.update_team_editor(&TeamEditorUpdate {
                request,
                response: TeamEditorResponse::Saved(Err(error.clone())),
            });
            assert_eq!(view.working, draft);
            assert_eq!(
                view.baseline.as_ref().map(|team| team.revision),
                (!create).then_some(7)
            );
            view.activate(Row::Save);
            assert!(
                view.take_action_request().is_none(),
                "no automatic or blind retry"
            );
            if error == TeamWriteError::Unconfirmed {
                // Edits after timeout do not alter the payload being confirmed.
                view.working.description = "Later local edits".into();
                let later_draft = view.working.clone();
                let mut accepted = draft.clone();
                accepted.revision = if create { 1 } else { 8 };
                let mut wrong_payload = accepted.clone();
                wrong_payload.members[0].system_prompt = Some("Other writer".into());
                let mut wrong_revision = accepted.clone();
                wrong_revision.revision += 1;
                for observation in [
                    None,
                    Some(draft.clone()),
                    Some(wrong_payload),
                    Some(wrong_revision),
                ] {
                    // For create, the sent revision already is the accepted one.
                    if create && observation.as_ref() == Some(&draft) {
                        continue;
                    }
                    view.activate(Row::Refresh);
                    let refresh = view.pending.clone().unwrap();
                    view.take_action_request();
                    view.update_team_editor(&TeamEditorUpdate {
                        request: refresh,
                        response: TeamEditorResponse::Refreshed(Ok(observation)),
                    });
                    for row in [Row::KeepDraft, Row::UseLatest, Row::Save] {
                        view.activate(row);
                        assert!(
                            view.take_action_request().is_none(),
                            "unknown write cannot be rebased or replayed"
                        );
                    }
                    assert_eq!(view.working, later_draft);
                    assert!(matches!(view.save_status, SaveStatus::Unconfirmed { .. }));
                }
                view.activate(Row::Refresh);
                let refresh = view.pending.clone().unwrap();
                view.take_action_request();
                view.update_team_editor(&TeamEditorUpdate {
                    request: refresh,
                    response: TeamEditorResponse::Refreshed(Ok(Some(accepted.clone()))),
                });
                assert_eq!(view.baseline, Some(accepted));
                assert_eq!(view.working.description, "Later local edits");
                assert_eq!(view.working.revision, if create { 1 } else { 8 });
                assert!(view.editing);
                assert!(matches!(view.save_status, SaveStatus::Ready));
                assert!(
                    view.take_action_request().is_none(),
                    "matching read confirms without a write"
                );
                continue;
            }
            view.activate(Row::Refresh);
            let refresh = view.pending.clone().unwrap();
            view.take_action_request();
            let mut latest = draft.clone();
            latest.name = "Concurrent rename".into();
            latest.members[0].system_prompt = Some("New responsibility".into());
            latest.revision = 8;
            view.update_team_editor(&TeamEditorUpdate {
                request: refresh,
                response: TeamEditorResponse::Refreshed(Ok(Some(latest.clone()))),
            });
            assert_eq!(
                view.working, draft,
                "reading does not silently merge or discard"
            );
            assert!(
                view.rows()
                    .iter()
                    .any(|(_, text)| text.contains("New responsibility"))
            );
            view.activate(Row::KeepDraft);
            assert_eq!(view.working.name, "Local rename");
            assert_eq!(view.working.team_id, draft.team_id);
            assert_eq!(view.working.revision, 8);
            assert!(
                view.take_action_request().is_none(),
                "review is not a write"
            );
            view.activate(Row::Save);
            let request = view.pending.clone().unwrap();
            let mut accepted = view.working.clone();
            accepted.revision = 9;
            view.update_team_editor(&TeamEditorUpdate {
                request,
                response: TeamEditorResponse::Saved(Ok(accepted.clone())),
            });
            assert_eq!(view.working, accepted);
            assert_eq!(view.baseline, Some(accepted));
            assert!(!view.editing);
        }
    }

    #[test]
    fn member_picker_uses_exact_offering_and_cancel_preserves_the_same_draft() {
        let mut view = fixture();
        view.working.members[0].model_selection = Some(astra_turn_types::ModelSelection {
            offering_id: "offer-b".into(),
        });
        assert_eq!(
            view.model_label(view.working.members[0].model_selection.as_ref(), false),
            "Configured model (exact)"
        );
        for inspecting in [true, false] {
            view.handle_key(key(KeyCode::Char('i')));
            let area = Rect::new(0, 0, 80, 24);
            let mut buffer = Buffer::empty(area);
            view.render(area, &mut buffer);
            let rendered = crate::tui::testing::render::buffer_to_string(&buffer);
            assert_eq!(rendered.contains("offer-b"), inspecting);
            if inspecting {
                assert!(
                    rendered.contains("Read-only: true") && rendered.contains("deny all"),
                    "{rendered}"
                );
            }
            assert!(view.take_action_request().is_none(), "inspection is local");
        }
        view.working.members[0].model_selection = None;
        view.editing = true;
        view.working.description = "Unfinished purpose".into();
        let draft = view.working.clone();
        let catalog: Vec<astra_services::ModelListItemResponse> = ["offer-a", "offer-b"].into_iter().map(|id| serde_json::from_value(serde_json::json!({
            "offering_id": id, "access_id": "self-hosted", "access_kind": "self_hosted",
            "access_label": "Self-hosted", "execution_placement": "server", "name": "Same display name",
            "provider": "openai", "is_active": true, "context_window": 8192,
            "max_completion_tokens": null, "architecture": null, "thinking_capability": null
        })).unwrap()).collect();
        for cancel in [true, false] {
            view.activate(Row::Model(0));
            let request = view.pending.clone().unwrap();
            view.take_action_request();
            let mut picker = view
                .update_team_editor(&TeamEditorUpdate {
                    request: request.clone(),
                    response: TeamEditorResponse::Models(Ok(catalog.clone())),
                })
                .unwrap();
            if cancel {
                picker.handle_key(key(KeyCode::Esc));
                assert!(picker.completion().unwrap().result.is_none());
                assert_eq!(view.working, draft);
            } else {
                picker.handle_key(key(KeyCode::Down));
                picker.handle_key(key(KeyCode::Down));
                picker.handle_key(key(KeyCode::Enter));
                let Some(ViewResult::TeamMemberModel {
                    target,
                    operation_id,
                    agent_id,
                    selection,
                }) = picker.completion().unwrap().result
                else {
                    panic!("typed member selection")
                };
                assert_eq!(selection.as_ref().unwrap().offering_id, "offer-b");
                let mut foreign = target.clone();
                foreign.editor_id = uuid::Uuid::new_v4();
                assert!(!view.select_team_member_model(
                    &foreign,
                    operation_id,
                    &agent_id,
                    selection.clone()
                ));
                assert!(view.select_team_member_model(&target, operation_id, &agent_id, selection));
                assert_eq!(
                    view.model_label(view.working.members[0].model_selection.as_ref(), false),
                    "Same display name"
                );
                assert!(
                    !view.select_team_member_model(&target, operation_id, &agent_id, None),
                    "duplicate result is inert"
                );
            }
        }
        view.activate(Row::Model(0));
        let request = view.pending.clone().unwrap();
        let mut picker = view
            .update_team_editor(&TeamEditorUpdate {
                request,
                response: TeamEditorResponse::Models(Ok(catalog)),
            })
            .unwrap();
        picker.handle_key(key(KeyCode::Up));
        picker.handle_key(key(KeyCode::Up));
        picker.handle_key(key(KeyCode::Enter));
        let Some(ViewResult::TeamMemberModel {
            target,
            operation_id,
            agent_id,
            selection,
        }) = picker.completion().unwrap().result
        else {
            panic!("inherit result")
        };
        assert!(selection.is_none());
        assert!(view.select_team_member_model(&target, operation_id, &agent_id, selection));
        assert_eq!(view.working, draft);
    }
}
