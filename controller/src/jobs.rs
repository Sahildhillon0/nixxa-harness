//! The agent's background jobs, as the controller watches them (D-068).
//!
//! Only jobs the agent of this session started are watched: their `Started` observations name
//! them. Every other sandbox event is ignored: network requests, the user's terminal (which
//! sends no job events), jobs of a session that was replaced. When a watched job ends, its end
//! becomes a note for the agent, in the sandbox's own words, given once: at the next think step
//! of a running turn, or, when the agent is idle, in a turn the controller starts for it (a
//! wake-up) or between turns.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use nixxa_contracts::{Event, JobId, SandboxEvent};

/// How every job note starts, so a resumed session can tell which jobs ended.
pub(crate) const NOTE: &str = "Background job: ";
/// Ends of jobs not (yet) known to be the agent's, kept for when its `Started` comes in.
const EARLY: usize = 32;

/// The watched jobs, shared by the command loop (which hears the sandbox) and the turn (which
/// starts jobs and gives the agent their notes).
pub(crate) type Shared = Arc<Mutex<Watch>>;

/// The session's own background jobs: those running, and ends not yet given to the agent.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    running: HashMap<JobId, Running>,
    ended: Vec<Ended>,
    /// Ends of jobs nobody here started yet: a job that fails at once can end before its
    /// `Started` reaches the controller (the reply and the event arrive together). Ids are
    /// unique per sandbox (D-063), so an end found here when its job starts is that job's.
    early: VecDeque<SandboxEvent>,
}

#[derive(Debug)]
struct Running {
    command: String,
    /// Whether its end starts a turn if the agent is idle: the agent is waiting on it.
    wakes: bool,
}

/// A job's end, not yet given to the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ended {
    pub(crate) job: JobId,
    /// What the agent is told.
    pub(crate) note: String,
    pub(crate) wakes: bool,
}

impl Watch {
    /// The agent started `job`. It waits on it from now on. If its end already came, it is
    /// taken now, and its event for the UI returned.
    pub(crate) fn started(&mut self, job: JobId, command: String) -> Option<Event> {
        self.running.insert(job.clone(), Running { command, wakes: true });
        let early = self
            .early
            .iter()
            .position(|event| matches!(event, SandboxEvent::JobFinished { job: done, .. } if *done == job))?;
        let event = self.early.remove(early)?;
        self.observe(event)
    }

    /// The jobs still running, with their commands.
    pub(crate) fn running(&self) -> Vec<(JobId, String)> {
        let mut running: Vec<_> = self.running.iter().map(|(job, r)| (job.clone(), r.command.clone())).collect();
        running.sort();
        running
    }

    /// The agent stopped `job` itself: its end is noted but wakes nobody.
    pub(crate) fn stopping(&mut self, job: &JobId) {
        if let Some(running) = self.running.get_mut(job) {
            running.wakes = false;
        }
    }

    /// The user wrote: they lead now, so nothing wakes the agent until it starts another job.
    /// Ends not given yet go into the user's turn.
    pub(crate) fn user_spoke(&mut self) {
        self.running.values_mut().for_each(|running| running.wakes = false);
        self.ended.iter_mut().for_each(|ended| ended.wakes = false);
    }

    /// A sandbox event: the end of a watched job becomes a note (and an event for the UI).
    /// Everything else is ignored.
    pub(crate) fn observe(&mut self, event: SandboxEvent) -> Option<Event> {
        let SandboxEvent::JobFinished { job, exit_code, duration_ms, status, .. } = &event else { return None };
        let (job, exit_code, duration_ms, status) = (job.clone(), *exit_code, *duration_ms, status.clone());
        let Some(running) = self.running.remove(&job) else {
            if self.early.len() == EARLY {
                self.early.pop_front();
            }
            self.early.push_back(event);
            return None;
        };
        let status = if status.is_empty() { format!("job {job} ended") } else { status };
        self.ended.push(Ended {
            job: job.clone(),
            note: format!("{NOTE}{status} (`{}`).", running.command),
            wakes: running.wakes,
        });
        Some(Event::JobFinished { job, command: running.command, exit_code, duration_ms, status })
    }

    /// Whether an end is waiting to wake the agent.
    pub(crate) fn wake_due(&self) -> bool {
        self.ended.iter().any(|ended| ended.wakes)
    }

    /// The jobs whose ends wake the agent now (their notes stay for the turn's first step).
    pub(crate) fn waking(&self) -> Vec<JobId> {
        self.ended.iter().filter(|ended| ended.wakes).map(|ended| ended.job.clone()).collect()
    }

    /// The ends not given to the agent yet, in order; each is taken once.
    pub(crate) fn take_ended(&mut self) -> Vec<Ended> {
        std::mem::take(&mut self.ended)
    }
}

/// Locks `watch`, recovering from a panic elsewhere (the state stays usable).
pub(crate) fn lock(watch: &Shared) -> std::sync::MutexGuard<'_, Watch> {
    watch.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a stored session shows about its jobs, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Seen {
    /// The agent started this job with this command.
    Started(JobId, String),
    /// A system message: its job notes end jobs.
    Note(String),
}

/// The note for jobs a stored session started that never reported an end (Nixxa quit while
/// they ran): jobs don't outlive the sandbox they ran in. Starts and ends are paired in order,
/// because ids start again at `j1` in every new sandbox. Said once: the note itself ends them
/// for the next resume.
pub(crate) fn lost_on_resume(history: &[Seen]) -> Option<String> {
    let mut running: Vec<(JobId, String)> = Vec::new();
    for seen in history {
        match seen {
            Seen::Started(job, command) => running.push((job.clone(), command.clone())),
            Seen::Note(text) => {
                for line in text.lines() {
                    let Some(rest) = line.strip_prefix(NOTE).and_then(|rest| rest.strip_prefix("job ")) else {
                        continue;
                    };
                    let id = rest.split(' ').next().unwrap_or_default();
                    if let Some(at) = running.iter().position(|(job, _)| job.as_str() == id) {
                        running.remove(at);
                    }
                }
            }
        }
    }
    let lost: Vec<String> = running
        .iter()
        .map(|(job, command)| format!("{NOTE}job {job} didn't survive: Nixxa quit while it ran (`{command}`)."))
        .collect();
    (!lost.is_empty())
        .then(|| format!("{}\nJobs don't outlive a session: start them again if you still need them.", lost.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ended(job: &str, exit_code: Option<i32>, status: &str) -> SandboxEvent {
        SandboxEvent::JobFinished {
            job: JobId::new(job),
            exit_code,
            duration_ms: 3100,
            truncated: false,
            status: status.into(),
        }
    }

    #[test]
    fn only_the_sessions_own_jobs_count_and_each_end_is_noted_once() {
        let mut watch = Watch::default();
        watch.started(JobId::new("j1"), "cargo build".into());
        assert_eq!(watch.observe(ended("j9", Some(0), "job j9 exited with code 0 after 3.1 s")), None, "not ours");
        let request = nixxa_contracts::NetRequest {
            time: nixxa_contracts::Timestamp(0),
            job: Some(JobId::new("j1")),
            host: "crates.io".into(),
            port: 443,
            method: None,
            url: None,
            bytes_up: 0,
            bytes_down: 0,
            duration_ms: 0,
            result: nixxa_contracts::NetResult::Allowed,
        };
        assert_eq!(watch.observe(SandboxEvent::NetRequest(request)), None, "a request is no job end");
        assert!(!watch.wake_due());
        let event = watch.observe(ended("j1", Some(0), "job j1 exited with code 0 after 3.1 s"));
        assert!(matches!(&event, Some(Event::JobFinished { command, .. }) if command == "cargo build"), "{event:?}");
        assert_eq!(watch.observe(ended("j1", Some(0), "again")), None, "an end is reported once");
        assert!(watch.wake_due());
        let notes = watch.take_ended();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].note, "Background job: job j1 exited with code 0 after 3.1 s (`cargo build`).");
        assert!(watch.take_ended().is_empty(), "given once");
    }

    #[test]
    fn a_user_message_or_the_agents_own_stop_means_no_wake() {
        let mut watch = Watch::default();
        watch.started(JobId::new("j1"), "a".into());
        watch.started(JobId::new("j2"), "b".into());
        watch.stopping(&JobId::new("j1"));
        watch.observe(ended("j1", None, "job j1 was stopped after 1.0 s"));
        assert!(!watch.wake_due(), "the agent stopped it");
        watch.observe(ended("j2", Some(1), "job j2 exited with code 1 after 1.0 s"));
        assert!(watch.wake_due());
        watch.user_spoke();
        assert!(!watch.wake_due(), "the user took over");
        assert_eq!(watch.take_ended().len(), 2, "both still go to the agent");
    }

    #[test]
    fn jobs_a_stored_session_left_running_are_said_lost_once() {
        let start = |job: &str, command: &str| Seen::Started(JobId::new(job), command.into());
        let mut history = vec![
            start("j1", "cargo build"),
            start("j2", "sleep 9"),
            Seen::Note("Background job: job j1 exited with code 0 after 1.0 s (`cargo build`).".into()),
        ];
        let said = lost_on_resume(&history).expect("j2 is lost");
        assert!(said.starts_with("Background job: job j2 didn't survive") && !said.contains("job j1"), "{said}");
        history.push(Seen::Note(said));
        assert_eq!(lost_on_resume(&history), None, "said once");
    }

    #[test]
    fn a_reused_id_in_a_later_run_is_still_said_lost() {
        // The review's case: each launch's sandbox starts again at j1.
        let history = vec![
            Seen::Started(JobId::new("j1"), "cargo build".into()),
            Seen::Note("Background job: job j1 exited with code 0 after 1.0 s (`cargo build`).".into()),
            Seen::Started(JobId::new("j1"), "npm test".into()),
        ];
        let said = lost_on_resume(&history).expect("the second j1 is lost");
        assert!(said.contains("job j1 didn't survive") && said.contains("npm test") && !said.contains("cargo build"));
    }

    #[test]
    fn an_end_that_comes_before_its_start_is_kept_for_it() {
        let mut watch = Watch::default();
        assert_eq!(watch.observe(ended("j1", Some(127), "job j1 exited with code 127 after 0.0 s")), None);
        let event = watch.started(JobId::new("j1"), "cargo biuld".into());
        assert!(matches!(event, Some(Event::JobFinished { exit_code: Some(127), .. })), "{event:?}");
        assert!(watch.wake_due());
        assert_eq!(
            watch.take_ended()[0].note,
            "Background job: job j1 exited with code 127 after 0.0 s (`cargo biuld`)."
        );
        // Bounded: only the newest unknown ends are kept.
        for n in 0..40 {
            watch.observe(ended(&format!("x{n}"), Some(0), "x"));
        }
        assert_eq!(watch.started(JobId::new("x0"), "old".into()), None, "dropped");
        assert!(watch.started(JobId::new("x39"), "new".into()).is_some());
    }
}
