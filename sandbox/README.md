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
`JobOutput`, `JobStop`, and `SandboxEvent::JobFinished` through `subscribe()`), with the limits in
`JobLimits` (4 at once, 30 min default and 2 h max each, the last 1 MiB of each stream; D-063).
The job table and the words the agent reads live in `src/job.rs`, shared by the daemon,
`HostSandbox` (host processes, one process group per job) and `FakeSandbox` (jobs end at once).

`bwrap::BwrapSandbox` (Linux, Stage 4) starts `nixxa-sandboxd`
inside bubblewrap and talks to it over length-prefixed JSON frames on stdio (`src/protocol.rs`,
protocol VERSION 3: requests carry ids and may overlap, and the daemon pushes `Event` frames
between replies; D-062). The daemon applies Landlock (filesystem) and a seccomp deny-list to
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
the backend name says so. Decisions: D-040 to D-046, D-061 to D-063.

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

Tests: `tests/network.rs` (blocked and allowed hosts, private addresses, IP literals, ports, raw
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
