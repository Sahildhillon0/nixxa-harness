//! The terminal loop: owns the terminal, feeds keys and controller events into [`App`], draws.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream};
use futures::StreamExt;
use nixxa_contracts::{AccountRef, Brain, WorkspaceInfo};
use nixxa_controller::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::{App, Effect};
use crate::ports::UiEvent;
use crate::settings::Settings;
use crate::wiring::{self, Anthropic};
use crate::{logging, paths, ui};

/// How long quitting waits for the controller to end the session.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// A running session.
struct Session {
    commands: mpsc::Sender<Command>,
    task: JoinHandle<()>,
    account: AccountRef,
}

/// Starts a controller for `account` and runs its loop in the background.
fn start_session(
    brain: Arc<dyn Brain>,
    account: AccountRef,
    workspace: WorkspaceInfo,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
) -> Session {
    let (commands, rx) = mpsc::channel(32);
    let mut controller = wiring::controller(brain, account.clone(), workspace, ui_tx.clone());
    let task = tokio::spawn(async move {
        if let Err(err) = controller.start_session().await {
            let message = format!("could not start a session: {err}");
            let _ = ui_tx.send(UiEvent::Controller(nixxa_contracts::Event::Error { turn: None, message }));
            return;
        }
        controller.run(rx).await;
    });
    Session { commands, task, account }
}

/// Runs the TUI until the user quits.
pub(crate) async fn run(settings: Settings) -> io::Result<()> {
    let workspace = wiring::workspace(&std::env::current_dir()?);
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let mut app = App::new(color);
    let log_filter = std::env::var("NIXXA_LOG").ok();
    match logging::init(paths::log_dir(), log_filter.as_deref()) {
        Ok(dir) => tracing::info!(version = env!("CARGO_PKG_VERSION"), logs = %dir.display(), "nixxa starting"),
        Err(why) => app.note(format!("logging is off: {why}")),
    }

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let mut session: Option<Session> = None;
    let mut anthropic: Option<Arc<Anthropic>> = None;
    if settings.fake {
        tracing::info!(brain = "scripted", "using the fake brain");
        let (brain, account) = wiring::scripted();
        session = Some(start_session(brain, account, workspace.clone(), ui_tx.clone()));
    } else {
        match Anthropic::new(&settings) {
            Ok(claude) => {
                let claude = Arc::new(claude);
                app.setup_available = true;
                if let Some(problem) = claude.storage_problem().await {
                    tracing::warn!("the OS keychain is unavailable");
                    let fallback = if settings.env_key.is_some() { " Using ANTHROPIC_API_KEY." } else { "" };
                    app.note(format!("The OS keychain can't be used: {problem}. Keys can't be saved.{fallback}"));
                }
                match claude.default_account().await {
                    Some(account) => {
                        tracing::info!(account = %account, "starting with the default account");
                        session = Some(start_session(claude.brain.clone(), account, workspace.clone(), ui_tx.clone()));
                    }
                    None => app.open_setup(true),
                }
                anthropic = Some(claude);
            }
            Err(err) => app.note(err),
        }
    }

    // `init` enters raw mode and the alternate screen and restores them on panic.
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
    let mut keys = EventStream::new();
    let result = loop {
        if let Err(err) = terminal.draw(|frame| ui::draw(frame, &mut app)) {
            break Err(err);
        }
        let effects = tokio::select! {
            input = keys.next() => match input {
                Some(Ok(TermEvent::Key(key))) => app.handle_key(key),
                Some(Ok(TermEvent::Paste(text))) => {
                    app.paste(&text);
                    Vec::new()
                }
                Some(Ok(_)) => Vec::new(),
                Some(Err(err)) => break Err(err),
                None => break Ok(()),
            },
            Some(event) = ui_rx.recv() => {
                let mut events = vec![event];
                // Take whatever else arrived, so a burst of deltas costs one redraw.
                while let Ok(event) = ui_rx.try_recv() {
                    events.push(event);
                }
                for event in events {
                    if let (UiEvent::KeySaved(Ok(info)), Some(claude)) = (&event, &anthropic) {
                        match &session {
                            None => {
                                let brain = claude.brain.clone();
                                session = Some(start_session(brain, info.account.clone(), workspace.clone(), ui_tx.clone()));
                            }
                            Some(current) if current.account != info.account => app.note(format!(
                                "This session keeps using {}; new sessions use {}.",
                                current.account, info.account
                            )),
                            Some(_) => {}
                        }
                    }
                    receive(&mut app, event);
                }
                Vec::new()
            }
        };
        let mut quit = false;
        for effect in effects {
            match effect {
                // No session yet (setup), or it already ended: nothing to send to.
                Effect::Send(command) => {
                    if let Some(session) = &session {
                        drop(session.commands.send(command).await);
                    }
                }
                Effect::SaveKey(key) => {
                    if let Some(claude) = anthropic.clone() {
                        let ui_tx = ui_tx.clone();
                        tokio::spawn(async move {
                            let result = claude.save_key(key).await;
                            let _ = ui_tx.send(UiEvent::KeySaved(result));
                        });
                    }
                }
                Effect::Quit => quit = true,
            }
        }
        if quit {
            break Ok(());
        }
    };

    if let Some(session) = session {
        let _ = session.commands.send(Command::Shutdown).await;
        if tokio::time::timeout(SHUTDOWN_GRACE, session.task).await.is_err() {
            tracing::warn!("the controller did not end the session in time");
        }
    }
    // Log what the controller said while shutting down (session ended).
    while let Ok(event) = ui_rx.try_recv() {
        if let UiEvent::Controller(event) = event {
            logging::event(&event);
        }
    }
    let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    match &result {
        Ok(()) => tracing::info!("nixxa quit"),
        Err(err) => tracing::error!(%err, "terminal error"),
    }
    result
}

fn receive(app: &mut App, event: UiEvent) {
    match event {
        UiEvent::Controller(event) => {
            logging::event(&event);
            app.apply(event);
        }
        UiEvent::Approval(prompt) => app.ask(prompt),
        UiEvent::KeySaved(result) => app.key_saved(&result),
    }
}
