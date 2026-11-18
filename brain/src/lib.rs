//! `nixxa-brain` — turns context into intents (talk, act, done) via an LLM.
//!
//! Stage 3: [`AnthropicBrain`] talks to Claude over the Messages API with streaming and tool
//! use. More providers arrive in Stage 8.

pub mod fake;
mod providers;

pub use providers::anthropic::{AnthropicBrain, AnthropicConfig, DEFAULT_BASE_URL, DEFAULT_MODEL};
