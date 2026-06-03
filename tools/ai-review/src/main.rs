mod github;
mod mistral;
mod review;
mod team;
mod types;

use anyhow::Context;
use github::InlineComment;
use octocrab::OctocrabBuilder;
use types::Mode;

struct Clients {
    octo: octocrab::Octocrab,
    http: reqwest::Client,
    github_token: String,
    mistral_key: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mode = Mode::from_env()?;

    let pr_number: u64 = std::env::var("PR_NUMBER")
        .context("PR_NUMBER not set")?
        .parse()
        .context("PR_NUMBER must be a number")?;

    let repository = std::env::var("GITHUB_REPOSITORY").context("GITHUB_REPOSITORY not set")?;
    let (owner, repo) = repository
        .split_once('/')
        .context("GITHUB_REPOSITORY must be in owner/repo format")?;

    let github_token = std::env::var("GITHUB_TOKEN").context("GITHUB_TOKEN not set")?;
    let mistral_key = std::env::var("MISTRAL_API_KEY").context("MISTRAL_API_KEY not set")?;

    let clients = Clients {
        octo: OctocrabBuilder::default()
            .personal_token(github_token.clone())
            .build()
            .context("failed to build Octocrab client")?,
        http: reqwest::Client::builder()
            .user_agent("ai-review-bot/0.1")
            .build()?,
        github_token,
        mistral_key,
    };

    match mode {
        Mode::Review | Mode::Security | Mode::Architecture | Mode::Performance => {
            run_analysis(&mode, &clients, owner, repo, pr_number).await?;
        }
        Mode::Product => {
            run_product(&clients, owner, repo, pr_number).await?;
        }
        Mode::Describe => {
            run_describe(&clients, owner, repo, pr_number).await?;
        }
        Mode::Team => {
            team::run_team(&clients, owner, repo, pr_number).await?;
        }
    }

    Ok(())
}

async fn run_analysis(
    mode: &Mode,
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<()> {
    let model = mode.mistral_model();
    let marker = mode.comment_marker();
    let label = mode.display_label();

    println!("Fetching PR #{pr_number} diff…");
    let (diff, file_count) = github::fetch_diff(&clients.octo, owner, repo, pr_number).await?;

    if diff.trim().is_empty() {
        println!("Empty diff — nothing to review.");
        return Ok(());
    }

    println!("Calling Mistral ({model}) — {label}…");
    let (response, truncated) = match mode {
        Mode::Review => {
            mistral::call_review(&clients.http, &clients.mistral_key, model, &diff).await?
        }
        Mode::Security => {
            mistral::call_security(&clients.http, &clients.mistral_key, model, &diff).await?
        }
        Mode::Architecture => {
            mistral::call_architecture(&clients.http, &clients.mistral_key, model, &diff).await?
        }
        Mode::Performance => {
            mistral::call_performance(&clients.http, &clients.mistral_key, model, &diff).await?
        }
        _ => unreachable!(),
    };

    let global_body =
        review::render_global_comment(&response, file_count, model, truncated, marker, label);
    println!("Upserting global comment…");
    github::upsert_global_comment(&clients.octo, owner, repo, pr_number, &global_body, marker)
        .await?;

    let inline = review::inline_findings(&response);
    if !inline.is_empty() {
        let head_sha = github::fetch_head_sha(&clients.octo, owner, repo, pr_number).await?;
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
            &clients.github_token,
            owner,
            repo,
            pr_number,
            &head_sha,
            &comments,
        )
        .await?;
    }

    println!("{label} complete.");
    Ok(())
}

async fn run_product(
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<()> {
    let model = Mode::Product.mistral_model();
    let marker = Mode::Product.comment_marker();
    let label = Mode::Product.display_label();

    println!("Fetching PR #{pr_number} diff for product review…");
    let (diff, _) = github::fetch_diff(&clients.octo, owner, repo, pr_number).await?;

    if diff.trim().is_empty() {
        println!("Empty diff — nothing to review.");
        return Ok(());
    }

    println!("Calling Mistral ({model}) — {label}…");
    let body = mistral::call_product(&clients.http, &clients.mistral_key, model, &diff).await?;

    let full_body = format!("{marker}\n{body}");
    println!("Upserting product comment…");
    github::upsert_global_comment(&clients.octo, owner, repo, pr_number, &full_body, marker)
        .await?;

    println!("{label} complete.");
    Ok(())
}

async fn run_describe(
    clients: &Clients,
    owner: &str,
    repo: &str,
    pr_number: u64,
) -> anyhow::Result<()> {
    let current_body = github::fetch_pr_body(&clients.octo, owner, repo, pr_number).await?;
    let is_empty = current_body.trim().is_empty() || current_body.trim().starts_with("<!--");

    if !is_empty {
        println!("PR body already set — skipping describe mode.");
        return Ok(());
    }

    println!("Fetching PR #{pr_number} diff for description…");
    let (diff, _) = github::fetch_diff(&clients.octo, owner, repo, pr_number).await?;

    let model = Mode::Describe.mistral_model();
    println!("Calling Mistral ({model})…");
    let generated_body =
        mistral::call_describe(&clients.http, &clients.mistral_key, model, &diff).await?;

    println!("Updating PR body…");
    github::update_pr_body(&clients.octo, owner, repo, pr_number, &generated_body).await?;

    println!("Description generated.");
    Ok(())
}
