use std::sync::Arc;

use ai_review::{
    run_team_review, CallPurpose, ChangedFile, ChangedFileStatus, ChatRequest, DiffContext,
    HeadFileSource, LanguageModel, TeamOutcome, TeamReviewInput, Verdict,
};

const PATCH: &str = "@@ -1,2 +1,3 @@\n fn main() {\n+    init();\n }";
const AGENT_REPORT: &str = r#"{"summary":"looked","strengths":[],"findings":[{"file":"src/lib.rs","line":2,"severity":"critical","message":"init can panic"}],"security":"none"}"#;
const SYNTHESIS: &str = r#"{"executive_summary":"summary","findings":[{"file":"src/lib.rs","line":2,"severity":"critical","category":"bug","message":"init can panic"}]}"#;
const CONFIRMING_LENS: &str = r#"{"contested":false,"reason":"confirmed"}"#;

struct ExternalModel;

impl LanguageModel for ExternalModel {
    async fn complete(&self, request: &ChatRequest) -> anyhow::Result<String> {
        assert!(!request.model().is_empty());
        assert!(!request.messages().is_empty());
        match request.purpose() {
            CallPurpose::Agent(_) => Ok(AGENT_REPORT.to_string()),
            CallPurpose::Synthesis => Ok(SYNTHESIS.to_string()),
            CallPurpose::Lens(_) => Ok(CONFIRMING_LENS.to_string()),
            other => anyhow::bail!("unexpected call {other:?}"),
        }
    }
}

struct NoHeadFiles;

impl HeadFileSource for NoHeadFiles {
    async fn read(&self, _path: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
}

#[tokio::test]
async fn an_external_crate_can_run_the_team_review_core() {
    let file = ChangedFile::from_patch(
        "src/lib.rs".to_string(),
        ChangedFileStatus::Modified,
        Some(PATCH.to_string()),
        1,
        0,
    );
    let context = DiffContext::from_changed_files(vec![file], None).expect("context");
    let model = Arc::new(ExternalModel);

    let outcome = run_team_review(
        &model,
        &NoHeadFiles,
        TeamReviewInput::without_cache(context),
    )
    .await;

    let TeamOutcome::Reviewed(review) = outcome else {
        panic!("expected a completed review");
    };
    assert!(review.is_complete());
    assert_eq!(review.verdict(), Verdict::NeedsWork);
    assert_eq!(review.scored().len(), 1);
    assert!(!review.scored()[0].1.contested);
    assert!(review.coverage_gaps().is_empty());
}
