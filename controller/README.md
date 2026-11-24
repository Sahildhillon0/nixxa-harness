# controller — decides

Runs the agent loop and is the only orchestrator: asks the brain, gates every action through policy, dispatches to the sandbox, records to the db, emits events.

| | |
|---|---|
| **Owns** | Loop state machine, policy gate (allow / ask / deny), routing to sandbox or knowledge, session primer and write-back, budgets, interrupts, retries |
| **Does not own** | How thinking, execution, storage or rendering work |
| **Contract** | Consumes `Brain`, `Sandbox`, `Store`, `Knowledge`, `ApprovalPort`, `EventSink` |
| **Depends on** | `contracts` only |
| **Comes alive** | Minimal in Stage 2; job results and wake-ups in Stage 6; redaction and project policy in Stage 7; plan mode and the verify loop in Stage 8; full policy and budgets in Stage 14 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole.

## Stage 2 status

The loop from ARCHITECTURE.md §4 runs end to end against fakes:

- **Policy:** every action is `Ask` → `ApprovalPort`. A port answer of `Ask` counts as a denial.
- **Routing:** `Action::Sandbox` → `Sandbox::execute`; `Action::Knowledge` → `Knowledge::call` / `record`.
- **Recording:** user, agent, reasoning, proposal (`Content::Action`) and observation messages go to
  the conversation and the `Store`; each action is stored as an `ActionRecord` with its final verdict.
- **One step, one message:** a step's whole output (reasoning, text, every proposed action) is
  recorded in stream order before any action runs. The observations then follow together. If the
  step stops early (interrupt, infrastructure error), every proposal that never ran is closed with a
  `Denied` observation, so each proposal has a result (D-028).
- **Turns:** one at a time; messages sent during a turn are queued. The conversation carries across turns.
- **Ending a turn:** `Done`, a brain error, an infrastructure error from the sandbox or knowledge,
  a step with no intent, the per-turn step cap (`max_steps`, announced as a system message), or an interrupt.
- **Interrupt / shutdown:** signals the turn, which stops at its next cancellation point (brain
  stream, approval prompt or action), closes an open action with a `Denied`/`Failed` observation
  and records "interrupted by the user". Store writes are never cancelled.
- **Shown vs recorded:** `Done` summaries are stored in the db only, as `Content::Summary`
  (D-056). They are not announced, and they're kept out of the conversation the brain sees,
  because they aren't part of the model's message (D-029). A `Say` streams `TextDelta`s and then announces the full text, so the
  harness should replace the streamed text with the final message, not append both.

## Stage 5: resume (D-057)

`Controller::resume_session(id)` continues a stored session instead of creating one; new
messages are appended to it.

- **UI:** after the usual start events, the history is told again with the existing events:
  `Message` for each text message, `ActionProposed` and `ActionFinished` for each past action
  (the observation carries the outcome: done, denied or failed), with the turn ids the session
  had. No turn starts, and turn numbers carry on.
- **Brain:** the stored conversation comes back as it was, minus `Content::Summary` (D-029).
- **A tool call that was never answered** (Nixxa quit or crashed while it waited for approval or
  ran) is closed first with a `Failed` observation and a `Deny` record: "not finished: Nixxa quit
  before this action finished, so it may not have run, or may have run only in part". Providers
  reject a tool call with no result, and the agent mustn't assume either way.
- **Notice:** a system message, shown and stored, tells the agent the session was resumed and
  that the workspace may have changed (the place isn't stored: it is chosen at every launch).
  When the session was recorded with another account, it says so and continues with the current
  one; a missing or different account never fails the resume.

## Stage 6: job results and wake-ups (D-068)

The loop subscribes to the sandbox's events. Jobs are watched from the `Started` observation of
the agent's own background `exec` (`src/jobs.rs`); nothing else counts (network requests, the
user's terminal, other sessions' jobs).

- **The note:** a watched job's end is emitted as `Event::JobFinished` and given to the agent
  once, as a system message in the sandbox's words (`Background job: job j1 exited with code 0
  after 3.1 s (\`cargo build\`).`), at the next think step: never between an action and its
  result.
- **Wake-ups:** idle, and the agent was waiting on the job (it started it, and the user hasn't
  written since; not if the agent stopped it), the controller starts a turn: `Event::WakeUp`, a
  recorded system message saying so, then the notes. Ends within `wake_delay` (1 s) share one
  turn; `max_wakes` (3) in a row at most, after which the notes are recorded between turns. Never
  during a turn or an approval. A user message cancels a pending wake; its turn gets the notes.
  Wake-up turns are ordinary turns: the same gate (Ask asks) and step cap.
- **Early ends:** an end that comes before the job's `Started` (it failed at once) is kept, up to
  32, and taken when the job is known.
- **Resume:** jobs a stored session started and never saw end are said lost, once; starts and
  ends are paired in order, since ids start again at `j1` in every launch.
- **Not covered:** a background `exec` interrupted while it starts may leave a job running that
  the agent never hears of (the cancel stops only foreground commands); reset and the session's
  end stop it.

Not yet: secret redaction and `AGENTS.md` policy (Stage 7), plan mode and the test loop (Stage 8), token/time/cost budgets and LOCKED (Stage 14), retries, session write-back (Stage 10).

Tests: `cargo test -p nixxa-controller`. The fakes live in `tests/controller.rs` because the
controller may depend only on `contracts`.
