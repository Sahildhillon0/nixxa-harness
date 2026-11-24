//! The user's terminal in the real sandbox (D-066): it runs in the same sandbox as the agent's
//! commands, survives everything the agent does, ends with exactly its own tree, isn't visible
//! to the agent, and can't be reached by the agent's commands.

#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::StreamExt;
use nixxa_contracts::{
    DomainRule, JobId, NetResult, NetworkPolicy, OpenTerminal, Outcome, Sandbox, SandboxAction, SandboxError,
    SandboxEvent, TerminalEvent, TerminalSize,
};
use nixxa_sandbox::bwrap::BwrapSandbox;

use common::{Scratch, exec, run, sandbox, sh, sh_out};

const SIZE: TerminalSize = TerminalSize { cols: 120, rows: 40 };

/// An open terminal and everything it has printed so far.
struct Term {
    open: OpenTerminal,
    seen: String,
    /// Where the next search starts.
    cursor: usize,
    closed: Option<String>,
}

impl Term {
    async fn open(sb: &BwrapSandbox) -> Self {
        let open = sb.open_terminal(SIZE).await.expect("open the terminal");
        Self { open, seen: String::new(), cursor: 0, closed: None }
    }

    /// Types `line` and Enter.
    async fn type_line(&mut self, line: &str) {
        self.open.terminal.write(format!("{line}\r").as_bytes()).await.expect("type into the terminal");
    }

    /// Reads until the output past the cursor contains `want`, within 15 s; returns where.
    async fn until(&mut self, want: &str) -> usize {
        let started = Instant::now();
        loop {
            if let Some(at) = self.seen[self.cursor..].find(want) {
                return self.cursor + at;
            }
            assert!(self.closed.is_none(), "the terminal closed ({:?}) before {want:?}: {}", self.closed, self.seen);
            let left = Duration::from_secs(15).saturating_sub(started.elapsed());
            match tokio::time::timeout(left, self.open.events.next()).await {
                Ok(Some(TerminalEvent::Output(bytes))) => self.seen.push_str(&String::from_utf8_lossy(&bytes)),
                Ok(Some(TerminalEvent::Closed { reason })) => self.closed = Some(reason),
                Ok(None) => panic!("the terminal's events ended before {want:?}: {}", self.seen),
                Err(_) => panic!("no {want:?} in the terminal: {}", self.seen),
            }
        }
    }

    /// Runs `command` in the terminal and returns everything shown meanwhile (the echo of what
    /// was typed, then the output). The end marker is computed by the shell, so the echo of
    /// what was typed doesn't match it.
    async fn run(&mut self, command: &str) -> String {
        self.cursor = self.seen.len();
        self.type_line(&format!("{command}; echo \"<<$((6*7))>>\"")).await;
        let end = self.until("<<42>>").await;
        let shown = self.seen[self.cursor..end].to_string();
        self.cursor = end + "<<42>>".len();
        shown
    }

    /// Reads until the terminal ends; returns why.
    async fn closed(&mut self) -> String {
        let started = Instant::now();
        while self.closed.is_none() {
            let left = Duration::from_secs(15).saturating_sub(started.elapsed());
            match tokio::time::timeout(left, self.open.events.next()).await {
                Ok(Some(TerminalEvent::Output(bytes))) => self.seen.push_str(&String::from_utf8_lossy(&bytes)),
                Ok(Some(TerminalEvent::Closed { reason })) => self.closed = Some(reason),
                Ok(None) => panic!("the events ended without Closed"),
                Err(_) => panic!("the terminal didn't close"),
            }
        }
        self.closed.clone().unwrap_or_default()
    }
}

/// The value after the last `name=` in `shown` (the output comes after the echo of the command).
fn value(shown: &str, name: &str) -> String {
    shown.rsplit(&format!("{name}=")).next().and_then(|rest| rest.split_whitespace().next()).unwrap_or("").into()
}

/// How many live (not zombie) processes have exactly this command line.
async fn count(sb: &BwrapSandbox, cmdline: &str) -> usize {
    let script = format!(
        "for p in /proc/[0-9]*; do grep -q '^State:.*Z' $p/status 2>/dev/null && continue; \
         tr '\\0' ' ' < $p/cmdline; echo; done 2>/dev/null | grep -cx '{cmdline} ' || true"
    );
    let (_, stdout, _) = sh(sb, &script).await;
    stdout.trim().parse().expect("a count")
}

async fn start(sb: &BwrapSandbox, command: &str) -> JobId {
    let action = SandboxAction::Exec { command: command.into(), cwd: None, timeout_ms: None, background: true };
    match run(sb, action).await.expect("start").outcome {
        Outcome::Started { job } => job,
        other => panic!("expected Started, got {other:?}"),
    }
}

#[tokio::test]
async fn the_terminal_runs_in_the_same_sandbox() {
    let scratch = Scratch::new("term-same");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut term = Term::open(&sb).await;
    term.type_line("echo from-the-user > made-in-terminal.txt").await;
    let shown = term.run("pwd; hostname; cat made-in-terminal.txt").await;
    assert!(shown.contains("/workspace") && shown.contains("nixxa") && shown.contains("from-the-user"), "{shown}");
    // The agent sees the same files.
    assert_eq!(sh_out(&sb, "cat made-in-terminal.txt").await, "from-the-user\n");
    assert_eq!(std::fs::read_to_string(scratch.workspace().join("made-in-terminal.txt")).unwrap(), "from-the-user\n");
    // A real terminal, with job control.
    let tty = term.run("tty; echo \"term=$TERM\"; [ -t 0 ] && echo interactive").await;
    assert!(tty.contains("/dev/pts/") && tty.contains("term=xterm-256color") && tty.contains("interactive"), "{tty}");
}

#[tokio::test]
async fn the_terminal_survives_everything_the_agent_does() {
    let scratch = Scratch::new("term-survives");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut term = Term::open(&sb).await;
    term.run("KEPT=since-the-start").await;

    // A foreground command that times out.
    let slow = SandboxAction::Exec { command: "sleep 30".into(), cwd: None, timeout_ms: Some(300), background: false };
    assert_eq!(run(&sb, slow).await.unwrap_err(), SandboxError::Timeout);
    // One that is cancelled.
    let cancelled = exec("sleep 30");
    let _ = futures::FutureExt::now_or_never(sb.execute(common::next_id(), cancelled));
    // One that floods its output past the cap.
    let flood = run(&sb, exec("head -c 1000000 /dev/zero")).await.expect("flood");
    assert!(flood.truncated, "{:?}", flood.truncated);
    // One that leaves something behind (killed 250 ms after the shell exits).
    assert_eq!(sh_out(&sb, "sleep 300 & echo left").await, "left\n");
    // A background job that is stopped, and one that ends by itself.
    let job = start(&sb, "sleep 400").await;
    let stopped = run(&sb, SandboxAction::JobStop { job }).await.expect("stop");
    assert!(matches!(&stopped.outcome, Outcome::Text { content } if content.contains("was stopped")), "{stopped:?}");
    start(&sb, "exit 3").await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The same shell, still there.
    let kept = term.run("echo \"kept=$KEPT\"").await;
    assert!(kept.contains("kept=since-the-start"), "the terminal's shell was replaced or killed: {kept}");
    assert!(term.closed.is_none());
}

#[tokio::test]
async fn closing_kills_the_terminals_whole_tree_and_nothing_else() {
    let scratch = Scratch::new("term-close");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let job = start(&sb, "sleep 411").await;
    let mut term = Term::open(&sb).await;
    // A child, a grandchild in a new session, and one that ignores hangups.
    term.run("sleep 410 & setsid sleep 412 & nohup sleep 413 >/dev/null 2>&1 & true").await;
    for cmdline in ["sleep 410", "sleep 412", "sleep 413"] {
        assert_eq!(count(&sb, cmdline).await, 1, "{cmdline} runs in the terminal");
    }
    term.open.terminal.close().await.expect("close");
    assert_eq!(term.closed().await, "it was closed");
    for cmdline in ["sleep 410", "sleep 412", "sleep 413"] {
        assert_eq!(count(&sb, cmdline).await, 0, "closing the terminal must kill {cmdline}");
    }
    assert_eq!(count(&sb, "sleep 411").await, 1, "closing the terminal must not touch the agent's job");
    let status = run(&sb, SandboxAction::JobStatus { job }).await.expect("status");
    assert!(matches!(&status.outcome, Outcome::Text { content } if content.contains("is running")), "{status:?}");
    // And a closed terminal refuses input.
    assert!(term.open.terminal.write(b"echo late\r").await.is_err());
}

#[tokio::test]
async fn closing_kills_the_tree_itself_not_the_orphan_sweep() {
    let scratch = Scratch::new("term-close-now");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut term = Term::open(&sb).await;
    // In a new session and holding nothing of the terminal: nothing about the terminal ending
    // waits for it.
    // (`setsid` forks, so `$!` isn't its pid: the process writes its own.)
    term.run("setsid sh -c 'echo $$ > /tmp/free.pid; exec sleep 414' </dev/null >/dev/null 2>&1 & sleep 0.2").await;
    assert_eq!(count(&sb, "sleep 414").await, 1);
    let free = sh_out(&sb, "cat /tmp/free.pid").await.trim().to_string();
    assert_eq!(sh_out(&sb, &format!("kill -0 {free} && echo alive")).await, "alive\n", "pid {free}");
    term.open.terminal.close().await.expect("close");
    // Gone when `close` returns: killed as part of the terminal's tree. The daemon's sweep of
    // orphans (every 200 ms) would only get it later.
    let alive = sh_out(&sb, &format!("kill -0 {free} 2>/dev/null && echo alive || echo gone")).await;
    assert_eq!(alive, "gone\n", "closing the terminal must kill its whole tree before it returns");
}

#[tokio::test]
async fn exit_and_reset_close_it_with_a_reason() {
    let scratch = Scratch::new("term-exit");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut term = Term::open(&sb).await;
    term.type_line("exit 5").await;
    assert_eq!(term.closed().await, "the shell exited with code 5");
    let mut term = Term::open(&sb).await;
    term.run("true").await;
    sb.reset().await.expect("reset");
    assert_eq!(term.closed().await, "the sandbox was reset");
}

#[tokio::test]
async fn daemon_death_closes_it_with_a_reason() {
    let scratch = Scratch::new("term-daemon");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut term = Term::open(&sb).await;
    term.run("true").await;
    // PID 2 is the daemon.
    let _ = run(&sb, exec("kill -9 2; sleep 5")).await;
    assert_eq!(term.closed().await, "the sandbox stopped");
    // The next terminal starts with the next daemon.
    let mut term = Term::open(&sb).await;
    assert!(term.run("echo again").await.contains("again"));
}

#[tokio::test]
async fn one_terminal_at_a_time() {
    let scratch = Scratch::new("term-one");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let _term = Term::open(&sb).await;
    let second = sb.open_terminal(SIZE).await;
    assert!(
        matches!(&second, Err(SandboxError::Unavailable(why)) if why == "a terminal is already open"),
        "{:?}",
        second.map(|_| "a second terminal")
    );
}

#[tokio::test]
async fn the_agent_cannot_see_the_terminal() {
    let scratch = Scratch::new("term-agent");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut events = sb.subscribe();
    let mut term = Term::open(&sb).await;
    term.run("sleep 1 & wait").await;
    // Not in the job table, by any name.
    for job in ["terminal", "t1", "j1", "1"] {
        for action in [
            SandboxAction::JobStatus { job: JobId::new(job) },
            SandboxAction::JobOutput { job: JobId::new(job), tail_bytes: 100 },
            SandboxAction::JobStop { job: JobId::new(job) },
        ] {
            let obs = run(&sb, action.clone()).await.expect("job action");
            assert!(
                matches!(&obs.outcome, Outcome::Failed { message } if message.contains("no job")),
                "{action:?} must not find the terminal: {:?}",
                obs.outcome
            );
        }
    }
    // Its end is no job's end.
    term.type_line("exit").await;
    term.closed().await;
    let quiet = tokio::time::timeout(Duration::from_millis(800), events.next()).await;
    assert!(quiet.is_err(), "the terminal must send no sandbox event: {quiet:?}");
}

#[tokio::test]
async fn the_terminals_requests_are_its_own_never_the_agents() {
    let scratch = Scratch::new("term-net");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    sb.set_network(NetworkPolicy::Allow(vec![DomainRule::parse("crates.io").unwrap()])).await.unwrap();
    let mut events = sb.subscribe();
    let mut term = Term::open(&sb).await;
    let env = term.run("env | grep -c '^https_proxy=http://127.0.0.1:3128$'").await;
    assert!(env.contains('1'), "the terminal gets the proxy variables commands get: {env}");
    // The agent's command runs while the terminal makes a (blocked) request.
    let agent = run(&sb, exec("sleep 2"));
    let user = term.run("curl -sS -m 5 -o /dev/null https://example.com; echo \"curl=$?\"");
    let (agent, shown) = tokio::join!(agent, user);
    assert!(shown.contains("curl=56") || shown.contains("curl=7"), "{shown}");
    assert!(agent.expect("agent").network.is_empty(), "the terminal's request must not reach the agent");
    let request = loop {
        match tokio::time::timeout(Duration::from_secs(5), events.next()).await {
            Ok(Some(SandboxEvent::NetRequest(request))) => break request,
            Ok(Some(_)) => {}
            other => panic!("the request is logged: {other:?}"),
        }
    };
    assert_eq!(request.job, Some(JobId::new("terminal")), "{request:?}");
    assert!(matches!(request.result, NetResult::Blocked { .. }), "{request:?}");
}

#[tokio::test]
async fn the_terminal_has_no_secret_environment_and_no_host_files() {
    let scratch = Scratch::new("term-secrets");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let secret = "nixxa-terminal-canary-51e7";
    std::fs::create_dir_all(scratch.base.join("outside")).unwrap();
    let outside = scratch.base.join("outside/canary.txt");
    std::fs::write(&outside, secret).unwrap();
    let home: Option<PathBuf> = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_dir()).map(|h| {
        let path = h.join(format!(".nixxa-terminal-canary-{}", std::process::id()));
        std::fs::write(&path, secret).unwrap();
        path
    });
    let mut term = Term::open(&sb).await;
    let names = term.run("env | cut -d= -f1 | sort | tr '\\n' ' '").await;
    let allowed = ["HOME", "LANG", "OLDPWD", "PATH", "PS1", "PWD", "SHLVL", "TERM", "_"];
    for name in names.split_whitespace().filter(|n| n.chars().all(|c| c.is_ascii_uppercase() || c == '_')) {
        assert!(allowed.contains(&name), "unexpected variable {name} in the terminal: {names}");
    }
    for path in std::iter::once(outside).chain(home.clone()) {
        let shown = term.run(&format!("cat {} 2>&1", path.display())).await;
        assert!(!shown.contains(secret), "{} was read from the terminal: {shown}", path.display());
        assert!(shown.contains("No such file"), "{shown}");
    }
    if let Some(home) = home {
        let _ = std::fs::remove_file(home);
    }
}

#[tokio::test]
async fn the_agent_cannot_plant_start_up_files_or_read_the_history() {
    let scratch = Scratch::new("term-home");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    // The review's probe: start-up files in the commands' home that would log every line typed.
    let plant = "for f in .bash_profile .bash_login .profile .bashrc .bash_logout; do \
                 printf '%s\\n' 'PROMPT_COMMAND=\"history 1 >> /tmp/keys\"; echo planted > /tmp/planted' > \"$HOME/$f\"; done; \
                 printf 'set editing-mode vi\\n' > \"$HOME/.inputrc\"; ls -a \"$HOME\"";
    let planted = sh_out(&sb, plant).await;
    assert!(planted.contains(".bash_profile"), "{planted}");
    let mut term = Term::open(&sb).await;
    let env =
        term.run("echo \"home=$HOME env=${ENV-unset} bash_env=${BASH_ENV-unset} pc=${PROMPT_COMMAND-unset}\"").await;
    assert_eq!(value(&env, "home"), "/nixxa-home", "{env}");
    for name in ["env", "bash_env", "pc"] {
        assert_eq!(value(&env, name), "unset", "{name}: {env}");
    }
    term.run("echo typed-secret-123").await;
    term.type_line("exit").await;
    term.closed().await;
    let leaked = sh_out(&sb, "cat /tmp/planted /tmp/keys \"$HOME/.bash_history\" 2>&1; ls -a /nixxa-home 2>&1; cat /nixxa-home/.bash_history 2>&1").await;
    assert!(!leaked.contains("planted\n") && !leaked.contains("typed-secret-123"), "{leaked}");
    assert!(
        leaked.contains("/nixxa-home/.bash_history: Permission denied"),
        "the terminal's home is not the agent's: {leaked}"
    );
    // The user's shell keeps its history at home, where the next terminal finds it.
    let mut term = Term::open(&sb).await;
    let history = term.run("grep -c typed-secret-123 \"$HOME/.bash_history\"").await;
    assert!(history.trim_end().ends_with('1'), "{history}");
}

#[tokio::test]
async fn the_agents_commands_cannot_reach_the_terminal() {
    let scratch = Scratch::new("term-reach");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut term = Term::open(&sb).await;
    let shown = term.run("echo \"pts=$(tty) shell=$$\"").await;
    let (pts, shell) = (value(&shown, "pts"), value(&shown, "shell"));
    assert!(pts.starts_with("/dev/pts/") && shell.parse::<u32>().is_ok(), "{shown}");
    // Reading what the user types, writing to their screen, making a terminal, or going
    // through the shell's file descriptors: all refused.
    for attempt in [
        format!("cat {pts}"),
        format!("echo injected > {pts}"),
        "exec 3<>/dev/ptmx".to_string(),
        "exec 3<>/dev/pts/ptmx".to_string(),
        format!("cat /proc/{shell}/fd/0"),
        // Listing the numbers is allowed (same user); following one is the ptrace-checked part.
        format!("readlink /proc/{shell}/fd/0"),
        format!("cat /proc/{shell}/environ"),
        "exec 3</dev/tty".to_string(),
    ] {
        let (code, out, err) = sh(&sb, &format!("{attempt} && echo REACHED")).await;
        assert!(code != Some(0) && !out.contains("REACHED"), "`{attempt}` must fail: {out}{err}");
    }
    assert!(sh_out(&sb, "tty").await.contains("not a tty"), "the agent's commands have no terminal");
    // TIOCSTI (pushing input into a terminal) is refused even in the user's own shell.
    if sh(&sb, "command -v python3").await.0 == Some(0) {
        let script = "import fcntl, termios\ntry:\n    fcntl.ioctl(0, termios.TIOCSTI, b'x')\n    print('sti=ok')\n\
                      except OSError as e:\n    print('sti=%d' % e.errno)\n";
        let write = SandboxAction::WriteFile { path: "sti.py".into(), content: script.into() };
        run(&sb, write).await.expect("write the script");
        let tiocsti = term.run("python3 sti.py").await;
        assert_eq!(value(&tiocsti, "sti"), "1", "TIOCSTI must be refused (EPERM): {tiocsti}");
    }
    // The terminal is fine.
    assert!(term.run("echo fine").await.contains("fine"));
}
