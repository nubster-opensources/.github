#![allow(clippy::missing_errors_doc)]

use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::types::{Agent, FindingVerdict, Lens, LensVerdict, Severity, SynthFinding, Verdict};
use crate::{github, mistral, review, Clients};

/// Maximum number of synthesised findings put through adversarial verification.
const MAX_VERIFIED_FINDINGS: usize = 15;
/// Maximum number of concurrent Mistral calls during the verification phase.
const MAX_CONCURRENT_CALLS: usize = 6;
/// The three adversarial lenses applied to every verified finding.
const LENSES: [Lens; 3] = [Lens::CodeConfirms, Lens::RealImpact, Lens::FalsePositive];

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
    let ctx = github::fetch_diff_context(&clients.octo, owner, repo, pr_number).await?;
    if ctx.full.trim().is_empty() {
        println!("Empty diff — nothing to review.");
        return Ok(());
    }

    println!("Running specialist agents…");
    let (report_ok, agents_ok, agents_failed) = run_agents(clients, &ctx.full).await;
    if report_ok.is_empty() {
        let body = format!(
            "{MARKER}\n## Team Review IA\n\n⚠️ Team review unavailable: all specialist agents failed."
        );
        github::upsert_global_comment(&clients.octo, owner, repo, pr_number, &body, MARKER).await?;
        return Ok(());
    }

    let raw_count: usize = report_ok.iter().map(|(_, r)| r.findings.len()).sum();
    println!("Synthesising {} agent report(s)…", report_ok.len());
    let mut synth =
        mistral::call_synthesis(&clients.http, &clients.mistral_key, &ctx.full, &report_ok).await?;

    let mut findings = std::mem::take(&mut synth.findings);
    findings.sort_by_key(|f| severity_rank(&f.severity));
    let dedup_count = findings.len();
    let capped = dedup_count.saturating_sub(MAX_VERIFIED_FINDINGS);
    if capped > 0 {
        println!(
            "Verifying top {MAX_VERIFIED_FINDINGS} of {dedup_count} findings ({capped} skipped)."
        );
        findings.truncate(MAX_VERIFIED_FINDINGS);
    }

    println!(
        "Verifying {} finding(s) with the 3-lens vote…",
        findings.len()
    );
    let verdicts = verify_findings(clients, &ctx, &findings).await;
    let scored: Vec<(SynthFinding, FindingVerdict)> = findings.into_iter().zip(verdicts).collect();
    let verdict = compute_verdict(&scored);

    let model = format!(
        "{} + {}",
        mistral::TEAM_AGENT_MODEL,
        mistral::TEAM_SYNTH_MODEL
    );
    let view = review::TeamCommentView {
        executive_summary: &synth.executive_summary,
        strengths: &synth.strengths,
        scored: &scored,
        verdict,
        file_count: ctx.file_count,
        agents_ok: &agents_ok,
        agents_failed: &agents_failed,
        raw_count,
        dedup_count,
        capped,
        model: &model,
    };
    let body = review::render_team_comment(&view);

    println!("Upserting team comment…");
    github::upsert_global_comment(&clients.octo, owner, repo, pr_number, &body, MARKER).await?;

    post_confirmed_inline(clients, owner, repo, pr_number, &scored).await?;

    println!("Team review complete.");
    Ok(())
}

/// Runs the four specialist agents concurrently, returning the successful
/// reports plus the lists of agents that succeeded and failed.
async fn run_agents(
    clients: &Clients,
    diff: &str,
) -> (
    Vec<(Agent, crate::types::ReviewResponse)>,
    Vec<Agent>,
    Vec<Agent>,
) {
    let agents = [
        Agent::Correctness,
        Agent::Security,
        Agent::Architecture,
        Agent::Performance,
    ];
    let (corr, sec, arch, perf) = tokio::join!(
        mistral::call_agent(
            &clients.http,
            &clients.mistral_key,
            Agent::Correctness,
            diff
        ),
        mistral::call_agent(&clients.http, &clients.mistral_key, Agent::Security, diff),
        mistral::call_agent(
            &clients.http,
            &clients.mistral_key,
            Agent::Architecture,
            diff
        ),
        mistral::call_agent(
            &clients.http,
            &clients.mistral_key,
            Agent::Performance,
            diff
        ),
    );

    let mut reports = Vec::new();
    let mut ok = Vec::new();
    let mut failed = Vec::new();
    for (agent, result) in agents.into_iter().zip([corr, sec, arch, perf]) {
        match result {
            Ok((response, _)) => {
                ok.push(agent);
                reports.push((agent, response));
            }
            Err(error) => {
                eprintln!("warning: {} agent failed: {error}", agent.label());
                failed.push(agent);
            }
        }
    }
    (reports, ok, failed)
}

/// Deterministically contests a finding whose line is known to fall outside
/// every hunk of its file patch, sparing the lens calls entirely. Findings
/// without a line, without a per-file patch, or anchored inside a hunk range
/// are left to the lens vote.
fn prefilter_verdict(ctx: &github::DiffContext, finding: &SynthFinding) -> Option<FindingVerdict> {
    if ctx.line_in_patch(finding) == Some(false) {
        return Some(FindingVerdict {
            contested: true,
            reasons: vec![format!(
                "deterministic check: line {} of {} is not part of this pull request's changes",
                finding.line, finding.file
            )],
        });
    }
    None
}

/// Verifies each finding with the 3-lens adversarial vote under a concurrency
/// bound, returning one [`FindingVerdict`] per finding (index-aligned).
/// Findings contested by the deterministic prefilter skip the vote.
async fn verify_findings(
    clients: &Clients,
    ctx: &github::DiffContext,
    findings: &[SynthFinding],
) -> Vec<FindingVerdict> {
    let prefilled: Vec<Option<FindingVerdict>> = findings
        .iter()
        .map(|finding| prefilter_verdict(ctx, finding))
        .collect();
    let skipped = prefilled.iter().filter(|slot| slot.is_some()).count();
    if skipped > 0 {
        println!("Prefiltered {skipped} finding(s) outside the diff (no lens calls).");
    }

    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CALLS));
    let mut set: JoinSet<(usize, Lens, anyhow::Result<LensVerdict>)> = JoinSet::new();

    for (idx, finding) in findings.iter().enumerate() {
        if prefilled[idx].is_some() {
            continue;
        }
        let patch = ctx.patch_for(finding).to_owned();
        for lens in LENSES {
            let permit_source = Arc::clone(&semaphore);
            let client = clients.http.clone();
            let key = clients.mistral_key.clone();
            let file = finding.file.clone();
            let message = finding.message.clone();
            let patch = patch.clone();
            set.spawn(async move {
                let _permit = permit_source.acquire_owned().await.ok();
                let result = mistral::call_lens(&client, &key, lens, &file, &message, &patch).await;
                (idx, lens, result)
            });
        }
    }

    let mut votes: Vec<Vec<LensVerdict>> = (0..findings.len()).map(|_| Vec::new()).collect();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((idx, _, Ok(verdict))) => votes[idx].push(verdict),
            Ok((idx, lens, Err(error))) => {
                eprintln!(
                    "warning: {} lens failed for finding {idx}: {error}",
                    lens.label()
                );
            }
            Err(error) => eprintln!("warning: a lens task did not complete: {error}"),
        }
    }

    prefilled
        .into_iter()
        .enumerate()
        .map(|(idx, pre)| pre.unwrap_or_else(|| aggregate_lens_votes(&votes[idx])))
        .collect()
}

/// Posts inline comments for confirmed, critical, line-located findings.
async fn post_confirmed_inline(
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
    scored: &[(SynthFinding, FindingVerdict)],
) -> anyhow::Result<()> {
    let inline: Vec<github::InlineComment> = scored
        .iter()
        .filter(|(f, v)| f.severity == Severity::Critical && !v.contested && f.line > 0)
        .map(|(f, _)| github::InlineComment {
            path: f.file.clone(),
            line: f.line,
            body: f.message.clone(),
        })
        .collect();

    if inline.is_empty() {
        return Ok(());
    }

    let head_sha = github::fetch_head_sha(&clients.octo, owner, repo, pr_number).await?;
    println!("Posting {} confirmed inline comment(s)…", inline.len());
    github::post_inline_comments(
        &clients.github_token,
        owner,
        repo,
        pr_number,
        &head_sha,
        &inline,
    )
    .await
}

/// Aggregates the lens votes for one finding using a sceptical majority rule:
/// contested when at least half of the received votes are contested, and when
/// no lens could be reached at all (an unverified finding is not confirmed).
#[must_use]
pub fn aggregate_lens_votes(votes: &[LensVerdict]) -> FindingVerdict {
    let total = votes.len();
    let contested_count = votes.iter().filter(|v| v.contested).count();
    let contested = total == 0 || contested_count * 2 >= total;
    let reasons = if total == 0 {
        vec!["verification unavailable (all lenses failed)".to_string()]
    } else {
        votes
            .iter()
            .filter(|v| v.contested)
            .map(|v| v.reason.clone())
            .collect()
    };
    FindingVerdict { contested, reasons }
}

/// Computes the overall verdict deterministically: a confirmed critical blocks,
/// an only-contested critical invites discussion, otherwise the PR may ship.
#[must_use]
pub fn compute_verdict(scored: &[(SynthFinding, FindingVerdict)]) -> Verdict {
    let confirmed_critical = scored
        .iter()
        .any(|(f, v)| f.severity == Severity::Critical && !v.contested);
    if confirmed_critical {
        return Verdict::NeedsWork;
    }
    let contested_critical = scored
        .iter()
        .any(|(f, v)| f.severity == Severity::Critical && v.contested);
    if contested_critical {
        return Verdict::Discuss;
    }
    Verdict::Ship
}

/// Sort key placing critical findings before minor ones.
fn severity_rank(severity: &Severity) -> u8 {
    match severity {
        Severity::Critical => 0,
        Severity::Minor => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Category;

    fn lens_vote(contested: bool) -> LensVerdict {
        LensVerdict {
            contested,
            reason: "because".to_string(),
        }
    }

    #[test]
    fn aggregate_three_lens_majority() {
        assert!(
            aggregate_lens_votes(&[lens_vote(true), lens_vote(true), lens_vote(false)]).contested
        );
        assert!(
            aggregate_lens_votes(&[lens_vote(true), lens_vote(true), lens_vote(true)]).contested
        );
        assert!(
            !aggregate_lens_votes(&[lens_vote(true), lens_vote(false), lens_vote(false)]).contested
        );
        assert!(
            !aggregate_lens_votes(&[lens_vote(false), lens_vote(false), lens_vote(false)])
                .contested
        );
    }

    #[test]
    fn aggregate_handles_abstentions_and_empty() {
        assert!(aggregate_lens_votes(&[]).contested);
        assert!(aggregate_lens_votes(&[lens_vote(true), lens_vote(false)]).contested);
        assert!(!aggregate_lens_votes(&[lens_vote(false), lens_vote(false)]).contested);
        assert!(aggregate_lens_votes(&[lens_vote(true)]).contested);
        assert!(!aggregate_lens_votes(&[lens_vote(false)]).contested);
    }

    fn finding(severity: Severity) -> SynthFinding {
        SynthFinding {
            file: "a.rs".to_string(),
            line: 1,
            severity,
            category: Category::Bug,
            message: "m".to_string(),
            sources: vec![],
        }
    }

    fn verdict(contested: bool) -> FindingVerdict {
        FindingVerdict {
            contested,
            reasons: vec![],
        }
    }

    #[test]
    fn verdict_needs_work_on_confirmed_critical() {
        let scored = vec![(finding(Severity::Critical), verdict(false))];
        assert_eq!(compute_verdict(&scored), Verdict::NeedsWork);
    }

    #[test]
    fn verdict_discuss_when_all_criticals_contested() {
        let scored = vec![
            (finding(Severity::Critical), verdict(true)),
            (finding(Severity::Minor), verdict(false)),
        ];
        assert_eq!(compute_verdict(&scored), Verdict::Discuss);
    }

    #[test]
    fn verdict_ship_without_criticals() {
        let scored = vec![(finding(Severity::Minor), verdict(false))];
        assert_eq!(compute_verdict(&scored), Verdict::Ship);
        assert_eq!(compute_verdict(&[]), Verdict::Ship);
    }

    fn finding_in_file(file: &str, line: u32) -> SynthFinding {
        SynthFinding {
            file: file.to_string(),
            line,
            severity: Severity::Minor,
            category: Category::Bug,
            message: "m".to_string(),
            sources: vec![],
        }
    }

    fn diff_ctx(file: &str, patch: &str) -> github::DiffContext {
        let mut by_file = std::collections::HashMap::new();
        by_file.insert(file.to_string(), patch.to_string());
        github::DiffContext {
            full: format!("--- {file}\n{patch}\n"),
            by_file,
            file_count: 1,
        }
    }

    const ONE_HUNK_PATCH: &str = "@@ -1,2 +1,3 @@\n fn main() {\n+    init();\n }";

    #[test]
    fn prefilter_contests_findings_outside_the_diff() {
        let ctx = diff_ctx("a.rs", ONE_HUNK_PATCH);
        let verdict = prefilter_verdict(&ctx, &finding_in_file("a.rs", 50)).expect("prefiltered");
        assert!(verdict.contested);
        assert!(verdict.reasons[0].contains("line 50"));
    }

    #[test]
    fn prefilter_keeps_findings_inside_or_undecidable() {
        let ctx = diff_ctx("a.rs", ONE_HUNK_PATCH);
        assert!(prefilter_verdict(&ctx, &finding_in_file("a.rs", 2)).is_none());
        assert!(prefilter_verdict(&ctx, &finding_in_file("a.rs", 0)).is_none());
        assert!(prefilter_verdict(&ctx, &finding_in_file("other.rs", 50)).is_none());
    }
}
