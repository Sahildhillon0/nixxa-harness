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

Two providers share `src/providers/common/`:
- the HTTP transport, the SSE parser and the replay transport;
- tool definitions and strict input validation;
- trimming, the system prompt with its `<untrusted-context>` primer block, and observation text;
- key scrubbing.

### OpenAI-compatible (`src/providers/openai_compat/`)

`OpenAiCompatBrain` speaks the Chat Completions API (`/chat/completions` with `stream: true` and
`stream_options.include_usage`, plus `/models`). It's for local models through LM Studio
(`http://localhost:1234/v1`) or Ollama (`http://localhost:11434/v1`), and later for hosted
OpenAI-compatible providers.

- **Accounts and auth:**
  - a `Credential::Local { base_url }` account supplies the server and needs no auth header;
  - an `ApiKey` account uses `OpenAiCompatConfig::base_url` with `Authorization: Bearer`. A key is
    never sent over plain http to anything but this machine.
- **Messages:** the system prompt, then the conversation.
  - Consecutive user text merges, because strict chat templates need alternating roles.
  - One step's text and calls make one assistant message, followed by one `tool` message per
    result (`Error: …` for denials and failures).
  - A missing result is filled in and an orphan dropped. Other providers' reasoning is skipped.
- **Streaming:**
  - text deltas are passed on live; text and calls are held until the stream ends;
  - any completed call counts, even when a server finishes with `stop` (Ollama sends whole calls);
  - `length` keeps the text and drops the cut-off call, then `Done("stopped: output limit")`;
  - `content_filter` and an `error` chunk are provider errors;
  - a stream may end without `[DONE]` once a choice has finished.
- **Cost:** `Some(0)` for a local server; the reported `usage.cost` (US dollars) when the config
  says the server reports one (OpenRouter); otherwise `None`, never guessed.
- **OpenRouter:** `OpenAiCompatConfig::openrouter(model)` uses `https://openrouter.ai/api/v1`,
  a signed-in `BearerToken` as `Authorization: Bearer`, and the recommended `HTTP-Referer` /
  `X-OpenRouter-Title` headers. `verify` first checks the key at `GET /key`.
- **No tools:** a server's "does not support tools" error (any common wording) becomes
  ``Provider("`<model>` can't use tools. Pick a tool-capable model …")``.
- **`list_models`:** the ids, plus tool support (`supported_parameters` / `capabilities`) and free
  (`:free`, or both prices 0), only when the server reports them. `verify` checks that the
  configured model is offered and lists what is.
- **Defaults:** a 32k context window, 4k output, and a 15-minute read timeout (local models can
  take minutes before the first token on a CPU).
- **Live check:** run by hand with a server running:
  `NIXXA_LOCAL_MODEL=… cargo test -p nixxa-brain -- --ignored live_local_tool_loop`.

### Anthropic (`src/providers/anthropic/`)

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
