use std::env;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;
use tracing::{debug, info, warn};

use crate::{ApiError, ClipboardContent, ProcessMode, RephraseParams, ThinkingMode};

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
    #[serde(skip_serializing_if = "Option::is_none")]
    chat_template_kwargs: Option<ChatTemplateKwargs>,
}

#[derive(Serialize)]
struct ChatTemplateKwargs {
    enable_thinking: bool,
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

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: Delta,
}

#[derive(Deserialize)]
struct Delta {
    content: Option<String>,
}

/// Parsed SSE event from a streaming response.
#[derive(Debug, PartialEq)]
pub(crate) enum SseEvent {
    Content(String),
    Done,
}

/// Line-based SSE parser that buffers incomplete lines across chunks.
pub(crate) struct SseParser {
    /// Accumulates complete UTF-8 text lines waiting for newline processing.
    buffer: String,
    /// Carry-over bytes for incomplete multi-byte UTF-8 sequences at chunk boundaries.
    tail: Vec<u8>,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            tail: Vec::new(),
        }
    }

    /// Feed raw bytes from `reqwest::Response::chunk()` and return parsed events.
    ///
    /// Uses a byte carry-over buffer to handle multi-byte UTF-8 sequences that span
    /// chunk boundaries, avoiding the replacement-character corruption of `from_utf8_lossy`.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        // Prepend any incomplete UTF-8 tail from the previous call.
        let data: std::borrow::Cow<[u8]> = if self.tail.is_empty() {
            std::borrow::Cow::Borrowed(chunk)
        } else {
            let mut v = std::mem::take(&mut self.tail);
            v.extend_from_slice(chunk);
            std::borrow::Cow::Owned(v)
        };

        // Find the longest valid UTF-8 prefix and carry over the remainder.
        let (valid, remainder) = match std::str::from_utf8(&data) {
            Ok(s) => (s, &[][..]),
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                // Safety: valid_up_to is guaranteed to be a valid UTF-8 boundary.
                let s = unsafe { std::str::from_utf8_unchecked(&data[..valid_up_to]) };
                (s, &data[valid_up_to..])
            }
        };
        self.tail.extend_from_slice(remainder);
        self.buffer.push_str(valid);

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

/// How thinking mode is controlled for the current model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingControlMethod {
    /// Model supports `chat_template_kwargs: { enable_thinking }`.
    ChatTemplateKwargs,
    /// Model supports `/think` and `/no_think` tags in the system prompt.
    SystemPromptTag,
    /// Model does not support controllable thinking.
    Unsupported,
}

struct LlmClientInner {
    client: Client,
    streaming_client: Client,
    endpoint: String,
    model: String,
    api_key: Option<String>,
    supports_vision: OnceCell<bool>,
    thinking_control: OnceCell<ThinkingControlMethod>,
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

    // --- Additional SseParser edge cases ---

    // SSE spec allows \r\n line endings; trim_end_matches('\r') must strip the CR.
    #[test]
    fn sse_crlf_line_ending() {
        let mut p = SseParser::new();
        let events =
            p.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\r\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Content(s) if s == "hi"));
    }

    // Empty string content must not produce an event (guarded by !content.is_empty()).
    #[test]
    fn sse_empty_content_not_emitted() {
        let mut p = SseParser::new();
        let events =
            p.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n");
        assert!(events.is_empty());
    }

    // null content field deserializes as Option::None and must not produce an event.
    #[test]
    fn sse_null_content_not_emitted() {
        let mut p = SseParser::new();
        let events =
            p.feed(b"data: {\"choices\":[{\"delta\":{\"content\":null}}]}\n");
        assert!(events.is_empty());
    }

    // Malformed JSON in a data line must be silently ignored (no panic, no event).
    #[test]
    fn sse_bad_json_silently_ignored() {
        let mut p = SseParser::new();
        let events = p.feed(b"data: not-valid-json\n");
        assert!(events.is_empty());
    }

    // [DONE] does not halt parsing; subsequent data lines are still processed.
    #[test]
    fn sse_done_does_not_stop_subsequent_parsing() {
        let mut p = SseParser::new();
        let input = concat!(
            "data: [DONE]\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"after\"}}]}\n",
        );
        let events = p.feed(input.as_bytes());
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], SseEvent::Done));
        assert!(matches!(&events[1], SseEvent::Content(s) if s == "after"));
    }

    // Multi-byte UTF-8 sequence (e.g. CJK character, 3 bytes) split at a chunk boundary
    // must not produce replacement characters — the incomplete bytes are carried over.
    #[test]
    fn sse_utf8_split_across_chunks() {
        // "가" = 0xEA 0xB0 0x80 (3-byte UTF-8)
        // Split after first byte, then after second byte.
        let prefix = b"data: {\"choices\":[{\"delta\":{\"content\":\"\xEA";
        let middle = b"\xB0";
        let suffix = b"\x80\"}}]}\n";

        let mut p = SseParser::new();
        assert!(p.feed(prefix).is_empty());
        assert!(p.feed(middle).is_empty());
        let events = p.feed(suffix);
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Content(s) if s == "가"));
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
            thinking_control: OnceCell::new(),
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
            chat_template_kwargs: None,
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

    /// Probe whether the model supports controllable thinking mode.
    /// Tries `chat_template_kwargs` first, then falls back to system prompt tag.
    /// Result is cached in `OnceCell`. Network errors skip caching (retry next time).
    pub async fn probe_thinking(&self) -> ThinkingControlMethod {
        let inner = &self.0;
        if let Some(&cached) = inner.thinking_control.get() {
            return cached;
        }

        info!("probing model thinking support...");

        // Step 1: try chat_template_kwargs with enable_thinking=true
        let method = match self.probe_thinking_kwargs(inner).await {
            Some(method) => method,
            None => return ThinkingControlMethod::Unsupported, // network error, don't cache
        };

        info!("thinking control method: {method:?}");
        let _ = inner.thinking_control.set(method);
        method
    }

    /// Try `chat_template_kwargs: { enable_thinking: true }`.
    /// Returns `None` on network error (caller should not cache).
    async fn probe_thinking_kwargs(&self, inner: &LlmClientInner) -> Option<ThinkingControlMethod> {
        let body = ChatRequest {
            model: &inner.model,
            messages: vec![Message {
                role: "user",
                content: MessageContent::Text("Say hi."),
            }],
            temperature: 0.0,
            max_tokens: 128,
            stream: None,
            chat_template_kwargs: Some(ChatTemplateKwargs { enable_thinking: true }),
        };

        let mut req = inner.client.post(&inner.endpoint).json(&body);
        if let Some(key) = &inner.api_key {
            req = req.bearer_auth(key);
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                // HTTP 200 + kwargs accepted = model supports chat_template_kwargs.
                // Don't require <think> in the response — the model may skip thinking
                // for trivial prompts even with enable_thinking=true.
                Some(ThinkingControlMethod::ChatTemplateKwargs)
            }
            Ok(resp) if resp.status().is_server_error() => {
                // 5xx: transient server error — don't cache, retry next time.
                warn!(
                    "thinking kwargs probe got {} (will retry)",
                    resp.status().as_u16()
                );
                None
            }
            Ok(_) => {
                // 4xx: server rejected the kwargs field — try prompt tag fallback.
                self.probe_thinking_prompt_tag(inner).await
            }
            Err(e) => {
                warn!("thinking probe failed (will retry): {e}");
                None
            }
        }
    }

    /// Fallback: try `/think` tag in the system prompt.
    /// Returns `None` on network error.
    async fn probe_thinking_prompt_tag(
        &self,
        inner: &LlmClientInner,
    ) -> Option<ThinkingControlMethod> {
        let body = ChatRequest {
            model: &inner.model,
            messages: vec![
                Message {
                    role: "system",
                    content: MessageContent::Text("/think"),
                },
                Message {
                    role: "user",
                    content: MessageContent::Text("Say hi."),
                },
            ],
            temperature: 0.0,
            max_tokens: 128,
            stream: None,
            chat_template_kwargs: None,
        };

        let mut req = inner.client.post(&inner.endpoint).json(&body);
        if let Some(key) = &inner.api_key {
            req = req.bearer_auth(key);
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let text = resp.text().await.unwrap_or_default();
                if let Ok(chat) = serde_json::from_str::<ChatResponse>(&text) {
                    let content = chat
                        .choices
                        .first()
                        .map(|c| c.message.content.as_str())
                        .unwrap_or("");
                    if content.contains("<think>") {
                        Some(ThinkingControlMethod::SystemPromptTag)
                    } else {
                        Some(ThinkingControlMethod::Unsupported)
                    }
                } else {
                    Some(ThinkingControlMethod::Unsupported)
                }
            }
            Ok(resp) if resp.status().is_server_error() => {
                warn!(
                    "thinking prompt-tag probe got {} (will retry)",
                    resp.status().as_u16()
                );
                None
            }
            Ok(_) => Some(ThinkingControlMethod::Unsupported),
            Err(e) => {
                warn!("thinking prompt-tag probe failed (will retry): {e}");
                None
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

    /// Resolve thinking mode into API-level controls based on probe result.
    fn resolve_thinking(
        thinking_mode: ThinkingMode,
        control: ThinkingControlMethod,
    ) -> (Option<&'static str>, Option<ChatTemplateKwargs>) {
        match (thinking_mode, control) {
            (ThinkingMode::Default, _) | (_, ThinkingControlMethod::Unsupported) => (None, None),
            (ThinkingMode::ForceOn, ThinkingControlMethod::ChatTemplateKwargs) => {
                (None, Some(ChatTemplateKwargs { enable_thinking: true }))
            }
            (ThinkingMode::ForceOff, ThinkingControlMethod::ChatTemplateKwargs) => {
                (None, Some(ChatTemplateKwargs { enable_thinking: false }))
            }
            (ThinkingMode::ForceOn, ThinkingControlMethod::SystemPromptTag) => {
                (Some("/think\n"), None)
            }
            (ThinkingMode::ForceOff, ThinkingControlMethod::SystemPromptTag) => {
                (Some("/no_think\n"), None)
            }
        }
    }

    /// Build and send a chat completion request. Probes vision and thinking support,
    /// constructs the request body, applies auth, and returns the raw response.
    /// `stream=true` uses the no-timeout streaming client; `false` uses the regular client.
    async fn build_and_send(
        &self,
        content: &ClipboardContent,
        mode: ProcessMode,
        rephrase_params: RephraseParams,
        thinking_mode: ThinkingMode,
        stream: bool,
    ) -> Result<reqwest::Response, ApiError> {
        let inner = &self.0;
        let vision = self.probe_vision().await;
        let thinking_control = self.probe_thinking().await;
        let (sys_prefix, template_kwargs) =
            Self::resolve_thinking(thinking_mode, thinking_control);

        let base_prompt = mode.system_prompt(rephrase_params);
        let sys_prompt = if let Some(prefix) = sys_prefix {
            format!("{prefix}{base_prompt}")
        } else {
            base_prompt
        };

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
            stream: if stream { Some(true) } else { None },
            chat_template_kwargs: template_kwargs,
        };

        let client = if stream { &inner.streaming_client } else { &inner.client };
        let mut req = client.post(&inner.endpoint).json(&body);
        if let Some(key) = &inner.api_key {
            req = req.bearer_auth(key);
        }
        Ok(req.send().await?.error_for_status()?)
    }

    /// Send content to the vLLM server and return the raw response content.
    /// Think-block stripping is handled separately by `response::strip_think_blocks`.
    pub async fn complete(
        &self,
        content: &ClipboardContent,
        mode: ProcessMode,
        rephrase_params: RephraseParams,
        thinking_mode: ThinkingMode,
    ) -> Result<String, ApiError> {
        let inner = &self.0;
        info!("sending request to {}", inner.endpoint);
        debug!("model={}, temperature={}, max_tokens={}", inner.model, TEMPERATURE, MAX_TOKENS);

        let resp = self
            .build_and_send(content, mode, rephrase_params, thinking_mode, false)
            .await?;
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
        rephrase_params: RephraseParams,
        thinking_mode: ThinkingMode,
    ) -> Result<reqwest::Response, ApiError> {
        let inner = &self.0;
        info!("sending streaming request to {}", inner.endpoint);
        self.build_and_send(content, mode, rephrase_params, thinking_mode, true)
            .await
    }
}
