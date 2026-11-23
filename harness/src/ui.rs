//! Draws [`App`] with ratatui. Layout and visual language follow DESIGN.md §4.

use nixxa_contracts::{
    Action, ActionRequest, ApprovalMode, KnowledgeAction, Outcome, SandboxAction, SourceStatus, Timestamp,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::app::{Activity, ActivityState, App, Focus, Speaker};
use crate::safe;
use crate::sessions::ago;
use crate::setup::{CHOICES, LM_STUDIO_URL, OLLAMA_URL, PRIVACY_NOTICE, PRIVACY_SHORT, SetupState, Source, Step};

/// Below this width the panes collapse to one column (Activity behind `tab`).
const WIDE: u16 = 100;
/// Width of the speaker label column in the conversation.
const LABEL: usize = 6;

/// Colour with meaning, or no colour at all (`NO_COLOR`).
#[derive(Clone, Copy)]
struct Theme {
    color: bool,
}

impl Theme {
    fn fg(self, color: Color) -> Style {
        if self.color { Style::new().fg(color) } else { Style::new() }
    }
    fn ok(self) -> Style {
        self.fg(Color::Green)
    }
    fn pending(self) -> Style {
        self.fg(Color::Yellow)
    }
    fn bad(self) -> Style {
        self.fg(Color::Red)
    }
    fn dim(self) -> Style {
        if self.color { Style::new().add_modifier(Modifier::DIM) } else { Style::new() }
    }
    fn accent(self) -> Style {
        self.fg(Color::Cyan)
    }
}

/// Draws the whole screen.
pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
    let theme = Theme { color: app.color };
    let input_lines = u16::try_from(app.input.split('\n').count()).unwrap_or(u16::MAX).clamp(1, 4);
    let [status, body, input, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(input_lines + 2),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(Paragraph::new(status_line(app, theme)), status);

    let side = if body.width >= WIDE {
        let [left, right] = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(body);
        draw_conversation(frame, app, theme, left);
        right
    } else if app.focus == Focus::Activity {
        body
    } else {
        draw_conversation(frame, app, theme, body);
        Rect::default()
    };
    if side.area() > 0 {
        let [list, detail] = Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).areas(side);
        draw_activity(frame, app, theme, list);
        draw_detail(frame, app, theme, detail);
    }

    draw_input(frame, app, theme, input);
    frame.render_widget(Paragraph::new(hint_line(app, theme, hints.width)), hints);
    draw_approval(frame, app, theme, if side.area() > 0 { side } else { body });
    draw_setup(frame, app, theme, body);
    draw_sessions(frame, app, theme, body);
    draw_place(frame, app, theme, body);
    safe::scrub(frame.buffer_mut());
}

fn pane(title: &str, focused: bool, theme: Theme) -> Block<'static> {
    let block = Block::bordered().title(format!(" {title} "));
    if focused { block.border_style(theme.accent()) } else { block }
}

fn status_line(app: &App, theme: Theme) -> Line<'static> {
    let mut spans = vec![Span::styled(" nixxa", theme.accent())];
    let part = |spans: &mut Vec<Span<'static>>, text: String, style: Style| {
        spans.push(Span::raw(" ─ "));
        spans.push(Span::styled(text, style));
    };
    if let Some(session) = &app.session {
        part(&mut spans, safe::line(session.as_str()), Style::new());
    }
    if let Some(account) = &app.account {
        let model = app.brain.as_ref().map(|b| format!(" · {}", b.model)).unwrap_or_default();
        part(&mut spans, safe::line(&format!("{account}{model}")), Style::new());
    }
    match &app.sandbox {
        Some(caps) if caps.isolated => {
            part(&mut spans, format!("{} ● isolated", safe::line(&caps.backend)), theme.ok())
        }
        Some(caps) => part(&mut spans, format!("{} ○ NOT ISOLATED", safe::line(&caps.backend)), theme.bad()),
        None => {}
    }
    if !app.sources.is_empty() {
        spans.push(Span::raw(" ─"));
        for source in &app.sources {
            let (icon, style) = match source.status {
                SourceStatus::Ready => ("●", theme.ok()),
                SourceStatus::Missing => ("○", theme.dim()),
                SourceStatus::Error(_) => ("✗", theme.bad()),
            };
            spans.push(Span::raw(format!(" {} ", safe::line(source.id.as_str()))));
            spans.push(Span::styled(icon, style));
        }
    }
    part(
        &mut spans,
        format!("${}.{:02}", app.cost_micro_usd / 1_000_000, app.cost_micro_usd / 10_000 % 100),
        Style::new(),
    );
    Line::from(spans)
}

fn draw_conversation(frame: &mut Frame, app: &mut App, theme: Theme, area: Rect) {
    let block = pane("Conversation", app.focus == Focus::Conversation, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = usize::from(inner.width).saturating_sub(LABEL).max(1);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let streaming = app.streaming.as_ref().map(|text| (Speaker::Agent, format!("{text}▌")));
    let entries = app
        .conversation
        .iter()
        .map(|e| (e.speaker, e.text.clone(), false))
        .chain(streaming.map(|(s, t)| (s, t, false)));
    let queued = app.queued.iter().map(|text| (Speaker::User, format!("{text} (queued)"), true));
    for (speaker, text, waiting) in entries.chain(queued) {
        let (label, style) = match speaker {
            Speaker::User if waiting => ("you", theme.dim()),
            Speaker::User => ("you", Style::new()),
            Speaker::Agent => ("nixxa", theme.accent()),
            Speaker::System => ("note", theme.dim()),
            Speaker::Error => ("error", theme.bad()),
        };
        for (i, row) in wrap(&safe::lines(&text).join("\n"), width).into_iter().enumerate() {
            let label = if i == 0 { label } else { "" };
            lines.push(Line::from(vec![
                Span::styled(format!("{label:<LABEL$}"), style.add_modifier(Modifier::BOLD)),
                Span::styled(row, if speaker == Speaker::User && !waiting { Style::new() } else { style }),
            ]));
        }
        lines.push(Line::default());
    }

    let height = usize::from(inner.height);
    let max_back = lines.len().saturating_sub(height);
    app.scroll_back = app.scroll_back.min(max_back);
    let top = max_back - app.scroll_back;
    let visible: Vec<Line> = lines.into_iter().skip(top).take(height).collect();
    frame.render_widget(Paragraph::new(visible), inner);
}

fn draw_activity(frame: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let block = pane("Activity", app.focus == Focus::Activity, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let height = usize::from(inner.height);
    let selected = app.selected();
    let first = selected.map_or(0, |s| (s + 1).saturating_sub(height));
    let lines: Vec<Line> = app
        .activity
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(i, activity)| {
            let line = activity_line(activity, theme, usize::from(inner.width));
            if Some(i) == selected { line.patch_style(Style::new().add_modifier(Modifier::REVERSED)) } else { line }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn activity_line(activity: &Activity, theme: Theme, width: usize) -> Line<'static> {
    let (icon, style) = state_icon(activity.state, theme);
    let timing = match activity.state {
        ActivityState::Proposed => "proposed".to_string(),
        ActivityState::Asking => "needs you".to_string(),
        ActivityState::Running => "running…".to_string(),
        ActivityState::Done => {
            activity.observation.as_ref().map_or_else(String::new, |o| format!("{} ms", o.duration_ms))
        }
        ActivityState::Denied => "denied".to_string(),
        ActivityState::Failed => "failed".to_string(),
    };
    let timing = if activity.auto_approved { format!("auto · {timing}") } else { timing };
    let (kind, target) = describe(&activity.request);
    let room = width.saturating_sub(2 + 7 + 1 + timing.chars().count());
    Line::from(vec![
        Span::styled(format!("{icon} "), style),
        Span::raw(format!("{kind:<6} ")),
        Span::raw(format!("{:<room$} ", truncate(&target, room))),
        Span::styled(timing, style),
    ])
}

fn state_icon(state: ActivityState, theme: Theme) -> (&'static str, Style) {
    match state {
        ActivityState::Proposed => ("·", theme.dim()),
        ActivityState::Asking => ("?", theme.pending()),
        ActivityState::Running => ("●", theme.pending()),
        ActivityState::Done => ("✓", theme.ok()),
        ActivityState::Denied | ActivityState::Failed => ("✗", theme.bad()),
    }
}

/// `(kind, target)` for an action, e.g. `("exec", "cargo test")` or `("notes", "search")`.
/// Both are safe to draw.
fn describe(request: &ActionRequest) -> (String, String) {
    let (kind, target) = match &request.action {
        Action::Sandbox { action } => match action {
            SandboxAction::Exec { command, .. } => ("exec".into(), command.clone()),
            SandboxAction::ReadFile { path } => ("read".into(), path.clone()),
            SandboxAction::WriteFile { path, .. } => ("write".into(), path.clone()),
            SandboxAction::ListDir { path } => ("list".into(), path.clone()),
            SandboxAction::JobStatus { job } => ("job-status".into(), job.to_string()),
            SandboxAction::JobOutput { job, .. } => ("job-output".into(), job.to_string()),
            SandboxAction::JobStop { job } => ("job-stop".into(), job.to_string()),
        },
        Action::Knowledge { action } => match action {
            KnowledgeAction::Call { source, tool, args } => (source.to_string(), format!("{tool} {args}")),
            KnowledgeAction::Record { source, entry } => (source.to_string(), format!("record {}", entry.title)),
        },
    };
    (safe::line(&kind), safe::line(&target))
}

fn draw_detail(frame: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let activity = app.selected().and_then(|i| app.activity.get(i));
    let title =
        activity.map_or_else(|| "Detail".to_string(), |a| format!("Detail: {}", truncate(&describe(&a.request).1, 40)));
    let block = pane(&title, false, theme);
    let Some(activity) = activity else {
        frame.render_widget(Paragraph::new(Line::styled("no actions yet", theme.dim())).block(block), area);
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    if let Some(reason) = &activity.request.reason {
        lines.push(Line::from(vec![
            Span::styled("why  ", theme.dim()),
            Span::raw(format!("\"{}\"", safe::line(reason))),
        ]));
    }
    match activity.observation.as_ref().map(|o| &o.outcome) {
        None => lines.push(Line::styled("waiting…", theme.dim())),
        Some(Outcome::Exec { exit_code, stdout, stderr }) => {
            let code = exit_code.map_or_else(|| "killed".to_string(), |c| c.to_string());
            lines.push(Line::from(vec![Span::styled("exit ", theme.dim()), Span::raw(code)]));
            lines.extend(output_lines(stdout).into_iter().map(Line::raw));
            lines.extend(output_lines(stderr).into_iter().map(|l| Line::styled(l, theme.bad())));
        }
        Some(Outcome::Text { content }) => lines.extend(output_lines(content).into_iter().map(Line::raw)),
        Some(Outcome::Listing { entries }) => lines.extend(
            entries
                .iter()
                .map(|e| Line::raw(safe::line(&if e.is_dir { format!("{}/", e.name) } else { e.name.clone() }))),
        ),
        Some(Outcome::Written { bytes }) => lines.push(Line::raw(format!("wrote {bytes} bytes"))),
        Some(Outcome::Recorded { location }) => {
            lines.push(Line::raw(format!(
                "recorded{}",
                location.as_ref().map(|l| format!(" at {}", safe::line(l))).unwrap_or_default()
            )));
        }
        Some(Outcome::Denied { reason }) => {
            lines.push(Line::styled(format!("denied: {}", safe::line(reason)), theme.bad()))
        }
        Some(Outcome::Failed { message }) => {
            lines.push(Line::styled(format!("failed: {}", safe::line(message)), theme.bad()))
        }
        Some(Outcome::Started { job }) => lines.push(Line::raw(format!("started job {job}"))),
    }
    if activity.observation.as_ref().is_some_and(|o| o.truncated) {
        lines.push(Line::styled("(output truncated)", theme.dim()));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}

fn draw_input(frame: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let block = pane("Message", app.focus == Focus::Input, theme);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let rows: Vec<&str> = app.input.split('\n').collect();
    let visible = rows.len().saturating_sub(usize::from(inner.height));
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(visible)
        .map(|(i, row)| Line::raw(format!("{}{}", if i == 0 { "> " } else { "  " }, safe::line(row))))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    if app.focus == Focus::Input && inner.area() > 0 {
        let last = rows.last().map_or(0, |r| safe::line(r).chars().count());
        let x =
            inner.x.saturating_add(u16::try_from(last + 2).unwrap_or(u16::MAX)).min(inner.right().saturating_sub(1));
        let y = inner.y.saturating_add(u16::try_from((rows.len() - visible).saturating_sub(1)).unwrap_or(0));
        frame.set_cursor_position((x, y));
    }
}

fn hint_line(app: &mut App, theme: Theme, width: u16) -> Line<'static> {
    let setup_keys = app.setup.as_ref().map(|setup| {
        let leave = match setup.step {
            Step::Choose { .. } if setup.first_run => "ctrl-c twice quit",
            Step::Choose { .. } => "esc cancel",
            _ => "esc back",
        };
        match setup.step {
            Step::Choose { .. } => format!("↑/↓ choose · enter select · {leave}"),
            Step::Pick(_) => format!("type to filter · ↑/↓ choose · enter select · {leave}"),
            Step::OpenRouterNotice => "y sign in · esc back".to_string(),
            Step::SigningIn(_) => "esc cancel".to_string(),
            Step::Key(_) | Step::LocalUrl(_) => format!("enter check & save · ctrl-u clear · {leave}"),
        }
    });
    let keys = if let Some(picker) = &app.place {
        let back = if picker.second { " · esc back" } else { "" };
        format!("↑/↓ choose · enter select · 1-{} pick{back} · ctrl-c twice quit", picker.rows().len())
    } else if let Some(keys) = setup_keys {
        keys
    } else if app.sessions.is_some() {
        "type to search · ↑/↓ choose · enter resume · ctrl-u clear · esc close".to_string()
    } else if app.approval().is_some() {
        match &app.notice {
            Some(notice) if app.confirming_auto(std::time::Instant::now()) => safe::line(notice),
            _ => "y allow · n deny · shift-tab auto · ctrl-c interrupt".to_string(),
        }
    } else if let Some(notice) = &app.notice {
        safe::line(notice)
    } else {
        let key = if app.setup_available { " · ctrl-a model setup" } else { "" };
        format!(
            "enter send · alt-enter newline · tab focus · shift-tab ask/auto · j/k move · ctrl-s sessions{key} · ctrl-c interrupt (twice: quit)"
        )
    };
    let auto = app.mode == ApprovalMode::Auto;
    let mode = if auto { "mode: AUTO ".to_string() } else { "mode: ask ".to_string() };
    let urgent = app.place.is_some()
        || app.setup.is_some()
        || app.sessions.is_some()
        || app.approval().is_some()
        || app.notice.is_some();
    let style = if urgent { theme.pending() } else { theme.dim() };
    let room = usize::from(width).saturating_sub(mode.chars().count() + 1);
    Line::from(vec![
        Span::styled(format!(" {:<room$}", truncate(&keys, room.saturating_sub(1))), style),
        if auto { Span::styled(mode, theme.bad().add_modifier(Modifier::BOLD)) } else { Span::raw(mode) },
    ])
}

/// The setup screen, over the panes. A key is only ever drawn masked.
fn draw_setup(frame: &mut Frame, app: &App, theme: Theme, over: Rect) {
    let Some(setup) = app.setup.as_ref() else { return };
    let mut cursor: Option<(usize, usize)> = None;
    let (title, mut lines): (&str, Vec<Line>) = match &setup.step {
        Step::Choose { selected } => {
            let mut lines = vec![Line::raw("How should Nixxa think?"), Line::default()];
            let details = [
                "Claude, with your API key",
                "LM Studio, Ollama or another server on this machine",
                "many models, billed to your OpenRouter account",
            ];
            for (i, (choice, detail)) in CHOICES.iter().zip(details).enumerate() {
                let marker = if i == *selected { "›" } else { " " };
                let style = if i == *selected { theme.accent() } else { Style::new() };
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker} {}  {choice}", i + 1), style.add_modifier(Modifier::BOLD)),
                    Span::styled(format!(" — {detail}"), theme.dim()),
                ]));
            }
            (" Set up a model ", lines)
        }
        Step::Key(form) => {
            let masked = form.masked();
            cursor = Some((3, 5 + masked.chars().count()));
            let mut lines = vec![
                Line::raw("Paste an API key from the Anthropic Console (console.anthropic.com)."),
                Line::styled(
                    "It is checked with Anthropic, then kept in the OS keychain, never in a file.",
                    theme.dim(),
                ),
                Line::default(),
                Line::from(vec![Span::styled("Key  ", theme.dim()), Span::raw(masked)]),
                Line::default(),
            ];
            lines.push(state_line(
                &form.state,
                "Only the last four characters are ever shown.",
                "Checking the key with Anthropic…",
                theme,
            ));
            (" Anthropic API key ", lines)
        }
        Step::LocalUrl(form) => {
            let url = safe::line(&form.url);
            cursor = Some((3, 5 + url.chars().count()));
            let mut lines = vec![
                Line::raw("The address of an OpenAI-compatible server on this machine (ending in /v1)."),
                Line::styled(format!("LM Studio: {LM_STUDIO_URL}   Ollama: {OLLAMA_URL}"), theme.dim()),
                Line::default(),
                Line::from(vec![Span::styled("URL  ", theme.dim()), Span::raw(url)]),
                Line::default(),
            ];
            lines.push(state_line(
                &form.state,
                "Start the server and load a model first.",
                "Asking the server for its models…",
                theme,
            ));
            (" Local model server ", lines)
        }
        Step::Pick(picker) => {
            let openrouter = picker.source == Source::OpenRouter;
            let place = if openrouter { "OpenRouter".to_string() } else { safe::line(&picker.url) };
            let mut lines = vec![
                Line::from(vec![Span::raw("Models on "), Span::styled(place, theme.dim())]),
                Line::from(vec![Span::styled("Filter ", theme.dim()), Span::raw(safe::line(&picker.filter))]),
            ];
            if openrouter {
                lines.push(Line::styled(PRIVACY_SHORT, theme.pending()));
            }
            lines.push(Line::default());
            let visible = picker.visible();
            let room = usize::from(over.height).saturating_sub(if openrouter { 11 } else { 9 }).max(3);
            let first = (picker.selected + 1).saturating_sub(room);
            for (i, model) in visible.iter().enumerate().skip(first).take(room) {
                let chosen = i == picker.selected;
                let marker = if chosen { "›" } else { " " };
                let mut marks = String::new();
                if model.supports_tools == Some(true) && !openrouter {
                    marks.push_str("  tools ✓");
                }
                if model.free == Some(true) {
                    marks.push_str("  free");
                }
                let style = if chosen { theme.accent().add_modifier(Modifier::BOLD) } else { Style::new() };
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker} {}", safe::line(&model.id)), style),
                    Span::styled(marks, theme.ok()),
                ]));
            }
            if visible.is_empty() {
                lines.push(Line::styled("  no model matches", theme.dim()));
            } else if visible.len() > room {
                lines.push(Line::styled(format!("  … {} models, type to narrow", visible.len()), theme.dim()));
            }
            lines.push(Line::default());
            lines.push(state_line(
                &picker.state,
                "The model must be able to call tools to run actions.",
                "Checking the model…",
                theme,
            ));
            (if openrouter { " Pick an OpenRouter model " } else { " Pick a local model " }, lines)
        }
        Step::OpenRouterNotice => {
            let mut lines = vec![Line::raw("Sign in with OpenRouter in your browser. Before you do:"), Line::default()];
            for line in PRIVACY_NOTICE {
                lines.push(Line::raw(format!("• {line}")));
            }
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled("[y]", theme.ok()),
                Span::raw(" I understand, sign in    "),
                Span::styled("[esc]", theme.dim()),
                Span::raw(" back"),
            ]));
            (" OpenRouter: privacy ", lines)
        }
        Step::SigningIn(wait) => {
            let mut lines = Vec::new();
            match &wait.url {
                None => lines.push(Line::raw("Starting the sign-in…")),
                Some(url) => {
                    lines.push(Line::raw(if wait.browser_opened {
                        "Your browser opened OpenRouter's sign-in page. If it didn't, open this address:"
                    } else {
                        "Open this address in your browser to sign in:"
                    }));
                    lines.push(Line::styled(safe::line(url), theme.accent()));
                }
            }
            lines.push(Line::default());
            let waiting = if wait.signed_in {
                "Signed in. Loading OpenRouter's models…"
            } else {
                "Waiting for sign-in… (esc to cancel)"
            };
            match &wait.state {
                SetupState::Failed(message) => {
                    lines.push(Line::styled(format!("✗ {}", safe::line(message)), theme.bad()));
                    lines.push(Line::styled("enter: try again · esc: back", theme.dim()));
                }
                _ => lines.push(Line::styled(format!("● {waiting}"), theme.pending())),
            }
            (" Sign in with OpenRouter ", lines)
        }
    };
    lines.insert(0, Line::default());
    // The sign-in page's address must copy cleanly: full width, no side borders to select by
    // accident.
    let full_width = matches!(setup.step, Step::SigningIn(_));
    let width = if full_width { over.width } else { over.width.min(84) };
    let side = if full_width { 0 } else { 2 };
    // Rows once wrapped to the panel's inner width, plus the border.
    let inner_width = usize::from(width.saturating_sub(side)).max(1);
    let rows: usize = lines.iter().map(|line| line.width().max(1).div_ceil(inner_width)).sum();
    let needed = u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(2);
    let height = needed.min(over.height);
    let area = Rect { x: over.x + (over.width - width) / 2, y: over.y + (over.height - height) / 2, width, height };
    let borders = if full_width { Borders::TOP | Borders::BOTTOM } else { Borders::ALL };
    let block = Block::default().borders(borders).title(title).border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
    if let Some((row, column)) = cursor.filter(|_| setup.state() != Some(&SetupState::Checking)) {
        let x = inner.x.saturating_add(u16::try_from(column).unwrap_or(u16::MAX)).min(inner.right().saturating_sub(1));
        let y = inner.y.saturating_add(u16::try_from(row + 1).unwrap_or(u16::MAX));
        if y < inner.bottom() {
            frame.set_cursor_position((x, y));
        }
    }
}

/// The start-up question: the sandbox, or the real files of the current directory.
fn draw_place(frame: &mut Frame, app: &App, theme: Theme, over: Rect) {
    let Some(picker) = app.place.as_ref() else { return };
    let question = if picker.second {
        "Sandbox with this directory: your real files, or a copy?"
    } else {
        "Where should the agent work?"
    };
    let mut lines = vec![
        Line::default(),
        Line::raw(question),
        Line::from(vec![Span::styled("Directory  ", theme.dim()), Span::raw(safe::line(&app.directory))]),
        Line::default(),
    ];
    for (i, (_row, name, detail)) in picker.rows().iter().enumerate() {
        let chosen = i == picker.selected;
        let marker = if chosen { "›" } else { " " };
        let reason = picker.reason(i);
        let style = match (reason.is_some(), chosen) {
            (true, _) => theme.dim(),
            (false, true) => theme.accent(),
            (false, false) => Style::new(),
        };
        let (detail, detail_style) = match reason {
            Some(why) => (safe::line(why), theme.bad()),
            None => ((*detail).to_string(), theme.dim()),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker} {}  {name}", i + 1), style.add_modifier(Modifier::BOLD)),
            Span::styled(format!(" — {detail}"), detail_style),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::styled("Every action asks you first; shift-tab switches to Auto at any time.", theme.dim()));
    let width = over.width.min(84);
    let inner_width = usize::from(width.saturating_sub(2)).max(1);
    let rows: usize = lines.iter().map(|line| line.width().max(1).div_ceil(inner_width)).sum();
    // Two borders, plus one row for word wrapping, which can take more rows than the width says.
    let height = u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(3).min(over.height);
    let area = Rect { x: over.x + (over.width - width) / 2, y: over.y + (over.height - height) / 2, width, height };
    let block = Block::bordered().title(" Start ").border_style(theme.accent());
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}

/// The session list (`ctrl-s`), over the panes: this directory's sessions, then the others.
fn draw_sessions(frame: &mut Frame, app: &App, theme: Theme, over: Rect) {
    let Some(list) = app.sessions.as_ref() else { return };
    let width = over.width.min(110);
    let inner = usize::from(width.saturating_sub(2));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| Timestamp(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)))
        .unwrap_or(Timestamp(0));
    let mut head = vec![
        Line::from(vec![
            Span::styled("Search  ", theme.dim()),
            Span::styled(format!("{}▌", safe::line(&list.query)), theme.accent()),
        ]),
        Line::default(),
    ];
    if list.loading() {
        head.push(Line::styled("Loading…", theme.dim()));
    }
    if let Some(why) = list.list_error() {
        head.push(Line::styled(format!("✗ {}", truncate(&safe::line(why), inner.saturating_sub(2))), theme.bad()));
    }
    if let Some(why) = &list.search_error {
        let why = truncate(&safe::line(why), inner.saturating_sub(18));
        head.push(Line::styled(format!("✗ search failed: {why}"), theme.bad()));
    }
    let rows = list.rows();
    if rows.is_empty() && !list.loading() && list.list_error().is_none() {
        let empty = if list.query.trim().is_empty() { "No sessions yet." } else { "Nothing matches." };
        head.push(Line::styled(empty, theme.dim()));
    }

    // Each row is a line, plus its snippet; a header starts each group.
    let mut body: Vec<Line> = Vec::new();
    let mut selected_line = 0;
    let mut group = None;
    for (i, row) in rows.iter().enumerate() {
        if group != Some(row.here) {
            group = Some(row.here);
            let title = if row.here { "This directory" } else { "Elsewhere" };
            body.push(Line::styled(title, theme.dim().add_modifier(Modifier::BOLD)));
        }
        let chosen = i == list.selected;
        if chosen {
            selected_line = body.len();
        }
        let workspace =
            if row.here { row.info.workspace.name.clone() } else { row.info.workspace.root.display().to_string() };
        let first = row.info.first_message.as_deref().and_then(|t| t.lines().next()).unwrap_or("(no message)");
        let style = if chosen { theme.accent().add_modifier(Modifier::BOLD) } else { Style::new() };
        let mut spans = vec![
            Span::styled(if chosen { "› " } else { "  " }, style),
            Span::styled(format!("{:<11}", ago(now, row.info.started_at)), theme.dim()),
            Span::styled(truncate(&safe::line(&workspace), 24), style),
            Span::styled(format!("  {}  ", truncate(&safe::line(&row.info.account.to_string()), 24)), theme.dim()),
        ];
        if row.open {
            spans.push(Span::styled("(open) ", theme.ok()));
        }
        let used: usize = spans.iter().map(Span::width).sum();
        spans.push(Span::styled(truncate(&safe::line(first), inner.saturating_sub(used)), style));
        body.push(Line::from(spans));
        if let Some(snippet) = row.snippet {
            let snippet = truncate(&safe::line(snippet), inner.saturating_sub(15));
            body.push(Line::styled(format!("             {snippet}"), theme.dim()));
        }
    }

    let height = over.height;
    let room = usize::from(height.saturating_sub(2)).saturating_sub(head.len()).max(1);
    // Keep the selected row (and its snippet) in view.
    let skip = (selected_line + 2).saturating_sub(room);
    let lines: Vec<Line> = head.into_iter().chain(body.into_iter().skip(skip).take(room)).collect();
    let area = Rect { x: over.x + (over.width - width) / 2, y: over.y, width, height };
    let block = Block::bordered().title(" Sessions ").border_style(theme.accent());
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

/// The status line under a setup form.
fn state_line(state: &SetupState, idle: &str, checking: &str, theme: Theme) -> Line<'static> {
    match state {
        SetupState::Editing => Line::styled(idle.to_string(), theme.dim()),
        SetupState::Checking => Line::styled(format!("● {checking}"), theme.pending()),
        SetupState::Failed(message) => Line::styled(format!("✗ {}", safe::line(message)), theme.bad()),
    }
}

fn draw_approval(frame: &mut Frame, app: &mut App, theme: Theme, over: Rect) {
    let Some(prompt) = app.approval() else { return };
    let (kind, target) = describe(&prompt.request);
    let mut lines = vec![Line::from(vec![Span::styled(format!("{kind:<6}"), theme.dim()), Span::raw(target)])];
    if let Action::Sandbox { action: SandboxAction::Exec { cwd: Some(cwd), .. } } = &prompt.request.action {
        lines.push(Line::from(vec![Span::styled("cwd   ", theme.dim()), Span::raw(safe::line(cwd))]));
    }
    if let Some(reason) = &prompt.request.reason {
        lines.push(Line::from(vec![
            Span::styled("why   ", theme.dim()),
            Span::raw(format!("\"{}\"", safe::line(reason))),
        ]));
    }
    lines.push(Line::styled(safe::line(&prompt.reason), theme.dim()));
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("[y]", theme.ok()),
        Span::raw(" allow  "),
        Span::styled("[n]", theme.bad()),
        Span::raw(" deny"),
    ]));

    let width = over.width.min(60);
    let height = u16::try_from(lines.len()).unwrap_or(u16::MAX).saturating_add(2).min(over.height);
    let area = Rect { x: over.x + (over.width - width) / 2, y: over.y + (over.height - height) / 2, width, height };
    let block = Block::bordered().title(" Approve action? ").border_style(theme.pending());
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
}

/// Untrusted output, one safe line per line; a trailing newline adds no empty line.
fn output_lines(text: &str) -> Vec<String> {
    let mut lines = safe::lines(text);
    if text.ends_with('\n') {
        lines.pop();
    }
    if text.is_empty() {
        lines.clear();
    }
    lines
}

/// Greedy word wrap by characters; long words are split.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut rows = Vec::new();
    for paragraph in text.split('\n') {
        let mut row = String::new();
        for word in paragraph.split(' ') {
            let mut word = word.to_string();
            loop {
                let row_len = row.chars().count();
                let word_len = word.chars().count();
                let needed = if row.is_empty() { word_len } else { row_len + 1 + word_len };
                if needed <= width {
                    if !row.is_empty() {
                        row.push(' ');
                    }
                    row.push_str(&word);
                    break;
                }
                if !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                    continue;
                }
                let head: String = word.chars().take(width).collect();
                word = word.chars().skip(width).collect();
                rows.push(head);
                if word.is_empty() {
                    break;
                }
            }
        }
        rows.push(row);
    }
    rows
}

fn truncate(text: &str, max: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

#[cfg(test)]
pub(crate) mod tests {
    use nixxa_contracts::{
        AccountRef, ActionId, BrainInfo, Event, Message, Observation, ProviderId, Role, SandboxActionKind,
        SandboxCapabilities, SessionId, SourceId, SourceInfo, Timestamp, TurnId,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::tests::key;
    use crate::place::Place;
    use crossterm::event::KeyCode;

    pub(crate) fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_session_list_groups_rows_and_shows_snippets_safely() {
        let mut app = started();
        app.root = std::env::temp_dir().join("proj");
        app.handle_key(crossterm::event::KeyEvent::new(KeyCode::Char('s'), crossterm::event::KeyModifiers::CONTROL));
        let info = |id: &str, root: std::path::PathBuf, first: &str| nixxa_contracts::SessionInfo {
            id: SessionId::new(id),
            workspace: nixxa_contracts::WorkspaceInfo { name: "proj".into(), root },
            account: AccountRef::new("anthropic", "work"),
            started_at: Timestamp(0),
            first_message: Some(first.into()),
        };
        app.sessions_listed(Ok(vec![
            info("s1", app.root.clone(), "fix \u{1b}[31mthe build\nsecond line"),
            info("s0", std::env::temp_dir().join("elsewhere"), "write docs"),
        ]));
        for c in "build".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        let hit = nixxa_contracts::SearchHit { session: SessionId::new("s0"), snippet: "the build log said".into() };
        app.sessions_found("build", Ok(vec![hit]));
        let text = screen(&mut app, 120, 20);
        assert!(text.contains(" Sessions ") && text.contains("Search  build"), "{text}");
        let here = text.find("This directory").expect("group header");
        let away = text.find("Elsewhere").expect("group header");
        assert!(here < away, "this directory first:\n{text}");
        assert!(text.contains("(open)") && text.contains("fix ^[[31mthe build"), "escaped, first line only:\n{text}");
        assert!(!text.contains("second line"));
        assert!(text.contains("the build log said"), "snippet shown:\n{text}");
        assert!(text.contains("enter resume"), "hints:\n{text}");
    }

    fn started() -> App {
        let mut app = App::new(false);
        app.apply(Event::SessionStarted {
            session: SessionId::new("s1"),
            account: AccountRef::new("scripted", "default"),
            brain: BrainInfo { provider: ProviderId::new("scripted"), model: "scripted-1".into() },
        });
        app.apply(Event::SandboxStatus {
            capabilities: SandboxCapabilities {
                backend: "fake".into(),
                isolated: false,
                network: false,
                jobs: false,
                actions: SandboxActionKind::ALL.to_vec(),
            },
        });
        app.apply(Event::SourceStatus {
            source: SourceInfo { id: SourceId::new("notes"), name: "Notes".into(), status: SourceStatus::Ready },
        });
        app.apply(Event::SourceStatus {
            source: SourceInfo { id: SourceId::new("vault"), name: "Vault".into(), status: SourceStatus::Missing },
        });
        app
    }

    #[test]
    fn the_start_question_shows_the_places_over_the_first_run_setup() {
        for width in [120, 60] {
            let mut app = App::new(false);
            app.directory = "/home/me/project".into();
            app.open_setup(true);
            app.ask_place(Vec::new());
            let screen = screen(&mut app, width, 24);
            for part in [
                "Where should the agent work?",
                "/home/me/project",
                "1  Sandbox (empty)",
                "2  This directory",
                "3  Sandbox with this directory",
            ] {
                assert!(screen.contains(part), "{part} missing at width {width}:\n{screen}");
            }
            assert!(!screen.contains("How should Nixxa think?"), "setup waits underneath:\n{screen}");
            assert!(screen.lines().last().unwrap().contains("enter select"), "{screen}");
            assert!(screen.contains("shift-tab switches"), "the last line fits at width {width}:\n{screen}");
        }
    }

    #[test]
    fn the_second_question_and_blocked_places_are_drawn() {
        let mut app = App::new(false);
        app.directory = "/home/me/project".into();
        app.ask_place(vec![(Place::Sandbox, "bubblewrap isn't installed".into())]);
        let first = screen(&mut app, 120, 24);
        assert!(first.contains("bubblewrap isn't installed"), "the reason replaces the description:\n{first}");
        app.handle_key(key(KeyCode::Char('3')));
        let second = screen(&mut app, 120, 24);
        for part in ["your real files, or a copy?", "1  Work on my files", "2  Work on a copy", "esc back"] {
            assert!(second.contains(part), "{part} missing:\n{second}");
        }
    }

    #[test]
    fn the_status_line_shows_the_approval_mode() {
        let mut app = App::new(false);
        assert!(screen(&mut app, 120, 24).lines().last().unwrap().contains("mode: ask"));
        app.handle_key(key(KeyCode::BackTab));
        assert!(screen(&mut app, 120, 24).lines().last().unwrap().contains("mode: AUTO"));
    }

    #[test]
    fn status_bar_shows_session_account_sandbox_and_sources() {
        let mut app = started();
        let screen = screen(&mut app, 120, 20);
        let status = screen.lines().next().unwrap();
        for part in ["s1", "scripted:default · scripted-1", "fake ○ NOT ISOLATED", "notes ●", "vault ○", "$0.00"]
        {
            assert!(status.contains(part), "missing {part:?} in {status:?}");
        }
        assert!(screen.contains("mode: ask"));
    }

    #[test]
    fn wide_screens_show_all_panes_and_narrow_ones_collapse() {
        let mut app = started();
        let wide = screen(&mut app, 120, 20);
        assert!(wide.contains("Conversation") && wide.contains("Activity") && wide.contains("Detail"));
        let narrow = screen(&mut app, 80, 20);
        assert!(narrow.contains("Conversation") && !narrow.contains("Activity"));
        app.focus = Focus::Activity;
        let narrow = screen(&mut app, 80, 20);
        assert!(!narrow.contains("Conversation") && narrow.contains("Activity"));
    }

    #[test]
    fn conversation_and_activity_render() {
        let mut app = started();
        let turn = TurnId::new("turn-0");
        app.apply(Event::Message { turn: turn.clone(), message: Message::text(Role::User, "list it", Timestamp(0)) });
        let request = ActionRequest {
            id: ActionId::new("a1"),
            action: Action::Sandbox {
                action: SandboxAction::Exec {
                    command: "cargo test".into(),
                    cwd: None,
                    timeout_ms: None,
                    background: false,
                },
            },
            reason: Some("check".into()),
        };
        app.apply(Event::ActionProposed { turn: turn.clone(), request });
        app.apply(Event::ActionFinished {
            turn,
            observation: Observation {
                action_id: ActionId::new("a1"),
                outcome: Outcome::Exec { exit_code: Some(0), stdout: "all good".into(), stderr: String::new() },
                duration_ms: 14,
                truncated: false,
            },
        });
        let screen = screen(&mut app, 120, 20);
        assert!(screen.contains("you   list it"));
        assert!(screen.contains("✓ exec   cargo test"));
        assert!(screen.contains("14 ms"));
        assert!(screen.contains("all good"));
    }

    #[test]
    fn tiny_terminals_do_not_panic() {
        let mut app = started();
        app.input = "a\nb\nc\nd\ne".into();
        let request = ActionRequest {
            id: ActionId::new("a1"),
            action: Action::Sandbox {
                action: SandboxAction::Exec {
                    command: "ls".into(),
                    cwd: Some("src".into()),
                    timeout_ms: None,
                    background: false,
                },
            },
            reason: Some("look".into()),
        };
        let (prompt, _answer) = crate::ports::PendingApproval::for_test(request, "approval needed");
        app.ask(prompt);
        assert!(screen(&mut app, 120, 20).contains("Approve action?"));
        for (width, height) in [(1, 1), (10, 5), (30, 8), (99, 12)] {
            screen(&mut app, width, height);
            app.focus = Focus::Activity;
            screen(&mut app, width, height);
            app.focus = Focus::Input;
        }
    }

    /// Everything a hostile string can carry onto the terminal.
    const HOSTILE: &str = "ok\x1b]0;PWNED-TITLE\x07\x1b]52;c;cHduZWQ=\x07\x1b[2J\r\u{202E}gnp.exe\u{9b}done";

    fn hostile_app() -> App {
        let mut app = started();
        let turn = TurnId::new("turn-0");
        app.apply(Event::Message { turn: turn.clone(), message: Message::text(Role::Agent, HOSTILE, Timestamp(0)) });
        app.apply(Event::Error { turn: Some(turn.clone()), message: HOSTILE.into() });
        let request = ActionRequest {
            id: ActionId::new("a1"),
            action: Action::Sandbox {
                action: SandboxAction::Exec {
                    command: HOSTILE.into(),
                    cwd: Some(HOSTILE.into()),
                    timeout_ms: None,
                    background: false,
                },
            },
            reason: Some(HOSTILE.into()),
        };
        app.apply(Event::ActionProposed { turn: turn.clone(), request });
        app.apply(Event::ActionFinished {
            turn,
            observation: Observation::new(
                ActionId::new("a1"),
                Outcome::Exec { exit_code: Some(0), stdout: HOSTILE.into(), stderr: HOSTILE.into() },
            ),
        });
        app.streaming = Some(HOSTILE.into());
        app.input = HOSTILE.into();
        app
    }

    /// Regression (PR #4 review, B1): content must never put its own control sequences on the
    /// host terminal. Drawn through the real crossterm backend into a byte buffer.
    #[test]
    fn hostile_content_never_reaches_the_terminal_raw() {
        use ratatui::backend::CrosstermBackend;
        use ratatui::{TerminalOptions, Viewport};

        let mut app = hostile_app();
        let (prompt, _answer) = crate::ports::PendingApproval::for_test(app.activity[0].request.clone(), HOSTILE);
        app.ask(prompt);
        /// Keeps what the backend writes, readable after the draw.
        #[derive(Clone, Default)]
        struct Captured(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.borrow_mut().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let captured = Captured::default();
        let viewport = Viewport::Fixed(Rect::new(0, 0, 160, 40));
        let mut terminal =
            Terminal::with_options(CrosstermBackend::new(captured.clone()), TerminalOptions { viewport }).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let bytes = captured.0.borrow().clone();
        let output = String::from_utf8_lossy(&bytes);

        assert!(!output.contains("\x1b]"), "an OSC sequence (title, clipboard) reached the terminal");
        assert!(!output.contains('\x07'), "a BEL reached the terminal");
        assert!(!output.contains("\x1b[2J"), "a clear-screen reached the terminal");
        assert!(!output.contains('\r'), "a carriage return reached the terminal");
        assert!(!output.contains('\u{202E}'), "a bidi override reached the terminal");
        assert!(!output.contains('\u{9b}'), "a C1 control reached the terminal");
        assert!(output.contains("^[]0;PWNED-TITLE^G"), "the sequence is shown, visibly");
    }

    #[test]
    fn every_pane_shows_hostile_content_visibly() {
        let mut app = hostile_app();
        let screen = screen(&mut app, 200, 40);
        assert!(!screen.chars().any(|c| c.is_control() && c != '\n'));
        assert!(screen.contains("^[]0;PWNED-TITLE^G"));
    }

    #[test]
    fn the_approval_prompt_cannot_be_spoofed() {
        let mut app = started();
        let request = ActionRequest {
            id: ActionId::new("a1"),
            action: Action::Sandbox {
                action: SandboxAction::Exec {
                    command: "rm -rf ~ \r\x1b[2Kecho harmless".into(),
                    cwd: None,
                    timeout_ms: None,
                    background: false,
                },
            },
            reason: Some("tidy \u{202E}up".into()),
        };
        let (prompt, _answer) = crate::ports::PendingApproval::for_test(request, "approval needed");
        app.ask(prompt);
        let screen = screen(&mut app, 120, 24);
        assert!(screen.contains("rm -rf ~ ^M^[[2Kecho harmless"), "the real command is shown in full:\n{screen}");
        assert!(screen.contains("tidy <U+202E>up"));
    }

    #[test]
    fn queued_messages_show_dimmed_after_the_conversation() {
        let mut app = started();
        app.queued.push_back("later".into());
        assert!(screen(&mut app, 120, 20).contains("you   later (queued)"));
    }

    #[test]
    fn the_setup_screen_never_draws_the_key() {
        let key = "sk-ant-api03-SECRETSECRET-wxyz";
        let mut app = started();
        app.open_setup(true);
        let screen = screen(&mut app, 120, 24);
        assert!(screen.contains("How should Nixxa think?") && screen.contains("Local model"), "{screen}");
        assert!(screen.contains("ctrl-c twice quit"), "first run, first step: quitting is the way out");
        app.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Char('1')));
        app.paste(key);
        let screen = screen_of(&mut app);
        assert!(screen.contains("Anthropic API key") && screen.contains("wxyz"), "{screen}");
        assert!(!screen.contains("SECRET") && !screen.contains("sk-ant"), "{screen}");
        assert!(screen.contains("enter check & save") && screen.contains("esc back"));

        app.account_saved(&Err("Anthropic rejected this key: invalid x-api-key".into()));
        let screen = screen_of(&mut app);
        assert!(screen.contains("✗ Anthropic rejected this key"), "{screen}");
    }

    #[test]
    fn the_local_setup_shows_the_url_then_the_models() {
        use crossterm::event::KeyCode;
        let key = crate::app::tests::key;
        let mut app = started();
        app.open_setup(false);
        app.handle_key(key(KeyCode::Char('2')));
        let screen = screen_of(&mut app);
        assert!(screen.contains("Local model server") && screen.contains("http://localhost:1234/v1"), "{screen}");
        assert!(screen.contains("Ollama: http://localhost:11434/v1") && screen.contains("esc back"));
        app.handle_key(key(KeyCode::Enter));
        assert!(screen_of(&mut app).contains("Asking the server for its models"));
        app.models_listed(Ok(vec![
            nixxa_brain::ModelInfo { id: "openai/gpt-oss-20b".into(), supports_tools: None, free: None },
            nixxa_brain::ModelInfo { id: "qwen3-8b".into(), supports_tools: Some(true), free: None },
            nixxa_brain::ModelInfo {
                id: "text-embedding-nomic-embed-text-v1.5".into(),
                supports_tools: None,
                free: None,
            },
        ]));
        let screen = screen_of(&mut app);
        assert!(screen.contains("Pick a local model") && screen.contains("› openai/gpt-oss-20b"), "{screen}");
        assert!(screen.contains("qwen3-8b  tools ✓") && !screen.contains("nomic-embed"), "{screen}");
        assert!(screen.contains("↑/↓ choose · enter select · esc back"));
    }

    #[test]
    fn the_openrouter_steps_show_the_notice_the_url_and_marked_models() {
        use crossterm::event::KeyCode;
        let key = crate::app::tests::key;
        let mut app = started();
        app.open_setup(false);
        assert!(screen_of(&mut app).contains("Sign in with OpenRouter"));
        app.handle_key(key(KeyCode::Char('3')));
        let shown = screen(&mut app, 140, 30);
        assert!(shown.contains("OpenRouter: privacy") && shown.contains("Free models may log"), "{shown}");
        assert!(shown.contains("workspace") && shown.contains("[y] I understand, sign in"), "{shown}");
        app.handle_key(key(KeyCode::Char('y')));
        let url = "https://openrouter.ai/auth?callback_url=http%3A%2F%2Flocalhost%3A5555%2Fcallback&code_challenge=abc&code_challenge_method=S256";
        app.setup.as_mut().unwrap().sign_in_started(url.into(), false);
        let shown = screen(&mut app, 200, 30);
        assert!(shown.contains("Open this address in your browser") && shown.contains("code_challenge=abc"), "{shown}");
        assert!(shown.contains("Waiting for sign-in… (esc to cancel)"));
        app.models_listed(Ok(vec![
            nixxa_brain::ModelInfo { id: "qwen/qwen3-coder:free".into(), supports_tools: Some(true), free: Some(true) },
            nixxa_brain::ModelInfo {
                id: "anthropic/claude-sonnet-5.5".into(),
                supports_tools: Some(true),
                free: Some(false),
            },
        ]));
        let shown = screen(&mut app, 140, 30);
        assert!(shown.contains("Pick an OpenRouter model") && shown.contains("qwen/qwen3-coder:free  free"), "{shown}");
        assert!(shown.contains("free models may log"), "the picker repeats the warning: {shown}");
        assert!(shown.contains("type to filter"));
    }

    /// Regression: on a narrow terminal the choices wrap, and the panel must grow to show both.
    #[test]
    fn both_choices_stay_visible_on_a_narrow_terminal() {
        let mut app = started();
        app.open_setup(true);
        let screen = screen(&mut app, 60, 24);
        assert!(screen.contains("Anthropic API key") && screen.contains("Local model"), "{screen}");
    }

    #[test]
    fn the_hint_bar_offers_the_key_setup_when_available() {
        let mut app = started();
        assert!(!screen(&mut app, 140, 20).contains("ctrl-a model setup"));
        app.setup_available = true;
        assert!(screen(&mut app, 140, 20).contains("ctrl-a model setup"));
    }

    fn screen_of(app: &mut App) -> String {
        screen(app, 120, 24)
    }

    #[test]
    fn wrap_breaks_on_words_and_splits_long_ones() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("abcdefg", 3), vec!["abc", "def", "g"]);
        assert_eq!(wrap("a\nb", 10), vec!["a", "b"]);
    }

    #[test]
    fn truncate_marks_the_cut() {
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(truncate("hi", 5), "hi");
    }
}
