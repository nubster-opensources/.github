use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use ai_review::{CallPurpose, ChangedFile, ChangedFileStatus, ChatRequest, LanguageModel, Lens};
use review_bench::case::{BenchCase, CaseOrigin, ExpectedBug};
use review_bench::runner::{median, run_bench, run_case};

const PATCH: &str = "@@ -1,2 +1,3 @@\n fn main() {\n+    init();\n }";
const AGENT_REPORT: &str = r#"{"summary":"s","strengths":[],"findings":[],"security":"none"}"#;
const ONE_FINDING: &str = r#"{"executive_summary":"s","findings":[{"file":"src/lib.rs","line":2,"severity":"critical","category":"bug","message":"init may panic"}]}"#;

struct ScriptModel {
    synthesis: String,
    contested_lens: bool,
    failing_agent: bool,
    calls: AtomicUsize,
}

impl LanguageModel for ScriptModel {
    async fn complete(&self, request: &ChatRequest) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match request.purpose() {
            CallPurpose::Agent(_) if self.failing_agent => anyhow::bail!("agent down"),
            CallPurpose::Agent(_) => Ok(AGENT_REPORT.to_string()),
            CallPurpose::Synthesis => Ok(self.synthesis.clone()),
            CallPurpose::Lens(Lens::FalsePositive) if self.contested_lens => {
                Ok(r#"{"contested":true,"reason":"not reachable"}"#.to_string())
            }
            CallPurpose::Lens(_) => Ok(r#"{"contested":false,"reason":"ok"}"#.to_string()),
            other => anyhow::bail!("unexpected {other:?}"),
        }
    }
}

fn model(synthesis: &str, contested_lens: bool, failing_agent: bool) -> Arc<ScriptModel> {
    Arc::new(ScriptModel {
        synthesis: synthesis.to_string(),
        contested_lens,
        failing_agent,
        calls: AtomicUsize::new(0),
    })
}

fn case(expected: Vec<ExpectedBug>) -> BenchCase {
    BenchCase {
        id: "case-a".to_string(),
        origin: CaseOrigin::Injected,
        files: vec![ChangedFile::from_patch(
            "src/lib.rs".to_string(),
            ChangedFileStatus::Modified,
            Some(PATCH.to_string()),
            1,
            0,
        )],
        head_files: HashMap::new(),
        expected_bugs: expected,
    }
}

fn panic_bug() -> ExpectedBug {
    ExpectedBug {
        file: "src/lib.rs".to_string(),
        first_line: 2,
        last_line: 2,
        keywords: vec!["panic".to_string()],
    }
}

#[tokio::test]
async fn a_confirmed_finding_is_returned() {
    let findings = run_case(
        &model(ONE_FINDING, false, false),
        &case(vec![panic_bug()]),
        1,
    )
    .await
    .expect("complete run");
    assert_eq!(findings.len(), 1);
}

#[tokio::test]
async fn a_contested_finding_does_not_count() {
    let findings = run_case(
        &model(ONE_FINDING, true, false),
        &case(vec![panic_bug()]),
        1,
    )
    .await
    .expect("complete run");
    assert!(findings.is_empty());
}

#[tokio::test]
async fn an_incomplete_review_is_an_error_not_a_score() {
    let error = run_case(
        &model(ONE_FINDING, false, true),
        &case(vec![panic_bug()]),
        2,
    )
    .await
    .expect_err("incomplete");
    assert_eq!(error.case_id, "case-a");
    assert_eq!(error.run, 2);
    assert!(error.to_string().contains("case case-a, run 2"));
}

#[tokio::test]
async fn the_bench_runs_each_case_the_requested_number_of_times() {
    let scripted = model(ONE_FINDING, false, false);
    let score = run_bench(&scripted, &[case(vec![panic_bug()])], 3, 0)
        .await
        .expect("score");
    assert_eq!(score.runs.len(), 3);
    assert_eq!(score.median_found, 1);
    assert_eq!(score.median_false_alerts, 0);
    assert_eq!(score.expected_bugs, 1);
    assert_eq!(scripted.calls.load(Ordering::SeqCst), 3 * (4 + 1 + 3));
}

#[tokio::test]
async fn one_incomplete_run_aborts_the_whole_measure() {
    let result = run_bench(
        &model(ONE_FINDING, false, true),
        &[case(vec![panic_bug()])],
        3,
        0,
    )
    .await;
    assert!(result.is_err());
}

#[test]
fn the_median_takes_the_middle_value_and_the_lower_one_on_ties() {
    assert_eq!(median(&[3, 1, 2]), 2);
    assert_eq!(median(&[4, 1, 3, 2]), 2);
    assert_eq!(median(&[]), 0);
}
