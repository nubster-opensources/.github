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
    /// Single-pass review analysis of a batch.
    Analysis,
    /// Review of a batch by one specialist agent.
    Agent(Agent),
    /// Merge of the agent reports into one deduplicated review.
    Synthesis,
    /// Adversarial verification of one finding through one lens.
    Lens(Lens),
    /// Product-angle review.
    Product,
    /// Pull request description generation.
    Describe,
}

/// One chat-completion request. Read it through its accessors.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    #[serde(skip)]
    pub(crate) purpose: CallPurpose,
    pub(crate) model: String,
    pub(crate) messages: Vec<Message>,
    pub(crate) response_format: ResponseFormat,
    pub(crate) temperature: f32,
}

/// One message of a chat request.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub(crate) role: String,
    pub(crate) content: String,
}

/// Response format requested from the model.
#[derive(Debug, Clone, Serialize)]
pub struct ResponseFormat {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
}

impl ChatRequest {
    /// What this call is for.
    #[must_use]
    pub fn purpose(&self) -> CallPurpose {
        self.purpose
    }

    /// Name of the model the request targets.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Messages sent to the model, in order.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Sampling temperature.
    #[must_use]
    pub fn temperature(&self) -> f32 {
        self.temperature
    }
}

impl Message {
    /// Role of the author of this message.
    #[must_use]
    pub fn role(&self) -> &str {
        &self.role
    }

    /// Text of this message.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_format_never_carries_the_call_purpose() {
        let request = ChatRequest {
            purpose: CallPurpose::Synthesis,
            model: "model".to_string(),
            messages: vec![Message {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            response_format: ResponseFormat {
                kind: "json_object",
            },
            temperature: 0.2,
        };
        let wire = serde_json::to_value(&request).expect("serializable");
        let keys: Vec<&str> = wire
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys.len(), 4);
        for key in ["model", "messages", "response_format", "temperature"] {
            assert!(keys.contains(&key), "missing {key}");
        }
        assert_eq!(request.purpose(), CallPurpose::Synthesis);
        assert_eq!(request.messages()[0].content(), "hello");
    }
}
