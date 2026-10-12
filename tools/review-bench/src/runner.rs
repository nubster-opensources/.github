//! Execution of benchmark cases against the team review.

use std::fmt;
use std::sync::Arc;

use ai_review::{
    run_team_review, DiffContext, LanguageModel, SynthFinding, TeamOutcome, TeamReviewInput,
};

use crate::case::BenchCase;
use crate::fixture_head_files::FixtureHeadFiles;
use crate::matching::{match_findings, CaseScore};

/// A review run that cannot be scored.
#[derive(Debug)]
pub struct IncompleteRun {
    /// Identifier of the case being run.
    pub case_id: String,
    /// Run number, as given by the caller.
    pub run: usize,
    /// Why the run cannot be scored.
    pub reason: String,
}

impl fmt::Display for IncompleteRun {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "case {}, run {}: {}",
            self.case_id, self.run, self.reason
        )
    }
}

impl std::error::Error for IncompleteRun {}

/// Confirmed (non-contested) findings of one run, or why the run cannot be scored.
///
/// # Errors
///
/// Returns [`IncompleteRun`] when the case context is invalid or the review is incomplete.
pub async fn run_case<M: LanguageModel>(
    model: &Arc<M>,
    case: &BenchCase,
    run: usize,
) -> Result<Vec<SynthFinding>, IncompleteRun> {
    let incomplete = |reason: String| IncompleteRun {
        case_id: case.id.clone(),
        run,
        reason,
    };
    let context = DiffContext::from_changed_files(case.files.clone(), None)
        .map_err(|error| incomplete(format!("invalid case context: {error}")))?;
    let head_files = FixtureHeadFiles(&case.head_files);
    let outcome =
        run_team_review(model, &head_files, TeamReviewInput::without_cache(context)).await;
    match outcome {
        TeamOutcome::Incomplete { reason, .. } => Err(incomplete(reason.to_string())),
        TeamOutcome::Reviewed(review) => {
            if !review.is_complete() {
                return Err(incomplete("blocking coverage gaps".to_string()));
            }
            Ok(review
                .scored()
                .iter()
                .filter(|(_, verdict)| !verdict.contested)
                .map(|(finding, _)| finding.clone())
                .collect())
        }
    }
}

/// Score of one full pass over every case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunScore {
    /// Score of each case, keyed by case identifier.
    pub per_case: Vec<(String, CaseScore)>,
    /// Expected bugs found, summed over the cases.
    pub found: usize,
    /// False alerts, summed over the cases.
    pub false_alerts: usize,
}

/// Score of a whole measure: every run and the medians.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchScore {
    /// One score per run.
    pub runs: Vec<RunScore>,
    /// Median of the bugs found per run.
    pub median_found: usize,
    /// Median of the false alerts per run.
    pub median_false_alerts: usize,
    /// Total expected bugs over all cases.
    pub expected_bugs: usize,
}

/// Runs every case `runs` times; any incomplete run aborts the whole measure.
///
/// # Errors
///
/// Returns the first [`IncompleteRun`] met.
pub async fn run_bench<M: LanguageModel>(
    model: &Arc<M>,
    cases: &[BenchCase],
    runs: usize,
    window: u32,
) -> Result<BenchScore, IncompleteRun> {
    let mut run_scores = Vec::with_capacity(runs);
    for run in 1..=runs {
        let mut per_case = Vec::with_capacity(cases.len());
        let mut found = 0;
        let mut false_alerts = 0;
        for case in cases {
            let findings = run_case(model, case, run).await?;
            let score = match_findings(&findings, &case.expected_bugs, window);
            found += score.found.len();
            false_alerts += score.false_alerts;
            per_case.push((case.id.clone(), score));
        }
        run_scores.push(RunScore {
            per_case,
            found,
            false_alerts,
        });
    }
    let founds: Vec<usize> = run_scores.iter().map(|score| score.found).collect();
    let alerts: Vec<usize> = run_scores.iter().map(|score| score.false_alerts).collect();
    Ok(BenchScore {
        runs: run_scores,
        median_found: median(&founds),
        median_false_alerts: median(&alerts),
        expected_bugs: cases.iter().map(|case| case.expected_bugs.len()).sum(),
    })
}

/// Median of the values; the lower one for an even count, 0 for an empty list.
#[must_use]
pub fn median(values: &[usize]) -> usize {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[(sorted.len() - 1) / 2]
}
