//! TUI state: what is on screen and how keys change it. No terminal I/O here, so all of it is
//! unit-testable; `ui` draws it and `tui` drives it.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use nixxa_contracts::RedactionSource;
use nixxa_contracts::{
    AccountRef, ActionRequest, ApprovalMode, BrainInfo, Content, Event, Observation, Outcome, Role,
    SandboxCapabilities, SessionId, SourceInfo, Verdict,
};
use nixxa_controller::scrub::{Found, Scrubber};
use nixxa_controller::{Command, UserMessage};

use crate::ports::{AutoApprove, PendingApproval};
use nixxa_accounts::methods::openrouter::KEYS_PAGE;
use nixxa_brain::ModelInfo;
use nixxa_contracts::{AccountInfo, AuthMethod, NetworkAccess, NetworkPolicy, SandboxEvent, Secret};

use crate::network::{self, Mode, NetworkPanel, PanelKey};
use crate::notify::{Alert, Channels, Finished, Notifications};
use crate::place::{Place, PlacePicker};
use crate::sessions::{ListKey, SessionList};
use crate::setup::Setup;
use crate::terminal::{self, PaneKey, TerminalPane};
use crate::undo::{self, Checkpoints, UndoView, ViewKey};
use nixxa_contracts::TerminalSize;

/// How long a first ctrl-c waits for a second one to quit.
const QUIT_WINDOW: Duration = Duration::from_secs(2);
/// How long switching to Auto in This directory waits for the confirming second shift-tab.
pub(crate) const CONFIRM_AUTO_WINDOW: Duration = Duration::from_secs(5);

/// Which pane keys go to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Input,
    Conversation,
    Activity,
    /// The terminal pane (`ctrl+t`): typing into the shell, or its own keys after `ctrl+]`.
    Terminal,
}

impl Focus {
    /// The next pane `tab` goes to; the terminal only while it is shown.
    fn next(self, terminal: bool) -> Self {
        match self {
            Self::Input => Self::Conversation,
            Self::Conversation => Self::Activity,
            Self::Activity if terminal => Self::Terminal,
            Self::Activity | Self::Terminal => Self::Input,
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
    /// Approved by Auto mode rather than by the user.
    pub(crate) auto_approved: bool,
}

/// Something the activity pane shows between actions: not an action of the agent's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Happening {
    /// A background job of the agent's ended.
    JobEnded { job: nixxa_contracts::JobId, command: String, exit_code: Option<i32>, status: String },
    /// The controller started a turn for these ended jobs.
    WakeUp { jobs: Vec<nixxa_contracts::JobId> },
    /// No checkpoint could be taken: what it was for can't be undone (D-072).
    NotUndoable { label: String, reason: String },
    /// The user's undo put the workspace back (D-073).
    Restored { label: String, paths: usize },
    /// A secret was replaced before it reached the agent, or sent as it is at the user's choice
    /// (D-077).
    Redacted { kind: String, from: String, placeholder: String, sent: bool },
}

/// Where a redacted secret was, in words.
fn redaction_from(source: &RedactionSource) -> String {
    match source {
        RedactionSource::UserMessage => "your message".into(),
        RedactionSource::TerminalShare => "the terminal lines in your message".into(),
        RedactionSource::FileRead { path } => format!("`{path}`"),
        RedactionSource::FileEdit { path } => format!("the edit of `{path}`"),
        RedactionSource::CommandOutput { action } => format!("the output of command {action}"),
        RedactionSource::JobOutput { job } => format!("the output of job {job}"),
        RedactionSource::JobNote => "a note that a job ended".into(),
        RedactionSource::NetworkRequest { action } => format!("a URL that command {action} requested"),
        RedactionSource::Knowledge { source } => format!("what {source} returned"),
        RedactionSource::ActionResult { action } => format!("the result of action {action}"),
        RedactionSource::Notice => "a note from Nixxa".into(),
        RedactionSource::ResumedHistory => "the resumed session's history".into(),
    }
}

/// Whether a character key types its character. A ctrl shortcut doesn't, except ctrl+alt:
/// on Windows, AltGr arrives as ctrl+alt and is how many layouts type `@`, `€` or `{`.
pub(crate) fn types_text(key: &KeyEvent) -> bool {
    !key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT)
}

/// What the UI loop must do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    Send(Command),
    Quit,
    /// Check this key with the provider, then store it.
    SaveKey(Secret),
    /// Ask the local server at this URL for its models.
    ListModels(String),
    /// Check the local server offers this model, then store the account.
    SaveLocal {
        url: String,
        model: String,
    },
    /// Start an OpenRouter sign-in (the user confirmed the privacy notice).
    StartSignIn,
    /// Stop the OpenRouter sign-in, and forget its key if it already arrived.
    CancelSignIn,
    /// Check the signed-in OpenRouter key and this model, then store the account.
    SaveOpenRouter {
        model: String,
    },
    /// The user chose where the agent works (the start-up question).
    ChoosePlace(Place),
    /// Read the stored sessions for the session list.
    ListSessions,
    /// Search stored transcripts for the session list.
    SearchSessions(String),
    /// Apply this network policy to the sandbox (the network settings, `ctrl+n`).
    SetNetwork(nixxa_contracts::NetworkPolicy),
    /// End the running session (it is already saved) and continue this stored one.
    Resume(SessionId),
    /// Open the user's terminal in the sandbox, at this size (`ctrl+t`).
    OpenTerminal(TerminalSize),
    /// Bytes for the terminal's shell: what the user typed or pasted there. Never approved,
    /// never sent to the agent.
    TerminalInput(Vec<u8>),
    /// Ask the sandbox what changed since this checkpoint, for the undo view.
    UndoDiff(nixxa_contracts::CheckpointId),
}

/// Everything the TUI shows.
pub(crate) struct App {
    pub(crate) color: bool,
    pub(crate) session: Option<SessionId>,
    pub(crate) account: Option<AccountRef>,
    pub(crate) brain: Option<BrainInfo>,
    pub(crate) sandbox: Option<SandboxCapabilities>,
    pub(crate) sources: Vec<SourceInfo>,
    /// Ask or Auto (D-052); shift-tab switches.
    pub(crate) mode: ApprovalMode,
    /// Shared with the approval port, which approves by itself while it is on.
    pub(crate) auto: AutoApprove,
    /// Until when a second shift-tab confirms Auto in This directory.
    confirm_auto_until: Option<Instant>,
    /// Whether the agent works in This directory (not isolated): Auto needs a confirmation.
    pub(crate) here: bool,
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
    /// The setup screen, when open.
    pub(crate) setup: Option<Setup>,
    /// Whether the setup form can be opened (not with `--fake`).
    pub(crate) setup_available: bool,
    /// The start-up question (sandbox or this directory), until it is answered. It sits above
    /// everything else, the setup screen included.
    pub(crate) place: Option<PlacePicker>,
    /// The directory Nixxa was started in, shown by the start-up question.
    pub(crate) directory: String,
    /// The same directory, to tell its sessions from the others in the session list.
    pub(crate) root: PathBuf,
    /// The session list (`ctrl-s`), when open.
    pub(crate) sessions: Option<SessionList>,
    /// The network settings (`ctrl+n`) and the recent requests.
    pub(crate) network: NetworkPanel,
    /// The user's terminal (`ctrl+t`).
    pub(crate) terminal: TerminalPane,
    /// Job ends and wake-ups, each with how many actions came before it (where it goes in the
    /// activity pane).
    pub(crate) happenings: Vec<(usize, Happening)>,
    /// The badge, presence and alerts (D-069).
    pub(crate) notifications: Notifications,
    /// Whether the running turn was started by the controller for ended jobs.
    woken: bool,
    /// The checkpoints of this run (D-072).
    pub(crate) checkpoints: Checkpoints,
    /// The undo view (`/undo`, `ctrl+z`), when open.
    pub(crate) undo: Option<UndoView>,
    /// The session's secret scrubber, the controller's own, to show what a message will have
    /// replaced before it is sent (D-077).
    pub(crate) scrubber: Option<Arc<Scrubber>>,
    /// Placeholders of secrets in the message that the user chose to send as they are, one by
    /// one, for this message only.
    pub(crate) send_anyway: Vec<String>,
    /// Whether the message holds lines shared from the terminal.
    pub(crate) shared: bool,
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
            auto: AutoApprove::default(),
            confirm_auto_until: None,
            here: false,
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
            setup: None,
            setup_available: false,
            place: None,
            directory: String::new(),
            root: PathBuf::new(),
            sessions: None,
            network: NetworkPanel::default(),
            terminal: TerminalPane::default(),
            happenings: Vec::new(),
            notifications: Notifications::new(Channels::default(), Instant::now()),
            woken: false,
            checkpoints: Checkpoints::default(),
            undo: None,
            scrubber: None,
            send_anyway: Vec::new(),
            shared: false,
        }
    }

    /// The secrets in the message being written, with what will replace each (D-077).
    pub(crate) fn input_secrets(&self) -> Vec<Found> {
        self.scrubber.as_ref().map_or_else(Vec::new, |scrubber| scrubber.find(&self.input))
    }

    /// The host terminal window gained or lost focus.
    pub(crate) fn focus_changed(&mut self, focused: bool) {
        self.notifications.focus(focused);
    }

    /// Bells and desktop notifications to deliver now.
    pub(crate) fn take_alerts(&mut self) -> Vec<Alert> {
        self.notifications.take_alerts()
    }

    /// Something finished: the badge counts it; if the user is away, it alerts them.
    fn finished(&mut self, what: Finished) {
        let in_terminal = self.typing_in_terminal();
        self.notifications.finished(&what, Instant::now(), in_terminal);
    }

    /// Asks where the agent should work. The places in `blocked` are shown with the reason and
    /// can't be picked.
    pub(crate) fn ask_place(&mut self, blocked: Vec<(Place, String)>) {
        self.place = Some(PlacePicker::new(blocked));
    }

    /// Whether a second shift-tab now would switch to Auto in This directory.
    pub(crate) fn confirming_auto(&self, now: Instant) -> bool {
        self.confirm_auto_until.is_some_and(|until| now < until)
    }

    /// Shift-tab: Ask ↔ Auto. In This directory, switching to Auto takes a second press within
    /// [`CONFIRM_AUTO_WINDOW`]. Switching to Auto also approves any prompt already open.
    pub(crate) fn toggle_mode(&mut self, now: Instant) {
        if self.mode == ApprovalMode::Auto {
            self.set_auto(false);
            self.notice = Some("Ask mode: every action asks first.".into());
            return;
        }
        if self.here && !self.confirming_auto(now) {
            self.confirm_auto_until = Some(now + CONFIRM_AUTO_WINDOW);
            self.notice = Some("Auto in your real files, without isolation? Press shift-tab again to confirm.".into());
            return;
        }
        self.set_auto(true);
        self.notice = Some("Auto mode: actions run without asking. shift-tab goes back to Ask.".into());
        while let Some(prompt) = self.approvals.pop_front() {
            let id = prompt.request.id.clone();
            prompt.answer(Verdict::Allow);
            self.auto_approved(&id);
        }
    }

    fn set_auto(&mut self, on: bool) {
        self.mode = if on { ApprovalMode::Auto } else { ApprovalMode::Ask };
        self.auto.set(on);
        self.confirm_auto_until = None;
    }

    /// Marks an action as approved by Auto mode.
    pub(crate) fn auto_approved(&mut self, id: &nixxa_contracts::ActionId) {
        if let Some(line) = self.activity.iter_mut().rev().find(|a| &a.request.id == id) {
            line.auto_approved = true;
        }
    }

    /// Opens the setup screen at its first step.
    pub(crate) fn open_setup(&mut self, first_run: bool) {
        self.setup = Some(Setup::new(first_run));
    }

    /// An account was set up and saved, or why not.
    pub(crate) fn account_saved(&mut self, result: &Result<AccountInfo, String>) {
        match result {
            Ok(info) => {
                self.setup = None;
                match info.method {
                    AuthMethod::SignIn => {
                        let hint = info.hint.as_ref().map(|h| format!(" (…{h})")).unwrap_or_default();
                        self.note(format!(
                            "Signed in: the OpenRouter key{hint} is saved for {} in the OS keychain. \
                             Nixxa can't revoke it for you; to revoke it, delete it at {KEYS_PAGE}.",
                            info.account
                        ));
                    }
                    AuthMethod::Local => {
                        let url = info.hint.as_deref().unwrap_or("the local server");
                        self.note(format!("Saved {} ({url}).", info.account));
                    }
                    _ => {
                        let hint = info.hint.as_ref().map(|h| format!(" (…{h})")).unwrap_or_default();
                        self.note(format!("Saved the API key{hint} for {} in the OS keychain.", info.account));
                    }
                }
            }
            Err(message) => {
                if let Some(setup) = self.setup.as_mut() {
                    setup.failed(message);
                }
            }
        }
    }

    /// The local server's models arrived (or why not).
    pub(crate) fn models_listed(&mut self, result: Result<Vec<ModelInfo>, String>) {
        if let Some(setup) = self.setup.as_mut() {
            setup.models_listed(result);
        }
    }

    /// Pasted text (bracketed paste): into the setup screen when it is open, the terminal while
    /// the user types in it, else the input.
    pub(crate) fn paste(&mut self, text: &str) -> Vec<Effect> {
        if self.place.is_some() {
            return Vec::new();
        }
        if let Some(setup) = self.setup.as_mut() {
            setup.paste(text);
        } else if self.typing_in_terminal() {
            if self.terminal.state == terminal::State::Open {
                return vec![Effect::TerminalInput(self.terminal.paste_bytes(text))];
            }
        } else if self.approvals.is_empty() && self.focus == Focus::Input {
            self.input.push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
        }
        Vec::new()
    }

    /// Whether keys go to the terminal's shell.
    pub(crate) fn typing_in_terminal(&self) -> bool {
        self.focus == Focus::Terminal && self.terminal.shown && self.terminal.typing
    }

    /// `ctrl+t`: shows the terminal (opening one if none is running) and types into it, or puts
    /// the details pane back. The terminal keeps running while hidden.
    fn toggle_terminal(&mut self) -> Vec<Effect> {
        if self.terminal.shown {
            self.terminal.shown = false;
            self.terminal.share = None;
            if self.focus == Focus::Terminal {
                self.focus = Focus::Input;
            }
            // The next ctrl+t opens a new one.
            if matches!(self.terminal.state, terminal::State::Ended(_)) {
                self.terminal.state = terminal::State::Closed;
            }
            return Vec::new();
        }
        self.terminal.shown = true;
        self.focus = Focus::Terminal;
        self.terminal.scroll_to_bottom();
        if self.terminal.is_live() {
            self.terminal.typing = self.terminal.state == terminal::State::Open;
            return Vec::new();
        }
        self.terminal.typing = true;
        vec![Effect::OpenTerminal(self.terminal.opening())]
    }

    /// The terminal opened, or why not (said, never worked around: D-051).
    pub(crate) fn terminal_opened(&mut self, result: Result<(), String>) -> Vec<Effect> {
        match result {
            Ok(()) => {
                self.terminal.opened();
                let place = if self.here { "in this directory" } else { "in the sandbox" };
                vec![Effect::Send(Command::Note(format!(
                    "The user opened a terminal {place}: their own shell, in the same place as your commands. \
                     You don't see it unless they share part of it."
                )))]
            }
            Err(why) => {
                self.terminal.ended(format!("it couldn't open: {why}"));
                self.notice = Some(format!("The terminal couldn't open: {why}"));
                Vec::new()
            }
        }
    }

    /// Output from the terminal's shell.
    pub(crate) fn terminal_output(&mut self, bytes: &[u8]) {
        self.terminal.feed(bytes);
    }

    /// The terminal ended. Recorded in the session (that it closed, never what was in it).
    pub(crate) fn terminal_closed(&mut self, reason: String) -> Vec<Effect> {
        let was_open = self.terminal.state == terminal::State::Open;
        self.terminal.ended(reason.clone());
        if was_open {
            self.notice = Some(format!("The terminal closed: {reason}. ctrl+t hides it; then ctrl+t opens a new one."));
            vec![Effect::Send(Command::Note(format!("The user's terminal closed ({reason}).")))]
        } else {
            Vec::new()
        }
    }

    /// The terminal pane's own keys (focused, not typing).
    fn terminal_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        match self.terminal.pane_key(key) {
            PaneKey::Send(bytes) => vec![Effect::TerminalInput(bytes)],
            PaneKey::Share(text) => {
                if !self.input.is_empty() && !self.input.ends_with('\n') {
                    self.input.push('\n');
                }
                self.input.push_str(&text);
                self.shared = true;
                self.focus = Focus::Input;
                self.notice =
                    Some("The terminal's lines are in your message: edit it as you like; enter sends it.".into());
                Vec::new()
            }
            PaneKey::Nothing | PaneKey::NotMine => Vec::new(),
        }
    }

    /// The approval prompt on screen, if any.
    pub(crate) fn approval(&mut self) -> Option<&PendingApproval> {
        self.approvals.retain(|prompt| !prompt.is_abandoned());
        self.approvals.front()
    }

    /// The stored sessions arrived for the session list.
    pub(crate) fn sessions_listed(&mut self, result: Result<Vec<nixxa_contracts::SessionInfo>, String>) {
        if let Some(list) = self.sessions.as_mut() {
            list.listed(result);
        }
    }

    /// Transcript matches arrived for the session list.
    pub(crate) fn sessions_found(&mut self, query: &str, result: Result<Vec<nixxa_contracts::SearchHit>, String>) {
        if let Some(list) = self.sessions.as_mut() {
            list.found(query, result);
        }
    }

    /// Another session is about to be shown: forget everything about the one on screen. Notes
    /// from the harness go too; the resumed session's own history replaces them.
    pub(crate) fn start_over(&mut self) {
        self.session = None;
        self.conversation.clear();
        self.streaming = None;
        self.queued.clear();
        self.activity.clear();
        self.happenings.clear();
        self.woken = false;
        self.selected = None;
        self.scroll_back = 0;
        self.approvals.clear();
        self.busy = false;
        self.ended = false;
    }

    /// Adds a note from the harness itself to the conversation.
    pub(crate) fn note(&mut self, text: impl Into<String>) {
        self.conversation.push(Entry { speaker: Speaker::System, text: text.into() });
    }

    /// Queues an approval prompt.
    pub(crate) fn ask(&mut self, prompt: PendingApproval) {
        self.approvals.push_back(prompt);
    }

    /// Whether the user chose an activity line (else the pane follows the newest).
    pub(crate) fn selected_explicitly(&self) -> bool {
        self.selected.is_some()
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
                self.close_undo("a turn started while /undo was open: open it again once it is over");
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
                self.activity.push(Activity {
                    request,
                    state: ActivityState::Proposed,
                    observation: None,
                    auto_approved: false,
                });
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
            Event::Error { turn, message } => {
                // A turn that failed (or was refused) leaves no half-written answer behind.
                if turn.is_some() {
                    self.streaming = None;
                }
                self.conversation.push(Entry { speaker: Speaker::Error, text: message });
            }
            Event::JobFinished { job, command, exit_code, status, .. } => {
                let what = Finished::Job { job: job.to_string(), exit_code, command: command.clone() };
                self.happenings.push((self.activity.len(), Happening::JobEnded { job, command, exit_code, status }));
                self.finished(what);
            }
            Event::WakeUp { jobs, .. } => {
                self.woken = true;
                self.happenings.push((self.activity.len(), Happening::WakeUp { jobs }));
            }
            Event::TurnDone { .. } => {
                let woken = std::mem::take(&mut self.woken);
                self.finished(Finished::Turn { woken });
                self.busy = false;
                // Text streamed by a turn that never finished its message (interrupt, error).
                if let Some(text) = self.streaming.take().filter(|t| !t.is_empty()) {
                    self.conversation.push(Entry { speaker: Speaker::Agent, text });
                }
            }
            Event::Checkpoint { turn, checkpoint, before } => {
                let label = self.checkpoint_label(&turn, before.as_ref());
                self.checkpoints.taken(checkpoint, label);
                self.close_undo("the workspace changed while /undo was open: open it again");
            }
            Event::NotUndoable { turn, before, reason } => {
                let label = self.checkpoint_label(&turn, before.as_ref());
                self.checkpoints.missing(label.clone(), reason.clone());
                self.happenings.push((self.activity.len(), Happening::NotUndoable { label, reason }));
            }
            Event::Restored { checkpoint, undone } => {
                let label = self
                    .checkpoints
                    .choices()
                    .into_iter()
                    .find(|choice| choice.id == checkpoint)
                    .map_or_else(|| "at a checkpoint".to_string(), |choice| choice.label);
                let paths = undone.changes.len();
                let text = format!(
                    "Undone: the workspace is back as it was {label}. {paths} path{} went back; the agent is told.",
                    if paths == 1 { "" } else { "s" }
                );
                self.conversation.push(Entry { speaker: Speaker::System, text });
                self.happenings.push((self.activity.len(), Happening::Restored { label, paths }));
                self.undo = None;
            }
            Event::SecretRedacted { kind, source, placeholder, sent, .. } => {
                let from = redaction_from(&source);
                let text = if sent {
                    format!(
                        "Sent a {kind} in {from} as it is, as you chose: the agent has the value; the session's record \
                         keeps {placeholder}."
                    )
                } else {
                    format!("Redacted a {kind} in {from}: the agent sees {placeholder} instead.")
                };
                self.conversation.push(Entry { speaker: Speaker::System, text });
                self.happenings.push((self.activity.len(), Happening::Redacted { kind, from, placeholder, sent }));
            }
            Event::SessionEnded { .. } => self.ended = true,
        }
    }

    /// What a checkpoint comes before, in words: an action, the start of a turn, or an undo.
    fn checkpoint_label(&self, turn: &nixxa_contracts::TurnId, before: Option<&nixxa_contracts::ActionId>) -> String {
        if let Some(id) = before {
            return match self.activity.iter().rev().find(|a| &a.request.id == id) {
                Some(activity) => {
                    let (kind, target) = crate::ui::describe(&activity.request);
                    format!("before `{kind} {}`", crate::ui::truncate(&target, 60))
                }
                None => format!("before action {id}"),
            };
        }
        if turn.as_str().starts_with("undo-") {
            return "before the last undo".to_string();
        }
        if self.woken {
            return "at the start of a turn the agent was woken for".to_string();
        }
        match self.conversation.iter().rev().find(|entry| entry.speaker == Speaker::User) {
            Some(entry) => format!("at the start of the turn for \"{}\"", crate::ui::truncate(&entry.text, 40)),
            None => "at the start of a turn".to_string(),
        }
    }

    /// Opens the undo view, or says why it can't open.
    fn open_undo(&mut self) -> Vec<Effect> {
        if self.busy {
            self.notice = Some("/undo works between turns: wait for this one to finish, or stop it with ctrl-c".into());
            return Vec::new();
        }
        if !self.sandbox.as_ref().is_some_and(|caps| caps.checkpoints) {
            self.notice = Some("this place keeps no checkpoints, so there is nothing to undo".into());
            return Vec::new();
        }
        let choices = self.checkpoints.choices();
        if choices.is_empty() {
            self.notice = Some("nothing to undo yet: no checkpoint has been taken".into());
            return Vec::new();
        }
        let view = UndoView::new(choices, self.checkpoints.newest().cloned());
        let first = view.chosen().id.clone();
        self.undo = Some(view);
        vec![Effect::UndoDiff(first)]
    }

    fn close_undo(&mut self, why: &str) {
        if self.undo.take().is_some() {
            self.notice = Some(why.into());
        }
    }

    fn undo_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(view) = self.undo.as_mut() else { return Vec::new() };
        match view.key(key) {
            ViewKey::Nothing => Vec::new(),
            ViewKey::Diff(id) => vec![Effect::UndoDiff(id)],
            ViewKey::Close => {
                self.undo = None;
                Vec::new()
            }
            ViewKey::Undo { to, seen } => {
                self.undo = None;
                self.notice = Some("undoing…".into());
                vec![Effect::Send(Command::Undo { to, seen })]
            }
        }
    }

    /// The diff the undo view asked for.
    pub(crate) fn undo_diff(
        &mut self,
        id: &nixxa_contracts::CheckpointId,
        result: Result<nixxa_contracts::WorkspaceDiff, String>,
    ) {
        if let Some(view) = self.undo.as_mut() {
            view.diff_ready(id, result);
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
        self.notifications.key(Instant::now());
        // Typing in the terminal: every key is the shell's (ctrl+c and ctrl+t too) but ctrl+],
        // which leaves it. Nothing typed there is approved or seen by the agent.
        if self.typing_in_terminal() && self.place.is_none() && self.setup.is_none() {
            self.last_ctrl_c = None;
            if terminal::is_leave(key) {
                self.terminal.typing = false;
                return Vec::new();
            }
            if self.terminal.state != terminal::State::Open {
                return Vec::new();
            }
            return self.terminal.key_bytes(key).map(|bytes| vec![Effect::TerminalInput(bytes)]).unwrap_or_default();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return self.ctrl_c();
        }
        self.last_ctrl_c = None;
        if key.code != KeyCode::BackTab {
            self.confirm_auto_until = None;
        }
        if let Some(picker) = self.place.as_mut() {
            let chosen = picker.key(key);
            if chosen.is_some() {
                self.place = None;
            }
            return chosen.into_iter().collect();
        }
        if self.setup.is_some() {
            return self.setup_key(key);
        }
        if self.sessions.is_some() {
            return self.sessions_key(key);
        }
        if self.network.open {
            return self.network_key(key);
        }
        if self.undo.is_some() {
            return self.undo_key(key);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('z') {
            return self.open_undo();
        }
        // Over an approval prompt too: allowing a host may be the answer to it.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            self.network.open = true;
            self.network.error = None;
            return Vec::new();
        }
        // Over an approval prompt too: the terminal may be where the user checks before
        // answering. Its keys never answer the prompt.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            return self.toggle_terminal();
        }
        if self.approval().is_some() {
            if key.code == KeyCode::BackTab {
                self.toggle_mode(Instant::now());
            } else {
                self.answer_approval(key.code);
            }
            return Vec::new();
        }
        self.notice = None;
        if self.setup_available && key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            self.open_setup(false);
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.sessions = Some(SessionList::new(&self.root, self.session.clone()));
            return vec![Effect::ListSessions];
        }
        match key.code {
            KeyCode::Tab if self.terminal.share.is_none() => {
                self.focus = self.focus.next(self.terminal.shown);
                if self.focus == Focus::Terminal {
                    self.terminal.typing = false;
                }
            }
            KeyCode::BackTab => self.toggle_mode(Instant::now()),
            _ => match self.focus {
                Focus::Input => return self.edit_input(key),
                Focus::Conversation => self.scroll_conversation(key.code),
                Focus::Activity => self.move_selection(key.code),
                Focus::Terminal => return self.terminal_key(key),
            },
        }
        Vec::new()
    }

    fn setup_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(setup) = self.setup.as_mut() else { return Vec::new() };
        let outcome = setup.key(key);
        if outcome.close {
            self.setup = None;
        }
        outcome.effects
    }

    fn sessions_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let Some(list) = self.sessions.as_mut() else { return Vec::new() };
        match list.key(key) {
            ListKey::Nothing => Vec::new(),
            ListKey::Close => {
                self.sessions = None;
                Vec::new()
            }
            ListKey::AlreadyOpen => {
                self.sessions = None;
                self.notice = Some("That session is the one open.".into());
                Vec::new()
            }
            ListKey::Search(query) => vec![Effect::SearchSessions(query)],
            ListKey::Resume(id) => {
                self.sessions = None;
                vec![Effect::Resume(id)]
            }
        }
    }

    fn network_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        let access = self.sandbox.as_ref().map(|caps| caps.network.clone());
        match self.network.key(key, access.as_ref()) {
            PanelKey::Apply(policy) => vec![Effect::SetNetwork(policy)],
            PanelKey::Nothing | PanelKey::Close => Vec::new(),
        }
    }

    /// The sandbox applied a network policy, or couldn't. A change is recorded in the session.
    pub(crate) fn network_set(&mut self, result: Result<NetworkPolicy, String>) -> Vec<Effect> {
        match result {
            Ok(policy) => {
                if let Some(caps) = self.sandbox.as_mut() {
                    caps.network = NetworkAccess::Controlled(policy.clone());
                }
                vec![Effect::Send(Command::Note(network::note(&policy)))]
            }
            Err(why) => {
                // The panel shows what was asked for; put it back to what the sandbox applies.
                if let Some(NetworkAccess::Controlled(policy)) = self.sandbox.as_ref().map(|caps| &caps.network) {
                    self.network.mode = match policy {
                        NetworkPolicy::Off => Mode::Off,
                        NetworkPolicy::Allow(_) => Mode::Allow,
                        NetworkPolicy::Open => Mode::Open,
                    };
                }
                self.network.error = Some(format!("the network wasn't changed: {why}"));
                Vec::new()
            }
        }
    }

    /// Something happened in the sandbox.
    pub(crate) fn sandbox_event(&mut self, event: SandboxEvent) {
        match event {
            SandboxEvent::NetRequest(request) => self.network.record(request),
            // Job results are shown from Stage 6 PR D.
            SandboxEvent::JobFinished { .. } => {}
        }
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
                let secrets = self.input_secrets();
                let text = std::mem::take(&mut self.input);
                let send_anyway: Vec<String> = std::mem::take(&mut self.send_anyway)
                    .into_iter()
                    .filter(|placeholder| secrets.iter().any(|found| &found.placeholder == placeholder))
                    .collect();
                let from_terminal = std::mem::take(&mut self.shared);
                if text.trim() == undo::COMMAND {
                    return self.open_undo();
                }
                if !text.trim().is_empty() {
                    if self.busy {
                        // The controller runs it after the current turn; show it until then, as
                        // the controller will record it.
                        let shown = self.scrubber.as_ref().map_or_else(|| text.clone(), |s| s.scrub(&text).text);
                        self.queued.push_back(shown);
                    }
                    return vec![Effect::Send(Command::UserMessage(UserMessage { text, send_anyway, from_terminal }))];
                }
            }
            // alt+1…9: send that secret as it is, or not, after all (D-077).
            KeyCode::Char(digit @ '1'..='9')
                if key.modifiers.contains(KeyModifiers::ALT) && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                let secrets = self.input_secrets();
                let n = digit as usize - '1' as usize;
                match secrets.get(n) {
                    Some(found) => {
                        if let Some(at) = self.send_anyway.iter().position(|p| *p == found.placeholder) {
                            self.send_anyway.remove(at);
                        } else {
                            self.send_anyway.push(found.placeholder.clone());
                        }
                    }
                    None => self.input.push(digit),
                }
            }
            KeyCode::Char(c) if types_text(&key) => self.input.push(c),
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
    use crate::setup::{KeyForm, Step};

    fn key_form(app: &App) -> &KeyForm {
        match &app.setup.as_ref().expect("setup is open").step {
            Step::Key(form) => form,
            _ => panic!("not on the key form"),
        }
    }

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
                action: SandboxAction::Exec { command: "ls".into(), cwd: None, timeout_ms: None, background: false },
            },
            reason: None,
        }
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
    }

    const KEY: &str = "sk-ant-api03-SECRETSECRET-wxyz";

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn input(effects: &[Effect]) -> Vec<u8> {
        effects
            .iter()
            .flat_map(|effect| match effect {
                Effect::TerminalInput(bytes) => bytes.clone(),
                other => panic!("only terminal input expected, got {other:?}"),
            })
            .collect()
    }

    /// An app with an open terminal the user is typing in.
    fn typing() -> App {
        let mut app = App::new(true);
        let opened = app.handle_key(ctrl('t'));
        assert!(matches!(opened.as_slice(), [Effect::OpenTerminal(_)]), "{opened:?}");
        let noted = app.terminal_opened(Ok(()));
        assert!(
            matches!(noted.as_slice(), [Effect::Send(Command::Note(note))] if note.contains("opened a terminal")),
            "{noted:?}"
        );
        app
    }

    fn job_finished(job: &str, exit_code: Option<i32>, command: &str) -> Event {
        Event::JobFinished {
            job: nixxa_contracts::JobId::new(job),
            command: command.into(),
            exit_code,
            duration_ms: 3100,
            status: format!("job {job} exited after 3.1 s"),
        }
    }

    fn delivered(app: &mut App) -> crate::notify::tests::FakeNotifier {
        let mut fake = crate::notify::tests::FakeNotifier::default();
        crate::notify::deliver(app.take_alerts(), &mut fake);
        fake
    }

    #[test]
    fn a_job_ending_while_the_user_is_here_only_counts_on_the_badge() {
        let mut app = App::new(true);
        app.apply(job_finished("j1", Some(0), "cargo build"));
        let fake = delivered(&mut app);
        assert_eq!((fake.bells, fake.desktop.len()), (0, 0), "present: no bell, no desktop");
        assert_eq!(app.notifications.unseen, 1);
        assert!(matches!(&app.happenings[..], [(0, Happening::JobEnded { exit_code: Some(0), .. })]));
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.notifications.unseen, 0, "seen once the user presses a key");
    }

    #[test]
    fn away_the_channels_fire_and_a_wake_up_turn_says_so() {
        let mut app = App::new(true);
        app.focus_changed(false);
        app.apply(job_finished("j1", Some(2), "cargo test \u{1b}]0;x\u{7}"));
        let fake = delivered(&mut app);
        assert_eq!(
            fake.desktop,
            vec![("Nixxa".to_string(), "job j1 failed (exit 2): cargo test ^[]0;x^G".to_string())]
        );
        assert_eq!(fake.bells, 1);
        app.apply(Event::WakeUp { turn: turn(), jobs: vec![nixxa_contracts::JobId::new("j1")] });
        app.apply(Event::TurnStarted { turn: turn() });
        app.apply(Event::TurnDone { turn: turn() });
        let fake = delivered(&mut app);
        assert_eq!(fake.desktop[0].1, "the agent finished the turn a job woke it for");
        app.apply(Event::TurnStarted { turn: turn() });
        app.apply(Event::TurnDone { turn: turn() });
        assert_eq!(delivered(&mut app).desktop[0].1, "the agent finished its turn", "only that turn was a wake-up");
        assert_eq!(app.notifications.unseen, 3);
        assert!(matches!(app.happenings[1].1, Happening::WakeUp { .. }));
    }

    #[test]
    fn typing_in_the_terminal_counts_as_away() {
        let mut app = typing();
        app.apply(job_finished("j1", Some(0), "cargo build"));
        assert_eq!(delivered(&mut app).desktop.len(), 1);
        app.handle_key(ctrl(']'));
        app.apply(job_finished("j2", Some(0), "cargo build"));
        assert_eq!(delivered(&mut app).desktop.len(), 0, "back in the app: present");
    }

    #[test]
    fn in_the_terminal_every_key_goes_to_the_shell_but_ctrl_bracket() {
        let mut app = App::new(true);
        assert_eq!(app.handle_key(ctrl('t')), vec![Effect::OpenTerminal(TerminalSize { cols: 80, rows: 24 })]);
        assert_eq!(app.focus, Focus::Terminal);
        assert!(app.handle_key(key(KeyCode::Char('a'))).is_empty(), "nothing goes anywhere before it is open");
        app.terminal_opened(Ok(()));
        let keys = [
            key(KeyCode::Char('l')),
            ctrl('c'),
            ctrl('c'),
            ctrl('t'),
            ctrl('n'),
            ctrl('s'),
            ctrl('a'),
            key(KeyCode::Tab),
            key(KeyCode::BackTab),
            key(KeyCode::Esc),
            key(KeyCode::Enter),
        ];
        let mut sent = Vec::new();
        for k in keys {
            sent.extend(input(&app.handle_key(k)));
        }
        assert_eq!(sent, b"l\x03\x03\x14\x0e\x13\x01\t\x1b[Z\x1b\r");
        assert!(!app.network.open && app.sessions.is_none() && app.setup.is_none(), "no shortcut fired");
        assert_eq!(app.mode, ApprovalMode::Ask, "shift-tab went to the shell");
        assert_eq!(app.focus, Focus::Terminal);
        assert!(app.input.is_empty() && app.conversation.is_empty(), "nothing typed reached the conversation");
        assert_eq!(input(&app.paste("ls\nls -a")), b"ls\rls -a");

        // ctrl+] leaves; the pane's own keys and the app's shortcuts work again.
        assert!(app.handle_key(ctrl(']')).is_empty());
        assert!(!app.terminal.typing);
        assert!(app.handle_key(key(KeyCode::Char('x'))).is_empty());
        assert!(app.paste("not for the shell").is_empty());
        assert!(app.handle_key(ctrl('t')).is_empty(), "hides it; it keeps running");
        assert!(!app.terminal.shown && app.focus == Focus::Input);
        assert!(app.handle_key(ctrl('t')).is_empty(), "shows it again: no second terminal");
        assert!(app.terminal.typing && app.focus == Focus::Terminal);
        // ctrl+] twice sends ctrl+] (vim's tag jump).
        app.handle_key(ctrl(']'));
        assert_eq!(input(&app.handle_key(ctrl(']'))), vec![0x1d]);
        assert!(app.terminal.typing);
    }

    #[test]
    fn typing_in_the_terminal_never_answers_an_approval() {
        let mut app = typing();
        let (prompt, mut answer) = PendingApproval::for_test(request("a1"), "exec asks first");
        app.ask(prompt);
        for c in ['y', 'Y', 'n'] {
            assert_eq!(input(&app.handle_key(key(KeyCode::Char(c)))), c.to_string().into_bytes());
        }
        assert!(answer.try_recv().is_err(), "the prompt is still waiting");
        app.handle_key(ctrl(']'));
        app.handle_key(key(KeyCode::Char('y')));
        assert_eq!(answer.try_recv(), Ok(Verdict::Allow), "answered once the user left the terminal");
    }

    #[test]
    fn tab_reaches_the_terminal_only_while_it_is_shown() {
        let mut app = App::new(true);
        for want in [Focus::Conversation, Focus::Activity, Focus::Input] {
            app.handle_key(key(KeyCode::Tab));
            assert_eq!(app.focus, want);
        }
        let mut app = typing();
        app.handle_key(ctrl(']'));
        for want in [Focus::Input, Focus::Conversation, Focus::Activity, Focus::Terminal] {
            app.handle_key(key(KeyCode::Tab));
            assert_eq!(app.focus, want);
        }
        assert!(!app.terminal.typing, "tab into the pane, not into the shell");
        app.handle_key(key(KeyCode::Char('i')));
        assert!(app.typing_in_terminal());
    }

    #[test]
    fn the_terminal_ending_is_recorded_and_ctrl_t_then_opens_a_new_one() {
        let mut app = typing();
        let closed = app.terminal_closed("the shell exited".into());
        assert_eq!(closed, vec![Effect::Send(Command::Note("The user's terminal closed (the shell exited).".into()))]);
        assert!(!app.terminal.typing);
        assert!(app.handle_key(key(KeyCode::Char('l'))).is_empty(), "nothing to type into");
        assert!(app.handle_key(ctrl('t')).is_empty(), "hides the closed one");
        assert!(matches!(app.handle_key(ctrl('t')).as_slice(), [Effect::OpenTerminal(_)]), "then opens a new one");
    }

    #[test]
    fn a_terminal_that_cannot_open_says_why() {
        let mut app = App::new(true);
        app.handle_key(ctrl('t'));
        assert!(app.terminal_opened(Err("this sandbox has no terminal".into())).is_empty());
        assert!(app.notice.as_deref().is_some_and(|n| n.contains("this sandbox has no terminal")), "{:?}", app.notice);
        assert!(matches!(&app.terminal.state, crate::terminal::State::Ended(why) if why.contains("no terminal")));
        assert!(app.terminal_closed("late".into()).is_empty(), "nothing to record: it never opened");
    }

    #[test]
    fn sharing_puts_the_lines_in_the_message_and_sends_nothing() {
        let mut app = typing();
        app.terminal_output(b"$ cargo test\r\nerror: 1 failed\r\n$ ");
        app.input = "why does this fail?".into();
        app.handle_key(ctrl(']'));
        assert!(app.handle_key(key(KeyCode::Char('s'))).is_empty());
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty(), "nothing is sent");
        assert_eq!(app.input, "why does this fail?\nFrom my terminal:\n```\n$ cargo test\nerror: 1 failed\n```\n");
        assert_eq!(app.focus, Focus::Input, "the user edits it, then sends it");
    }

    #[test]
    fn the_setup_form_masks_all_but_the_last_four() {
        let mut app = App::new(true);
        app.open_setup(true);
        app.handle_key(key(KeyCode::Char('1')));
        type_text(&mut app, "sk-ant");
        assert_eq!(key_form(&app).masked(), "••••••", "short input shows no characters");
        type_text(&mut app, &KEY["sk-ant".len()..]);
        let masked = key_form(&app).masked();
        assert!(masked.ends_with("wxyz") && !masked.contains("SECRET") && !masked.contains("sk-ant"), "{masked}");
    }

    #[test]
    fn enter_checks_the_key_and_typing_waits_while_it_runs() {
        let mut app = App::new(true);
        app.open_setup(true);
        app.handle_key(key(KeyCode::Char('1')));
        type_text(&mut app, KEY);
        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(effects, vec![Effect::SaveKey(Secret::new(KEY))]);
        assert_eq!(key_form(&app).state, crate::setup::SetupState::Checking);
        type_text(&mut app, "zz");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty(), "one check at a time");
        assert!(key_form(&app).masked().ends_with("wxyz"), "typing is ignored while checking");
    }

    #[test]
    fn a_failed_check_is_shown_and_can_be_retried() {
        let mut app = App::new(true);
        app.open_setup(true);
        app.handle_key(key(KeyCode::Char('1')));
        type_text(&mut app, KEY);
        app.handle_key(key(KeyCode::Enter));
        app.account_saved(&Err("Anthropic rejected this key: invalid x-api-key".into()));
        assert_eq!(
            key_form(&app).state,
            crate::setup::SetupState::Failed("Anthropic rejected this key: invalid x-api-key".into())
        );
        app.handle_key(ctrl('u'));
        assert_eq!(key_form(&app).masked(), "");
        assert!(matches!(app.handle_key(key(KeyCode::Enter)).as_slice(), []), "an empty key isn't sent");
        assert_eq!(key_form(&app).state, crate::setup::SetupState::Failed("Paste a key first.".into()));
    }

    #[test]
    fn a_saved_key_closes_the_form_with_a_note() {
        let mut app = App::new(true);
        app.open_setup(true);
        let info = AccountInfo {
            account: nixxa_contracts::AccountRef::new("anthropic", "default"),
            method: nixxa_contracts::AuthMethod::ApiKey,
            is_default: true,
            hint: Some("wxyz".into()),
        };
        app.account_saved(&Ok(info));
        assert!(app.setup.is_none());
        assert_eq!(
            app.conversation.last().unwrap().text,
            "Saved the API key (…wxyz) for anthropic:default in the OS keychain."
        );
    }

    #[test]
    fn the_place_question_comes_first_and_enter_picks_the_highlighted_choice() {
        let mut app = App::new(true);
        app.ask_place(Vec::new());
        app.open_setup(true);
        app.paste("pasted");
        assert_eq!(app.handle_key(key(KeyCode::Char('1'))), vec![Effect::ChoosePlace(Place::Sandbox)]);
        assert!(app.place.is_none(), "answered");
        assert!(matches!(app.setup.as_ref().unwrap().step, Step::Choose { .. }), "setup got neither key nor paste");

        app.ask_place(Vec::new());
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty(), "there is no way past it but answering");
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), vec![Effect::ChoosePlace(Place::Here)]);
        app.ask_place(Vec::new());
        assert_eq!(app.handle_key(key(KeyCode::Char('2'))), vec![Effect::ChoosePlace(Place::Here)]);
        app.ask_place(Vec::new());
        assert!(app.handle_key(key(KeyCode::Char('3'))).is_empty(), "opens the second question");
        assert_eq!(app.handle_key(key(KeyCode::Char('2'))), vec![Effect::ChoosePlace(Place::SandboxCopy)]);
        app.ask_place(vec![(Place::Here, "refused: your home directory".into())]);
        assert!(app.handle_key(key(KeyCode::Char('2'))).is_empty(), "a blocked place can't be picked");
        assert!(app.place.is_some());
        assert!(app.handle_key(key(KeyCode::Char('4'))).is_empty());
        assert_eq!(app.handle_key(ctrl_c()), Vec::new(), "ctrl-c still works: the first warns");
        assert_eq!(app.handle_key(ctrl_c()), vec![Effect::Quit]);
    }

    #[test]
    fn esc_leaves_setup_only_when_an_account_already_exists() {
        let mut app = App::new(true);
        app.open_setup(true);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.setup.is_some(), "first run: there is nothing to go back to");
        app.open_setup(false);
        app.handle_key(key(KeyCode::Esc));
        assert!(app.setup.is_none());
    }

    #[test]
    fn ctrl_a_opens_setup_only_when_available() {
        let mut app = App::new(true);
        app.handle_key(ctrl('a'));
        assert!(app.setup.is_none(), "not with --fake");
        app.setup_available = true;
        app.handle_key(ctrl('a'));
        assert!(app.setup.as_ref().is_some_and(|f| !f.first_run));
    }

    #[test]
    fn a_paste_goes_to_the_setup_form_without_whitespace() {
        let mut app = App::new(true);
        app.open_setup(true);
        app.handle_key(key(KeyCode::Char('1')));
        app.paste(&format!("  {KEY}\n"));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), vec![Effect::SaveKey(Secret::new(KEY))]);
        assert!(app.input.is_empty());
    }

    fn stored(id: &str, root: PathBuf) -> nixxa_contracts::SessionInfo {
        nixxa_contracts::SessionInfo {
            id: SessionId::new(id),
            workspace: nixxa_contracts::WorkspaceInfo { name: "w".into(), root },
            account: AccountRef::new("anthropic", "default"),
            started_at: Timestamp(0),
            first_message: Some(format!("start of {id}")),
        }
    }

    #[test]
    fn ctrl_s_lists_sessions_enter_resumes_and_esc_closes() {
        let mut app = App::new(false);
        app.root = std::env::temp_dir().join("here");
        app.session = Some(SessionId::new("now"));
        assert_eq!(app.handle_key(ctrl('s')), vec![Effect::ListSessions]);
        app.sessions_listed(Ok(vec![
            stored("now", app.root.clone()),
            stored("old", app.root.clone()),
            stored("away", std::env::temp_dir().join("there")),
        ]));
        type_text(&mut app, "start");
        assert!(app.input.is_empty(), "typing goes to the list, not the input");
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), vec![Effect::SearchSessions("startx".into())]);
        app.sessions_found(
            "startx",
            Ok(vec![nixxa_contracts::SearchHit { session: SessionId::new("away"), snippet: "…startx…".into() }]),
        );
        assert_eq!(app.handle_key(key(KeyCode::Enter)), vec![Effect::Resume(SessionId::new("away"))]);
        assert!(app.sessions.is_none(), "closed once chosen");

        app.handle_key(ctrl('s'));
        app.sessions_listed(Ok(vec![stored("now", app.root.clone())]));
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty(), "the open session isn't resumed again");
        assert!(app.sessions.is_none() && app.notice.is_some());

        app.handle_key(ctrl('s'));
        assert!(app.handle_key(key(KeyCode::Esc)).is_empty());
        assert!(app.sessions.is_none());
    }

    #[test]
    fn starting_over_forgets_the_session_on_screen() {
        let mut app = App::new(false);
        app.apply(Event::Message { turn: turn(), message: Message::text(Role::User, "hi", Timestamp(0)) });
        app.apply(Event::ActionProposed { turn: turn(), request: request("a") });
        app.apply(Event::TurnStarted { turn: turn() });
        app.start_over();
        assert!(app.conversation.is_empty() && app.activity.is_empty() && !app.busy && app.session.is_none());
    }

    #[test]
    fn a_paste_goes_to_the_input_otherwise() {
        let mut app = App::new(true);
        app.paste("line one\r\nline two");
        assert_eq!(app.input, "line one\nline two");
    }

    #[test]
    fn a_failed_turn_discards_its_half_streamed_answer() {
        let mut app = App::new(true);
        app.apply(Event::TurnStarted { turn: turn() });
        app.apply(Event::TextDelta { turn: turn(), text: "I will".into() });
        app.apply(Event::Error {
            turn: Some(turn()),
            message: "provider error: the model declined this request".into(),
        });
        app.apply(Event::TurnDone { turn: turn() });
        assert_eq!(
            app.conversation,
            vec![Entry { speaker: Speaker::Error, text: "provider error: the model declined this request".into() }]
        );
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

    /// Regression (PR #7 review, B1): an unhandled ctrl shortcut must not type its letter.
    #[test]
    fn ctrl_shortcuts_do_not_type_but_altgr_does() {
        let mut app = App::new(true);
        app.handle_key(ctrl('a'));
        app.handle_key(ctrl('x'));
        assert_eq!(app.input, "", "ctrl-a and ctrl-x type nothing");
        let altgr = KeyEvent::new(KeyCode::Char('@'), KeyModifiers::CONTROL | KeyModifiers::ALT);
        app.handle_key(altgr);
        assert_eq!(app.input, "@", "AltGr (ctrl+alt on Windows) still types");

        app.open_setup(false);
        app.handle_key(key(KeyCode::Char('1')));
        app.handle_key(ctrl('x'));
        assert_eq!(key_form(&app).masked(), "", "nor in the key field");
        app.handle_key(KeyEvent::new(KeyCode::Char('{'), KeyModifiers::CONTROL | KeyModifiers::ALT));
        assert_eq!(key_form(&app).masked(), "•");
    }

    /// D-077: the user sees what their message will have replaced before sending, and chooses,
    /// one by one, to send a secret as it is.
    #[test]
    fn secrets_in_the_message_are_shown_and_can_be_sent_one_by_one() {
        let mut app = App::new(false);
        app.scrubber = Some(Arc::new(Scrubber::with_key([5; 32])));
        let (id, secret) = ("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        type_text(&mut app, &format!("use {id} and {secret}"));
        let found = app.input_secrets();
        assert_eq!(found.iter().map(|f| f.kind.as_str()).collect::<Vec<_>>(), ["aws-access-key", "aws-secret-key"]);
        let alt = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT);
        app.handle_key(alt('2'));
        assert_eq!(app.send_anyway, [found[1].placeholder.clone()]);
        app.handle_key(alt('1'));
        app.handle_key(alt('1'));
        assert_eq!(app.send_anyway, [found[1].placeholder.clone()], "alt+1 twice: back to replaced");
        app.handle_key(alt('9'));
        assert!(app.input.ends_with('9'), "no ninth secret: alt+9 types as before");
        app.input.pop();
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Send(Command::UserMessage(message))] = &effects[..] else { panic!("{effects:?}") };
        assert_eq!(message.send_anyway, [found[1].placeholder.clone()]);
        assert!(!message.from_terminal);
        assert!(app.send_anyway.is_empty(), "the choice was for that message only");
    }

    #[test]
    fn a_message_with_shared_terminal_lines_says_so() {
        let mut app = App::new(false);
        app.shared = true;
        type_text(&mut app, "look");
        let effects = app.handle_key(key(KeyCode::Enter));
        let [Effect::Send(Command::UserMessage(message))] = &effects[..] else { panic!("{effects:?}") };
        assert!(message.from_terminal);
        assert!(!app.shared);
    }

    #[test]
    fn each_redaction_is_told_in_the_conversation_and_the_activity_pane() {
        let mut app = App::new(false);
        app.apply(Event::SecretRedacted {
            turn: None,
            kind: "aws-access-key".into(),
            source: RedactionSource::FileRead { path: ".env".into() },
            placeholder: "‹secret:aws-access-key#3f2a›".into(),
            sent: false,
        });
        app.apply(Event::SecretRedacted {
            turn: None,
            kind: "github-token".into(),
            source: RedactionSource::UserMessage,
            placeholder: "‹secret:github-token#0b1c›".into(),
            sent: true,
        });
        let said: Vec<&str> = app.conversation.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            said[0],
            "Redacted a aws-access-key in `.env`: the agent sees ‹secret:aws-access-key#3f2a› instead."
        );
        assert!(said[1].starts_with("Sent a github-token in your message as it is, as you chose"), "{}", said[1]);
        assert_eq!(app.happenings.len(), 2);
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
    fn ctrl_n_opens_the_network_settings_and_keys_go_to_them() {
        let mut app = App::new(false);
        app.apply(Event::SandboxStatus {
            capabilities: nixxa_contracts::SandboxCapabilities {
                backend: "fake".into(),
                isolated: false,
                network: NetworkAccess::default(),
                jobs: false,
                terminal: true,
                checkpoints: false,
                actions: Vec::new(),
            },
        });
        assert!(app.handle_key(ctrl('n')).is_empty());
        assert!(app.network.open);
        assert_eq!(app.handle_key(key(KeyCode::Char('2'))), vec![Effect::SetNetwork(NetworkPolicy::Allow(Vec::new()))]);
        assert!(app.input.is_empty(), "keys don't reach the input");
        assert!(app.handle_key(key(KeyCode::Char('3'))).is_empty(), "Open waits for a second press");
        assert_eq!(app.handle_key(key(KeyCode::Char('3'))), vec![Effect::SetNetwork(NetworkPolicy::Open)]);
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.network.open);
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.input, "x");
    }

    #[test]
    fn an_applied_policy_is_noted_in_the_session_and_a_refused_one_is_undone() {
        let mut app = App::new(false);
        app.apply(Event::SandboxStatus {
            capabilities: nixxa_contracts::SandboxCapabilities {
                backend: "bubblewrap".into(),
                isolated: true,
                network: NetworkAccess::default(),
                jobs: true,
                terminal: true,
                checkpoints: false,
                actions: Vec::new(),
            },
        });
        let effects = app.network_set(Ok(NetworkPolicy::Open));
        assert_eq!(
            effects,
            vec![Effect::Send(Command::Note("The user opened the network to every public host.".into()))]
        );
        assert_eq!(app.sandbox.as_ref().unwrap().network, NetworkAccess::Controlled(NetworkPolicy::Open));
        app.network.mode = Mode::Allow;
        assert!(app.network_set(Err("the network proxy can't start: no room".into())).is_empty());
        assert_eq!(app.network.mode, Mode::Open, "back to what the sandbox applies");
        assert!(app.network.error.as_deref().is_some_and(|e| e.contains("no room")));
        assert_eq!(app.sandbox.as_ref().unwrap().network, NetworkAccess::Controlled(NetworkPolicy::Open));
    }

    #[test]
    fn tab_cycles_focus() {
        let mut app = App::new(true);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Conversation);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Activity);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Input);
    }

    #[test]
    fn shift_tab_switches_between_ask_and_auto() {
        let mut app = App::new(true);
        assert_eq!(app.mode, ApprovalMode::Ask, "every launch starts in Ask");
        assert!(!app.auto.is_on());
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.mode, ApprovalMode::Auto);
        assert!(app.auto.is_on(), "the approval port sees it");
        assert_eq!(app.focus, Focus::Input, "shift-tab no longer moves focus");
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.mode, ApprovalMode::Ask);
        assert!(!app.auto.is_on());
    }

    #[test]
    fn auto_in_this_directory_needs_a_second_press_in_time() {
        let mut app = App::new(true);
        app.here = true;
        let now = Instant::now();
        app.toggle_mode(now);
        assert_eq!(app.mode, ApprovalMode::Ask, "the first press only asks");
        assert!(app.notice.as_deref().unwrap().contains("confirm"));
        app.toggle_mode(now + CONFIRM_AUTO_WINDOW + Duration::from_millis(1));
        assert_eq!(app.mode, ApprovalMode::Ask, "too late: that press asks again");
        app.toggle_mode(now + CONFIRM_AUTO_WINDOW + Duration::from_secs(1));
        assert_eq!(app.mode, ApprovalMode::Auto, "confirmed in time");
        assert!(app.auto.is_on());

        // Another key in between cancels the confirmation.
        app.toggle_mode(now);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.mode, ApprovalMode::Ask);
        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(app.mode, ApprovalMode::Ask, "the confirmation was cancelled by the other key");
    }

    #[tokio::test]
    async fn shift_tab_on_an_open_prompt_approves_it_and_switches_to_auto() {
        use crate::ports::{TuiApprovals, UiEvent};
        use nixxa_contracts::ApprovalPort;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(true);
        let approvals = std::sync::Arc::new(TuiApprovals::new(tx, app.auto.clone()));
        app.apply(Event::ActionProposed { turn: turn(), request: request("a1") });
        let asking = {
            let approvals = approvals.clone();
            tokio::spawn(async move { approvals.ask(&request("a1"), "why", nixxa_contracts::Asking::ByMode).await })
        };
        let Some(UiEvent::Approval(prompt)) = rx.recv().await else { panic!("expected a prompt") };
        app.ask(prompt);
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(asking.await.unwrap(), Verdict::Allow);
        assert!(app.approval().is_none());
        assert_eq!(app.mode, ApprovalMode::Auto);
        assert!(app.activity[0].auto_approved, "shown as auto-approved");

        // The next action doesn't prompt at all.
        app.apply(Event::ActionProposed { turn: turn(), request: request("a2") });
        assert_eq!(approvals.ask(&request("a2"), "why", nixxa_contracts::Asking::ByMode).await, Verdict::Allow);
        let Some(UiEvent::AutoApproved(id)) = rx.recv().await else { panic!("expected an auto-approval") };
        app.auto_approved(&id);
        assert!(app.activity[1].auto_approved);
    }

    #[tokio::test]
    async fn y_and_n_answer_the_prompt_and_other_keys_wait() {
        use crate::ports::{TuiApprovals, UiEvent};
        use nixxa_contracts::ApprovalPort;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let approvals = std::sync::Arc::new(TuiApprovals::new(tx, crate::ports::AutoApprove::default()));
        let mut app = App::new(true);

        for (answer, expected) in [('y', Verdict::Allow), ('n', Verdict::Deny("denied by the user".into()))] {
            let asking = {
                let approvals = approvals.clone();
                tokio::spawn(async move { approvals.ask(&request("a1"), "why", nixxa_contracts::Asking::ByMode).await })
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

    // ---- /undo (D-073) ----

    fn with_checkpoints() -> App {
        let mut app = App::new(false);
        app.apply(Event::SandboxStatus {
            capabilities: SandboxCapabilities {
                backend: "bubblewrap".into(),
                isolated: true,
                network: NetworkAccess::Controlled(NetworkPolicy::Off),
                jobs: true,
                terminal: true,
                checkpoints: true,
                actions: nixxa_contracts::SandboxActionKind::ALL.to_vec(),
            },
        });
        app
    }

    fn checkpoint(app: &mut App, id: &str, before: Option<&str>) {
        app.apply(Event::Checkpoint {
            turn: turn(),
            checkpoint: nixxa_contracts::CheckpointId::new(id),
            before: before.map(ActionId::new),
        });
    }

    /// One turn: the user's message, a checkpoint at its start, one action with its own.
    fn one_turn(app: &mut App) {
        app.apply(Event::TurnStarted { turn: turn() });
        app.apply(Event::Message { turn: turn(), message: Message::text(Role::User, "clean up", Timestamp(0)) });
        checkpoint(app, "c1", None);
        app.apply(Event::ActionProposed { turn: turn(), request: request("a1") });
        checkpoint(app, "c2", Some("a1"));
        app.apply(Event::TurnDone { turn: turn() });
    }

    #[test]
    fn ctrl_z_and_slash_undo_open_the_view_on_the_last_checkpoint() {
        let mut app = with_checkpoints();
        one_turn(&mut app);
        let effects = app.handle_key(ctrl('z'));
        assert_eq!(effects, vec![Effect::UndoDiff(nixxa_contracts::CheckpointId::new("c2"))]);
        let view = app.undo.as_ref().expect("open");
        assert_eq!(view.chosen().label, "before `exec ls`");
        assert_eq!(view.choices[1].label, "at the start of the turn for \"clean up\"");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.undo.is_none());
        type_text(&mut app, "/undo");
        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(effects, vec![Effect::UndoDiff(nixxa_contracts::CheckpointId::new("c2"))], "not sent as a message");
        assert!(app.input.is_empty() && app.undo.is_some());
    }

    #[test]
    fn undo_sends_the_checkpoint_and_what_the_user_saw_after_a_second_enter() {
        let mut app = with_checkpoints();
        one_turn(&mut app);
        app.handle_key(ctrl('z'));
        assert_eq!(
            app.handle_key(key(KeyCode::Left)),
            vec![Effect::UndoDiff(nixxa_contracts::CheckpointId::new("c1"))]
        );
        let change = nixxa_contracts::FileChange {
            path: "src/main.rs".into(),
            before: Some(nixxa_contracts::EntryState::Dir { mode: 0o755 }),
            after: None,
            diff: None,
        };
        let diff = nixxa_contracts::WorkspaceDiff { changes: vec![change], not_covered: Vec::new() };
        app.undo_diff(&nixxa_contracts::CheckpointId::new("c1"), Ok(diff));
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty(), "the first enter asks");
        let effects = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            effects,
            vec![Effect::Send(Command::Undo {
                to: nixxa_contracts::CheckpointId::new("c1"),
                seen: Some(nixxa_contracts::CheckpointId::new("c2")),
            })]
        );
        assert!(app.undo.is_none());
    }

    #[test]
    fn undo_is_refused_during_a_turn_or_an_approval_and_without_checkpoints() {
        let mut app = with_checkpoints();
        one_turn(&mut app);
        app.apply(Event::TurnStarted { turn: turn() });
        assert!(app.handle_key(ctrl('z')).is_empty());
        assert!(app.undo.is_none());
        assert!(app.notice.as_deref().unwrap().contains("between turns"), "{:?}", app.notice);
        // Typed as a message during a turn, it still isn't sent to the agent.
        type_text(&mut app, "/undo");
        assert!(app.handle_key(key(KeyCode::Enter)).is_empty());
        let mut none = App::new(false);
        assert!(none.handle_key(ctrl('z')).is_empty());
        assert!(none.notice.as_deref().unwrap().contains("keeps no checkpoints"));
        let mut early = with_checkpoints();
        assert!(early.handle_key(ctrl('z')).is_empty());
        assert!(early.notice.as_deref().unwrap().contains("nothing to undo yet"));
    }

    #[test]
    fn a_new_checkpoint_or_turn_closes_a_stale_view() {
        let mut app = with_checkpoints();
        one_turn(&mut app);
        app.handle_key(ctrl('z'));
        checkpoint(&mut app, "c3", None);
        assert!(app.undo.is_none(), "what it showed is stale");
        assert!(app.notice.as_deref().unwrap().contains("changed while /undo was open"));
        app.notice = None;
        app.handle_key(ctrl('z'));
        app.apply(Event::TurnStarted { turn: turn() });
        assert!(app.undo.is_none());
    }

    #[test]
    fn restores_and_actions_without_a_checkpoint_show_in_the_panes() {
        let mut app = with_checkpoints();
        one_turn(&mut app);
        app.apply(Event::NotUndoable {
            turn: turn(),
            before: Some(ActionId::new("a1")),
            reason: "the store is full".into(),
        });
        let undone = nixxa_contracts::WorkspaceDiff::default();
        app.apply(Event::Restored { checkpoint: nixxa_contracts::CheckpointId::new("c1"), undone });
        assert!(matches!(&app.happenings[0].1, Happening::NotUndoable { reason, .. } if reason == "the store is full"));
        assert!(
            matches!(&app.happenings[1].1, Happening::Restored { paths: 0, label } if label.contains("start of the turn"))
        );
        let note = &app.conversation.last().unwrap().text;
        assert!(note.starts_with("Undone: the workspace is back as it was at the start of the turn"), "{note}");
    }
}
