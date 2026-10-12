//! Head-revision file access backed by the files of a benchmark case.

use std::collections::HashMap;

use ai_review::HeadFileSource;

/// Serves head-revision files from the fixture files of a benchmark case.
pub struct FixtureHeadFiles<'a>(pub &'a HashMap<String, String>);

impl HeadFileSource for FixtureHeadFiles<'_> {
    async fn read(&self, path: &str) -> anyhow::Result<Option<String>> {
        Ok(self.0.get(path).cloned())
    }
}
