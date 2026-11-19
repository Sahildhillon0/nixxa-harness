//! `nixxa-sandbox` — executes actions and returns observations.
//!
//! Two backends so far: [`fake::FakeSandbox`] (in memory, the "sandbox" choice until the isolated
//! runtime arrives in Stage 4) and [`host::HostSandbox`] (the real files of one directory, not
//! isolated, for "work in this directory").

pub mod fake;
pub mod host;
