#[tokio::test]
async fn library_entry_point_rejects_an_unknown_mode() {
    std::env::set_var("AI_MODE", "not-a-mode");
    let error = ai_review::run_from_env()
        .await
        .expect_err("an unknown mode must be rejected");
    assert!(error.to_string().contains("unknown AI_MODE: not-a-mode"));
}
