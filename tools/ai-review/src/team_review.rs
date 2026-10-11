//! Forge-free core of the multi-agent team review.

#![allow(clippy::missing_errors_doc)]

use crate::github::DiffContext;
use crate::head_file_source::HeadFileSource;
use crate::types::SynthFinding;

/// Fills `context.head_files` with the head content of every distinct file
/// named by a line-located finding that also has a per-file patch. A missing
/// or unreadable file is skipped: its lenses fall back to the patch alone.
pub(crate) async fn populate_head_files<H: HeadFileSource>(
    head_files: &H,
    findings: &[SynthFinding],
    context: &mut DiffContext,
) {
    let mut wanted: Vec<String> = findings
        .iter()
        .filter(|finding| finding.line > 0 && context.by_file.contains_key(&finding.file))
        .map(|finding| finding.file.clone())
        .collect();
    wanted.sort();
    wanted.dedup();
    for path in wanted {
        match head_files.read(&path).await {
            Ok(Some(text)) => {
                context.head_files.insert(path, text);
            }
            Ok(None) => {}
            Err(error) => eprintln!("warning: could not fetch head file {path}: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::test_support::{review_context, source_file, MapHeadFiles};
    use crate::types::{Category, Severity};

    const PATCH: &str = "@@ -1,2 +1,3 @@\n fn main() {\n+    init();\n }";

    fn located(file: &str, line: u32) -> SynthFinding {
        SynthFinding {
            file: file.to_string(),
            line,
            severity: Severity::Critical,
            category: Category::Bug,
            message: "init can panic".to_string(),
            message_fr: String::new(),
            sources: Vec::new(),
        }
    }

    #[tokio::test]
    async fn head_files_are_read_once_for_line_located_findings_with_a_patch() {
        let mut context = review_context(vec![source_file("src/lib.rs", PATCH)]);
        let head_files = MapHeadFiles(HashMap::from([
            (
                "src/lib.rs".to_string(),
                "fn main() {\n    init();\n}".to_string(),
            ),
            ("README.md".to_string(), "# readme".to_string()),
        ]));
        let findings = vec![
            located("src/lib.rs", 2),
            located("src/lib.rs", 3),
            located("README.md", 1),
            located("src/lib.rs", 0),
        ];
        populate_head_files(&head_files, &findings, &mut context).await;
        assert_eq!(context.head_files.len(), 1);
        assert!(context.head_files.contains_key("src/lib.rs"));
    }

    #[tokio::test]
    async fn an_unreadable_head_file_is_skipped_and_the_lens_keeps_the_patch() {
        let mut context = review_context(vec![source_file("src/lib.rs", PATCH)]);
        let head_files = MapHeadFiles(HashMap::from([(
            "src/lib.rs".to_string(),
            "<error>".to_string(),
        )]));
        let finding = located("src/lib.rs", 2);
        populate_head_files(&head_files, std::slice::from_ref(&finding), &mut context).await;
        assert!(context.head_files.is_empty());
        assert_eq!(context.lens_context(&finding), PATCH);
    }
}
