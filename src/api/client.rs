use std::env;
use std::time::Duration;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::ApiError;

// Defaults — overridable via environment variables.
const DEFAULT_API_ENDPOINT: &str = "http://localhost:8000/v1";
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const DEFAULT_MODEL_NAME: &str = "MiniMaxAI/MiniMax-M2.5";
const SYSTEM_PROMPT: &str = "You are a helpful assistant.";
const TEMPERATURE: f64 = 0.3;
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

pub struct LlmClient {
    client: Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
}

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
        Ok(Self {
            client,
            endpoint,
            model,
            api_key,
        })
    }

    /// Send user text to the vLLM server and return the raw response content.
    /// Think-block stripping is handled separately by `response::strip_think_blocks`.
    pub fn complete(&self, user_text: &str) -> Result<String, ApiError> {
        let body = ChatRequest {
            model: &self.model,
            messages: vec![
                Message {
                    role: "system",
                    content: SYSTEM_PROMPT,
                },
                Message {
                    role: "user",
                    content: user_text,
                },
            ],
            temperature: TEMPERATURE,
            max_tokens: MAX_TOKENS,
        };

        info!("sending request to {}", self.endpoint);
        debug!("model={}, temperature={}, max_tokens={}", self.model, TEMPERATURE, MAX_TOKENS);

        let mut req = self.client.post(&self.endpoint).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send()?.error_for_status()?;

        let chat: ChatResponse = resp.json()?;

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
        Ok(content)
    }
}
