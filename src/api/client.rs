use std::env;
use std::sync::Arc;
use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;
use tracing::{debug, info, trace, warn};

use crate::{ApiError, ClipboardContent, DebugCapture, ProcessMode, RephraseParams, ThinkingMode};

// Defaults — overridable via environment variables.
const CHAT_COMPLETIONS_PATH: &str = "/chat/completions";
const TEMPERATURE: f64 = 0.1;
const MAX_TOKENS: u32 = 16384;
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Streaming only: max wait for response headers before the attempt is retried.
/// Non-streaming responses arrive only after full generation, so they are
/// bounded by the regular client's total timeout instead.
const INITIAL_RESPONSE_TIMEOUT_SECS: u64 = 10;
/// Transient-failure and rate-limit retries per request (total attempts =
/// MAX_RETRIES + 1). The same budget covers both: a connect/timeout/5xx
/// failure retries after `RETRY_DELAY`, an HTTP 429 retries after the
/// server's `Retry-After` hint (see [`parse_retry_after`]).
const MAX_RETRIES: u32 = 1;
const RETRY_DELAY: Duration = Duration::from_millis(500);
/// Maximum wait honored from a 429 `Retry-After` header before the single
/// automatic retry of the main completion request.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(15);
/// Wait used for the 429 retry when the response has no `Retry-After` header,
/// or the header isn't in the integer-seconds form this parses (the
/// HTTP-date form is intentionally not supported).
const RETRY_AFTER_DEFAULT: Duration = Duration::from_secs(2);
/// Dynamic-budget mode: floor for the computed output budget (never request
/// fewer than this many completion tokens, even for a near-full prompt).
const MIN_OUTPUT_TOKENS: u32 = 512;
/// Dynamic-budget mode: tokens held back from the budget for tokenization
/// variance and per-message/JSON framing the estimate does not count.
const BUDGET_MARGIN: u32 = 256;

/// Conservative token estimate used only for `token_budget` clamping. It
/// deliberately OVER-estimates so the computed `max_tokens` never pushes
/// `prompt + max_tokens` past the budget (which would be rejected): Hangul
/// counts as ~1 token/char (qwen/most BPEs are denser, so this is safe) and
/// other non-whitespace as ~1 token per 3 chars. Whitespace is ignored.
fn estimate_prompt_tokens(text: &str) -> u32 {
    let mut hangul: u32 = 0;
    let mut other: u32 = 0;
    for c in text.chars() {
        if ('\u{AC00}'..='\u{D7A3}').contains(&c) || ('\u{1100}'..='\u{11FF}').contains(&c) {
            hangul += 1;
        } else if !c.is_whitespace() {
            other += 1;
        }
    }
    hangul + other / 3
}

/// Compute the effective `max_tokens` for a request. Without a budget this is
/// just the configured ceiling; with one, it is `budget - prompt_est - margin`
/// clamped into `[min(MIN_OUTPUT_TOKENS, ceiling), ceiling]`.
fn effective_max_tokens(ceiling: u32, budget: Option<u32>, prompt_est: u32) -> u32 {
    match budget {
        None => ceiling,
        Some(budget) => {
            let avail = budget.saturating_sub(prompt_est).saturating_sub(BUDGET_MARGIN);
            avail.clamp(MIN_OUTPUT_TOKENS.min(ceiling), ceiling)
        }
    }
}

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
    /// Token accounting for the request, when the server includes it. Present
    /// on essentially all OpenAI-compatible non-streaming responses.
    pub usage: Option<Usage>,
}

/// Token usage reported by the server for a completed request:
/// `prompt_tokens` / `completion_tokens` / `total_tokens`. Parsed
/// opportunistically wherever a server includes it — we do not send
/// `stream_options: { include_usage: true }` on streaming requests (compat
/// risk with some servers), so streaming usage is only available when a
/// server attaches it unprompted, typically on the final chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Deserialize)]
pub(crate) struct Choice {
    pub message: ResponseMessage,
    /// `"stop"` on normal completion, `"length"` when generation hit
    /// max_tokens — the reply is truncated and must not pass as success.
    pub finish_reason: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ResponseMessage {
    pub content: String,
}

// -- SSE streaming types (used by worker in streaming loop) --

#[derive(Deserialize)]
struct StreamChunk {
    /// Some servers send a final usage-only chunk with an empty or absent
    /// `choices` array, so this must not be required.
    #[serde(default)]
    choices: Vec<StreamChoice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: Delta,
    /// Set on the final content chunk (before `[DONE]`); `"length"` means
    /// generation was cut off by max_tokens.
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct Delta {
    content: Option<String>,
    /// Reasoning/thinking tokens delivered in a separate field by servers that
    /// run a reasoning parser, rather than inline `<think>` tags inside
    /// `content`. The field name varies by provider: vLLM, DeepSeek, and
    /// llama.cpp use `reasoning_content`; Ollama and OpenRouter use `reasoning`
    /// (an exact alias). Accept both.
    #[serde(alias = "reasoning")]
    reasoning_content: Option<String>,
}

/// Parsed SSE event from a streaming response.
#[derive(Debug, PartialEq)]
pub(crate) enum SseEvent {
    Content(String),
    /// A reasoning/thinking token from a server-side reasoning parser
    /// (separate `reasoning_content` field). Precedes the answer's `Content`.
    Reasoning(String),
    /// The server reported why generation stopped (e.g. "stop", "length").
    Finish(String),
    /// Token usage, when a chunk carries it (some servers attach it to the
    /// final chunk even without `stream_options.include_usage`).
    Usage(Usage),
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

        // Decode into the line buffer. An *incomplete* sequence at the end of
        // the chunk (a multi-byte char split across chunks) is carried over to
        // the next feed; *invalid* bytes in the middle are replaced with
        // U+FFFD and decoding continues — otherwise a single corrupt byte
        // would pin `valid_up_to` at 0 forever, absorbing all later chunks
        // into the tail and silently stalling the stream.
        let mut rest: &[u8] = &data;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    self.buffer.push_str(s);
                    break;
                }
                Err(e) => {
                    let valid_up_to = e.valid_up_to();
                    // Safety: valid_up_to is guaranteed to be a valid UTF-8 boundary.
                    let s = unsafe { std::str::from_utf8_unchecked(&rest[..valid_up_to]) };
                    self.buffer.push_str(s);
                    match e.error_len() {
                        // Invalid bytes mid-data: replace and keep decoding.
                        Some(n) => {
                            self.buffer.push(char::REPLACEMENT_CHARACTER);
                            rest = &rest[valid_up_to + n..];
                        }
                        // Chunk ends mid-sequence: carry over to the next feed.
                        None => {
                            self.tail.extend_from_slice(&rest[valid_up_to..]);
                            break;
                        }
                    }
                }
            }
        }

        let mut events = Vec::new();
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..pos].trim_end_matches('\r').to_string();
            self.buffer = self.buffer[pos + 1..].to_string();

            // SSE spec: a field line is `field:value` with an OPTIONAL single
            // space after the colon ("data: x" and "data:x" are equivalent).
            // Accepting only "data: " would silently drop every event if a
            // server or proxy emits the no-space form.
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.strip_prefix(' ').unwrap_or(data);

            if data == "[DONE]" {
                events.push(SseEvent::Done);
                continue;
            }

            if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                if let Some(choice) = chunk.choices.first() {
                    // Reasoning tokens arrive before the answer; emit first so
                    // they wrap correctly as a leading think block downstream.
                    if let Some(reasoning) = &choice.delta.reasoning_content
                        && !reasoning.is_empty()
                    {
                        events.push(SseEvent::Reasoning(reasoning.clone()));
                    }
                    if let Some(content) = &choice.delta.content
                        && !content.is_empty()
                    {
                        events.push(SseEvent::Content(content.clone()));
                    }
                    if let Some(reason) = &choice.finish_reason {
                        events.push(SseEvent::Finish(reason.clone()));
                    }
                }
                // Usage may arrive on a chunk with empty/no choices (a
                // dedicated final usage chunk) as well as alongside content.
                if let Some(usage) = chunk.usage {
                    events.push(SseEvent::Usage(usage));
                }
            }
        }

        events
    }

    /// Process a final line left in the buffer without a trailing newline.
    /// Call once at end-of-stream: a server may send a last `data: [DONE]` or
    /// finish chunk and then close the connection without the terminating
    /// `\n`, leaving that line unparsed by [`feed`] (which is newline-driven).
    pub fn flush(&mut self) -> Vec<SseEvent> {
        // A leftover tail at end-of-stream is a truncated multi-byte char that
        // can never complete. Discard it: any line needing those bytes is
        // unparseable regardless, while appending a replacement char would
        // corrupt an otherwise-parseable final line (e.g. `data: [DONE]`).
        self.tail.clear();
        if self.buffer.is_empty() {
            return Vec::new();
        }
        // Append the missing newline so the line-based logic in `feed` runs.
        self.buffer.push('\n');
        self.feed(&[])
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
    custom_headers: Vec<(String, String)>,
    temperature: f64,
    max_tokens: u32,
    token_budget: Option<u32>,
    initial_response_timeout: Duration,
    supports_vision: OnceCell<bool>,
    thinking_control: OnceCell<ThinkingControlMethod>,
}

/// Minimal 1x1 transparent PNG for vision probe (67 bytes).
const PROBE_PNG_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// Decide whether a vision-probe HTTP status yields a cacheable verdict.
/// `Some(true)` = supported, `Some(false)` = unsupported and cache it — covers
/// both a 400/422 image-payload rejection and a 429 rate limit (the latter to
/// stop re-probing a rate-limited endpoint, #63). `None` = inconclusive
/// (401/403/404/5xx): do not cache, so the next request re-probes.
fn classify_vision_status(status: u16) -> Option<bool> {
    match status {
        200..=299 => Some(true),
        400 | 422 | 429 => Some(false),
        _ => None,
    }
}

#[derive(Clone)]
pub struct LlmClient(Arc<LlmClientInner>);

impl LlmClientInner {
    /// Apply authentication headers (Bearer token and custom headers) to a request.
    fn apply_auth(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        for (name, value) in &self.custom_headers {
            req = req.header(name, value);
        }
        req
    }
}

/// Resolves a REQUIRED setting by precedence: env var > config file. An empty
/// string from either source is treated as unset. Returns `MissingConfig(name)`
/// when neither provides a value, so the caller fails fast at startup instead of
/// falling back to a guessed default.
fn require_setting(
    env_value: Option<String>,
    config_value: Option<&str>,
    name: &'static str,
) -> Result<String, ApiError> {
    env_value
        .filter(|s| !s.is_empty())
        .or_else(|| config_value.filter(|s| !s.is_empty()).map(str::to_owned))
        .ok_or(ApiError::MissingConfig(name))
}

/// Whether a request failure is transient and worth a retry: connect errors,
/// timeouts (total or initial-response), and HTTP 5xx.
fn is_transient_error(e: &ApiError) -> bool {
    match e {
        ApiError::InitialResponseTimeout(_) => true,
        ApiError::Http(e) => {
            e.is_connect() || e.is_timeout() || e.status().is_some_and(|s| s.is_server_error())
        }
        _ => false,
    }
}

/// Whether a request failure is an HTTP 429 (rate limited) on the main
/// completion request. Handled separately from `is_transient_error`: its
/// retry delay honors the server's `Retry-After` header instead of the fixed
/// `RETRY_DELAY`. Deliberately not applied to the vision/thinking capability
/// probes (they have their own 429 caching behavior, #63) — those call
/// `req.send()` directly and never go through `send_request`/`build_and_send`.
fn is_rate_limited(e: &ApiError) -> bool {
    matches!(e, ApiError::Http(err) if err.status() == Some(reqwest::StatusCode::TOO_MANY_REQUESTS))
}

/// Parse a `Retry-After` header value into a wait duration, clamped to
/// `RETRY_AFTER_MAX`. Only the integer-seconds form (e.g. `"5"`) is
/// supported; the HTTP-date form and anything unparsable or absent fall back
/// to `RETRY_AFTER_DEFAULT`.
fn parse_retry_after(value: Option<&str>) -> Duration {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs).min(RETRY_AFTER_MAX))
        .unwrap_or(RETRY_AFTER_DEFAULT)
}

/// Parses the `CLIP_LLM_CUSTOM_HEADERS` env format: comma-separated `Key:Value`
/// pairs, optionally wrapped in quotes.
fn parse_custom_headers(raw: &str) -> Vec<(String, String)> {
    raw.trim_matches('"')
        .split(',')
        .filter_map(|pair| {
            let pair = pair.trim();
            if pair.is_empty() {
                return None;
            }
            let (key, value) = pair.split_once(':')?;
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Replace long inline `data:` URIs (base64 images) in a request JSON value with
/// a short placeholder, so the debug capture stays readable instead of being a
/// wall of base64. Recurses through arrays and objects.
fn sanitize_debug_json(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) if s.starts_with("data:") && s.len() > 128 => {
            *s = format!("[inline data elided, {} bytes]", s.len());
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(sanitize_debug_json),
        serde_json::Value::Object(map) => map.values_mut().for_each(sanitize_debug_json),
        _ => {}
    }
}

impl LlmClient {
    pub fn new() -> Result<Self, ApiError> {
        // Precedence for every setting: env var > config file > built-in default.
        let config = crate::config::get();

        // endpoint, model, and api_key are REQUIRED with no built-in default:
        // each is deployment-specific (an internal vLLM server, its model name,
        // and its access token), so a guessed fallback would silently mislead.
        // When any is unset, fail fast so the app logs a clear error at startup
        // and exits instead of talking to the wrong server.
        let base = require_setting(
            env::var("CLIP_LLM_API_ENDPOINT").ok(),
            config.api_endpoint(),
            "api.endpoint",
        )?;
        let endpoint = format!("{}{}", base.trim_end_matches('/'), CHAT_COMPLETIONS_PATH);
        let model = require_setting(
            env::var("CLIP_LLM_MODEL").ok(),
            config.api_model(),
            "api.model",
        )?;
        let api_key = Some(require_setting(
            env::var("CLIP_LLM_API_KEY").ok(),
            config.api_key(),
            "api.api_key",
        )?);
        // Empty env var = unset, so it does not silently suppress configured headers.
        let custom_headers: Vec<(String, String)> =
            match env::var("CLIP_LLM_CUSTOM_HEADERS").ok().filter(|s| !s.is_empty()) {
                Some(raw) => parse_custom_headers(&raw),
                None => config
                    .api_headers()
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            };

        // Generation parameters: config file > built-in default (no env var).
        let temperature = config.generation_temperature().unwrap_or(TEMPERATURE);
        let max_tokens = config.generation_max_tokens().unwrap_or(MAX_TOKENS);
        let token_budget = config.generation_token_budget();
        let timeout = Duration::from_secs(
            config
                .generation_request_timeout_secs()
                .unwrap_or(REQUEST_TIMEOUT_SECS),
        );
        let initial_response_timeout = Duration::from_secs(
            config
                .generation_initial_response_timeout_secs()
                .unwrap_or(INITIAL_RESPONSE_TIMEOUT_SECS),
        );

        // The endpoint URL stays at debug: it may carry embedded credentials in
        // some gateway setups, and info-level records ship to remote telemetry.
        debug!("endpoint={endpoint}");
        info!(
            "model={model}, api_key={}, custom_headers={}, temperature={temperature}, max_tokens={max_tokens}, timeout={}s, initial_response_timeout={}s",
            if api_key.is_some() { "set" } else { "unset" },
            if custom_headers.is_empty() {
                "none".to_string()
            } else {
                custom_headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(",")
            },
            timeout.as_secs(),
            initial_response_timeout.as_secs(),
        );

        let client = Client::builder().timeout(timeout).build()?;
        // Streaming client: connect timeout only, no total body timeout.
        //
        // pool_max_idle_per_host(0): never reuse a pooled keep-alive connection
        // for streaming. This daemon fires one request per hotkey press, so a
        // pooled connection sits idle between uses and the server/proxy often
        // closes it by its own keep-alive timeout. Reusing such a half-closed
        // connection makes the first request after an idle gap die mid-stream
        // (then a retry on a fresh connection succeeds) — a frequent Windows
        // symptom. A fresh TCP+TLS handshake per request is negligible here.
        let mut streaming_builder = Client::builder()
            .connect_timeout(timeout)
            .pool_max_idle_per_host(0);
        // Diagnostic escape hatch: force HTTP/1.1 for the streaming connection.
        // Some HTTP/2 proxies mishandle SSE and end the stream after the first
        // frame; CLIP_LLM_STREAM_HTTP1=1 isolates that case.
        if env::var("CLIP_LLM_STREAM_HTTP1").is_ok() {
            info!("streaming client forced to HTTP/1.1 (CLIP_LLM_STREAM_HTTP1)");
            streaming_builder = streaming_builder.http1_only();
        }
        let streaming_client = streaming_builder.build()?;
        Ok(Self(Arc::new(LlmClientInner {
            client,
            streaming_client,
            endpoint,
            model,
            api_key,
            custom_headers,
            temperature,
            max_tokens,
            token_budget,
            initial_response_timeout,
            supports_vision: OnceCell::new(),
            thinking_control: OnceCell::new(),
        })))
    }

    /// Probe whether the model supports vision by sending a tiny image request.
    /// The result is cached in `OnceCell` on a definitive verdict — a 2xx
    /// (supported) or a 400/422 rejection of the image payload (unsupported) —
    /// and also on 429, where the conservative `false` is cached to avoid a
    /// re-probe storm against a rate-limited endpoint (#63). Other transient or
    /// ambiguous responses (401/403/404/5xx) and network errors skip caching so
    /// the next request re-probes.
    #[tracing::instrument(skip(self))]
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
        if tracing::enabled!(tracing::Level::TRACE)
            && let Ok(json) = serde_json::to_string_pretty(&body)
        {
            trace!("vision probe request:\n{json}");
        }
        let req = inner.apply_auth(inner.client.post(&inner.endpoint).json(&body));

        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                match classify_vision_status(status) {
                    Some(true) => {
                        info!("model vision support: true (HTTP {status})");
                        let _ = inner.supports_vision.set(true);
                        true
                    }
                    Some(false) => {
                        // 400/422 = the server understood the multimodal request
                        // and rejected the image (definitive "no vision"). 429 =
                        // rate limited: capability is undeterminable now, and
                        // re-probing every request would amplify the limit (#63).
                        // Both cache the conservative `false` for the session so
                        // the hot path stops re-probing; a restart re-probes.
                        if status == 429 {
                            warn!("vision probe rate limited (HTTP 429); assuming no vision for this session");
                        } else {
                            info!("model vision support: false (HTTP {status})");
                        }
                        let _ = inner.supports_vision.set(false);
                        false
                    }
                    None => {
                        // Other transient or ambiguous (401/403/404/5xx): do NOT
                        // cache, so the next request re-probes. Treat as
                        // unsupported only for this request.
                        warn!("vision probe inconclusive (HTTP {status}); will retry");
                        false
                    }
                }
            }
            Err(e) => {
                warn!("vision probe failed (will retry): {e}");
                false
            }
        }
    }

    /// Probe whether the model supports controllable thinking mode.
    /// Tries `chat_template_kwargs` first, then falls back to system prompt tag.
    /// The result is cached in `OnceCell` on a definitive verdict, and also on
    /// 429, where the conservative `Unsupported` is cached to avoid a re-probe
    /// storm against a rate-limited endpoint (#63). Other transient or ambiguous
    /// responses (401/403/404/5xx) and network errors skip caching so the next
    /// request re-probes.
    #[tracing::instrument(skip(self))]
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

        if tracing::enabled!(tracing::Level::TRACE)
            && let Ok(json) = serde_json::to_string_pretty(&body)
        {
            trace!("thinking kwargs probe request:\n{json}");
        }
        let req = inner.apply_auth(inner.client.post(&inner.endpoint).json(&body));

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                trace!("thinking kwargs probe response: HTTP {}", resp.status().as_u16());
                // HTTP 200 + kwargs accepted = model supports chat_template_kwargs.
                // Don't require <think> in the response — the model may skip thinking
                // for trivial prompts even with enable_thinking=true.
                Some(ThinkingControlMethod::ChatTemplateKwargs)
            }
            Ok(resp) if resp.status().as_u16() == 400 || resp.status().as_u16() == 422 => {
                trace!("thinking kwargs probe rejected: HTTP {}", resp.status().as_u16());
                // Server understood but rejected the kwargs field — try the
                // system-prompt tag fallback to determine the real method.
                self.probe_thinking_prompt_tag(inner).await
            }
            Ok(resp) if resp.status().as_u16() == 429 => {
                // Rate limited: re-probing per request amplifies the limit (#63).
                // Cache the conservative default (no thinking control) for the
                // session via the Some(...) the caller stores; a restart re-probes
                // once quota recovers.
                warn!("thinking kwargs probe rate limited (HTTP 429); assuming no thinking control for this session");
                Some(ThinkingControlMethod::Unsupported)
            }
            Ok(resp) => {
                // Other transient or ambiguous (401/403/404/5xx): don't cache,
                // retry next time rather than permanently deciding the method.
                warn!(
                    "thinking kwargs probe inconclusive (HTTP {}); will retry",
                    resp.status().as_u16()
                );
                None
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

        if tracing::enabled!(tracing::Level::TRACE)
            && let Ok(json) = serde_json::to_string_pretty(&body)
        {
            trace!("thinking prompt-tag probe request:\n{json}");
        }
        let req = inner.apply_auth(inner.client.post(&inner.endpoint).json(&body));

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let text = resp.text().await.unwrap_or_default();
                trace!("thinking prompt-tag probe response:\n{text}");
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
            Ok(resp) if resp.status().as_u16() == 400 || resp.status().as_u16() == 422 => {
                // Server understood and rejected the /think system tag —
                // thinking is definitively uncontrollable. Worth caching.
                Some(ThinkingControlMethod::Unsupported)
            }
            Ok(resp) if resp.status().as_u16() == 429 => {
                // Rate limited: cache the conservative default for the session
                // rather than re-probing on every request (#63).
                warn!("thinking prompt-tag probe rate limited (HTTP 429); assuming no thinking control for this session");
                Some(ThinkingControlMethod::Unsupported)
            }
            Ok(resp) => {
                // Other transient or ambiguous (401/403/404/5xx): don't cache, retry.
                warn!(
                    "thinking prompt-tag probe inconclusive (HTTP {}); will retry",
                    resp.status().as_u16()
                );
                None
            }
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
        use_images: bool,
    ) -> MessageContent<'a> {
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
            (_, ThinkingControlMethod::Unsupported) => (None, None),
            (ThinkingMode::Think, ThinkingControlMethod::ChatTemplateKwargs) => {
                (None, Some(ChatTemplateKwargs { enable_thinking: true }))
            }
            (ThinkingMode::NoThink, ThinkingControlMethod::ChatTemplateKwargs) => {
                (None, Some(ChatTemplateKwargs { enable_thinking: false }))
            }
            (ThinkingMode::Think, ThinkingControlMethod::SystemPromptTag) => {
                (Some("/think\n"), None)
            }
            (ThinkingMode::NoThink, ThinkingControlMethod::SystemPromptTag) => {
                (Some("/no_think\n"), None)
            }
        }
    }

    /// Build and send a chat completion request. Probes vision and thinking support,
    /// constructs the request body, applies auth, and returns the raw response.
    /// `stream=true` uses the no-timeout streaming client; `false` uses the regular client.
    #[tracing::instrument(
        skip_all,
        fields(endpoint = %self.0.endpoint, model = %self.0.model, stream)
    )]
    async fn build_and_send(
        &self,
        content: &ClipboardContent,
        mode: ProcessMode,
        rephrase_params: RephraseParams,
        thinking_mode: ThinkingMode,
        stream: bool,
        capture: &mut DebugCapture,
    ) -> Result<reqwest::Response, ApiError> {
        let inner = &self.0;
        let vision = self.probe_vision().await;
        let thinking_control = self.probe_thinking().await;
        let (sys_prefix, template_kwargs) =
            Self::resolve_thinking(thinking_mode, thinking_control);

        let use_images =
            mode == ProcessMode::Summarize && vision && content.has_images();
        let image_only = use_images && !content.has_text();

        // Image-only clipboard but model lacks vision — nothing useful to send.
        if !content.has_text() && content.has_images() && !vision {
            return Err(ApiError::NoUsableContent);
        }

        let base_prompt = mode.system_prompt(rephrase_params, image_only);
        let sys_prompt = if let Some(prefix) = sys_prefix {
            format!("{prefix}{base_prompt}")
        } else {
            base_prompt
        };

        // With a token_budget, shrink max_tokens to keep (prompt + completion)
        // under it. Image inputs aren't text-estimable, so they keep the ceiling.
        let budget = if use_images { None } else { inner.token_budget };
        let prompt_est = budget.map_or(0, |_| {
            estimate_prompt_tokens(&sys_prompt)
                + estimate_prompt_tokens(content.text.as_deref().unwrap_or(""))
        });
        let max_tokens = effective_max_tokens(inner.max_tokens, budget, prompt_est);
        if budget.is_some() {
            debug!(
                "dynamic max_tokens={max_tokens} (budget={budget:?}, prompt_est={prompt_est}, ceiling={})",
                inner.max_tokens
            );
        }

        let body = ChatRequest {
            model: &inner.model,
            messages: vec![
                Message {
                    role: "system",
                    content: MessageContent::Text(&sys_prompt),
                },
                Message {
                    role: "user",
                    content: Self::build_user_content(content, use_images),
                },
            ],
            temperature: inner.temperature,
            max_tokens,
            stream: if stream { Some(true) } else { None },
            chat_template_kwargs: template_kwargs,
        };

        // Capture the final request for the debug view: serialize, then elide
        // base64 image payloads so the snapshot (and the DEBUG log below) stay
        // readable. Auth lives in headers, not the body, so no secret is stored.
        capture.endpoint = Some(inner.endpoint.clone());
        if let Ok(mut value) = serde_json::to_value(&body) {
            sanitize_debug_json(&mut value);
            capture.request = serde_json::to_string_pretty(&value).ok();
        }
        if let Some(req_json) = &capture.request {
            debug!("LLM request body:\n{req_json}");
        }
        let client = if stream { &inner.streaming_client } else { &inner.client };
        let mut req = inner.apply_auth(client.post(&inner.endpoint).json(&body));
        // The streaming client has no total timeout, so without this bound a
        // server that accepts the connection but never sends headers would
        // hang the request forever.
        let headers_timeout = stream.then_some(inner.initial_response_timeout);

        let mut retry_after: Option<Duration> = None;
        for attempt in 1..=MAX_RETRIES {
            // try_clone() fails only for stream bodies; JSON bodies always clone.
            let Some(retry_req) = req.try_clone() else { break };
            match Self::send_request(req, headers_timeout, capture, &mut retry_after).await {
                Err(e) if is_transient_error(&e) => {
                    warn!(
                        "transient request failure (attempt {attempt}/{}): {e}; retrying in {}ms",
                        MAX_RETRIES + 1,
                        RETRY_DELAY.as_millis(),
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                    req = retry_req;
                }
                Err(e) if is_rate_limited(&e) => {
                    let delay = retry_after.unwrap_or(RETRY_AFTER_DEFAULT);
                    warn!(
                        "rate limited (attempt {attempt}/{}): {e}; retrying in {}ms (Retry-After)",
                        MAX_RETRIES + 1,
                        delay.as_millis(),
                    );
                    tokio::time::sleep(delay).await;
                    req = retry_req;
                }
                result => return result,
            }
        }
        Self::send_request(req, headers_timeout, capture, &mut retry_after).await
    }

    /// Send a single request attempt. When `headers_timeout` is set, the wait
    /// for response headers is bounded. Records the response status into
    /// `capture`, and on a non-2xx rejection reads the server's error body
    /// (which `error_for_status` would otherwise discard) so the debug view can
    /// show why the request was refused. On a 429, also parses the
    /// `Retry-After` header into `retry_after` (overwriting/clearing any stale
    /// value from a prior attempt) so the caller's retry loop can honor it.
    async fn send_request(
        req: reqwest::RequestBuilder,
        headers_timeout: Option<Duration>,
        capture: &mut DebugCapture,
        retry_after: &mut Option<Duration>,
    ) -> Result<reqwest::Response, ApiError> {
        *retry_after = None;
        let resp = match headers_timeout {
            Some(t) => tokio::time::timeout(t, req.send())
                .await
                .map_err(|_| ApiError::InitialResponseTimeout(t.as_secs()))??,
            None => req.send().await?,
        };
        capture.status = Some(resp.status().as_u16());
        if let Err(status_err) = resp.error_for_status_ref() {
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let header = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok());
                *retry_after = Some(parse_retry_after(header));
            }
            // Consume the body for the error detail before dropping the response.
            capture.response_raw = resp.text().await.ok();
            return Err(ApiError::Http(status_err));
        }
        // Success: drop any error body captured on a prior (retried) attempt so
        // it cannot masquerade as this response's body. The real body is filled
        // by `complete` (non-streaming) or the streaming finalize.
        capture.response_raw = None;
        Ok(resp)
    }

    /// Send content to the vLLM server and return the raw response content.
    /// Think-block stripping is handled separately by `response::strip_think_blocks`.
    pub async fn complete(
        &self,
        content: &ClipboardContent,
        mode: ProcessMode,
        rephrase_params: RephraseParams,
        thinking_mode: ThinkingMode,
        capture: &mut DebugCapture,
    ) -> Result<String, ApiError> {
        let inner = &self.0;
        // Debug, not info: the endpoint URL may carry embedded credentials in
        // some gateway setups, and info-level records ship to remote telemetry.
        debug!("sending request to {}", inner.endpoint);
        debug!("model={}, temperature={}, max_tokens={}", inner.model, inner.temperature, inner.max_tokens);

        let resp = self
            .build_and_send(content, mode, rephrase_params, thinking_mode, false, capture)
            .await?;
        let text = resp.text().await?;
        // Record the raw body before parsing, so even a parse failure exposes it.
        capture.response_raw = Some(text.clone());
        if tracing::enabled!(tracing::Level::DEBUG) {
            if let Ok(pretty) = serde_json::from_str::<serde_json::Value>(&text) {
                debug!(
                    "LLM response:\n{}",
                    serde_json::to_string_pretty(&pretty).unwrap_or_default()
                );
            } else {
                debug!("LLM response (raw):\n{text}");
            }
        }
        // A 200 with a non-completion body (proxy error page, truncated body)
        // is not "the model returned nothing" — surface the parse failure so
        // the real cause isn't masked. The raw body is already in `capture`.
        let chat: ChatResponse =
            serde_json::from_str(&text).map_err(|e| ApiError::MalformedResponse(e.to_string()))?;

        if let Some(usage) = chat.usage {
            capture.prompt_tokens = Some(usage.prompt_tokens);
            capture.completion_tokens = Some(usage.completion_tokens);
            capture.total_tokens = Some(usage.total_tokens);
            info!(
                prompt_tokens = usage.prompt_tokens,
                completion_tokens = usage.completion_tokens,
                total_tokens = usage.total_tokens,
                mode = mode.label(),
                "token usage"
            );
        }

        let choice = chat
            .choices
            .into_iter()
            .next()
            .ok_or(ApiError::EmptyResponse)?;
        if choice.finish_reason.as_deref() == Some("length") {
            return Err(ApiError::Truncated);
        }
        let resp_content = choice.message.content;

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
        capture: &mut DebugCapture,
    ) -> Result<reqwest::Response, ApiError> {
        let inner = &self.0;
        // Debug, not info — see `complete` for the endpoint-in-telemetry rationale.
        debug!("sending streaming request to {}", inner.endpoint);
        self.build_and_send(content, mode, rephrase_params, thinking_mode, true, capture)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_setting_precedence_and_missing() {
        // env var wins over config.
        assert_eq!(
            require_setting(Some("env".to_string()), Some("cfg"), "x").unwrap(),
            "env"
        );
        // config wins when env is absent.
        assert_eq!(require_setting(None, Some("cfg"), "x").unwrap(), "cfg");
        // an empty env string is treated as unset and falls through to config.
        assert_eq!(
            require_setting(Some(String::new()), Some("cfg"), "x").unwrap(),
            "cfg"
        );
        // neither set (or both empty) -> MissingConfig carrying the field name.
        assert!(matches!(
            require_setting(None, None, "api.endpoint"),
            Err(ApiError::MissingConfig("api.endpoint"))
        ));
        assert!(matches!(
            require_setting(Some(String::new()), Some(""), "api.model"),
            Err(ApiError::MissingConfig("api.model"))
        ));
    }

    #[test]
    fn estimate_prompt_tokens_overestimates() {
        // Whitespace is free; Hangul counts ~1/char, other non-space ~1/3 chars.
        assert_eq!(estimate_prompt_tokens(""), 0);
        assert_eq!(estimate_prompt_tokens("   \n\t "), 0);
        // 6 latin letters -> 6/3 = 2.
        assert_eq!(estimate_prompt_tokens("abcdef"), 2);
        // 3 Hangul syllables -> 3.
        assert_eq!(estimate_prompt_tokens("가나다"), 3);
        // Mixed: 3 Hangul + "abc" (3/3=1) = 4; the space is ignored.
        assert_eq!(estimate_prompt_tokens("가나다 abc"), 4);
    }

    #[test]
    fn effective_max_tokens_clamps_to_budget() {
        // No budget -> ceiling unchanged.
        assert_eq!(effective_max_tokens(8000, None, 5000), 8000);
        // Budget with small prompt -> budget - prompt - margin, under the ceiling.
        // 6000 - 300 - 256 = 5444.
        assert_eq!(effective_max_tokens(40960, Some(6000), 300), 5444);
        // Ceiling caps the result when the budget would allow more.
        assert_eq!(effective_max_tokens(4000, Some(6000), 300), 4000);
        // A near-full prompt floors at MIN_OUTPUT_TOKENS rather than going to 0.
        assert_eq!(effective_max_tokens(40960, Some(6000), 5900), MIN_OUTPUT_TOKENS);
        // Floor is itself capped by a tiny ceiling.
        assert_eq!(effective_max_tokens(128, Some(6000), 5900), 128);
    }

    #[test]
    fn is_transient_error_classification() {
        // Initial-response timeout is transient (worth a retry).
        assert!(is_transient_error(&ApiError::InitialResponseTimeout(10)));
        // Permanent failures must not be retried.
        assert!(!is_transient_error(&ApiError::EmptyResponse));
        assert!(!is_transient_error(&ApiError::NoUsableContent));
        assert!(!is_transient_error(&ApiError::Cancelled));
    }

    #[test]
    fn parse_retry_after_integer_seconds() {
        assert_eq!(parse_retry_after(Some("5")), Duration::from_secs(5));
        assert_eq!(parse_retry_after(Some("0")), Duration::from_secs(0));
        // Leading/trailing whitespace is tolerated.
        assert_eq!(parse_retry_after(Some(" 3 ")), Duration::from_secs(3));
    }

    #[test]
    fn parse_retry_after_clamps_to_max() {
        assert_eq!(parse_retry_after(Some("60")), RETRY_AFTER_MAX);
        assert_eq!(parse_retry_after(Some("15")), RETRY_AFTER_MAX);
        // Just under the cap is left unclamped.
        assert_eq!(parse_retry_after(Some("14")), Duration::from_secs(14));
    }

    #[test]
    fn parse_retry_after_missing_defaults() {
        assert_eq!(parse_retry_after(None), RETRY_AFTER_DEFAULT);
    }

    #[test]
    fn parse_retry_after_http_date_form_defaults() {
        // The HTTP-date form is intentionally not parsed.
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            RETRY_AFTER_DEFAULT
        );
    }

    #[test]
    fn parse_retry_after_garbage_defaults() {
        assert_eq!(parse_retry_after(Some("not-a-number")), RETRY_AFTER_DEFAULT);
        assert_eq!(parse_retry_after(Some("")), RETRY_AFTER_DEFAULT);
        // Negative numbers are not valid u64 and fall back to the default.
        assert_eq!(parse_retry_after(Some("-5")), RETRY_AFTER_DEFAULT);
    }

    #[test]
    fn is_rate_limited_classification() {
        // Non-Http errors are never rate-limit errors.
        assert!(!is_rate_limited(&ApiError::EmptyResponse));
        assert!(!is_rate_limited(&ApiError::Cancelled));
        assert!(!is_rate_limited(&ApiError::InitialResponseTimeout(10)));
    }

    #[test]
    fn parse_custom_headers_pairs() {
        let parsed = parse_custom_headers("\"X-A:1, X-B:2\"");
        assert_eq!(
            parsed,
            vec![
                ("X-A".to_string(), "1".to_string()),
                ("X-B".to_string(), "2".to_string()),
            ]
        );
        assert!(parse_custom_headers("").is_empty());
    }

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

    #[test]
    fn parse_finish_reason_length() {
        let json = r#"{"choices":[{"message":{"content":"cut off"},"finish_reason":"length"}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn parse_response_usage_present() {
        let json = r#"{"choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.expect("usage must be parsed");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn parse_response_usage_absent() {
        let json = r#"{"choices":[{"message":{"content":"hi"}}]}"#;
        let resp: ChatResponse = serde_json::from_str(json).unwrap();
        assert!(resp.usage.is_none());
    }

    #[test]
    fn classify_vision_status_caches_429() {
        // #63: 429 must be a cacheable `false` so the hot path stops re-probing
        // a rate-limited endpoint instead of re-firing the probe burst.
        assert_eq!(classify_vision_status(429), Some(false));
        // Definitive verdicts cache as before.
        assert_eq!(classify_vision_status(200), Some(true));
        assert_eq!(classify_vision_status(204), Some(true));
        assert_eq!(classify_vision_status(400), Some(false));
        assert_eq!(classify_vision_status(422), Some(false));
        // Genuinely transient / config errors stay uncached (re-probe).
        assert_eq!(classify_vision_status(401), None);
        assert_eq!(classify_vision_status(403), None);
        assert_eq!(classify_vision_status(404), None);
        assert_eq!(classify_vision_status(500), None);
        assert_eq!(classify_vision_status(503), None);
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
    fn sse_reasoning_event() {
        // Qwen3/vLLM reasoning parser: thinking arrives in `reasoning_content`.
        let mut p = SseParser::new();
        let events =
            p.feed(b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"}}]}\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Reasoning(s) if s == "thinking"));
    }

    #[test]
    fn sse_reasoning_alias_field_parsed() {
        // Ollama and OpenRouter deliver reasoning under `reasoning`, an exact
        // alias of vLLM/DeepSeek's `reasoning_content`.
        let mut p = SseParser::new();
        let events =
            p.feed(b"data: {\"choices\":[{\"delta\":{\"reasoning\":\"hmm\"}}]}\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Reasoning(s) if s == "hmm"));
    }

    #[test]
    fn sse_reasoning_empty_not_emitted() {
        let mut p = SseParser::new();
        let events =
            p.feed(b"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"\"}}]}\n");
        assert!(events.is_empty());
    }

    #[test]
    fn sse_done_event() {
        let mut p = SseParser::new();
        let events = p.feed(b"data: [DONE]\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Done));
    }

    #[test]
    fn sse_finish_reason_emitted() {
        // Final OpenAI-compatible chunk: empty delta + finish_reason.
        let mut p = SseParser::new();
        let events = p.feed(
            br#"data: {"choices":[{"delta":{},"finish_reason":"length"}]}"#.as_ref(),
        );
        assert!(events.is_empty()); // incomplete line
        let events = p.feed(b"\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Finish(r) if r == "length"));
    }

    #[test]
    fn sse_usage_only_chunk_with_no_choices() {
        // Some servers send a dedicated final chunk carrying only `usage`,
        // with an empty (or absent) `choices` array.
        let mut p = SseParser::new();
        let events = p.feed(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n",
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            SseEvent::Usage(usage) => {
                assert_eq!(usage.prompt_tokens, 10);
                assert_eq!(usage.completion_tokens, 5);
                assert_eq!(usage.total_tokens, 15);
            }
            other => panic!("expected Usage, got {other:?}"),
        }
    }

    #[test]
    fn sse_usage_alongside_content_and_finish() {
        // Some servers attach usage to the same chunk as the final content
        // and finish_reason instead of a separate trailing chunk.
        let mut p = SseParser::new();
        let events = p.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"end\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n",
        );
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], SseEvent::Content(s) if s == "end"));
        assert!(matches!(&events[1], SseEvent::Finish(r) if r == "stop"));
        assert!(matches!(&events[2], SseEvent::Usage(u) if u.total_tokens == 3));
    }

    #[test]
    fn sse_chunk_without_usage_field_emits_no_usage_event() {
        let mut p = SseParser::new();
        let events =
            p.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n");
        assert_eq!(events.len(), 1);
        assert!(!events.iter().any(|e| matches!(e, SseEvent::Usage(_))));
    }

    #[test]
    fn sse_content_and_finish_reason_in_one_chunk() {
        // Some servers attach finish_reason to the last content chunk.
        let mut p = SseParser::new();
        let events = p.feed(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"end\"},\"finish_reason\":\"stop\"}]}\n",
        );
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], SseEvent::Content(s) if s == "end"));
        assert!(matches!(&events[1], SseEvent::Finish(r) if r == "stop"));
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

    // SSE allows `data:` with no space after the colon; events must still parse.
    #[test]
    fn sse_data_without_space_parsed() {
        let mut p = SseParser::new();
        let events = p.feed(b"data:{\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Content(s) if s == "x"));
        // [DONE] terminator without the space is also recognized.
        let events = p.feed(b"data:[DONE]\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Done));
    }

    // Only the FIRST space after the colon is the optional separator; a second
    // leading space is part of the data value.
    #[test]
    fn sse_data_preserves_second_leading_space() {
        let mut p = SseParser::new();
        // Two spaces after the colon -> value retains one leading space, so this
        // is not the bare "[DONE]" token and parses as (failed) JSON -> ignored.
        let events = p.feed(b"data:  [DONE]\n");
        assert!(events.is_empty());
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

    // A genuinely invalid byte (not a chunk-split sequence) must not stall the
    // parser: it is replaced with U+FFFD and later events still parse.
    #[test]
    fn sse_invalid_byte_does_not_stall_stream() {
        let mut p = SseParser::new();
        // 0xFF is never valid UTF-8; it corrupts this line's JSON.
        let corrupt = b"data: {\"choices\":[{\"delta\":{\"content\":\"a\xFFb\"}}]}\n";
        let events = p.feed(corrupt);
        // The corrupted line still decodes (with U+FFFD) and parses as JSON.
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Content(s) if s == "a\u{FFFD}b"));
        // Subsequent well-formed events must keep flowing.
        let events = p.feed(b"data: [DONE]\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Done));
    }

    // A truncated multi-byte char at end-of-stream must not corrupt the final
    // (newline-less) line: flush() discards the unfinishable tail bytes and
    // still recovers the buffered line.
    #[test]
    fn sse_flush_discards_incomplete_utf8_tail() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: [DONE]").is_empty()); // no newline yet
        assert!(p.feed(b"\xEA").is_empty()); // first byte of a 3-byte char, never completed
        let events = p.flush();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Done));
    }

    // flush() recovers a final line the server sent without a trailing newline.
    #[test]
    fn sse_flush_recovers_trailing_done_without_newline() {
        let mut p = SseParser::new();
        assert!(p.feed(b"data: [DONE]").is_empty()); // no newline yet
        let events = p.flush();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Done));
    }

    #[test]
    fn sse_flush_recovers_trailing_finish_without_newline() {
        let mut p = SseParser::new();
        assert!(
            p.feed(br#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#.as_ref())
                .is_empty()
        );
        let events = p.flush();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], SseEvent::Finish(r) if r == "stop"));
    }

    #[test]
    fn sse_flush_empty_buffer_is_noop() {
        let mut p = SseParser::new();
        assert!(p.flush().is_empty());
        // After a clean newline-terminated feed, nothing is left to flush.
        let _ = p.feed(b"data: [DONE]\n");
        assert!(p.flush().is_empty());
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
        let mc = LlmClient::build_user_content(&content, false);
        assert_eq!(mc, MessageContent::Text("hello"));
    }

    #[test]
    fn build_user_content_with_image_summarize() {
        let content = ClipboardContent {
            text: Some("caption".into()),
            images: vec![Arc::new(vec![0x89, 0x50])],
        };
        let mc = LlmClient::build_user_content(&content, true);
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
    fn build_user_content_no_images_returns_text() {
        let content = ClipboardContent {
            text: Some("hello".into()),
            images: vec![Arc::new(vec![0x89])],
        };
        // use_images=false: caller decided not to include images.
        let mc = LlmClient::build_user_content(&content, false);
        assert_eq!(mc, MessageContent::Text("hello"));
    }

    #[test]
    fn build_user_content_image_only_no_text_part() {
        let content = ClipboardContent {
            text: None,
            images: vec![Arc::new(vec![0x89, 0x50])],
        };
        let mc = LlmClient::build_user_content(&content, true);
        match mc {
            MessageContent::Parts(parts) => {
                // Only image part, no text part since text is None.
                assert_eq!(parts.len(), 1);
                assert!(matches!(&parts[0], ContentPart::ImageUrl { .. }));
            }
            _ => panic!("expected Parts"),
        }
    }

    #[test]
    fn build_user_content_empty_text_with_image() {
        let content = ClipboardContent {
            text: Some("".into()),
            images: vec![Arc::new(vec![0x89, 0x50])],
        };
        let mc = LlmClient::build_user_content(&content, true);
        match mc {
            MessageContent::Parts(parts) => {
                // Empty text should be omitted, only image part.
                assert_eq!(parts.len(), 1);
                assert!(matches!(&parts[0], ContentPart::ImageUrl { .. }));
            }
            _ => panic!("expected Parts"),
        }
    }

    // --- resolve_thinking tests ---

    #[test]
    fn resolve_thinking_unsupported_returns_none() {
        let (prefix, kwargs) = LlmClient::resolve_thinking(
            ThinkingMode::Think,
            ThinkingControlMethod::Unsupported,
        );
        assert!(prefix.is_none());
        assert!(kwargs.is_none());
    }

    #[test]
    fn resolve_thinking_think_with_kwargs() {
        let (prefix, kwargs) = LlmClient::resolve_thinking(
            ThinkingMode::Think,
            ThinkingControlMethod::ChatTemplateKwargs,
        );
        assert!(prefix.is_none());
        assert!(kwargs.unwrap().enable_thinking);
    }

    #[test]
    fn resolve_thinking_nothink_with_kwargs() {
        let (prefix, kwargs) = LlmClient::resolve_thinking(
            ThinkingMode::NoThink,
            ThinkingControlMethod::ChatTemplateKwargs,
        );
        assert!(prefix.is_none());
        assert!(!kwargs.unwrap().enable_thinking);
    }

    #[test]
    fn resolve_thinking_think_with_prompt_tag() {
        let (prefix, kwargs) = LlmClient::resolve_thinking(
            ThinkingMode::Think,
            ThinkingControlMethod::SystemPromptTag,
        );
        assert_eq!(prefix.unwrap(), "/think\n");
        assert!(kwargs.is_none());
    }

    #[test]
    fn resolve_thinking_nothink_with_prompt_tag() {
        let (prefix, kwargs) = LlmClient::resolve_thinking(
            ThinkingMode::NoThink,
            ThinkingControlMethod::SystemPromptTag,
        );
        assert_eq!(prefix.unwrap(), "/no_think\n");
        assert!(kwargs.is_none());
    }
}
