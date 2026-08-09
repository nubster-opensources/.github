use std::fs;
use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_repository_file(path: &str) -> String {
    let full_path = repository_root().join(path);
    fs::read_to_string(&full_path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", full_path.display()))
}

fn assert_remote_actions_are_pinned(workflow_path: &str) {
    let workflow = read_repository_file(workflow_path);
    for line in workflow.lines() {
        let Some(uses) = line.trim().strip_prefix("- uses: ") else {
            continue;
        };
        if uses.starts_with("./") {
            continue;
        }
        let reference = uses
            .split('#')
            .next()
            .expect("split always returns one item")
            .trim()
            .rsplit_once('@')
            .unwrap_or_else(|| panic!("action has no revision in {workflow_path}: {uses}"))
            .1;
        assert!(
            reference.len() == 40
                && reference
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "action is not pinned to a full commit SHA in {workflow_path}: {uses}"
        );
    }
}

#[test]
fn reusable_workflow_checks_out_its_own_revision() {
    let workflow = read_repository_file(".github/workflows/ai-review.yml");

    assert!(workflow.contains("repository: ${{ job.workflow_repository }}"));
    assert!(workflow.contains("ref: ${{ job.workflow_sha }}"));
    assert!(!workflow.contains("repository: nubster-opensources/.github"));
    assert!(!workflow.contains("ref: main"));
}

#[test]
fn reusable_workflow_skips_untrusted_pull_requests_before_using_secrets() {
    let workflow = read_repository_file(".github/workflows/ai-review.yml");

    assert!(workflow.contains("mistral-api-key:\n        required: false"));
    assert!(workflow.contains("github.event.pull_request.head.repo.full_name == github.repository"));
    assert!(workflow.contains("github.actor != 'dependabot[bot]'"));
    assert!(workflow.contains("ai-review-skipped:"));
}

#[test]
fn reusable_workflow_cancels_stale_runs_per_pr_and_mode() {
    let workflow = read_repository_file(".github/workflows/ai-review.yml");

    assert!(workflow.contains(
        "group: ai-review-${{ github.repository }}-${{ inputs.pr-number }}-${{ inputs.mode }}"
    ));
    assert!(workflow.contains("cancel-in-progress: true"));
}

#[test]
fn workflows_pin_every_remote_action_to_a_commit() {
    assert_remote_actions_are_pinned(".github/workflows/ai-review.yml");
    assert_remote_actions_are_pinned(".github/workflows/ci.yml");
}

#[test]
fn documented_caller_is_safe_for_forks_and_stale_runs() {
    let readme = read_repository_file("README.md");

    assert!(readme.contains("github.event.pull_request.head.repo.full_name == github.repository"));
    assert!(readme.contains("cancel-in-progress: true"));
    assert!(readme.contains("@<reviewed-commit-sha>"));
    assert!(!readme.contains("ai-review.yml@main"));
}
