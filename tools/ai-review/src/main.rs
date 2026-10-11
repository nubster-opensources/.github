#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ai_review::run_from_env().await
}
