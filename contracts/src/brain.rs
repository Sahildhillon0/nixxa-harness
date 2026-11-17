//! The [`Brain`] contract: context in, intents out. Never executes anything.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{ActionRequest, BoxStream, BrainError, Context, ProviderId};

/// What the brain wants to happen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum Intent {
    /// Say something to the user (the full text; deltas were streamed before).
    Say {
        /// The text.
        text: String,
    },
    /// Propose an action. It goes through the controller's policy gate.
    Act {
        /// The proposal.
        request: ActionRequest,
    },
    /// The task is finished.
    Done {
        /// Short summary of what was done.
        summary: String,
    },
}

/// Token usage and cost of one think step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cost in millionths of a US dollar, if known.
    pub cost_micro_usd: Option<u64>,
}

/// One item of a think stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BrainEvent {
    /// A piece of streamed text.
    TextDelta {
        /// The piece.
        text: String,
    },
    /// A complete intent. A stream may contain several (e.g. parallel tool calls).
    Intent {
        /// The intent.
        intent: Intent,
    },
    /// Usage for this step, usually last.
    Usage {
        /// The usage.
        usage: Usage,
    },
}

/// Describes a brain implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrainInfo {
    /// The provider, e.g. `anthropic`.
    pub provider: ProviderId,
    /// The default model.
    pub model: String,
}

/// The stream returned by [`Brain::think`].
pub type BrainStream = BoxStream<'static, Result<BrainEvent, BrainError>>;

/// Turns a [`Context`] into a stream of [`BrainEvent`]s.
///
/// Contract:
/// * the stream terminates and yields at least one [`BrainEvent::Intent`];
/// * every [`Intent::Act`] carries a non-empty, per-stream-unique [`ActionId`](crate::ActionId);
/// * the brain never executes actions and never reads files or env vars itself;
/// * credentials come from [`Credentials`](crate::Credentials) for `context.account`.
#[async_trait]
pub trait Brain: Send + Sync {
    /// Static description.
    fn describe(&self) -> BrainInfo;

    /// Starts one think step.
    async fn think(&self, context: Context) -> Result<BrainStream, BrainError>;
}
