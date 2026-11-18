//! The terminal loop: owns the terminal, feeds keys and controller events into [`App`], draws.

use std::io;
use std::time::Duration;

use crossterm::event::{Event as TermEvent, EventStream};
use futures::StreamExt;
use nixxa_controller::Command;
use tokio::sync::mpsc;

use crate::app::{App, Effect};
use crate::ports::UiEvent;
use crate::{ui, wiring};

/// How long quitting waits for the controller to end the session.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

/// Runs the TUI until the user quits.
pub(crate) async fn run() -> io::Result<()> {
    let root = std::env::current_dir()?;
    let color = std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());

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
    let mut app = App::new(color);
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
    let _ = tokio::time::timeout(SHUTDOWN_GRACE, controller_task).await;
    ratatui::restore();
    result
}

fn receive(app: &mut App, event: UiEvent) {
    match event {
        UiEvent::Controller(event) => app.apply(event),
        UiEvent::Approval(prompt) => app.ask(prompt),
    }
}
