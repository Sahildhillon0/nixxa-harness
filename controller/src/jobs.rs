//! The agent's background jobs, as the controller watches them (D-068).
//!
//! Only jobs the agent of this session started are watched: their `Started` observations name
//! them. Every other sandbox event is ignored: network requests, the user's terminal (which
//! sends no job events), jobs of a session that was replaced. When a watched job ends, its end
//! becomes a note for the agent, in the sandbox's own words, given once: at the next think step
//! of a running turn, or, when the agent is idle, in a turn the controller starts for it (a
//! wake-up) or between turns.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nixxa_contracts::{Event, JobId, SandboxEvent};

/// How every job note starts, so a resumed session can tell which jobs ended.
pub(crate) const NOTE: &str = "Background job: ";

/// The watched jobs, shared by the command loop (which hears the sandbox) and the turn (which
/// starts jobs and gives the agent their notes).
pub(crate) type Shared = Arc<Mutex<Watch>>;

/// The session's own background jobs: those running, and ends not yet given to the agent.
#[derive(Debug, Default)]
pub(crate) struct Watch {
    running: HashMap<JobId, Running>,
    ended: Vec<Ended>,
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
    /// The agent started `job`. It waits on it from now on.
    pub(crate) fn started(&mut self, job: JobId, command: String) {
        self.running.insert(job, Running { command, wakes: true });
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
        let SandboxEvent::JobFinished { job, exit_code, duration_ms, status, .. } = event else { return None };
        let running = self.running.remove(&job)?;
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

/// The note for jobs a stored session started that never reported an end (Nixxa quit while
/// they ran): jobs don't outlive the sandbox they ran in. Said once: the note itself marks them
/// as ended for the next resume.
pub(crate) fn lost_on_resume(started: &[(JobId, String)], notes: &[String]) -> Option<String> {
    let lost: Vec<String> = started
        .iter()
        .filter(|(job, _)| !notes.iter().any(|note| note.contains(&format!("{NOTE}job {job} "))))
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
        let started = vec![(JobId::new("j1"), "cargo build".into()), (JobId::new("j2"), "sleep 9".into())];
        let notes = vec!["Background job: job j1 exited with code 0 after 1.0 s (`cargo build`).".to_string()];
        let said = lost_on_resume(&started, &notes).expect("j2 is lost");
        assert!(said.starts_with("Background job: job j2 didn't survive") && !said.contains("job j1"), "{said}");
        let notes = [notes, vec![said]].concat();
        assert_eq!(lost_on_resume(&started, &notes), None, "said once");
    }
}
