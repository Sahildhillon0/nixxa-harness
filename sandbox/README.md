# sandbox — acts

Executes an `Action` in isolation and returns an `Observation`. The only place anything actually happens. Enforces its own hard limits.

| | |
|---|---|
| **Owns** | Isolation (one backend per OS; Linux: bubblewrap + Landlock + seccomp), workspace mount, resource limits, network policy, built-in actions, output capture |
| **Does not own** | Deciding what to run |
| **Contract** | Implements `Sandbox` |
| **Depends on** | `contracts` only |
| **Comes alive** | Fake in Stage 2, isolated out-of-process on Linux in Stage 4, macOS + Windows backends in Stage 10 |

Backends today: `fake::FakeSandbox` (in memory) and `host::HostSandbox`, which works on the real
files of one host directory with no isolation. The user picks the second at start-up ("This
directory", D-039); it refuses paths that leave the directory, hides the secrets the harness
names from commands, and caps time (120 s default, 600 s max) and output (64 KiB per stream).

`bwrap::BwrapSandbox` (Linux, Stage 4, not wired into the harness yet) starts `nixxa-sandboxd`
inside bubblewrap and talks to it over length-prefixed JSON frames on stdio (`src/protocol.rs`).
The daemon runs commands as `/bin/sh -c` in their own process group and does reads, writes and
listings itself with `openat2(RESOLVE_BENEATH)` beneath `/workspace`. Limits: a time limit per
command, 64 KiB per output stream (the command is killed past it), rlimits, and a
`systemd-run --user --scope` cgroup with memory and task caps when the user's systemd allows it
(otherwise the backend is named `bubblewrap (rlimits only, no cgroup)`). It reports
`isolated: false` until Landlock and seccomp are added. Decisions: D-040 to D-043.

Tests: `tests/bwrap_conformance.rs` (the contract suite), `tests/bwrap.rs` (cancellation, reset,
daemon restart, limits) and `tests/escape.rs` (canaries outside the workspace and in `$HOME`,
symlinks, `..`, `/proc/*/root`, the parent environment, host processes, the network, a fork bomb,
a memory hog, timeouts, huge output, host sockets). They need `bwrap` and unprivileged user
namespaces; without `bwrap` they print a loud SKIPPED banner. On Ubuntu (including CI runners)
AppArmor blocks unprivileged user namespaces, so CI runs
`sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` first.

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole.
