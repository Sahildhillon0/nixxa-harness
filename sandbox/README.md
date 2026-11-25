# sandbox — acts

Executes an `Action` in isolation and returns an `Observation`. The only place anything actually happens. Enforces its own hard limits.

| | |
|---|---|
| **Owns** | Isolation (one backend per OS; Linux: bubblewrap + Landlock + seccomp), workspace mount, resource limits, network policy, built-in actions, output capture |
| **Does not own** | Deciding what to run |
| **Contract** | Implements `Sandbox` |
| **Depends on** | `contracts` only |
| **Comes alive** | Fake in Stage 2, isolated out-of-process on Linux in Stage 4, network allowlist proxy, terminal PTY and background jobs in Stage 6, patches and checkpoints in Stage 7, LSP and repo map in Stage 9, macOS + Windows backends in Stage 16 |

Backends today: `fake::FakeSandbox` (in memory) and `host::HostSandbox`, which works on the real
files of one host directory with no isolation. The user picks the second at start-up ("This
directory", D-039); it refuses paths that leave the directory, hides the secrets the harness
names from commands, and caps time (120 s default, 600 s max) and output (64 KiB per stream).

Every backend runs **background jobs** (`Exec { background: true }`, then `JobStatus`,
`JobOutput`, `JobStop`, and `SandboxEvent::JobFinished` through `subscribe()`, which says how the
job ended in the same words `JobStatus` uses, or that it was lost with its sandbox), with the limits in
`JobLimits` (4 at once, 30 min default and 2 h max each, the last 1 MiB of each stream; D-063).
The job table and the words the agent reads live in `src/job.rs`, shared by the daemon,
`HostSandbox` (host processes, one process group per job) and `FakeSandbox` (jobs end at once).

`bwrap::BwrapSandbox` (Linux, Stage 4) starts `nixxa-sandboxd`
inside bubblewrap and talks to it over length-prefixed JSON frames on stdio (`src/protocol.rs`,
protocol VERSION 5: requests carry ids and may overlap, the daemon pushes `Event` frames
between replies (D-062), and the user's terminal has its own requests and frames). The daemon applies Landlock (filesystem) and a seccomp deny-list to
itself at start-up, and does reads, writes and listings itself with `openat2(RESOLVE_BENEATH)`
beneath `/workspace`. Each command, foreground or background, runs under its own supervisor
(`nixxa-sandboxd --limited`, a child subreaper that applies the rlimits and starts
`/bin/sh -c`), so a job is exactly that supervisor's descendants, `setsid` and double forks
included; ending a job kills that tree and nothing else (D-061, which replaces D-043's
kill-everything sweep). Limits: a time limit per command, 64 KiB per
output stream (the command is killed past it), rlimits, and a `systemd-run --user --scope`
cgroup with memory and task caps when the user's systemd allows it (otherwise the backend is
named `bubblewrap (rlimits only, no cgroup)`). It reports `isolated: true` only when
namespaces, Landlock and seccomp are all active; a kernel without Landlock runs degraded and
the backend name says so. Decisions: D-040 to D-046, D-061 to D-063, D-066.

**Patches (D-070):** `SandboxAction::Patch` applies search/replace edits against the file's
SHA-256 (every `ReadFile` returns it, as `Outcome::File`). A stale hash fails with the current
one; each search must match exactly once; all the edits apply or none do. The file is replaced
by a new one renamed over it, with the same mode, and a symbolic link is never followed, its own
name included. The logic and the hash are shared (`src/patch.rs`). The daemon opens through
`openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`, `HostSandbox` checks `symlink_metadata` first,
and `FakeSandbox` works in memory.

**Checkpoints (D-071):** `checkpoint()`, `restore(id)` and `diff(from, to)` in every backend.
`FakeSandbox` copies its files; `HostSandbox` and `BwrapSandbox` use `src/checkpoint/`, a
content-addressed store of our own in the directory the harness gives
(`CheckpointConfig { dir, cap }`; created 0700, removed with the sandbox). Blobs are on disk
under their SHA-256; trees are in memory, shared between checkpoints. Unchanged files are
recognised by device, inode, size, mtime and ctime, and the first 2 s of a change are always
re-read: 10,000 files take about 12 ms unchanged and 11 ms with one changed. Past the cap a
checkpoint is `LimitExceeded` and keeps nothing. A fixed list of build directories (`target`,
`node_modules`, …, `EXCLUDED`) and special files are left out, and `.gitignore` is never read.
For `BwrapSandbox`, snapshot and restore run **on the host** over the mounted directory, never
through the daemon, and the store is never mounted. Every entry is hostile:
- everything is relative to held directory descriptors, one name at a time;
- `O_NOFOLLOW` everywhere: links are stored as links, their targets never read;
- FIFOs, sockets and devices are never opened;
- a restore removes a link where a directory was, without following it, and writes files as
  new ones renamed into place;
- on Windows, the checks are by path (`windows.rs`).

**The network (D-064):** `BwrapSandbox` owns a host-side proxy (`src/proxy/`): a Unix socket in a
private directory mounted read-only at `/run/nixxa`, which the daemon forwards from
`127.0.0.1:3128`. HTTPS is CONNECT with an SNI check; plain HTTP is absolute-form, sent as one
request with `Host` from the checked URL; names are resolved on the host and only public
addresses are dialled, IP literals need an exact rule, and every request is logged into the
foreground command's observation and as a `SandboxEvent`. **The allowlist limits which servers
are reached, not which sites they serve:** inside an HTTPS tunnel the `Host` is encrypted, so
allowing a name on a shared CDN lets commands ask that server for its other sites too. The
policy starts Off (no proxy variables, nothing logged). The presets live in `src/network.rs`.
`HostSandbox` reports the network as unrestricted; `FakeSandbox` keeps the policy it is given.

**The user's terminal (D-066):** `Sandbox::open_terminal(size)` → a `Terminal` handle and a stream
of output then `Closed { reason }`. In `BwrapSandbox` the daemon opens a PTY in the sandbox's own
`devpts` and runs bash (or sh) under its own supervisor (`src/bin/nixxa-sandboxd/terminal.rs`; the
supervisor starts a session and takes the PTY as its controlling terminal in `exec.rs`): a job
for rlimits, cgroup and tree tracking, but not the agent's (no time limit or output cap, not in
the job table, no `JobFinished`, untouched by the agent's commands); `close`, `reset` and the
daemon ending stop it, one at a time. Output is flow-controlled (64 KiB ahead of the client's
acknowledgements). The daemon's Landlock layer allows `/dev/ptmx`, `/dev/pts` and the terminal's home
`/nixxa-home` (a tmpfs, so its start-up files and history aren't the agent's); each agent
command's supervisor adds a second layer without them (`landlock.rs`), so the agent's commands
can't open the user's terminal, plant files its shell runs, read its history, or reach its
shell's `/proc/<pid>/fd` and `environ` (they can still see command lines and send signals; and
what the user runs from the workspace is code the agent may have written). TIOCSTI stays refused
by seccomp. Without Landlock there is no terminal. `HostSandbox` runs the user's `$SHELL` on a PTY (`src/host_terminal.rs`, `portable-pty`,
Unix only; Windows says it needs ConPTY), without the hidden variables, killed in `Drop`.
`FakeSandbox` has a scripted one.

Tests: `tests/undo.rs` (patches through the daemon never following a link; links the agent
makes to host files never entering the store; a directory swapped for a link to the host before
an undo; `.gitignore` hiding nothing; a FIFO not hanging a checkpoint), the store's own tests
(`src/checkpoint/tests.rs`: links never read, special files, churn, file and directory swaps,
hard links, locked directories, the cap, metadata put back, and the 10,000-file timing,
`--ignored`), `tests/terminal.rs` (the same sandbox, survives everything the agent does, close kills
its whole tree at once and nothing else, exit / reset / daemon death with reasons, one at a time,
invisible to the agent's job actions and events, its requests attributed to it, no secret
environment or host files, unreachable from the agent's commands, TIOCSTI refused),
`tests/network.rs` (blocked and allowed hosts, private addresses, IP literals, ports, raw
TCP and UDP, Off, the mount, job attribution; two `needs_internet_` tests run with `--ignored`),
the proxy's own tests (`src/proxy/tests.rs`), `tests/bwrap_conformance.rs` (the contract suite), `tests/bwrap.rs` (cancellation, reset,
daemon restart, limits), `tests/jobs.rs` (background jobs: a job's whole tree dies with it and
nothing else does, `setsid` orphans, a job that kills its own supervisor, the limits, reset,
events while a command runs) and `tests/escape.rs` (canaries outside the workspace and in `$HOME`,
symlinks, `..`, `/proc/*/root`, the parent environment, host processes, the network, a fork bomb,
a memory hog, timeouts, huge output, host sockets). They need `bwrap` and unprivileged user
namespaces; without `bwrap` they print a loud SKIPPED banner. On Ubuntu (including CI runners)
AppArmor blocks unprivileged user namespaces, so CI runs
`sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` first.

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole.
