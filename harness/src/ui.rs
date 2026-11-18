//! Draws [`App`] with ratatui. Layout and visual language follow DESIGN.md §4.

use nixxa_contracts::{Action, ActionRequest, KnowledgeAction, Outcome, SandboxAction, SourceStatus};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::{Activity, ActivityState, App, Focus, Speaker};

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
        part(&mut spans, session.to_string(), Style::new());
    }
    if let Some(account) = &app.account {
        let model = app.brain.as_ref().map(|b| format!(" · {}", b.model)).unwrap_or_default();
        part(&mut spans, format!("{account}{model}"), Style::new());
    }
    match &app.sandbox {
        Some(caps) if caps.isolated => part(&mut spans, format!("{} ● isolated", caps.backend), theme.ok()),
        Some(caps) => part(&mut spans, format!("{} ○ NOT ISOLATED", caps.backend), theme.bad()),
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
            spans.push(Span::raw(format!(" {} ", source.id)));
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
    let entries = app.conversation.iter().map(|e| (e.speaker, e.text.clone())).chain(streaming);
    for (speaker, text) in entries {
        let (label, style) = match speaker {
            Speaker::User => ("you", Style::new()),
            Speaker::Agent => ("nixxa", theme.accent()),
            Speaker::System => ("note", theme.dim()),
            Speaker::Error => ("error", theme.bad()),
        };
        for (i, row) in wrap(&text, width).into_iter().enumerate() {
            let label = if i == 0 { label } else { "" };
            lines.push(Line::from(vec![
                Span::styled(format!("{label:<LABEL$}"), style.add_modifier(Modifier::BOLD)),
                Span::styled(row, if speaker == Speaker::User { Style::new() } else { style }),
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
fn describe(request: &ActionRequest) -> (String, String) {
    match &request.action {
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
    }
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
        lines.push(Line::from(vec![Span::styled("why  ", theme.dim()), Span::raw(format!("\"{reason}\""))]));
    }
    match activity.observation.as_ref().map(|o| &o.outcome) {
        None => lines.push(Line::styled("waiting…", theme.dim())),
        Some(Outcome::Exec { exit_code, stdout, stderr }) => {
            let code = exit_code.map_or_else(|| "killed".to_string(), |c| c.to_string());
            lines.push(Line::from(vec![Span::styled("exit ", theme.dim()), Span::raw(code)]));
            lines.extend(stdout.lines().map(|l| Line::raw(l.to_string())));
            lines.extend(stderr.lines().map(|l| Line::styled(l.to_string(), theme.bad())));
        }
        Some(Outcome::Text { content }) => lines.extend(content.lines().map(|l| Line::raw(l.to_string()))),
        Some(Outcome::Listing { entries }) => lines
            .extend(entries.iter().map(|e| Line::raw(if e.is_dir { format!("{}/", e.name) } else { e.name.clone() }))),
        Some(Outcome::Written { bytes }) => lines.push(Line::raw(format!("wrote {bytes} bytes"))),
        Some(Outcome::Recorded { location }) => {
            lines.push(Line::raw(format!(
                "recorded{}",
                location.as_ref().map(|l| format!(" at {l}")).unwrap_or_default()
            )));
        }
        Some(Outcome::Denied { reason }) => lines.push(Line::styled(format!("denied: {reason}"), theme.bad())),
        Some(Outcome::Failed { message }) => lines.push(Line::styled(format!("failed: {message}"), theme.bad())),
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
        .map(|(i, row)| Line::raw(format!("{}{row}", if i == 0 { "> " } else { "  " })))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    if app.focus == Focus::Input && inner.area() > 0 {
        let last = rows.last().map_or(0, |r| r.chars().count());
        let x =
            inner.x.saturating_add(u16::try_from(last + 2).unwrap_or(u16::MAX)).min(inner.right().saturating_sub(1));
        let y = inner.y.saturating_add(u16::try_from((rows.len() - visible).saturating_sub(1)).unwrap_or(0));
        frame.set_cursor_position((x, y));
    }
}

fn hint_line(app: &mut App, theme: Theme, width: u16) -> Line<'static> {
    let keys = if app.approval().is_some() {
        "y allow · n deny · ctrl-c interrupt".to_string()
    } else if let Some(notice) = &app.notice {
        notice.clone()
    } else {
        "enter send · alt-enter newline · tab focus · j/k move · ctrl-c interrupt (twice: quit)".to_string()
    };
    let mode = format!("mode: {} ", format!("{:?}", app.mode).to_uppercase());
    let style = if app.approval().is_some() || app.notice.is_some() { theme.pending() } else { theme.dim() };
    let room = usize::from(width).saturating_sub(mode.chars().count() + 1);
    Line::from(vec![
        Span::styled(format!(" {:<room$}", truncate(&keys, room.saturating_sub(1))), style),
        Span::raw(mode),
    ])
}

fn draw_approval(frame: &mut Frame, app: &mut App, theme: Theme, over: Rect) {
    let Some(prompt) = app.approval() else { return };
    let (kind, target) = describe(&prompt.request);
    let mut lines = vec![Line::from(vec![Span::styled(format!("{kind:<6}"), theme.dim()), Span::raw(target)])];
    if let Action::Sandbox { action: SandboxAction::Exec { cwd: Some(cwd), .. } } = &prompt.request.action {
        lines.push(Line::from(vec![Span::styled("cwd   ", theme.dim()), Span::raw(cwd.clone())]));
    }
    if let Some(reason) = &prompt.request.reason {
        lines.push(Line::from(vec![Span::styled("why   ", theme.dim()), Span::raw(format!("\"{reason}\""))]));
    }
    lines.push(Line::styled(prompt.reason.clone(), theme.dim()));
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
mod tests {
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
