//! The terminal loop: owns the terminal, feeds keys and controller events into [`App`], draws.

use std::io;
use std::time::Duration;

use crossterm::event::{Event as TermEvent, EventStream};
use futures::StreamExt;
use nixxa_controller::Command;
use tokio::sync::mpsc;

use crate::app::{App, Effect};
use crate::ports::UiEvent;
use crate::{logging, paths, ui, wiring};

/// How long quitting waits for the controller to end the session.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// Runs the TUI until the user quits.
pub(crate) async fn run() -> io::Result<()> {
    let root = std::env::current_dir()?;
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let mut app = App::new(color);
    let log_filter = std::env::var("NIXXA_LOG").ok();
    match logging::init(paths::log_dir(), log_filter.as_deref()) {
        Ok(dir) => tracing::info!(version = env!("CARGO_PKG_VERSION"), logs = %dir.display(), "nixxa starting"),
        Err(why) => app.note(format!("logging is off: {why}")),
    }

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let mut controller = wiring::controller(wiring::workspace(&root), ui_tx.clone());
    let controller_task = tokio::spawn(async move {
        if let Err(err) = controller.start_session().await {
            let message = format!("could not start a session: {err}");
            let _ = ui_tx.send(UiEvent::Controller(nixxa_contracts::Event::Error { turn: None, message }));
            return;
        }
        controller.run(cmd_rx).await;
    });

    // `init` enters raw mode and the alternate screen and restores them on panic.
    let mut terminal = ratatui::init();
    let mut keys = EventStream::new();
    let result = loop {
        if let Err(err) = terminal.draw(|frame| ui::draw(frame, &mut app)) {
            break Err(err);
        }
        let effects = tokio::select! {
            input = keys.next() => match input {
                Some(Ok(TermEvent::Key(key))) => app.handle_key(key),
                Some(Ok(_)) => Vec::new(),
                Some(Err(err)) => break Err(err),
                None => break Ok(()),
            },
            Some(event) = ui_rx.recv() => {
                receive(&mut app, event);
                // Drain whatever else arrived, so a burst of deltas costs one redraw.
                while let Ok(event) = ui_rx.try_recv() {
                    receive(&mut app, event);
                }
                Vec::new()
            }
        };
        let mut quit = false;
        for effect in effects {
            match effect {
                // A closed channel means the session already ended; quitting still works.
                Effect::Send(command) => drop(cmd_tx.send(command).await),
                Effect::Quit => quit = true,
            }
        }
        if quit {
            break Ok(());
        }
    };

    let _ = cmd_tx.send(Command::Shutdown).await;
    if tokio::time::timeout(SHUTDOWN_GRACE, controller_task).await.is_err() {
        tracing::warn!("the controller did not end the session in time");
    }
    // Log what the controller said while shutting down (session ended).
    while let Ok(event) = ui_rx.try_recv() {
        if let UiEvent::Controller(event) = event {
            logging::event(&event);
        }
    }
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
    }
}
