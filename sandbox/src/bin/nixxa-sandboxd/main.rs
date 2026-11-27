//! `nixxa-sandboxd`: the sandbox daemon. It runs inside bubblewrap, speaks the framed protocol
//! in `protocol.rs` over stdin/stdout, and logs metadata (never output or command text) to
//! stderr.
//!
//! It runs as `--limited` too: that is the supervisor at the top of every job (see `exec.rs`).

#[cfg(target_os = "linux")]
mod exec;
#[cfg(target_os = "linux")]
mod files;
#[cfg(target_os = "linux")]
#[path = "../../job.rs"]
mod job;
#[cfg(target_os = "linux")]
mod jobs;
#[cfg(target_os = "linux")]
mod landlock;
#[cfg(target_os = "linux")]
mod network;
#[cfg(target_os = "linux")]
#[path = "../../patch.rs"]
mod patch;
#[cfg(target_os = "linux")]
#[path = "../../protocol.rs"]
#[allow(dead_code)]
mod protocol;
#[cfg(target_os = "linux")]
mod seccomp;
#[cfg(target_os = "linux")]
mod terminal;

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("nixxa-sandboxd only runs on Linux");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--limited") {
        let code = match (args.get(2), args.get(3).map(String::as_str), args.get(4)) {
            (Some(spec), Some("--"), Some(command)) if args.len() == 5 => exec::limited(spec, command),
            _ => {
                eprintln!("nixxa-sandboxd: usage: --limited <spec> -- <command>");
                126
            }
        };
        std::process::exit(code);
    }
    // No core dumps, and no ptrace by the commands it runs (same user).
    if rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable).is_err() {
        eprintln!("nixxa-sandboxd: cannot set the dumpable flag");
        std::process::exit(1);
    }
    // Orphans of a job whose supervisor died come here instead of escaping to PID 1.
    if rustix::process::set_child_subreaper(Some(rustix::process::getpid())).is_err() {
        eprintln!("nixxa-sandboxd: cannot become a subreaper");
        std::process::exit(1);
    }
    let code = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt.block_on(daemon::serve()),
        Err(err) => {
            eprintln!("nixxa-sandboxd: no runtime: {err}");
            1
        }
    };
    std::process::exit(code);
}

#[cfg(target_os = "linux")]
mod daemon {
    use std::os::fd::OwnedFd;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::io::Stdout;
    use tokio::sync::{mpsc, oneshot};
    use tokio::task::JoinHandle;

    use nixxa_contracts::{Outcome, SandboxAction, TerminalSize};

    use crate::files;
    use crate::job::{End, Table, no_such_job};
    use crate::jobs::{self, Capture, Spec, Supervisors};
    use crate::network::{self, Forwarder};
    use crate::protocol::{FrameError, FromDaemon, Limits, Op, Reply, ToDaemon, VERSION, read_frame, write_frame};
    use crate::terminal::{self, Terminal};

    /// How often orphans of a dead supervisor are looked for.
    const SWEEP_EVERY: Duration = Duration::from_millis(200);

    fn log(line: std::fmt::Arguments<'_>) {
        eprintln!("nixxa-sandboxd: {line}");
    }

    pub(crate) async fn serve() -> i32 {
        // One task reads frames, so a frame is never half-read when the loop below moves on.
        let (tx, mut inbox) = mpsc::channel(16);
        tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            loop {
                let frame = read_frame::<_, ToDaemon>(&mut stdin).await;
                let fatal = !matches!(frame, Ok(_) | Err(FrameError::Malformed(_)));
                if tx.send(frame).await.is_err() || fatal {
                    break;
                }
            }
        });
        // One task writes frames, so replies and events never interleave mid-frame. It stops on
        // the first failed write, which closes `out`.
        let (out, mut outgoing) = mpsc::unbounded_channel::<FromDaemon>();
        tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(msg) = outgoing.recv().await {
                if !write(&mut stdout, msg).await {
                    break;
                }
            }
        });

        let limits = match inbox.recv().await {
            Some(Ok(ToDaemon::Hello { version, limits })) if version == VERSION => limits,
            Some(Ok(ToDaemon::Hello { version, .. })) => {
                log(format_args!("protocol version {version} is not {VERSION}"));
                return 2;
            }
            _ => {
                log(format_args!("no handshake"));
                return 2;
            }
        };
        let root = match files::open_root(files::WORKSPACE) {
            Ok(root) => root,
            Err(err) => {
                log(format_args!("cannot open the workspace: {err}"));
                return 3;
            }
        };
        let self_exe = match std::env::current_exe() {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(err) => {
                log(format_args!("cannot find itself: {err}"));
                return 3;
            }
        };
        // Apply the isolation layers before answering the handshake, so every
        // command inherits them. no_new_privs first.
        let nnp_ok = rustix::thread::set_no_new_privs(true).is_ok();
        let landlock_ok = crate::landlock::apply();
        let seccomp_ok = nnp_ok && crate::seccomp::apply();
        log(format_args!("layers: landlock={landlock_ok} seccomp={seccomp_ok}"));
        let layers = crate::protocol::ActiveLayers { landlock: landlock_ok, seccomp: seccomp_ok };
        let supervisors = Supervisors::default();
        // Bound before the first command runs, so none can take the port first.
        let forwarder = match Forwarder::start(supervisors.clone(), limits.network).await {
            Ok(forwarder) => Some(forwarder),
            Err(err) => {
                log(format_args!("cannot listen on {}: {err}", network::PROXY));
                None
            }
        };
        if out.send(FromDaemon::Hello { version: VERSION, layers }).is_err() {
            return 4;
        }
        log(format_args!("ready"));

        let background =
            Table::new(limits.max_background_jobs as usize, limits.background_output_cap as usize, &limits.job_prefix);
        let mut daemon = Daemon {
            out,
            limits,
            root,
            self_exe,
            supervisors,
            background,
            foreground: None,
            forwarder,
            confine: landlock_ok,
            terminal: None,
        };
        let output = daemon.out.clone();
        let mut sweep = tokio::time::interval(SWEEP_EVERY);
        loop {
            tokio::select! {
                msg = inbox.recv() => match msg {
                    Some(Ok(ToDaemon::Request { id, op })) => daemon.handle(id, op).await,
                    Some(Ok(ToDaemon::Cancel { id })) => daemon.cancel(id),
                    Some(Ok(ToDaemon::TerminalAck { terminal, bytes })) => {
                        if let Some(open) = daemon.terminal.as_ref().filter(|open| open.id == terminal) {
                            open.acknowledge(bytes);
                        }
                    }
                    Some(Ok(ToDaemon::Hello { .. })) => {}
                    Some(Err(FrameError::Malformed(why))) => {
                        log(format_args!("ignored a malformed frame ({} bytes of error)", why.len()));
                    }
                    Some(Err(err)) => {
                        log(format_args!("input closed: {}", kind_of(&err)));
                        break;
                    }
                    None => break,
                },
                _ = daemon.foreground_done(), if daemon.foreground.is_some() => daemon.foreground = None,
                () = output.closed() => {
                    log(format_args!("output closed"));
                    break;
                }
                _ = sweep.tick() => jobs::sweep_orphans(&daemon.supervisors),
            }
        }
        daemon.stop_all("the sandbox stopped").await;
        jobs::sweep_orphans(&daemon.supervisors);
        0
    }

    /// Writes one frame. A reply too large to send is replaced by an error saying so (nothing
    /// was written, so the stream is intact). False once the stream has failed.
    async fn write(stdout: &mut Stdout, msg: FromDaemon) -> bool {
        let result = match write_frame(stdout, &msg).await {
            Err(FrameError::TooLarge(n)) => match msg {
                FromDaemon::Reply { id, .. } => {
                    let message = format!("the result ({n} bytes) is too large to send back");
                    write_frame(stdout, &FromDaemon::Reply { id, reply: Reply::Error { message } }).await
                }
                _ => Ok(()),
            },
            other => other,
        };
        match result {
            Ok(()) => true,
            Err(err) => {
                log(format_args!("cannot write: {}", kind_of(&err)));
                false
            }
        }
    }

    fn kind_of(err: &FrameError) -> &'static str {
        match err {
            FrameError::Eof => "eof",
            FrameError::TooLarge(_) => "oversized frame",
            FrameError::Malformed(_) => "malformed frame",
            FrameError::Io(_) => "io error",
        }
    }

    struct Daemon {
        out: mpsc::UnboundedSender<FromDaemon>,
        limits: Limits,
        root: OwnedFd,
        self_exe: String,
        supervisors: Supervisors,
        background: Table,
        /// `127.0.0.1:3128`, if it could be bound.
        forwarder: Option<Forwarder>,
        /// The agent's foreground command, if one is running.
        foreground: Option<Foreground>,
        /// Whether the agent's commands get their own Landlock layer (the daemon's is enforced).
        confine: bool,
        /// The user's terminal, if open (D-066). Not one of the agent's jobs.
        terminal: Option<Terminal>,
    }

    struct Foreground {
        request: u64,
        /// Taken once the command has been asked to stop.
        stop: Option<oneshot::Sender<()>>,
        task: JoinHandle<()>,
    }

    impl Daemon {
        /// The proxy variables, while the network is on.
        fn network_env(&self) -> Vec<(String, String)> {
            if self.forwarder.as_ref().is_some_and(Forwarder::is_on) { network::env() } else { Vec::new() }
        }

        fn reply(&self, id: u64, reply: Reply) {
            let _ = self.out.send(FromDaemon::Reply { id, reply });
        }

        fn outcome(&self, id: u64, outcome: Outcome) {
            self.reply(id, Reply::Outcome { outcome, truncated: false });
        }

        async fn foreground_done(&mut self) {
            if let Some(fg) = self.foreground.as_mut() {
                let _ = (&mut fg.task).await;
            }
        }

        fn cancel(&mut self, id: u64) {
            if let Some(fg) = self.foreground.as_mut().filter(|fg| fg.request == id) {
                if let Some(stop) = fg.stop.take() {
                    let _ = stop.send(());
                }
            }
        }

        /// Stops the foreground command, every background job and the terminal (saying `why`),
        /// and waits for them to end.
        async fn stop_all(&mut self, why: &str) {
            if let Some(open) = self.terminal.take() {
                open.close(why).await;
            }
            if let Some(mut fg) = self.foreground.take() {
                if let Some(stop) = fg.stop.take() {
                    let _ = stop.send(());
                }
                let _ = fg.task.await;
            }
            for job in self.background.stop_all() {
                job.wait().await;
            }
        }

        async fn handle(&mut self, id: u64, op: Op) {
            match op {
                Op::Ping => self.reply(id, Reply::Done),
                Op::Network { on } => match &self.forwarder {
                    Some(forwarder) => {
                        forwarder.set(on);
                        log(format_args!("network on={on}"));
                        self.reply(id, Reply::Done);
                    }
                    None if !on => self.reply(id, Reply::Done),
                    None => {
                        let message =
                            format!("the sandbox couldn't listen on {} for the network proxy", network::PROXY);
                        self.reply(id, Reply::Error { message });
                    }
                },
                Op::Reset => {
                    self.stop_all("the sandbox was reset").await;
                    log(format_args!("reset id={id}"));
                    self.reply(id, Reply::Done);
                }
                Op::Action { action } => self.act(id, action).await,
                Op::TerminalOpen { terminal, size } => self.open_terminal(id, terminal, size),
                Op::TerminalInput { terminal, data } => match self.live_terminal(terminal) {
                    // Answered by the terminal once written.
                    Some(open) => {
                        if open.write(id, data).is_err() {
                            self.reply(id, Reply::Error { message: "the terminal has ended".into() });
                        }
                    }
                    None => self.reply(id, Reply::Error { message: "the terminal has ended".into() }),
                },
                Op::TerminalResize { terminal, size } => {
                    let reply = match self.live_terminal(terminal).map(|open| open.resize(size)) {
                        Some(Ok(())) => Reply::Done,
                        Some(Err(message)) => Reply::Error { message },
                        None => Reply::Error { message: "the terminal has ended".into() },
                    };
                    self.reply(id, reply);
                }
                Op::TerminalClose { terminal } => match self.terminal.take_if(|open| open.id == terminal) {
                    // Answered once its whole tree is gone.
                    Some(open) => {
                        let out = self.out.clone();
                        tokio::spawn(async move {
                            open.close("it was closed").await;
                            let _ = out.send(FromDaemon::Reply { id, reply: Reply::Done });
                        });
                    }
                    None => self.reply(id, Reply::Done),
                },
            }
        }

        /// The terminal with the client's id `terminal`, if it is still running.
        fn live_terminal(&self, terminal: u64) -> Option<&Terminal> {
            self.terminal.as_ref().filter(|open| open.id == terminal && !open.is_finished())
        }

        /// Opens the user's terminal: one at a time.
        fn open_terminal(&mut self, id: u64, terminal: u64, size: TerminalSize) {
            // Without the agent's own Landlock layer, its commands could open the terminal.
            if !self.confine {
                let message = "no terminal without Landlock: the agent's commands couldn't be kept away from it".into();
                return self.reply(id, Reply::Error { message });
            }
            if self.terminal.as_ref().is_some_and(|open| !open.is_finished()) {
                return self.reply(id, Reply::Error { message: "a terminal is already open".into() });
            }
            // Its CPU time isn't limited to a foreground command's.
            let mut rlimits = self.limits.rlimits;
            rlimits.cpu_secs = rlimits.cpu_secs.max(self.limits.background_max_timeout_ms / 1000);
            let setup = terminal::Setup { self_exe: self.self_exe.clone(), rlimits, env: self.network_env() };
            match Terminal::open(terminal, size, setup, &self.supervisors, &self.out) {
                Ok(open) => {
                    log(format_args!("terminal {terminal} opened"));
                    self.terminal = Some(open);
                    self.reply(id, Reply::Done);
                }
                Err(message) => {
                    self.reply(id, Reply::Error { message: format!("the terminal couldn't open: {message}") })
                }
            }
        }

        async fn act(&mut self, id: u64, action: SandboxAction) {
            match action {
                SandboxAction::Exec { command, cwd, timeout_ms, background } => {
                    let dir = match files::working_dir(&self.root, cwd.as_deref()) {
                        Ok(dir) => dir,
                        Err(why) => {
                            return self.outcome(id, Outcome::Failed { message: format!("working directory: {why}") });
                        }
                    };
                    if background {
                        self.start_background(id, command, dir, timeout_ms);
                    } else {
                        self.start_foreground(id, command, dir, timeout_ms).await;
                    }
                }
                SandboxAction::JobStatus { job } => self.outcome(id, self.background.answer(&job, None)),
                SandboxAction::JobOutput { job, tail_bytes } => {
                    self.outcome(id, self.background.answer(&job, Some(tail_bytes)));
                }
                SandboxAction::JobStop { job } => match self.background.find(&job) {
                    Some(job) => {
                        // Answered once it has ended, which may take a moment (a fork bomb).
                        job.stop();
                        let out = self.out.clone();
                        tokio::spawn(async move {
                            job.wait().await;
                            let outcome = Outcome::Text { content: job.status() };
                            let _ =
                                out.send(FromDaemon::Reply { id, reply: Reply::Outcome { outcome, truncated: false } });
                        });
                    }
                    None => self.outcome(id, no_such_job(&job)),
                },
                SandboxAction::ReadFile { path } => {
                    let outcome = files::read(&self.root, &path, self.limits.read_cap);
                    log(format_args!("read id={id} ok={}", !matches!(outcome, Outcome::Failed { .. })));
                    self.outcome(id, outcome);
                }
                SandboxAction::WriteFile { path, content } => {
                    let outcome = files::write(&self.root, &path, &content, self.limits.write_cap);
                    log(format_args!("write id={id} ok={}", !matches!(outcome, Outcome::Failed { .. })));
                    self.outcome(id, outcome);
                }
                SandboxAction::Patch { path, edits, expected_hash } => {
                    let outcome = files::patch(&self.root, &path, &edits, &expected_hash, self.limits.write_cap);
                    log(format_args!("patch id={id} ok={}", !matches!(outcome, Outcome::Failed { .. })));
                    self.outcome(id, outcome);
                }
                SandboxAction::ListDir { path } => {
                    let outcome = files::list(&self.root, &path);
                    log(format_args!("list id={id} ok={}", !matches!(outcome, Outcome::Failed { .. })));
                    self.outcome(id, outcome);
                }
            }
        }

        async fn start_foreground(&mut self, id: u64, command: String, cwd: String, timeout_ms: Option<u64>) {
            // A command being stopped (its caller went away) is about to end: let it, so the
            // caller's next command doesn't find the daemon busy.
            if let Some(fg) = self.foreground.take_if(|fg| fg.stop.is_none()) {
                let _ = fg.task.await;
            }
            // The last command's task ends right after sending its reply, and the main loop may
            // read the caller's next request before it notices: a finished task is no command.
            if self.foreground.as_ref().is_some_and(|fg| fg.task.is_finished()) {
                self.foreground = None;
            }
            if self.foreground.is_some() {
                return self.reply(id, Reply::Error { message: "busy: a command is running".into() });
            }
            let limits = &self.limits;
            let limit = timeout_ms.unwrap_or(limits.default_timeout_ms).min(limits.max_timeout_ms);
            let spec = Spec {
                command,
                cwd,
                limit: Some(Duration::from_millis(limit)),
                rlimits: limits.rlimits,
                self_exe: self.self_exe.clone(),
                owner: "fg".into(),
                env: self.network_env(),
                confine: self.confine,
                pty: None,
            };
            let capture = Capture::First { cap: limits.output_cap as usize };
            let (stop, stopped) = oneshot::channel();
            let (supervisors, out) = (self.supervisors.clone(), self.out.clone());
            let task = tokio::spawn(async move {
                let ran = jobs::run(spec, capture, &supervisors, stopped).await;
                let (what, reply) = match ran.end {
                    End::TimedOut => ("timeout", Reply::Timeout),
                    End::Stopped => ("cancelled", Reply::Cancelled),
                    End::Failed(message) => {
                        ("error", Reply::Outcome { outcome: Outcome::Failed { message }, truncated: false })
                    }
                    End::Exited(exit_code) => {
                        let outcome = Outcome::Exec {
                            exit_code,
                            stdout: String::from_utf8_lossy(&ran.stdout).into_owned(),
                            stderr: String::from_utf8_lossy(&ran.stderr).into_owned(),
                        };
                        ("done", Reply::Outcome { outcome, truncated: ran.truncated })
                    }
                };
                log(format_args!("exec id={id} {what} ms={} truncated={}", ran.duration.as_millis(), ran.truncated));
                let _ = out.send(FromDaemon::Reply { id, reply });
            });
            self.foreground = Some(Foreground { request: id, stop: Some(stop), task });
        }

        fn start_background(&mut self, id: u64, command: String, cwd: String, timeout_ms: Option<u64>) {
            let (job, handles) = match self.background.add() {
                Ok(added) => added,
                Err(message) => return self.outcome(id, Outcome::Failed { message }),
            };
            let limits = &self.limits;
            let limit =
                timeout_ms.unwrap_or(limits.background_default_timeout_ms).min(limits.background_max_timeout_ms);
            // A long build may use more CPU time than a foreground command is allowed.
            let mut rlimits = limits.rlimits;
            rlimits.cpu_secs = rlimits.cpu_secs.max(limits.background_max_timeout_ms / 1000);
            let spec = Spec {
                command,
                cwd,
                limit: Some(Duration::from_millis(limit)),
                rlimits,
                self_exe: self.self_exe.clone(),
                owner: job.id.to_string(),
                env: self.network_env(),
                confine: self.confine,
                pty: None,
            };
            let capture = Capture::Last { stdout: Arc::clone(&job.stdout), stderr: Arc::clone(&job.stderr) };
            // The reply goes out before the task runs (one thread, no await in between), so
            // `Started` always precedes the job's `JobFinished`.
            self.outcome(id, Outcome::Started { job: job.id.clone() });
            let (supervisors, out) = (self.supervisors.clone(), self.out.clone());
            tokio::spawn(async move {
                let ran = jobs::run(spec, capture, &supervisors, handles.stop).await;
                log(format_args!("job {} ended: {:?} ms={}", job.id, ran.end, ran.duration.as_millis()));
                let event = job.finish(handles.done, ran.end, ran.duration);
                let _ = out.send(FromDaemon::Event { event });
            });
        }
    }
}
