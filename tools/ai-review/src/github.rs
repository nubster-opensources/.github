#![allow(clippy::missing_errors_doc)]

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::Context;
use octocrab::models::repos::{DiffEntry, DiffEntryStatus};
use octocrab::Octocrab;

use crate::diff_context::{parse_added_lines, DiffContext};
use crate::head_file_source::HeadFileSource;
use crate::review::has_bot_marker;
use crate::types::{ChangedFile, ChangedFileStatus, PatchAvailability};

/// An inline comment to post on a specific file line.
pub struct InlineComment {
    pub path: String,
    pub line: u32,
    pub body: String,
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
    let expected_file_count = expected_file_count
        .map(usize::try_from)
        .transpose()
        .context("PR changed_files count does not fit this platform")?;
    let files = entries.into_iter().map(changed_file).collect();
    DiffContext::from_changed_files(files, expected_file_count)
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

/// Reads head file contents from GitHub at a fixed commit.
pub(crate) struct GitHubHeadFiles<'a> {
    pub octo: &'a Octocrab,
    pub owner: &'a str,
    pub repo: &'a str,
    pub sha: &'a str,
}

impl HeadFileSource for GitHubHeadFiles<'_> {
    async fn read(&self, path: &str) -> anyhow::Result<Option<String>> {
        fetch_head_file(self.octo, self.owner, self.repo, path, self.sha).await
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

    let trusted_author = std::env::var("AI_REVIEW_COMMENT_AUTHOR")
        .ok()
        .filter(|author| !author.trim().is_empty());
    let existing_id = comments
        .iter()
        .find(|comment| {
            is_trusted_global_comment(
                &comment.user.login,
                comment.body.as_deref(),
                marker,
                trusted_author.as_deref(),
            )
        })
        .map(|comment| comment.id);

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

/// Returns the existing global bot comment containing `marker`, if any.
pub async fn fetch_global_comment(
    octo: &Octocrab,
    owner: &str,
    repo: &str,
    pr_number: u64,
    marker: &str,
    author_login: &str,
) -> anyhow::Result<Option<String>> {
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
    Ok(comments
        .into_iter()
        .find(|comment| {
            is_trusted_global_comment(
                &comment.user.login,
                comment.body.as_deref(),
                marker,
                Some(author_login),
            )
        })
        .and_then(|comment| comment.body))
}

fn is_trusted_global_comment(
    comment_author: &str,
    body: Option<&str>,
    marker: &str,
    trusted_author: Option<&str>,
) -> bool {
    trusted_author.is_none_or(|author| comment_author == author)
        && body.is_some_and(|value| has_bot_marker(value, marker))
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
    fn global_comment_markers_are_accepted_only_from_the_trusted_author() {
        let body = Some("review\n<!-- ai-team-bot -->");
        assert!(is_trusted_global_comment(
            "github-actions[bot]",
            body,
            "<!-- ai-team-bot -->",
            Some("github-actions[bot]")
        ));
        assert!(!is_trusted_global_comment(
            "pull-request-author",
            body,
            "<!-- ai-team-bot -->",
            Some("github-actions[bot]")
        ));
        assert!(!is_trusted_global_comment(
            "github-actions[bot]",
            Some("ordinary comment"),
            "<!-- ai-team-bot -->",
            Some("github-actions[bot]")
        ));
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
