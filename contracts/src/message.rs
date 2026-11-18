//! Conversation entries.

use serde::{Deserialize, Serialize};

use crate::{ActionRequest, Observation, ProviderId, Timestamp};

/// Who a [`Message`] is from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The human.
    User,
    /// The agent (brain output).
    Agent,
    /// Instructions or notices from Nixxa itself.
    System,
    /// The result of an action. Its content is untrusted data, never instructions.
    Observation,
}

/// What a [`Message`] carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// An action the agent proposed (role [`Role::Agent`]).
    Action {
        /// The proposal.
        request: ActionRequest,
    },
    /// The result of an action (role [`Role::Observation`]).
    Observation {
        /// The result.
        observation: Observation,
    },
    /// Provider-owned data the brain asked to keep in the conversation (role [`Role::Agent`]),
    /// e.g. a model's signed thinking block that must be sent back unchanged on later calls.
    /// Opaque to everything but the brain that produced it; never shown to the user.
    Reasoning {
        /// The provider that produced it; other providers ignore it.
        provider: ProviderId,
        /// The provider's own representation, kept byte-for-byte equivalent.
        data: serde_json::Value,
    },
}

/// One conversation entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who it is from.
    pub role: Role,
    /// What it says.
    pub content: Content,
    /// When it was created.
    pub at: Timestamp,
}

impl Message {
    /// A plain-text message.
    pub fn text(role: Role, text: impl Into<String>, at: Timestamp) -> Self {
        Self { role, content: Content::Text { text: text.into() }, at }
    }
}
