#![allow(clippy::missing_errors_doc)]

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::Context;
use octocrab::Octocrab;

use crate::review::has_bot_marker;
use crate::types::SynthFinding;

/// An inline comment to post on a specific file line.
pub struct InlineComment {
    pub path: String,
    pub line: u32,
    pub body: String,
}

/// Maximum bytes of the full diff handed to a lens as fallback context when a
/// finding has no per-file patch (file-level or cross-file findings).
const PATCH_FALLBACK_CHARS: usize = 8_000;

/// Number of head-file lines shown on each side of a finding's line to a lens.
#[allow(dead_code)]
const LENS_WINDOW_LINES: u32 = 40;

/// The PR diff in two shapes: a concatenated view and a per-file patch map.
pub struct DiffContext {
    pub full: String,
    pub by_file: HashMap<String, String>,
    #[allow(dead_code)]
    pub head_files: HashMap<String, String>,
    pub file_count: usize,
}

impl DiffContext {
    /// Returns the patch to hand to a lens for `finding`, falling back to a
    /// truncated view of the full diff for file-level or cross-file findings.
    #[must_use]
    pub fn patch_for(&self, finding: &SynthFinding) -> &str {
        match self.by_file.get(&finding.file) {
            Some(patch) if !patch.is_empty() => patch.as_str(),
            _ => safe_truncate(&self.full, PATCH_FALLBACK_CHARS),
        }
    }

    /// Returns the context handed to a lens for `finding`: the file patch plus,
    /// when the head file is available and the finding is line-located, a
    /// numbered window of the real file around that line. Falls back to the
    /// patch alone for file-level findings or files without head content.
    #[must_use]
    #[allow(dead_code)]
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

    /// Reports whether the finding's line falls inside a hunk of its file
    /// patch: `Some(true)` when it does, `Some(false)` when the patch is
    /// known and the line belongs to no hunk, and `None` when the question
    /// cannot be decided (file-level finding or no per-file patch).
    #[must_use]
    pub fn line_in_patch(&self, finding: &SynthFinding) -> Option<bool> {
        if finding.line == 0 {
            return None;
        }
        let patch = self.by_file.get(&finding.file).filter(|p| !p.is_empty())?;
        let hunks = parse_hunks(patch);
        if hunks.is_empty() {
            return None;
        }
        Some(hunks.iter().any(|h| h.contains_new_line(finding.line)))
    }
}

/// The new-file line range covered by one hunk of a unified diff patch.
struct Hunk {
    new_start: u32,
    new_count: u32,
}

impl Hunk {
    /// Returns true when `line` (new-file side) falls inside this hunk.
    fn contains_new_line(&self, line: u32) -> bool {
        line >= self.new_start
            && u64::from(line) < u64::from(self.new_start) + u64::from(self.new_count)
    }
}

/// Extracts the hunk ranges of a unified diff patch from its `@@` headers.
fn parse_hunks(patch: &str) -> Vec<Hunk> {
    patch
        .lines()
        .filter_map(parse_hunk_header)
        .map(|(new_start, new_count)| Hunk {
            new_start,
            new_count,
        })
        .collect()
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

/// Returns the lines of `content` within `radius` of `line` (1-based), each
/// prefixed by its line number, joined by newlines. Empty when out of range.
#[allow(dead_code)]
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

/// Truncates `s` to at most `max` bytes without splitting a UTF-8 sequence.
fn safe_truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Returns the PR diff as a [`DiffContext`] (concatenated view plus per-file map).
pub async fn fetch_diff_context(
    octo: &Octocrab,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<DiffContext> {
    let files = octo
        .pulls(owner, repo)
        .list_files(pr_number)
        .await
        .context("failed to list PR files")?;

    let file_count = files.items.len();
    let mut full = String::new();
    let mut by_file = HashMap::with_capacity(file_count);

    for file in &files.items {
        writeln!(full, "--- {}", file.filename).unwrap();
        if let Some(patch) = &file.patch {
            full.push_str(patch);
            by_file.insert(file.filename.clone(), patch.clone());
        }
        full.push('\n');
    }

    Ok(DiffContext {
        full,
        by_file,
        head_files: HashMap::new(),
        file_count,
    })
}

/// Returns (concatenated diff as string, number of changed files).
pub async fn fetch_diff(
    octo: &Octocrab,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<(String, usize)> {
    let ctx = fetch_diff_context(octo, owner, repo, pr_number).await?;
    Ok((ctx.full, ctx.file_count))
}

/// Returns the current PR body (empty string if None).
pub async fn fetch_pr_body(
    octo: &Octocrab,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<String> {
    let pr = octo
        .pulls(owner, repo)
        .get(pr_number)
        .await
        .context("failed to get PR info")?;

    Ok(pr.body.unwrap_or_default())
}

/// Returns the head SHA of the PR (needed for inline review comments).
pub async fn fetch_head_sha(
    octo: &Octocrab,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<String> {
    let pr = octo
        .pulls(owner, repo)
        .get(pr_number)
        .await
        .context("failed to get PR head SHA")?;

    Ok(pr.head.sha)
}

/// Upsert the global bot comment (edit if marker found, create otherwise).
pub async fn upsert_global_comment(
    octo: &Octocrab,
    owner: &str,
    repo: &str,
    pr_number: u64,
    body: &str,
    marker: &str,
) -> anyhow::Result<()> {
    let comments = octo
        .issues(owner, repo)
        .list_comments(pr_number)
        .send()
        .await
        .context("failed to list PR comments")?;

    let existing_id = comments.items.iter().find_map(|c| {
        c.body
            .as_deref()
            .filter(|b| has_bot_marker(b, marker))
            .map(|_| c.id)
    });

    if let Some(comment_id) = existing_id {
        octo.issues(owner, repo)
            .update_comment(comment_id, body)
            .await
            .context("failed to update bot comment")?;
    } else {
        octo.issues(owner, repo)
            .create_comment(pr_number, body)
            .await
            .context("failed to create bot comment")?;
    }

    Ok(())
}

#[derive(serde::Serialize)]
struct ReviewRequest<'a> {
    commit_id: &'a str,
    body: &'a str,
    event: &'a str,
    comments: Vec<GhComment<'a>>,
}

#[derive(serde::Serialize)]
struct GhComment<'a> {
    path: &'a str,
    line: u32,
    side: &'a str,
    body: &'a str,
}

/// Post inline review comments for critical findings via GitHub REST API.
///
/// Falls back silently (warning to stderr) if the review API returns an error.
pub async fn post_inline_comments(
    token: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    comments: &[InlineComment],
) -> anyhow::Result<()> {
    if comments.is_empty() {
        return Ok(());
    }

    let gh_comments: Vec<GhComment<'_>> = comments
        .iter()
        .map(|c| GhComment {
            path: &c.path,
            line: c.line,
            side: "RIGHT",
            body: &c.body,
        })
        .collect();

    let request = ReviewRequest {
        commit_id: head_sha,
        body: "",
        event: "COMMENT",
        comments: gh_comments,
    };

    let url = format!("https://api.github.com/repos/{owner}/{repo}/pulls/{pr_number}/reviews");

    let client = reqwest::Client::builder()
        .user_agent("ai-review-bot/0.1")
        .build()?;

    let resp = client
        .post(&url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&request)
        .send()
        .await
        .context("failed to post inline review")?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        eprintln!("warning: inline review returned {status}: {body_text}");
    }

    Ok(())
}

/// Update the PR description body.
pub async fn update_pr_body(
    octo: &Octocrab,
    owner: &str,
    repo: &str,
    pr_number: u64,
    body: &str,
) -> anyhow::Result<()> {
    octo.pulls(owner, repo)
        .update(pr_number)
        .body(body.to_owned())
        .send()
        .await
        .context("failed to update PR body")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Category, Severity, SynthFinding};

    const TWO_HUNK_PATCH: &str = "@@ -1,4 +1,5 @@\n fn main() {\n-    let x = 1;\n+    let x = 2;\n+    println!(\"{x}\");\n }\n@@ -10,3 +11,4 @@ fn helper()\n fn helper() {\n+    do_thing();\n }";

    fn finding_at(file: &str, line: u32) -> SynthFinding {
        SynthFinding {
            file: file.to_string(),
            line,
            severity: Severity::Minor,
            category: Category::Bug,
            message: "m".to_string(),
            sources: vec![],
        }
    }

    fn ctx_with(file: &str, patch: &str) -> DiffContext {
        let mut by_file = HashMap::new();
        by_file.insert(file.to_string(), patch.to_string());
        DiffContext {
            full: format!("--- {file}\n{patch}\n"),
            by_file,
            head_files: HashMap::new(),
            file_count: 1,
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
    fn splits_patch_into_hunk_ranges() {
        let hunks = parse_hunks(TWO_HUNK_PATCH);
        assert_eq!(hunks.len(), 2);
        assert_eq!((hunks[0].new_start, hunks[0].new_count), (1, 5));
        assert_eq!((hunks[1].new_start, hunks[1].new_count), (11, 4));
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
    fn line_in_patch_reports_hunk_membership() {
        let ctx = ctx_with("src/lib.rs", TWO_HUNK_PATCH);
        assert_eq!(ctx.line_in_patch(&finding_at("src/lib.rs", 3)), Some(true));
        assert_eq!(ctx.line_in_patch(&finding_at("src/lib.rs", 12)), Some(true));
        assert_eq!(ctx.line_in_patch(&finding_at("src/lib.rs", 8)), Some(false));
        assert_eq!(
            ctx.line_in_patch(&finding_at("src/lib.rs", 999)),
            Some(false)
        );
        assert_eq!(ctx.line_in_patch(&finding_at("src/lib.rs", 0)), None);
        assert_eq!(ctx.line_in_patch(&finding_at("unknown.rs", 3)), None);
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
