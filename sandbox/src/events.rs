//! Sandbox events as the stream [`Sandbox::subscribe`](nixxa_contracts::Sandbox::subscribe)
//! returns.

use futures::stream::BoxStream;
use tokio::sync::broadcast;

use nixxa_contracts::SandboxEvent;

/// Events buffered per subscriber before the slowest one starts missing some.
pub(crate) const CAPACITY: usize = 1024;

/// A subscription as a stream. A subscriber that falls behind skips what it missed (with a
/// warning) rather than losing the stream; the stream ends when the sandbox is dropped.
pub(crate) fn stream(rx: broadcast::Receiver<SandboxEvent>) -> BoxStream<'static, SandboxEvent> {
    Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => return Some((event, rx)),
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(target: "nixxa_sandbox", "a sandbox event subscriber missed {missed} events");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }))
}
