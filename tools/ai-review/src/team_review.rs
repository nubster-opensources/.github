//! Forge-free core of the multi-agent team review.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::github::DiffContext;
use crate::head_file_source::HeadFileSource;
use crate::language_model::LanguageModel;
use crate::mistral;
use crate::team_cache::ReviewCache;
use crate::types::{
    Agent, CoverageGap, CoverageGapKind, FindingVerdict, Lens, LensVerdict, ReviewPriority,
    Severity, SynthFinding, SynthReport, Verdict,
};

/// Reason reported when no textual patch could be placed in a review batch.
pub(crate) const NO_BATCH_REASON: &str = "no textual patch could be placed in a review batch";
/// Reason reported when every specialist agent failed on every batch.
pub(crate) const ALL_AGENTS_FAILED_REASON: &str = "all specialist agents failed";
/// Reason reported when no batch could be synthesised.
pub(crate) const ALL_SYNTHESES_FAILED_REASON: &str = "every batch synthesis failed";

/// Everything the team review needs from the caller.
pub(crate) struct TeamReviewInput<'a> {
    pub context: DiffContext,
    pub previous_cache: Option<&'a ReviewCache>,
}

/// Result of a team review run.
pub(crate) enum TeamOutcome {
    /// The review could not be completed; the caller publishes the gaps.
    Incomplete {
        reason: &'static str,
        coverage_gaps: Vec<CoverageGap>,
    },
    /// The review ran; `TeamReview::synthesis.findings` is empty because the
    /// findings live in `scored`.
    Reviewed(Box<TeamReview>),
}

/// A completed team review, ready to be rendered and published.
pub(crate) struct TeamReview {
    pub context: DiffContext,
    pub synthesis: SynthReport,
    pub scored: Vec<(SynthFinding, FindingVerdict)>,
    pub verdict: Verdict,
    pub agents_ok: Vec<Agent>,
    pub agents_failed: Vec<Agent>,
    pub raw_count: usize,
    pub dedup_count: usize,
    pub capped: usize,
    pub has_incomplete_coverage: bool,
    pub cacheable: Vec<bool>,
}

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

/// Runs the multi-agent team review over `input.context`: specialist agents per
/// batch, synthesis, verification cap, head file reads, three-lens vote and the
/// deterministic verdict. Never touches the forge: the caller fetches and publishes.
pub(crate) async fn run_team_review<M: LanguageModel, H: HeadFileSource>(
    language_model: &Arc<M>,
    head_files: &H,
    input: TeamReviewInput<'_>,
) -> TeamOutcome {
    let TeamReviewInput {
        mut context,
        previous_cache,
    } = input;

    let (batch_runs, agents_ok, agents_failed, agent_gaps) =
        run_batch_plan(language_model.as_ref(), &context.batches).await;
    context.coverage_gaps.extend(agent_gaps);
    if batch_runs.iter().all(|run| run.reports.is_empty()) {
        let reason = if context.batches.is_empty() {
            NO_BATCH_REASON
        } else {
            ALL_AGENTS_FAILED_REASON
        };
        return TeamOutcome::Incomplete {
            reason,
            coverage_gaps: context.coverage_gaps,
        };
    }

    let raw_count = raw_finding_count(&batch_runs);
    let (synthesis, synthesis_gaps) =
        synthesize_batches(language_model.as_ref(), &batch_runs).await;
    context.coverage_gaps.extend(synthesis_gaps);
    let Some(mut synthesis) = synthesis else {
        return TeamOutcome::Incomplete {
            reason: ALL_SYNTHESES_FAILED_REASON,
            coverage_gaps: context.coverage_gaps,
        };
    };

    let mut findings = std::mem::take(&mut synthesis.findings);
    findings.sort_by_key(|finding| severity_rank(&finding.severity));
    let dedup_count = findings.len();
    let (findings, dropped) = apply_verification_cap(&context, findings);
    let capped = dropped.len();
    if capped > 0 {
        println!(
            "Capping the lens vote at {MAX_VERIFIED_FINDINGS} of {dedup_count} findings ({capped} left unverified)."
        );
    }

    populate_head_files(head_files, &findings, &mut context).await;

    println!(
        "Verifying {} finding(s) with the 3-lens vote…",
        findings.len()
    );
    let verification = verify_findings(language_model, &context, &findings, previous_cache).await;
    let scored: Vec<(SynthFinding, FindingVerdict)> =
        findings.into_iter().zip(verification.verdicts).collect();
    let has_incomplete_coverage = coverage_is_incomplete(&dropped, &context.coverage_gaps);
    let verdict = compute_verdict(&scored, has_incomplete_coverage);

    TeamOutcome::Reviewed(Box::new(TeamReview {
        context,
        synthesis,
        scored,
        verdict,
        agents_ok,
        agents_failed,
        raw_count,
        dedup_count,
        capped,
        has_incomplete_coverage,
        cacheable: verification.cacheable,
    }))
}

/// Maximum number of synthesised findings put through adversarial verification.
const MAX_VERIFIED_FINDINGS: usize = 15;
/// Maximum number of concurrent Mistral calls during the verification phase.
const MAX_CONCURRENT_CALLS: usize = 6;
/// The three adversarial lenses applied to every verified finding.
const LENSES: [Lens; 3] = [Lens::CodeConfirms, Lens::RealImpact, Lens::FalsePositive];
/// Minimum number of lens votes required before a finding may be confirmed.
const MIN_CONFIRM_VOTES: usize = 2;

fn raw_finding_count(batch_runs: &[BatchRun]) -> usize {
    batch_runs
        .iter()
        .flat_map(|run| &run.reports)
        .map(|(_, report)| report.findings.len())
        .sum()
}

type AgentReports = Vec<(Agent, crate::types::ReviewResponse)>;

struct BatchRun {
    batch: crate::types::ReviewBatch,
    reports: AgentReports,
}

async fn run_batch_plan<M: LanguageModel>(
    language_model: &M,
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
        let (batch_reports, ok, failed) = run_agents(language_model, &batch.content).await;
        ok_set.extend(ok);
        for agent in failed {
            failed_set.insert(agent);
            gaps.push(CoverageGap {
                kind: CoverageGapKind::AgentFailed,
                file: batch.files.join(", "),
                detail: format!("{} failed on review batch {}", agent.label(), batch.id),
                priority: ReviewPriority::SourceCode,
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

async fn synthesize_batches<M: LanguageModel>(
    language_model: &M,
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
        match mistral::call_synthesis(language_model, &run.batch.content, &run.reports).await {
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
                    priority: ReviewPriority::SourceCode,
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
async fn run_agents<M: LanguageModel>(
    language_model: &M,
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
        mistral::call_agent(language_model, Agent::Correctness, diff),
        mistral::call_agent(language_model, Agent::Security, diff),
        mistral::call_agent(language_model, Agent::Architecture, diff),
        mistral::call_agent(language_model, Agent::Performance, diff),
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
fn prefilter_verdict(ctx: &DiffContext, finding: &SynthFinding) -> Option<FindingVerdict> {
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

/// Splits severity-sorted `findings` into the ones this run carries to the
/// verdict and the ones the verification cap drops.
///
/// [`MAX_VERIFIED_FINDINGS`] exists to bound the cost of the lens vote, so
/// only the findings that actually reach that vote consume it. A finding
/// [`prefilter_verdict`] already settles costs no lens call and is therefore
/// always kept, however many of them the synthesis produced: counting those
/// against the cap would report findings as unverified when they were in
/// fact decided, and a review is declared incomplete on that count alone.
///
/// Relative order is preserved, so the caller's severity sort survives.
fn apply_verification_cap(
    ctx: &DiffContext,
    findings: Vec<SynthFinding>,
) -> (Vec<SynthFinding>, Vec<SynthFinding>) {
    let mut retained = Vec::with_capacity(findings.len());
    let mut reaching_the_vote = 0;
    let mut dropped = Vec::new();
    for finding in findings {
        if prefilter_verdict(ctx, &finding).is_some() {
            retained.push(finding);
        } else if reaching_the_vote < MAX_VERIFIED_FINDINGS {
            reaching_the_vote += 1;
            retained.push(finding);
        } else {
            dropped.push(finding);
        }
    }
    (retained, dropped)
}

/// Whether the run must refuse to conclude.
///
/// A dropped finding and an unread input are both coverage the run does not
/// have, but neither is fatal on its own. The cap cuts the severity-sorted
/// tail, so dropping only minor findings still means every critical one
/// reached the vote, and treating that as a failure would leave any large
/// pull request permanently red. A gap counts only when reading it was
/// possible at all, which [`CoverageGapKind::is_blocking`] decides.
fn coverage_is_incomplete(dropped: &[SynthFinding], gaps: &[CoverageGap]) -> bool {
    dropped
        .iter()
        .any(|finding| matches!(finding.severity, Severity::Critical))
        || gaps.iter().any(CoverageGap::is_blocking)
}

/// Verifies each finding with the 3-lens adversarial vote under a concurrency
/// bound, returning one [`FindingVerdict`] per finding (index-aligned).
/// Findings contested by the deterministic prefilter skip the vote.
struct VerificationResults {
    verdicts: Vec<FindingVerdict>,
    cacheable: Vec<bool>,
}

struct PrefilledVerdicts {
    verdicts: Vec<Option<FindingVerdict>>,
    deterministic_count: usize,
    cache_hit_count: usize,
}

fn prefill_verdicts(
    ctx: &DiffContext,
    findings: &[SynthFinding],
    previous_cache: Option<&ReviewCache>,
) -> PrefilledVerdicts {
    let mut verdicts = Vec::with_capacity(findings.len());
    let mut deterministic_count = 0;
    let mut cache_hit_count = 0;
    for finding in findings {
        if let Some(verdict) = prefilter_verdict(ctx, finding) {
            deterministic_count += 1;
            verdicts.push(Some(verdict));
        } else if let Some(verdict) = previous_cache.and_then(|cache| cache.lookup(ctx, finding)) {
            cache_hit_count += 1;
            verdicts.push(Some(verdict));
        } else {
            verdicts.push(None);
        }
    }
    PrefilledVerdicts {
        verdicts,
        deterministic_count,
        cache_hit_count,
    }
}

async fn verify_findings<M: LanguageModel>(
    language_model: &Arc<M>,
    ctx: &DiffContext,
    findings: &[SynthFinding],
    previous_cache: Option<&ReviewCache>,
) -> VerificationResults {
    let PrefilledVerdicts {
        verdicts: prefilled,
        deterministic_count,
        cache_hit_count,
    } = prefill_verdicts(ctx, findings, previous_cache);
    if deterministic_count > 0 {
        println!("Prefiltered {deterministic_count} finding(s) outside the diff (no lens calls).");
    }
    if cache_hit_count > 0 {
        println!("Reused {cache_hit_count} cached finding verdict(s) (no lens calls).");
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
            let language_model = Arc::clone(language_model);
            let file = finding.file.clone();
            let message = finding.message.clone();
            let patch = patch.clone();
            set.spawn(async move {
                let _permit = permit_source.acquire_owned().await.ok();
                let result =
                    mistral::call_lens(language_model.as_ref(), lens, &file, &message, &patch)
                        .await;
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

    let cacheable = prefilled
        .iter()
        .enumerate()
        .map(|(idx, pre)| pre.is_some() || votes[idx].len() >= MIN_CONFIRM_VOTES)
        .collect();
    let verdicts = prefilled
        .into_iter()
        .enumerate()
        .map(|(idx, pre)| pre.unwrap_or_else(|| aggregate_lens_votes(&votes[idx])))
        .collect();
    VerificationResults {
        verdicts,
        cacheable,
    }
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
    use std::collections::HashMap;

    use super::*;
    use crate::language_model::{CallPurpose, ChatRequest};
    use crate::team_cache::ReviewCache;
    use crate::test_support::ScriptedModel;
    use crate::test_support::{review_context, source_file, MapHeadFiles};
    use crate::types::{Category, Severity};
    use crate::types::{
        ChangedFile, ChangedFileStatus, CoverageGapKind, FindingVerdict, PatchAvailability, Verdict,
    };
    use crate::{github, team_cache};
    use std::sync::Arc;

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

    const AGENT_REPORT: &str = r#"{"summary":"looked","strengths":[],"findings":[{"file":"src/lib.rs","line":2,"severity":"critical","message":"init can panic"}],"security":"none"}"#;
    const FINDING_ON_ADDED_LINE: &str = r#"{"file":"src/lib.rs","line":2,"severity":"critical","category":"bug","message":"init can panic"}"#;
    const FINDING_ON_CONTEXT_LINE: &str = r#"{"file":"src/lib.rs","line":1,"severity":"critical","category":"bug","message":"main is wrong"}"#;
    const CONFIRMING_LENS: &str = r#"{"contested":false,"reason":"confirmed"}"#;

    fn synthesis_with(findings: &[&str]) -> String {
        format!(
            r#"{{"executive_summary":"summary","findings":[{}]}}"#,
            findings.join(",")
        )
    }

    fn scripted(
        synthesis: String,
        lens: impl Fn(Lens) -> anyhow::Result<String> + Send + Sync + 'static,
    ) -> Arc<ScriptedModel> {
        Arc::new(ScriptedModel::new(
            move |request: &ChatRequest| match request.purpose {
                CallPurpose::Agent(_) => Ok(AGENT_REPORT.to_string()),
                CallPurpose::Synthesis => Ok(synthesis.clone()),
                CallPurpose::Lens(kind) => lens(kind),
                other => anyhow::bail!("unexpected call {other:?}"),
            },
        ))
    }

    fn no_head_files() -> MapHeadFiles {
        MapHeadFiles(HashMap::new())
    }

    fn input(context: DiffContext) -> TeamReviewInput<'static> {
        TeamReviewInput {
            context,
            previous_cache: None,
        }
    }

    fn reviewed(outcome: TeamOutcome) -> TeamReview {
        match outcome {
            TeamOutcome::Reviewed(review) => *review,
            TeamOutcome::Incomplete { reason, .. } => panic!("expected a review, got: {reason}"),
        }
    }

    fn incomplete(outcome: TeamOutcome) -> (&'static str, Vec<CoverageGap>) {
        match outcome {
            TeamOutcome::Incomplete {
                reason,
                coverage_gaps,
            } => (reason, coverage_gaps),
            TeamOutcome::Reviewed(_) => panic!("expected an incomplete review"),
        }
    }

    fn count(purposes: &[CallPurpose], wanted: fn(&CallPurpose) -> bool) -> usize {
        purposes.iter().filter(|purpose| wanted(purpose)).count()
    }

    #[tokio::test]
    async fn a_confirmed_critical_on_an_added_line_needs_work() {
        let model = scripted(synthesis_with(&[FINDING_ON_ADDED_LINE]), |_| {
            Ok(CONFIRMING_LENS.to_string())
        });
        let head_files = MapHeadFiles(HashMap::from([(
            "src/lib.rs".to_string(),
            "fn main() {\n    init();\n}".to_string(),
        )]));
        let context = review_context(vec![source_file("src/lib.rs", PATCH)]);

        let review = reviewed(run_team_review(&model, &head_files, input(context)).await);

        assert_eq!(review.verdict, Verdict::NeedsWork);
        assert_eq!(review.scored.len(), 1);
        assert!(!review.scored[0].1.contested);
        assert_eq!(review.raw_count, 4);
        assert_eq!(review.dedup_count, 1);
        assert_eq!(review.capped, 0);
        assert!(!review.has_incomplete_coverage);
        assert_eq!(review.cacheable, vec![true]);
        assert!(review.context.head_files.contains_key("src/lib.rs"));
        assert!(review.synthesis.findings.is_empty());
        let purposes = model.purposes();
        assert_eq!(count(&purposes, |p| matches!(p, CallPurpose::Agent(_))), 4);
        assert_eq!(count(&purposes, |p| matches!(p, CallPurpose::Synthesis)), 1);
        assert_eq!(count(&purposes, |p| matches!(p, CallPurpose::Lens(_))), 3);
    }

    #[tokio::test]
    async fn a_finding_outside_the_diff_is_contested_without_any_lens_call() {
        let model = scripted(synthesis_with(&[FINDING_ON_CONTEXT_LINE]), |_| {
            anyhow::bail!("no lens may run")
        });
        let context = review_context(vec![source_file("src/lib.rs", PATCH)]);

        let review = reviewed(run_team_review(&model, &no_head_files(), input(context)).await);

        assert!(review.scored[0].1.contested);
        assert_eq!(review.verdict, Verdict::Discuss);
        assert_eq!(
            count(&model.purposes(), |p| matches!(p, CallPurpose::Lens(_))),
            0
        );
    }

    #[tokio::test]
    async fn failing_agents_everywhere_leave_the_review_incomplete() {
        let model = Arc::new(ScriptedModel::new(|_| anyhow::bail!("agent down")));
        let context = review_context(vec![source_file("src/lib.rs", PATCH)]);

        let (reason, gaps) =
            incomplete(run_team_review(&model, &no_head_files(), input(context)).await);

        assert_eq!(reason, "all specialist agents failed");
        assert_eq!(
            gaps.iter()
                .filter(|gap| gap.kind == CoverageGapKind::AgentFailed)
                .count(),
            4
        );
    }

    #[tokio::test]
    async fn a_failing_synthesis_leaves_the_review_incomplete() {
        let model = scripted("this is not json".to_string(), |_| {
            Ok(CONFIRMING_LENS.to_string())
        });
        let context = review_context(vec![source_file("src/lib.rs", PATCH)]);

        let (reason, gaps) =
            incomplete(run_team_review(&model, &no_head_files(), input(context)).await);

        assert_eq!(reason, "every batch synthesis failed");
        assert!(gaps
            .iter()
            .any(|gap| gap.kind == CoverageGapKind::SynthesisFailed));
    }

    #[tokio::test]
    async fn a_change_without_any_batch_is_incomplete_and_calls_no_model() {
        let model = scripted(synthesis_with(&[]), |_| Ok(CONFIRMING_LENS.to_string()));
        let unpatched = ChangedFile {
            path: "src/large.rs".to_string(),
            status: ChangedFileStatus::Modified,
            previous_path: None,
            additions: 3,
            deletions: 0,
            patch: PatchAvailability::Missing,
            added_lines: std::collections::BTreeSet::new(),
        };
        let context = review_context(vec![unpatched]);

        let (reason, _) =
            incomplete(run_team_review(&model, &no_head_files(), input(context)).await);

        assert_eq!(reason, "no textual patch could be placed in a review batch");
        assert!(model.purposes().is_empty());
    }

    #[tokio::test]
    async fn a_cached_verdict_spares_the_lens_vote() {
        let model = scripted(synthesis_with(&[FINDING_ON_ADDED_LINE]), |_| {
            anyhow::bail!("no lens may run")
        });
        let finding: SynthFinding = serde_json::from_str(FINDING_ON_ADDED_LINE).expect("finding");
        let cached = FindingVerdict {
            contested: false,
            reasons: Vec::new(),
            reasons_fr: Vec::new(),
        };
        let mut cache = ReviewCache::new("revision", "hash".to_string());
        cache.record(
            &review_context(vec![source_file("src/lib.rs", PATCH)]),
            &finding,
            &cached,
        );
        let context = review_context(vec![source_file("src/lib.rs", PATCH)]);

        let review = reviewed(
            run_team_review(
                &model,
                &no_head_files(),
                TeamReviewInput {
                    context,
                    previous_cache: Some(&cache),
                },
            )
            .await,
        );

        assert_eq!(review.scored[0].1, cached);
        assert_eq!(
            count(&model.purposes(), |p| matches!(p, CallPurpose::Lens(_))),
            0
        );
    }

    #[tokio::test]
    async fn lens_failures_below_the_quorum_contest_the_finding_and_block_the_cache() {
        let model = scripted(
            synthesis_with(&[FINDING_ON_ADDED_LINE]),
            |lens| match lens {
                Lens::FalsePositive => Ok(CONFIRMING_LENS.to_string()),
                _ => anyhow::bail!("lens down"),
            },
        );
        let context = review_context(vec![source_file("src/lib.rs", PATCH)]);

        let review = reviewed(run_team_review(&model, &no_head_files(), input(context)).await);

        assert!(review.scored[0].1.contested);
        assert!(review.scored[0].1.reasons[0].starts_with("insufficient verification"));
        assert_eq!(review.cacheable, vec![false]);
        assert_eq!(review.verdict, Verdict::Discuss);
    }

    #[tokio::test]
    async fn two_failing_agents_leave_the_review_incomplete() {
        let synthesis = synthesis_with(&[]);
        let model = Arc::new(ScriptedModel::new(
            move |request: &ChatRequest| match request.purpose {
                CallPurpose::Agent(Agent::Security | Agent::Performance) => {
                    anyhow::bail!("agent down")
                }
                CallPurpose::Agent(_) => Ok(AGENT_REPORT.to_string()),
                CallPurpose::Synthesis => Ok(synthesis.clone()),
                other => anyhow::bail!("unexpected call {other:?}"),
            },
        ));
        let context = review_context(vec![source_file("src/lib.rs", PATCH)]);

        let review = reviewed(run_team_review(&model, &no_head_files(), input(context)).await);

        assert_eq!(
            review.agents_ok,
            vec![Agent::Correctness, Agent::Architecture]
        );
        assert_eq!(
            review.agents_failed,
            vec![Agent::Security, Agent::Performance]
        );
        assert!(review.has_incomplete_coverage);
        assert_eq!(review.verdict, Verdict::Incomplete);
    }

    #[tokio::test]
    async fn lens_votes_stay_aligned_with_their_finding() {
        const SECOND_FILE_FINDING: &str = r#"{"file":"src/other.rs","line":2,"severity":"critical","category":"bug","message":"other can panic"}"#;
        let model = Arc::new(ScriptedModel::new(|request: &ChatRequest| {
            match request.purpose {
                CallPurpose::Agent(_) => Ok(AGENT_REPORT.to_string()),
                CallPurpose::Synthesis => Ok(synthesis_with(&[
                    FINDING_ON_ADDED_LINE,
                    SECOND_FILE_FINDING,
                ])),
                CallPurpose::Lens(_) => {
                    let user_message = &request.messages[1].content;
                    if user_message.contains("File: src/other.rs") {
                        Ok(r#"{"contested":true,"reason":"not reachable"}"#.to_string())
                    } else {
                        Ok(CONFIRMING_LENS.to_string())
                    }
                }
                other => anyhow::bail!("unexpected call {other:?}"),
            }
        }));
        let context = review_context(vec![
            source_file("src/lib.rs", PATCH),
            source_file("src/other.rs", PATCH),
        ]);

        let review = reviewed(run_team_review(&model, &no_head_files(), input(context)).await);

        let verdict_for = |file: &str| {
            review
                .scored
                .iter()
                .find(|(finding, _)| finding.file == file)
                .map(|(_, verdict)| verdict.contested)
                .expect("finding present")
        };
        assert!(!verdict_for("src/lib.rs"));
        assert!(verdict_for("src/other.rs"));
    }

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
                added_lines: github::parse_added_lines(patch),
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

    #[test]
    fn unchanged_finding_context_is_prefilled_from_cache() {
        let ctx = diff_ctx("a.rs", ONE_HUNK_PATCH);
        let finding = finding_in_file("a.rs", 0);
        let cached_verdict = verdict(false);
        let mut cache = team_cache::ReviewCache::new("revision", team_cache::diff_hash(&ctx));
        cache.record(&ctx, &finding, &cached_verdict);

        let hit = prefill_verdicts(&ctx, std::slice::from_ref(&finding), Some(&cache));
        assert_eq!(hit.cache_hit_count, 1);
        assert_eq!(hit.deterministic_count, 0);
        assert_eq!(hit.verdicts, vec![Some(cached_verdict)]);

        let changed = diff_ctx("a.rs", "@@ -1,2 +1,3 @@\n fn main() {\n+    changed();\n }");
        let miss = prefill_verdicts(&changed, &[finding], Some(&cache));
        assert_eq!(miss.cache_hit_count, 0);
        assert_eq!(miss.verdicts, vec![None]);
    }

    /// Any line of `a.rs` other than the single added line 2 falls outside
    /// the hunk, so [`prefilter_verdict`] settles it without a lens call.
    fn settled_by_prefilter(line: u32) -> SynthFinding {
        finding_in_file("a.rs", line)
    }

    /// Line 2 is the added line of [`ONE_HUNK_PATCH`], so the finding
    /// survives the prefilter and reaches the lens vote.
    fn reaching_the_vote(tag: &str) -> SynthFinding {
        let mut finding = finding_in_file("a.rs", 2);
        finding.message = tag.to_string();
        finding
    }

    #[test]
    fn verification_cap_spares_findings_the_prefilter_settles() {
        let ctx = diff_ctx("a.rs", ONE_HUNK_PATCH);
        let findings: Vec<SynthFinding> = (0..MAX_VERIFIED_FINDINGS + 8)
            .map(|index| settled_by_prefilter(50 + u32::try_from(index).expect("small index")))
            .collect();
        let total = findings.len();

        let (retained, dropped) = apply_verification_cap(&ctx, findings);

        assert!(dropped.is_empty(), "prefiltered findings cost no lens call");
        assert_eq!(retained.len(), total);
    }

    #[test]
    fn verification_cap_keeps_every_finding_when_the_vote_stays_under_the_bound() {
        let ctx = diff_ctx("a.rs", ONE_HUNK_PATCH);
        let findings: Vec<SynthFinding> = (0..MAX_VERIFIED_FINDINGS)
            .map(|index| reaching_the_vote(&format!("vote-{index}")))
            .collect();

        let (retained, dropped) = apply_verification_cap(&ctx, findings);

        assert!(dropped.is_empty());
        assert_eq!(retained.len(), MAX_VERIFIED_FINDINGS);
    }

    #[test]
    fn verification_cap_drops_only_findings_that_reach_the_lens_vote() {
        let ctx = diff_ctx("a.rs", ONE_HUNK_PATCH);
        let mut findings = vec![settled_by_prefilter(50)];
        findings.extend(
            (0..MAX_VERIFIED_FINDINGS + 2).map(|index| reaching_the_vote(&format!("vote-{index}"))),
        );
        findings.push(settled_by_prefilter(51));
        findings.push(settled_by_prefilter(52));

        let (retained, dropped) = apply_verification_cap(&ctx, findings);

        assert_eq!(
            dropped.len(),
            2,
            "only the votes beyond the bound are dropped"
        );
        assert_eq!(retained.len(), MAX_VERIFIED_FINDINGS + 3);

        let mut expected: Vec<String> = vec!["m".to_string()];
        expected.extend((0..MAX_VERIFIED_FINDINGS).map(|index| format!("vote-{index}")));
        expected.push("m".to_string());
        expected.push("m".to_string());
        let observed: Vec<String> = retained
            .iter()
            .map(|finding| finding.message.clone())
            .collect();
        assert_eq!(observed, expected, "input order must survive the cap");

        let prefiltered_lines: Vec<u32> = retained
            .iter()
            .filter(|finding| finding.line != 2)
            .map(|finding| finding.line)
            .collect();
        assert_eq!(prefiltered_lines, vec![50, 51, 52]);
    }

    fn gap(kind: CoverageGapKind) -> CoverageGap {
        CoverageGap {
            kind,
            file: "tests/fixtures/client.p12".to_string(),
            detail: "d".to_string(),
            priority: ReviewPriority::Prose,
        }
    }

    fn budget_gap(priority: ReviewPriority) -> CoverageGap {
        CoverageGap {
            kind: CoverageGapKind::BatchBudgetExceeded,
            file: "docs/readme.md".to_string(),
            detail: "d".to_string(),
            priority,
        }
    }

    #[test]
    fn dropping_only_minor_findings_leaves_the_review_complete() {
        let dropped = vec![finding(Severity::Minor), finding(Severity::Minor)];
        assert!(
            !coverage_is_incomplete(&dropped, &[]),
            "the cap cuts the severity-sorted tail, so every critical finding was voted on"
        );
    }

    #[test]
    fn dropping_a_critical_finding_leaves_the_review_incomplete() {
        let dropped = vec![finding(Severity::Minor), finding(Severity::Critical)];
        assert!(coverage_is_incomplete(&dropped, &[]));
    }

    #[test]
    fn a_binary_gap_alone_leaves_the_review_complete() {
        assert!(
            !coverage_is_incomplete(&[], &[gap(CoverageGapKind::BinaryContent)]),
            "no textual patch exists to read, so the gap can never be closed"
        );
    }

    #[test]
    fn a_gap_that_retrying_could_close_leaves_the_review_incomplete() {
        assert!(coverage_is_incomplete(
            &[],
            &[
                gap(CoverageGapKind::BinaryContent),
                gap(CoverageGapKind::PatchUnavailable),
            ]
        ));
    }

    #[test]
    fn a_batch_budget_gap_blocks_only_when_it_fell_on_code_or_configuration() {
        assert!(
            !coverage_is_incomplete(&[], &[budget_gap(ReviewPriority::Prose)]),
            "the budget only cut prose, so every line of code was still reviewed"
        );
        assert!(
            coverage_is_incomplete(&[], &[budget_gap(ReviewPriority::SourceCode)]),
            "the budget cut source code, so the review cannot be declared complete"
        );
    }
}
