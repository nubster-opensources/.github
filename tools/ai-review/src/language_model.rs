//! Port to the chat-completion model used by the review core.

use serde::Serialize;

use crate::types::{Agent, Lens};

/// Port to a chat-completion model: the review core sends a request and receives the raw text of the first choice.
pub trait LanguageModel: Send + Sync + 'static {
    /// Sends one chat request and returns the raw text of the first choice.
    fn complete(
        &self,
        request: &ChatRequest,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;
}

/// What a model call is for. Never sent over the wire: it lets a substitute model route its answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPurpose {
    Analysis,
    Agent(Agent),
    Synthesis,
    Lens(Lens),
    Product,
    Describe,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    #[serde(skip)]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) purpose: CallPurpose,
    pub(crate) model: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) response_format: ResponseFormat,
    pub(crate) temperature: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub(crate) role: String,
    pub(crate) content: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
}
