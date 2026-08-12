#![allow(clippy::missing_errors_doc)]

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::types::{
    Agent, CoverageGap, CoverageGapKind, FindingVerdict, Lens, LensVerdict, Severity, SynthFinding,
    SynthReport, Verdict,
};
use crate::{github, mistral, review, Clients};

/// Maximum number of synthesised findings put through adversarial verification.
const MAX_VERIFIED_FINDINGS: usize = 15;
/// Maximum number of concurrent Mistral calls during the verification phase.
const MAX_CONCURRENT_CALLS: usize = 6;
/// The three adversarial lenses applied to every verified finding.
const LENSES: [Lens; 3] = [Lens::CodeConfirms, Lens::RealImpact, Lens::FalsePositive];

/// Marker used to upsert the single team-review comment on a pull request.
const MARKER: &str = "<!-- ai-team-bot -->";
/// Minimum number of lens votes required before a finding may be confirmed.
const MIN_CONFIRM_VOTES: usize = 2;

/// Runs the full multi-agent team review pipeline for a pull request.
pub async fn run_team(
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<()> {
    println!("Fetching PR #{pr_number} diff for team review…");
    let mut ctx = github::fetch_diff_context(&clients.octo, owner, repo, pr_number).await?;
    if ctx.full.trim().is_empty() {
        if ctx.coverage_gaps.is_empty() {
            println!("Empty diff: nothing to review.");
        } else {
            let body = review::render_incomplete_team_comment(
                "no textual patch was available for review",
                &ctx.coverage_gaps,
            );
            github::upsert_global_comment(&clients.octo, owner, repo, pr_number, &body, MARKER)
                .await?;
        }
        return Ok(());
    }

    let (batch_runs, agents_ok, agents_failed, agent_gaps) =
        run_batch_plan(clients, &ctx.batches).await;
    ctx.coverage_gaps.extend(agent_gaps);
    if batch_runs.iter().all(|run| run.reports.is_empty()) {
        let reason = if ctx.batches.is_empty() {
            "no textual patch could be placed in a review batch"
        } else {
            "all specialist agents failed"
        };
        let body = review::render_incomplete_team_comment(reason, &ctx.coverage_gaps);
        github::upsert_global_comment(&clients.octo, owner, repo, pr_number, &body, MARKER).await?;
        return Ok(());
    }

    let raw_count: usize = batch_runs
        .iter()
        .flat_map(|run| &run.reports)
        .map(|(_, report)| report.findings.len())
        .sum();
    let (synth, synthesis_gaps) = synthesize_batches(clients, &batch_runs).await;
    ctx.coverage_gaps.extend(synthesis_gaps);
    let Some(mut synth) = synth else {
        let body = review::render_incomplete_team_comment(
            "every batch synthesis failed",
            &ctx.coverage_gaps,
        );
        github::upsert_global_comment(&clients.octo, owner, repo, pr_number, &body, MARKER).await?;
        return Ok(());
    };

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

    let head_sha = github::fetch_head_sha(&clients.octo, owner, repo, pr_number).await?;
    github::populate_head_files(&clients.octo, owner, repo, &head_sha, &findings, &mut ctx).await;

    println!(
        "Verifying {} finding(s) with the 3-lens vote…",
        findings.len()
    );
    let verdicts = verify_findings(clients, &ctx, &findings).await;
    let scored: Vec<(SynthFinding, FindingVerdict)> = findings.into_iter().zip(verdicts).collect();
    let verdict = compute_verdict(&scored, capped > 0 || !ctx.coverage_gaps.is_empty());

    let model = format!(
        "{} + {}",
        mistral::TEAM_AGENT_MODEL,
        mistral::TEAM_SYNTH_MODEL
    );
    let view = review::TeamCommentView {
        executive_summary: &synth.executive_summary,
        executive_summary_fr: &synth.executive_summary_fr,
        strengths: &synth.strengths,
        strengths_fr: &synth.strengths_fr,
        scored: &scored,
        verdict,
        file_count: ctx.file_count,
        agents_ok: &agents_ok,
        agents_failed: &agents_failed,
        raw_count,
        dedup_count,
        capped,
        model: &model,
        coverage_gaps: &ctx.coverage_gaps,
    };
    let body = review::render_team_comment(&view);

    println!("Upserting team comment…");
    github::upsert_global_comment(&clients.octo, owner, repo, pr_number, &body, MARKER).await?;

    post_confirmed_inline(clients, owner, repo, pr_number, &head_sha, &ctx, &scored).await?;

    println!("Team review complete.");
    Ok(())
}

type AgentReports = Vec<(Agent, crate::types::ReviewResponse)>;

struct BatchRun {
    batch: crate::types::ReviewBatch,
    reports: AgentReports,
}

async fn run_batch_plan(
    clients: &Clients,
    batches: &[crate::types::ReviewBatch],
) -> (Vec<BatchRun>, Vec<Agent>, Vec<Agent>, Vec<CoverageGap>) {
    println!(
        "Running specialist agents over {} review batch(es)…",
        batches.len()
    );
    let mut runs = Vec::new();
    let mut ok_set = HashSet::new();
    let mut failed_set = HashSet::new();
    let mut gaps = Vec::new();
    for batch in batches {
        println!(
            "Running batch {}/{} ({} file(s), {} bytes)…",
            batch.id,
            batches.len(),
            batch.files.len(),
            batch.content.len()
        );
        let (batch_reports, ok, failed) = run_agents(clients, &batch.content).await;
        ok_set.extend(ok);
        for agent in failed {
            failed_set.insert(agent);
            gaps.push(CoverageGap {
                kind: CoverageGapKind::AgentFailed,
                file: batch.files.join(", "),
                detail: format!("{} failed on review batch {}", agent.label(), batch.id),
            });
        }
        runs.push(BatchRun {
            batch: batch.clone(),
            reports: batch_reports,
        });
    }
    let agents = [
        Agent::Correctness,
        Agent::Security,
        Agent::Architecture,
        Agent::Performance,
    ];
    let ok = agents
        .iter()
        .copied()
        .filter(|agent| ok_set.contains(agent))
        .collect();
    let failed = agents
        .iter()
        .copied()
        .filter(|agent| failed_set.contains(agent))
        .collect();
    (runs, ok, failed, gaps)
}

async fn synthesize_batches(
    clients: &Clients,
    runs: &[BatchRun],
) -> (Option<SynthReport>, Vec<CoverageGap>) {
    let report_count: usize = runs.iter().map(|run| run.reports.len()).sum();
    println!(
        "Synthesising {report_count} agent report(s) across {} batch(es)…",
        runs.len()
    );
    let mut completed = Vec::new();
    let mut gaps = Vec::new();
    for run in runs.iter().filter(|run| !run.reports.is_empty()) {
        match mistral::call_synthesis(
            &clients.http,
            &clients.mistral_key,
            &run.batch.content,
            &run.reports,
        )
        .await
        {
            Ok(report) => completed.push((run.batch.id, report)),
            Err(error) => {
                eprintln!(
                    "warning: synthesis failed for review batch {}: {error}",
                    run.batch.id
                );
                gaps.push(CoverageGap {
                    kind: CoverageGapKind::SynthesisFailed,
                    file: run.batch.files.join(", "),
                    detail: format!("synthesis failed on review batch {}", run.batch.id),
                });
            }
        }
    }
    (merge_batch_syntheses(completed), gaps)
}

fn merge_batch_syntheses(reports: Vec<(usize, SynthReport)>) -> Option<SynthReport> {
    if reports.is_empty() {
        return None;
    }
    if reports.len() == 1 {
        return reports.into_iter().next().map(|(_, report)| report);
    }

    let mut merged = SynthReport {
        executive_summary: String::new(),
        executive_summary_fr: String::new(),
        strengths: Vec::new(),
        strengths_fr: Vec::new(),
        findings: Vec::new(),
    };
    for (batch_id, report) in reports {
        if !merged.executive_summary.is_empty() {
            merged.executive_summary.push('\n');
            merged.executive_summary_fr.push('\n');
        }
        let _ = write!(
            merged.executive_summary,
            "Batch {batch_id}: {}",
            report.executive_summary
        );
        let _ = write!(
            merged.executive_summary_fr,
            "Lot {batch_id} : {}",
            report.executive_summary_fr
        );
        extend_unique(&mut merged.strengths, report.strengths);
        extend_unique(&mut merged.strengths_fr, report.strengths_fr);
        merged.findings.extend(report.findings);
    }
    Some(merged)
}

fn extend_unique(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value);
        }
    }
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
    if ctx.line_is_added(finding) == Some(false) {
        return Some(FindingVerdict {
            contested: true,
            reasons: vec![format!(
                "deterministic check: line {} of {} is not part of this pull request's changes",
                finding.line, finding.file
            )],
            reasons_fr: vec![format!(
                "controle deterministe : la ligne {} de {} ne fait pas partie des changements de cette pull request",
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
        let patch = ctx.lens_context(finding);
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
/// The body shows the French message first and the English message second,
/// or just the English message when no translation is available.
async fn post_confirmed_inline(
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    ctx: &github::DiffContext,
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

    if inline.is_empty() {
        return Ok(());
    }

    println!("Posting {} confirmed inline comment(s)…", inline.len());
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

/// Aggregates the lens votes for one finding with a strict quorum: a finding
/// is confirmed only when at least `MIN_CONFIRM_VOTES` lenses responded and none
/// of them contested it; any contestation or too few votes leaves it contested.
#[must_use]
pub fn aggregate_lens_votes(votes: &[LensVerdict]) -> FindingVerdict {
    let total = votes.len();
    let contested_count = votes.iter().filter(|v| v.contested).count();
    let contested = total < MIN_CONFIRM_VOTES || contested_count > 0;
    let (reasons, reasons_fr) = if total < MIN_CONFIRM_VOTES {
        (
            vec![format!(
                "insufficient verification ({total} of {MIN_CONFIRM_VOTES} required lenses responded)"
            )],
            vec![format!(
                "verification insuffisante ({total} lentille(s) sur {MIN_CONFIRM_VOTES} requises)"
            )],
        )
    } else if contested_count > 0 {
        let reasons = votes
            .iter()
            .filter(|v| v.contested)
            .map(|v| v.reason.clone())
            .collect();
        let reasons_fr = votes
            .iter()
            .filter(|v| v.contested)
            .map(|v| {
                if v.reason_fr.is_empty() {
                    v.reason.clone()
                } else {
                    v.reason_fr.clone()
                }
            })
            .collect();
        (reasons, reasons_fr)
    } else {
        (Vec::new(), Vec::new())
    };
    FindingVerdict {
        contested,
        reasons,
        reasons_fr,
    }
}

/// Computes the overall verdict deterministically. A confirmed critical always
/// blocks. Without one, incomplete coverage prevents a ship/discuss conclusion;
/// otherwise a contested critical invites discussion and the remaining cases ship.
#[must_use]
pub fn compute_verdict(
    scored: &[(SynthFinding, FindingVerdict)],
    incomplete_coverage: bool,
) -> Verdict {
    let confirmed_critical = scored
        .iter()
        .any(|(f, v)| f.severity == Severity::Critical && !v.contested);
    if confirmed_critical {
        return Verdict::NeedsWork;
    }
    if incomplete_coverage {
        return Verdict::Incomplete;
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

    fn synth_report(summary: &str, finding: SynthFinding) -> SynthReport {
        SynthReport {
            executive_summary: summary.to_string(),
            executive_summary_fr: format!("fr-{summary}"),
            strengths: vec![format!("strength-{summary}")],
            strengths_fr: vec![format!("force-{summary}")],
            findings: vec![finding],
        }
    }

    #[test]
    fn batch_syntheses_keep_findings_from_every_batch() {
        let first = finding_in_file("first.rs", 1);
        let mut second = finding_in_file("second.rs", 200);
        second.severity = Severity::Critical;
        let merged = merge_batch_syntheses(vec![
            (1, synth_report("first", first)),
            (2, synth_report("second", second)),
        ])
        .expect("merged report");
        assert_eq!(merged.findings.len(), 2);
        assert!(merged
            .findings
            .iter()
            .any(|finding| finding.file == "second.rs" && finding.severity == Severity::Critical));
        assert!(merged.executive_summary.contains("Batch 1"));
        assert!(merged.executive_summary.contains("Batch 2"));
    }

    fn lens_vote(contested: bool) -> LensVerdict {
        LensVerdict {
            contested,
            reason: "because".to_string(),
            reason_fr: "parce que".to_string(),
        }
    }

    #[test]
    fn aggregate_confirms_only_on_unanimous_present_min_two() {
        // two or three present, none contested -> confirmed (not contested)
        assert!(!aggregate_lens_votes(&[lens_vote(false), lens_vote(false)]).contested);
        assert!(
            !aggregate_lens_votes(&[lens_vote(false), lens_vote(false), lens_vote(false)])
                .contested
        );
        // any single contestation -> contested
        assert!(
            aggregate_lens_votes(&[lens_vote(true), lens_vote(false), lens_vote(false)]).contested
        );
        assert!(aggregate_lens_votes(&[lens_vote(true), lens_vote(false)]).contested);
    }

    #[test]
    fn aggregate_contests_when_too_few_lenses_responded() {
        assert!(aggregate_lens_votes(&[]).contested);
        assert!(aggregate_lens_votes(&[lens_vote(false)]).contested); // 1 vote < min 2
        assert!(aggregate_lens_votes(&[lens_vote(true)]).contested);
        let v = aggregate_lens_votes(&[lens_vote(false)]);
        assert!(v.reasons[0].contains("insufficient verification"));
    }

    #[test]
    fn aggregate_propagates_contested_reasons() {
        let vote = LensVerdict {
            contested: true,
            reason: "wrong smell".to_string(),
            reason_fr: "mauvaise odeur".to_string(),
        };
        let result = aggregate_lens_votes(&[vote, lens_vote(false)]);
        assert!(result.contested);
        assert_eq!(result.reasons, vec!["wrong smell".to_string()]);
    }

    fn finding(severity: Severity) -> SynthFinding {
        SynthFinding {
            file: "a.rs".to_string(),
            line: 1,
            severity,
            category: Category::Bug,
            message: "m".to_string(),
            message_fr: "m".to_string(),
            sources: vec![],
        }
    }

    fn verdict(contested: bool) -> FindingVerdict {
        FindingVerdict {
            contested,
            reasons: vec![],
            reasons_fr: vec![],
        }
    }

    #[test]
    fn verdict_needs_work_on_confirmed_critical() {
        let scored = vec![(finding(Severity::Critical), verdict(false))];
        assert_eq!(compute_verdict(&scored, false), Verdict::NeedsWork);
    }

    #[test]
    fn verdict_discuss_when_all_criticals_contested() {
        let scored = vec![
            (finding(Severity::Critical), verdict(true)),
            (finding(Severity::Minor), verdict(false)),
        ];
        assert_eq!(compute_verdict(&scored, false), Verdict::Discuss);
    }

    #[test]
    fn verdict_ship_without_criticals() {
        let scored = vec![(finding(Severity::Minor), verdict(false))];
        assert_eq!(compute_verdict(&scored, false), Verdict::Ship);
        assert_eq!(compute_verdict(&[], false), Verdict::Ship);
    }

    #[test]
    fn verdict_incomplete_when_coverage_has_gaps_or_findings_are_capped() {
        let scored = vec![(finding(Severity::Minor), verdict(false))];
        assert_eq!(compute_verdict(&scored, true), Verdict::Incomplete);
        assert_eq!(compute_verdict(&[], true), Verdict::Incomplete);
    }

    #[test]
    fn confirmed_critical_takes_precedence_over_incomplete_coverage() {
        let scored = vec![(finding(Severity::Critical), verdict(false))];
        assert_eq!(compute_verdict(&scored, true), Verdict::NeedsWork);
    }

    fn finding_in_file(file: &str, line: u32) -> SynthFinding {
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

    fn diff_ctx(file: &str, patch: &str) -> github::DiffContext {
        let mut by_file = std::collections::HashMap::new();
        by_file.insert(file.to_string(), patch.to_string());
        github::DiffContext {
            full: format!("--- {file}\n{patch}\n"),
            by_file,
            head_files: std::collections::HashMap::new(),
            file_count: 1,
            files: vec![crate::types::ChangedFile {
                path: file.to_string(),
                status: crate::types::ChangedFileStatus::Modified,
                previous_path: None,
                additions: 1,
                deletions: 0,
                patch: crate::types::PatchAvailability::Present(patch.to_string()),
                added_lines: super::github::parse_added_lines(patch),
            }],
            batches: vec![],
            coverage_gaps: vec![],
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
