//! Logging: `tracing` to daily files in `<data dir>/nixxa/logs`, never to stdout or stderr
//! (the TUI owns the terminal).
//!
//! Only metadata is logged (ids, kinds, verdicts, error messages), never message text or action
//! output, which can hold file contents. Secrets cannot reach a log: `Credential` redacts
//! itself, and the controller never sees one. The free text that is logged (error words, the
//! reason an action can't be undone) comes scrubbed from the controller, and is scrubbed again
//! here, with a scrubber of the log's own (D-074): a log outlives the session.

use std::path::{Path, PathBuf};

use std::sync::OnceLock;

use nixxa_contracts::{Action, Event, KnowledgeAction, Outcome, RedactionSource, Verdict};
use nixxa_controller::scrub::Scrubber;
use tracing::Level;

use crate::safe;
use tracing_appender::rolling::{RollingFileAppender, Rotation};

/// How many daily log files are kept.
const KEEP: usize = 7;

/// Starts logging to `dir`. `filter` is the `NIXXA_LOG` value (`error` … `trace`; default
/// `info`). Returns the log directory, or why logging is off.
pub(crate) fn init(dir: Option<PathBuf>, filter: Option<&str>) -> Result<PathBuf, String> {
    let dir = dir.ok_or("no data directory found for logs")?;
    let appender = appender(&dir)?;
    let level = level(filter)?;
    tracing_subscriber::fmt()
        .with_writer(appender)
        .with_ansi(false)
        .with_max_level(level)
        .try_init()
        .map_err(|err| format!("could not start logging: {err}"))?;
    Ok(dir)
}

/// Logs the metadata of one controller event.
pub(crate) fn event(event: &Event) {
    match event {
        Event::SessionStarted { session, account, brain } => {
            tracing::info!(
                session = %safe::line(session.as_str()),
                account = %safe::line(&account.to_string()),
                model = %safe::line(&brain.model),
                "session started"
            );
        }
        Event::SessionEnded { session } => tracing::info!(session = %safe::line(session.as_str()), "session ended"),
        Event::TurnStarted { turn } => tracing::debug!(%turn, "turn started"),
        Event::TurnDone { turn } => tracing::debug!(%turn, "turn done"),
        // Metadata only: the job and how it exited, never its command or output.
        Event::JobFinished { job, exit_code, duration_ms, .. } => {
            tracing::info!(job = %safe::line(job.as_str()), ?exit_code, duration_ms, "background job finished");
        }
        Event::WakeUp { turn, jobs } => {
            let jobs: Vec<String> = jobs.iter().map(|job| safe::line(job.as_str())).collect();
            tracing::info!(%turn, ?jobs, "turn started for ended jobs");
        }
        Event::ActionProposed { turn, request } => {
            tracing::info!(%turn, action = %safe::line(request.id.as_str()), kind = action_kind(&request.action), "action proposed");
        }
        Event::ActionStarted { turn, action_id } => {
            tracing::debug!(%turn, action = %safe::line(action_id.as_str()), "action started")
        }
        Event::ActionFinished { turn, observation } => tracing::info!(
            %turn,
            action = %safe::line(observation.action_id.as_str()),
            outcome = outcome_kind(&observation.outcome),
            duration_ms = observation.duration_ms,
            "action finished"
        ),
        Event::Error { turn, message } => {
            let turn = turn.as_ref().map(ToString::to_string).unwrap_or_default();
            tracing::warn!(turn, message = %clean(message), "controller error");
        }
        Event::Usage { account, usage } => tracing::debug!(
            account = %safe::line(&account.to_string()),
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            cost_micro_usd = usage.cost_micro_usd,
            "usage"
        ),
        Event::SourceStatus { source } => {
            tracing::info!(source = %safe::line(source.id.as_str()), status = ?source.status, "knowledge source")
        }
        Event::SandboxStatus { capabilities } => {
            tracing::info!(backend = %safe::line(&capabilities.backend), isolated = capabilities.isolated, "sandbox");
        }
        Event::ModeChanged { mode } => tracing::info!(?mode, "approval mode changed"),
        Event::AccountChanged { account } => {
            tracing::info!(account = %safe::line(&account.to_string()), "account changed")
        }
        Event::Checkpoint { turn, checkpoint, before } => {
            let before = before.as_ref().map(|id| safe::line(id.as_str())).unwrap_or_default();
            tracing::info!(%turn, checkpoint = %safe::line(checkpoint.as_str()), before, "checkpoint");
        }
        Event::NotUndoable { turn, before, reason } => {
            let before = before.as_ref().map(|id| safe::line(id.as_str())).unwrap_or_default();
            tracing::warn!(%turn, before, reason = %clean(reason), "no checkpoint: can't be undone");
        }
        // How many paths, never which (paths are the agent's).
        Event::Restored { checkpoint, undone } => tracing::info!(
            checkpoint = %safe::line(checkpoint.as_str()),
            paths = undone.changes.len(),
            "workspace restored"
        ),
        // The kind and where from; never the placeholder, a path or the value.
        Event::SecretRedacted { turn, kind, source, sent, .. } => {
            let turn = turn.as_ref().map(ToString::to_string).unwrap_or_default();
            let from = source_kind(source);
            tracing::info!(turn, kind = %safe::line(kind), from, sent, "secret redacted");
        }
        // Message text and streamed tokens are content, not metadata.
        Event::Message { .. } | Event::TextDelta { .. } | Event::ApprovalNeeded { .. } => {}
    }
}

/// Logs the human's answer to an approval prompt.
pub(crate) fn approval(action: &nixxa_contracts::ActionId, verdict: &Verdict) {
    let decision = match verdict {
        Verdict::Allow => "allow",
        Verdict::Deny(_) => "deny",
        Verdict::Ask(_) => "ask",
    };
    tracing::info!(action = %safe::line(action.as_str()), decision, "approval decided");
}

fn action_kind(action: &Action) -> &'static str {
    match action {
        Action::Sandbox { action } => match action.kind() {
            nixxa_contracts::SandboxActionKind::Exec => "exec",
            nixxa_contracts::SandboxActionKind::ReadFile => "read",
            nixxa_contracts::SandboxActionKind::WriteFile => "write",
            nixxa_contracts::SandboxActionKind::Patch => "patch",
            nixxa_contracts::SandboxActionKind::ListDir => "list",
            nixxa_contracts::SandboxActionKind::JobStatus => "job-status",
            nixxa_contracts::SandboxActionKind::JobOutput => "job-output",
            nixxa_contracts::SandboxActionKind::JobStop => "job-stop",
        },
        Action::Knowledge { action: KnowledgeAction::Call { .. } } => "knowledge-call",
        Action::Knowledge { action: KnowledgeAction::Record { .. } } => "knowledge-record",
    }
}

/// Free text for the log: scrubbed, on one line. Without a scrubber (no random source) it is
/// left out rather than written as it is.
fn clean(text: &str) -> String {
    static SCRUBBER: OnceLock<Option<Scrubber>> = OnceLock::new();
    match SCRUBBER.get_or_init(|| Scrubber::new().ok()) {
        Some(scrubber) => safe::line(&scrubber.scrub(text).text),
        None => "[left out: no secret scrubber]".to_string(),
    }
}

fn source_kind(source: &RedactionSource) -> &'static str {
    match source {
        RedactionSource::UserMessage => "user-message",
        RedactionSource::TerminalShare => "terminal-share",
        RedactionSource::FileRead { .. } => "file-read",
        RedactionSource::FileEdit { .. } => "file-edit",
        RedactionSource::CommandOutput { .. } => "command-output",
        RedactionSource::JobOutput { .. } => "job-output",
        RedactionSource::JobNote => "job-note",
        RedactionSource::NetworkRequest { .. } => "network-request",
        RedactionSource::Knowledge { .. } => "knowledge",
        RedactionSource::ActionResult { .. } => "action-result",
        RedactionSource::Notice => "notice",
        RedactionSource::ResumedHistory => "resumed-history",
    }
}

fn outcome_kind(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::Exec { .. } => "exec",
        Outcome::Text { .. } => "text",
        Outcome::File { .. } => "file",
        Outcome::Patched { .. } => "patched",
        Outcome::Listing { .. } => "listing",
        Outcome::Written { .. } => "written",
        Outcome::Recorded { .. } => "recorded",
        Outcome::Denied { .. } => "denied",
        Outcome::Failed { .. } => "failed",
        Outcome::Started { .. } => "started",
    }
}

pub(crate) fn appender(dir: &Path) -> Result<RollingFileAppender, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("could not create {}: {err}", dir.display()))?;
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("nixxa")
        .filename_suffix("log")
        .max_log_files(KEEP)
        .build(dir)
        .map_err(|err| format!("could not open a log file in {}: {err}", dir.display()))
}

fn level(filter: Option<&str>) -> Result<Level, String> {
    match filter.map(str::trim).filter(|f| !f.is_empty()) {
        None => Ok(Level::INFO),
        Some(filter) => filter.parse().map_err(|_| format!("NIXXA_LOG={filter:?} is not a log level")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    /// Tests on other threads hit the same callsites with no subscriber, and tracing caches a
    /// callsite as disabled when nothing wants it. A global, everything-enabled subscriber
    /// (writing nowhere) means no callsite can be cached as disabled. Call before capturing.
    pub(crate) fn enable_every_callsite() {
        static GLOBAL: std::sync::Once = std::sync::Once::new();
        GLOBAL.call_once(|| {
            let sink =
                tracing_subscriber::fmt().with_writer(std::io::sink).with_max_level(tracing::Level::TRACE).finish();
            tracing::subscriber::set_global_default(sink).expect("no other global subscriber in tests");
            tracing::callsite::rebuild_interest_cache();
        });
    }

    use std::io::Write;

    use super::*;

    #[test]
    fn level_defaults_to_info_and_parses_names() {
        assert_eq!(level(None), Ok(Level::INFO));
        assert_eq!(level(Some(" ")), Ok(Level::INFO));
        assert_eq!(level(Some("debug")), Ok(Level::DEBUG));
        assert!(level(Some("loud")).is_err());
    }

    /// Runs `f` with a subscriber that captures everything logged.
    fn captured(f: impl FnOnce()) -> String {
        #[derive(Clone, Default)]
        struct Buf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl Write for Buf {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buf = Buf::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_max_level(Level::TRACE)
            .finish();
        enable_every_callsite();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buf.0.lock().unwrap().clone();
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn events_log_metadata_but_never_content() {
        use nixxa_contracts::{
            AccountRef, ActionId, ActionRequest, BrainInfo, Message, Observation, ProviderId, Role, SandboxAction,
            SessionId, Timestamp, TurnId,
        };
        let turn = TurnId::new("turn-0");
        let request = ActionRequest {
            id: ActionId::new("a1"),
            action: Action::Sandbox {
                action: SandboxAction::Exec {
                    command: "cat secret.txt".into(),
                    cwd: None,
                    timeout_ms: None,
                    background: false,
                },
            },
            reason: None,
        };
        let log = captured(|| {
            event(&Event::SessionStarted {
                session: SessionId::new("s1"),
                account: AccountRef::new("scripted", "default"),
                brain: BrainInfo { provider: ProviderId::new("scripted"), model: "scripted-1".into() },
            });
            event(&Event::Message {
                turn: turn.clone(),
                message: Message::text(Role::User, "my password", Timestamp(0)),
            });
            event(&Event::ActionProposed { turn: turn.clone(), request: request.clone() });
            approval(&request.id, &Verdict::Allow);
            event(&Event::ActionFinished {
                turn: turn.clone(),
                observation: Observation::new(
                    ActionId::new("a1"),
                    Outcome::Exec { exit_code: Some(0), stdout: "hunter2".into(), stderr: String::new() },
                ),
            });
            event(&Event::Error { turn: Some(turn.clone()), message: "boom\x1b]0;x\x07".into() });
            // Free text that is logged is scrubbed here too, whatever the controller did.
            event(&Event::Error { turn: None, message: "bad key AKIAIOSFODNN7EXAMPLE".into() });
            event(&Event::NotUndoable {
                turn: turn.clone(),
                before: None,
                reason: "can't read postgres://app:hunter5@db/x".into(),
            });
            event(&Event::SecretRedacted {
                turn: Some(turn.clone()),
                kind: "aws-access-key".into(),
                source: nixxa_contracts::RedactionSource::FileRead { path: "secret-plans.env".into() },
                placeholder: "‹secret:aws-access-key#abcd›".into(),
                sent: false,
            });
            // A patch's diff and a restore's paths are the agent's content.
            event(&Event::ActionFinished {
                turn: turn.clone(),
                observation: Observation::new(
                    ActionId::new("a2"),
                    Outcome::Patched { diff: "-old\n+hunter3\n".into(), hash: "h".into() },
                ),
            });
            event(&Event::Checkpoint {
                turn: turn.clone(),
                checkpoint: nixxa_contracts::CheckpointId::new("run.1"),
                before: Some(ActionId::new("a2")),
            });
            event(&Event::Restored {
                checkpoint: nixxa_contracts::CheckpointId::new("run.1"),
                undone: nixxa_contracts::WorkspaceDiff {
                    changes: vec![nixxa_contracts::FileChange {
                        path: "secret-plans.txt".into(),
                        before: None,
                        after: None,
                        diff: Some("+hunter4".into()),
                    }],
                    not_covered: Vec::new(),
                },
            });
            event(&Event::SessionEnded { session: SessionId::new("s1") });
        });

        for expected in [
            "session started",
            "action proposed",
            "kind=\"exec\"",
            "approval decided",
            "decision=\"allow\"",
            "action finished",
            "controller error",
            "session ended",
            "outcome=\"patched\"",
            "checkpoint=run.1",
            "workspace restored",
            "paths=1",
            "secret redacted",
            "kind=aws-access-key",
            "from=\"file-read\"",
            "‹secret:aws-access-key#",
        ] {
            assert!(log.contains(expected), "missing {expected:?} in:\n{log}");
        }
        for leaked in [
            "my password",
            "cat secret.txt",
            "hunter2",
            "hunter3",
            "hunter4",
            "hunter5",
            "IOSFODNN7",
            "secret-plans",
            "#abcd",
            "\x1b",
            "\x07",
        ] {
            assert!(!log.contains(leaked), "{leaked:?} reached the log:\n{log}");
        }
    }

    #[test]
    fn no_data_dir_turns_logging_off() {
        assert_eq!(init(None, None), Err("no data directory found for logs".into()));
    }

    #[test]
    fn the_appender_writes_into_the_log_dir() {
        let dir = std::env::temp_dir().join(format!("nixxa-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut appender = appender(&dir.join("nixxa").join("logs")).expect("appender");
        writeln!(appender, "hello").unwrap();
        appender.flush().unwrap();
        let files: Vec<_> = std::fs::read_dir(dir.join("nixxa").join("logs")).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        let name = files[0].file_name().to_string_lossy().into_owned();
        assert!(name.starts_with("nixxa") && name.ends_with(".log"), "{name}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
