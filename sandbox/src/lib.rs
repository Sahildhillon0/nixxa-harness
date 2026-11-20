//! `nixxa-sandbox` — executes actions and returns observations.
//!
//! Backends: [`fake::FakeSandbox`] (in memory, the "sandbox" choice until the isolated runtime is
//! wired in), [`host::HostSandbox`] (the real files of one directory, not isolated, for "work in
//! this directory") and, on Linux, [`bwrap::BwrapSandbox`] (`nixxa-sandboxd` out of process under
//! bubblewrap; not wired into the harness yet).

#[cfg(target_os = "linux")]
pub mod bwrap;
pub mod fake;
pub mod host;
#[cfg(target_os = "linux")]
mod protocol;
