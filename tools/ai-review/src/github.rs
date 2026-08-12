#![allow(clippy::missing_errors_doc)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use anyhow::Context;
use octocrab::models::repos::{DiffEntry, DiffEntryStatus};
use octocrab::Octocrab;

use crate::batching::{build_review_batches, DEFAULT_BATCH_BYTES, DEFAULT_MAX_BATCHES};
use crate::review::has_bot_marker;
use crate::text::truncate_utf8;
use crate::types::{
    ChangedFile, ChangedFileStatus, CoverageGap, PatchAvailability, ReviewBatch, SynthFinding,
};

/// An inline comment to post on a specific file line.
pub struct InlineComment {
    pub path: String,
    pub line: u32,
    pub body: String,
}

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

/// Returns the PR diff as a [`DiffContext`] (concatenated view plus per-file map).
pub async fn fetch_diff_context(
    octo: &Octocrab,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<DiffContext> {
    let pulls = octo.pulls(owner, repo);
    let expected_file_count = pulls
        .get(pr_number)
        .await
        .context("failed to get PR file count")?
        .changed_files;
    let first_page = pulls
        .list_files(pr_number)
        .await
        .context("failed to list PR files")?;
    let entries = octo
        .all_pages(first_page)
        .await
        .context("failed to fetch every PR files page")?;

    build_diff_context(entries, expected_file_count)
}

fn build_diff_context(
    entries: Vec<DiffEntry>,
    expected_file_count: Option<u64>,
) -> anyhow::Result<DiffContext> {
    let mut seen = HashSet::with_capacity(entries.len());
    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
        if !seen.insert(entry.filename.clone()) {
            anyhow::bail!(
                "GitHub returned duplicate changed file path: {}",
                entry.filename
            );
        }
        files.push(changed_file(entry));
    }

    let collected_file_count = files.len();
    let expected_file_count = expected_file_count
        .map(usize::try_from)
        .transpose()
        .context("PR changed_files count does not fit this platform")?;
    let file_count = expected_file_count.unwrap_or(collected_file_count);
    let mut full = String::new();
    let mut by_file = HashMap::with_capacity(collected_file_count);

    for file in &files {
        writeln!(full, "--- {}", file.path).unwrap();
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
        });
    }

    Ok(DiffContext {
        full,
        by_file,
        head_files: HashMap::new(),
        file_count,
        files,
        batches: plan.batches,
        coverage_gaps: plan.gaps,
    })
}

fn changed_file(entry: DiffEntry) -> ChangedFile {
    let patch = entry
        .patch
        .filter(|patch| !patch.is_empty())
        .map_or(PatchAvailability::Missing, PatchAvailability::Present);
    let added_lines = match &patch {
        PatchAvailability::Present(text) => parse_added_lines(text),
        PatchAvailability::Missing => BTreeSet::new(),
    };
    ChangedFile {
        path: entry.filename,
        status: changed_file_status(&entry.status),
        previous_path: entry.previous_filename,
        additions: entry.additions,
        deletions: entry.deletions,
        patch,
        added_lines,
    }
}

fn changed_file_status(status: &DiffEntryStatus) -> ChangedFileStatus {
    match status {
        DiffEntryStatus::Added => ChangedFileStatus::Added,
        DiffEntryStatus::Removed => ChangedFileStatus::Removed,
        DiffEntryStatus::Modified => ChangedFileStatus::Modified,
        DiffEntryStatus::Renamed => ChangedFileStatus::Renamed,
        DiffEntryStatus::Copied => ChangedFileStatus::Copied,
        DiffEntryStatus::Unchanged => ChangedFileStatus::Unchanged,
        _ => ChangedFileStatus::Changed,
    }
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

/// Fetches the decoded text content of `path` at commit `sha`. Returns
/// `Ok(None)` when the content cannot be decoded as UTF-8 text (binary file or
/// submodule). A missing path surfaces as an `Err`; callers that want a missing
/// file to be a no-op should treat the error as a skip.
pub async fn fetch_head_file(
    octo: &Octocrab,
    owner: &str,
    repo: &str,
    path: &str,
    sha: &str,
) -> anyhow::Result<Option<String>> {
    let contents = octo
        .repos(owner, repo)
        .get_content()
        .path(path)
        .r#ref(sha)
        .send()
        .await
        .context("failed to fetch head file content")?;
    Ok(contents
        .items
        .into_iter()
        .next()
        .and_then(|item| item.decoded_content()))
}

/// Fills `ctx.head_files` with the head content of every distinct file named by
/// a line-located finding that also has a per-file patch. Failures degrade
/// gracefully: a missing or unreadable file is simply skipped.
pub async fn populate_head_files(
    octo: &Octocrab,
    owner: &str,
    repo: &str,
    sha: &str,
    findings: &[SynthFinding],
    ctx: &mut DiffContext,
) {
    let mut wanted: Vec<String> = findings
        .iter()
        .filter(|f| f.line > 0 && ctx.by_file.contains_key(&f.file))
        .map(|f| f.file.clone())
        .collect();
    wanted.sort();
    wanted.dedup();
    for path in wanted {
        match fetch_head_file(octo, owner, repo, &path, sha).await {
            Ok(Some(content)) => {
                ctx.head_files.insert(path, content);
            }
            Ok(None) => {}
            Err(error) => eprintln!("warning: could not fetch head file {path}: {error}"),
        }
    }
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
    let first_page = octo
        .issues(owner, repo)
        .list_comments(pr_number)
        .per_page(100)
        .send()
        .await
        .context("failed to list PR comments")?;
    let comments = octo
        .all_pages(first_page)
        .await
        .context("failed to fetch every PR comment page")?;

    let existing_id = comments.iter().find_map(|c| {
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

#[derive(Debug)]
struct PreparedInlineComment {
    path: String,
    line: u32,
    body: String,
    marker: String,
}

#[derive(Debug, serde::Deserialize)]
struct ExistingReviewComment {
    id: u64,
    #[serde(default)]
    body: String,
}

/// Upserts inline review comments for one mode and head commit.
///
/// Findings that share a line are combined into one comment. A stable hidden
/// marker lets reruns update that comment instead of publishing duplicates.
pub async fn upsert_inline_comments(
    token: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    namespace: &str,
    comments: &[InlineComment],
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .user_agent("ai-review-bot/0.1")
        .build()?;
    upsert_inline_comments_at(
        &client,
        "https://api.github.com",
        token,
        owner,
        repo,
        pr_number,
        head_sha,
        namespace,
        comments,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn upsert_inline_comments_at(
    client: &reqwest::Client,
    api_base: &str,
    token: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    namespace: &str,
    comments: &[InlineComment],
) -> anyhow::Result<()> {
    let prepared = prepare_inline_comments(namespace, head_sha, comments);
    let existing = list_review_comments(client, api_base, token, owner, repo, pr_number).await?;
    let mut new_comments = Vec::new();
    let mut retained_existing_ids = HashSet::new();

    for comment in &prepared {
        if let Some(found) = existing
            .iter()
            .find(|existing| existing.body.contains(&comment.marker))
        {
            retained_existing_ids.insert(found.id);
            if found.body != comment.body {
                let url = format!(
                    "{api_base}/repos/{owner}/{repo}/pulls/comments/{}",
                    found.id
                );
                client
                    .patch(url)
                    .bearer_auth(token)
                    .header("Accept", "application/vnd.github+json")
                    .header("X-GitHub-Api-Version", "2022-11-28")
                    .json(&serde_json::json!({ "body": comment.body }))
                    .send()
                    .await
                    .context("failed to update inline review comment")?
                    .error_for_status()
                    .context("GitHub rejected inline review comment update")?;
            }
        } else {
            new_comments.push(comment);
        }
    }

    if !new_comments.is_empty() {
        let gh_comments: Vec<GhComment<'_>> = new_comments
            .iter()
            .map(|comment| GhComment {
                path: &comment.path,
                line: comment.line,
                side: "RIGHT",
                body: &comment.body,
            })
            .collect();
        let request = ReviewRequest {
            commit_id: head_sha,
            body: "",
            event: "COMMENT",
            comments: gh_comments,
        };
        let url = format!("{api_base}/repos/{owner}/{repo}/pulls/{pr_number}/reviews");

        client
            .post(&url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .json(&request)
            .send()
            .await
            .context("failed to post inline review")?
            .error_for_status()
            .context("GitHub rejected inline review")?;
    }

    let scope_prefix = inline_scope_prefix(namespace, head_sha);
    for stale in existing.iter().filter(|existing| {
        existing.body.contains(&scope_prefix) && !retained_existing_ids.contains(&existing.id)
    }) {
        let url = format!(
            "{api_base}/repos/{owner}/{repo}/pulls/comments/{}",
            stale.id
        );
        client
            .delete(url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("failed to delete stale inline review comment")?
            .error_for_status()
            .context("GitHub rejected stale inline review comment deletion")?;
    }

    Ok(())
}

async fn list_review_comments(
    client: &reqwest::Client,
    api_base: &str,
    token: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<Vec<ExistingReviewComment>> {
    const PER_PAGE: usize = 100;
    let url = format!("{api_base}/repos/{owner}/{repo}/pulls/{pr_number}/comments");
    let mut all = Vec::new();
    let mut page = 1_u32;
    loop {
        let current: Vec<ExistingReviewComment> = client
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .query(&[("per_page", PER_PAGE), ("page", page as usize)])
            .send()
            .await
            .context("failed to list inline review comments")?
            .error_for_status()
            .context("GitHub rejected inline review comment listing")?
            .json()
            .await
            .context("failed to decode inline review comments")?;
        let is_last = current.len() < PER_PAGE;
        all.extend(current);
        if is_last {
            break;
        }
        page = page
            .checked_add(1)
            .context("inline review comment pagination overflowed")?;
    }
    Ok(all)
}

fn prepare_inline_comments(
    namespace: &str,
    head_sha: &str,
    comments: &[InlineComment],
) -> Vec<PreparedInlineComment> {
    let mut grouped: BTreeMap<(String, u32), Vec<String>> = BTreeMap::new();
    for comment in comments {
        grouped
            .entry((comment.path.clone(), comment.line))
            .or_default()
            .push(comment.body.clone());
    }

    grouped
        .into_iter()
        .map(|((path, line), mut bodies)| {
            bodies.sort();
            bodies.dedup();
            let marker = inline_comment_marker(namespace, head_sha, &path, line);
            PreparedInlineComment {
                path,
                line,
                body: format!("{}\n\n{marker}", bodies.join("\n\n---\n\n")),
                marker,
            }
        })
        .collect()
}

fn inline_comment_marker(namespace: &str, head_sha: &str, path: &str, line: u32) -> String {
    let scope = stable_inline_hash(&[namespace.as_bytes(), head_sha.as_bytes()], None);
    let location = stable_inline_hash(&[path.as_bytes()], Some(line));
    format!("<!-- ai-review-inline:{scope:016x}:{location:016x} -->")
}

fn inline_scope_prefix(namespace: &str, head_sha: &str) -> String {
    let scope = stable_inline_hash(&[namespace.as_bytes(), head_sha.as_bytes()], None);
    format!("<!-- ai-review-inline:{scope:016x}:")
}

fn stable_inline_hash(parts: &[&[u8]], line: Option<u32>) -> u64 {
    // Stable FNV-1a keeps arbitrary file names out of the HTML marker while
    // preserving the same identity across binaries and workflow reruns.
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for part in parts {
        for byte in part.iter().copied().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    if let Some(line) = line {
        for byte in line.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
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
    fn builds_context_for_all_thirty_one_files() {
        let entries: Vec<DiffEntry> = (0..31)
            .map(|index| {
                serde_json::from_value(serde_json::json!({
                    "sha": format!("sha-{index}"),
                    "filename": format!("src/file{index}.rs"),
                    "status": "modified",
                    "additions": 1,
                    "deletions": 0,
                    "changes": 1,
                    "blob_url": null,
                    "raw_url": null,
                    "contents_url": format!("https://api.github.test/file{index}"),
                    "patch": "@@ -0,0 +1 @@\n+new"
                }))
                .expect("valid DiffEntry fixture")
            })
            .collect();
        let ctx = build_diff_context(entries, Some(31)).expect("context");
        assert_eq!(ctx.file_count, 31);
        assert_eq!(ctx.files.len(), 31);
        assert_eq!(ctx.by_file.len(), 31);
        let covered: HashSet<_> = ctx
            .batches
            .iter()
            .flat_map(|batch| batch.files.iter())
            .collect();
        assert_eq!(covered.len(), 31);
    }

    #[test]
    fn preserves_rename_metadata_and_marks_empty_patches_missing() {
        let entry: DiffEntry = serde_json::from_value(serde_json::json!({
            "sha": "sha",
            "filename": "src/new.rs",
            "previous_filename": "src/old.rs",
            "status": "renamed",
            "additions": 0,
            "deletions": 0,
            "changes": 0,
            "blob_url": null,
            "raw_url": null,
            "contents_url": "https://api.github.test/new",
            "patch": ""
        }))
        .expect("valid renamed fixture");
        let file = changed_file(entry);
        assert_eq!(file.status, ChangedFileStatus::Renamed);
        assert_eq!(file.previous_path.as_deref(), Some("src/old.rs"));
        assert_eq!(file.patch, PatchAvailability::Missing);
        assert!(file.added_lines.is_empty());
    }

    #[test]
    fn records_a_gap_when_github_returns_fewer_files_than_reported() {
        let entries: Vec<DiffEntry> = (0..3)
            .map(|index| {
                serde_json::from_value(serde_json::json!({
                    "sha": format!("sha-{index}"),
                    "filename": format!("file{index}.rs"),
                    "status": "modified",
                    "additions": 1,
                    "deletions": 0,
                    "changes": 1,
                    "blob_url": null,
                    "raw_url": null,
                    "contents_url": format!("https://api.github.test/file{index}"),
                    "patch": "@@ -0,0 +1 @@\n+new"
                }))
                .expect("valid fixture")
            })
            .collect();
        let ctx = build_diff_context(entries, Some(3_001)).expect("context");
        assert_eq!(ctx.file_count, 3_001);
        assert!(ctx.coverage_gaps.iter().any(|gap| {
            gap.kind == crate::types::CoverageGapKind::GitHubFileListIncomplete
                && gap.detail.contains("returned 3")
        }));
    }

    #[tokio::test]
    async fn fetch_diff_context_follows_the_second_github_page() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        fn entry(index: usize) -> serde_json::Value {
            serde_json::json!({
                "sha": format!("sha-{index}"),
                "filename": format!("src/file{index}.rs"),
                "status": "modified",
                "additions": 1,
                "deletions": 0,
                "changes": 1,
                "blob_url": null,
                "raw_url": null,
                "contents_url": format!("https://api.github.test/file{index}"),
                "patch": format!("@@ -0,0 +1 @@\n+const FILE: usize = {index};")
            })
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let pr_body = serde_json::json!({
            "url": format!("http://{address}/repos/owner/repo/pulls/7"),
            "id": 7,
            "number": 7,
            "head": {"ref": "feature", "sha": "head"},
            "base": {"ref": "main", "sha": "base"},
            "changed_files": 31
        })
        .to_string();
        let first_body = serde_json::to_string(&(0..30).map(entry).collect::<Vec<_>>())
            .expect("first page JSON");
        let second_body = serde_json::to_string(&vec![entry(30)]).expect("second page JSON");
        let server = tokio::spawn(async move {
            for (index, body) in [pr_body, first_body, second_body].into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut request = vec![0_u8; 4096];
                let read = socket.read(&mut request).await.expect("read request");
                let request = String::from_utf8_lossy(&request[..read]);
                if index == 0 {
                    assert!(request.starts_with("GET /repos/owner/repo/pulls/7 "));
                } else if index == 1 {
                    assert!(request.starts_with("GET /repos/owner/repo/pulls/7/files"));
                } else {
                    assert!(request.starts_with("GET /repos/owner/repo/pulls/7/files?page=2"));
                }
                let link = if index == 1 {
                    format!(
                        "Link: <http://{address}/repos/owner/repo/pulls/7/files?page=2>; rel=\"next\"\r\n"
                    )
                } else {
                    String::new()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{link}Connection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });

        let octo = octocrab::OctocrabBuilder::new()
            .base_uri(format!("http://{address}"))
            .expect("base URI")
            .build()
            .expect("Octocrab");
        let ctx = fetch_diff_context(&octo, "owner", "repo", 7)
            .await
            .expect("all pages");
        server.await.expect("server task");

        assert_eq!(ctx.file_count, 31);
        assert!(ctx.files.iter().any(|file| file.path == "src/file30.rs"));
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

    #[test]
    fn prepares_one_stable_inline_comment_per_location() {
        let comments = vec![
            InlineComment {
                path: "src/lib.rs".to_string(),
                line: 12,
                body: "second issue".to_string(),
            },
            InlineComment {
                path: "src/lib.rs".to_string(),
                line: 12,
                body: "first issue".to_string(),
            },
            InlineComment {
                path: "src/lib.rs".to_string(),
                line: 12,
                body: "first issue".to_string(),
            },
        ];
        let prepared = prepare_inline_comments("team", "abc123", &comments);
        assert_eq!(prepared.len(), 1);
        assert_eq!(prepared[0].path, "src/lib.rs");
        assert_eq!(prepared[0].line, 12);
        assert_eq!(
            prepared[0].body.matches("first issue").count(),
            1,
            "duplicate messages must be collapsed"
        );
        assert!(prepared[0]
            .body
            .contains("first issue\n\n---\n\nsecond issue"));
        assert!(prepared[0].body.ends_with(&prepared[0].marker));
        assert_eq!(
            prepared[0].marker,
            inline_comment_marker("team", "abc123", "src/lib.rs", 12)
        );
    }

    #[test]
    fn inline_marker_is_scoped_to_mode_head_and_location() {
        let marker = inline_comment_marker("team", "abc123", "src/lib.rs", 12);
        assert_ne!(
            marker,
            inline_comment_marker("review", "abc123", "src/lib.rs", 12)
        );
        assert_ne!(
            marker,
            inline_comment_marker("team", "def456", "src/lib.rs", 12)
        );
        assert_ne!(
            marker,
            inline_comment_marker("team", "abc123", "src/main.rs", 12)
        );
        assert_ne!(
            marker,
            inline_comment_marker("team", "abc123", "src/lib.rs", 13)
        );
    }

    #[tokio::test]
    async fn inline_upsert_finds_markers_after_the_first_page_and_updates() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let comment = InlineComment {
            path: "src/lib.rs".to_string(),
            line: 12,
            body: "fresh finding".to_string(),
        };
        let marker = inline_comment_marker("team", "abc123", &comment.path, comment.line);
        let first_page = serde_json::to_string(
            &(0..100)
                .map(|id| serde_json::json!({ "id": id, "body": "human comment" }))
                .collect::<Vec<_>>(),
        )
        .expect("first page JSON");
        let second_page = serde_json::json!([{
            "id": 4242,
            "body": format!("stale finding\n\n{marker}")
        }])
        .to_string();

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            for (index, body) in [first_page, second_page, "{}".to_string()]
                .into_iter()
                .enumerate()
            {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut request = vec![0_u8; 16_384];
                let read = socket.read(&mut request).await.expect("read request");
                let request = String::from_utf8_lossy(&request[..read]);
                match index {
                    0 => assert!(request.starts_with(
                        "GET /repos/owner/repo/pulls/7/comments?per_page=100&page=1 "
                    )),
                    1 => assert!(request.starts_with(
                        "GET /repos/owner/repo/pulls/7/comments?per_page=100&page=2 "
                    )),
                    2 => {
                        assert!(request.starts_with("PATCH /repos/owner/repo/pulls/comments/4242 "));
                        assert!(request.contains("fresh finding"));
                    }
                    _ => unreachable!(),
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });

        let client = reqwest::Client::builder()
            .user_agent("test")
            .build()
            .expect("client");
        upsert_inline_comments_at(
            &client,
            &format!("http://{address}"),
            "token",
            "owner",
            "repo",
            7,
            "abc123",
            "team",
            &[comment],
        )
        .await
        .expect("upsert");
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn inline_upsert_deletes_stale_comments_when_no_findings_remain() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let marker = inline_comment_marker("team", "abc123", "src/lib.rs", 12);
        let existing = serde_json::json!([{
            "id": 4242,
            "body": format!("stale finding\n\n{marker}")
        }])
        .to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            for (index, body) in [existing, "{}".to_string()].into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut request = vec![0_u8; 4096];
                let read = socket.read(&mut request).await.expect("read request");
                let request = String::from_utf8_lossy(&request[..read]);
                if index == 0 {
                    assert!(request.starts_with(
                        "GET /repos/owner/repo/pulls/7/comments?per_page=100&page=1 "
                    ));
                } else {
                    assert!(request.starts_with("DELETE /repos/owner/repo/pulls/comments/4242 "));
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });

        let client = reqwest::Client::builder()
            .user_agent("test")
            .build()
            .expect("client");
        upsert_inline_comments_at(
            &client,
            &format!("http://{address}"),
            "token",
            "owner",
            "repo",
            7,
            "abc123",
            "team",
            &[],
        )
        .await
        .expect("remove stale comment");
        server.await.expect("server task");
    }
}
