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

/// The PR diff in two shapes: a concatenated view and a per-file patch map.
#[allow(dead_code)]
pub struct DiffContext {
    pub full: String,
    pub by_file: HashMap<String, String>,
    pub file_count: usize,
}

#[allow(dead_code)]
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
