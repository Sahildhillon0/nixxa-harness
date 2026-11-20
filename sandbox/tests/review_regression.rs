mod common;
use common::*;
use nixxa_contracts::{Outcome, Sandbox, SandboxAction};
use std::task::{Context, Waker};
use std::time::Instant;

#[tokio::test]
async fn proc2_fd_forgery_fails() {
    let scratch = Scratch::new("proc2fd");
    let Some(sb) = sandbox(scratch.workspace().as_path()) else { return };
    let (code, stdout, stderr) = sh(&sb, "printf 'FORGED' > /proc/2/fd/1 2>&1; echo \"write_exit=$?\"").await;
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("Permission denied") || combined.contains("Operation not permitted"),
        "write to /proc/2/fd/1 should be refused: code={code:?} out={combined:?}"
    );
    let (code, stdout, stderr) = sh(&sb, "cat /proc/2/fd/0 2>&1; echo \"read_exit=$?\"").await;
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("Permission denied") || combined.contains("Operation not permitted"),
        "read of /proc/2/fd/0 should be refused: code={code:?} out={combined:?}"
    );
    let (code, stdout, _) = sh(&sb, "echo nixxa").await;
    assert_eq!((code, stdout.as_str()), (Some(0), "nixxa\n"));
}

#[tokio::test]
async fn fifo_read_and_write_fail_fast() {
    let scratch = Scratch::new("fifo");
    let Some(sb) = sandbox(scratch.workspace().as_path()) else { return };
    let _ = run(&sb, SandboxAction::Exec { command: "mkfifo f".into(), cwd: None, timeout_ms: Some(5000) }).await;
    let started = Instant::now();
    let obs = run(&sb, SandboxAction::ReadFile { path: "/workspace/f".into() }).await.unwrap();
    assert!(started.elapsed().as_millis() < 1000, "read stalled: {:?}", started.elapsed());
    assert!(matches!(obs.outcome, Outcome::Failed { .. }), "{:?}", obs.outcome);
    let started = Instant::now();
    let obs = run(&sb, SandboxAction::WriteFile { path: "/workspace/f".into(), content: "x".into() }).await.unwrap();
    assert!(started.elapsed().as_millis() < 1000, "write stalled: {:?}", started.elapsed());
    assert!(matches!(obs.outcome, Outcome::Failed { .. }), "{:?}", obs.outcome);
}

#[tokio::test]
async fn mid_send_drop_recovers() {
    let scratch = Scratch::new("mid");
    let Some(sb) = sandbox(scratch.workspace().as_path()) else { return };
    let big = "x".repeat(1000 * 1024);
    let fut = sb.execute(next_id(), SandboxAction::WriteFile { path: "/workspace/big.txt".into(), content: big });
    let mut fut = Box::pin(fut);
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let _ = fut.as_mut().poll(&mut cx);
    drop(fut);
    let started = Instant::now();
    let (code, stdout, _) = sh(&sb, "echo recovered").await;
    assert_eq!((code, stdout.trim()), (Some(0), "recovered"));
    assert!(started.elapsed().as_secs() < 2, "hung: {:?}", started.elapsed());
}
