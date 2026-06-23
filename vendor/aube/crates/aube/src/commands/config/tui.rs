use super::{
    ListLocation, Location, NpmrcEdit, literal_aliases, read_merged, read_single, resolve_aliases,
    set_cmd, setting_search_score, settings_meta, user_npmrc_path,
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use miette::{IntoDiagnostic, miette};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use yaml_serde::{Number, Value};

enum TuiMode {
    Browse,
    Search,
    Edit,
}

struct ConfigTui {
    settings: Vec<&'static settings_meta::SettingMeta>,
    filtered: Vec<usize>,
    selected: usize,
    query: String,
    edit: Option<EditState>,
    mode: TuiMode,
    status: Option<StatusMessage>,
}

struct EditState {
    target: EditTarget,
    value: String,
    original: String,
    input: EditInput,
}

enum EditInput {
    Text,
    Choice {
        choices: Vec<String>,
        selected: usize,
    },
}

#[derive(Clone)]
enum EditTarget {
    Npmrc { path: PathBuf, key: String },
    WorkspaceYaml { path: PathBuf, key: String },
}

struct StatusMessage {
    kind: StatusKind,
    text: String,
}

#[derive(Clone, Copy)]
enum StatusKind {
    Info,
    Error,
}

impl ConfigTui {
    fn new() -> Self {
        let settings = settings_meta::all().iter().collect::<Vec<_>>();
        let filtered = (0..settings.len()).collect::<Vec<_>>();
        Self {
            settings,
            filtered,
            selected: 0,
            query: String::new(),
            edit: None,
            mode: TuiMode::Browse,
            status: None,
        }
    }

    fn selected(&self) -> Option<&'static settings_meta::SettingMeta> {
        self.filtered
            .get(self.selected)
            .and_then(|idx| self.settings.get(*idx))
            .copied()
    }

    fn apply_filter(&mut self) {
        let terms = self
            .query
            .split_whitespace()
            .map(|q| q.to_ascii_lowercase())
            .collect::<Vec<_>>();
        self.filtered = if terms.is_empty() {
            (0..self.settings.len()).collect()
        } else {
            self.settings
                .iter()
                .enumerate()
                .filter_map(|(idx, meta)| (setting_search_score(meta, &terms) > 0).then_some(idx))
                .collect()
        };
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.filtered.len() - 1);
    }

    fn begin_edit(&mut self) {
        let Some(meta) = self.selected() else {
            self.status = None;
            return;
        };
        let Some(target) = edit_target(meta) else {
            self.set_error(edit_unavailable_message(meta));
            return;
        };
        let original = match target.value() {
            Ok(value) => value.unwrap_or_default(),
            Err(err) => {
                self.set_error(err.to_string());
                return;
            }
        };
        self.status = None;
        self.edit = Some(EditState::new(meta, target, original));
        self.mode = TuiMode::Edit;
    }

    fn save_edit(&mut self) {
        let Some(meta) = self.selected() else {
            self.mode = TuiMode::Browse;
            return;
        };
        let Some(edit) = &self.edit else {
            self.mode = TuiMode::Browse;
            return;
        };
        if let Err(err) = validate_edit_value(meta, &edit.value) {
            self.set_error(err.to_string());
            return;
        }

        let edit = self.edit.take().expect("edit state checked above");
        match edit.target.write(meta, &edit.value) {
            Ok(()) => self.set_info(format!("saved {}", edit.target.label())),
            Err(err) => self.set_error(err.to_string()),
        }
        self.mode = TuiMode::Browse;
    }

    fn clear_selected(&mut self) {
        let Some(meta) = self.selected() else {
            self.mode = TuiMode::Browse;
            return;
        };
        let Some(target) = self
            .edit
            .as_ref()
            .map(|edit| edit.target.clone())
            .or_else(|| edit_target(meta))
        else {
            self.set_error(edit_unavailable_message(meta));
            return;
        };

        match target.clear() {
            Ok(true) => self.set_info(format!("cleared {}", target.label())),
            Ok(false) => self.set_info(format!("{} is already unset", target.label())),
            Err(err) => self.set_error(err.to_string()),
        }
        self.edit = None;
        self.mode = TuiMode::Browse;
    }

    fn set_info(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            kind: StatusKind::Info,
            text: text.into(),
        });
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            kind: StatusKind::Error,
            text: text.into(),
        });
    }
}

pub fn run() -> miette::Result<()> {
    if !io::stdout().is_terminal() {
        return Err(miette!(
            "`{}` requires an interactive terminal",
            aube_util::cmd("config tui")
        ));
    }

    enable_raw_mode().into_diagnostic()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).into_diagnostic()?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).into_diagnostic()?;

    let result = run_tui(&mut terminal);
    let restore = disable_raw_mode()
        .into_diagnostic()
        .and_then(|_| execute!(terminal.backend_mut(), LeaveAlternateScreen).into_diagnostic())
        .and_then(|_| terminal.show_cursor().into_diagnostic());

    result.and(restore)
}

fn run_tui(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> miette::Result<()> {
    let mut app = ConfigTui::new();
    loop {
        terminal
            .draw(|frame| draw_tui(frame, &mut app))
            .into_diagnostic()?;

        let Event::Key(key) = event::read().into_diagnostic()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        if matches!(app.mode, TuiMode::Search) {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => app.mode = TuiMode::Browse,
                KeyCode::Backspace => {
                    app.query.pop();
                    app.apply_filter();
                }
                KeyCode::Char(c) => {
                    app.query.push(c);
                    app.apply_filter();
                }
                _ => {}
            }
            continue;
        }

        if matches!(app.mode, TuiMode::Edit) {
            match key.code {
                KeyCode::Esc => {
                    app.set_info("edit canceled");
                    app.edit = None;
                    app.mode = TuiMode::Browse;
                }
                KeyCode::Enter => app.save_edit(),
                KeyCode::Char('d') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    app.clear_selected();
                }
                KeyCode::Left => {
                    if let Some(edit) = &mut app.edit {
                        edit.cycle_choice(-1);
                    }
                }
                KeyCode::Right | KeyCode::Tab => {
                    if let Some(edit) = &mut app.edit {
                        edit.cycle_choice(1);
                    }
                }
                KeyCode::Backspace => {
                    if let Some(edit) = &mut app.edit {
                        edit.backspace();
                    }
                }
                KeyCode::Char('u') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                    if let Some(edit) = &mut app.edit {
                        edit.clear();
                    }
                }
                KeyCode::Char(c) => {
                    if let Some(edit) = &mut app.edit {
                        if edit.is_choice() {
                            match c {
                                ' ' | 'l' => edit.cycle_choice(1),
                                'h' => edit.cycle_choice(-1),
                                'f' => {
                                    edit.set_choice("false");
                                }
                                't' => {
                                    edit.set_choice("true");
                                }
                                _ => {}
                            }
                        } else {
                            edit.push(c);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('/') => app.mode = TuiMode::Search,
            KeyCode::Char('e') | KeyCode::Enter => app.begin_edit(),
            KeyCode::Char('c') => app.clear_selected(),
            KeyCode::Char('x') if app.status.is_some() => app.status = None,
            KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
            KeyCode::PageDown => app.move_selection(10),
            KeyCode::PageUp => app.move_selection(-10),
            KeyCode::Home => app.selected = 0,
            KeyCode::End => app.selected = app.filtered.len().saturating_sub(1),
            _ => {}
        }
    }
}

fn draw_tui(frame: &mut ratatui::Frame<'_>, app: &mut ConfigTui) {
    let has_status = app.status.is_some();
    let constraints = if has_status {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(5),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ]
    };
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.area());
    let main_area = vertical[1];
    let status_area = has_status.then(|| vertical[2]);
    let help_area = vertical[vertical.len() - 1];

    let search_title = if matches!(app.mode, TuiMode::Search) {
        "Search (Enter/Esc to finish)"
    } else {
        "Search (/ to edit)"
    };
    let search = Paragraph::new(app.query.as_str())
        .block(Block::default().borders(Borders::ALL).title(search_title));
    frame.render_widget(search, vertical[0]);
    if matches!(app.mode, TuiMode::Search) {
        let cursor_x = vertical[0].x + 1 + text_cursor_offset(&app.query, vertical[0].width, 2);
        frame.set_cursor_position((cursor_x, vertical[0].y + 1));
    }

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(42), Constraint::Percentage(58)])
        .split(main_area);

    let items = app
        .filtered
        .iter()
        .map(|idx| {
            let meta = app.settings[*idx];
            ListItem::new(Line::from(vec![
                Span::styled(
                    meta.name,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {}", meta.description)),
            ]))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !app.filtered.is_empty() {
        state.select(Some(app.selected));
    }
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Settings"))
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, horizontal[0], &mut state);

    let details = app
        .selected()
        .map(setting_detail_lines)
        .unwrap_or_else(|| vec![Line::from("No settings match the search.")]);
    let details = Paragraph::new(details)
        .block(Block::default().borders(Borders::ALL).title("Details"))
        .wrap(Wrap { trim: false });
    frame.render_widget(details, horizontal[1]);

    if matches!(app.mode, TuiMode::Edit) {
        draw_edit_modal(frame, app, main_area);
    }

    if let (Some(area), Some(status)) = (status_area, app.status.as_ref()) {
        draw_status_panel(frame, area, status);
    }

    let help_text = match app.mode {
        TuiMode::Browse if app.status.is_some() => {
            "q quit  / search  e edit  c clear  x dismiss message  arrows/jk move"
        }
        TuiMode::Browse => "q quit  / search  e edit  c clear  arrows/jk move",
        TuiMode::Search => "type search  Enter/Esc done  Backspace delete",
        TuiMode::Edit => app
            .edit
            .as_ref()
            .map(EditState::help)
            .unwrap_or("Enter save  Esc cancel"),
    };
    let help = Paragraph::new(help_text);
    frame.render_widget(help, help_area);
}

fn draw_status_panel(frame: &mut ratatui::Frame<'_>, area: Rect, status: &StatusMessage) {
    let (title, color) = match status.kind {
        StatusKind::Info => ("Message", Color::Green),
        StatusKind::Error => ("Error", Color::Red),
    };
    let panel = Paragraph::new(status.text.as_str())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(color)),
        )
        .style(Style::default().fg(color))
        .wrap(Wrap { trim: false });
    frame.render_widget(panel, area);
}

fn setting_detail_lines(meta: &settings_meta::SettingMeta) -> Vec<Line<'static>> {
    let target = edit_target(meta);
    let target_value = target
        .as_ref()
        .and_then(|target| target.value().ok().flatten())
        .unwrap_or_else(|| "undefined".to_string());
    let npmrc_key = literal_aliases(meta.npmrc_keys).into_iter().next();
    let npmrc_effective = npmrc_key
        .as_deref()
        .and_then(|key| config_value(key, ListLocation::Merged).ok().flatten())
        .unwrap_or_else(|| "undefined".to_string());

    let mut lines = vec![
        Line::from(Span::styled(
            meta.name.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("Type: {}", meta.type_)),
        Line::from(format!("Default: {}", meta.default)),
        Line::from(format!("Effective .npmrc value: {npmrc_effective}")),
        Line::from(format!(
            "Editing file: {}",
            target_file_label(target.as_ref())
        )),
        Line::from(format!(
            "Editing key: {}",
            target_key_label(target.as_ref())
        )),
        Line::from(format!("Target value: {target_value}")),
        Line::from(format!("Description: {}", meta.description)),
    ];

    detail_source_line(&mut lines, "CLI flags", meta.cli_flags);
    detail_source_line(&mut lines, "Environment", meta.env_vars);
    detail_source_line(&mut lines, ".npmrc keys", meta.npmrc_keys);
    detail_source_line(&mut lines, "Workspace YAML keys", meta.workspace_yaml_keys);

    let docs = meta.docs.trim();
    if !docs.is_empty() {
        lines.push(Line::from(""));
        lines.extend(docs.lines().map(|line| Line::from(line.to_string())));
    }

    if !meta.examples.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from("Examples:"));
        lines.extend(
            meta.examples
                .iter()
                .map(|example| Line::from(format!("  {example}"))),
        );
    }

    lines
}

fn detail_source_line(lines: &mut Vec<Line<'static>>, label: &str, values: &[&str]) {
    if !values.is_empty() {
        lines.push(Line::from(format!("{label}: {}", values.join(", "))));
    }
}

fn draw_edit_modal(frame: &mut ratatui::Frame<'_>, app: &ConfigTui, parent: Rect) {
    let area = centered_rect(76, 13, parent);
    frame.render_widget(Clear, area);

    let Some(edit) = &app.edit else {
        return;
    };
    let title = app
        .selected()
        .map(|meta| format!("Edit {}", meta.name))
        .unwrap_or_else(|| "Edit setting".to_string());
    let value_line = match &edit.input {
        EditInput::Text => Line::from(format!(" {} ", edit.value)),
        EditInput::Choice { choices, selected } => {
            let mut spans = vec![Span::raw(" ")];
            for (idx, choice) in choices.iter().enumerate() {
                if idx > 0 {
                    spans.push(Span::raw("  "));
                }
                let style = if idx == *selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                spans.push(Span::styled(format!(" {choice} "), style));
            }
            Line::from(spans)
        }
    };
    let lines = vec![
        Line::from(vec![
            Span::styled("File: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(edit.target.file_label()),
        ]),
        Line::from(vec![
            Span::styled("Key: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(edit.target.key()),
        ]),
        Line::from(vec![
            Span::styled("Previous: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(if edit.original.is_empty() {
                "undefined".to_string()
            } else {
                edit.original.clone()
            }),
        ]),
        Line::from(vec![
            Span::styled("Expected: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(
                app.selected()
                    .map(expected_value_hint)
                    .unwrap_or_else(|| "unknown".to_string()),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "New value",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        value_line,
        Line::from(""),
        Line::from(edit.help()),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(Color::Cyan));
    let modal = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(modal, area);
    if matches!(edit.input, EditInput::Text) {
        let cursor_x = area.x + 2 + text_cursor_offset(&edit.value, area.width, 4);
        frame.set_cursor_position((cursor_x, area.y + 7));
    }
}

impl EditState {
    fn new(meta: &settings_meta::SettingMeta, target: EditTarget, original: String) -> Self {
        let input = edit_input(meta, &original);
        let value = match &input {
            EditInput::Text => original.clone(),
            EditInput::Choice { choices, selected } => choices[*selected].clone(),
        };
        Self {
            target,
            value,
            original,
            input,
        }
    }

    fn push(&mut self, c: char) {
        if matches!(self.input, EditInput::Text) {
            self.value.push(c);
        }
    }

    fn backspace(&mut self) {
        if matches!(self.input, EditInput::Text) {
            self.value.pop();
        }
    }

    fn clear(&mut self) {
        if matches!(self.input, EditInput::Text) {
            self.value.clear();
        }
    }

    fn cycle_choice(&mut self, delta: isize) {
        let EditInput::Choice { choices, selected } = &mut self.input else {
            return;
        };
        if choices.is_empty() {
            return;
        }
        *selected = (*selected as isize + delta).rem_euclid(choices.len() as isize) as usize;
        self.value = choices[*selected].clone();
    }

    fn set_choice(&mut self, value: &str) -> bool {
        let EditInput::Choice { choices, selected } = &mut self.input else {
            return false;
        };
        let Some(idx) = choices.iter().position(|choice| choice == value) else {
            return false;
        };
        *selected = idx;
        self.value = value.to_string();
        true
    }

    fn help(&self) -> &'static str {
        match self.input {
            EditInput::Text => {
                "type value  Enter save  Ctrl-D clear setting  Ctrl-U clear input  Esc cancel"
            }
            EditInput::Choice { .. } => {
                "Left/Right choose  Space toggle  Enter save  Ctrl-D clear setting  Esc cancel"
            }
        }
    }

    fn is_choice(&self) -> bool {
        matches!(self.input, EditInput::Choice { .. })
    }
}

fn edit_input(meta: &settings_meta::SettingMeta, original: &str) -> EditInput {
    let choices = match meta.type_ {
        "bool" => Some(vec!["false".to_string(), "true".to_string()]),
        t if t.starts_with('"') => {
            let variants = enum_variants(t);
            (!variants.is_empty()).then_some(variants)
        }
        _ => None,
    };
    let Some(choices) = choices else {
        return EditInput::Text;
    };
    let selected = choices
        .iter()
        .position(|choice| choice == original)
        .or_else(|| choices.iter().position(|choice| choice == meta.default))
        .unwrap_or(0);
    EditInput::Choice { choices, selected }
}

fn text_cursor_offset(value: &str, area_width: u16, reserved_columns: u16) -> u16 {
    let max = area_width.saturating_sub(reserved_columns);
    (value.chars().count() as u16).min(max)
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn edit_target(meta: &settings_meta::SettingMeta) -> Option<EditTarget> {
    if !is_tui_editable_type(meta.type_) {
        return None;
    }
    let cwd = crate::dirs::project_root_or_cwd().ok()?;
    let workspace_key = meta.workspace_yaml_keys.first().map(|key| key.to_string());
    if let Some(key) = workspace_key.clone()
        && let Some(path) = aube_manifest::workspace::workspace_yaml_existing(&cwd)
    {
        return Some(EditTarget::WorkspaceYaml { path, key });
    }

    let npmrc_key = literal_aliases(meta.npmrc_keys).into_iter().next();
    if let Some(key) = npmrc_key.clone() {
        let path = cwd.join(".npmrc");
        if path.exists() || workspace_key.is_none() {
            return Some(EditTarget::Npmrc { path, key });
        }
    }

    // No existing workspace yaml: fall back to the canonical write target
    // (this tool's branded YAML name, e.g. `aube-workspace.yaml`) rather than
    // a hardcoded filename, so an embedder's branded name is honored.
    workspace_key.map(|key| EditTarget::WorkspaceYaml {
        path: aube_manifest::workspace::workspace_yaml_target(&cwd),
        key,
    })
}

fn edit_unavailable_message(meta: &settings_meta::SettingMeta) -> String {
    if !is_tui_editable_type(meta.type_) {
        return format!(
            "`{}` uses `{}` values, which this TUI editor cannot write yet. Use `{} {}` for the accepted shape, then edit the config file manually.",
            meta.name,
            meta.type_,
            aube_util::cmd("config explain"),
            meta.name
        );
    }

    let mut actions = Vec::new();
    if let Some(flag) = meta.cli_flags.first() {
        actions.push(format!("pass `{flag}` on the command line"));
    }
    if let Some(env) = meta.env_vars.first() {
        actions.push(format!("set `{env}` in the environment"));
    }
    if !meta.npmrc_keys.is_empty() || !meta.workspace_yaml_keys.is_empty() {
        actions.push(format!(
            "run `{} {}` to see the supported config-file keys",
            aube_util::cmd("config explain"),
            meta.name
        ));
    }

    if actions.is_empty() {
        format!(
            "`{}` has no writable .npmrc or workspace YAML key. Run `{} {}` to see where it can be set.",
            meta.name,
            aube_util::cmd("config explain"),
            meta.name
        )
    } else {
        format!(
            "`{}` has no writable .npmrc or workspace YAML key here; {}.",
            meta.name,
            actions.join(", or ")
        )
    }
}

fn target_file_label(target: Option<&EditTarget>) -> String {
    target
        .map(EditTarget::file_label)
        .unwrap_or_else(|| "read-only".to_string())
}

fn target_key_label(target: Option<&EditTarget>) -> String {
    target
        .map(|target| target.key().to_string())
        .unwrap_or_else(|| "read-only".to_string())
}

fn config_value(key: &str, location: ListLocation) -> miette::Result<Option<String>> {
    let aliases = resolve_aliases(key);
    let cwd = crate::dirs::project_root_or_cwd()?;
    let entries = match location {
        ListLocation::Merged => read_merged(&cwd)?,
        ListLocation::User | ListLocation::Global => read_single(&user_npmrc_path()?)?,
        ListLocation::Project => read_single(&cwd.join(".npmrc"))?,
    };

    Ok(entries
        .iter()
        .rev()
        .find(|(k, _)| aliases.iter().any(|a| a == k))
        .map(|(_, v)| v.clone()))
}

impl EditTarget {
    fn label(&self) -> String {
        match self {
            EditTarget::Npmrc { path, key } | EditTarget::WorkspaceYaml { path, key } => {
                format!("{} ({key})", path.display())
            }
        }
    }

    fn file_label(&self) -> String {
        match self {
            EditTarget::Npmrc { path, .. } | EditTarget::WorkspaceYaml { path, .. } => {
                path.display().to_string()
            }
        }
    }

    fn key(&self) -> &str {
        match self {
            EditTarget::Npmrc { key, .. } | EditTarget::WorkspaceYaml { key, .. } => key,
        }
    }

    fn value(&self) -> miette::Result<Option<String>> {
        match self {
            EditTarget::Npmrc { key, .. } => config_value(key, ListLocation::Project),
            EditTarget::WorkspaceYaml { path, key } => workspace_value(path, key),
        }
    }

    fn write(&self, meta: &settings_meta::SettingMeta, value: &str) -> miette::Result<()> {
        validate_edit_value(meta, value)?;
        match self {
            EditTarget::Npmrc { key, path } => {
                let location = if path.file_name().and_then(|name| name.to_str()) == Some(".npmrc")
                {
                    Location::Project
                } else {
                    Location::User
                };
                set_cmd::set_value(key, value, location, false)
            }
            EditTarget::WorkspaceYaml { path, key } => {
                write_workspace_value(path, key, meta, value)
            }
        }
    }

    fn clear(&self) -> miette::Result<bool> {
        match self {
            EditTarget::Npmrc { path, key } => clear_npmrc_value(path, key),
            EditTarget::WorkspaceYaml { path, key } => clear_workspace_value(path, key),
        }
    }
}

fn clear_npmrc_value(path: &std::path::Path, key: &str) -> miette::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let aliases = resolve_aliases(key);
    let mut edit = NpmrcEdit::load(path)?;
    let mut removed = false;
    for alias in &aliases {
        removed |= edit.remove(alias);
    }
    if removed {
        edit.save(path)?;
    }
    Ok(removed)
}

fn workspace_value(path: &std::path::Path, key: &str) -> miette::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| miette!("failed to read {}: {e}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(None);
    }
    let doc: Value = yaml_serde::from_str(&content)
        .map_err(|e| miette!("failed to parse {}: {e}", path.display()))?;
    let Some(map) = doc.as_mapping() else {
        return Ok(None);
    };
    Ok(map
        .get(Value::String(key.to_string()))
        .map(workspace_value_to_string))
}

fn clear_workspace_value(path: &std::path::Path, key: &str) -> miette::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    // Pre-flight: read the file once so a non-mapping or empty
    // top-level shape (which `edit_workspace_yaml` would reject as
    // a hard error) stays a graceful `Ok(false)` — the original
    // behavior before the comment-preserving migration. Parse
    // errors *do* propagate, since silently treating a malformed
    // workspace yaml as "key not found" would mask file corruption
    // the user needs to know about. The double read against the
    // file is fine: this only fires on an interactive TUI clear,
    // not the install hot path.
    let content = std::fs::read_to_string(path)
        .map_err(|e| miette!("failed to read {}: {e}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(false);
    }
    let parsed: Value = yaml_serde::from_str(&content)
        .map_err(|e| miette!("failed to parse {}: {e}", path.display()))?;
    if parsed.as_mapping().is_none() {
        return Ok(false);
    }
    // The bool return reports whether the key was actually present, so
    // we have to track removal inside the closure. `edit_workspace_yaml`
    // already short-circuits the rewrite when the closure produces no
    // structural change, so a clear of a missing key won't touch
    // comments on adjacent entries.
    let mut removed = false;
    aube_manifest::workspace::edit_workspace_yaml(path, |map| {
        removed = map.shift_remove(Value::String(key.to_string())).is_some();
        Ok(())
    })
    .map_err(miette::Report::new)
    .map_err(|e| e.wrap_err(format!("failed to write {}", path.display())))?;
    Ok(removed)
}

fn workspace_value_to_string(value: &Value) -> String {
    match value {
        Value::Null => "undefined".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Sequence(seq) => seq
            .iter()
            .map(workspace_value_to_string)
            .collect::<Vec<_>>()
            .join(","),
        other => yaml_serde::to_string(other)
            .unwrap_or_else(|_| format!("{other:?}"))
            .trim()
            .to_string(),
    }
}

fn write_workspace_value(
    path: &std::path::Path,
    key: &str,
    meta: &settings_meta::SettingMeta,
    value: &str,
) -> miette::Result<()> {
    let parsed = parse_workspace_value(meta.type_, value)?;
    aube_manifest::workspace::edit_workspace_yaml(path, |map| {
        map.insert(Value::String(key.to_string()), parsed);
        Ok(())
    })
    .map_err(miette::Report::new)
    .map_err(|e| e.wrap_err(format!("failed to write {}", path.display())))?;
    Ok(())
}

fn parse_workspace_value(type_: &str, value: &str) -> miette::Result<Value> {
    match type_ {
        "bool" => match value {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(miette!("expected `true` or `false`")),
        },
        "int" => value
            .parse::<u64>()
            .map(|n| Value::Number(Number::from(n)))
            .map_err(|_| miette!("expected an integer")),
        "list<string>" => Ok(Value::Sequence(
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| Value::String(item.to_string()))
                .collect(),
        )),
        t if t.starts_with('"') => Ok(Value::String(value.to_string())),
        "path" | "string" | "url" => Ok(Value::String(value.to_string())),
        _ => Err(miette!(
            "{} cannot be edited in this one-line editor",
            type_
        )),
    }
}

fn validate_edit_value(meta: &settings_meta::SettingMeta, value: &str) -> miette::Result<()> {
    match meta.type_ {
        "bool" => match value {
            "true" | "false" => Ok(()),
            _ => Err(miette!("expected `true` or `false`")),
        },
        "int" => value
            .parse::<u64>()
            .map(|_| ())
            .map_err(|_| miette!("expected a non-negative integer")),
        "url" => reqwest::Url::parse(value)
            .map(|_| ())
            .map_err(|_| miette!("expected an absolute URL")),
        "list<string>" | "path" | "string" => Ok(()),
        t if t.starts_with('"') => {
            let variants = enum_variants(t);
            if variants.iter().any(|variant| variant == value) {
                Ok(())
            } else {
                Err(miette!("expected one of: {}", variants.join(", ")))
            }
        }
        _ => Err(miette!(
            "{} cannot be edited in this one-line editor",
            meta.type_
        )),
    }
}

fn is_tui_editable_type(type_: &str) -> bool {
    matches!(
        type_,
        "bool" | "int" | "url" | "list<string>" | "path" | "string"
    ) || type_.starts_with('"')
}

fn expected_value_hint(meta: &settings_meta::SettingMeta) -> String {
    match meta.type_ {
        "bool" => "`true` or `false`".to_string(),
        "int" => "non-negative integer".to_string(),
        "url" => "absolute URL".to_string(),
        "list<string>" => "comma-separated strings".to_string(),
        "path" => "path".to_string(),
        "string" => "string".to_string(),
        t if t.starts_with('"') => enum_variants(t).join(", "),
        other => other.to_string(),
    }
}

fn enum_variants(type_: &str) -> Vec<String> {
    type_
        .split('|')
        .filter_map(|variant| {
            let variant = variant.trim();
            variant
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .map(str::to_string)
        })
        .collect()
}
