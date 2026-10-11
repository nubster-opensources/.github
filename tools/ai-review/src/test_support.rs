//! Test doubles shared by the unit tests of the review core.

use std::sync::Mutex;

use crate::language_model::{CallPurpose, ChatRequest, LanguageModel};

type Responder = dyn Fn(&ChatRequest) -> anyhow::Result<String> + Send + Sync;

/// A language model that answers from a closure and records every request.
pub(crate) struct ScriptedModel {
    responder: Box<Responder>,
    requests: Mutex<Vec<ChatRequest>>,
}

impl ScriptedModel {
    pub(crate) fn new(
        responder: impl Fn(&ChatRequest) -> anyhow::Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            responder: Box::new(responder),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("requests lock").clone()
    }

    pub(crate) fn purposes(&self) -> Vec<CallPurpose> {
        self.requests()
            .iter()
            .map(|request| request.purpose)
            .collect()
    }
}

impl LanguageModel for ScriptedModel {
    async fn complete(&self, request: &ChatRequest) -> anyhow::Result<String> {
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());
        (self.responder)(request)
    }
}
