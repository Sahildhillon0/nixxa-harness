//! Conformance suite for [`Sandbox`].

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use futures::future::Either;
use futures::stream::BoxStream;

use super::id;
use crate::{
    CheckpointId, DomainRule, Edit, EntryState, JobId, NetworkAccess, NetworkPolicy, OpenTerminal, Outcome, Sandbox,
    SandboxAction, SandboxActionKind, SandboxError, SandboxEvent, TerminalEvent, TerminalSize, WorkspaceDiff,
};

/// The job kinds a backend with jobs must list.
const JOB_KINDS: [SandboxActionKind; 3] =
    [SandboxActionKind::JobStatus, SandboxActionKind::JobOutput, SandboxActionKind::JobStop];
/// The SHA-256 of `hello nixxa\n`, the sample file's contents.
const HELLO_HASH: &str = "cb6112f1c4749bb872768a2125c60119597c2a657661a3618dc11c0a1f68e685";
/// How long a job's `JobFinished` may take to arrive.
const JOB_PATIENCE: Duration = Duration::from_secs(20);

fn sample(kind: SandboxActionKind) -> SandboxAction {
    match kind {
        SandboxActionKind::Exec => {
            SandboxAction::Exec { command: "echo nixxa".into(), cwd: None, timeout_ms: Some(5_000), background: false }
        }
        SandboxActionKind::ReadFile => SandboxAction::ReadFile { path: "conformance/hello.txt".into() },
        SandboxActionKind::WriteFile => {
            SandboxAction::WriteFile { path: "conformance/hello.txt".into(), content: "hello nixxa\n".into() }
        }
        SandboxActionKind::Patch => SandboxAction::Patch {
            path: "conformance/hello.txt".into(),
            edits: vec![Edit { search: "hello".into(), replace: "hi".into() }],
            expected_hash: "0".repeat(64),
        },
        SandboxActionKind::ListDir => SandboxAction::ListDir { path: "conformance".into() },
        SandboxActionKind::JobStatus => SandboxAction::JobStatus { job: JobId::new("conformance") },
        SandboxActionKind::JobOutput => SandboxAction::JobOutput { job: JobId::new("conformance"), tail_bytes: 64 },
        SandboxActionKind::JobStop => SandboxAction::JobStop { job: JobId::new("conformance") },
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
    jobs_are_declared_consistently(&fresh()).await;
    background_jobs_report_their_end(&fresh()).await;
    unknown_jobs_fail(&fresh()).await;
    reset_stops_jobs(&fresh()).await;
    network_policy_is_reported(&fresh()).await;
    terminals_are_declared_consistently(&fresh()).await;
    terminal_echoes_and_reports_its_size(&fresh()).await;
    terminal_outlives_commands(&fresh()).await;
    closing_a_terminal_ends_it(&fresh()).await;
    dropping_a_terminal_does_not_wedge(&fresh()).await;
    reset_ends_the_terminal(&fresh()).await;
    patches_apply_exactly(&fresh()).await;
    patches_refuse_symlinks(&fresh()).await;
    checkpoints_are_declared_consistently(&fresh()).await;
    checkpoints_restore_writes_and_patches(&fresh()).await;
    checkpoints_restore_what_commands_did(&fresh()).await;
    older_checkpoints_restore_after_newer(&fresh()).await;
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
    assert_eq!(
        r.outcome,
        Outcome::File { content: "hello nixxa\n".into(), hash: HELLO_HASH.into() },
        "read must return what was written, with the SHA-256 of its bytes"
    );
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
        SandboxAction::Patch {
            path: "../escape.txt".into(),
            edits: vec![Edit { search: "x".into(), replace: "y".into() }],
            expected_hash: "0".repeat(64),
        },
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
    assert!(
        matches!(&r.outcome, Outcome::File { content, .. } if content == "hello nixxa\n"),
        "reset must keep the workspace: {r:?}"
    );
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
    let slow =
        SandboxAction::Exec { command: "sleep 30".into(), cwd: None, timeout_ms: Some(60_000), background: false };
    // Poll once, so the command is in flight, then drop it: the contract says that cancels it.
    let _ = futures::FutureExt::now_or_never(sandbox.execute(id(500), slow));
    let want = id(501);
    let next = sandbox.execute(want.clone(), sample(SandboxActionKind::Exec)).await;
    match next {
        Ok(obs) => assert_eq!(obs.action_id, want, "the call after a dropped execute must answer its own id"),
        Err(err) => panic!("dropping an in-flight execute wedged the sandbox: {err:?}"),
    }
}

fn background(command: &str) -> SandboxAction {
    SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: Some(60_000), background: true }
}

async fn jobs_are_declared_consistently(sandbox: &dyn Sandbox) {
    let caps = sandbox.capabilities();
    if caps.jobs {
        for kind in JOB_KINDS.into_iter().chain([SandboxActionKind::Exec]) {
            assert!(caps.actions.contains(&kind), "a backend with jobs must list {kind:?}");
        }
        return;
    }
    for kind in JOB_KINDS {
        assert!(!caps.actions.contains(&kind), "a backend without jobs must not list {kind:?}");
    }
    if caps.actions.contains(&SandboxActionKind::Exec) {
        let r = sandbox.execute(id(600), background("echo nixxa")).await;
        assert!(
            matches!(r, Err(SandboxError::Unsupported(_))),
            "without jobs, a background Exec must be Unsupported, got {r:?}"
        );
    }
}

/// Starts a background job and returns its id.
async fn start(sandbox: &dyn Sandbox, n: u32, command: &str) -> JobId {
    let obs = sandbox.execute(id(n), background(command)).await.expect("start a background job");
    match obs.outcome {
        Outcome::Started { job } => job,
        other => panic!("a background Exec must return Started at once, got {other:?}"),
    }
}

/// Resolves after `after`, whatever executor polls it.
fn timer(after: Duration) -> futures::channel::oneshot::Receiver<()> {
    let (tx, rx) = futures::channel::oneshot::channel();
    std::thread::spawn(move || {
        std::thread::sleep(after);
        let _ = tx.send(());
    });
    rx
}

/// Reads `events` until every job in `want` has a `JobFinished`; returns their exit codes.
/// Panics with `rule` if one doesn't arrive in time.
async fn wait_finished(
    events: &mut BoxStream<'static, SandboxEvent>,
    want: &[JobId],
    rule: &str,
) -> HashMap<JobId, Option<i32>> {
    let mut deadline = timer(JOB_PATIENCE);
    let mut got = HashMap::new();
    while got.len() < want.len() {
        match futures::future::select(events.next(), &mut deadline).await {
            Either::Left((Some(SandboxEvent::JobFinished { job, exit_code, status, .. }), _)) => {
                if want.contains(&job) {
                    assert!(
                        status.contains(&format!("job {job} ")),
                        "JobFinished must say how the job ended in words naming it, got {status:?}"
                    );
                    let first = got.insert(job.clone(), exit_code).is_none();
                    assert!(first, "job {job} finished twice: a job ends with exactly one JobFinished");
                }
            }
            Either::Left((Some(_), _)) => {}
            Either::Left((None, _)) => panic!("the event stream ended early: {rule}"),
            Either::Right(_) => {
                let missing: Vec<_> = want.iter().filter(|job| !got.contains_key(*job)).collect();
                panic!("no JobFinished for {missing:?}: {rule}")
            }
        }
    }
    got
}

async fn background_jobs_report_their_end(sandbox: &dyn Sandbox) {
    if !sandbox.capabilities().jobs {
        return;
    }
    let mut events = sandbox.subscribe();
    // `&&` means the same in `sh` and Windows `cmd`.
    let failing = start(sandbox, 610, "echo nixxa && exit 3").await;
    let passing = start(sandbox, 611, "echo nixxa").await;
    assert_ne!(failing, passing, "job ids must be unique");
    let both = [failing.clone(), passing.clone()];
    let ended = wait_finished(&mut events, &both, "a backend with jobs must report each job's end").await;
    assert_eq!(ended[&failing], Some(3), "JobFinished must carry the shell's exit code");
    assert_eq!(ended[&passing], Some(0), "JobFinished must carry the shell's exit code");
    let output = sandbox
        .execute(id(612), SandboxAction::JobOutput { job: failing.clone(), tail_bytes: 4096 })
        .await
        .expect("job output");
    match output.outcome {
        Outcome::Text { content } => assert!(content.contains("nixxa"), "job output must include what it printed"),
        other => panic!("job output must include what it printed, got {other:?}"),
    }
    let status = sandbox.execute(id(613), SandboxAction::JobStatus { job: failing }).await.expect("job status");
    assert!(
        !matches!(status.outcome, Outcome::Failed { .. } | Outcome::Denied { .. }),
        "the status of a job this sandbox started must succeed, got {:?}",
        status.outcome
    );
}

async fn unknown_jobs_fail(sandbox: &dyn Sandbox) {
    if !sandbox.capabilities().jobs {
        return;
    }
    let ghost = JobId::new("never-started");
    let actions = [
        SandboxAction::JobStatus { job: ghost.clone() },
        SandboxAction::JobOutput { job: ghost.clone(), tail_bytes: 64 },
        SandboxAction::JobStop { job: ghost },
    ];
    for (n, action) in actions.into_iter().enumerate() {
        let shown = format!("{action:?}");
        if let Ok(obs) = sandbox.execute(id(620 + n as u32), action).await {
            assert!(
                matches!(obs.outcome, Outcome::Failed { .. } | Outcome::Denied { .. }),
                "{shown}: a job the sandbox never started must not succeed, got {:?}",
                obs.outcome
            );
        }
    }
}

async fn reset_stops_jobs(sandbox: &dyn Sandbox) {
    if !sandbox.capabilities().jobs {
        return;
    }
    let mut events = sandbox.subscribe();
    // Far longer than JOB_PATIENCE: only a reset ends it in time.
    let job = start(sandbox, 630, "sleep 300").await;
    sandbox.reset().await.expect("reset");
    wait_finished(&mut events, std::slice::from_ref(&job), "reset must stop every job").await;
}

async fn network_policy_is_reported(sandbox: &dyn Sandbox) {
    let crates = DomainRule { host: "crates.io".into(), ports: Vec::new() };
    let allow = NetworkPolicy::Allow(vec![crates]);
    match sandbox.capabilities().network {
        NetworkAccess::Unrestricted => {
            let r = sandbox.set_network(allow).await;
            assert!(
                matches!(r, Err(SandboxError::Unsupported(_))),
                "a sandbox that can't restrict the network must refuse a policy, got {r:?}"
            );
        }
        NetworkAccess::Controlled(start) => {
            assert_eq!(start, NetworkPolicy::Off, "the network must start off");
            for policy in [allow, NetworkPolicy::Open, NetworkPolicy::Off] {
                sandbox.set_network(policy.clone()).await.expect("set the network policy");
                assert_eq!(
                    sandbox.capabilities().network,
                    NetworkAccess::Controlled(policy),
                    "capabilities must report the policy just set"
                );
            }
        }
    }
}

/// The size every terminal scenario opens with.
const SIZE: TerminalSize = TerminalSize { cols: 80, rows: 24 };
/// How long a terminal may take to answer.
const TERMINAL_PATIENCE: Duration = Duration::from_secs(20);

async fn open(sandbox: &dyn Sandbox, rule: &str) -> OpenTerminal {
    match sandbox.open_terminal(SIZE).await {
        Ok(open) => open,
        Err(err) => panic!("{rule}: open_terminal failed: {err:?}"),
    }
}

/// Reads `events` until the output so far contains `want`. Panics with `rule` if it doesn't
/// arrive in time or the terminal ends first.
async fn wait_output(events: &mut BoxStream<'static, TerminalEvent>, want: &str, rule: &str) {
    let mut deadline = timer(TERMINAL_PATIENCE);
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains(want) {
        match futures::future::select(events.next(), &mut deadline).await {
            Either::Left((Some(TerminalEvent::Output(bytes)), _)) => seen.extend(bytes),
            Either::Left((Some(TerminalEvent::Closed { reason }), _)) => {
                panic!("{rule}: the terminal ended ({reason}) before showing {want:?}")
            }
            Either::Left((None, _)) => panic!("{rule}: the terminal's events ended before showing {want:?}"),
            Either::Right(_) => {
                let tail = String::from_utf8_lossy(&seen);
                let tail: String = tail.chars().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect();
                panic!("{rule}: no {want:?} in the terminal's output (it ends {tail:?})")
            }
        }
    }
}

/// Reads `events` until [`TerminalEvent::Closed`] and the end of the stream; returns the reason.
async fn wait_closed(events: &mut BoxStream<'static, TerminalEvent>, rule: &str) -> String {
    let mut deadline = timer(TERMINAL_PATIENCE);
    let mut reason = None;
    loop {
        match futures::future::select(events.next(), &mut deadline).await {
            Either::Left((Some(TerminalEvent::Output(_)), _)) => {
                assert!(reason.is_none(), "{rule}: output came after Closed");
            }
            Either::Left((Some(TerminalEvent::Closed { reason: why }), _)) => {
                assert!(reason.is_none(), "{rule}: Closed came twice");
                assert!(!why.trim().is_empty(), "{rule}: Closed must say why");
                reason = Some(why);
            }
            Either::Left((None, _)) => match reason {
                Some(reason) => return reason,
                None => panic!("{rule}: the events ended without Closed"),
            },
            Either::Right(_) => panic!("{rule}: the terminal didn't end"),
        }
    }
}

async fn terminals_are_declared_consistently(sandbox: &dyn Sandbox) {
    if sandbox.capabilities().terminal {
        return;
    }
    let r = sandbox.open_terminal(SIZE).await;
    assert!(
        matches!(r, Err(SandboxError::Unsupported(_))),
        "without a terminal, open_terminal must be Unsupported, got {:?}",
        r.map(|_| "a terminal")
    );
}

async fn terminal_echoes_and_reports_its_size(sandbox: &dyn Sandbox) {
    if !sandbox.capabilities().terminal {
        return;
    }
    let rule = "a terminal must echo its input";
    let OpenTerminal { terminal, mut events } = open(sandbox, rule).await;
    terminal.write(b"echo nixxa-terminal\r").await.expect("write to the terminal");
    wait_output(&mut events, "nixxa-terminal", rule).await;
    terminal.write(b"stty size\r").await.expect("write to the terminal");
    wait_output(&mut events, "24 80", "`stty size` must report the terminal's size").await;
    terminal.resize(TerminalSize { cols: 100, rows: 30 }).await.expect("resize the terminal");
    terminal.write(b"stty size\r").await.expect("write to the terminal");
    wait_output(&mut events, "30 100", "`stty size` must report the size after a resize").await;
    terminal.close().await.expect("close the terminal");
    wait_closed(&mut events, "closing a terminal must end it").await;
}

async fn terminal_outlives_commands(sandbox: &dyn Sandbox) {
    let caps = sandbox.capabilities();
    if !(caps.terminal && caps.actions.contains(&SandboxActionKind::Exec)) {
        return;
    }
    let rule = "a terminal must outlive the agent's commands";
    let OpenTerminal { terminal, mut events } = open(sandbox, rule).await;
    sandbox.execute(id(700), sample(SandboxActionKind::Exec)).await.expect("exec");
    if caps.jobs {
        let mut jobs = sandbox.subscribe();
        let job = start(sandbox, 701, "echo nixxa").await;
        wait_finished(&mut jobs, std::slice::from_ref(&job), "a background job must report its end").await;
        let status = sandbox.execute(id(702), SandboxAction::JobStatus { job: JobId::new("terminal") }).await;
        if let Ok(obs) = status {
            assert!(
                matches!(obs.outcome, Outcome::Failed { .. } | Outcome::Denied { .. }),
                "the terminal must not be a job of the agent's, got {:?}",
                obs.outcome
            );
        }
    }
    terminal.write(b"echo still-here\r").await.unwrap_or_else(|err| panic!("{rule}: {err:?}"));
    wait_output(&mut events, "still-here", rule).await;
    terminal.close().await.expect("close the terminal");
    wait_closed(&mut events, "closing a terminal must end it").await;
}

async fn closing_a_terminal_ends_it(sandbox: &dyn Sandbox) {
    if !sandbox.capabilities().terminal {
        return;
    }
    let rule = "closing a terminal must end it";
    let OpenTerminal { terminal, mut events } = open(sandbox, rule).await;
    terminal.close().await.expect("close the terminal");
    wait_closed(&mut events, rule).await;
    assert!(terminal.write(b"echo late\r").await.is_err(), "a closed terminal's write must fail");
    assert!(terminal.resize(SIZE).await.is_err(), "a closed terminal's resize must fail");
    terminal.close().await.expect("closing a closed terminal does nothing");
}

async fn dropping_a_terminal_does_not_wedge(sandbox: &dyn Sandbox) {
    if !sandbox.capabilities().terminal {
        return;
    }
    let rule = "dropping a terminal's handle must end it";
    let OpenTerminal { terminal, mut events } = open(sandbox, rule).await;
    drop(terminal);
    wait_closed(&mut events, rule).await;
    // One at a time may be all a backend allows: the dropped one must not be in the way.
    let OpenTerminal { terminal, mut events } = open(sandbox, "dropping a terminal wedged the sandbox").await;
    terminal.write(b"echo again\r").await.expect("write to the terminal");
    wait_output(&mut events, "again", "a terminal opened after a dropped one must work").await;
    terminal.close().await.expect("close the terminal");
    wait_closed(&mut events, "closing a terminal must end it").await;
}

async fn reset_ends_the_terminal(sandbox: &dyn Sandbox) {
    if !sandbox.capabilities().terminal {
        return;
    }
    let OpenTerminal { terminal, mut events } = open(sandbox, "reset must end the terminal").await;
    sandbox.reset().await.expect("reset");
    wait_closed(&mut events, "reset must end the terminal").await;
    assert!(terminal.write(b"echo late\r").await.is_err(), "a terminal ended by reset must refuse input");
}

// ---- Patches and checkpoints ----

/// Whether the backend has everything a scenario needs.
fn lists(sandbox: &dyn Sandbox, kinds: &[SandboxActionKind]) -> bool {
    let caps = sandbox.capabilities();
    kinds.iter().all(|kind| caps.actions.contains(kind))
}

async fn write(sandbox: &dyn Sandbox, n: u32, path: &str, content: &str) {
    let action = SandboxAction::WriteFile { path: path.into(), content: content.into() };
    let o = sandbox.execute(id(n), action).await.expect("write");
    assert!(matches!(o.outcome, Outcome::Written { .. }), "write {path}: {o:?}");
}

/// The file's contents and hash, or `None` when the read fails.
async fn read(sandbox: &dyn Sandbox, n: u32, path: &str) -> Option<(String, String)> {
    let o = sandbox.execute(id(n), SandboxAction::ReadFile { path: path.into() }).await.expect("read");
    match o.outcome {
        Outcome::File { content, hash } => Some((content, hash)),
        Outcome::Failed { .. } => None,
        other => panic!("ReadFile must answer File or Failed, got {other:?}"),
    }
}

async fn patch(sandbox: &dyn Sandbox, n: u32, path: &str, edits: &[(&str, &str)], hash: &str) -> Outcome {
    let edits = edits.iter().map(|(search, replace)| Edit { search: (*search).into(), replace: (*replace).into() });
    let action = SandboxAction::Patch { path: path.into(), edits: edits.collect(), expected_hash: hash.into() };
    sandbox.execute(id(n), action).await.expect("patch").outcome
}

fn failure<'o>(outcome: &'o Outcome, rule: &str) -> &'o str {
    match outcome {
        Outcome::Failed { message } => message,
        other => panic!("{rule}: expected the patch to fail, got {other:?}"),
    }
}

/// Whether the backend runs a POSIX shell for `Exec` (the command scenarios need `rm`, `mv`,
/// `chmod`, `ln`). The probe needs shell syntax, so `cmd` with Git's tools on `PATH` fails it.
async fn posix(sandbox: &dyn Sandbox) -> bool {
    if !lists(sandbox, &[SandboxActionKind::Exec]) {
        return false;
    }
    let probe = SandboxAction::Exec {
        command: "x=posix; printf \"$x\"".into(),
        cwd: None,
        timeout_ms: Some(10_000),
        background: false,
    };
    matches!(sandbox.execute(id(900), probe).await, Ok(o) if matches!(&o.outcome, Outcome::Exec { exit_code: Some(0), stdout, .. } if stdout == "posix"))
}

async fn sh(sandbox: &dyn Sandbox, n: u32, command: &str) -> String {
    let action =
        SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: Some(20_000), background: false };
    match sandbox.execute(id(n), action).await.expect("exec").outcome {
        Outcome::Exec { exit_code: Some(0), stdout, .. } => stdout,
        other => panic!("`{command}` failed: {other:?}"),
    }
}

async fn patches_apply_exactly(sandbox: &dyn Sandbox) {
    use SandboxActionKind::{Patch, ReadFile, WriteFile};
    if !lists(sandbox, &[Patch, ReadFile, WriteFile]) {
        return;
    }
    let path = "conformance/patch.txt";
    let original = "one\ntwo\nthree\ntwo again\n";
    write(sandbox, 910, path, original).await;
    let (_, h0) = read(sandbox, 911, path).await.expect("read");
    assert!(
        h0.len() == 64 && h0.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
        "a hash is lowercase hex SHA-256: {h0}"
    );

    // Success: the diff shows the change, and the new hash is the file's.
    let done = patch(sandbox, 912, path, &[("three", "THREE")], &h0).await;
    let Outcome::Patched { diff, hash: h1 } = done else { panic!("a matching patch must apply: {done:?}") };
    assert!(diff.contains("-three") && diff.contains("+THREE"), "the patch's diff must show the change:\n{diff}");
    let (now, h_now) = read(sandbox, 913, path).await.expect("read");
    assert_eq!(now, "one\ntwo\nTHREE\ntwo again\n", "the patch must be applied");
    assert_eq!(h1, h_now, "the patch's hash must be the file's new hash");

    // No match, and two matches: each says which edit, and nothing changes.
    let none = patch(sandbox, 914, path, &[("one", "1"), ("absent", "x")], &h1).await;
    let rule = "a search that matches nothing must fail, saying which edit";
    assert!(failure(&none, rule).contains("edit 2"), "{rule}: {none:?}");
    let (after, _) = read(sandbox, 915, path).await.expect("read");
    assert_eq!(after, now, "all the edits apply, or none do");
    let two = patch(sandbox, 916, path, &[("two", "2")], &h1).await;
    let rule = "a search that matches twice must fail, saying which edit";
    assert!(failure(&two, rule).contains("edit 1"), "{rule}: {two:?}");
    assert_eq!(read(sandbox, 917, path).await.expect("read").0, now, "a search that matches twice must change nothing");

    // A stale hash: nothing changes, and the failure gives the current hash.
    let stale = patch(sandbox, 918, path, &[("one", "1")], &h0).await;
    let rule = "a stale patch must fail with the file's current hash";
    assert!(failure(&stale, rule).contains(&h1), "{rule}: {stale:?}");
    assert_eq!(read(sandbox, 919, path).await.expect("read").0, now, "a stale patch must change nothing");

    // A missing file.
    let missing = patch(sandbox, 925, "conformance/missing.txt", &[("a", "b")], &h1).await;
    failure(&missing, "a patch of a missing file must fail");
}

async fn patches_refuse_symlinks(sandbox: &dyn Sandbox) {
    use SandboxActionKind::{Patch, ReadFile, WriteFile};
    if !(lists(sandbox, &[Patch, ReadFile, WriteFile]) && posix(sandbox).await) {
        return;
    }
    write(sandbox, 920, "target.txt", "keep\n").await;
    // Some shells (MSYS) copy instead of linking without the right privilege.
    if sh(sandbox, 921, "ln -s target.txt link.txt && if test -L link.txt; then echo link; fi").await.trim() != "link" {
        return;
    }
    let (_, hash) = read(sandbox, 922, "target.txt").await.expect("read");
    let o = patch(sandbox, 923, "link.txt", &[("keep", "changed")], &hash).await;
    failure(&o, "a patch of a symlink must fail");
    assert_eq!(read(sandbox, 924, "target.txt").await.expect("read").0, "keep\n", "a patch never follows a symlink");
}

async fn checkpoints_are_declared_consistently(sandbox: &dyn Sandbox) {
    let never = CheckpointId::new("conformance-never-taken");
    if !sandbox.capabilities().checkpoints {
        assert!(
            matches!(sandbox.checkpoint().await, Err(SandboxError::Unsupported(_))),
            "without checkpoints, checkpoint must be Unsupported"
        );
        assert!(
            matches!(sandbox.restore(&never).await, Err(SandboxError::Unsupported(_))),
            "without checkpoints, restore must be Unsupported"
        );
        assert!(
            matches!(sandbox.diff(&never, None).await, Err(SandboxError::Unsupported(_))),
            "without checkpoints, diff must be Unsupported"
        );
        return;
    }
    let taken = sandbox.checkpoint().await.expect("checkpoint");
    assert!(sandbox.diff(&taken, None).await.expect("diff").is_empty(), "nothing changed since the checkpoint");
    assert!(
        matches!(sandbox.restore(&never).await, Err(SandboxError::NoCheckpoint(_))),
        "an unknown checkpoint must be NoCheckpoint"
    );
    assert!(
        matches!(sandbox.diff(&never, None).await, Err(SandboxError::NoCheckpoint(_))),
        "an unknown checkpoint must be NoCheckpoint"
    );
    assert!(
        matches!(sandbox.diff(&taken, Some(&never)).await, Err(SandboxError::NoCheckpoint(_))),
        "an unknown checkpoint must be NoCheckpoint"
    );
    sandbox.reset().await.expect("reset");
    sandbox.restore(&taken).await.expect("checkpoints survive a reset");
}

fn change<'d>(diff: &'d WorkspaceDiff, path: &str) -> &'d crate::FileChange {
    diff.changes.iter().find(|c| c.path == path).unwrap_or_else(|| panic!("the diff must list {path}: {diff:?}"))
}

fn lacks(diff: &WorkspaceDiff, path: &str) {
    assert!(diff.changes.iter().all(|c| c.path != path), "the diff must not list {path}: {diff:?}");
}

async fn checkpoints_restore_writes_and_patches(sandbox: &dyn Sandbox) {
    use SandboxActionKind::{Patch, ReadFile, WriteFile};
    if !(sandbox.capabilities().checkpoints && lists(sandbox, &[Patch, ReadFile, WriteFile])) {
        return;
    }
    write(sandbox, 930, "a.txt", "alpha\n").await;
    write(sandbox, 931, "b.txt", "beta\n").await;
    write(sandbox, 932, "same.txt", "same\n").await;
    let before = sandbox.checkpoint().await.expect("checkpoint");
    write(sandbox, 933, "a.txt", "ALPHA\n").await;
    let (_, hash) = read(sandbox, 934, "b.txt").await.expect("read");
    let o = patch(sandbox, 935, "b.txt", &[("beta", "BETA")], &hash).await;
    assert!(matches!(o, Outcome::Patched { .. }), "{o:?}");
    write(sandbox, 936, "new/created.txt", "new\n").await;

    let diff = sandbox.diff(&before, None).await.expect("diff");
    let a = change(&diff, "a.txt");
    assert!(matches!((&a.before, &a.after), (Some(EntryState::File { .. }), Some(EntryState::File { .. }))), "{a:?}");
    assert!(
        a.diff.as_deref().is_some_and(|d| d.contains("-alpha") && d.contains("+ALPHA")),
        "a text change carries its diff: {a:?}"
    );
    change(&diff, "b.txt");
    let created = change(&diff, "new/created.txt");
    assert!(created.before.is_none() && created.after.is_some(), "{created:?}");
    lacks(&diff, "same.txt");

    let undone = sandbox.restore(&before).await.expect("restore");
    change(&undone, "a.txt");
    change(&undone, "new/created.txt");
    assert_eq!(read(sandbox, 937, "a.txt").await.expect("a.txt").0, "alpha\n", "restore must bring back the content");
    assert_eq!(read(sandbox, 938, "b.txt").await.expect("b.txt").0, "beta\n", "restore must undo the patch");
    assert!(read(sandbox, 939, "new/created.txt").await.is_none(), "restore must remove files created since");
    assert!(sandbox.diff(&before, None).await.expect("diff").is_empty(), "after a restore nothing differs from it");
}

async fn checkpoints_restore_what_commands_did(sandbox: &dyn Sandbox) {
    use SandboxActionKind::{ReadFile, WriteFile};
    if !(sandbox.capabilities().checkpoints && lists(sandbox, &[ReadFile, WriteFile]) && posix(sandbox).await) {
        return;
    }
    write(sandbox, 940, "keep/doomed.txt", "doomed\n").await;
    write(sandbox, 941, "moved.txt", "moved\n").await;
    write(sandbox, 942, "plain.sh", "echo hi\n").await;
    sh(sandbox, 943, "chmod 644 plain.sh && ln -s moved.txt link").await;
    let before = sandbox.checkpoint().await.expect("checkpoint");
    sh(sandbox, 944, "rm keep/doomed.txt && rmdir keep && mv moved.txt elsewhere.txt && mkdir -p made/deep && echo x > made/deep/f && chmod 755 plain.sh && rm link").await;

    let diff = sandbox.diff(&before, None).await.expect("diff");
    for gone in ["keep", "keep/doomed.txt", "moved.txt", "link"] {
        let c = change(&diff, gone);
        assert!(c.before.is_some() && c.after.is_none(), "{gone} was deleted: {c:?}");
    }
    for made in ["elsewhere.txt", "made", "made/deep", "made/deep/f"] {
        let c = change(&diff, made);
        assert!(c.before.is_none() && c.after.is_some(), "{made} was created: {c:?}");
    }
    let mode = change(&diff, "plain.sh");
    assert!(
        matches!((&mode.before, &mode.after), (Some(EntryState::File { mode: m0, .. }), Some(EntryState::File { mode: m1, .. })) if m0 & 0o777 == 0o644 && m1 & 0o777 == 0o755),
        "a mode change must show: {mode:?}"
    );
    let link = change(&diff, "link");
    assert_eq!(link.before, Some(EntryState::Symlink { target: "moved.txt".into() }), "a symlink is kept as a link");

    sandbox.restore(&before).await.expect("restore");
    let check = "test -f keep/doomed.txt && test -f moved.txt && test ! -e elsewhere.txt && test ! -e made \
                 && test ! -x plain.sh && test -L link && test \"$(readlink link)\" = moved.txt && echo restored";
    assert_eq!(sh(sandbox, 945, check).await.trim(), "restored", "restore must undo rm, mv, mkdir, chmod and ln");
    assert_eq!(read(sandbox, 946, "keep/doomed.txt").await.expect("read").0, "doomed\n");
    assert!(sandbox.diff(&before, None).await.expect("diff").is_empty(), "after a restore nothing differs from it");
}

async fn older_checkpoints_restore_after_newer(sandbox: &dyn Sandbox) {
    use SandboxActionKind::{ReadFile, WriteFile};
    if !(sandbox.capabilities().checkpoints && lists(sandbox, &[ReadFile, WriteFile])) {
        return;
    }
    write(sandbox, 950, "v.txt", "1\n").await;
    let one = sandbox.checkpoint().await.expect("checkpoint");
    write(sandbox, 951, "v.txt", "2\n").await;
    write(sandbox, 952, "two.txt", "2\n").await;
    let two = sandbox.checkpoint().await.expect("checkpoint");
    write(sandbox, 953, "v.txt", "3\n").await;
    let between = sandbox.diff(&one, Some(&two)).await.expect("diff");
    change(&between, "v.txt");
    change(&between, "two.txt");
    sandbox.restore(&two).await.expect("restore the newer");
    assert_eq!(read(sandbox, 954, "v.txt").await.expect("read").0, "2\n");
    sandbox.restore(&one).await.expect("restore the older after the newer");
    assert_eq!(read(sandbox, 955, "v.txt").await.expect("read").0, "1\n");
    assert!(read(sandbox, 956, "two.txt").await.is_none(), "two.txt came after the older checkpoint");
    sandbox.restore(&two).await.expect("and the newer again");
    assert_eq!(read(sandbox, 957, "v.txt").await.expect("read").0, "2\n");
    assert_eq!(read(sandbox, 958, "two.txt").await.expect("read").0, "2\n");
}
