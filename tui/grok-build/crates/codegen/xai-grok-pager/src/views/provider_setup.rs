//! `/login` provider setup wizard.
//!
//! The grok build ships `/login` as an xAI-account OAuth flow. OpenBuild talks
//! to the ACP kernel instead, whose only advertised auth method
//! (`opencode-login`) is deliberately treated as non-interactive, so the
//! upstream command had no method to start and returned without drawing
//! anything. This module repurposes the command: it collects one third-party
//! provider (OpenAI- or Anthropic-compatible endpoint) and hands it to
//! [`openbuild_backend::providers`], which writes the file the kernel reads.
//!
//! The widget is drawn like `new_worktree_dialog` rather than through
//! `modal_window` because it is a form, not a list: every row owns a
//! `LineEditor` and Tab/arrow keys move between them.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::input::line_editor::{LineEditOutcome, LineEditor};
use crate::theme::Theme;
use openbuild_backend::providers::{
    ANTHROPIC_COMPATIBLE_PACKAGE, EFFORT_LEVELS, OPENAI_COMPATIBLE_PACKAGE, ProviderDraft,
};

/// Validated result of a finished wizard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSetupSubmission {
    pub draft: ProviderDraft,
    pub make_default: bool,
}

/// How the wizard consumed an input event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderSetupOutcome {
    Submitted(ProviderSetupSubmission),
    Cancelled,
    Changed,
    Unchanged,
}

/// Editable rows, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Id,
    Name,
    BaseUrl,
    ApiKey,
    ModelId,
    ModelName,
    Context,
    Output,
    Package,
    ReasoningEffort,
    DefaultModel,
    Save,
}

const FIELDS: [Field; 12] = [
    Field::Id,
    Field::Name,
    Field::BaseUrl,
    Field::ApiKey,
    Field::ModelId,
    Field::ModelName,
    Field::Context,
    Field::Output,
    Field::Package,
    Field::ReasoningEffort,
    Field::DefaultModel,
    Field::Save,
];

/// The empty first entry means "do not configure one": the provider default
/// applies. Everything after it comes from [`EFFORT_LEVELS`], the list that is
/// also written to the config, so the picker can never offer a level the saved
/// capability omits.
const UNSET_EFFORT_CHOICE: &str = "";
const EFFORT_CHOICE_COUNT: usize = EFFORT_LEVELS.len() + 1;

/// The level at `index`, or `None` when the index is past the end.
fn effort_choice_at(index: usize) -> Option<&'static str> {
    if index == 0 {
        return Some(UNSET_EFFORT_CHOICE);
    }
    EFFORT_LEVELS.get(index - 1).copied()
}

/// The index of `level` in the picker, or the "unset" row when unknown.
fn effort_choice_index(level: &str) -> usize {
    EFFORT_LEVELS
        .iter()
        .position(|candidate| *candidate == level)
        .map_or(0, |index| index + 1)
}

/// State of the provider setup wizard.
pub struct ProviderSetupState {
    id: LineEditor,
    name: LineEditor,
    base_url: LineEditor,
    api_key: LineEditor,
    model_id: LineEditor,
    model_name: LineEditor,
    context: LineEditor,
    output: LineEditor,
    use_anthropic_package: bool,
    make_default: bool,
    /// Index into the picker; 0 leaves the provider default alone.
    effort_choice: usize,
    focus: usize,
    /// Validation error shown under the form; cleared on the next edit.
    error: Option<String>,
}

impl Default for ProviderSetupState {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderSetupState {
    pub fn new() -> Self {
        let mut context = LineEditor::default();
        context.set_text(openbuild_backend::providers::DEFAULT_CONTEXT.to_string());
        let mut output = LineEditor::default();
        output.set_text(openbuild_backend::providers::DEFAULT_OUTPUT.to_string());
        Self {
            id: LineEditor::default(),
            name: LineEditor::default(),
            base_url: LineEditor::default(),
            api_key: LineEditor::default(),
            model_id: LineEditor::default(),
            model_name: LineEditor::default(),
            context,
            output,
            use_anthropic_package: false,
            make_default: true,
            effort_choice: 0,
            focus: 0,
            error: None,
        }
    }

    /// Pre-fill from an existing provider so re-running `/login` edits instead
    /// of duplicating.
    pub fn prefilled(draft: &ProviderDraft, make_default: bool) -> Self {
        let mut state = Self::new();
        state.id.set_text(draft.id.clone());
        state.name.set_text(draft.name.clone());
        state.base_url.set_text(draft.base_url.clone());
        state.api_key.set_text(draft.api_key.clone());
        state.model_id.set_text(draft.model_id.clone());
        state.model_name.set_text(draft.model_name.clone());
        state.context.set_text(draft.context.to_string());
        state.output.set_text(draft.output.to_string());
        state.use_anthropic_package = draft.package == ANTHROPIC_COMPATIBLE_PACKAGE;
        state.effort_choice = draft
            .reasoning_effort
            .as_deref()
            .map(effort_choice_index)
            .unwrap_or(0);
        state.make_default = make_default;
        state
    }

    fn field(&self, field: Field) -> &LineEditor {
        match field {
            Field::Id => &self.id,
            Field::Name => &self.name,
            Field::BaseUrl => &self.base_url,
            Field::ApiKey => &self.api_key,
            Field::ModelId => &self.model_id,
            Field::ModelName => &self.model_name,
            Field::Context => &self.context,
            Field::Output => &self.output,
            Field::Package | Field::ReasoningEffort | Field::DefaultModel | Field::Save => &self.id,
        }
    }

    fn field_mut(&mut self, field: Field) -> Option<&mut LineEditor> {
        Some(match field {
            Field::Id => &mut self.id,
            Field::Name => &mut self.name,
            Field::BaseUrl => &mut self.base_url,
            Field::ApiKey => &mut self.api_key,
            Field::ModelId => &mut self.model_id,
            Field::ModelName => &mut self.model_name,
            Field::Context => &mut self.context,
            Field::Output => &mut self.output,
            Field::Package | Field::ReasoningEffort | Field::DefaultModel | Field::Save => return None,
        })
    }

    fn focused_field(&self) -> Field {
        FIELDS[self.focus.min(FIELDS.len() - 1)]
    }

    fn is_editor_field(field: Field) -> bool {
        matches!(
            field,
            Field::Id
                | Field::Name
                | Field::BaseUrl
                | Field::ApiKey
                | Field::ModelId
                | Field::ModelName
                | Field::Context
                | Field::Output
        )
    }

    /// Build a draft from the current text, normalizing empty optional names.
    pub fn draft(&self) -> ProviderDraft {
        let mut draft = ProviderDraft {
            id: self.id.text().to_string(),
            name: self.name.text().to_string(),
            base_url: self.base_url.text().to_string(),
            api_key: self.api_key.text().to_string(),
            package: if self.use_anthropic_package {
                ANTHROPIC_COMPATIBLE_PACKAGE
            } else {
                OPENAI_COMPATIBLE_PACKAGE
            }
            .to_string(),
            model_id: self.model_id.text().to_string(),
            model_name: self.model_name.text().to_string(),
            context: self.context.text().trim().parse().unwrap_or(0),
            output: self.output.text().trim().parse().unwrap_or(0),
            reasoning_effort: self.reasoning_effort().map(str::to_string),
        };
        draft.normalize();
        draft
    }

    /// The selected level, or `None` for "provider default".
    fn reasoning_effort(&self) -> Option<&'static str> {
        effort_choice_at(self.effort_choice).filter(|choice| !choice.is_empty())
    }

    fn cycle_effort(&mut self, forward: bool) {
        self.effort_choice = if forward {
            (self.effort_choice + 1) % EFFORT_CHOICE_COUNT
        } else {
            (self.effort_choice + EFFORT_CHOICE_COUNT - 1) % EFFORT_CHOICE_COUNT
        };
        self.error = None;
    }

    fn submit(&mut self) -> ProviderSetupOutcome {
        let draft = self.draft();
        match draft.validate() {
            Ok(()) => ProviderSetupOutcome::Submitted(ProviderSetupSubmission {
                draft,
                make_default: self.make_default,
            }),
            Err(error) => {
                self.error = Some(error);
                ProviderSetupOutcome::Changed
            }
        }
    }

    fn move_focus(&mut self, delta: isize) -> ProviderSetupOutcome {
        let len = FIELDS.len() as isize;
        let next = (self.focus as isize + delta).rem_euclid(len);
        self.focus = next as usize;
        ProviderSetupOutcome::Changed
    }

    /// Route one key event. Tab/Shift+Tab and arrows move between rows;
    /// printable keys go to the focused editor, except on the two toggle rows.
    pub fn handle_key(&mut self, key: &KeyEvent) -> ProviderSetupOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let focused = self.focused_field();
        let on_editor = Self::is_editor_field(focused);
        match key.code {
            KeyCode::Esc => return ProviderSetupOutcome::Cancelled,
            KeyCode::Enter => return self.submit(),
            KeyCode::Tab => return self.move_focus(1),
            KeyCode::BackTab => return self.move_focus(-1),
            KeyCode::Down if on_editor => return self.move_focus(1),
            KeyCode::Up if on_editor => return self.move_focus(-1),
            _ => {}
        }

        let field = focused;
        match field {
            Field::Package => match key.code {
                KeyCode::Left
                | KeyCode::Right
                | KeyCode::Char(' ')
                | KeyCode::Char('h')
                | KeyCode::Char('l') => {
                    self.use_anthropic_package = !self.use_anthropic_package;
                    self.error = None;
                    ProviderSetupOutcome::Changed
                }
                KeyCode::Down => self.move_focus(1),
                KeyCode::Up => self.move_focus(-1),
                _ => ProviderSetupOutcome::Unchanged,
            },
            Field::DefaultModel => match key.code {
                KeyCode::Char(' ')
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Char('h')
                | KeyCode::Char('l') => {
                    self.make_default = !self.make_default;
                    ProviderSetupOutcome::Changed
                }
                KeyCode::Down => self.move_focus(1),
                KeyCode::Up => self.move_focus(-1),
                _ => ProviderSetupOutcome::Unchanged,
            },
            Field::ReasoningEffort => match key.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    self.cycle_effort(false);
                    ProviderSetupOutcome::Changed
                }
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                    self.cycle_effort(true);
                    ProviderSetupOutcome::Changed
                }
                KeyCode::Down => self.move_focus(1),
                KeyCode::Up => self.move_focus(-1),
                _ => ProviderSetupOutcome::Unchanged,
            },
            Field::Save => match key.code {
                KeyCode::Down => self.move_focus(1),
                KeyCode::Up => self.move_focus(-1),
                _ => ProviderSetupOutcome::Unchanged,
            },
            _ => {
                // Ctrl+Down/Up would collide with editor movement, so only
                // unmodified arrows change rows; the editor keeps the rest.
                if ctrl && matches!(key.code, KeyCode::Down | KeyCode::Up) {
                    return ProviderSetupOutcome::Unchanged;
                }
                let Some(editor) = self.field_mut(field) else {
                    return ProviderSetupOutcome::Unchanged;
                };
                let outcome = editor.handle_key(key);
                match outcome {
                    LineEditOutcome::Unhandled => ProviderSetupOutcome::Unchanged,
                    _ => {
                        self.error = None;
                        ProviderSetupOutcome::Changed
                    }
                }
            }
        }
    }

    /// Paste goes to the focused editor; toggle rows ignore it.
    pub fn insert_paste(&mut self, text: &str) -> ProviderSetupOutcome {
        let field = self.focused_field();
        let Some(editor) = self.field_mut(field) else {
            return ProviderSetupOutcome::Unchanged;
        };
        match editor.insert_paste(text) {
            LineEditOutcome::Unhandled => ProviderSetupOutcome::Unchanged,
            _ => {
                self.error = None;
                ProviderSetupOutcome::Changed
            }
        }
    }
}

const DIALOG_WIDTH: u16 = 78;
const DIALOG_HEIGHT: u16 = 22;
const INNER_PAD: u16 = 2;

/// Render the provider setup wizard centered in `area`.
pub fn render_provider_setup(area: Rect, buf: &mut Buffer, state: &ProviderSetupState) {
    let theme = Theme::current();
    if area.height < 8 || area.width < 30 {
        return;
    }

    let width = DIALOG_WIDTH.min(area.width.saturating_sub(4));
    let height = DIALOG_HEIGHT.min(area.height.saturating_sub(2));
    let [_, dialog_area, _] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(width),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(area);
    let [_, dialog, _] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(height),
        Constraint::Min(0),
    ])
    .flex(Flex::Center)
    .areas(dialog_area);

    let bg = Style::default().bg(theme.bg_dark);
    for y in dialog.y..dialog.y + dialog.height {
        for x in dialog.x..dialog.x + dialog.width {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_style(bg);
            }
        }
    }

    let border = Style::default().fg(theme.gray_dim).bg(theme.bg_dark);
    let left = dialog.x;
    let right = dialog.x + dialog.width - 1;
    let top = dialog.y;
    let bottom = dialog.y + dialog.height - 1;
    for x in left + 1..right {
        if let Some(cell) = buf.cell_mut((x, top)) {
            cell.set_char('─');
            cell.set_style(border);
        }
        if let Some(cell) = buf.cell_mut((x, bottom)) {
            cell.set_char('─');
            cell.set_style(border);
        }
    }
    for y in top + 1..bottom {
        if let Some(cell) = buf.cell_mut((left, y)) {
            cell.set_char('│');
            cell.set_style(border);
        }
        if let Some(cell) = buf.cell_mut((right, y)) {
            cell.set_char('│');
            cell.set_style(border);
        }
    }
    for (x, y, ch) in [
        (left, top, '╭'),
        (right, top, '╮'),
        (left, bottom, '╰'),
        (right, bottom, '╯'),
    ] {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_style(border);
        }
    }

    let inner_x = dialog.x + INNER_PAD;
    let inner_width = dialog.width.saturating_sub(INNER_PAD * 2);
    let mut row = dialog.y + 1;

    let title = Line::from(Span::styled(
        "Connect a third-party model",
        Style::default()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    ));
    title.render(Rect::new(inner_x, row, inner_width, 1), buf);
    row += 1;
    let hint = Line::from(Span::styled(
        "Writes a provider the kernel reads on next restart.",
        Style::default().fg(theme.gray),
    ));
    hint.render(Rect::new(inner_x, row, inner_width, 1), buf);
    row += 2;

    let labels = [
        (Field::Id, "Provider id", false),
        (Field::Name, "Display name", false),
        (Field::BaseUrl, "Base URL", false),
        (Field::ApiKey, "API key", true),
        (Field::ModelId, "Model id", false),
        (Field::ModelName, "Model name", false),
        (Field::Context, "Context tokens", false),
        (Field::Output, "Max output", false),
    ];
    for (field, label, masked) in labels {
        if row >= bottom {
            return;
        }
        render_field_row(
            buf,
            Rect::new(inner_x, row, inner_width, 1),
            label,
            state.field(field),
            state.focused_field() == field,
            masked,
            &theme,
        );
        row += 1;
    }

    // Package toggle
    if row < bottom {
        let focused = state.focused_field() == Field::Package;
        let label = Span::styled(
            "API dialect    ",
            Style::default().fg(if focused {
                theme.accent_user
            } else {
                theme.gray_bright
            }),
        );
        let options = Line::from(vec![
            label,
            Span::styled(
                if state.use_anthropic_package {
                    "  Anthropic /v1/messages "
                } else {
                    "[ OpenAI /chat/completions ]"
                },
                Style::default()
                    .fg(if state.use_anthropic_package {
                        theme.gray
                    } else {
                        theme.accent_user
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if state.use_anthropic_package {
                    "  [ Anthropic /v1/messages ]"
                } else {
                    "    Anthropic /v1/messages "
                },
                Style::default().fg(if state.use_anthropic_package {
                    theme.accent_user
                } else {
                    theme.gray
                }),
            ),
        ]);
        options.render(Rect::new(inner_x, row, inner_width, 1), buf);
        row += 1;
    }

    // Reasoning-effort row
    if row < bottom {
        let focused = state.focused_field() == Field::ReasoningEffort;
        let current = state
            .reasoning_effort()
            .unwrap_or("provider default (unset)");
        let line = Line::from(vec![
            Span::styled(
                format!("{:<LABEL_WIDTH$}", "Thinking effort"),
                Style::default().fg(if focused {
                    theme.accent_user
                } else {
                    theme.gray_bright
                }),
            ),
            Span::styled(
                format!("◄ {current} ►"),
                Style::default()
                    .fg(if focused {
                        theme.accent_user
                    } else {
                        theme.text_primary
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        line.render(Rect::new(inner_x, row, inner_width, 1), buf);
        row += 1;
    }

    // Default-model toggle
    if row < bottom {
        let focused = state.focused_field() == Field::DefaultModel;
        let mark = if state.make_default { "[x]" } else { "[ ]" };
        let line = Line::from(vec![
            Span::styled(
                "Set as default  ",
                Style::default().fg(if focused {
                    theme.accent_user
                } else {
                    theme.gray_bright
                }),
            ),
            Span::styled(
                format!("{mark} model"),
                Style::default().fg(if state.make_default {
                    theme.accent_success
                } else {
                    theme.gray
                }),
            ),
        ]);
        line.render(Rect::new(inner_x, row, inner_width, 1), buf);
        row += 2;
    }

    // Error or save hint
    if row < bottom {
        let line = match &state.error {
            Some(error) => Line::from(Span::styled(
                format!("! {error}"),
                Style::default().fg(theme.accent_error),
            )),
            None => Line::from(vec![
                Span::styled(
                    "enter",
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" save   ", Style::default().fg(theme.gray)),
                Span::styled(
                    "tab",
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" next field   ", Style::default().fg(theme.gray)),
                Span::styled(
                    "esc",
                    Style::default()
                        .fg(theme.accent_user)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" cancel", Style::default().fg(theme.gray)),
            ]),
        };
        line.render(Rect::new(inner_x, row, inner_width, 1), buf);
    }
}

const LABEL_WIDTH: usize = 16;

fn render_field_row(
    buf: &mut Buffer,
    area: Rect,
    label: &str,
    editor: &LineEditor,
    focused: bool,
    masked: bool,
    theme: &Theme,
) {
    let label_style = Style::default().fg(if focused {
        theme.accent_user
    } else {
        theme.gray_bright
    });
    let label_text = format!("{label:<LABEL_WIDTH$}");
    Line::from(Span::styled(label_text, label_style)).render(area, buf);

    let value_x = area.x + LABEL_WIDTH as u16;
    let value_width = area.width.saturating_sub(LABEL_WIDTH as u16);
    if value_width == 0 {
        return;
    }
    let value_area = Rect::new(value_x, area.y, value_width, 1);

    // The API key is masked, and its cursor is pinned to the end: showing the
    // real cursor position inside bullets leaks the key's length variation and
    // buys nothing.
    if masked {
        let count = editor.text().chars().count();
        let shown: String = "•".repeat(count.min(value_width as usize));
        Line::from(Span::styled(
            shown,
            Style::default().fg(if focused {
                theme.text_primary
            } else {
                theme.gray
            }),
        ))
        .render(value_area, buf);
        if focused {
            let cursor_x = value_x + (count.min(value_width as usize - 1) as u16);
            if let Some(cell) = buf.cell_mut((cursor_x, area.y)) {
                cell.set_style(theme.block_cursor_over(theme.bg_dark));
            }
        }
        return;
    }

    let viewport = editor.viewport(value_width as usize);
    let visible = editor.text().get(viewport.visible_byte_range).unwrap_or("");
    Line::from(Span::styled(
        visible,
        Style::default().fg(if focused {
            theme.text_primary
        } else {
            theme.gray
        }),
    ))
    .render(value_area, buf);
    if focused {
        let cursor_x = value_x + viewport.cursor_display_column as u16;
        if let Some(cell) = buf.cell_mut((cursor_x, area.y)) {
            cell.set_style(theme.block_cursor_over(theme.bg_dark));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn type_str(state: &mut ProviderSetupState, text: &str) {
        for c in text.chars() {
            let outcome = state.handle_key(&key(KeyCode::Char(c)));
            assert!(matches!(outcome, ProviderSetupOutcome::Changed));
        }
    }

    fn filled() -> ProviderSetupState {
        let mut state = ProviderSetupState::new();
        type_str(&mut state, "gateway");
        state.handle_key(&key(KeyCode::Tab));
        type_str(&mut state, "My Gateway");
        state.handle_key(&key(KeyCode::Tab));
        type_str(&mut state, "https://gateway.example.com/v1");
        state.handle_key(&key(KeyCode::Tab));
        type_str(&mut state, "sk-secret");
        state.handle_key(&key(KeyCode::Tab));
        type_str(&mut state, "gpt-oss-120b");
        state
    }

    #[test]
    fn tab_cycles_through_every_row_and_wraps() {
        let mut state = ProviderSetupState::new();
        for expected in 1..FIELDS.len() {
            state.handle_key(&key(KeyCode::Tab));
            assert_eq!(state.focus, expected);
        }
        state.handle_key(&key(KeyCode::Tab));
        assert_eq!(state.focus, 0, "focus must wrap back to the first row");
    }

    #[test]
    fn back_tab_wraps_backwards() {
        let mut state = ProviderSetupState::new();
        state.handle_key(&key(KeyCode::BackTab));
        assert_eq!(state.focus, FIELDS.len() - 1);
    }

    #[test]
    fn enter_with_empty_form_reports_the_first_error_and_stays_open() {
        let mut state = ProviderSetupState::new();
        let outcome = state.handle_key(&key(KeyCode::Enter));
        assert!(matches!(outcome, ProviderSetupOutcome::Changed));
        assert!(state.error.as_deref().unwrap().contains("Provider id"));
    }

    #[test]
    fn enter_after_filling_submits_a_normalized_draft() {
        let mut state = filled();
        let outcome = state.handle_key(&key(KeyCode::Enter));
        let ProviderSetupOutcome::Submitted(submission) = outcome else {
            panic!("expected submission, got {outcome:?}");
        };
        assert_eq!(submission.draft.id, "gateway");
        assert_eq!(submission.draft.model_ref(), "gateway/gpt-oss-120b");
        assert_eq!(submission.draft.api_key, "sk-secret");
        assert!(submission.make_default, "default starts enabled");
        assert_eq!(
            submission.draft.package,
            openbuild_backend::providers::OPENAI_COMPATIBLE_PACKAGE
        );
    }

    #[test]
    fn space_on_the_default_row_toggles_it() {
        let mut state = filled();
        while state.focused_field() != Field::DefaultModel {
            state.handle_key(&key(KeyCode::Tab));
        }
        assert!(state.make_default);
        state.handle_key(&key(KeyCode::Char(' ')));
        assert!(!state.make_default);
    }

    #[test]
    fn left_right_on_the_package_row_switch_dialect() {
        let mut state = ProviderSetupState::new();
        while state.focused_field() != Field::Package {
            state.handle_key(&key(KeyCode::Tab));
        }
        assert!(!state.use_anthropic_package);
        state.handle_key(&key(KeyCode::Right));
        assert!(state.use_anthropic_package);
        state.handle_key(&key(KeyCode::Left));
        assert!(!state.use_anthropic_package);
    }

    #[test]
    fn esc_cancels_without_writing() {
        let mut state = filled();
        assert_eq!(
            state.handle_key(&key(KeyCode::Esc)),
            ProviderSetupOutcome::Cancelled
        );
    }

    #[test]
    fn typing_clears_a_previous_error() {
        let mut state = ProviderSetupState::new();
        state.handle_key(&key(KeyCode::Enter));
        assert!(state.error.is_some());
        type_str(&mut state, "a");
        assert!(state.error.is_none());
    }

    #[test]
    fn prefilled_round_trips_an_existing_provider() {
        let draft = ProviderDraft {
            id: "gateway".to_string(),
            name: "GW".to_string(),
            base_url: "https://gw.example.com/v1".to_string(),
            api_key: "sk".to_string(),
            package: ANTHROPIC_COMPATIBLE_PACKAGE.to_string(),
            model_id: "claude-x".to_string(),
            model_name: "Claude X".to_string(),
            context: 200_000,
            output: 8_192,
            reasoning_effort: Some("high".to_string()),
        };
        let state = ProviderSetupState::prefilled(&draft, false);
        assert_eq!(state.draft(), draft);
        assert!(state.use_anthropic_package);
        assert!(!state.make_default);
    }

    #[test]
    fn the_effort_row_cycles_and_starts_unset() {
        let mut state = filled();
        assert!(state.draft().reasoning_effort.is_none(), "unset by default");

        // Focus lands on the effort row by walking to it explicitly.
        while state.focused_field() != Field::ReasoningEffort {
            state.handle_key(&key(KeyCode::Tab));
        }
        state.handle_key(&key(KeyCode::Right));
        assert_eq!(state.draft().reasoning_effort.as_deref(), Some("none"));
        state.handle_key(&key(KeyCode::Right));
        assert_eq!(state.draft().reasoning_effort.as_deref(), Some("minimal"));
        state.handle_key(&key(KeyCode::Left));
        assert_eq!(state.draft().reasoning_effort.as_deref(), Some("none"));
        state.handle_key(&key(KeyCode::Left));
        assert!(state.draft().reasoning_effort.is_none(), "wraps back to unset");
    }

    #[test]
    fn renders_labels_and_masks_the_key() {
        let _theme = crate::theme::cache::pin_theme();
        let area = Rect::new(0, 0, 100, 26);
        let mut buf = Buffer::empty(area);
        let state = filled();
        render_provider_setup(area, &mut buf, &state);
        let mut text = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                text.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            text.push('\n');
        }
        assert!(text.contains("Connect a third-party model"), "{text}");
        assert!(text.contains("Base URL"), "{text}");
        assert!(text.contains("gateway"), "{text}");
        assert!(
            !text.contains("sk-secret"),
            "the API key must never be rendered:\n{text}"
        );
        assert!(
            text.contains('•'),
            "masked key placeholder missing:\n{text}"
        );
    }
}
