use std::process::Command;

#[test]
fn empty_mistral_key_stops_the_executable_before_external_calls() {
    let output = Command::new(env!("CARGO_BIN_EXE_ai-review"))
        .env_clear()
        .env("GITHUB_REPOSITORY", "owner/repository")
        .env("GITHUB_TOKEN", "test-token")
        .env("MISTRAL_API_KEY", "   ")
        .env("PR_NUMBER", "1")
        .output()
        .expect("ai-review executable should start");

    assert!(!output.status.success());

    let standard_error =
        String::from_utf8(output.stderr).expect("ai-review should write UTF-8 errors to stderr");
    assert!(standard_error.contains("MISTRAL_API_KEY is empty"));
}
