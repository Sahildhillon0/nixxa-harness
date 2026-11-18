//! Replaying recorded responses instead of calling the network (feature `replay`), so the
//! harness can test the real wiring end to end without a key or a connection.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use nixxa_contracts::Credentials;
use serde_json::Value;

use super::transport::{Request, Response, Transport};
use super::{AnthropicBrain, AnthropicConfig};

/// Responses to hand out in order, and the request bodies that were sent.
#[derive(Clone, Default)]
pub struct Replay {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    responses: Mutex<VecDeque<(u16, Vec<u8>)>>,
    bodies: Mutex<Vec<Value>>,
}

impl Replay {
    /// An empty replay; queue responses with [`Replay::push`] or [`Replay::push_fixture`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a response.
    pub fn push(&self, status: u16, body: impl Into<Vec<u8>>) -> &Self {
        self.inner.responses.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push_back((status, body.into()));
        self
    }

    /// Queues one of the brain's recorded SSE fixtures (e.g. `"parallel_tool_use.sse"`) as a 200.
    pub fn push_fixture(&self, name: &str) -> &Self {
        let path = format!("{}/src/providers/anthropic/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        let body = std::fs::read(&path).unwrap_or_else(|err| panic!("fixture {path}: {err}"));
        self.push(200, body)
    }

    /// The JSON body of every request sent so far, in order.
    pub fn request_bodies(&self) -> Vec<Value> {
        self.inner.bodies.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

#[async_trait]
impl Transport for Replay {
    async fn send(&self, request: Request) -> Result<Response, String> {
        if let Some(body) = request.body.as_deref().and_then(|b| serde_json::from_slice(b).ok()) {
            self.inner.bodies.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(body);
        }
        let next = self.inner.responses.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop_front();
        let (status, body) = next.ok_or_else(|| "the replay has no response left".to_string())?;
        let chunks: Vec<Result<Bytes, String>> = body.chunks(7).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        Ok(Response { status, retry_after: None, body: futures::stream::iter(chunks).boxed() })
    }
}

impl AnthropicBrain {
    /// A brain that answers from `replay` instead of the network.
    pub fn replaying(credentials: Arc<dyn Credentials>, config: AnthropicConfig, replay: &Replay) -> Self {
        Self::with_transport(credentials, config, Arc::new(replay.clone()))
    }
}
