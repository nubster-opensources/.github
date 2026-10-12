use std::path::{Path, PathBuf};

use ai_review::PatchAvailability;
use review_bench::case::{load_case, load_cases, CaseOrigin};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/cases")
        .join(name)
}

#[test]
fn a_valid_case_is_loaded_with_its_patch_and_head_file() {
    let case = load_case(&fixture("valid-injected")).expect("valid case");
    assert_eq!(case.id, "valid-injected");
    assert!(matches!(case.origin, CaseOrigin::Injected));
    assert_eq!(case.files.len(), 1);
    assert!(matches!(case.files[0].patch, PatchAvailability::Present(_)));
    assert!(case.files[0].added_lines.contains(&2));
    assert_eq!(
        case.head_files.get("src/lib.rs").map(String::as_str),
        Some("fn main() {\n    init();\n}\n")
    );
    assert_eq!(case.expected_bugs[0].first_line, 2);
    assert_eq!(case.expected_bugs[0].keywords, vec!["panic".to_string()]);
}

#[test]
fn a_real_case_without_its_fix_commit_is_rejected() {
    let error = load_case(&fixture("real-without-commit")).expect_err("must fail");
    let message = format!("{error:#}");
    assert!(message.contains("real-without-commit"));
    assert!(message.contains("fix_commit"));
}

#[test]
fn a_file_reference_outside_the_case_is_rejected() {
    let error = load_case(&fixture("escaping-path")).expect_err("must fail");
    assert!(format!("{error:#}").contains("outside the case"));
}

#[test]
fn an_expected_bug_on_an_unknown_file_is_rejected() {
    let error = load_case(&fixture("unknown-bug-file")).expect_err("must fail");
    assert!(format!("{error:#}").contains("src/other.rs"));
}

#[test]
fn loading_a_directory_reports_the_first_invalid_case() {
    assert!(load_cases(&fixture("")).is_err());
}
