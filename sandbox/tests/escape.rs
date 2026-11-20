//! Escape tests: the real sandbox (bubblewrap + `nixxa-sandboxd`) must not let a command or a
//! file action touch the host outside the workspace, see the host's environment or processes,
//! reach the network, or outgrow its limits.
//!
//! Each test plants canaries OUTSIDE the workspace (in a temp dir next to it, and in the real
//! `$HOME`) and tries to read or overwrite them. Linux only. If bubblewrap is missing the tests
//! say so loudly and skip; any other failure to start fails them.
//!
//! Nothing here runs an agent command on the host: every command goes through `BwrapSandbox`.

#![cfg(target_os = "linux")]

mod common;

use std::io::ErrorKind;
use std::net::TcpListener;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Scratch, exec, is_failed, run, sh, sh_out};

/// Syscall numbers for the calls the seccomp tests make directly. The older calls differ by
/// architecture; numbers from 424 on (io_uring_setup, open_tree) are the same everywhere.
#[cfg(target_arch = "x86_64")]
mod nr {
    pub const ADD_KEY: u32 = 248;
    pub const UNSHARE: u32 = 272;
    pub const CLONE: u32 = 56;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const ADD_KEY: u32 = 217;
    pub const UNSHARE: u32 = 97;
    pub const CLONE: u32 = 220;
}
const IO_URING_SETUP: u32 = 425;
const OPEN_TREE: u32 = 428;

/// Runs `libc.syscall(<args>)` in python inside the sandbox and returns `"<result> <errno>"`.
async fn syscall(sb: &nixxa_sandbox::bwrap::BwrapSandbox, args: &str) -> String {
    let script = format!(
        "python3 -c 'import ctypes; libc=ctypes.CDLL(\"libc.so.6\", use_errno=True); print(libc.syscall({args}), ctypes.get_errno())'"
    );
    sh_out(sb, &script).await
}
use nixxa_contracts::{Outcome, SandboxAction, SandboxError};

const SECRET: &str = "nixxa-canary-7f3a9c1e";

/// A secret file outside the workspace (`<scratch>/outside/canary.txt`) and one in `$HOME`.
struct Canaries {
    scratch: Scratch,
    outside: PathBuf,
    home: Option<PathBuf>,
}

impl Canaries {
    fn plant(tag: &str) -> Self {
        let scratch = Scratch::new(tag);
        std::fs::create_dir_all(scratch.base.join("outside")).unwrap();
        let outside = scratch.base.join("outside/canary.txt");
        std::fs::write(&outside, SECRET).unwrap();
        let home = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_dir()).map(|h| {
            let path = h.join(format!(".nixxa-escape-canary-{}-{tag}", std::process::id()));
            std::fs::write(&path, SECRET).unwrap();
            path
        });
        Self { scratch, outside, home }
    }

    fn workspace(&self) -> PathBuf {
        self.scratch.workspace()
    }

    fn all(&self) -> Vec<PathBuf> {
        let mut all = vec![self.outside.clone()];
        all.extend(self.home.clone());
        all
    }

    /// Every canary still holds the secret, and nothing new appeared next to the outside one.
    fn assert_untouched(&self) {
        for path in self.all() {
            assert_eq!(std::fs::read_to_string(&path).unwrap(), SECRET, "{} was modified", path.display());
        }
        let names: Vec<_> =
            std::fs::read_dir(self.outside.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names, vec![std::ffi::OsString::from("canary.txt")], "a file appeared outside the workspace");
    }
}

impl Drop for Canaries {
    fn drop(&mut self) {
        if let Some(home) = &self.home {
            let _ = std::fs::remove_file(home);
        }
    }
}

fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn assert_no_secret(what: &str, output: &str) {
    assert!(!output.contains(SECRET), "{what} leaked the canary: {output}");
}

#[tokio::test]
async fn absolute_paths_outside_the_workspace_fail() {
    let c = Canaries::plant("absolute");
    let Some(sb) = common::sandbox(&c.workspace()) else { return };
    for canary in c.all() {
        let p = s(&canary);
        let read = run(&sb, SandboxAction::ReadFile { path: p.clone() }).await.unwrap();
        assert!(is_failed(&read), "ReadFile {p}: {read:?}");
        let write = run(&sb, SandboxAction::WriteFile { path: p.clone(), content: "pwned".into() }).await.unwrap();
        assert!(is_failed(&write), "WriteFile {p}: {write:?}");
        let list = run(&sb, SandboxAction::ListDir { path: s(canary.parent().unwrap()) }).await.unwrap();
        assert!(is_failed(&list), "ListDir {p}: {list:?}");
        assert_no_secret("cat", &sh_out(&sb, &format!("cat '{p}'")).await);
        sh(&sb, &format!("echo pwned > '{p}'; echo pwned >> '{p}'; rm -f '{p}'")).await;
        sh(&sb, &format!("touch '{}/new.txt'", s(canary.parent().unwrap()))).await;
    }
    c.assert_untouched();
}

#[tokio::test]
async fn symlinks_to_the_canaries_fail() {
    let c = Canaries::plant("symlink");
    let ws = c.workspace();
    symlink(&c.outside, ws.join("abs")).unwrap();
    symlink("../outside/canary.txt", ws.join("rel")).unwrap();
    symlink(c.outside.parent().unwrap(), ws.join("dir")).unwrap();
    let mut links = vec!["abs", "rel", "dir/canary.txt"];
    if let Some(home) = &c.home {
        symlink(home, ws.join("home")).unwrap();
        links.push("home");
    }
    let Some(sb) = common::sandbox(&ws) else { return };
    for link in links {
        let read = run(&sb, SandboxAction::ReadFile { path: link.into() }).await.unwrap();
        assert!(is_failed(&read), "ReadFile {link}: {read:?}");
        let write = run(&sb, SandboxAction::WriteFile { path: link.into(), content: "pwned".into() }).await.unwrap();
        assert!(is_failed(&write), "WriteFile {link}: {write:?}");
        assert_no_secret("cat", &sh_out(&sb, &format!("cat '{link}'")).await);
        sh(&sb, &format!("echo pwned > '{link}'")).await;
    }
    sh(&sb, "touch dir/new.txt").await;
    let list = run(&sb, SandboxAction::ListDir { path: "dir".into() }).await.unwrap();
    assert!(is_failed(&list), "ListDir through a symlink: {list:?}");
    c.assert_untouched();
}

#[tokio::test]
async fn dot_dot_traversal_fails() {
    let c = Canaries::plant("dotdot");
    let Some(sb) = common::sandbox(&c.workspace()) else { return };
    for path in [
        "../outside/canary.txt",
        "./../outside/canary.txt",
        "a/../../outside/canary.txt",
        "/workspace/../outside/canary.txt",
    ] {
        let read = run(&sb, SandboxAction::ReadFile { path: path.into() }).await.unwrap();
        assert!(is_failed(&read), "ReadFile {path}: {read:?}");
        let write = run(&sb, SandboxAction::WriteFile { path: path.into(), content: "pwned".into() }).await.unwrap();
        assert!(is_failed(&write), "WriteFile {path}: {write:?}");
    }
    for path in ["..", "../outside", "/workspace/.."] {
        let list = run(&sb, SandboxAction::ListDir { path: path.into() }).await.unwrap();
        assert!(is_failed(&list), "ListDir {path}: {list:?}");
    }
    let cwd = run(&sb, SandboxAction::Exec { command: "pwd".into(), cwd: Some("..".into()), timeout_ms: None })
        .await
        .unwrap();
    assert!(is_failed(&cwd), "an Exec cwd of `..`: {cwd:?}");
    assert_no_secret("cat ..", &sh_out(&sb, "cat ../outside/canary.txt; cat ../../outside/canary.txt").await);
    // The parent of /workspace is the sandbox's own root, not the host directory.
    let parent = sh_out(&sb, "ls -a /workspace/..").await;
    assert!(!parent.contains("outside"), "the host parent is visible: {parent}");
    // Search the whole sandbox file system except the kernel views (/proc holds this very
    // command line) and the read-only system files.
    let found = sh_out(
        &sb,
        &format!("grep -rl '{SECRET}' / --exclude-dir=proc --exclude-dir=dev --exclude-dir=usr 2>/dev/null; true"),
    )
    .await;
    assert_no_secret("grep -r /", &found);
    assert!(found.trim().is_empty(), "the canary was found from inside: {found}");
    c.assert_untouched();
}

#[tokio::test]
async fn proc_root_tricks_fail() {
    let c = Canaries::plant("procroot");
    symlink(format!("/proc/self/root{}", s(&c.outside)), c.workspace().join("magic")).unwrap();
    let Some(sb) = common::sandbox(&c.workspace()) else { return };
    for canary in c.all() {
        let p = s(&canary);
        for prefix in ["/proc/self/root", "/proc/1/root", "/proc/2/root", "/proc/thread-self/root"] {
            assert_no_secret(prefix, &sh_out(&sb, &format!("cat '{prefix}{p}'")).await);
            let read = run(&sb, SandboxAction::ReadFile { path: format!("{prefix}{p}") }).await.unwrap();
            assert!(is_failed(&read), "ReadFile {prefix}{p}: {read:?}");
        }
    }
    let read = run(&sb, SandboxAction::ReadFile { path: "magic".into() }).await.unwrap();
    assert!(is_failed(&read), "ReadFile through a /proc magic link: {read:?}");
    assert_no_secret("cat magic", &sh_out(&sb, "cat magic").await);
    let root = sh_out(&sb, "ls /proc/1/root/").await;
    assert!(!root.contains("home") && !root.contains("etc\nhome"), "PID 1's root shows the host: {root}");
    c.assert_untouched();
}

/// Run in a child process (see `the_parent_environment_does_not_leak`), whose environment holds
/// a canary and look-alike secrets.
#[tokio::test]
#[ignore = "run by the_parent_environment_does_not_leak"]
async fn env_child() {
    if std::env::var_os("NIXXA_ESCAPE_ENV_CHILD").is_none() {
        return;
    }
    assert_eq!(std::env::var("NIXXA_CANARY_SECRET").as_deref(), Ok(SECRET), "the canary must be set in the parent");
    let scratch = Scratch::new("env");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let (code, env, _) = sh(&sb, "env").await;
    assert_eq!(code, Some(0));
    assert_no_secret("env", &env);
    let allowed = ["PATH", "HOME", "LANG", "TERM", "PWD", "SHLVL", "_", "OLDPWD"];
    for line in env.lines() {
        let key = line.split_once('=').map_or(line, |(k, _)| k);
        assert!(allowed.contains(&key), "unexpected variable inside the sandbox: {key}");
    }
    assert!(env.contains("HOME=/tmp") && env.contains("TERM=dumb"), "the minimal environment is missing: {env}");
    let proc_env =
        sh_out(&sb, "cat /proc/1/environ /proc/2/environ /proc/self/environ 2>/dev/null | tr '\\0' '\\n'").await;
    assert_no_secret("/proc/*/environ", &proc_env);
}

#[test]
fn the_parent_environment_does_not_leak() {
    let me = std::env::current_exe().unwrap();
    let out = std::process::Command::new(me)
        .args(["--exact", "env_child", "--ignored", "--nocapture"])
        .env("NIXXA_ESCAPE_ENV_CHILD", "1")
        .env("NIXXA_CANARY_SECRET", SECRET)
        .env("ANTHROPIC_API_KEY", SECRET)
        .env("SSH_AUTH_SOCK", format!("/tmp/{SECRET}"))
        .env("DBUS_SESSION_BUS_ADDRESS", format!("unix:path=/tmp/{SECRET}"))
        .env("DISPLAY", ":99")
        .env("WAYLAND_DISPLAY", "wayland-99")
        .output()
        .unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if text.contains("SKIPPED") {
        eprintln!("{text}");
        return;
    }
    assert!(out.status.success(), "the child failed:\n{text}");
    assert!(text.contains("1 passed"), "the child did not run env_child:\n{text}");
}

#[tokio::test]
async fn host_processes_are_invisible() {
    let scratch = Scratch::new("pids");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let host_pid = std::process::id();
    let pids = sh_out(&sb, "for p in /proc/[0-9]*; do echo ${p#/proc/}; done").await;
    let pids: Vec<u32> = pids.lines().filter_map(|l| l.trim().parse().ok()).collect();
    assert!(pids.len() <= 8, "too many processes visible inside: {pids:?}");
    assert!(pids.iter().all(|&p| p < 100), "host-sized PIDs are visible inside: {pids:?}");
    assert!(!pids.contains(&host_pid) || host_pid < 100, "the test process is visible inside");
    let ps = sh_out(&sb, "command -v ps >/dev/null && ps -e -o comm= || echo no-ps").await;
    if !ps.contains("no-ps") {
        assert!(!ps.contains("escape-"), "the host test process shows in ps: {ps}");
        assert!(ps.lines().count() <= 8, "ps shows host processes: {ps}");
    }
    let (_, comm, _) = sh(&sb, "cat /proc/1/comm").await;
    assert_eq!(comm.trim(), "bwrap", "PID 1 inside must be bubblewrap's init");
}

#[tokio::test]
async fn the_network_is_unreachable() {
    let scratch = Scratch::new("net");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    if sh(&sb, "command -v bash").await.0 != Some(0) {
        eprintln!("SKIPPED part of the network test: no bash inside for /dev/tcp");
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let tries = [
        (
            "TCP to 1.1.1.1:443",
            "timeout 5 bash -c 'exec 3<>/dev/tcp/1.1.1.1/443' && echo CONNECTED || echo BLOCKED".to_owned(),
        ),
        (
            "TCP to the host's 127.0.0.1",
            format!("timeout 5 bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}' && echo CONNECTED || echo BLOCKED"),
        ),
        ("DNS", "timeout 5 getent hosts example.com && echo CONNECTED || echo BLOCKED".to_owned()),
    ];
    for (what, command) in tries {
        let out = sh_out(&sb, &command).await;
        assert!(!out.contains("CONNECTED"), "{what} got through: {out}");
        assert!(out.contains("BLOCKED"), "{what}: unexpected output {out}");
    }
    match listener.accept() {
        Err(err) if err.kind() == ErrorKind::WouldBlock => {}
        other => panic!("the host listener got a connection from inside: {other:?}"),
    }
    let links = sh_out(&sb, "cat /proc/net/dev").await;
    let ifaces: Vec<&str> = links.lines().skip(2).filter_map(|l| l.split(':').next()).map(str::trim).collect();
    assert_eq!(ifaces, vec!["lo"], "the sandbox has network interfaces besides lo: {links}");
}

/// The sandbox's own loopback works (local servers, test suites), over IPv4 and IPv6. It is the
/// sandbox's `lo`, not the host's: `the_network_is_unreachable` checks the host stays out of reach.
#[tokio::test]
async fn the_sandbox_loopback_works() {
    let scratch = Scratch::new("loopback");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    if sh(&sb, "command -v python3").await.0 != Some(0) {
        eprintln!("SKIPPED the loopback test: no python3 inside");
        return;
    }
    let script = r#"
import socket
for family, host in ((socket.AF_INET, "127.0.0.1"), (socket.AF_INET6, "::1")):
    server = socket.socket(family, socket.SOCK_STREAM)
    server.bind((host, 0))
    server.listen(1)
    client = socket.create_connection(server.getsockname()[:2], timeout=5)
    conn, _ = server.accept()
    client.sendall(b"ping")
    print(family.name, conn.recv(4).decode())
"#;
    let out = sh_out(&sb, &format!("python3 -c '{script}'")).await;
    assert_eq!(out, "AF_INET ping\nAF_INET6 ping\n", "loopback inside the sandbox: {out}");
}

#[tokio::test]
async fn a_fork_bomb_is_contained() {
    fork_bomb(true).await;
    fork_bomb(false).await;
}

/// With the cgroup scope (`TasksMax`) and with rlimits only (`RLIMIT_NPROC`).
async fn fork_bomb(cgroup: bool) {
    let scratch = Scratch::new("forkbomb");
    let mut config = common::config(&scratch.workspace());
    config.cgroup = cgroup;
    let limit = config.pids_max as usize;
    let Some(sb) = common::sandbox_with(config) else { return };
    // Counting /proc and the busy wait use only shell builtins, so they work with no PIDs left.
    let bomb = "bomb() { bomb | bomb & }; bomb; \
                i=0; while [ $i -lt 300000 ]; do i=$((i+1)); done; \
                n=0; for p in /proc/[0-9]*; do n=$((n+1)); done; echo procs=$n";
    let started = Instant::now();
    let action = SandboxAction::Exec { command: bomb.into(), cwd: None, timeout_ms: Some(15_000) };
    match run(&sb, action).await {
        Ok(obs) => {
            if let Outcome::Exec { stdout, .. } = &obs.outcome {
                if let Some(n) = stdout.lines().find_map(|l| l.strip_prefix("procs=")) {
                    let n: usize = n.trim().parse().unwrap();
                    assert!(n <= limit, "{n} processes ran, over the {limit} limit (cgroup: {cgroup})");
                }
            }
        }
        Err(SandboxError::Timeout) => {}
        Err(err) => panic!("the fork bomb broke the sandbox: {err}"),
    }
    assert!(started.elapsed() < Duration::from_secs(40), "the fork bomb was not stopped");
    // Everything it started is gone, and the sandbox still works.
    let left = sh_out(&sb, "n=0; for p in /proc/[0-9]*; do n=$((n+1)); done; echo $n").await;
    assert!(left.trim().parse::<usize>().unwrap() <= 4, "fork bomb leftovers: {left}");
    assert_eq!(sh(&sb, "echo alive").await.1, "alive\n");
}

#[tokio::test]
async fn a_memory_hog_is_contained() {
    memory_hog(true).await;
    memory_hog(false).await;
}

/// With the cgroup scope (`MemoryMax`) and with rlimits only (`RLIMIT_DATA`).
async fn memory_hog(cgroup: bool) {
    let scratch = Scratch::new("memory");
    let mut config = common::config(&scratch.workspace());
    config.cgroup = cgroup;
    let Some(sb) = common::sandbox_with(config) else { return };
    let hog = SandboxAction::Exec {
        command: "awk 'BEGIN { s = \"xxxxxxxxxxxxxxxx\"; while (1) s = s s }'".into(),
        cwd: None,
        timeout_ms: Some(30_000),
    };
    let started = Instant::now();
    let obs = run(&sb, hog).await.expect("the hog must end before its time limit");
    match obs.outcome {
        Outcome::Exec { exit_code, .. } => {
            assert_ne!(exit_code, Some(0), "the memory hog was not stopped (cgroup: {cgroup})")
        }
        other => panic!("{other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(30));
    assert_eq!(sh(&sb, "echo alive").await.1, "alive\n");
}

#[tokio::test]
async fn sleep_is_killed_by_the_timeout() {
    let scratch = Scratch::new("timeout");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let started = Instant::now();
    let action = SandboxAction::Exec { command: "sleep 1000".into(), cwd: None, timeout_ms: Some(1_000) };
    assert_eq!(run(&sb, action).await.unwrap_err(), SandboxError::Timeout);
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
    let left = sh_out(&sb, "pgrep -c sleep || true").await;
    assert_eq!(left.trim(), "0", "the sleep survived its timeout");
    assert_eq!(sh(&sb, "echo alive").await.1, "alive\n");
}

#[tokio::test]
async fn huge_output_is_truncated() {
    let scratch = Scratch::new("output");
    let config = common::config(&scratch.workspace());
    let cap = config.output_cap as usize;
    let Some(sb) = common::sandbox_with(config) else { return };
    for command in ["yes | head -c 50000000", "yes"] {
        let started = Instant::now();
        let obs = run(&sb, exec(command)).await.unwrap();
        assert!(obs.truncated, "`{command}` was not marked truncated");
        match obs.outcome {
            Outcome::Exec { stdout, .. } => assert!(stdout.len() <= cap, "kept {} bytes", stdout.len()),
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(15), "`{command}` was not stopped at the cap");
    }
    assert_eq!(sh(&sb, "echo alive").await.1, "alive\n");
}

#[tokio::test]
async fn host_sockets_and_home_are_absent() {
    let scratch = Scratch::new("sockets");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let mut paths = vec![
        "/run/user".to_owned(),
        "/run/dbus/system_bus_socket".to_owned(),
        "/var/run/dbus".to_owned(),
        "/tmp/.X11-unix".to_owned(),
        "/home".to_owned(),
        "/root".to_owned(),
        "/sys".to_owned(),
        "/etc/shadow".to_owned(),
        "/etc/ssh".to_owned(),
    ];
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        paths.extend([s(&dir), s(&dir.join("bus")), s(&dir.join("keyring")), s(&dir.join("gnupg"))]);
    }
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(s(Path::new(&home)));
    }
    let checks: String = paths.iter().map(|p| format!("[ -e '{p}' ] && echo FOUND:{p}; ")).collect();
    let found = sh_out(&sb, &format!("{checks}true")).await;
    assert!(!found.contains("FOUND"), "host paths are visible inside: {found}");
    let tmp = sh_out(&sb, "ls -A /tmp").await;
    assert!(tmp.trim().is_empty(), "/tmp inside is not private: {tmp}");
}

#[tokio::test]
async fn ptrace_is_blocked() {
    let scratch = Scratch::new("ptrace");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let out = sh_out(&sb, "python3 -c 'import ctypes; libc=ctypes.CDLL(\"libc.so.6\", use_errno=True); print(libc.ptrace(0, 0, 0, 0), ctypes.get_errno())'").await;
    assert!(out.trim_end().ends_with(" 1"), "ptrace got through: {out}");
}

#[tokio::test]
async fn keyctl_is_blocked() {
    let scratch = Scratch::new("keyctl");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    // SYS_add_key on x86_64 = 248.
    let out = syscall(&sb, &format!("{}, b\"user\", b\"nixxa\", b\"test\", 4, -3", nr::ADD_KEY)).await;
    assert!(out.trim_end().ends_with(" 1"), "keyctl got through: {out}");
}

#[tokio::test]
async fn unshare_is_blocked() {
    let scratch = Scratch::new("unshare");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    for cmd in ["unshare -U true", "unshare -n true"] {
        let (code, _, _) = sh(&sb, cmd).await;
        assert_ne!(code, Some(0), "{cmd} succeeded inside the sandbox");
    }
    let out = syscall(&sb, &format!("{}, 0x10000000", nr::UNSHARE)).await;
    assert!(out.trim_end().ends_with(" 1"), "unshare syscall got through: {out}");
}

/// The mount family. `mount` and `fsopen` need capabilities the sandbox doesn't have, so they
/// fail with EPERM even without seccomp; `open_tree` without `OPEN_TREE_CLONE` needs none, so
/// only the seccomp rule stops it.
#[tokio::test]
async fn mount_calls_are_blocked() {
    let scratch = Scratch::new("mount");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let out = syscall(&sb, &format!("{OPEN_TREE}, -100, b\"/tmp\", 0")).await;
    assert!(out.trim_end().ends_with(" 1"), "open_tree got through: {out}");
    let (code, _, _) = sh(&sb, "mkdir -p /tmp/mnt && mount -t tmpfs tmpfs /tmp/mnt").await;
    assert_ne!(code, Some(0), "mount succeeded inside the sandbox");
}

/// `clone` with a namespace flag. Without seccomp, a new user namespace fails with ENOSPC
/// (bubblewrap's `--disable-userns`); the seccomp rule makes it EPERM.
#[tokio::test]
async fn clone_with_namespace_flags_is_blocked() {
    let scratch = Scratch::new("clone");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    // CLONE_NEWUSER | SIGCHLD
    let out = syscall(&sb, &format!("{}, 0x10000011, 0, 0, 0, 0", nr::CLONE)).await;
    assert!(out.trim_end().ends_with(" 1"), "clone(CLONE_NEWUSER) got through or failed for another reason: {out}");
}

#[tokio::test]
async fn tiocsti_is_blocked() {
    let scratch = Scratch::new("tiocsti");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let out = sh_out(&sb, "python3 -c 'import ctypes, os; libc=ctypes.CDLL(\"libc.so.6\", use_errno=True); print(libc.ioctl(1, 0x5412, b\"x\"), ctypes.get_errno())'").await;
    assert!(out.trim_end().ends_with(" 1"), "TIOCSTI got through: {out}");
}

#[tokio::test]
async fn io_uring_is_blocked() {
    let scratch = Scratch::new("io_uring");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let out = syscall(&sb, &format!("{IO_URING_SETUP}, 1, 0")).await;
    assert!(out.trim_end().ends_with(" 1"), "io_uring_setup got through: {out}");
}

#[tokio::test]
async fn af_netlink_socket_is_blocked() {
    let scratch = Scratch::new("netlink");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let (code, _, _) = sh(&sb, "python3 -c 'import socket; socket.socket(16, 3, 0)'").await;
    assert_ne!(code, Some(0), "AF_NETLINK socket succeeded inside the sandbox");
}

#[tokio::test]
async fn personality_is_blocked() {
    let scratch = Scratch::new("personality");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let out = sh_out(&sb, "python3 -c 'import ctypes; libc=ctypes.CDLL(\"libc.so.6\", use_errno=True); print(libc.personality(0x40000), ctypes.get_errno())'").await;
    assert!(out.trim_end().ends_with(" 1"), "personality got through: {out}");
}

#[tokio::test]
async fn landlock_denies_writes_to_read_only_paths() {
    let scratch = Scratch::new("landlock");
    let Some(sb) = common::sandbox(&scratch.workspace()) else { return };
    let (code, _, _) = sh(&sb, "echo x > /dev/probe").await;
    assert_ne!(code, Some(0), "writing under /usr succeeded (Landlock inactive?)");
}
