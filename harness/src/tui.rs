//! The terminal loop: owns the terminal, feeds keys and controller events into [`App`], draws.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange, Event as TermEvent, EventStream,
};
use futures::StreamExt;
use nixxa_contracts::{
    AccountRef, Brain, OpenTerminal, Sandbox, SessionId, Store, TerminalEvent, TerminalSize, WorkspaceInfo,
};
use nixxa_controller::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::{App, Effect};
use crate::place::Place;
use crate::ports::{UiEvent, UiLink};
use crate::settings::Settings;
use crate::setup::Setup;
use crate::wiring::{self, Models};
use crate::workplace::{self, Workplace};
use crate::{browser, logging, paths, ui};
use nixxa_accounts::methods::SignInError;
use nixxa_accounts::methods::openrouter::{OpenRouterConfig, OpenRouterSignIn};
use nixxa_contracts::{Credential, Secret};

/// How long quitting waits for the controller to end the session.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
/// How long the sandbox gets to start and answer once a sandboxed place is chosen.
const SANDBOX_START: Duration = Duration::from_secs(30);

/// Terminal output the UI loop may hold before the sandbox has to wait for it.
const TERMINAL_BACKLOG: usize = 16;

/// What the open terminal's task tells the UI loop, with the terminal's number (so a replaced
/// terminal's last words are dropped).
enum TermMsg {
    Opened(u64, Result<(), String>),
    Output(u64, Vec<u8>),
    Closed(u64, String),
}

/// What the UI loop asks of the open terminal.
enum TermOp {
    Write(Vec<u8>),
    Resize(TerminalSize),
}

/// The open terminal's task. Dropping it (or its `ops`) closes the terminal.
struct TermLink {
    number: u64,
    ops: mpsc::UnboundedSender<TermOp>,
    task: JoinHandle<()>,
}

/// Opens the user's terminal and runs it: its output to the UI loop (waiting while the loop is
/// behind, so the sandbox holds the shell instead of anything piling up), and the loop's input
/// and resizes to it, in order. Ends when the terminal does, or when the loop lets go.
async fn run_terminal(
    sandbox: Arc<dyn Sandbox>,
    size: TerminalSize,
    number: u64,
    tx: mpsc::Sender<TermMsg>,
    mut ops: mpsc::UnboundedReceiver<TermOp>,
) {
    let OpenTerminal { terminal, mut events } = match sandbox.open_terminal(size).await {
        Ok(open) => open,
        Err(err) => {
            tracing::warn!(%err, "the terminal couldn't open");
            let _ = tx.send(TermMsg::Opened(number, Err(err.to_string()))).await;
            return;
        }
    };
    tracing::info!("terminal opened");
    if tx.send(TermMsg::Opened(number, Ok(()))).await.is_err() {
        return;
    }
    let output = async {
        while let Some(event) = events.next().await {
            match event {
                TerminalEvent::Output(bytes) => {
                    if tx.send(TermMsg::Output(number, bytes)).await.is_err() {
                        return;
                    }
                }
                TerminalEvent::Closed { reason } => {
                    tracing::info!("terminal closed");
                    let _ = tx.send(TermMsg::Closed(number, reason)).await;
                    return;
                }
            }
        }
        let _ = tx.send(TermMsg::Closed(number, "its output ended".into())).await;
    };
    let input = async {
        while let Some(op) = ops.recv().await {
            // A failure means it has ended; its Closed says why.
            let _ = match op {
                TermOp::Write(bytes) => terminal.write(&bytes).await,
                TermOp::Resize(size) => terminal.resize(size).await,
            };
        }
    };
    tokio::select! {
        () = output => {}
        () = input => {}
    }
}

/// A running session.
struct Session {
    commands: mpsc::Sender<Command>,
    task: JoinHandle<()>,
    brain: Arc<dyn Brain>,
    account: AccountRef,
}

/// Starts a controller for `account` and runs its loop in the background: a new session, or
/// the stored session `resume` (D-057).
fn start_session(
    brain: Arc<dyn Brain>,
    account: AccountRef,
    workspace: WorkspaceInfo,
    workplace: &Workplace,
    store: Arc<dyn Store>,
    ui: &UiLink,
    resume: Option<SessionId>,
) -> Session {
    let (commands, rx) = mpsc::channel(32);
    let mut controller = wiring::controller(brain.clone(), account.clone(), workspace, workplace, store, ui);
    let ui_tx = ui.tx.clone();
    let task = tokio::spawn(async move {
        let started = match &resume {
            Some(id) => {
                controller.resume_session(id).await.map_err(|err| format!("could not resume the session: {err}"))
            }
            None => controller.start_session().await.map_err(|err| format!("could not start a session: {err}")),
        };
        if let Err(message) = started {
            let _ = ui_tx.send(UiEvent::Controller(nixxa_contracts::Event::Error { turn: None, message }));
            return;
        }
        controller.run(rx).await;
    });
    Session { commands, task, brain, account }
}

/// With `--continue`, the first session to start is the latest one for this directory. Asked
/// once; says so when there is nothing to continue.
async fn continue_target(
    pending: &mut bool,
    store: &dyn Store,
    workspace: &WorkspaceInfo,
    app: &mut App,
) -> Option<SessionId> {
    if !std::mem::take(pending) {
        return None;
    }
    match wiring::latest_session(store, &workspace.root).await {
        Ok(Some(id)) => {
            tracing::info!("continuing the latest session for this directory");
            Some(id)
        }
        Ok(None) => {
            app.note("There is no earlier session in this directory to continue: starting a new one.");
            None
        }
        Err(err) => {
            app.note(format!("Couldn't look for an earlier session ({err}): starting a new one."));
            None
        }
    }
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
    let root = workspace.root.display();
    let modes = "Every action asks first; shift-tab switches between Ask and Auto.";
    match workplace.place {
        _ if workplace.is_fake() => {
            format!("Working in the demo's in-memory sandbox: nothing on this machine is read or changed. {modes}")
        }
        Place::Sandbox => format!(
            "Working in an empty sandbox: your files aren't visible to the agent, and there is no internet. {modes}"
        ),
        Place::Here => format!(
            "Working in {root}, without isolation: the agent can read, edit and run commands on your real files there. {modes}"
        ),
        Place::SandboxHere => format!(
            "Working on {root} inside the sandbox: changes to it are real, but nothing else on this machine is reachable. {modes}"
        ),
        Place::SandboxCopy => {
            let copy = workplace.copy.as_deref().map(|c| c.display().to_string()).unwrap_or_default();
            format!(
                "Working on a copy of {root}, at {copy}: your real directory is not changed, and the copy stays there after you quit. {modes}"
            )
        }
    }
}

/// Sets up the chosen place and, for the sandboxed ones, starts the sandbox and waits for its
/// answer, so the status line shows the real backend before the first action and a start
/// failure is reported here. The setup itself (a copy, probing systemd) runs on a blocking thread.
async fn prepare(place: Place, workspace: WorkspaceInfo) -> Result<Workplace, String> {
    let chosen = tokio::task::spawn_blocking(move || Workplace::new(place, &workspace))
        .await
        .map_err(|_| "setting up the workplace failed".to_string())??;
    if place.sandboxed() {
        let started = match tokio::time::timeout(SANDBOX_START, chosen.sandbox.health()).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(err)) => Err(format!("the sandbox did not start: {err}")),
            Err(_) => Err("the sandbox did not answer in time".to_string()),
        };
        if let Err(why) = started {
            if let Some(copy) = &chosen.copy {
                let _ = std::fs::remove_dir_all(copy);
            }
            return Err(why);
        }
    }
    Ok(chosen)
}

/// Runs the TUI until the user quits.
pub(crate) async fn run(settings: Settings) -> io::Result<()> {
    let workspace = wiring::workspace(&std::env::current_dir()?);
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let mut app = App::new(color);
    app.notifications.channels = settings.notify;
    // Asked on every launch; no session starts until it is answered.
    app.directory = workspace.root.display().to_string();
    app.root = workspace.root.clone();
    workplace::remove_old_scratch();
    workplace::remove_old_checkpoints();
    let mut blocked = workplace::blocked(&workspace.root, settings.fake);
    app.ask_place(blocked.clone());
    let mut workplace: Option<Workplace> = None;
    // A brain and account ready to go, waiting for the answer.
    let mut waiting: Option<(Arc<dyn Brain>, AccountRef)> = None;
    let log_filter = std::env::var("NIXXA_LOG").ok();
    match logging::init(paths::log_dir(), log_filter.as_deref()) {
        Ok(dir) => tracing::info!(version = env!("CARGO_PKG_VERSION"), logs = %dir.display(), "nixxa starting"),
        Err(why) => app.note(format!("logging is off: {why}")),
    }

    let (store, history_off) = wiring::store(settings.fake, paths::db_path()).await;
    if let Some(note) = history_off {
        app.note(note);
    }
    let mut continue_latest = settings.resume_latest;

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let link = UiLink::new(ui_tx.clone(), app.auto.clone());
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

    // The user's terminal (`ctrl+t`), and its messages.
    let (term_tx, mut term_rx) = mpsc::channel::<TermMsg>(TERMINAL_BACKLOG);
    let mut shell: Option<TermLink> = None;
    let mut opened = 0u64;

    // `init` enters raw mode and the alternate screen and restores them on panic.
    let mut terminal = ratatui::init();
    // Focus reporting tells whether the user is looking (D-069); terminals without it never say
    // they lost focus, and then only the idle time counts.
    let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste, EnableFocusChange);
    let mut notifier = crate::notify::SystemNotifier;
    let mut keys = EventStream::new();
    let result = loop {
        if let Err(err) = terminal.draw(|frame| ui::draw(frame, &mut app)) {
            break Err(err);
        }
        // The terminal pane's size follows the layout; its shell is told of every change.
        if let (Some(size), Some(link)) = (app.terminal.take_resize(), &shell) {
            let _ = link.ops.send(TermOp::Resize(size));
        }
        let effects = tokio::select! {
            input = keys.next() => match input {
                Some(Ok(TermEvent::Key(key))) => app.handle_key(key),
                Some(Ok(TermEvent::Paste(text))) => app.paste(&text),
                Some(Ok(TermEvent::FocusGained)) => {
                    app.focus_changed(true);
                    Vec::new()
                }
                Some(Ok(TermEvent::FocusLost)) => {
                    app.focus_changed(false);
                    Vec::new()
                }
                Some(Ok(_)) => Vec::new(),
                Some(Err(err)) => break Err(err),
                None => break Ok(()),
            },
            Some(message) = term_rx.recv() => {
                let mut messages = vec![message];
                while let Ok(message) = term_rx.try_recv() {
                    messages.push(message);
                }
                let current = shell.as_ref().map(|link| link.number);
                let mut produced = Vec::new();
                for message in messages {
                    match message {
                        TermMsg::Opened(n, result) if Some(n) == current => {
                            if result.is_err() {
                                shell = None;
                            }
                            produced.extend(app.terminal_opened(result));
                        }
                        TermMsg::Output(n, bytes) if Some(n) == current => app.terminal_output(&bytes),
                        TermMsg::Closed(n, reason) if Some(n) == current => {
                            shell = None;
                            produced.extend(app.terminal_closed(reason));
                        }
                        _ => {}
                    }
                }
                produced
            }
            Some(event) = ui_rx.recv() => {
                let mut events = vec![event];
                // Take whatever else arrived, so a burst of deltas costs one redraw.
                while let Ok(event) = ui_rx.try_recv() {
                    events.push(event);
                }
                let mut produced = Vec::new();
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
                    produced.extend(receive(&mut app, event));
                    if let Some((account, found)) = saved {
                        // A key rotation for the account in use needs nothing: the brain reads the
                        // key on every request. Anything else starts a session with the new brain.
                        let rotation = session.as_ref().is_some_and(|s| s.account == account && account.provider.as_str() == wiring::ANTHROPIC);
                        if !rotation {
                            match found.brain_for(&account).await {
                                Ok(brain) => {
                                    if let Some(old) = session.take() {
                                        link.generation.advance();
                                        end_session(old);
                                        app.note(format!("Switched to {account}: a new session started."));
                                    }
                                    match &workplace {
                                        Some(place) => {
                                            let resume = continue_target(&mut continue_latest, &*store, &workspace, &mut app).await;
                                            session = Some(start_session(brain, account, workspace.clone(), place, store.clone(), &link, resume));
                                        }
                                        None => waiting = Some((brain, account)),
                                    }
                                }
                                Err(why) => app.note(why),
                            }
                        }
                    }
                }
                produced
            }
        };
        crate::notify::deliver(app.take_alerts(), &mut notifier);
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
                    let demo = settings.fake && choice == Place::Sandbox;
                    if choice.sandboxed() && !demo {
                        app.note(if choice == Place::SandboxCopy {
                            "Copying this directory and starting the sandbox…"
                        } else {
                            "Starting the sandbox…"
                        });
                        let _ = terminal.draw(|frame| ui::draw(frame, &mut app));
                    }
                    let ready = if demo { Ok(Workplace::fake()) } else { prepare(choice, workspace.clone()).await };
                    match ready {
                        Ok(chosen) => {
                            tracing::info!(place = ?chosen.place, "workplace chosen");
                            app.here = chosen.place == Place::Here;
                            app.note(place_note(&chosen, &workspace));
                            if let Some(why) = &chosen.undo_off {
                                app.note(format!("Undo is off for this run: {why}."));
                            }
                            if let Some((brain, account)) = waiting.take() {
                                let resume = continue_target(&mut continue_latest, &*store, &workspace, &mut app).await;
                                session = Some(start_session(
                                    brain,
                                    account,
                                    workspace.clone(),
                                    &chosen,
                                    store.clone(),
                                    &link,
                                    resume,
                                ));
                            }
                            watch_sandbox(&chosen, &ui_tx);
                            workplace = Some(chosen);
                        }
                        Err(why) => {
                            // Never fall back to something less isolated (D-008): ask again, with
                            // this place shown as unavailable and why.
                            tracing::warn!(place = ?choice, "the chosen place can't be used");
                            app.note(format!("{why}. Choose again."));
                            blocked.retain(|(place, _)| *place != choice);
                            blocked.push((choice, why));
                            app.ask_place(blocked.clone());
                        }
                    }
                }
                Effect::ListSessions => {
                    let (store, ui_tx) = (store.clone(), ui_tx.clone());
                    tokio::spawn(async move {
                        let result = store.list_sessions().await.map_err(|err| err.to_string());
                        let _ = ui_tx.send(UiEvent::SessionsListed(result));
                    });
                }
                Effect::SearchSessions(query) => {
                    let (store, ui_tx) = (store.clone(), ui_tx.clone());
                    tokio::spawn(async move {
                        let result = store.search(&query).await.map_err(|err| err.to_string());
                        let _ = ui_tx.send(UiEvent::SessionsFound { query, result });
                    });
                }
                Effect::Resume(id) => {
                    // The running session is saved as it goes: ending it loses nothing.
                    let ready =
                        session.as_ref().map(|s| (s.brain.clone(), s.account.clone())).or_else(|| waiting.clone());
                    match (ready, &workplace) {
                        (Some((brain, account)), Some(place)) => {
                            tracing::info!("resuming a stored session");
                            link.generation.advance();
                            if let Some(old) = session.take() {
                                end_session(old);
                            }
                            waiting = None;
                            continue_latest = false;
                            app.start_over();
                            app.note(place_note(place, &workspace));
                            session = Some(start_session(
                                brain,
                                account,
                                workspace.clone(),
                                place,
                                store.clone(),
                                &link,
                                Some(id),
                            ));
                        }
                        _ => app.note("Set up a model first (ctrl-a); then the session can be resumed."),
                    }
                }
                Effect::SetNetwork(policy) => match &workplace {
                    Some(place) => {
                        let (sandbox, ui_tx) = (place.sandbox.clone(), ui_tx.clone());
                        tokio::spawn(async move {
                            let result =
                                sandbox.set_network(policy.clone()).await.map(|()| policy).map_err(|e| e.to_string());
                            let _ = ui_tx.send(UiEvent::NetworkSet(result));
                        });
                    }
                    None => produced_now(&mut app, UiEvent::NetworkSet(Err("choose where to work first".into()))),
                },
                Effect::UndoDiff(checkpoint) => match &workplace {
                    Some(place) => {
                        let (sandbox, ui_tx) = (place.sandbox.clone(), ui_tx.clone());
                        tokio::spawn(async move {
                            let result = sandbox.diff(&checkpoint, None).await.map_err(|err| err.to_string());
                            let _ = ui_tx.send(UiEvent::UndoDiff { checkpoint, result });
                        });
                    }
                    None => produced_now(
                        &mut app,
                        UiEvent::UndoDiff { checkpoint, result: Err("choose where to work first".into()) },
                    ),
                },
                Effect::OpenTerminal(size) => match &workplace {
                    Some(place) => {
                        if let Some(old) = shell.take() {
                            old.task.abort();
                        }
                        opened += 1;
                        let (ops, rx) = mpsc::unbounded_channel();
                        let task = tokio::spawn(run_terminal(place.sandbox.clone(), size, opened, term_tx.clone(), rx));
                        shell = Some(TermLink { number: opened, ops, task });
                    }
                    None => {
                        let _ = app.terminal_opened(Err("choose where to work first".into()));
                    }
                },
                Effect::TerminalInput(bytes) => {
                    if let Some(link) = &shell {
                        let _ = link.ops.send(TermOp::Write(bytes));
                    }
                }
                Effect::Quit => quit = true,
            }
        }
        if quit {
            break Ok(());
        }
    };

    // The terminal ends with Nixxa.
    if let Some(link) = shell.take() {
        link.task.abort();
        let _ = link.task.await;
    }
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
    let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste, DisableFocusChange);
    ratatui::restore();
    if let Some(copy) = workplace.as_ref().and_then(|w| w.copy.as_ref()) {
        println!("nixxa: the copy the agent worked on is at {}", copy.display());
    }
    match &result {
        Ok(()) => tracing::info!("nixxa quit"),
        Err(err) => tracing::error!(%err, "terminal error"),
    }
    result
}

/// Forwards the sandbox's events (network requests) to the UI loop for as long as it runs.
fn watch_sandbox(place: &Workplace, ui_tx: &mpsc::UnboundedSender<UiEvent>) {
    let mut events = place.sandbox.subscribe();
    let ui_tx = ui_tx.clone();
    tokio::spawn(async move {
        while let Some(event) = events.next().await {
            if ui_tx.send(UiEvent::Sandbox(event)).is_err() {
                return;
            }
        }
    });
}

/// Applies an event at once, outside the event branch (its effects are only notes here).
fn produced_now(app: &mut App, event: UiEvent) {
    let _ = receive(app, event);
}

/// Applies an event from the controller side; returns what it asks for in turn.
fn receive(app: &mut App, event: UiEvent) -> Vec<Effect> {
    match event {
        UiEvent::Controller(event) => {
            logging::event(&event);
            app.apply(event);
        }
        UiEvent::Approval(prompt) => app.ask(prompt),
        UiEvent::AutoApproved(id) => app.auto_approved(&id),
        UiEvent::AccountSaved(result) => app.account_saved(&result),
        UiEvent::ModelsListed(result) => app.models_listed(result),
        UiEvent::SessionsListed(result) => app.sessions_listed(result),
        UiEvent::SessionsFound { query, result } => app.sessions_found(&query, result),
        UiEvent::NetworkSet(result) => return app.network_set(result),
        UiEvent::Sandbox(event) => app.sandbox_event(event),
        UiEvent::UndoDiff { checkpoint, result } => app.undo_diff(&checkpoint, result),
        // Handled in the loop, which holds the sign-in state.
        UiEvent::SignInStarted(_) | UiEvent::SignedIn(_) => {}
    }
    Vec::new()
}
