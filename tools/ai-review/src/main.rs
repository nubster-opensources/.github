mod github;
mod mistral;
mod review;
mod types;

use anyhow::Context;
use github::InlineComment;
use octocrab::OctocrabBuilder;
use types::Mode;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mode = Mode::from_env()?;

    let pr_number: u64 = std::env::var("PR_NUMBER")
        .context("PR_NUMBER not set")?
        .parse()
        .context("PR_NUMBER must be a number")?;

    let repository =
        std::env::var("GITHUB_REPOSITORY").context("GITHUB_REPOSITORY not set")?;
    let (owner, repo) = repository
        .split_once('/')
        .context("GITHUB_REPOSITORY must be in owner/repo format")?;

    let github_token =
        std::env::var("GITHUB_TOKEN").context("GITHUB_TOKEN not set")?;
    let mistral_key =
        std::env::var("MISTRAL_API_KEY").context("MISTRAL_API_KEY not set")?;

    let octo = OctocrabBuilder::default()
        .personal_token(github_token.clone())
        .build()
        .context("failed to build Octocrab client")?;

    let http = reqwest::Client::builder()
        .user_agent("ai-review-bot/0.1")
        .build()?;

    match mode {
        Mode::Review => {
            run_review(&octo, &http, &github_token, &mistral_key, owner, repo, pr_number)
                .await?;
        }
        Mode::Describe => {
            run_describe(&octo, &http, &mistral_key, owner, repo, pr_number).await?;
        }
    }

    Ok(())
}

async fn run_review(
    octo: &octocrab::Octocrab,
    http: &reqwest::Client,
    github_token: &str,
    mistral_key: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<()> {
    println!("Fetching PR #{pr_number} diff…");
    let (diff, file_count) = github::fetch_diff(octo, owner, repo, pr_number).await?;

    if diff.trim().is_empty() {
        println!("Empty diff — nothing to review.");
        return Ok(());
    }

    let model = Mode::Review.mistral_model();
    println!("Calling Mistral ({model})…");
    let (response, truncated) =
        mistral::call_review(http, mistral_key, model, &diff).await?;

    let global_body = review::render_global_comment(&response, file_count, model, truncated);
    println!("Upserting global comment…");
    github::upsert_global_comment(octo, owner, repo, pr_number, &global_body).await?;

    let inline = review::inline_findings(&response);
    if !inline.is_empty() {
        let head_sha = github::fetch_head_sha(octo, owner, repo, pr_number).await?;
        let comments: Vec<InlineComment> = inline
            .into_iter()
            .map(|f| InlineComment {
                path: f.file.clone(),
                line: f.line,
                body: f.message.clone(),
            })
            .collect();

        println!("Posting {} inline comment(s)…", comments.len());
        github::post_inline_comments(
            github_token,
            owner,
            repo,
            pr_number,
            &head_sha,
            &comments,
        )
        .await?;
    }

    println!("Review complete.");
    Ok(())
}

async fn run_describe(
    octo: &octocrab::Octocrab,
    http: &reqwest::Client,
    mistral_key: &str,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<()> {
    let current_body = github::fetch_pr_body(octo, owner, repo, pr_number).await?;
    let is_empty = current_body.trim().is_empty()
        || current_body.trim().starts_with("<!--");

    if !is_empty {
        println!("PR body already set — skipping describe mode.");
        return Ok(());
    }

    println!("Fetching PR #{pr_number} diff for description…");
    let (diff, _) = github::fetch_diff(octo, owner, repo, pr_number).await?;

    let model = Mode::Describe.mistral_model();
    println!("Calling Mistral ({model})…");
    let generated_body = mistral::call_describe(http, mistral_key, model, &diff).await?;

    println!("Updating PR body…");
    github::update_pr_body(octo, owner, repo, pr_number, &generated_body).await?;

    println!("Description generated.");
    Ok(())
}
