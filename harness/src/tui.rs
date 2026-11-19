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
use crate::place::Place;
use crate::ports::UiEvent;
use crate::settings::Settings;
use crate::setup::Setup;
use crate::wiring::{self, Models, Workplace};
use crate::{browser, logging, paths, ui};
use nixxa_accounts::methods::SignInError;
use nixxa_accounts::methods::openrouter::{OpenRouterConfig, OpenRouterSignIn};
use nixxa_contracts::{Credential, Secret};

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
    workplace: &Workplace,
    ui_tx: mpsc::UnboundedSender<UiEvent>,
) -> Session {
    let (commands, rx) = mpsc::channel(32);
    let mut controller = wiring::controller(brain, account.clone(), workspace, workplace, ui_tx.clone());
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

/// A sign-in problem, in words for the setup screen.
fn sign_in_words(err: &SignInError) -> String {
    let mut words = err.to_string();
    if let Some(first) = words.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    words
}

/// Ends a session in the background (the controller emits `SessionEnded`).
fn end_session(session: Session) {
    tokio::spawn(async move {
        let _ = session.commands.send(Command::Shutdown).await;
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, session.task).await;
    });
}

/// Where the agent works, in words for the conversation once the start-up question is answered.
fn place_note(workplace: &Workplace, workspace: &WorkspaceInfo) -> String {
    match workplace.place {
        Place::Sandbox => "Working in the sandbox: nothing on this machine is read or changed.".into(),
        Place::Here => format!(
            "Working in {}: the agent can read, edit and run commands on your real files there. \
             Every action still asks first.",
            workspace.root.display()
        ),
    }
}

/// Runs the TUI until the user quits.
pub(crate) async fn run(settings: Settings) -> io::Result<()> {
    let workspace = wiring::workspace(&std::env::current_dir()?);
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let mut app = App::new(color);
    // Asked on every launch; no session starts until it is answered.
    app.directory = workspace.root.display().to_string();
    app.ask_place();
    let mut workplace: Option<Workplace> = None;
    // A brain and account ready to go, waiting for the answer.
    let mut waiting: Option<(Arc<dyn Brain>, AccountRef)> = None;
    let log_filter = std::env::var("NIXXA_LOG").ok();
    match logging::init(paths::log_dir(), log_filter.as_deref()) {
        Ok(dir) => tracing::info!(version = env!("CARGO_PKG_VERSION"), logs = %dir.display(), "nixxa starting"),
        Err(why) => app.note(format!("logging is off: {why}")),
    }

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let mut session: Option<Session> = None;
    let mut models: Option<Arc<Models>> = None;
    // The running OpenRouter sign-in, and the key it produced until a model is chosen.
    let mut sign_in: Option<JoinHandle<()>> = None;
    let mut signed_in_key: Option<Secret> = None;
    if settings.fake {
        tracing::info!(brain = "scripted", "using the fake brain");
        waiting = Some(wiring::scripted());
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
                            waiting = Some((brain, account));
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
                    match &event {
                        UiEvent::SignInStarted(Ok(url)) => {
                            let opened = browser::open(url);
                            if let Some(setup) = app.setup.as_mut() {
                                setup.sign_in_started(url.clone(), opened);
                            }
                            continue;
                        }
                        UiEvent::SignInStarted(Err(why)) | UiEvent::SignedIn(Err(why)) => {
                            tracing::warn!("OpenRouter sign-in failed");
                            if let Some(setup) = app.setup.as_mut() {
                                setup.failed(why);
                            }
                            continue;
                        }
                        UiEvent::SignedIn(Ok(key)) => {
                            tracing::info!("OpenRouter sign-in finished");
                            signed_in_key = Some(key.clone());
                            if let Some(setup) = app.setup.as_mut() {
                                setup.signed_in();
                            }
                            if let Some(found) = models.clone() {
                                let (ui_tx, key) = (ui_tx.clone(), key.clone());
                                tokio::spawn(async move {
                                    let result = found.list_openrouter_models(&key).await;
                                    let _ = ui_tx.send(UiEvent::ModelsListed(result));
                                });
                            }
                            continue;
                        }
                        UiEvent::AccountSaved(Ok(info)) if info.account.provider.as_str() == wiring::OPENROUTER => {
                            signed_in_key = None;
                        }
                        _ => {}
                    }
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
                                    match &workplace {
                                        Some(place) => {
                                            session = Some(start_session(brain, account, workspace.clone(), place, ui_tx.clone()));
                                        }
                                        None => waiting = Some((brain, account)),
                                    }
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
                Effect::StartSignIn => {
                    if let Some(old) = sign_in.take() {
                        old.abort();
                    }
                    signed_in_key = None;
                    tracing::info!("OpenRouter sign-in started");
                    let ui_tx = ui_tx.clone();
                    sign_in = Some(tokio::spawn(async move {
                        let started = match OpenRouterSignIn::new(OpenRouterConfig::default()) {
                            Ok(flow) => flow.start().await,
                            Err(err) => Err(err),
                        };
                        let pending = match started {
                            Ok((url, pending)) => {
                                let _ = ui_tx.send(UiEvent::SignInStarted(Ok(url)));
                                pending
                            }
                            Err(err) => {
                                let _ = ui_tx.send(UiEvent::SignInStarted(Err(sign_in_words(&err))));
                                return;
                            }
                        };
                        let result = match pending.finish().await {
                            Ok(Credential::BearerToken { token, .. }) => Ok(token),
                            Ok(_) => Err("OpenRouter answered with an unexpected credential.".to_string()),
                            Err(err) => Err(sign_in_words(&err)),
                        };
                        let _ = ui_tx.send(UiEvent::SignedIn(result));
                    }));
                }
                Effect::CancelSignIn => {
                    if let Some(task) = sign_in.take() {
                        task.abort();
                        tracing::info!("OpenRouter sign-in cancelled");
                    }
                    signed_in_key = None;
                }
                Effect::SaveOpenRouter { model } => match (models.clone(), signed_in_key.clone()) {
                    (Some(found), Some(key)) => {
                        let ui_tx = ui_tx.clone();
                        tokio::spawn(async move {
                            let result = found.save_openrouter(key, &model).await;
                            let _ = ui_tx.send(UiEvent::AccountSaved(result));
                        });
                    }
                    _ => app.account_saved(&Err("The sign-in expired; sign in again.".into())),
                },
                Effect::ChoosePlace(choice) => {
                    let chosen = Workplace::new(choice, &workspace).unwrap_or_else(|why| {
                        tracing::warn!(%why, "can't work in the current directory");
                        app.note(format!("{why}. Using the sandbox instead."));
                        Workplace::new(Place::Sandbox, &workspace).expect("the sandbox needs no directory")
                    });
                    tracing::info!(place = ?chosen.place, "workplace chosen");
                    app.note(place_note(&chosen, &workspace));
                    if let Some((brain, account)) = waiting.take() {
                        session = Some(start_session(brain, account, workspace.clone(), &chosen, ui_tx.clone()));
                    }
                    workplace = Some(chosen);
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
        // Handled in the loop, which holds the sign-in state.
        UiEvent::SignInStarted(_) | UiEvent::SignedIn(_) => {}
    }
}
