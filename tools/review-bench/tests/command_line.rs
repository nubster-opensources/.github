use std::process::Command;

#[test]
fn a_missing_key_is_reported_as_not_measured() {
    let output = Command::new(env!("CARGO_BIN_EXE_review-bench"))
        .args([
            "--cases",
            "tests/fixtures/cases/valid-injected",
            "--baseline",
            "missing.toml",
        ])
        .env_remove("MISTRAL_API_KEY")
        .output()
        .expect("run binary");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stdout).contains("not measured"));
}

#[test]
fn an_unknown_argument_prints_the_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_review-bench"))
        .arg("--bogus")
        .env("MISTRAL_API_KEY", "unused")
        .output()
        .expect("run binary");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage"));
}
