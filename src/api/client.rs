use std::env;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;
use tracing::{debug, info, warn};

use crate::{ApiError, ClipboardContent, ProcessMode};

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
    content: MessageContent<'a>,
}

/// Polymorphic message content: plain string or multimodal parts array.
/// `#[serde(untagged)]` serializes Text as `"string"` and Parts as `[{...}]`.
#[derive(Serialize)]
#[cfg_attr(test, derive(Debug, PartialEq))]
#[serde(untagged)]
enum MessageContent<'a> {
    Text(&'a str),
    Parts(Vec<ContentPart>),
}

#[derive(Serialize)]
#[cfg_attr(test, derive(Debug, PartialEq))]
#[serde(tag = "type")]
enum ContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrl },
}

#[derive(Serialize)]
#[cfg_attr(test, derive(Debug, PartialEq))]
struct ImageUrl {
    url: String,
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
    supports_vision: OnceCell<bool>,
}

/// Minimal 1x1 transparent PNG for vision probe (67 bytes).
const PROBE_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

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

    // --- MessageContent serialization tests ---

    #[test]
    fn message_content_text_serializes_as_string() {
        let mc = MessageContent::Text("hello");
        let json = serde_json::to_value(&mc).unwrap();
        assert_eq!(json, serde_json::json!("hello"));
    }

    #[test]
    fn message_content_parts_serializes_as_array() {
        let mc = MessageContent::Parts(vec![
            ContentPart::Text {
                text: "describe".to_owned(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: "data:image/png;base64,abc".to_owned(),
                },
            },
        ]);
        let json = serde_json::to_value(&mc).unwrap();
        let arr = json.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "describe");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/png;base64,abc");
    }

    // --- build_user_content tests ---

    #[test]
    fn build_user_content_text_only() {
        let content = ClipboardContent::text_only("hello".into());
        let mc = LlmClient::build_user_content(&content, ProcessMode::Summarize, true);
        assert_eq!(mc, MessageContent::Text("hello"));
    }

    #[test]
    fn build_user_content_with_image_summarize() {
        let content = ClipboardContent {
            text: Some("caption".into()),
            images: vec![Arc::new(vec![0x89, 0x50])],
        };
        let mc = LlmClient::build_user_content(&content, ProcessMode::Summarize, true);
        match mc {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(&parts[0], ContentPart::Text { text } if text == "caption"));
                assert!(matches!(&parts[1], ContentPart::ImageUrl { .. }));
            }
            _ => panic!("expected Parts"),
        }
    }

    #[test]
    fn build_user_content_with_image_translate_ignores_image() {
        let content = ClipboardContent {
            text: Some("hello".into()),
            images: vec![Arc::new(vec![0x89])],
        };
        let mc = LlmClient::build_user_content(&content, ProcessMode::Translate, true);
        assert_eq!(mc, MessageContent::Text("hello"));
    }

    #[test]
    fn build_user_content_no_vision_ignores_image() {
        let content = ClipboardContent {
            text: Some("hello".into()),
            images: vec![Arc::new(vec![0x89])],
        };
        let mc = LlmClient::build_user_content(&content, ProcessMode::Summarize, false);
        assert_eq!(mc, MessageContent::Text("hello"));
    }

    #[test]
    fn build_user_content_image_only_no_text_part() {
        let content = ClipboardContent {
            text: None,
            images: vec![Arc::new(vec![0x89, 0x50])],
        };
        let mc = LlmClient::build_user_content(&content, ProcessMode::Summarize, true);
        match mc {
            MessageContent::Parts(parts) => {
                // Only image part, no text part since text is None.
                assert_eq!(parts.len(), 1);
                assert!(matches!(&parts[0], ContentPart::ImageUrl { .. }));
            }
            _ => panic!("expected Parts"),
        }
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
            supports_vision: OnceCell::new(),
        })))
    }

    /// Probe whether the model supports vision by sending a tiny image request.
    /// Result is cached in `OnceCell`. Network/server errors skip caching (retry next time).
    pub async fn probe_vision(&self) -> bool {
        let inner = &self.0;
        if let Some(&cached) = inner.supports_vision.get() {
            return cached;
        }

        let data_uri = format!("data:image/png;base64,{PROBE_PNG_BASE64}");
        let body = ChatRequest {
            model: &inner.model,
            messages: vec![Message {
                role: "user",
                content: MessageContent::Parts(vec![
                    ContentPart::Text {
                        text: "Describe this image in one word.".to_owned(),
                    },
                    ContentPart::ImageUrl {
                        image_url: ImageUrl { url: data_uri },
                    },
                ]),
            }],
            temperature: 0.0,
            max_tokens: 1,
            stream: None,
        };

        info!("probing model vision support...");
        let mut req = inner.client.post(&inner.endpoint).json(&body);
        if let Some(key) = &inner.api_key {
            req = req.bearer_auth(key);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let supported = (200..300).contains(&status);
                info!("model vision support: {supported} (HTTP {status})");
                let _ = inner.supports_vision.set(supported);
                supported
            }
            Err(e) => {
                warn!("vision probe failed (will retry): {e}");
                false
            }
        }
    }

    /// Build user message content: multimodal parts if images should be included,
    /// otherwise plain text.
    fn build_user_content<'a>(
        content: &'a ClipboardContent,
        mode: ProcessMode,
        vision: bool,
    ) -> MessageContent<'a> {
        let use_images =
            mode == ProcessMode::Summarize && vision && content.has_images();

        let text = content.text.as_deref().unwrap_or("");

        if !use_images {
            return MessageContent::Text(text);
        }

        let mut parts = Vec::with_capacity(1 + content.images.len());
        if !text.is_empty() {
            parts.push(ContentPart::Text {
                text: text.to_owned(),
            });
        }
        for png_bytes in &content.images {
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes.as_ref());
            parts.push(ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: format!("data:image/png;base64,{b64}"),
                },
            });
        }
        MessageContent::Parts(parts)
    }

    /// Send content to the vLLM server and return the raw response content.
    /// Think-block stripping is handled separately by `response::strip_think_blocks`.
    pub async fn complete(
        &self,
        content: &ClipboardContent,
        mode: ProcessMode,
    ) -> Result<String, ApiError> {
        let inner = &self.0;
        let vision = self.probe_vision().await;
        let sys_prompt = mode.system_prompt();
        let body = ChatRequest {
            model: &inner.model,
            messages: vec![
                Message {
                    role: "system",
                    content: MessageContent::Text(&sys_prompt),
                },
                Message {
                    role: "user",
                    content: Self::build_user_content(content, mode, vision),
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

        let resp_content = chat
            .choices
            .into_iter()
            .next()
            .ok_or(ApiError::EmptyResponse)?
            .message
            .content;

        if resp_content.is_empty() {
            return Err(ApiError::EmptyResponse);
        }

        info!("received response ({} chars)", resp_content.len());
        debug!("response content: {resp_content}");
        Ok(resp_content)
    }

    /// Start a streaming request. Returns the raw `reqwest::Response` whose body
    /// the caller reads via `chunk()` and feeds into [`SseParser`].
    pub async fn complete_stream(
        &self,
        content: &ClipboardContent,
        mode: ProcessMode,
    ) -> Result<reqwest::Response, ApiError> {
        let inner = &self.0;
        let vision = self.probe_vision().await;
        let sys_prompt = mode.system_prompt();
        let body = ChatRequest {
            model: &inner.model,
            messages: vec![
                Message {
                    role: "system",
                    content: MessageContent::Text(&sys_prompt),
                },
                Message {
                    role: "user",
                    content: Self::build_user_content(content, mode, vision),
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
