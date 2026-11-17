//! Shared type aliases for the Nixxa contracts crate.

use futures_core::Stream;
use std::pin::Pin;

/// A lazily-evaluated stream of `T`, boxed for object safety and `Send`.
pub type BoxStream<'a, T> = Pin<Box<dyn Stream<Item = T> + Send + 'a>>;
