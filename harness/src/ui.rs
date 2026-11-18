//! Draws [`App`] with ratatui. Layout and visual language follow DESIGN.md §4.

use nixxa_contracts::{Action, ActionRequest, KnowledgeAction, Outcome, SandboxAction, SourceStatus};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::{Activity, ActivityState, App, Focus, SetupState, Speaker};
use crate::safe;

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
    let setup = app.setup.as_ref().map(|form| form.first_run);
    let keys = if let Some(first_run) = setup {
        let leave = if first_run { "ctrl-c twice quit" } else { "esc cancel" };
        format!("enter check & save · ctrl-u clear · {leave}")
    } else if app.approval().is_some() {
        "y allow · n deny · ctrl-c interrupt".to_string()
    } else if let Some(notice) = &app.notice {
        safe::line(notice)
    } else {
        let key = if app.setup_available { " · ctrl-a API key" } else { "" };
        format!("enter send · alt-enter newline · tab focus · j/k move{key} · ctrl-c interrupt (twice: quit)")
    };
    let mode = format!("mode: {} ", format!("{:?}", app.mode).to_uppercase());
    let urgent = setup.is_some() || app.approval().is_some() || app.notice.is_some();
    let style = if urgent { theme.pending() } else { theme.dim() };
    let room = usize::from(width).saturating_sub(mode.chars().count() + 1);
    Line::from(vec![
        Span::styled(format!(" {:<room$}", truncate(&keys, room.saturating_sub(1))), style),
        Span::raw(mode),
    ])
}

/// The API-key setup form, over the panes. The key is only ever drawn masked.
fn draw_setup(frame: &mut Frame, app: &App, theme: Theme, over: Rect) {
    let Some(form) = app.setup.as_ref() else { return };
    let masked = form.masked();
    let mut lines = vec![
        Line::raw("Paste an API key from the Anthropic Console (console.anthropic.com)."),
        Line::styled("It is checked with Anthropic, then kept in the OS keychain, never in a file.", theme.dim()),
        Line::default(),
        Line::from(vec![Span::styled("Key  ", theme.dim()), Span::raw(masked.clone())]),
        Line::default(),
    ];
    match &form.state {
        SetupState::Editing => lines.push(Line::styled("Only the last four characters are ever shown.", theme.dim())),
        SetupState::Checking => lines.push(Line::styled("● Checking the key with Anthropic…", theme.pending())),
        SetupState::Failed(message) => lines.push(Line::styled(format!("✗ {}", safe::line(message)), theme.bad())),
    }
    let width = over.width.min(78);
    let height = 9.min(over.height);
    let area = Rect { x: over.x + (over.width - width) / 2, y: over.y + (over.height - height) / 2, width, height };
    let block = Block::bordered().title(" Anthropic API key ").border_style(theme.accent());
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
    if form.state != SetupState::Checking && inner.height > 3 {
        let x = inner.x.saturating_add(u16::try_from(5 + masked.chars().count()).unwrap_or(u16::MAX));
        frame.set_cursor_position((x.min(inner.right().saturating_sub(1)), inner.y + 3));
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

    pub(crate) fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
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
    fn status_bar_shows_session_account_sandbox_and_sources() {
        let mut app = started();
        let screen = screen(&mut app, 120, 20);
        let status = screen.lines().next().unwrap();
        for part in ["s1", "scripted:default · scripted-1", "fake ○ NOT ISOLATED", "notes ●", "vault ○", "$0.00"]
        {
            assert!(status.contains(part), "missing {part:?} in {status:?}");
        }
        assert!(screen.contains("mode: ASK"));
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
                action: SandboxAction::Exec { command: "cargo test".into(), cwd: None, timeout_ms: None },
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
                action: SandboxAction::Exec { command: "ls".into(), cwd: Some("src".into()), timeout_ms: None },
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
                action: SandboxAction::Exec { command: HOSTILE.into(), cwd: Some(HOSTILE.into()), timeout_ms: None },
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
        app.paste(key);
        let screen = screen(&mut app, 120, 24);
        assert!(screen.contains("Anthropic API key") && screen.contains("wxyz"), "{screen}");
        assert!(!screen.contains("SECRET") && !screen.contains("sk-ant"), "{screen}");
        assert!(screen.contains("enter check & save") && screen.contains("ctrl-c twice quit"));

        app.key_saved(&Err("Anthropic rejected this key: invalid x-api-key".into()));
        let screen = screen_of(&mut app);
        assert!(screen.contains("✗ Anthropic rejected this key"), "{screen}");
    }

    #[test]
    fn the_hint_bar_offers_the_key_setup_when_available() {
        let mut app = started();
        assert!(!screen(&mut app, 140, 20).contains("ctrl-a API key"));
        app.setup_available = true;
        assert!(screen(&mut app, 140, 20).contains("ctrl-a API key"));
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
