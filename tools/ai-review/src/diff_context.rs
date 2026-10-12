//! Review context built from the changed files of a pull request, independent of any forge.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use crate::batching::{build_review_batches, DEFAULT_BATCH_BYTES, DEFAULT_MAX_BATCHES};
use crate::text::truncate_utf8;
use crate::types::{ChangedFile, CoverageGap, PatchAvailability, ReviewBatch, SynthFinding};

/// Maximum bytes of the full diff handed to a lens as fallback context when a
/// finding has no per-file patch (file-level or cross-file findings).
const PATCH_FALLBACK_BYTES: usize = 8_000;

/// Number of head-file lines shown on each side of a finding's line to a lens.
const LENS_WINDOW_LINES: u32 = 40;

/// The PR diff in two shapes: a concatenated view and a per-file patch map.
pub struct DiffContext {
    pub full: String,
    pub by_file: HashMap<String, String>,
    pub head_files: HashMap<String, String>,
    pub file_count: usize,
    pub files: Vec<ChangedFile>,
    pub batches: Vec<ReviewBatch>,
    pub coverage_gaps: Vec<CoverageGap>,
}

impl DiffContext {
    /// Builds the review context from changed files already normalised away from
    /// any forge type, so a caller other than the GitHub adapter can feed the
    /// review core.
    ///
    /// # Errors
    ///
    /// Returns an error when two changed files share the same path.
    pub fn from_changed_files(
        files: Vec<ChangedFile>,
        expected_file_count: Option<usize>,
    ) -> anyhow::Result<Self> {
        let mut seen = HashSet::with_capacity(files.len());
        for file in &files {
            if !seen.insert(file.path.as_str()) {
                anyhow::bail!("duplicate changed file path: {}", file.path);
            }
        }

        let collected_file_count = files.len();
        let file_count = expected_file_count.unwrap_or(collected_file_count);
        let mut full = String::new();
        let mut by_file = HashMap::with_capacity(collected_file_count);
        for file in &files {
            let _ = writeln!(full, "--- {}", file.path);
            match &file.patch {
                PatchAvailability::Present(patch) => {
                    full.push_str(patch);
                    by_file.insert(file.path.clone(), patch.clone());
                }
                PatchAvailability::Missing => full.push_str("[textual patch unavailable]"),
            }
            full.push('\n');
        }

        let mut plan = build_review_batches(&files, DEFAULT_BATCH_BYTES, DEFAULT_MAX_BATCHES);
        if expected_file_count.is_some_and(|expected| expected != collected_file_count) {
            plan.gaps.push(CoverageGap {
                kind: crate::types::CoverageGapKind::GitHubFileListIncomplete,
                file: "pull request".to_string(),
                detail: format!(
                    "GitHub reported {file_count} changed files but the files endpoint returned {collected_file_count}; the endpoint is capped at 3,000 files"
                ),
                priority: crate::types::ReviewPriority::Prose,
            });
        }

        Ok(Self {
            full,
            by_file,
            head_files: HashMap::new(),
            file_count,
            files,
            batches: plan.batches,
            coverage_gaps: plan.gaps,
        })
    }

    /// Returns the patch to hand to a lens for `finding`, falling back to a
    /// truncated view of the full diff for file-level or cross-file findings.
    #[must_use]
    pub fn patch_for(&self, finding: &SynthFinding) -> &str {
        match self.by_file.get(&finding.file) {
            Some(patch) if !patch.is_empty() => patch.as_str(),
            _ => truncate_utf8(&self.full, PATCH_FALLBACK_BYTES).0,
        }
    }

    /// Returns the context handed to a lens for `finding`: the file patch plus,
    /// when the head file is available and the finding is line-located, a
    /// numbered window of the real file around that line. Falls back to the
    /// patch alone for file-level findings or files without head content.
    #[must_use]
    pub fn lens_context(&self, finding: &SynthFinding) -> String {
        let patch = self.patch_for(finding).to_string();
        if finding.line == 0 {
            return patch;
        }
        let Some(head) = self.head_files.get(&finding.file) else {
            return patch;
        };
        let window = head_window(head, finding.line, LENS_WINDOW_LINES);
        if window.is_empty() {
            return patch;
        }
        format!("{patch}\n\nFull-file context around the finding (head):\n{window}")
    }

    /// Reports whether the finding points to a line actually added by the pull
    /// request. Context lines and deletion-only lines return `Some(false)`.
    #[must_use]
    pub fn line_is_added(&self, finding: &SynthFinding) -> Option<bool> {
        self.line_is_added_at(&finding.file, finding.line)
    }

    /// Reports whether `line` in `file` is an added line in this diff.
    #[must_use]
    pub fn line_is_added_at(&self, file: &str, line: u32) -> Option<bool> {
        if line == 0 {
            return None;
        }
        let file = self.files.iter().find(|changed| changed.path == file)?;
        match file.patch {
            PatchAvailability::Present(_) => Some(file.added_lines.contains(&line)),
            PatchAvailability::Missing => None,
        }
    }
}

/// Parses the new-file range of a `@@ -a,b +c,d @@` header into (start, count).
fn parse_hunk_header(line: &str) -> Option<(u32, u32)> {
    if !line.starts_with("@@") {
        return None;
    }
    let after_plus = line.split('+').nth(1)?;
    let range_end = after_plus.find([' ', '@'])?;
    let range = &after_plus[..range_end];
    match range.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

/// Parses the exact new-file line numbers represented by `+` records.
pub(crate) fn parse_added_lines(patch: &str) -> BTreeSet<u32> {
    let mut added = BTreeSet::new();
    let mut new_line = None;

    for line in patch.lines() {
        if let Some((start, _)) = parse_hunk_header(line) {
            new_line = Some(start);
            continue;
        }
        let Some(current) = new_line else {
            continue;
        };
        if line.starts_with('+') {
            if current > 0 {
                added.insert(current);
            }
            new_line = current.checked_add(1);
        } else if line.starts_with('-') {
            // A deletion advances only the old-file cursor.
        } else if !line.starts_with('\\') {
            // Context lines advance both cursors. Unknown records are treated
            // as context so a malformed patch cannot shift additions earlier.
            new_line = current.checked_add(1);
        }
    }
    added
}

/// Returns the lines of `content` within `radius` of `line` (1-based), each
/// prefixed by its line number, joined by newlines. Empty when out of range.
fn head_window(content: &str, line: u32, radius: u32) -> String {
    let line = usize::try_from(line).unwrap_or(usize::MAX);
    let radius = usize::try_from(radius).unwrap_or(usize::MAX);
    let start = line.saturating_sub(radius).max(1);
    let end = line.saturating_add(radius);
    content
        .lines()
        .enumerate()
        .filter_map(|(idx, text)| {
            let number = idx + 1;
            (number >= start && number <= end).then(|| format!("{number:>5} {text}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Category, ChangedFileStatus, Severity};

    const TWO_HUNK_PATCH: &str = "@@ -1,4 +1,5 @@\n fn main() {\n-    let x = 1;\n+    let x = 2;\n+    println!(\"{x}\");\n }\n@@ -10,3 +11,4 @@ fn helper()\n fn helper() {\n+    do_thing();\n }";

    const SOURCE_PATCH: &str = "@@ -1,2 +1,3 @@
 fn main() {
+    init();
 }";

    fn source_file(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.to_string(),
            status: ChangedFileStatus::Modified,
            previous_path: None,
            additions: 1,
            deletions: 0,
            patch: PatchAvailability::Present(SOURCE_PATCH.to_string()),
            added_lines: parse_added_lines(SOURCE_PATCH),
        }
    }

    #[test]
    fn context_is_built_from_changed_files_alone() {
        let context = DiffContext::from_changed_files(vec![source_file("src/lib.rs")], None)
            .expect("context");
        assert_eq!(context.file_count, 1);
        assert_eq!(
            context.by_file.get("src/lib.rs").map(String::as_str),
            Some(SOURCE_PATCH)
        );
        assert!(context.full.starts_with(
            "--- src/lib.rs
"
        ));
        assert_eq!(context.batches.len(), 1);
        assert_eq!(context.line_is_added_at("src/lib.rs", 2), Some(true));
        assert!(context.head_files.is_empty());
    }

    #[test]
    fn context_rejects_a_duplicate_changed_file_path() {
        let error = DiffContext::from_changed_files(
            vec![source_file("src/lib.rs"), source_file("src/lib.rs")],
            None,
        )
        .err()
        .expect("duplicate paths must be rejected");
        assert!(error
            .to_string()
            .contains("duplicate changed file path: src/lib.rs"));
    }

    #[test]
    fn context_reports_a_file_list_shorter_than_announced() {
        let context = DiffContext::from_changed_files(vec![source_file("src/lib.rs")], Some(2))
            .expect("context");
        assert_eq!(context.file_count, 2);
        assert!(context
            .coverage_gaps
            .iter()
            .any(|gap| gap.kind == crate::types::CoverageGapKind::GitHubFileListIncomplete));
    }

    fn finding_at(file: &str, line: u32) -> SynthFinding {
        SynthFinding {
            file: file.to_string(),
            line,
            severity: Severity::Minor,
            category: Category::Bug,
            message: "m".to_string(),
            message_fr: "m".to_string(),
            sources: vec![],
        }
    }

    fn ctx_with(file: &str, patch: &str) -> DiffContext {
        let mut by_file = HashMap::new();
        by_file.insert(file.to_string(), patch.to_string());
        let changed = ChangedFile {
            path: file.to_string(),
            status: ChangedFileStatus::Modified,
            previous_path: None,
            additions: 1,
            deletions: 0,
            patch: PatchAvailability::Present(patch.to_string()),
            added_lines: parse_added_lines(patch),
        };
        DiffContext {
            full: format!("--- {file}\n{patch}\n"),
            by_file,
            head_files: HashMap::new(),
            file_count: 1,
            files: vec![changed],
            batches: vec![],
            coverage_gaps: vec![],
        }
    }

    #[test]
    fn parses_standard_hunk_headers() {
        assert_eq!(parse_hunk_header("@@ -1,4 +1,5 @@"), Some((1, 5)));
        assert_eq!(
            parse_hunk_header("@@ -10,3 +11,4 @@ fn helper()"),
            Some((11, 4))
        );
    }

    #[test]
    fn parses_header_without_count_as_one_line() {
        assert_eq!(parse_hunk_header("@@ -1 +1 @@"), Some((1, 1)));
    }

    #[test]
    fn rejects_malformed_headers() {
        assert_eq!(parse_hunk_header("@@ garbage @@"), None);
        assert_eq!(parse_hunk_header("not a header"), None);
    }

    #[test]
    fn parses_only_lines_actually_added() {
        let added = parse_added_lines(TWO_HUNK_PATCH);
        assert_eq!(added, BTreeSet::from([2, 3, 12]));
    }

    #[test]
    fn parses_new_files_multiple_hunks_and_no_newline_markers() {
        let patch = "@@ -0,0 +1,2 @@\n+first\n+second\n\\ No newline at end of file\n@@ -10,1 +12,2 @@\n context\n+third";
        assert_eq!(parse_added_lines(patch), BTreeSet::from([1, 2, 13]));
        assert!(parse_added_lines("@@ -1,1 +0,0 @@\n-old").is_empty());
        assert_eq!(
            parse_added_lines("@@ -0,0 +1 @@\n++++ value starts with pluses"),
            BTreeSet::from([1])
        );
    }

    const HEAD_FILE: &str = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8";

    #[test]
    fn head_window_returns_numbered_lines_around_target() {
        let window = head_window(HEAD_FILE, 4, 2);
        assert!(window.contains("    2 line2"));
        assert!(window.contains("    6 line6"));
        assert!(!window.contains("line1"));
        assert!(!window.contains("line7"));
    }

    #[test]
    fn head_window_clamps_at_file_bounds() {
        let window = head_window(HEAD_FILE, 1, 2);
        assert!(window.contains("    1 line1"));
        assert!(window.contains("    3 line3"));
        assert!(!window.contains("line4"));
    }

    #[test]
    fn lens_context_appends_head_window_when_available() {
        let mut ctx = ctx_with("a.rs", "@@ -1,2 +1,3 @@\n+changed");
        ctx.head_files
            .insert("a.rs".to_string(), HEAD_FILE.to_string());
        let out = ctx.lens_context(&finding_at("a.rs", 4));
        assert!(out.contains("+changed"));
        assert!(out.contains("Full-file context"));
        assert!(out.contains("line4"));
    }

    #[test]
    fn lens_context_falls_back_to_patch_without_head_or_line() {
        let ctx = ctx_with("a.rs", "@@ -1,2 +1,3 @@\n+changed");
        assert_eq!(
            ctx.lens_context(&finding_at("a.rs", 4)),
            ctx.patch_for(&finding_at("a.rs", 4))
        );
        let mut ctx2 = ctx_with("a.rs", "@@ -1,2 +1,3 @@\n+changed");
        ctx2.head_files
            .insert("a.rs".to_string(), HEAD_FILE.to_string());
        assert_eq!(
            ctx2.lens_context(&finding_at("a.rs", 0)),
            ctx2.patch_for(&finding_at("a.rs", 0))
        );
    }

    #[test]
    fn line_is_added_rejects_context_inside_hunks() {
        let ctx = ctx_with("src/lib.rs", TWO_HUNK_PATCH);
        assert_eq!(ctx.line_is_added(&finding_at("src/lib.rs", 3)), Some(true));
        assert_eq!(ctx.line_is_added(&finding_at("src/lib.rs", 12)), Some(true));
        assert_eq!(ctx.line_is_added(&finding_at("src/lib.rs", 1)), Some(false));
        assert_eq!(ctx.line_is_added(&finding_at("src/lib.rs", 4)), Some(false));
        assert_eq!(ctx.line_is_added(&finding_at("src/lib.rs", 8)), Some(false));
        assert_eq!(
            ctx.line_is_added(&finding_at("src/lib.rs", 999)),
            Some(false)
        );
        assert_eq!(ctx.line_is_added(&finding_at("src/lib.rs", 0)), None);
        assert_eq!(ctx.line_is_added(&finding_at("unknown.rs", 3)), None);
    }

    #[test]
    fn lens_context_falls_back_when_line_beyond_eof() {
        assert_eq!(head_window(HEAD_FILE, 1000, 40), "");
        let mut ctx = ctx_with("a.rs", "@@ -1,2 +1,3 @@\n+changed");
        ctx.head_files
            .insert("a.rs".to_string(), HEAD_FILE.to_string());
        let finding = finding_at("a.rs", 1000);
        assert_eq!(ctx.lens_context(&finding), ctx.patch_for(&finding));
    }
}
