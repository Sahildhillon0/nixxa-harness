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
use crate::setup::Setup;
use crate::wiring::{self, Models};
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

/// Ends a session in the background (the controller emits `SessionEnded`).
fn end_session(session: Session) {
    tokio::spawn(async move {
        let _ = session.commands.send(Command::Shutdown).await;
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, session.task).await;
    });
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
    let mut models: Option<Arc<Models>> = None;
    if settings.fake {
        tracing::info!(brain = "scripted", "using the fake brain");
        let (brain, account) = wiring::scripted();
        session = Some(start_session(brain, account, workspace.clone(), ui_tx.clone()));
    } else {
        match Models::new(&settings) {
            Ok(found) => {
                let found = Arc::new(found);
                app.setup_available = true;
                if let Some(problem) = found.storage_problem().await {
                    tracing::warn!("the OS keychain is unavailable");
                    let fallback = if settings.env_key.is_some() { " Using ANTHROPIC_API_KEY." } else { "" };
                    app.note(format!("The OS keychain can't be used: {problem}. Accounts can't be saved.{fallback}"));
                }
                match found.start_account().await {
                    Some(account) => match found.brain_for(&account).await {
                        Ok(brain) => {
                            tracing::info!(account = %account, "starting with the active account");
                            session = Some(start_session(brain, account, workspace.clone(), ui_tx.clone()));
                        }
                        Err(why) => {
                            app.note(why);
                            app.setup = Some(match found.local_url(&account).await {
                                Some(url) => Setup::local(true, &url),
                                None => Setup::new(true),
                            });
                        }
                    },
                    None => app.open_setup(true),
                }
                models = Some(found);
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
                    let saved = match (&event, &models) {
                        (UiEvent::AccountSaved(Ok(info)), Some(found)) => Some((info.account.clone(), found.clone())),
                        _ => None,
                    };
                    receive(&mut app, event);
                    if let Some((account, found)) = saved {
                        // A key rotation for the account in use needs nothing: the brain reads the
                        // key on every request. Anything else starts a session with the new brain.
                        let rotation = session.as_ref().is_some_and(|s| s.account == account && account.provider.as_str() == wiring::ANTHROPIC);
                        if !rotation {
                            match found.brain_for(&account).await {
                                Ok(brain) => {
                                    if let Some(old) = session.take() {
                                        end_session(old);
                                        app.note(format!("Switched to {account}: a new session started."));
                                    }
                                    session = Some(start_session(brain, account, workspace.clone(), ui_tx.clone()));
                                }
                                Err(why) => app.note(why),
                            }
                        }
                    }
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
                    if let Some(found) = models.clone() {
                        let ui_tx = ui_tx.clone();
                        tokio::spawn(async move {
                            let result = found.save_key(key).await;
                            let _ = ui_tx.send(UiEvent::AccountSaved(result));
                        });
                    }
                }
                Effect::ListModels(url) => {
                    if let Some(found) = models.clone() {
                        let ui_tx = ui_tx.clone();
                        tokio::spawn(async move {
                            let result = found.list_local_models(&url).await;
                            let _ = ui_tx.send(UiEvent::ModelsListed(result));
                        });
                    }
                }
                Effect::SaveLocal { url, model } => {
                    if let Some(found) = models.clone() {
                        let ui_tx = ui_tx.clone();
                        tokio::spawn(async move {
                            let result = found.save_local(&url, &model).await;
                            let _ = ui_tx.send(UiEvent::AccountSaved(result));
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
        UiEvent::AccountSaved(result) => app.account_saved(&result),
        UiEvent::ModelsListed(result) => app.models_listed(result),
    }
}
