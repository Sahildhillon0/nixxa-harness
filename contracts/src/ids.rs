//! Opaque identifiers and time.

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wraps a raw identifier.
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            /// The raw identifier.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                Self::new(id)
            }
        }

        impl From<String> for $name {
            fn from(id: String) -> Self {
                Self(id)
            }
        }
    };
}

id_type!(
    /// Identifies a session. Assigned by the [`Store`](crate::Store).
    SessionId
);
id_type!(
    /// Identifies one user turn within a session. Assigned by the controller.
    TurnId
);
id_type!(
    /// Identifies one proposed action. Assigned by the brain (usually the provider's tool-call
    /// id) so the matching [`Observation`](crate::Observation) can be correlated.
    ActionId
);
id_type!(
    /// Identifies a background job started in the sandbox.
    JobId
);
id_type!(
    /// Identifies a knowledge source, e.g. `claude-mem`, `obsidian`, `codegraph`, `context7`.
    SourceId
);
id_type!(
    /// Identifies a model provider or credentialed integration, e.g. `anthropic`, `context7`.
    ProviderId
);

/// Milliseconds since the Unix epoch (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(pub u64);
