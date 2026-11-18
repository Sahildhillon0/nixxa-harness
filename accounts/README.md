# accounts — holds the keys

Stores every account and credential you add, for any model provider or integration (many
accounts per provider), and hands out a valid credential on request.

| | |
|---|---|
| **Owns** | Account registry, OS keychain storage (Secret Service / Keychain / Credential Manager), encrypted-file fallback, sign-in flows where permitted, token refresh/revocation, key validation, env-var accounts, redaction |
| **Does not own** | Calling model APIs (brain), choosing a session's account (controller/harness), UI (harness) |
| **Contract** | Implements `Credentials` |
| **Depends on** | `contracts` only |
| **Comes alive** | In-memory fake in Stage 2; API keys + keychain in Stage 3; all methods, multi-account and switching in Stage 8 |

| Method | Examples |
|---|---|
| API key | Anthropic Console, OpenAI, Gemini, OpenRouter, Mistral, Groq, Context7, OpenAI-compatible |
| Cloud | Bedrock, Vertex AI, Azure / Microsoft Foundry |
| Sign-in (OAuth) | only providers that officially allow third-party apps |
| Local | Ollama, LM Studio, llama.cpp |
| Env | `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, … (read-only) |

Never hands a credential to the sandbox; never lets one reach logs, the db, the vault,
claude-mem or the model's context. See [ARCHITECTURE.md § 6](../ARCHITECTURE.md#6-accounts-and-credentials).

## Stage 3 status

`KeychainCredentials` implements `Credentials`:

- **Storage:** API keys in the OS keychain (Secret Service on Linux, Keychain on macOS,
  Credential Manager on Windows) as `nixxa/<provider>/<account>`. The non-secret account list
  lives in the keychain entry `nixxa/_registry` as JSON (D-022). No file is written.
- **Env accounts:** the harness passes keys it read from the environment
  (`KeychainConfig::env_keys`). Each one becomes a read-only `<provider>:env` account
  (`AuthMethod::Env`). It can't be added over or removed, is never stored, and is the
  provider's default unless a stored account is.
- **Unavailable keychain:** `CredentialsError::Storage` with a clear message. Env accounts keep
  working. The encrypted-file fallback is Stage 8.
- **Stage 3 scope:** API keys and local servers (`Credential::Local { base_url }`). A local server
  has no secret: its base URL (plain `http(s)://`, no credentials in it) is the registry hint,
  and no keychain secret entry is written. Other credential kinds return `Unsupported` until their
  methods land.
- **Checks:** keys are checked locally (non-empty, no spaces or control characters). Provider
  and label must be plain lower-case (`a-z 0-9 - _ .`), because they become entry names and
  Windows Credential Manager matches names case-insensitively.
- **Settings:** `setting` / `set_setting` keep small non-secret values next to the account list
  (e.g. which account to start with, a local account's model), so the harness needs no settings
  file (D-035). Never put a secret there.
- **Threading:** keychain calls run on tokio's blocking pool, so use it inside a tokio runtime.
- **In memory:** `KeychainCredentials::in_memory` behaves the same but keeps everything in
  memory, for tests and offline demos.

Tests never touch the real keychain: the conformance suite and the other tests run on the
in-memory store behind the same seam. The one real-keychain test is ignored. Run it by hand:

```sh
cargo test -p nixxa-accounts -- --ignored real_keychain
```

It writes one throwaway entry (`nixxa/nixxa-manual-test/<pid>`), reads it back and deletes
it.
