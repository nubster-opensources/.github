//! Mistral chat-completion adapter for the language model port.

use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use crate::language_model::{ChatRequest, LanguageModel};

const API_URL: &str = "https://api.mistral.ai/v1/chat/completions";
const MAX_REQUEST_ATTEMPTS: usize = 3;
const RETRY_DELAY_MILLIS: u64 = 250;

/// Maximum duration of one Mistral API request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Mistral chat-completion API adapter for the [`LanguageModel`] port.
pub struct MistralHttp {
    client: reqwest::Client,
    api_key: String,
    endpoint: String,
}

impl MistralHttp {
    /// Builds the adapter with the production endpoint and request timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when the HTTP client cannot be built.
    pub fn new(api_key: String) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent("ai-review-bot/0.1")
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            client,
            api_key,
            endpoint: API_URL.to_string(),
        })
    }
}

impl LanguageModel for MistralHttp {
    async fn complete(&self, request: &ChatRequest) -> anyhow::Result<String> {
        send_request_to(&self.client, &self.api_key, request, &self.endpoint).await
    }
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

async fn send_request_to(
    client: &reqwest::Client,
    api_key: &str,
    request: &ChatRequest,
    endpoint: &str,
) -> anyhow::Result<String> {
    for attempt in 1..=MAX_REQUEST_ATTEMPTS {
        match client
            .post(endpoint)
            .bearer_auth(api_key)
            .json(request)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {
                let chat: ChatResponse = response
                    .json()
                    .await
                    .context("failed to parse Mistral API response")?;
                return chat
                    .choices
                    .into_iter()
                    .next()
                    .map(|choice| choice.message.content)
                    .context("Mistral returned no choices");
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                if is_retryable_status(status) && attempt < MAX_REQUEST_ATTEMPTS {
                    eprintln!(
                        "warning: Mistral API returned {status}; retrying request {attempt}/{MAX_REQUEST_ATTEMPTS}"
                    );
                    wait_before_retry(attempt).await;
                    continue;
                }
                anyhow::bail!("Mistral API error {status}: {body}");
            }
            Err(error) if attempt < MAX_REQUEST_ATTEMPTS => {
                eprintln!(
                    "warning: Mistral API request failed: {error}; retrying request {attempt}/{MAX_REQUEST_ATTEMPTS}"
                );
                wait_before_retry(attempt).await;
            }
            Err(error) => return Err(error).context("failed to reach Mistral API"),
        }
    }

    unreachable!("the request loop either returns a response or an error")
}

fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

async fn wait_before_retry(attempt: usize) {
    let delay = Duration::from_millis(RETRY_DELAY_MILLIS * attempt as u64);
    tokio::time::sleep(delay).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::{CallPurpose, ResponseFormat};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const SUCCESS_RESPONSE: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 42\r\n\r\n{\"choices\":[{\"message\":{\"content\":\"ok\"}}]}";
    const TOO_MANY_REQUESTS_RESPONSE: &str =
        "HTTP/1.1 429 Too Many Requests\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
    const SERVER_ERROR_RESPONSE: &str =
        "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
    const UNAUTHORIZED_RESPONSE: &str =
        "HTTP/1.1 401 Unauthorized\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";

    async fn response_server(
        responses: Vec<&'static str>,
    ) -> (String, tokio::task::JoinHandle<usize>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("read test server address");
        let server = tokio::spawn(async move {
            let mut request_count = 0;
            for response in responses {
                let Ok(Ok((mut stream, _))) =
                    tokio::time::timeout(Duration::from_secs(2), listener.accept()).await
                else {
                    break;
                };
                let mut request = [0_u8; 4_096];
                let _bytes_read = stream.read(&mut request).await.expect("read test request");
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write test response");
                stream.shutdown().await.expect("close test response");
                request_count += 1;
            }
            request_count
        });
        (format!("http://{address}"), server)
    }

    fn test_request() -> ChatRequest {
        ChatRequest {
            purpose: CallPurpose::Analysis,
            model: "test".to_string(),
            messages: Vec::new(),
            response_format: ResponseFormat {
                kind: "json_object",
            },
            temperature: 0.0,
        }
    }

    #[tokio::test]
    async fn retries_a_transient_response_before_returning_success() {
        let (endpoint, server) = response_server(vec![
            SERVER_ERROR_RESPONSE,
            TOO_MANY_REQUESTS_RESPONSE,
            SUCCESS_RESPONSE,
        ])
        .await;
        let response = send_request_to(&reqwest::Client::new(), "key", &test_request(), &endpoint)
            .await
            .expect("retry succeeds");

        assert_eq!(response, "ok");
        assert_eq!(server.await.expect("test server joins"), 3);
    }

    #[tokio::test]
    async fn does_not_retry_an_authentication_failure() {
        let (endpoint, server) =
            response_server(vec![UNAUTHORIZED_RESPONSE, SUCCESS_RESPONSE]).await;
        let result =
            send_request_to(&reqwest::Client::new(), "key", &test_request(), &endpoint).await;

        assert!(result.is_err());
        assert_eq!(server.await.expect("test server joins"), 1);
    }

    #[tokio::test]
    async fn stops_after_the_bounded_number_of_transient_failures() {
        let (endpoint, server) = response_server(vec![
            SERVER_ERROR_RESPONSE,
            SERVER_ERROR_RESPONSE,
            SERVER_ERROR_RESPONSE,
        ])
        .await;
        let result =
            send_request_to(&reqwest::Client::new(), "key", &test_request(), &endpoint).await;

        assert!(result.is_err());
        assert_eq!(
            server.await.expect("test server joins"),
            MAX_REQUEST_ATTEMPTS
        );
    }

    #[test]
    fn retries_transient_responses_but_not_authentication_failures() {
        assert!(is_retryable_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(!is_retryable_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(reqwest::StatusCode::BAD_REQUEST));
    }
}
