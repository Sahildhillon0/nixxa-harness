//! TUI state: what is on screen and how keys change it. No terminal I/O here, so all of it is
//! unit-testable; `ui` draws it and `tui` drives it.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use nixxa_contracts::{
    AccountRef, ActionRequest, ApprovalMode, BrainInfo, Content, Event, Observation, Outcome, Role,
    SandboxCapabilities, SessionId, SourceInfo, Verdict,
};
use nixxa_controller::Command;

use crate::ports::PendingApproval;

/// How long a first ctrl-c waits for a second one to quit.
const QUIT_WINDOW: Duration = Duration::from_secs(2);

/// Which pane keys go to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Input,
    Conversation,
    Activity,
}

impl Focus {
    fn next(self) -> Self {
        match self {
            Self::Input => Self::Conversation,
            Self::Conversation => Self::Activity,
            Self::Activity => Self::Input,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Input => Self::Activity,
            Self::Conversation => Self::Input,
            Self::Activity => Self::Conversation,
        }
    }
}

/// Who a conversation entry is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Speaker {
    User,
    Agent,
    System,
    Error,
}

/// One entry in the conversation pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) speaker: Speaker,
    pub(crate) text: String,
}

/// Where an action is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivityState {
    Proposed,
    Asking,
    Running,
    Done,
    Denied,
    Failed,
}

/// One line in the activity pane.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Activity {
    pub(crate) request: ActionRequest,
    pub(crate) state: ActivityState,
    pub(crate) observation: Option<Observation>,
}

/// What the UI loop must do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    Send(Command),
    Quit,
}

/// Everything the TUI shows.
pub(crate) struct App {
    pub(crate) color: bool,
    pub(crate) session: Option<SessionId>,
    pub(crate) account: Option<AccountRef>,
    pub(crate) brain: Option<BrainInfo>,
    pub(crate) sandbox: Option<SandboxCapabilities>,
    pub(crate) sources: Vec<SourceInfo>,
    pub(crate) mode: ApprovalMode,
    pub(crate) cost_micro_usd: u64,
    pub(crate) conversation: Vec<Entry>,
    /// Text streamed for the agent message being written; replaced by the final message.
    pub(crate) streaming: Option<String>,
    /// Messages sent while a turn was running, waiting for the controller to start them.
    pub(crate) queued: VecDeque<String>,
    pub(crate) activity: Vec<Activity>,
    /// The selected activity line; `None` = follow the newest.
    selected: Option<usize>,
    /// Lines scrolled up from the bottom of the conversation.
    pub(crate) scroll_back: usize,
    pub(crate) input: String,
    pub(crate) focus: Focus,
    approvals: VecDeque<PendingApproval>,
    pub(crate) busy: bool,
    pub(crate) notice: Option<String>,
    pub(crate) ended: bool,
    last_ctrl_c: Option<Instant>,
}

impl App {
    pub(crate) fn new(color: bool) -> Self {
        Self {
            color,
            session: None,
            account: None,
            brain: None,
            sandbox: None,
            sources: Vec::new(),
            mode: ApprovalMode::Ask,
            cost_micro_usd: 0,
            conversation: Vec::new(),
            streaming: None,
            queued: VecDeque::new(),
            activity: Vec::new(),
            selected: None,
            scroll_back: 0,
            input: String::new(),
            focus: Focus::Input,
            approvals: VecDeque::new(),
            busy: false,
            notice: None,
            ended: false,
            last_ctrl_c: None,
        }
    }

    /// The approval prompt on screen, if any.
    pub(crate) fn approval(&mut self) -> Option<&PendingApproval> {
        self.approvals.retain(|prompt| !prompt.is_abandoned());
        self.approvals.front()
    }

    /// Adds a note from the harness itself to the conversation.
    pub(crate) fn note(&mut self, text: impl Into<String>) {
        self.conversation.push(Entry { speaker: Speaker::System, text: text.into() });
    }

    /// Queues an approval prompt.
    pub(crate) fn ask(&mut self, prompt: PendingApproval) {
        self.approvals.push_back(prompt);
    }

    /// The index of the selected activity line.
    pub(crate) fn selected(&self) -> Option<usize> {
        self.selected.or_else(|| self.activity.len().checked_sub(1))
    }

    /// Applies one controller event.
    pub(crate) fn apply(&mut self, event: Event) {
        match event {
            Event::SessionStarted { session, account, brain } => {
                self.session = Some(session);
                self.account = Some(account);
                self.brain = Some(brain);
            }
            Event::SandboxStatus { capabilities } => self.sandbox = Some(capabilities),
            Event::SourceStatus { source } => match self.sources.iter_mut().find(|s| s.id == source.id) {
                Some(existing) => *existing = source,
                None => self.sources.push(source),
            },
            Event::ModeChanged { mode } => self.mode = mode,
            Event::AccountChanged { account } => self.account = Some(account),
            Event::TurnStarted { .. } => {
                self.busy = true;
                self.streaming = None;
                self.scroll_back = 0;
            }
            Event::TextDelta { text, .. } => self.streaming.get_or_insert_with(String::new).push_str(&text),
            Event::Message { message, .. } => {
                let Content::Text { text } = message.content else { return };
                let speaker = match message.role {
                    Role::User => {
                        // Its turn started: it is no longer waiting.
                        if self.queued.front() == Some(&text) {
                            self.queued.pop_front();
                        }
                        Speaker::User
                    }
                    Role::Agent => {
                        // The final message replaces whatever was streamed for it.
                        self.streaming = None;
                        Speaker::Agent
                    }
                    Role::System => Speaker::System,
                    Role::Observation => return,
                };
                self.conversation.push(Entry { speaker, text });
            }
            Event::ActionProposed { request, .. } => {
                self.activity.push(Activity { request, state: ActivityState::Proposed, observation: None });
            }
            Event::ApprovalNeeded { request, .. } => self.set_state(&request.id, ActivityState::Asking),
            Event::ActionStarted { action_id, .. } => self.set_state(&action_id, ActivityState::Running),
            Event::ActionFinished { observation, .. } => {
                let state = match observation.outcome {
                    Outcome::Denied { .. } => ActivityState::Denied,
                    Outcome::Failed { .. } => ActivityState::Failed,
                    _ => ActivityState::Done,
                };
                if let Some(activity) = self.activity.iter_mut().rev().find(|a| a.request.id == observation.action_id) {
                    activity.state = state;
                    activity.observation = Some(observation);
                }
            }
            Event::Usage { usage, .. } => self.cost_micro_usd += usage.cost_micro_usd.unwrap_or(0),
            Event::Error { message, .. } => self.conversation.push(Entry { speaker: Speaker::Error, text: message }),
            Event::TurnDone { .. } => {
                self.busy = false;
                // Text streamed by a turn that never finished its message (interrupt, error).
                if let Some(text) = self.streaming.take().filter(|t| !t.is_empty()) {
                    self.conversation.push(Entry { speaker: Speaker::Agent, text });
                }
            }
            Event::SessionEnded { .. } => self.ended = true,
        }
    }

    fn set_state(&mut self, id: &nixxa_contracts::ActionId, state: ActivityState) {
        if let Some(activity) = self.activity.iter_mut().rev().find(|a| &a.request.id == id) {
            activity.state = state;
        }
    }

    /// Handles one key press.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if key.kind != KeyEventKind::Press {
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.ctrl_c();
        }
        self.last_ctrl_c = None;
        if self.approval().is_some() {
            self.answer_approval(key.code);
            return Vec::new();
        }
        self.notice = None;
        match key.code {
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.previous(),
            _ => match self.focus {
                Focus::Input => return self.edit_input(key),
                Focus::Conversation => self.scroll_conversation(key.code),
                Focus::Activity => self.move_selection(key.code),
            },
        }
        Vec::new()
    }

    fn ctrl_c(&mut self) -> Vec<Effect> {
        let now = Instant::now();
        if self.last_ctrl_c.is_some_and(|last| now.duration_since(last) < QUIT_WINDOW) {
            return vec![Effect::Quit];
        }
        self.last_ctrl_c = Some(now);
        if self.busy {
            self.notice = Some("interrupting… (ctrl-c again to quit)".into());
            vec![Effect::Send(Command::Interrupt)]
        } else {
            self.notice = Some("press ctrl-c again to quit".into());
            Vec::new()
        }
    }

    fn answer_approval(&mut self, code: KeyCode) {
        let verdict = match code {
            KeyCode::Char('y' | 'Y') => Verdict::Allow,
            KeyCode::Char('n' | 'N') => Verdict::Deny("denied by the user".into()),
            _ => return,
        };
        if let Some(prompt) = self.approvals.pop_front() {
            prompt.answer(verdict);
        }
    }

    fn edit_input(&mut self, key: KeyEvent) -> Vec<Effect> {
        let newline = key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
        match key.code {
            KeyCode::Enter if newline => self.input.push('\n'),
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                if !text.trim().is_empty() {
                    if self.busy {
                        // The controller runs it after the current turn; show it until then.
                        self.queued.push_back(text.clone());
                    }
                    return vec![Effect::Send(Command::UserMessage(text))];
                }
            }
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Backspace => {
                self.input.pop();
            }
            _ => {}
        }
        Vec::new()
    }

    fn scroll_conversation(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('k') | KeyCode::Up => self.scroll_back += 1,
            KeyCode::Char('j') | KeyCode::Down => self.scroll_back = self.scroll_back.saturating_sub(1),
            KeyCode::End => self.scroll_back = 0,
            _ => {}
        }
    }

    fn move_selection(&mut self, code: KeyCode) {
        let Some(last) = self.activity.len().checked_sub(1) else { return };
        let current = self.selected().unwrap_or(last);
        let next = match code {
            KeyCode::Char('k') | KeyCode::Up => current.saturating_sub(1),
            KeyCode::Char('j') | KeyCode::Down => (current + 1).min(last),
            KeyCode::End => last,
            _ => return,
        };
        // Back on the newest line: follow new actions again.
        self.selected = (next != last).then_some(next);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use nixxa_contracts::{Action, ActionId, Message, SandboxAction, SourceId, SourceStatus, Timestamp, TurnId, Usage};

    use super::*;

    pub(crate) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_c() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
    }

    fn turn() -> TurnId {
        TurnId::new("turn-0")
    }

    fn request(id: &str) -> ActionRequest {
        ActionRequest {
            id: ActionId::new(id),
            action: Action::Sandbox {
                action: SandboxAction::Exec { command: "ls".into(), cwd: None, timeout_ms: None },
            },
            reason: None,
        }
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    #[test]
    fn enter_sends_the_input_and_clears_it() {
        let mut app = App::new(true);
        type_text(&mut app, "hi");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), vec![Effect::Send(Command::UserMessage("hi".into()))]);
        assert!(app.input.is_empty());
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty(), "blank input is not sent");
    }

    #[test]
    fn a_message_sent_during_a_turn_shows_as_queued_until_its_turn_starts() {
        let mut app = App::new(true);
        app.apply(Event::TurnStarted { turn: turn() });
        type_text(&mut app, "next");
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.queued, ["next"]);
        app.apply(Event::TurnDone { turn: turn() });
        app.apply(Event::TurnStarted { turn: TurnId::new("turn-1") });
        app.apply(Event::Message {
            turn: TurnId::new("turn-1"),
            message: Message::text(Role::User, "next", Timestamp(0)),
        });
        assert!(app.queued.is_empty());
        assert_eq!(app.conversation, vec![Entry { speaker: Speaker::User, text: "next".into() }]);
    }

    #[test]
    fn alt_enter_adds_a_newline() {
        let mut app = App::new(true);
        type_text(&mut app, "a");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
        type_text(&mut app, "b");
        assert_eq!(app.input, "a\nb");
    }

    #[test]
    fn streamed_text_is_replaced_by_the_final_message() {
        let mut app = App::new(true);
        app.apply(Event::TurnStarted { turn: turn() });
        app.apply(Event::TextDelta { turn: turn(), text: "Hel".into() });
        app.apply(Event::TextDelta { turn: turn(), text: "lo".into() });
        assert_eq!(app.streaming.as_deref(), Some("Hello"));
        app.apply(Event::Message { turn: turn(), message: Message::text(Role::Agent, "Hello", Timestamp(0)) });
        app.apply(Event::TurnDone { turn: turn() });
        assert_eq!(app.conversation, vec![Entry { speaker: Speaker::Agent, text: "Hello".into() }]);
        assert!(!app.busy);
    }

    #[test]
    fn unfinished_streamed_text_is_kept_when_the_turn_ends() {
        let mut app = App::new(true);
        app.apply(Event::TurnStarted { turn: turn() });
        app.apply(Event::TextDelta { turn: turn(), text: "Hold on".into() });
        app.apply(Event::TurnDone { turn: turn() });
        assert_eq!(app.conversation, vec![Entry { speaker: Speaker::Agent, text: "Hold on".into() }]);
    }

    #[test]
    fn actions_move_through_their_states() {
        let mut app = App::new(true);
        app.apply(Event::ActionProposed { turn: turn(), request: request("a1") });
        assert_eq!(app.activity[0].state, ActivityState::Proposed);
        app.apply(Event::ApprovalNeeded { turn: turn(), request: request("a1"), reason: "r".into() });
        assert_eq!(app.activity[0].state, ActivityState::Asking);
        app.apply(Event::ActionStarted { turn: turn(), action_id: ActionId::new("a1") });
        assert_eq!(app.activity[0].state, ActivityState::Running);
        app.apply(Event::ActionFinished {
            turn: turn(),
            observation: Observation::new(ActionId::new("a1"), Outcome::Written { bytes: 1 }),
        });
        assert_eq!(app.activity[0].state, ActivityState::Done);
        app.apply(Event::ActionProposed { turn: turn(), request: request("a2") });
        app.apply(Event::ActionFinished { turn: turn(), observation: Observation::denied(ActionId::new("a2"), "no") });
        assert_eq!(app.activity[1].state, ActivityState::Denied);
    }

    #[test]
    fn source_status_updates_in_place() {
        let mut app = App::new(true);
        let source = |status| SourceInfo { id: SourceId::new("notes"), name: "Notes".into(), status };
        app.apply(Event::SourceStatus { source: source(SourceStatus::Missing) });
        app.apply(Event::SourceStatus { source: source(SourceStatus::Ready) });
        assert_eq!(app.sources, vec![source(SourceStatus::Ready)]);
    }

    #[test]
    fn usage_cost_adds_up() {
        let mut app = App::new(true);
        let usage = Usage { input_tokens: 1, output_tokens: 1, cost_micro_usd: Some(1_500) };
        let account = AccountRef::new("p", "a");
        app.apply(Event::Usage { account: account.clone(), usage });
        app.apply(Event::Usage { account, usage });
        assert_eq!(app.cost_micro_usd, 3_000);
    }

    #[test]
    fn ctrl_c_interrupts_a_busy_turn_and_twice_quits() {
        let mut app = App::new(true);
        app.apply(Event::TurnStarted { turn: turn() });
        assert_eq!(app.handle_key(ctrl_c()), vec![Effect::Send(Command::Interrupt)]);
        assert_eq!(app.handle_key(ctrl_c()), vec![Effect::Quit]);
    }

    #[test]
    fn ctrl_c_when_idle_only_warns_first() {
        let mut app = App::new(true);
        assert!(app.handle_key(ctrl_c()).is_empty());
        assert_eq!(app.notice.as_deref(), Some("press ctrl-c again to quit"));
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.handle_key(ctrl_c()).is_empty(), "another key resets the double press");
    }

    #[test]
    fn activity_selection_follows_the_newest_until_moved() {
        let mut app = App::new(true);
        app.focus = Focus::Activity;
        app.apply(Event::ActionProposed { turn: turn(), request: request("a1") });
        app.apply(Event::ActionProposed { turn: turn(), request: request("a2") });
        assert_eq!(app.selected(), Some(1));
        app.handle_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected(), Some(0));
        app.apply(Event::ActionProposed { turn: turn(), request: request("a3") });
        assert_eq!(app.selected(), Some(0), "a moved selection stays put");
        app.handle_key(key(KeyCode::End));
        app.apply(Event::ActionProposed { turn: turn(), request: request("a4") });
        assert_eq!(app.selected(), Some(3));
    }

    #[test]
    fn tab_cycles_focus() {
        let mut app = App::new(true);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Conversation);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Activity);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.focus, Focus::Conversation);
    }

    #[tokio::test]
    async fn y_and_n_answer_the_prompt_and_other_keys_wait() {
        use crate::ports::{TuiApprovals, UiEvent};
        use nixxa_contracts::ApprovalPort;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let approvals = std::sync::Arc::new(TuiApprovals::new(tx));
        let mut app = App::new(true);

        for (answer, expected) in [('y', Verdict::Allow), ('n', Verdict::Deny("denied by the user".into()))] {
            let asking = {
                let approvals = approvals.clone();
                tokio::spawn(async move { approvals.ask(&request("a1"), "why").await })
            };
            let Some(UiEvent::Approval(prompt)) = rx.recv().await else { panic!("expected a prompt") };
            app.ask(prompt);
            app.handle_key(key(KeyCode::Char('x')));
            assert!(app.approval().is_some(), "other keys do not answer");
            assert!(app.input.is_empty(), "and do not reach the input");
            app.handle_key(key(KeyCode::Char(answer)));
            assert!(app.approval().is_none());
            assert_eq!(asking.await.unwrap(), expected);
        }
    }
}
