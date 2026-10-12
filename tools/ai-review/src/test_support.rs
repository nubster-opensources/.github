//! Test doubles shared by the unit tests of the review core.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::diff_context::{parse_added_lines, DiffContext};
use crate::head_file_source::HeadFileSource;
use crate::language_model::{CallPurpose, ChatRequest, LanguageModel};
use crate::types::{ChangedFile, ChangedFileStatus, PatchAvailability};

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

/// Head file contents keyed by path; a path mapped to `"<error>"` fails.
pub(crate) struct MapHeadFiles(pub HashMap<String, String>);

impl HeadFileSource for MapHeadFiles {
    async fn read(&self, path: &str) -> anyhow::Result<Option<String>> {
        match self.0.get(path).map(String::as_str) {
            Some("<error>") => anyhow::bail!("unreadable head file {path}"),
            Some(content) => Ok(Some(content.to_string())),
            None => Ok(None),
        }
    }
}

pub(crate) fn source_file(file_path: &str, diff_patch: &str) -> ChangedFile {
    ChangedFile {
        path: file_path.to_string(),
        status: ChangedFileStatus::Modified,
        previous_path: None,
        additions: 1,
        deletions: 0,
        patch: PatchAvailability::Present(diff_patch.to_string()),
        added_lines: parse_added_lines(diff_patch),
    }
}

pub(crate) fn review_context(files: Vec<ChangedFile>) -> DiffContext {
    DiffContext::from_changed_files(files, None).expect("review context")
}
