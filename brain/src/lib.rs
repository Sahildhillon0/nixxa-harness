//! `nixxa-brain` — turns context into intents (talk, act, done) via an LLM.
//!
//! Stage 3: [`AnthropicBrain`] talks to Claude over the Messages API, and [`OpenAiCompatBrain`]
//! to any OpenAI-compatible server (local models through LM Studio or Ollama), both with
//! streaming and tool use. More providers arrive in Stage 8.

pub mod fake;
mod providers;

pub use providers::anthropic::{AnthropicBrain, AnthropicConfig, DEFAULT_BASE_URL, DEFAULT_MODEL, Effort};
#[cfg(feature = "replay")]
pub use providers::common::replay::Replay;
pub use providers::openai_compat::{ModelInfo, OpenAiCompatBrain, OpenAiCompatConfig};
