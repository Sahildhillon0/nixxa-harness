//! What every backend's background jobs share: the table of jobs and its limit, the kept tail
//! of their output, how a job ended, and the words the agent reads about it.
//!
//! This file is compiled into both the library and the daemon binary.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};

use nixxa_contracts::{JobId, Outcome, SandboxEvent};

/// Ended jobs kept for `JobStatus` / `JobOutput`.
const KEPT_ENDED: usize = 32;
/// The most one `JobOutput` shows of each stream, like a foreground command's output cap (the
/// table may keep more).
pub(crate) const SHOWN_CAP: usize = 64 * 1024;

/// The background jobs of one sandbox: running, and recently ended.
#[derive(Debug)]
pub(crate) struct Table {
    /// Oldest first.
    jobs: Vec<Arc<Job>>,
    next: u64,
    /// Put before every id, so ids stay unique across tables of one sandbox (a restarted daemon).
    prefix: String,
    max_running: usize,
    output_cap: usize,
}

/// What the code running a job needs: when to stop, and how to say it has ended.
pub(crate) struct Handles {
    /// Fires (or closes) when the job must stop.
    pub stop: oneshot::Receiver<()>,
    /// Passed to [`Job::finish`].
    pub done: Done,
}

/// Wakes [`Job::wait`]ers when the job has ended.
pub(crate) struct Done(watch::Sender<bool>);

impl Table {
    /// A table allowing `max_running` jobs at once, each keeping `output_cap` bytes per stream,
    /// with ids `<prefix>j1`, `<prefix>j2`, …
    pub(crate) fn new(max_running: usize, output_cap: usize, prefix: &str) -> Self {
        Self { jobs: Vec::new(), next: 1, prefix: prefix.to_string(), max_running, output_cap }
    }

    /// Starts tracking a new job, or says why it can't start.
    pub(crate) fn add(&mut self) -> Result<(Arc<Job>, Handles), String> {
        let running = self.jobs.iter().filter(|job| job.running()).count();
        if running >= self.max_running {
            return Err(format!(
                "{running} background jobs are already running, the most allowed; \
                 wait for one to finish or stop one first"
            ));
        }
        let (stop_tx, stop) = oneshot::channel();
        let (done, done_rx) = watch::channel(false);
        let job = Arc::new(Job {
            id: JobId::new(format!("{}j{}", self.prefix, self.next)),
            started: Instant::now(),
            stop: Mutex::new(Some(stop_tx)),
            stdout: Arc::new(Mutex::new(Tail::new(self.output_cap))),
            stderr: Arc::new(Mutex::new(Tail::new(self.output_cap))),
            end: Mutex::new(None),
            done: done_rx,
        });
        self.next += 1;
        self.jobs.push(Arc::clone(&job));
        // Forget the oldest ended jobs past the limit.
        let mut ended = self.jobs.iter().filter(|job| !job.running()).count();
        self.jobs.retain(|job| {
            let forget = ended > KEPT_ENDED && !job.running();
            ended -= usize::from(forget);
            !forget
        });
        Ok((job, Handles { stop, done: Done(done) }))
    }

    /// The job with this id, if it is still known.
    pub(crate) fn find(&self, id: &JobId) -> Option<Arc<Job>> {
        self.jobs.iter().find(|job| &job.id == id).cloned()
    }

    /// Answers `JobStatus` (`tail_bytes: None`) or `JobOutput`.
    pub(crate) fn answer(&self, id: &JobId, tail_bytes: Option<u64>) -> Outcome {
        let Some(job) = self.find(id) else { return no_such_job(id) };
        let status = job.status();
        let Some(n) = tail_bytes else { return Outcome::Text { content: status } };
        let n = usize::try_from(n).unwrap_or(usize::MAX).min(self.output_cap).min(SHOWN_CAP);
        let (Ok(out), Ok(err)) = (job.stdout.lock(), job.stderr.lock()) else {
            return Outcome::Failed { message: "the output is unavailable".into() };
        };
        Outcome::Text { content: output_text(&status, &out, &err, n) }
    }

    /// Asks every job to stop; the caller waits for the returned ones. They stay in the table,
    /// so their status says they were stopped.
    pub(crate) fn stop_all(&self) -> Vec<Arc<Job>> {
        self.jobs.iter().for_each(|job| job.stop());
        self.jobs.clone()
    }
}

/// "There is no such job", as an outcome.
pub(crate) fn no_such_job(id: &JobId) -> Outcome {
    Outcome::Failed { message: format!("there is no job `{id}` in this sandbox") }
}

/// One background job.
#[derive(Debug)]
pub(crate) struct Job {
    /// Its id.
    pub id: JobId,
    started: Instant,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    /// The last part of its stdout.
    pub stdout: Arc<Mutex<Tail>>,
    /// The last part of its stderr.
    pub stderr: Arc<Mutex<Tail>>,
    /// How it ended and how long it ran; `None` while running.
    end: Mutex<Option<(End, Duration)>>,
    done: watch::Receiver<bool>,
}

impl Job {
    /// Whether it is still running.
    pub(crate) fn running(&self) -> bool {
        self.end.lock().is_ok_and(|end| end.is_none())
    }

    /// One line: running, or how it ended.
    pub(crate) fn status(&self) -> String {
        match self.end.lock().ok().and_then(|end| end.clone()) {
            Some((end, ran)) => status_line(self.id.as_str(), Some(&end), ran),
            None => status_line(self.id.as_str(), None, self.started.elapsed()),
        }
    }

    /// Asks it to stop (once).
    pub(crate) fn stop(&self) {
        if let Some(stop) = self.stop.lock().ok().and_then(|mut stop| stop.take()) {
            let _ = stop.send(());
        }
    }

    /// Waits until it has ended.
    pub(crate) async fn wait(&self) {
        let mut done = self.done.clone();
        let _ = done.wait_for(|done| *done).await;
    }

    /// Records how it ended, and returns the `JobFinished` to send.
    pub(crate) fn finish(&self, done: Done, end: End, ran: Duration) -> SandboxEvent {
        let exit_code = end.exit_code();
        let truncated = [&self.stdout, &self.stderr].iter().any(|tail| tail.lock().is_ok_and(|tail| tail.cut()));
        let status = status_line(self.id.as_str(), Some(&end), ran);
        if let Ok(mut slot) = self.end.lock() {
            *slot = Some((end, ran));
        }
        let _ = done.0.send(true);
        SandboxEvent::JobFinished {
            job: self.id.clone(),
            exit_code,
            duration_ms: u64::try_from(ran.as_millis()).unwrap_or(u64::MAX),
            truncated,
            status,
        }
    }
}

/// The last `cap` bytes written to a stream, and whether earlier bytes were dropped.
#[derive(Debug, Default)]
pub(crate) struct Tail {
    cap: usize,
    bytes: VecDeque<u8>,
    cut: bool,
}

impl Tail {
    /// An empty tail keeping at most `cap` bytes.
    pub(crate) fn new(cap: usize) -> Self {
        Self { cap, bytes: VecDeque::new(), cut: false }
    }

    /// Appends `chunk`, dropping the oldest bytes past the cap.
    pub(crate) fn push(&mut self, chunk: &[u8]) {
        let chunk = if chunk.len() > self.cap {
            self.cut = true;
            &chunk[chunk.len() - self.cap..]
        } else {
            chunk
        };
        let over = (self.bytes.len() + chunk.len()).saturating_sub(self.cap);
        if over > 0 {
            self.cut = true;
            self.bytes.drain(..over);
        }
        self.bytes.extend(chunk);
    }

    /// Whether bytes were dropped.
    pub(crate) fn cut(&self) -> bool {
        self.cut
    }

    /// The last `n` kept bytes, as text.
    pub(crate) fn last(&self, n: usize) -> String {
        let skip = self.bytes.len().saturating_sub(n);
        let bytes: Vec<u8> = self.bytes.iter().skip(skip).copied().collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// How a job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum End {
    /// The shell exited with this code, or was killed by a signal (`None`).
    Exited(Option<i32>),
    /// It ran past its time limit and was stopped.
    TimedOut,
    /// It was stopped (`JobStop`, a reset, or the session ending).
    Stopped,
    /// It could not be started.
    Failed(String),
}

impl End {
    /// The exit code `JobFinished` reports: only for a shell that exited by itself.
    pub(crate) fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Exited(code) => *code,
            _ => None,
        }
    }
}

/// One line on a job: running, or how it ended.
pub(crate) fn status_line(job: &str, end: Option<&End>, ran: Duration) -> String {
    let ran = seconds(ran);
    match end {
        None => format!("job {job} is running ({ran} so far)"),
        Some(End::Exited(Some(code))) => format!("job {job} exited with code {code} after {ran}"),
        Some(End::Exited(None)) => format!("job {job} was killed by a signal after {ran}"),
        Some(End::TimedOut) => format!("job {job} hit its time limit and was stopped after {ran}"),
        Some(End::Stopped) => format!("job {job} was stopped after {ran}"),
        Some(End::Failed(why)) => format!("job {job} could not start: {why}"),
    }
}

/// The status line, then the last `n` bytes of each stream.
pub(crate) fn output_text(status: &str, stdout: &Tail, stderr: &Tail, n: usize) -> String {
    let mut text = status.to_string();
    for (name, tail) in [("stdout", stdout), ("stderr", stderr)] {
        let shown = tail.last(n);
        let earlier = if tail.cut() || shown.len() < tail.bytes.len() { ", earlier output dropped" } else { "" };
        text.push_str(&format!("\n--- {name} (last {} bytes{earlier}) ---\n{shown}", shown.len()));
    }
    text
}

fn seconds(d: Duration) -> String {
    format!("{:.1} s", d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tail_keeps_the_last_bytes() {
        let mut tail = Tail::new(5);
        tail.push(b"abc");
        assert_eq!((tail.last(10).as_str(), tail.cut()), ("abc", false));
        tail.push(b"defg");
        assert_eq!((tail.last(10).as_str(), tail.cut()), ("cdefg", true));
        tail.push(b"0123456789");
        assert_eq!(tail.last(10), "56789");
        assert_eq!(tail.last(2), "89");
    }

    #[test]
    fn the_status_says_how_it_ended() {
        let s = Duration::from_millis(1500);
        assert_eq!(status_line("j1", None, s), "job j1 is running (1.5 s so far)");
        assert_eq!(status_line("j1", Some(&End::Exited(Some(2))), s), "job j1 exited with code 2 after 1.5 s");
        assert_eq!(status_line("j1", Some(&End::TimedOut), s), "job j1 hit its time limit and was stopped after 1.5 s");
        assert_eq!(End::Stopped.exit_code(), None);
        assert_eq!(End::Exited(Some(4)).exit_code(), Some(4));
    }

    #[tokio::test]
    async fn the_table_limits_running_jobs_and_answers_for_them() {
        let mut table = Table::new(2, 16, "");
        let (first, handles) = table.add().unwrap();
        let (_second, _keep) = table.add().unwrap();
        let Err(refused) = table.add() else { panic!("a third job must be refused") };
        assert!(refused.starts_with("2 background jobs are already running"), "{refused}");
        assert_ne!(first.id, _second.id);
        first.stdout.lock().unwrap().push(b"hello");
        let event = first.finish(handles.done, End::Exited(Some(3)), Duration::from_secs(1));
        assert!(
            matches!(&event, SandboxEvent::JobFinished { exit_code: Some(3), status, .. } if status == "job j1 exited with code 3 after 1.0 s"),
            "{event:?}"
        );
        first.wait().await;
        assert!(table.add().is_ok(), "an ended job frees its place");
        let Outcome::Text { content } = table.answer(&first.id, Some(100)) else { panic!() };
        assert!(content.starts_with("job j1 exited with code 3 after 1.0 s"), "{content}");
        assert!(content.contains("hello"), "{content}");
        assert!(matches!(table.answer(&JobId::new("nope"), None), Outcome::Failed { .. }));
        let stopped = table.stop_all();
        assert_eq!(stopped.len(), 3);
        assert!(table.find(&first.id).is_some(), "stopped jobs are still known");
        let mut later = Table::new(1, 16, "2-");
        assert_eq!(later.add().map(|(job, _)| job.id.to_string()).ok().as_deref(), Some("2-j1"));
    }

    #[test]
    fn the_output_shows_both_streams() {
        let (mut out, mut err) = (Tail::new(100), Tail::new(100));
        out.push(b"hello\n");
        err.push(b"warn\n");
        let text = output_text("job j1 is running (0.0 s so far)", &out, &err, 3);
        assert_eq!(
            text,
            "job j1 is running (0.0 s so far)\n--- stdout (last 3 bytes, earlier output dropped) ---\nlo\n\
             \n--- stderr (last 3 bytes, earlier output dropped) ---\nrn\n"
        );
    }
}
