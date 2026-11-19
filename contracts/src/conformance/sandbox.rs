//! Conformance suite for [`Sandbox`].

use super::id;
use crate::{Outcome, Sandbox, SandboxAction, SandboxActionKind, SandboxError};

fn sample(kind: SandboxActionKind) -> SandboxAction {
    match kind {
        SandboxActionKind::Exec => {
            SandboxAction::Exec { command: "echo nixxa".into(), cwd: None, timeout_ms: Some(5_000) }
        }
        SandboxActionKind::ReadFile => SandboxAction::ReadFile { path: "conformance/hello.txt".into() },
        SandboxActionKind::WriteFile => {
            SandboxAction::WriteFile { path: "conformance/hello.txt".into(), content: "hello nixxa\n".into() }
        }
        SandboxActionKind::ListDir => SandboxAction::ListDir { path: "conformance".into() },
    }
}

/// Runs every scenario, each on a fresh sandbox (with an empty workspace) from `fresh`.
pub async fn run<S: Sandbox, F: Fn() -> S>(fresh: F) {
    describes_itself(&fresh()).await;
    unsupported_kinds_error(&fresh()).await;
    ids_are_echoed(&fresh()).await;
    write_read_list(&fresh()).await;
    escapes_never_succeed(&fresh()).await;
    reset_keeps_the_workspace(&fresh()).await;
    isolated_exec_really_runs(&fresh()).await;
    dropped_execs_do_not_wedge(&fresh()).await;
}

async fn describes_itself(sandbox: &dyn Sandbox) {
    let caps = sandbox.capabilities();
    assert!(!caps.backend.is_empty(), "backend name must be set");
    assert!(!caps.actions.is_empty(), "a sandbox must support at least one action");
    sandbox.health().await.expect("health");
    sandbox.reset().await.expect("reset");
}

async fn unsupported_kinds_error(sandbox: &dyn Sandbox) {
    let caps = sandbox.capabilities();
    for (n, kind) in SandboxActionKind::ALL.into_iter().enumerate() {
        if !caps.actions.contains(&kind) {
            let r = sandbox.execute(id(n as u32), sample(kind)).await;
            assert!(
                matches!(r, Err(SandboxError::Unsupported(_))),
                "{kind:?} is unsupported, expected Unsupported, got {r:?}"
            );
        }
    }
}

async fn ids_are_echoed(sandbox: &dyn Sandbox) {
    let caps = sandbox.capabilities();
    for (n, kind) in caps.actions.iter().copied().enumerate() {
        let want = id(100 + n as u32);
        if let Ok(obs) = sandbox.execute(want.clone(), sample(kind)).await {
            assert_eq!(obs.action_id, want, "{kind:?}: observation must carry the request id");
        }
    }
}

async fn write_read_list(sandbox: &dyn Sandbox) {
    let caps = sandbox.capabilities();
    let has = |k| caps.actions.contains(&k);
    if !(has(SandboxActionKind::WriteFile) && has(SandboxActionKind::ReadFile)) {
        return;
    }
    let w = sandbox.execute(id(1), sample(SandboxActionKind::WriteFile)).await.expect("write");
    assert!(matches!(w.outcome, Outcome::Written { .. }), "write: {w:?}");
    let r = sandbox.execute(id(2), sample(SandboxActionKind::ReadFile)).await.expect("read");
    assert_eq!(r.outcome, Outcome::Text { content: "hello nixxa\n".into() }, "read must return what was written");
    if has(SandboxActionKind::ListDir) {
        let l = sandbox.execute(id(3), sample(SandboxActionKind::ListDir)).await.expect("list");
        match l.outcome {
            Outcome::Listing { entries } => {
                assert!(entries.iter().any(|e| e.name == "hello.txt" && !e.is_dir), "listing must include the file")
            }
            other => panic!("list: expected Listing, got {other:?}"),
        }
    }
}

async fn escapes_never_succeed(sandbox: &dyn Sandbox) {
    let caps = sandbox.capabilities();
    let attempts = [
        SandboxAction::ReadFile { path: "../outside.txt".into() },
        SandboxAction::ReadFile { path: "/etc/hostname".into() },
        SandboxAction::WriteFile { path: "../escape.txt".into(), content: "x".into() },
        SandboxAction::ListDir { path: "..".into() },
    ];
    for (n, action) in attempts.into_iter().enumerate() {
        if !caps.actions.contains(&action.kind()) {
            continue;
        }
        let shown = format!("{action:?}");
        match sandbox.execute(id(200 + n as u32), action).await {
            Err(_) => {}
            Ok(obs) => assert!(
                matches!(obs.outcome, Outcome::Failed { .. } | Outcome::Denied { .. }),
                "{shown} escaped the workspace: {obs:?}"
            ),
        }
    }
}

async fn reset_keeps_the_workspace(sandbox: &dyn Sandbox) {
    let caps = sandbox.capabilities();
    let has = |k| caps.actions.contains(&k);
    if !(has(SandboxActionKind::WriteFile) && has(SandboxActionKind::ReadFile)) {
        return;
    }
    let w = sandbox.execute(id(300), sample(SandboxActionKind::WriteFile)).await.expect("write");
    assert!(matches!(w.outcome, Outcome::Written { .. }), "write: {w:?}");
    sandbox.reset().await.expect("reset");
    let r = sandbox.execute(id(301), sample(SandboxActionKind::ReadFile)).await.expect("read after reset");
    assert_eq!(r.outcome, Outcome::Text { content: "hello nixxa\n".into() }, "reset must keep the workspace");
}

async fn isolated_exec_really_runs(sandbox: &dyn Sandbox) {
    let caps = sandbox.capabilities();
    if !(caps.isolated && caps.actions.contains(&SandboxActionKind::Exec)) {
        return;
    }
    let o = sandbox.execute(id(400), sample(SandboxActionKind::Exec)).await.expect("exec");
    match o.outcome {
        Outcome::Exec { exit_code, stdout, .. } => {
            assert_eq!(exit_code, Some(0), "an isolated sandbox must really run `echo nixxa`");
            assert_eq!(stdout.trim_end(), "nixxa", "an isolated sandbox must really run `echo nixxa`");
        }
        other => panic!("an isolated sandbox must really run `echo nixxa`, got {other:?}"),
    }
}

async fn dropped_execs_do_not_wedge(sandbox: &dyn Sandbox) {
    let caps = sandbox.capabilities();
    if !caps.actions.contains(&SandboxActionKind::Exec) {
        return;
    }
    sandbox.health().await.expect("health");
    let slow = SandboxAction::Exec { command: "sleep 30".into(), cwd: None, timeout_ms: Some(60_000) };
    // Poll once, so the command is in flight, then drop it: the contract says that cancels it.
    let _ = futures::FutureExt::now_or_never(sandbox.execute(id(500), slow));
    let want = id(501);
    let next = sandbox.execute(want.clone(), sample(SandboxActionKind::Exec)).await;
    match next {
        Ok(obs) => assert_eq!(obs.action_id, want, "the call after a dropped execute must answer its own id"),
        Err(err) => panic!("dropping an in-flight execute wedged the sandbox: {err:?}"),
    }
}
