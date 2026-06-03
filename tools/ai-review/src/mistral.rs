#![allow(clippy::missing_errors_doc)]

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::types::ReviewResponse;

const API_URL: &str = "https://api.mistral.ai/v1/chat/completions";
const MAX_DIFF_CHARS: usize = 20_000;

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    response_format: ResponseFormat,
    temperature: f32,
}

#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
}

#[derive(Deserialize)]
struct DescribeResponse {
    body: String,
}

fn review_system_prompt() -> String {
    r#"You are an expert code reviewer. Analyze the pull request diff and return ONLY a valid JSON object with this exact structure:
{
  "summary": "3-5 sentence overview of what this PR does and its quality",
  "strengths": ["specific strength 1", "specific strength 2"],
  "findings": [
    {
      "file": "exact/path/to/file.rs",
      "line": 42,
      "severity": "critical",
      "message": "specific, actionable issue description"
    }
  ],
  "security": "Security assessment: issues found or 'No security issues detected.'"
}

Rules:
- severity MUST be exactly "critical" or "minor"
- line MUST be the exact line number in the file (use 0 if no specific line applies)
- Only report real issues, not style preferences already enforced by a linter
- Keep messages concise and actionable (one sentence)
- Return valid JSON only, no markdown fences"#
        .to_string()
}

fn describe_system_prompt() -> String {
    r#"You are a helpful assistant that writes clear pull request descriptions.
Analyze the diff and return ONLY a valid JSON object:
{
  "body": "A clear markdown PR description explaining what was changed and why. Use bullet points. 3-8 sentences."
}
Return valid JSON only, no markdown fences."#
        .to_string()
}

/// Returns `(ReviewResponse, truncated)`.
pub async fn call_review(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    diff: &str,
) -> anyhow::Result<(ReviewResponse, bool)> {
    let (content, truncated) = truncate_diff(diff);

    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: review_system_prompt(),
            },
            Message {
                role: "user".to_string(),
                content: format!("Review this pull request diff:\n\n{content}"),
            },
        ],
        response_format: ResponseFormat { kind: "json_object" },
        temperature: 0.1,
    };

    let raw = send_request(client, api_key, &request).await?;
    let response: ReviewResponse = serde_json::from_str(&raw)
        .context("failed to parse Mistral review response as JSON")?;

    Ok((response, truncated))
}

/// Returns the generated PR body markdown.
pub async fn call_describe(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    diff: &str,
) -> anyhow::Result<String> {
    let (content, _) = truncate_diff(diff);

    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: "system".to_string(),
                content: describe_system_prompt(),
            },
            Message {
                role: "user".to_string(),
                content: format!("Generate a PR description for this diff:\n\n{content}"),
            },
        ],
        response_format: ResponseFormat { kind: "json_object" },
        temperature: 0.3,
    };

    let raw = send_request(client, api_key, &request).await?;
    let parsed: DescribeResponse =
        serde_json::from_str(&raw).context("failed to parse Mistral describe response")?;

    Ok(parsed.body)
}

fn truncate_diff(diff: &str) -> (&str, bool) {
    if diff.len() <= MAX_DIFF_CHARS {
        (diff, false)
    } else {
        (&diff[..MAX_DIFF_CHARS], true)
    }
}

async fn send_request(
    client: &reqwest::Client,
    api_key: &str,
    request: &ChatRequest,
) -> anyhow::Result<String> {
    let resp = client
        .post(API_URL)
        .bearer_auth(api_key)
        .json(request)
        .send()
        .await
        .context("failed to reach Mistral API")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Mistral API error {status}: {body}");
    }

    let chat: ChatResponse = resp
        .json()
        .await
        .context("failed to parse Mistral API response")?;
    chat.choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .context("Mistral returned no choices")
}
