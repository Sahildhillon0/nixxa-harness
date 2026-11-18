//! # nixxa
//!
//! The harness: the TUI and the composition root. Stage 2 wires every module to its fake —
//! scripted brain, echo sandbox, in-memory store, fake knowledge — so the whole loop runs end
//! to end. See ARCHITECTURE.md § Stages.

mod app;
mod logging;
mod paths;
mod ports;
mod safe;
mod tui;
mod ui;
mod wiring;

fn main() -> std::io::Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(tui::run())
}
