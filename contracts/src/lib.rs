//! Shared language for the Nixxa blackbox modules.
//!
//! `nixxa-contracts` owns the domain types, module traits, events, error enums and
//! conformance suites that every other Nixxa module speaks. It owns no logic and no I/O.

#![warn(missing_docs)]

pub mod accounts;
pub mod action;
pub mod brain;
#[cfg(feature = "conformance")]
pub mod conformance;
pub mod context;
pub mod error;
pub mod event;
pub mod ids;
pub mod knowledge;
pub mod message;
pub mod observation;
pub mod policy;
pub mod ports;
pub mod sandbox;
pub mod store;
pub mod types;

pub use accounts::*;
pub use action::*;
pub use async_trait::async_trait;
pub use brain::*;
pub use context::*;
pub use error::*;
pub use event::*;
pub use ids::*;
pub use knowledge::*;
pub use message::*;
pub use observation::*;
pub use policy::*;
pub use ports::*;
pub use sandbox::*;
pub use store::*;
pub use types::*;
