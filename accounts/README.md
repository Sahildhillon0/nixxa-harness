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
claude-mem or the model's context. See [ARCHITECTURE.md § 6](../ARCHITECTURE.md#6-accounts-and-credentials). Crate exists but is empty until the stage above.
