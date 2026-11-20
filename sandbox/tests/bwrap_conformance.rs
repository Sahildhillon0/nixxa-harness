//! The contract's conformance suite against the real Linux sandbox.

#![cfg(target_os = "linux")]

mod common;

use std::sync::Mutex;

use nixxa_sandbox::bwrap::BwrapSandbox;

#[tokio::test]
async fn bwrap_sandbox_conforms() {
    // Every scenario gets a fresh, empty workspace; they are all removed at the end.
    let scratches = Mutex::new(Vec::new());
    let probe = common::Scratch::new("conformance-probe");
    if common::sandbox(&probe.workspace()).is_none() {
        return;
    }
    nixxa_contracts::conformance::sandbox::run(|| -> BwrapSandbox {
        let scratch = common::Scratch::new("conformance");
        let sandbox = common::sandbox(&scratch.workspace()).expect("checked above");
        scratches.lock().unwrap().push(scratch);
        sandbox
    })
    .await;
}
