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

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole.
