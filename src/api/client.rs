use std::env;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::{ApiError, ProcessMode};

// Defaults — overridable via environment variables.
const DEFAULT_API_ENDPOINT: &str = "http://localhost:8000/v1";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const DEFAULT_MODEL_NAME: &str = "MiniMaxAI/MiniMax-M2.5";
const TEMPERATURE: f64 = 0.1;
const MAX_TOKENS: u32 = 1024;
const REQUEST_TIMEOUT_SECS: u64 = 30;

// -- Request types (OpenAI chat completions schema) --

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f64,
    max_tokens: u32,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

// -- Response types --

#[derive(Deserialize)]
pub(crate) struct ChatResponse {
    pub choices: Vec<Choice>,
}

#[derive(Deserialize)]
pub(crate) struct Choice {
    pub message: ResponseMessage,
}

#[derive(Deserialize)]
pub(crate) struct ResponseMessage {
    pub content: String,
}

// -- Client --

struct LlmClientInner {
    client: Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
}

#[derive(Clone)]
pub struct LlmClient(Arc<LlmClientInner>);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_response() {
        let json = r#"{"choices":[{"message":{"content":"hello"}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content, "hello");
    }

    #[test]
    fn parse_empty_choices() {
        let json = r#"{"choices":[]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.choices.is_empty());
    }

    #[test]
    fn parse_ignores_extra_fields() {
        let json = r#"{"id":"x","choices":[{"index":0,"message":{"role":"assistant","content":"hi"}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content, "hi");
    }
}

impl LlmClient {
    pub fn new() -> Result<Self, ApiError> {
        let base = env::var("CLIP_LLM_API_ENDPOINT")
            .unwrap_or_else(|_| DEFAULT_API_ENDPOINT.to_string());
        let endpoint = format!("{}{}", base.trim_end_matches('/'), CHAT_COMPLETIONS_PATH);
        let model =
            env::var("CLIP_LLM_MODEL").unwrap_or_else(|_| DEFAULT_MODEL_NAME.to_string());
        let api_key = env::var("CLIP_LLM_API_KEY").ok();

        info!("endpoint={endpoint}, model={model}, api_key={}", if api_key.is_some() { "set" } else { "unset" });

        let client = Client::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()?;
        Ok(Self(Arc::new(LlmClientInner {
            client,
            endpoint,
            model,
            api_key,
        })))
    }

    /// Send user text to the vLLM server and return the raw response content.
    /// Think-block stripping is handled separately by `response::strip_think_blocks`.
    pub async fn complete(&self, user_text: &str, mode: ProcessMode) -> Result<String, ApiError> {
        let inner = &self.0;
        let sys_prompt = mode.system_prompt();
        let body = ChatRequest {
            model: &inner.model,
            messages: vec![
                Message {
                    role: "system",
                    content: &sys_prompt,
                },
                Message {
                    role: "user",
                    content: user_text,
                },
            ],
            temperature: TEMPERATURE,
            max_tokens: MAX_TOKENS,
        };

        info!("sending request to {}", inner.endpoint);
        debug!("model={}, temperature={}, max_tokens={}", inner.model, TEMPERATURE, MAX_TOKENS);
        debug!("request body: {}", serde_json::to_string(&body).unwrap_or_default());

        let mut req = inner.client.post(&inner.endpoint).json(&body);
        if let Some(key) = &inner.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await?.error_for_status()?;

        let chat: ChatResponse = resp.json().await?;

        let content = chat
            .choices
            .into_iter()
            .next()
            .ok_or(ApiError::EmptyResponse)?
            .message
            .content;

        if content.is_empty() {
            return Err(ApiError::EmptyResponse);
        }

        info!("received response ({} chars)", content.len());
        debug!("response content: {content}");
        Ok(content)
    }
}
