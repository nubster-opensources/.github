#![allow(clippy::missing_errors_doc)]

use crate::team_review::{self, TeamOutcome, TeamReview, TeamReviewInput};
use crate::types::{CoverageGap, FindingVerdict, Severity, SynthFinding};
use crate::{github, mistral, review, team_cache, Clients};

/// Marker used to upsert the single team-review comment on a pull request.
const MARKER: &str = "<!-- ai-team-bot -->";

/// Runs the full multi-agent team review pipeline for a pull request.
pub async fn run_team(
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<()> {
    println!("Fetching PR #{pr_number} diff for team review…");
    let context = github::fetch_diff_context(&clients.octo, owner, repo, pr_number).await?;
    if context.full.trim().is_empty() {
        return handle_empty_review_input(clients, owner, repo, pr_number, &context).await;
    }

    let cache_state = load_team_cache(clients, owner, repo, pr_number, &context).await?;
    if cache_state.matches_run() {
        println!("Identical diff and reviewer revision: keeping the cached team review.");
        return Ok(());
    }

    let head_sha = github::fetch_head_sha(&clients.octo, owner, repo, pr_number).await?;
    let head_files = github::GitHubHeadFiles {
        octo: &clients.octo,
        owner,
        repo,
        sha: &head_sha,
    };
    let input = TeamReviewInput {
        context,
        previous_cache: cache_state.reusable(),
    };
    match team_review::run_team_review(&clients.mistral, &head_files, input).await {
        TeamOutcome::Incomplete {
            reason,
            coverage_gaps,
        } => {
            publish_incomplete_review(clients, owner, repo, pr_number, reason, &coverage_gaps).await
        }
        TeamOutcome::Reviewed(reviewed) => {
            publish_team_review(
                clients,
                owner,
                repo,
                pr_number,
                &head_sha,
                &cache_state,
                *reviewed,
            )
            .await
        }
    }
}

async fn publish_team_review(
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    cache_state: &TeamCacheState,
    reviewed: TeamReview,
) -> anyhow::Result<()> {
    let model = team_model_label();
    let view = review::TeamCommentView {
        executive_summary: &reviewed.synthesis.executive_summary,
        executive_summary_fr: &reviewed.synthesis.executive_summary_fr,
        strengths: &reviewed.synthesis.strengths,
        strengths_fr: &reviewed.synthesis.strengths_fr,
        scored: &reviewed.scored,
        verdict: reviewed.verdict,
        file_count: reviewed.context.file_count,
        agents_ok: &reviewed.agents_ok,
        agents_failed: &reviewed.agents_failed,
        raw_count: reviewed.raw_count,
        dedup_count: reviewed.dedup_count,
        capped: reviewed.capped,
        model: &model,
        coverage_gaps: &reviewed.context.coverage_gaps,
    };
    let mut body = review::render_team_comment(&view);
    append_team_cache(
        &mut body,
        cache_state,
        &reviewed.context,
        &reviewed.scored,
        reviewed.cacheable,
        reviewed.has_incomplete_coverage,
    )?;

    println!("Upserting team comment…");
    github::upsert_global_comment(&clients.octo, owner, repo, pr_number, &body, MARKER).await?;

    post_confirmed_inline(
        clients,
        owner,
        repo,
        pr_number,
        head_sha,
        &reviewed.context,
        &reviewed.scored,
    )
    .await?;

    ensure_complete_review(reviewed.has_incomplete_coverage)?;
    println!("Team review complete.");
    Ok(())
}

fn team_model_label() -> String {
    format!(
        "{} + {}",
        mistral::TEAM_AGENT_MODEL,
        mistral::TEAM_SYNTH_MODEL
    )
}

async fn handle_empty_review_input(
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
    ctx: &crate::diff_context::DiffContext,
) -> anyhow::Result<()> {
    if ctx.coverage_gaps.is_empty() {
        println!("Empty diff: nothing to review.");
        return Ok(());
    }
    publish_incomplete_review(
        clients,
        owner,
        repo,
        pr_number,
        "no textual patch was available for review",
        &ctx.coverage_gaps,
    )
    .await
}

fn ensure_complete_review(has_incomplete_coverage: bool) -> anyhow::Result<()> {
    if has_incomplete_coverage {
        anyhow::bail!("team review is incomplete")
    }
    Ok(())
}

async fn publish_incomplete_review(
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
    reason: &str,
    coverage_gaps: &[CoverageGap],
) -> anyhow::Result<()> {
    let body = review::render_incomplete_team_comment(reason, coverage_gaps);
    github::upsert_global_comment(&clients.octo, owner, repo, pr_number, &body, MARKER).await?;
    ensure_complete_review(true)
}

struct TeamCacheState {
    reviewer_revision: Option<String>,
    trusted_author_configured: bool,
    diff_hash: String,
    previous: Option<team_cache::ReviewCache>,
}

impl TeamCacheState {
    fn matches_run(&self) -> bool {
        match (&self.reviewer_revision, &self.previous) {
            (Some(revision), Some(cache)) => cache.matches_run(revision, &self.diff_hash),
            _ => false,
        }
    }

    fn reusable(&self) -> Option<&team_cache::ReviewCache> {
        match (&self.reviewer_revision, &self.previous) {
            (Some(revision), Some(cache)) if cache.supports_revision(revision) => Some(cache),
            _ => None,
        }
    }
}

async fn load_team_cache(
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
    ctx: &crate::diff_context::DiffContext,
) -> anyhow::Result<TeamCacheState> {
    let reviewer_revision = std::env::var("AI_REVIEW_REVISION")
        .ok()
        .filter(|revision| !revision.trim().is_empty());
    let comment_author = std::env::var("AI_REVIEW_COMMENT_AUTHOR")
        .ok()
        .filter(|author| !author.trim().is_empty());
    let existing_comment = if let Some(author) = comment_author.as_deref() {
        github::fetch_global_comment(&clients.octo, owner, repo, pr_number, MARKER, author).await?
    } else {
        None
    };
    let previous = existing_comment.as_deref().and_then(|body| {
        match team_cache::ReviewCache::from_comment(body) {
            Ok(cache) => cache,
            Err(error) => {
                eprintln!("warning: ignoring invalid team cache: {error}");
                None
            }
        }
    });
    Ok(TeamCacheState {
        reviewer_revision,
        trusted_author_configured: comment_author.is_some(),
        diff_hash: team_cache::diff_hash(ctx),
        previous,
    })
}

fn append_team_cache(
    body: &mut String,
    state: &TeamCacheState,
    ctx: &crate::diff_context::DiffContext,
    scored: &[(SynthFinding, FindingVerdict)],
    cacheable: Vec<bool>,
    has_incomplete_coverage: bool,
) -> anyhow::Result<()> {
    let complete = can_cache_review(has_incomplete_coverage, &cacheable);
    if let Some(revision) = state
        .reviewer_revision
        .as_deref()
        .filter(|_| state.trusted_author_configured && complete)
    {
        let mut cache = team_cache::ReviewCache::new(revision, state.diff_hash.clone());
        for ((finding, finding_verdict), is_cacheable) in scored.iter().zip(cacheable) {
            if is_cacheable {
                cache.record(ctx, finding, finding_verdict);
            }
        }
        *body = team_cache::append_marker(body, &cache.encode_bounded()?);
    } else if state.reviewer_revision.is_some() && state.trusted_author_configured {
        eprintln!("warning: incomplete verification detected; team cache was not updated");
    } else {
        eprintln!(
            "warning: AI_REVIEW_REVISION or AI_REVIEW_COMMENT_AUTHOR is unset; team caching is disabled"
        );
    }
    Ok(())
}

fn can_cache_review(has_incomplete_coverage: bool, cacheable: &[bool]) -> bool {
    !has_incomplete_coverage && cacheable.iter().all(|value| *value)
}

/// Posts inline comments for confirmed, critical, line-located findings.
/// The body shows the French message first and the English message second,
/// or just the English message when no translation is available.
async fn post_confirmed_inline(
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    ctx: &crate::diff_context::DiffContext,
    scored: &[(SynthFinding, FindingVerdict)],
) -> anyhow::Result<()> {
    let inline: Vec<github::InlineComment> = scored
        .iter()
        .filter(|(f, v)| {
            f.severity == Severity::Critical && !v.contested && ctx.line_is_added(f) == Some(true)
        })
        .map(|(f, _)| github::InlineComment {
            path: f.file.clone(),
            line: f.line,
            body: if f.message_fr.is_empty() {
                f.message.clone()
            } else {
                format!("{}\n\n{}", f.message_fr, f.message)
            },
        })
        .collect();

    println!("Upserting {} confirmed inline comment(s)…", inline.len());
    github::upsert_inline_comments(
        &clients.github_token,
        owner,
        repo,
        pr_number,
        head_sha,
        MARKER,
        &inline,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_review_returns_an_error_after_its_result_is_published() {
        assert!(ensure_complete_review(false).is_ok());
        assert!(ensure_complete_review(true).is_err());
    }

    #[test]
    fn incomplete_reviews_are_never_cacheable() {
        assert!(can_cache_review(false, &[true, true]));
        assert!(!can_cache_review(true, &[true, true]));
        assert!(!can_cache_review(false, &[true, false]));
    }
}
