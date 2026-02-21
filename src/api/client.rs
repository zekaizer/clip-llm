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
const MAX_TOKENS: u32 = 16384;
const REQUEST_TIMEOUT_SECS: u64 = 30;

// -- Request types (OpenAI chat completions schema) --

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    temperature: f64,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
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

// -- SSE streaming types (used by worker in streaming loop) --

#[allow(dead_code)]
#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct StreamChoice {
    delta: Delta,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct Delta {
    content: Option<String>,
}

/// Parsed SSE event from a streaming response.
#[allow(dead_code)]
#[derive(Debug, PartialEq)]
pub(crate) enum SseEvent {
    Content(String),
    Done,
}

/// Line-based SSE parser that buffers incomplete lines across chunks.
#[allow(dead_code)]
pub(crate) struct SseParser {
    buffer: String,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl SseParser {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
        }
    }

    /// Feed raw bytes from `reqwest::Response::chunk()` and return parsed events.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));

        let mut events = Vec::new();
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..pos].trim_end_matches('\r').to_string();
            self.buffer = self.buffer[pos + 1..].to_string();

            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };

            if data == "[DONE]" {
                events.push(SseEvent::Done);
                continue;
            }

            if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                if let Some(choice) = chunk.choices.first() {
                    if let Some(content) = &choice.delta.content {
                        if !content.is_empty() {
                            events.push(SseEvent::Content(content.clone()));
                        }
                    }
                }
            }
        }

        events
    }
}

// -- Client --

struct LlmClientInner {
    client: Client,
    streaming_client: Client,
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

    // --- SseParser tests ---

    #[test]
    fn sse_single_event() {
        let mut p = SseParser::new();
        let events =
            p.feed(br#"data: {"choices":[{"delta":{"content":"hello"}}]}"#.as_ref());
        // No newline yet — line is incomplete.
        assert!(events.is_empty());

        let events = p.feed(b"\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Content(s) if s == "hello"));
    }

    #[test]
    fn sse_done_event() {
        let mut p = SseParser::new();
        let events = p.feed(b"data: [DONE]\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Done));
    }

    #[test]
    fn sse_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.feed(br#"data: {"choices":[{"de"#).is_empty());
        let events = p.feed(br#"lta":{"content":"hi"}}]}"#.as_ref());
        assert!(events.is_empty()); // still no newline

        let events = p.feed(b"\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Content(s) if s == "hi"));
    }

    #[test]
    fn sse_multiple_events() {
        let mut p = SseParser::new();
        let input = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"b\"}}]}\n",
        );
        let events = p.feed(input.as_bytes());
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], SseEvent::Content(s) if s == "a"));
        assert!(matches!(&events[1], SseEvent::Content(s) if s == "b"));
    }

    #[test]
    fn sse_role_only_delta_skipped() {
        let mut p = SseParser::new();
        let events =
            p.feed(br#"data: {"choices":[{"delta":{"role":"assistant"}}]}"#.as_ref());
        assert!(events.is_empty()); // incomplete line
        let events = p.feed(b"\n");
        assert!(events.is_empty()); // no content field
    }

    #[test]
    fn sse_non_data_lines_ignored() {
        let mut p = SseParser::new();
        let input = concat!(
            ": comment\n",
            "event: message\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n",
        );
        let events = p.feed(input.as_bytes());
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Content(s) if s == "x"));
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
        // Streaming client: connect timeout only, no total body timeout.
        let streaming_client = Client::builder()
            .connect_timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()?;
        Ok(Self(Arc::new(LlmClientInner {
            client,
            streaming_client,
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
            stream: None,
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

    /// Start a streaming request. Returns the raw `reqwest::Response` whose body
    /// the caller reads via `chunk()` and feeds into [`SseParser`].
    pub async fn complete_stream(
        &self,
        user_text: &str,
        mode: ProcessMode,
    ) -> Result<reqwest::Response, ApiError> {
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
            stream: Some(true),
        };

        info!("sending streaming request to {}", inner.endpoint);

        let mut req = inner.streaming_client.post(&inner.endpoint).json(&body);
        if let Some(key) = &inner.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await?.error_for_status()?;
        Ok(resp)
    }
}
