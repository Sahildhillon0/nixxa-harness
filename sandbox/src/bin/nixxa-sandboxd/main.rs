//! `nixxa-sandboxd`: the sandbox daemon. `BwrapSandbox` starts it inside bubblewrap and talks to
//! it over framed JSON on stdin/stdout (see `protocol.rs`). It runs commands and does file
//! operations beneath `/workspace`. Its stderr carries metadata-only logs: never command text,
//! paths, file contents or output.

#[cfg(target_os = "linux")]
mod exec;
#[cfg(target_os = "linux")]
mod files;
#[cfg(target_os = "linux")]
#[path = "../../protocol.rs"]
#[allow(dead_code)] // Shared with the client, which uses the other half.
mod protocol;

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
            (Some(rlimits), Some("--"), Some(command)) if args.len() == 5 => exec::limited(rlimits, command),
            _ => {
                eprintln!("nixxa-sandboxd: usage: --limited <rlimits> -- <command>");
                126
            }
        };
        std::process::exit(code);
    }
    // Keep the daemon's /proc entry out of reach of the commands it runs: they share a user and
    // a PID namespace, so without this any command could open /proc/2/fd/{0,1} and forge frames.
    if rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable).is_err() {
        eprintln!("nixxa-sandboxd: cannot set the dumpable flag");
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
    use tokio::io::Stdout;
    use tokio::sync::mpsc;

    use nixxa_contracts::SandboxAction;

    use crate::exec::{self, Stop};
    use crate::files;
    use crate::protocol::{FrameError, FromDaemon, Limits, Op, Reply, ToDaemon, VERSION, read_frame, write_frame};

    type Inbox = mpsc::Receiver<Result<ToDaemon, FrameError>>;

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
        let mut out = tokio::io::stdout();

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
        if write_frame(&mut out, &FromDaemon::Hello { version: VERSION }).await.is_err() {
            return 4;
        }
        log(format_args!("ready"));

        loop {
            let (id, op) = match inbox.recv().await {
                Some(Ok(ToDaemon::Request { id, op })) => (id, op),
                // Nothing is running, so the request it names has already been answered.
                Some(Ok(ToDaemon::Cancel { .. } | ToDaemon::Hello { .. })) => continue,
                Some(Err(FrameError::Malformed(why))) => {
                    log(format_args!("ignored a malformed frame ({} bytes of error)", why.len()));
                    continue;
                }
                Some(Err(err)) => {
                    log(format_args!("input closed: {}", kind_of(&err)));
                    break;
                }
                None => break,
            };
            let more = match op {
                Op::Ping => send(&mut out, id, Reply::Done).await,
                Op::Reset => {
                    exec::kill_all().await;
                    log(format_args!("reset id={id}"));
                    send(&mut out, id, Reply::Done).await
                }
                Op::Action { action: SandboxAction::Exec { command, cwd, timeout_ms } } => {
                    run_exec(&mut out, &mut inbox, id, &command, cwd.as_deref(), timeout_ms, &limits, &root, &self_exe)
                        .await
                }
                Op::Action { action } => {
                    let (kind, outcome) = match action {
                        SandboxAction::ReadFile { path } => ("read", files::read(&root, &path, limits.read_cap)),
                        SandboxAction::WriteFile { path, content } => {
                            ("write", files::write(&root, &path, &content, limits.write_cap))
                        }
                        SandboxAction::ListDir { path } => ("list", files::list(&root, &path)),
                        SandboxAction::Exec { .. } => unreachable!("handled above"),
                    };
                    let ok = !matches!(outcome, nixxa_contracts::Outcome::Failed { .. });
                    log(format_args!("{kind} id={id} ok={ok}"));
                    send(&mut out, id, Reply::Outcome { outcome, truncated: false }).await
                }
            };
            if !more {
                break;
            }
        }
        exec::kill_all().await;
        0
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_exec(
        out: &mut Stdout,
        inbox: &mut Inbox,
        id: u64,
        command: &str,
        cwd: Option<&str>,
        timeout_ms: Option<u64>,
        limits: &Limits,
        root: &std::os::fd::OwnedFd,
        self_exe: &str,
    ) -> bool {
        let dir = match files::working_dir(root, cwd) {
            Ok(dir) => dir,
            Err(why) => {
                let outcome = nixxa_contracts::Outcome::Failed { message: format!("working directory: {why}") };
                return send(out, id, Reply::Outcome { outcome, truncated: false }).await;
            }
        };
        let started = std::time::Instant::now();
        let (stop_tx, mut stop_rx) = mpsc::channel(1);
        let run = exec::run(command, &dir, timeout_ms, limits, self_exe, &mut stop_rx);
        tokio::pin!(run);
        let mut reset: Option<u64> = None;
        let mut client_gone = false;
        let (reply, exit) = loop {
            tokio::select! {
                done = &mut run => break done,
                msg = inbox.recv(), if !client_gone => match msg {
                    Some(Ok(ToDaemon::Cancel { id: target })) if target == id => {
                        let _ = stop_tx.try_send(Stop::Cancelled);
                    }
                    Some(Ok(ToDaemon::Request { id: other, op: Op::Reset })) => {
                        reset = Some(other);
                        let _ = stop_tx.try_send(Stop::Cancelled);
                    }
                    Some(Ok(ToDaemon::Request { id: other, .. })) => {
                        let busy = Reply::Error { message: "busy: a command is running".into() };
                        if !send(out, other, busy).await {
                            client_gone = true;
                            let _ = stop_tx.try_send(Stop::Cancelled);
                        }
                    }
                    Some(Ok(_)) | Some(Err(FrameError::Malformed(_))) => {}
                    Some(Err(_)) | None => {
                        client_gone = true;
                        let _ = stop_tx.try_send(Stop::Cancelled);
                    }
                },
            }
        };
        let (truncated, what) = match &reply {
            Reply::Outcome { truncated, .. } => (*truncated, "done"),
            Reply::Timeout => (false, "timeout"),
            Reply::Cancelled => (false, "cancelled"),
            _ => (false, "error"),
        };
        log(format_args!(
            "exec id={id} {what} exit={exit:?} ms={} truncated={truncated}",
            started.elapsed().as_millis()
        ));
        if client_gone || !send(out, id, reply).await {
            return false;
        }
        match reset {
            Some(reset_id) => {
                exec::kill_all().await;
                send(out, reset_id, Reply::Done).await
            }
            None => true,
        }
    }

    async fn send(out: &mut Stdout, id: u64, reply: Reply) -> bool {
        let result = match write_frame(out, &FromDaemon::Reply { id, reply }).await {
            // Nothing was written, so the stream is intact: say why instead.
            Err(FrameError::TooLarge(n)) => {
                let message = format!("the result ({n} bytes) is too large to send back");
                write_frame(out, &FromDaemon::Reply { id, reply: Reply::Error { message } }).await
            }
            other => other,
        };
        match result {
            Ok(()) => true,
            Err(err) => {
                log(format_args!("cannot answer id={id}: {}", kind_of(&err)));
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
}
