//! Behaviour of `BwrapSandbox` beyond the conformance suite: real files, cancellation, reset,
//! daemon death and the configured limits. Linux only; skips loudly without bubblewrap.

#![cfg(target_os = "linux")]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Scratch, exec, run, sh, sh_out};
use nixxa_contracts::{Outcome, Sandbox, SandboxAction, SandboxActionKind, SandboxError};
use nixxa_sandbox::bwrap::{BwrapConfig, BwrapSandbox};

#[tokio::test]
async fn it_describes_itself_as_isolated() {
    let scratch = Scratch::new("caps");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    sb.health().await.unwrap();
    let caps = sb.capabilities();
    eprintln!("backend: {} (cgroup scope: {})", caps.backend, sb.has_cgroup());
    assert!(caps.backend.starts_with("bubblewrap"), "{}", caps.backend);
    assert_eq!(caps.backend == "bubblewrap", sb.has_cgroup(), "the name must say when there is no cgroup");
    assert!(caps.isolated, "isolated once Landlock and seccomp are applied");
    assert!(!caps.network);
    assert_eq!(caps.actions, SandboxActionKind::ALL.to_vec());
}

#[tokio::test]
async fn a_missing_bwrap_is_unavailable() {
    let scratch = Scratch::new("nobwrap");
    let mut config = common::config(&scratch.workspace());
    config.bwrap = "nixxa-no-such-bwrap".into();
    let err = BwrapSandbox::new(config).unwrap_err();
    assert!(matches!(&err, SandboxError::Unavailable(why) if why.contains("not installed")), "{err:?}");
    let config = BwrapConfig::new(scratch.base.join("missing"), env!("CARGO_BIN_EXE_nixxa-sandboxd"));
    assert!(matches!(BwrapSandbox::new(config), Err(SandboxError::Unavailable(_))));
}

#[tokio::test]
async fn files_are_shared_with_the_workspace_on_the_host() {
    let scratch = Scratch::new("files");
    let ws = scratch.workspace();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/host.txt"), "from the host").unwrap();
    let Some(sb) = common::sandbox(&ws) else { return };
    assert_eq!(sh(&sb, "cat src/host.txt").await.1, "from the host");
    sh(&sb, "mkdir -p out && echo from inside > out/inside.txt").await;
    assert_eq!(std::fs::read_to_string(ws.join("out/inside.txt")).unwrap(), "from inside\n");
    let obs = run(
        &sb,
        SandboxAction::Exec { command: "pwd; ls".into(), cwd: Some("src".into()), timeout_ms: None, background: false },
    )
    .await
    .unwrap();
    assert_eq!(
        obs.outcome,
        Outcome::Exec { exit_code: Some(0), stdout: "/workspace/src\nhost.txt\n".into(), stderr: String::new() }
    );
    let (code, _, stderr) = sh(&sb, "echo oops >&2; exit 3").await;
    assert_eq!((code, stderr.as_str()), (Some(3), "oops\n"));
    let listing = run(&sb, SandboxAction::ListDir { path: ".".into() }).await.unwrap();
    match listing.outcome {
        Outcome::Listing { entries } => {
            let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["out", "src"]);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn dropping_an_exec_kills_its_command() {
    let scratch = Scratch::new("drop");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    sb.health().await.unwrap();
    let slow = run(&sb, exec("sleep 300 & sleep 300; wait"));
    assert!(tokio::time::timeout(Duration::from_millis(500), slow).await.is_err(), "it should still be running");
    let started = Instant::now();
    assert_eq!(sh_out(&sb, "pgrep -c sleep || true").await.trim(), "0", "the dropped command is still running");
    assert!(started.elapsed() < Duration::from_secs(5), "cancelling took {:?}", started.elapsed());
}

#[tokio::test]
async fn reset_stops_a_running_command_and_keeps_the_workspace() {
    let scratch = Scratch::new("reset");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let sb = Arc::new(sb);
    sh(&sb, "echo kept > keep.txt").await;
    let running = tokio::spawn({
        let sb = Arc::clone(&sb);
        async move { run(&sb, exec("sleep 300")).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let started = Instant::now();
    sb.reset().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(5), "reset waited for the command: {:?}", started.elapsed());
    let stopped = running.await.unwrap().unwrap();
    assert!(matches!(stopped.outcome, Outcome::Failed { .. }), "{stopped:?}");
    assert_eq!(sh_out(&sb, "pgrep -c sleep || true").await.trim(), "0");
    assert_eq!(sh(&sb, "cat keep.txt").await.1, "kept\n");
    assert_eq!(std::fs::read_to_string(scratch.workspace().join("keep.txt")).unwrap(), "kept\n");
}

#[tokio::test]
async fn a_dead_daemon_is_unavailable_then_restarted() {
    let scratch = Scratch::new("death");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    // PID 2 inside is the daemon (bubblewrap's init is PID 1).
    assert_eq!(sh(&sb, "cat /proc/2/comm").await.1.trim(), "nixxa-sandboxd");
    let err = run(&sb, exec("kill -9 2; sleep 5")).await.unwrap_err();
    assert!(matches!(err, SandboxError::Unavailable(_)), "{err:?}");
    assert_eq!(sh(&sb, "echo back").await.1, "back\n");
    sb.health().await.unwrap();
}

#[tokio::test]
async fn timeouts_are_capped_by_the_configured_maximum() {
    let scratch = Scratch::new("maxtime");
    let mut config = common::config(&scratch.workspace());
    config.max_timeout = Duration::from_secs(1);
    let Some(sb) = common::sandbox_with(config) else { return };
    let started = Instant::now();
    let action =
        SandboxAction::Exec { command: "sleep 30".into(), cwd: None, timeout_ms: Some(100_000), background: false };
    assert_eq!(run(&sb, action).await.unwrap_err(), SandboxError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
}

#[tokio::test]
async fn background_processes_do_not_outlive_their_command() {
    let scratch = Scratch::new("background");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let started = Instant::now();
    let (code, stdout, _) = sh(&sb, "setsid sleep 300 & echo started").await;
    assert_eq!((code, stdout.as_str()), (Some(0), "started\n"));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(sh_out(&sb, "pgrep -c sleep || true").await.trim(), "0");
}

#[tokio::test]
async fn rlimits_apply_to_commands() {
    let scratch = Scratch::new("rlimits");
    let config = common::config(&scratch.workspace());
    let (pids, files) = (config.pids_max, config.open_files_max);
    let Some(sb) = common::sandbox_with(config) else { return };
    let out = sh_out(&sb, "bash -c 'ulimit -u; ulimit -n'").await;
    assert_eq!(out, format!("{pids}\n{files}\n"));
    let (code, _, _) = sh(&sb, &format!("ulimit -n {}", files + 1)).await;
    assert_ne!(code, Some(0), "a command must not raise its own limits");
}
