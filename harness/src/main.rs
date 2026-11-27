//! # nixxa
//!
//! The harness: the TUI and the composition root. Claude, local or OpenRouter models with keys
//! in the OS keychain (or `--fake` for the scripted Stage 2 brain), the sandbox chosen at start-up,
//! and sessions kept in SQLite; knowledge is still a fake. See ARCHITECTURE.md § Stages.

mod app;
mod browser;
#[cfg(target_os = "linux")]
mod copy;
mod logging;
mod network;
mod notify;
mod paths;
mod place;
mod ports;
mod safe;
mod sessions;
mod settings;
mod setup;
mod terminal;
mod tui;
mod ui;
mod undo;
mod wiring;
mod workplace;

use std::process::ExitCode;

use settings::Parsed;

fn main() -> ExitCode {
    // Before the TUI takes the terminal, printing is fine.
    let settings = match settings::parse(std::env::args().skip(1), &|name| std::env::var(name).ok()) {
        Ok(Parsed::Run(settings)) => settings,
        Ok(Parsed::Help) => {
            println!("{}", settings::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("nixxa {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(message) => {
            eprintln!("nixxa: {message}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("nixxa: could not start: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(tui::run(settings)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("nixxa: {err}");
            ExitCode::FAILURE
        }
    }
}
