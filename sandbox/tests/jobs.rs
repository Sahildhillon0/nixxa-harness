//! Background jobs in the real sandbox: each job is its own process tree, ending a job kills
//! exactly that tree, and the limits hold.

#![cfg(target_os = "linux")]

mod common;

use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::stream::BoxStream;
use nixxa_contracts::{JobId, Outcome, Sandbox, SandboxAction, SandboxEvent};
use nixxa_sandbox::bwrap::BwrapSandbox;

use common::{Scratch, config, run, sandbox, sandbox_with, sh, sh_out};

fn background(command: &str) -> SandboxAction {
    SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: None, background: true }
}

async fn start(sb: &BwrapSandbox, command: &str) -> JobId {
    match run(sb, background(command)).await.expect("start").outcome {
        Outcome::Started { job } => job,
        other => panic!("`{command}`: expected Started, got {other:?}"),
    }
}

async fn text(sb: &BwrapSandbox, action: SandboxAction) -> String {
    match run(sb, action).await.expect("job action").outcome {
        Outcome::Text { content } => content,
        other => panic!("expected text, got {other:?}"),
    }
}

/// How many live processes have exactly this command line.
async fn count(sb: &BwrapSandbox, cmdline: &str) -> usize {
    // A process may exit mid-scan; the shell's complaint about it goes to stderr, not the count.
    let script = format!(
        "for p in /proc/[0-9]*; do tr '\\0' ' ' < $p/cmdline; echo; done 2>/dev/null | grep -cx '{cmdline} ' || true"
    );
    let (_, stdout, _) = sh(sb, &script).await;
    stdout.trim().parse().expect("a count")
}

/// Waits (up to 10 s) until `count(cmdline) == want`.
async fn wait_count(sb: &BwrapSandbox, cmdline: &str, want: usize) {
    let started = Instant::now();
    loop {
        let n = count(sb, cmdline).await;
        if n == want {
            return;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "`{cmdline}`: {n} running, wanted {want}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The next `JobFinished` for `job` within 15 s: (exit code, truncated).
async fn finished(events: &mut BoxStream<'static, SandboxEvent>, job: &JobId) -> (Option<i32>, bool) {
    let wait = async {
        while let Some(event) = events.next().await {
            if let SandboxEvent::JobFinished { job: done, exit_code, truncated, .. } = event {
                if &done == job {
                    return (exit_code, truncated);
                }
            }
        }
        panic!("the event stream ended");
    };
    tokio::time::timeout(Duration::from_secs(15), wait).await.expect("no JobFinished in time")
}

#[tokio::test]
async fn a_background_job_outlives_foreground_commands_and_stopping_it_kills_its_whole_tree() {
    let scratch = Scratch::new("job-tree");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut events = sb.subscribe();
    // A child in the job's session, and an orphan (double fork) that leaves it with setsid.
    let job = start(&sb, "sleep 300 & (setsid sleep 301 &); wait").await;
    wait_count(&sb, "sleep 300", 1).await;
    wait_count(&sb, "sleep 301", 1).await;
    let sessions = sh_out(
        &sb,
        "for p in /proc/[0-9]*; do case \"$(tr '\\0' ' ' < $p/cmdline 2>/dev/null)\" in \
         'sleep 300 '|'sleep 301 ') awk '{print $6}' $p/stat;; esac; done | sort -u | wc -l",
    )
    .await;
    assert_eq!(sessions.trim(), "2", "the setsid child must really be in its own session");
    for _ in 0..3 {
        assert_eq!(sh_out(&sb, "echo foreground").await, "foreground\n");
    }
    // Longer than the daemon's orphan sweep: an orphan not held by its job's supervisor would be
    // gone by now.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(count(&sb, "sleep 300").await, 1, "the job must survive foreground commands");
    assert_eq!(count(&sb, "sleep 301").await, 1, "the job must survive foreground commands");
    let status = text(&sb, SandboxAction::JobStatus { job: job.clone() }).await;
    assert!(status.contains("is running"), "{status}");

    let stopped = text(&sb, SandboxAction::JobStop { job: job.clone() }).await;
    assert!(stopped.contains("was stopped"), "{stopped}");
    assert_eq!(count(&sb, "sleep 300").await, 0, "stopping a job must kill its children");
    assert_eq!(count(&sb, "sleep 301").await, 0, "stopping a job must kill what left its session");
    assert_eq!(finished(&mut events, &job).await, (None, false));
}

#[tokio::test]
async fn a_foreground_commands_leftovers_die_but_an_earlier_background_job_survives() {
    let scratch = Scratch::new("job-leftovers");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    start(&sb, "sleep 303").await;
    wait_count(&sb, "sleep 303", 1).await;
    assert_eq!(sh_out(&sb, "sleep 304 & (setsid sleep 305 &); echo started").await, "started\n");
    assert_eq!(count(&sb, "sleep 304").await, 0, "the foreground command's leftovers must die");
    assert_eq!(count(&sb, "sleep 305").await, 0, "the foreground command's leftovers must die");
    assert_eq!(count(&sb, "sleep 303").await, 1, "the background job must survive");
}

#[tokio::test]
async fn job_finished_carries_the_exit_code_and_the_output_stays_readable() {
    let scratch = Scratch::new("job-exit");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut events = sb.subscribe();
    let job = start(&sb, "echo out; echo err >&2; exit 7").await;
    assert_eq!(finished(&mut events, &job).await, (Some(7), false));
    let output = text(&sb, SandboxAction::JobOutput { job: job.clone(), tail_bytes: 100 }).await;
    assert!(output.starts_with(&format!("job {job} exited with code 7")), "{output}");
    assert!(output.contains("--- stdout (last 4 bytes) ---\nout\n"), "{output}");
    assert!(output.contains("--- stderr (last 4 bytes) ---\nerr\n"), "{output}");
}

#[tokio::test]
async fn at_most_the_configured_number_of_jobs_run_at_once() {
    let scratch = Scratch::new("job-count");
    let mut config = config(&scratch.workspace());
    config.jobs.max_running = 2;
    let Some(sb) = sandbox_with(config) else { return };
    let first = start(&sb, "sleep 310").await;
    start(&sb, "sleep 311").await;
    let refused = run(&sb, background("sleep 312")).await.unwrap();
    assert!(
        matches!(&refused.outcome, Outcome::Failed { message } if message.contains("already running")),
        "{refused:?}"
    );
    text(&sb, SandboxAction::JobStop { job: first }).await;
    start(&sb, "sleep 312").await;
}

#[tokio::test]
async fn a_job_past_its_time_limit_is_stopped() {
    let scratch = Scratch::new("job-time");
    let mut config = config(&scratch.workspace());
    config.jobs.max_timeout = Duration::from_secs(1);
    let Some(sb) = sandbox_with(config) else { return };
    let mut events = sb.subscribe();
    let started = Instant::now();
    // Asks for far more than the maximum.
    let action =
        SandboxAction::Exec { command: "sleep 313".into(), cwd: None, timeout_ms: Some(3_600_000), background: true };
    let Outcome::Started { job } = run(&sb, action).await.unwrap().outcome else { panic!("not started") };
    assert_eq!(finished(&mut events, &job).await, (None, false));
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    let status = text(&sb, SandboxAction::JobStatus { job }).await;
    assert!(status.contains("hit its time limit"), "{status}");
    assert_eq!(count(&sb, "sleep 313").await, 0);
}

#[tokio::test]
async fn only_the_last_part_of_a_jobs_output_is_kept() {
    let scratch = Scratch::new("job-output");
    let mut config = config(&scratch.workspace());
    config.jobs.output_cap = 1000;
    let Some(sb) = sandbox_with(config) else { return };
    let mut events = sb.subscribe();
    let job = start(&sb, "yes 'abcd efgh' | head -c 50002; echo END").await;
    assert_eq!(finished(&mut events, &job).await, (Some(0), true), "the job runs on, its output cut");
    let output = text(&sb, SandboxAction::JobOutput { job, tail_bytes: 1_000_000 }).await;
    // The last 1000 bytes start inside a line: that line may hold the end of a secret, or a
    // value without its name, so it goes (D-074).
    assert!(output.contains("--- stdout (last 996 bytes, earlier output dropped) ---\nabcd efgh\n"), "{output}");
    assert!(output.contains("abcd efgh\nabEND\n"), "{output}");
}

#[tokio::test]
async fn reset_stops_every_job() {
    let scratch = Scratch::new("job-reset");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut events = sb.subscribe();
    let a = start(&sb, "sleep 320").await;
    let b = start(&sb, "sleep 321 & sleep 322; wait").await;
    wait_count(&sb, "sleep 322", 1).await;
    sb.reset().await.unwrap();
    assert_eq!(finished(&mut events, &a).await.0, None);
    assert_eq!(finished(&mut events, &b).await.0, None);
    for cmdline in ["sleep 320", "sleep 321", "sleep 322"] {
        assert_eq!(count(&sb, cmdline).await, 0, "`{cmdline}` survived a reset");
    }
    let status = text(&sb, SandboxAction::JobStatus { job: a }).await;
    assert!(status.contains("was stopped"), "a job reset stopped is still known: {status}");
}

#[tokio::test]
async fn a_job_that_kills_its_own_supervisor_still_dies() {
    let scratch = Scratch::new("job-orphans");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut events = sb.subscribe();
    // $PPID is the job's supervisor: without it, the shell and its child are orphaned.
    let job = start(&sb, "sleep 330 & kill -9 $PPID; sleep 331").await;
    assert_eq!(finished(&mut events, &job).await.0, None);
    wait_count(&sb, "sleep 330", 0).await;
    wait_count(&sb, "sleep 331", 0).await;
    // And the daemon is fine.
    assert_eq!(sh_out(&sb, "echo still here").await, "still here\n");
}

#[tokio::test]
async fn events_arrive_while_a_foreground_command_runs() {
    let scratch = Scratch::new("job-events");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut events = sb.subscribe();
    let job = start(&sb, "sleep 1; exit 4").await;
    let foreground = sh_out(&sb, "sleep 3; echo done");
    let (event, out) = tokio::join!(finished(&mut events, &job), foreground);
    assert_eq!(event, (Some(4), false));
    assert_eq!(out, "done\n");
}

#[tokio::test]
async fn a_restarted_daemon_never_reuses_a_job_id() {
    let scratch = Scratch::new("job-ids");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut events = sb.subscribe();
    let mut words = sb.subscribe();
    let first = start(&sb, "sleep 340").await;
    // PID 2 is the daemon; killing it takes its jobs with it.
    assert!(
        run(
            &sb,
            SandboxAction::Exec {
                command: "kill -9 2; sleep 5".into(),
                cwd: None,
                timeout_ms: None,
                background: false
            }
        )
        .await
        .is_err()
    );
    assert_eq!(finished(&mut events, &first).await, (None, false), "a dead daemon's job is reported as ended");
    let status = loop {
        match words.next().await {
            Some(SandboxEvent::JobFinished { job, status, .. }) if job == first => break status,
            Some(_) => {}
            None => panic!("the events ended"),
        }
    };
    assert!(status.starts_with(&format!("job {first} was lost after")), "{status}");
    let second = start(&sb, "sleep 341").await;
    assert_ne!(second, first, "a restarted daemon reused job id {first}");
    let old = run(&sb, SandboxAction::JobStatus { job: first.clone() }).await.unwrap();
    assert!(matches!(old.outcome, Outcome::Failed { .. }), "{first} must not name the new job: {old:?}");
}
