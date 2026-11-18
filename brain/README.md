# brain — thinks

Turns a `Context` into a stream of brain events ending in an `Intent` (say / act / done). Never executes anything.

| | |
|---|---|
| **Owns** | LLM provider adapters (`providers/`), prompt assembly, tool schemas, token counting, context trimming, streaming |
| **Does not own** | Executing actions, policy decisions, storage |
| **Contract** | Implements `Brain` |
| **Depends on** | `contracts` only (gets `Credentials` from the harness) |
| **Comes alive** | Fake in Stage 2, first provider in Stage 3, all providers in Stage 8 |

See [ARCHITECTURE.md](../ARCHITECTURE.md) for how this module fits the whole.

## Stage 3 status

`AnthropicBrain` (`src/providers/anthropic/`) implements `Brain` over the Messages API: raw
HTTP (reqwest, rustls on `ring`, OS trust store), SSE streaming and tool use.

**Setup:** `AnthropicBrain::new(credentials, AnthropicConfig { … })`. It fetches the account's key
from `Credentials` on every request. Config fields:

- `model` (default `claude-sonnet-5-5`; a context's `model` overrides it);
- `max_output_tokens` (64 000, unless the context sets a limit);
- `base_url` and `context_window`;
- `prompt_caching` (on);
- `refusal_fallback` (on: `fallbacks: "default"`, Claude API only);
- `effort` (unset = the model's default; `Medium` is a good start for multi-step tool use).

HTTPS goes through the OS trust store and honours `HTTPS_PROXY` / `HTTP_PROXY` / `NO_PROXY`
(D-030).

**Request:**
- **System prompt:** the instructions, then the primer inside an `<untrusted-context>` block
  that its content can't close.
- **Messages:**
  - user text → user turn;
  - agent text → assistant text;
  - `Content::Action` → `tool_use` (id = `ActionId`);
  - `Content::Observation` → `tool_result` (with `is_error` for `Failed`/`Denied`);
  - Nixxa notices → `[Note from Nixxa: …]`;
  - `Content::Reasoning` → replayed verbatim.

  Same-role turns merge, so every result for one assistant turn lands in one user message.
- **Stable prefix:** rendering is a pure function of the append-only conversation, so each
  request repeats the previous prefix exactly. Signed thinking blocks depend on this.

**Tools:**
- **Sandbox:** `exec`, `read_file`, `write_file`, `list_dir`, sent only when the context offers
  them.
- **Knowledge:** `<source>__<tool>`. Names with characters the API doesn't allow are cleaned and
  given a stable hash suffix.
- **Validation:** tool input is model output, so it's validated strictly. An unknown tool or bad
  input is a `BrainError::Provider`.

**Streaming:**
- **Text** deltas are passed on live.
- **Everything else** (`Reasoning`, `Say`, `Act`, `Done`, `Usage`) is held until `message_stop`.
  That way a call from a refused or `max_tokens`-truncated partial never reaches the controller.
- **Stop reasons:**
  - `end_turn` → `Done`;
  - `tool_use` → no `Done`;
  - `max_tokens` → what arrived, then `Done("stopped: output limit")`;
  - `refusal` → a provider error.
- **Mid-stream fallback:** only the declined model's text is kept.

**Usage and cost:** cost comes from the official price table (`pricing.rs`). An unknown model, or
a step served by several models, is `None`; the price is never guessed.

**Errors:**
- 401/403 → `Auth`;
- 429 → `RateLimited` (with `retry-after`);
- 413 and "prompt is too long" → `ContextTooLarge`;
- other 4xx/5xx and overloaded → `Provider`;
- transport failures → `Network`.

There are no automatic retries. The key never appears in an error or in `Debug`.

**Trimming:**
- **Estimate:** a pessimistic three bytes per token, no network call.
- **What goes:** the oldest whole exchanges first, so a call never loses its result; the newest
  exchange always stays.
- **Effect:** `[Earlier conversation was trimmed …]` is inserted, and no thinking block is sent,
  because the prefix changed.

**`verify` / `verify_key`:** a free live check that reads the model's entry from the Models API.

**Feature `replay`:** `AnthropicBrain::replaying(credentials, config, &Replay)` answers from recorded
responses instead of the network, so the harness can test the real wiring offline. It's test-only;
nothing enables it in a normal build.

**Tests:** recorded SSE fixtures behind an internal transport seam; no network.
`fixtures/generate.py` writes the fixtures from the documented event shapes. The live check is
ignored. Run it by hand (costs a few tokens):

```sh
ANTHROPIC_API_KEY=… cargo test -p nixxa-brain -- --ignored live_conformance
ANTHROPIC_API_KEY=… cargo test -p nixxa-brain -- --ignored live_tool_loop   # request 1 calls a tool, request 2 replays it
```
