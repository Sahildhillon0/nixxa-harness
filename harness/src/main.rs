//! # nixxa
//!
//! The harness: the TUI and the composition root. Stage 3 runs Claude with keys in the OS
//! keychain (or `--fake` for the scripted Stage 2 brain); the sandbox, store and knowledge are
//! still fakes. See ARCHITECTURE.md § Stages.

mod app;
mod logging;
mod paths;
mod ports;
mod safe;
mod settings;
mod setup;
mod tui;
mod ui;
mod wiring;

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
