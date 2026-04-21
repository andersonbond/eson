//! Generic LLM clients (Anthropic + OpenAI-compatible providers).
//!
//! Both clients use **server-sent events** (SSE) for the actual HTTP call so
//! that **reasoning tokens are streamed to the UI as they're generated**,
//! not buffered until the round completes. Internally we still assemble the
//! full response so [`AnthropicClient::complete_with_tools`] /
//! [`OpenAiCompatClient::complete_with_tools`] keep returning the complete
//! text — the streaming side-channel is used purely for the
//! `on_thinking_delta` callback that powers the inline "Reasoning" panel.
//!
//! See `services/agent/src/main.rs::emit_llm_thinking_delta` for the
//! orchestrator event payload that wraps each delta.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use thiserror::Error;

const DEFAULT_ANTHROPIC_MODEL: &str = "claude-haiku-4-5-20251001";
const ANTHROPIC_API_VERSION: &str = "2023-06-01";

/// Per-call HTTP timeout for every LLM request (covers connect + streaming body).
/// Overridable via `ESON_LLM_HTTP_TIMEOUT_SECS`. Default **600 s (10 min)** so a
/// slow local model (Ollama `gemma4:e4b` on CPU) has room to finish a single
/// round without reqwest dropping the SSE stream mid-flight, even when the
/// preceding tool stuffed a multi-page result into the prompt. Bumped from the
/// original 120 s → 300 s → 600 s as users reported turns getting cut off
/// mid-stream on heavier local workloads.
///
/// Cloud providers (Anthropic, OpenAI) finish well under a minute in practice,
/// so this is effectively a safety net — not a functional cap — for them. For
/// per-session overrides from the UI, see [`resolve_http_timeout`].
pub fn llm_http_timeout() -> std::time::Duration {
    const DEFAULT_SECS: u64 = 600;
    let secs = std::env::var("ESON_LLM_HTTP_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_SECS);
    std::time::Duration::from_secs(secs)
}

/// Resolve a per-call HTTP timeout, with the following precedence:
///   1. `override_secs` (UI Settings → Advanced → "Per-request HTTP timeout")
///   2. `ESON_LLM_HTTP_TIMEOUT_SECS` env (set in `secrets.env`)
///   3. Built-in default (10 min)
///
/// Used by the `*_for_session` helpers in `main.rs` so a slow local model
/// can be given more headroom from the UI without restarting the agent.
/// Hard-capped at one hour to keep a runaway value from wedging a worker
/// thread on a hung connection forever.
pub fn resolve_http_timeout(override_secs: Option<u64>) -> std::time::Duration {
    const HARD_CAP_SECS: u64 = 3600;
    if let Some(secs) = override_secs.filter(|n| *n > 0) {
        return std::time::Duration::from_secs(secs.min(HARD_CAP_SECS));
    }
    llm_http_timeout()
}

/// Max LLM↔tool orchestration rounds per user message (each round is at least one model call).
/// `ESON_MAX_LLM_TOOL_ROUNDS` overrides the default; hard-capped at **1000**.
pub fn max_llm_tool_rounds() -> u32 {
    const DEFAULT: u32 = 1000;
    const HARD_CAP: u32 = 1000;
    std::env::var("ESON_MAX_LLM_TOOL_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT)
        .clamp(1, HARD_CAP)
}

const DEFAULT_OPENAI_BASE: &str = "https://api.openai.com/v1";
const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini";
const DEFAULT_OLLAMA_BASE: &str = "http://127.0.0.1:11434/v1";
const DEFAULT_OLLAMA_MODEL: &str = "gemma4:e4b";

/// One turn for chat APIs. `content` is either a string (user/assistant text) or an array of
/// blocks (assistant `tool_use` / user `tool_result` turns).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiMessage {
    pub role: String,
    pub content: Value,
}

// --------------------- Anthropic ---------------------

#[derive(Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
}

impl AnthropicConfig {
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty())?;
        let model = std::env::var("ANTHROPIC_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_string());
        let max_tokens = std::env::var("ANTHROPIC_MAX_TOKENS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8192);
        Some(Self {
            api_key,
            model,
            max_tokens,
        })
    }
}

/// Anthropic extended-thinking budget. Setting `ANTHROPIC_THINKING_BUDGET=0`
/// disables the feature; any positive integer is forwarded as `budget_tokens`
/// (capped server-side). Default is `1024` so users get reasoning visibility
/// out of the box on supporting models (Haiku 4.5+, Sonnet 4+, Opus 4+).
fn anthropic_thinking_budget() -> u32 {
    std::env::var("ANTHROPIC_THINKING_BUDGET")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

#[derive(Debug, Serialize)]
struct AnthropicThinking {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u32,
}

#[derive(Debug, Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [Value]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    /// Always `true` — we exclusively use SSE for the actual HTTP call so
    /// reasoning tokens reach the UI as they're produced.
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    error: ApiErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ApiErrorDetail {
    message: String,
}

#[derive(Debug, Error)]
pub enum AnthropicError {
    #[error("HTTP {0}: {1}")]
    Http(u16, String),
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("no text in assistant response")]
    EmptyResponse,
    #[error("tool loop exceeded {0} rounds")]
    ToolLoopLimit(u32),
}

#[derive(Clone)]
pub struct AnthropicClient {
    cfg: AnthropicConfig,
    http: reqwest::Client,
}

impl AnthropicClient {
    pub fn new(cfg: AnthropicConfig) -> Self {
        Self::new_with_timeout(cfg, llm_http_timeout())
    }

    /// Same as [`Self::new`] but uses a caller-supplied total request
    /// timeout instead of the env-driven default. Lets the per-session
    /// `ProviderSettings.http_timeout_secs` (UI Settings → Advanced)
    /// take effect immediately without restarting the agent.
    pub fn new_with_timeout(cfg: AnthropicConfig, timeout: std::time::Duration) -> Self {
        Self {
            cfg,
            http: reqwest::Client::builder()
                // Short connect cap so a bad host fails fast and we move on
                // to the next provider in the fallback chain; the long total
                // timeout only matters once the connection is alive.
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(timeout)
                .build()
                .expect("reqwest client"),
        }
    }

    /// Model name (e.g. `claude-haiku-4-5-20251001`) — used for telemetry.
    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// Full HTTP endpoint the client posts to — used for telemetry.
    pub fn endpoint(&self) -> &'static str {
        "https://api.anthropic.com/v1/messages"
    }

    /// One round-trip against `/v1/messages` using **SSE streaming**.
    /// Returns the assembled `content` array (same shape as the
    /// non-streaming response) and fires:
    ///   * `on_thinking_delta(text)` for every `thinking_delta` chunk, and
    ///   * `on_content_delta(text)`  for every `text_delta` chunk
    /// *as they arrive* — together they power the live "Reasoning" panel
    /// **and** the streaming answer in the chat bubble.
    async fn post_messages_stream<T, C>(
        &self,
        messages: &[ApiMessage],
        system: Option<&str>,
        tools: Option<&[Value]>,
        mut on_thinking_delta: T,
        mut on_content_delta: C,
    ) -> Result<Vec<Value>, AnthropicError>
    where
        T: FnMut(&str),
        C: FnMut(&str),
    {
        // Extended thinking adds a `thinking` content block in the
        // response. The budget must stay strictly less than `max_tokens`
        // (Anthropic constraint), so we clamp it here defensively rather
        // than failing the request.
        let thinking = match anthropic_thinking_budget() {
            0 => None,
            n => {
                let cap = self.cfg.max_tokens.saturating_sub(256).max(256);
                Some(AnthropicThinking {
                    kind: "enabled",
                    budget_tokens: n.min(cap),
                })
            }
        };
        let body = AnthropicRequest {
            model: &self.cfg.model,
            max_tokens: self.cfg.max_tokens,
            messages: messages.to_vec(),
            system,
            tools,
            thinking,
            stream: true,
        };
        let res = self
            .http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.cfg.api_key)
            .header("anthropic-version", ANTHROPIC_API_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            // SSE never started — the server returned a normal JSON error.
            let body_text = res.text().await.unwrap_or_default();
            let msg = serde_json::from_str::<ApiErrorBody>(&body_text)
                .map(|e| e.error.message)
                .unwrap_or(body_text);
            return Err(AnthropicError::Http(status.as_u16(), msg));
        }

        // Per-block accumulators keyed by Anthropic's `index`. Each block
        // contributes one entry to the final `content` array. Maps preserve
        // insertion order via `BTreeMap` (Anthropic indexes are dense + 0-based,
        // so iteration order matches model intent).
        #[derive(Default)]
        struct BlockAcc {
            kind: String,
            text: String,
            thinking: String,
            signature: Option<String>,
            tool_id: Option<String>,
            tool_name: Option<String>,
            tool_input_json: String,
        }
        let mut blocks: BTreeMap<u64, BlockAcc> = BTreeMap::new();
        let mut byte_stream = res.bytes_stream();
        let mut buf = String::new();
        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk?;
            // SSE frames are ASCII-safe (JSON-only), so lossy UTF-8 is fine.
            buf.push_str(&String::from_utf8_lossy(&chunk));
            // Frames are separated by `\n\n`. Each frame may have multiple
            // `data:` lines that should be concatenated.
            while let Some(idx) = buf.find("\n\n") {
                let frame = buf[..idx].to_string();
                buf.drain(..idx + 2);
                let mut data_lines: Vec<&str> = Vec::new();
                for line in frame.lines() {
                    if let Some(rest) = line.strip_prefix("data:") {
                        data_lines.push(rest.trim_start());
                    }
                }
                if data_lines.is_empty() {
                    continue;
                }
                let payload = data_lines.join("\n");
                if payload.trim().is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let event_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match event_type {
                    "content_block_start" => {
                        let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                        let cb = v.get("content_block").cloned().unwrap_or(Value::Null);
                        let kind = cb
                            .get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("text")
                            .to_string();
                        let mut acc = BlockAcc {
                            kind: kind.clone(),
                            ..Default::default()
                        };
                        if kind == "tool_use" {
                            acc.tool_id =
                                cb.get("id").and_then(|s| s.as_str()).map(str::to_string);
                            acc.tool_name =
                                cb.get("name").and_then(|s| s.as_str()).map(str::to_string);
                        }
                        blocks.insert(index, acc);
                    }
                    "content_block_delta" => {
                        let index = v.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                        let delta = v.get("delta").cloned().unwrap_or(Value::Null);
                        let dtype = delta.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        let acc = blocks.entry(index).or_default();
                        match dtype {
                            "text_delta" => {
                                if let Some(t) = delta.get("text").and_then(|t| t.as_str()) {
                                    if !t.is_empty() {
                                        acc.text.push_str(t);
                                        // Stream the user-visible answer in
                                        // real time so the chat bubble fills
                                        // as the model writes — without this
                                        // the UI sees nothing between the
                                        // last `</think>` and `turn_end`.
                                        on_content_delta(t);
                                    }
                                }
                            }
                            "thinking_delta" => {
                                if let Some(t) = delta.get("thinking").and_then(|t| t.as_str()) {
                                    if !t.is_empty() {
                                        acc.thinking.push_str(t);
                                        on_thinking_delta(t);
                                    }
                                }
                            }
                            "signature_delta" => {
                                // Append-only signature for cache-aware thinking
                                // blocks. Preserved on the assembled block so
                                // it round-trips into the next request.
                                if let Some(s) = delta.get("signature").and_then(|s| s.as_str()) {
                                    acc.signature
                                        .get_or_insert_with(String::new)
                                        .push_str(s);
                                }
                            }
                            "input_json_delta" => {
                                if let Some(p) =
                                    delta.get("partial_json").and_then(|p| p.as_str())
                                {
                                    acc.tool_input_json.push_str(p);
                                }
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        // No-op; we already accumulated everything via deltas.
                    }
                    "message_delta" | "message_start" | "ping" | "message_stop" => {}
                    "error" => {
                        let msg = v
                            .get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(|m| m.as_str())
                            .unwrap_or("anthropic stream error")
                            .to_string();
                        return Err(AnthropicError::Http(500, msg));
                    }
                    _ => {}
                }
            }
        }

        // Assemble the final `content` array in index order.
        let mut content_arr: Vec<Value> = Vec::new();
        for (_idx, b) in blocks.into_iter() {
            match b.kind.as_str() {
                "thinking" => {
                    if !b.thinking.is_empty() {
                        let mut block = json!({
                            "type": "thinking",
                            "thinking": b.thinking,
                        });
                        if let Some(sig) = b.signature {
                            block["signature"] = json!(sig);
                        }
                        content_arr.push(block);
                    }
                }
                "text" => {
                    if !b.text.is_empty() {
                        content_arr.push(json!({ "type": "text", "text": b.text }));
                    }
                }
                "tool_use" => {
                    let input: Value = if b.tool_input_json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&b.tool_input_json).unwrap_or(json!({}))
                    };
                    content_arr.push(json!({
                        "type": "tool_use",
                        "id": b.tool_id.unwrap_or_default(),
                        "name": b.tool_name.unwrap_or_default(),
                        "input": input,
                    }));
                }
                _ => {}
            }
        }
        Ok(content_arr)
    }

    pub async fn complete_with_tools<F, T, C, R>(
        &self,
        messages: &mut Vec<ApiMessage>,
        system: Option<&str>,
        tools: &[Value],
        mut run_tool: F,
        mut on_thinking_delta: T,
        mut on_content_delta: C,
        mut on_round_begin: R,
    ) -> Result<String, AnthropicError>
    where
        F: FnMut(&str, &Value) -> String,
        T: FnMut(&str),
        C: FnMut(&str),
        R: FnMut(u32),
    {
        let max_rounds = max_llm_tool_rounds();
        for round in 0..max_rounds {
            // Per-round signal so the caller can surface "API → … round N"
            // in the orchestrator stream. Without this the user sees
            // exactly one `API → ollama` event for the whole turn even
            // when the model loops through 3-4 tool rounds, which on
            // slow local models looks identical to the agent being hung
            // between the tool finishing and the next response landing.
            on_round_begin(round + 1);
            let content_arr = self
                .post_messages_stream(
                    messages,
                    system,
                    Some(tools),
                    &mut on_thinking_delta,
                    &mut on_content_delta,
                )
                .await?;
            if content_arr.is_empty() {
                return Err(AnthropicError::EmptyResponse);
            }

            let has_tool_use = content_arr
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"));
            if !has_tool_use {
                let text = join_text_blocks(&content_arr);
                if text.is_empty() {
                    return Err(AnthropicError::EmptyResponse);
                }
                messages.push(ApiMessage {
                    role: "assistant".into(),
                    content: json!(text),
                });
                return Ok(text);
            }

            messages.push(ApiMessage {
                role: "assistant".into(),
                content: Value::Array(content_arr.clone()),
            });

            let mut tool_results = Vec::new();
            for block in &content_arr {
                if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                    continue;
                }
                let id = block
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown_id");
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown_tool");
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                let output = run_tool(name, &input);
                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": output,
                }));
            }

            if tool_results.is_empty() {
                return Err(AnthropicError::Http(
                    500,
                    "model returned tool_use blocks we could not parse".into(),
                ));
            }

            messages.push(ApiMessage {
                role: "user".into(),
                content: Value::Array(tool_results),
            });
        }
        Err(AnthropicError::ToolLoopLimit(max_rounds))
    }
}

// --------------------- OpenAI-compatible ---------------------

#[derive(Clone)]
pub struct OpenAiCompatConfig {
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
    pub base_url: String,
}

impl OpenAiCompatConfig {
    pub fn from_openai_env() -> Option<Self> {
        let api_key = std::env::var("OPENAI_API_KEY").ok().filter(|s| !s.is_empty())?;
        let model = std::env::var("OPENAI_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string());
        let max_tokens = std::env::var("OPENAI_MAX_TOKENS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8192);
        let base_url = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_OPENAI_BASE.to_string());
        Some(Self {
            api_key,
            model,
            max_tokens,
            base_url,
        })
    }

    pub fn from_ollama_env() -> Option<Self> {
        let model = std::env::var("OLLAMA_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string());
        let max_tokens = std::env::var("OLLAMA_MAX_TOKENS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(8192);
        let base_url = std::env::var("OLLAMA_BASE_URL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| {
                let trimmed = s.trim_end_matches('/');
                if trimmed.ends_with("/v1") {
                    trimmed.to_string()
                } else {
                    format!("{trimmed}/v1")
                }
            })
            .unwrap_or_else(|| DEFAULT_OLLAMA_BASE.to_string());
        let api_key = std::env::var("OLLAMA_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "ollama".to_string());
        Some(Self {
            api_key,
            model,
            max_tokens,
            base_url,
        })
    }
}

#[derive(Debug, Error)]
pub enum OpenAiCompatError {
    #[error("HTTP {0}: {1}")]
    Http(u16, String),
    #[error("request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("no text in assistant response")]
    EmptyResponse,
    #[error("tool loop exceeded {0} rounds")]
    ToolLoopLimit(u32),
}

#[derive(Clone)]
pub struct OpenAiCompatClient {
    cfg: OpenAiCompatConfig,
    http: reqwest::Client,
}

#[derive(Serialize)]
struct ChatReq<'a> {
    model: &'a str,
    messages: Vec<ChatTurn<'a>>,
    max_tokens: u32,
}

#[derive(Serialize)]
struct ChatTurn<'a> {
    role: &'a str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResp {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMsg,
}

#[derive(Deserialize)]
struct ChoiceMsg {
    content: Option<String>,
}

// --- OpenAI-compatible chat completions with tools (OpenAI, Ollama, Azure, vLLM, etc.) ---

#[derive(Serialize)]
struct ChatReqTools<'a> {
    model: &'a str,
    messages: Vec<Value>,
    max_tokens: u32,
    tools: &'a [Value],
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    /// Always `true` for [`OpenAiCompatClient::complete_with_tools`] — we
    /// stream so reasoning + tool-call args can be surfaced incrementally.
    stream: bool,
}

/// Convert Anthropic-style `tools` entries (`name`, `description`, `input_schema`) to OpenAI
/// `chat/completions` tool definitions (`type`, `function` with `parameters`).
pub fn anthropic_tool_specs_to_openai(specs: &[Value]) -> Vec<Value> {
    specs
        .iter()
        .filter_map(|t| {
            let name = t.get("name")?.as_str()?;
            let description = t
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let parameters = t
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters
                }
            }))
        })
        .collect()
}

fn api_messages_to_openai_chat(messages: &[ApiMessage], system: Option<&str>) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(s) = system {
        if !s.is_empty() {
            out.push(json!({ "role": "system", "content": s }));
        }
    }
    for m in messages {
        let role = if m.role == "assistant" {
            "assistant"
        } else {
            "user"
        };
        let text = flatten_content(&m.content);
        out.push(json!({ "role": role, "content": text }));
    }
    out
}

impl OpenAiCompatClient {
    pub fn new(cfg: OpenAiCompatConfig) -> Self {
        Self::new_with_timeout(cfg, llm_http_timeout())
    }

    /// Same as [`Self::new`] but uses a caller-supplied total request
    /// timeout. Used by the `*_for_session` helpers in `main.rs` so the
    /// UI's "Per-request HTTP timeout" override takes effect on the next
    /// turn without an agent restart.
    pub fn new_with_timeout(cfg: OpenAiCompatConfig, timeout: std::time::Duration) -> Self {
        Self {
            cfg,
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(timeout)
                .build()
                .expect("reqwest client"),
        }
    }

    /// Model name (e.g. `gpt-4o-mini`, `gemma4:e4b`) — used for telemetry.
    pub fn model(&self) -> &str {
        &self.cfg.model
    }

    /// Full HTTP endpoint the client posts to — used for telemetry.
    pub fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'))
    }

    pub async fn complete(
        &self,
        messages: Vec<ApiMessage>,
        system: Option<&str>,
    ) -> Result<String, OpenAiCompatError> {
        let mut turns = Vec::new();
        if let Some(s) = system {
            turns.push(ChatTurn {
                role: "system",
                content: s.to_string(),
            });
        }
        for m in messages {
            turns.push(ChatTurn {
                role: if m.role == "assistant" {
                    "assistant"
                } else {
                    "user"
                },
                content: flatten_content(&m.content),
            });
        }

        let body = ChatReq {
            model: &self.cfg.model,
            messages: turns,
            max_tokens: self.cfg.max_tokens,
        };
        let url = format!("{}/chat/completions", self.cfg.base_url.trim_end_matches('/'));
        let res = self
            .http
            .post(url)
            .header("authorization", format!("Bearer {}", self.cfg.api_key))
            .json(&body)
            .send()
            .await?;
        let status = res.status();
        let body_text = res.text().await?;
        if !status.is_success() {
            return Err(OpenAiCompatError::Http(status.as_u16(), body_text));
        }
        let parsed: ChatResp =
            serde_json::from_str(&body_text).map_err(|e| OpenAiCompatError::Http(500, e.to_string()))?;
        let text = parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();
        if text.trim().is_empty() {
            return Err(OpenAiCompatError::EmptyResponse);
        }
        Ok(text)
    }

    /// Multi-turn tool loop using OpenAI Chat Completions tool calling
    /// (works with OpenAI API, Ollama OpenAI-compatible endpoint, vLLM,
    /// Azure, etc.). Uses **SSE streaming** so reasoning + final-answer
    /// tokens reach the orchestrator as they're generated; the assembled
    /// response is returned as a single string for the existing tool-loop
    /// contract.
    pub async fn complete_with_tools<F, T, C, R>(
        &self,
        messages: &mut Vec<ApiMessage>,
        system: Option<&str>,
        tools_anthropic_format: &[Value],
        mut run_tool: F,
        mut on_thinking_delta: T,
        mut on_content_delta: C,
        mut on_round_begin: R,
    ) -> Result<String, OpenAiCompatError>
    where
        F: FnMut(&str, &Value) -> String,
        T: FnMut(&str),
        C: FnMut(&str),
        R: FnMut(u32),
    {
        let openai_tools = anthropic_tool_specs_to_openai(tools_anthropic_format);
        if openai_tools.is_empty() {
            return self.complete(messages.clone(), system).await;
        }

        let mut oa_messages = api_messages_to_openai_chat(messages, system);
        let url = format!(
            "{}/chat/completions",
            self.cfg.base_url.trim_end_matches('/')
        );

        let max_rounds = max_llm_tool_rounds();
        for round in 0..max_rounds {
            // See [`AnthropicClient::complete_with_tools`] for why this
            // exists — surfaces "API → ollama · round 2" so the user can
            // tell when slow local models are processing a tool result
            // (vs the agent being hung).
            on_round_begin(round + 1);
            let stream_out = self
                .post_chat_completions_stream(
                    &url,
                    &oa_messages,
                    &openai_tools,
                    &mut on_thinking_delta,
                    &mut on_content_delta,
                )
                .await?;

            // Whichever path produced *something*, decide whether this
            // round closes (final answer) or continues (tool calls).
            if !stream_out.tool_calls.is_empty() {
                let tool_calls_json: Vec<Value> = stream_out
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments,
                            }
                        })
                    })
                    .collect();
                let content_field: Value = if stream_out.visible_content.trim().is_empty() {
                    Value::Null
                } else {
                    json!(stream_out.visible_content)
                };
                oa_messages.push(json!({
                    "role": "assistant",
                    "content": content_field,
                    "tool_calls": tool_calls_json
                }));

                for tc in &stream_out.tool_calls {
                    let args: Value = serde_json::from_str(tc.arguments.trim())
                        .unwrap_or_else(|_| json!({}));
                    let output = run_tool(tc.name.as_str(), &args);
                    oa_messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tc.id,
                        "content": output
                    }));
                }
                continue;
            }

            let text = stream_out.visible_content.trim().to_string();
            if text.is_empty() {
                return Err(OpenAiCompatError::EmptyResponse);
            }
            messages.push(ApiMessage {
                role: "assistant".into(),
                content: json!(text.clone()),
            });
            return Ok(text);
        }
        Err(OpenAiCompatError::ToolLoopLimit(max_rounds))
    }

    /// One streaming round-trip against `/chat/completions`. Internally
    /// accumulates the response and routes:
    ///
    /// * `delta.reasoning_content` / `delta.reasoning`  → `on_thinking_delta`
    /// * inline `<think>…</think>` chunks inside `delta.content` → `on_thinking_delta`
    /// * everything else in `delta.content` → `on_content_delta` *and*
    ///   accumulated into `visible_content` (returned as the final answer)
    /// * `delta.tool_calls` → merged by index into `tool_calls`
    async fn post_chat_completions_stream<T, C>(
        &self,
        url: &str,
        oa_messages: &[Value],
        openai_tools: &[Value],
        mut on_thinking_delta: T,
        mut on_content_delta: C,
    ) -> Result<StreamedRound, OpenAiCompatError>
    where
        T: FnMut(&str),
        C: FnMut(&str),
    {
        let body = ChatReqTools {
            model: &self.cfg.model,
            messages: oa_messages.to_vec(),
            max_tokens: self.cfg.max_tokens,
            tools: openai_tools,
            tool_choice: Some(json!("auto")),
            stream: true,
        };
        let res = self
            .http
            .post(url)
            .header("authorization", format!("Bearer {}", self.cfg.api_key))
            .header("accept", "text/event-stream")
            .json(&body)
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            let body_text = res.text().await.unwrap_or_default();
            return Err(OpenAiCompatError::Http(status.as_u16(), body_text));
        }

        let mut think_parser = ThinkStreamParser::default();
        let mut tool_calls: BTreeMap<u64, StreamedToolCall> = BTreeMap::new();
        let mut byte_stream = res.bytes_stream();
        let mut buf = String::new();
        'outer: while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(idx) = buf.find("\n\n") {
                let frame = buf[..idx].to_string();
                buf.drain(..idx + 2);
                let mut data_lines: Vec<&str> = Vec::new();
                for line in frame.lines() {
                    if let Some(rest) = line.strip_prefix("data:") {
                        data_lines.push(rest.trim_start());
                    }
                }
                if data_lines.is_empty() {
                    continue;
                }
                let payload = data_lines.join("\n");
                if payload.trim().is_empty() {
                    continue;
                }
                if payload.trim() == "[DONE]" {
                    break 'outer;
                }
                let v: Value = match serde_json::from_str(&payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let choice = v
                    .get("choices")
                    .and_then(|c| c.as_array())
                    .and_then(|a| a.first())
                    .cloned()
                    .unwrap_or(Value::Null);
                let delta = choice.get("delta").cloned().unwrap_or(Value::Null);

                // Reasoning fields. Some servers send these on every chunk
                // (often empty); only forward non-empty deltas.
                if let Some(r) = delta.get("reasoning_content").and_then(|s| s.as_str()) {
                    if !r.is_empty() {
                        on_thinking_delta(r);
                    }
                }
                if let Some(r) = delta.get("reasoning").and_then(|s| s.as_str()) {
                    if !r.is_empty() {
                        on_thinking_delta(r);
                    }
                }
                // Visible content + inline `<think>` extraction.
                if let Some(c) = delta.get("content").and_then(|s| s.as_str()) {
                    if !c.is_empty() {
                        let (visible, thoughts) = think_parser.feed(c);
                        for t in &thoughts {
                            if !t.is_empty() {
                                on_thinking_delta(t);
                            }
                        }
                        // Stream the answer chunk to the orchestrator the
                        // moment it arrives so the chat bubble fills live.
                        // The parser also keeps it internally for the final
                        // assembled `visible_content` returned below.
                        if !visible.is_empty() {
                            on_content_delta(&visible);
                        }
                    }
                }
                // Tool-call deltas: merge by index.
                if let Some(tc_arr) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tc_arr {
                        let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                        let entry = tool_calls.entry(idx).or_default();
                        if let Some(id) = tc.get("id").and_then(|s| s.as_str()) {
                            if !id.is_empty() {
                                entry.id = id.to_string();
                            }
                        }
                        if let Some(func) = tc.get("function") {
                            if let Some(name) = func.get("name").and_then(|s| s.as_str()) {
                                if !name.is_empty() {
                                    entry.name = name.to_string();
                                }
                            }
                            if let Some(args) = func.get("arguments").and_then(|s| s.as_str()) {
                                entry.arguments.push_str(args);
                            }
                        }
                    }
                }

                // Defensive end-of-stream detection. The OpenAI streaming
                // contract is "final chunk has non-null `finish_reason`,
                // then `data: [DONE]`", but Ollama's `/v1/chat/completions`
                // (and a few self-hosted vLLM/llama.cpp builds) routinely
                // ship the terminal chunk **without** the `[DONE]` sentinel
                // — they just stop sending bytes while keeping the HTTP
                // connection open. Without this break the byte_stream sits
                // idle until reqwest's overall timeout fires (5 min), so
                // the user sees a finished bubble but a "Working…" /
                // disabled composer for minutes after the model is done.
                if let Some(reason) = choice.get("finish_reason") {
                    if !reason.is_null() {
                        break 'outer;
                    }
                }
            }
        }
        // Flush any trailing chars in the think parser (handles unclosed
        // `<think>` and short tail buffers).
        let trailing = think_parser.flush();
        for t in &trailing.thoughts {
            if !t.is_empty() {
                on_thinking_delta(t);
            }
        }
        if !trailing.visible.is_empty() {
            on_content_delta(&trailing.visible);
        }

        Ok(StreamedRound {
            visible_content: think_parser.take_visible(),
            tool_calls: tool_calls.into_values().collect(),
        })
    }
}

/// Assembled output of a single streaming `/chat/completions` round.
struct StreamedRound {
    visible_content: String,
    tool_calls: Vec<StreamedToolCall>,
}

#[derive(Default)]
struct StreamedToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Incremental `<think>…</think>` extractor for streamed chat completions.
/// Maintains a small lookahead so a tag straddling chunk boundaries is
/// still detected; visible (non-thinking) content accumulates internally
/// and is drained via [`take_visible`].
#[derive(Default)]
struct ThinkStreamParser {
    in_think: bool,
    pending: String,
    visible: String,
}

impl ThinkStreamParser {
    /// Feed a chunk; returns the *thoughts emitted in this call*. Visible
    /// chunks are buffered; collect them at end via [`take_visible`].
    fn feed(&mut self, chunk: &str) -> (String, Vec<String>) {
        self.pending.push_str(chunk);
        let mut emitted_thoughts: Vec<String> = Vec::new();
        let mut emitted_visible = String::new();
        loop {
            if self.in_think {
                if let Some(close) = self.pending.find("</think>") {
                    let thought: String = self.pending.drain(..close).collect();
                    self.pending.drain(.."</think>".len());
                    if !thought.is_empty() {
                        emitted_thoughts.push(thought);
                    }
                    self.in_think = false;
                    continue;
                }
                // Hold back the last 7 chars in case a `</think>` tag spans
                // chunks. Emit the rest as a thought delta.
                let safe_len = self.pending.len().saturating_sub(7);
                if safe_len == 0 {
                    break;
                }
                // Keep on UTF-8 char boundary.
                let cut = floor_char_boundary(&self.pending, safe_len);
                if cut == 0 {
                    break;
                }
                let thought: String = self.pending.drain(..cut).collect();
                if !thought.is_empty() {
                    emitted_thoughts.push(thought);
                }
                break;
            } else {
                if let Some(open) = self.pending.find("<think>") {
                    let visible: String = self.pending.drain(..open).collect();
                    self.pending.drain(.."<think>".len());
                    self.visible.push_str(&visible);
                    emitted_visible.push_str(&visible);
                    self.in_think = true;
                    continue;
                }
                let safe_len = self.pending.len().saturating_sub(6); // len("<think") = 6
                if safe_len == 0 {
                    break;
                }
                let cut = floor_char_boundary(&self.pending, safe_len);
                if cut == 0 {
                    break;
                }
                let visible: String = self.pending.drain(..cut).collect();
                self.visible.push_str(&visible);
                emitted_visible.push_str(&visible);
                break;
            }
        }
        (emitted_visible, emitted_thoughts)
    }

    fn flush(&mut self) -> ThinkFlush {
        let mut thoughts = Vec::new();
        let mut visible = String::new();
        let leftover = std::mem::take(&mut self.pending);
        if self.in_think {
            // Unclosed `<think>` — surface remainder as a thought.
            if !leftover.is_empty() {
                thoughts.push(leftover);
            }
            self.in_think = false;
        } else {
            self.visible.push_str(&leftover);
            if !leftover.is_empty() {
                visible = leftover;
            }
        }
        ThinkFlush { thoughts, visible }
    }

    fn take_visible(&mut self) -> String {
        std::mem::take(&mut self.visible)
    }
}

struct ThinkFlush {
    thoughts: Vec<String>,
    visible: String,
}

/// `str::floor_char_boundary` is unstable; this is a small backport that
/// walks left from `index` until it lands on a UTF-8 char boundary.
fn floor_char_boundary(s: &str, mut index: usize) -> usize {
    if index >= s.len() {
        return s.len();
    }
    while index > 0 && !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn join_text_blocks(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|b| {
            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                b.get("text").and_then(|t| t.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

fn flatten_content(v: &Value) -> String {
    match v {
        Value::String(s) => s.to_string(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str())
                } else if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    b.get("content").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => json!(v).to_string(),
    }
}

pub fn anthropic_client_or_none() -> Option<AnthropicClient> {
    AnthropicConfig::from_env().map(AnthropicClient::new)
}

pub fn openai_client_or_none() -> Option<OpenAiCompatClient> {
    OpenAiCompatConfig::from_openai_env().map(OpenAiCompatClient::new)
}

pub fn ollama_client_or_none() -> Option<OpenAiCompatClient> {
    OpenAiCompatConfig::from_ollama_env().map(OpenAiCompatClient::new)
}

/// Values for desktop Settings prefill (resolved from the agent process environment).
/// When `expose_secrets` is false, `api_key` is omitted and only `api_key_configured` is set.
pub fn provider_ui_defaults(expose_secrets: bool) -> Value {
    let anth = AnthropicConfig::from_env();
    let oa = OpenAiCompatConfig::from_openai_env();
    let ol = OpenAiCompatConfig::from_ollama_env();

    let ollama_url_display = ol.as_ref().map(|c| {
        c.base_url
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_string()
    });

    let anth_key = if expose_secrets {
        Value::String(
            anth
                .as_ref()
                .map(|a| a.api_key.clone())
                .unwrap_or_default(),
        )
    } else {
        Value::Null
    };
    let oa_key = if expose_secrets {
        Value::String(
            oa.as_ref()
                .map(|o| o.api_key.clone())
                .unwrap_or_default(),
        )
    } else {
        Value::Null
    };

    // Vision routing defaults — surfaced separately in the UI so users
    // can pick a different multimodal backend than their chat backend.
    // Provider env: ESON_VISION_PROVIDER (defaults to ollama), model env:
    // ESON_VISION_MODEL → falls back to OLLAMA_VISION_MODEL → gemma4:e4b.
    let vision_provider = std::env::var("ESON_VISION_PROVIDER")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| matches!(s.as_str(), "ollama" | "anthropic" | "openai" | "claude" | "gpt" | "local"))
        .map(|s| match s.as_str() {
            "claude" => "anthropic".to_string(),
            "gpt" => "openai".to_string(),
            "local" => "ollama".to_string(),
            _ => s,
        })
        .unwrap_or_else(|| "ollama".to_string());
    let vision_model = std::env::var("ESON_VISION_MODEL")
        .ok()
        .or_else(|| std::env::var("OLLAMA_VISION_MODEL").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| match vision_provider.as_str() {
            "anthropic" => anth
                .as_ref()
                .map(|a| a.model.clone())
                .unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_string()),
            "openai" => oa
                .as_ref()
                .map(|o| o.model.clone())
                .unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string()),
            _ => "gemma4:e4b".to_string(),
        });

    // Embeddings routing — read-only in the UI for now. Mirrors the
    // resolution in [`crate::embedder::EmbedClient::from_env`] so the
    // Settings panel shows exactly what the agent + sidecars will use
    // for `scan_images` indexing and the `search_images` LLM tool.
    // Kept in sync with that client on purpose; changing one without
    // the other would desync the UI from runtime behavior.
    let embed_model = std::env::var("ESON_EMBED_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "qwen3-embedding:4b".to_string());
    let embed_base_raw = std::env::var("ESON_EMBED_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("OLLAMA_BASE_URL").ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://127.0.0.1:11434".to_string());
    let embed_base = embed_base_raw
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string();
    let embed_provider = std::env::var("ESON_EMBED_PROVIDER")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "ollama".to_string());

    json!({
        "anthropic": {
            "model": anth.as_ref().map(|a| a.model.clone()).unwrap_or_else(|| DEFAULT_ANTHROPIC_MODEL.to_string()),
            "api_key": anth_key,
            "api_key_configured": anth.is_some(),
        },
        "openai": {
            "model": oa.as_ref().map(|o| o.model.clone()).unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string()),
            "api_key": oa_key,
            "api_key_configured": oa.is_some(),
        },
        "ollama": {
            "url": ollama_url_display.unwrap_or_else(|| "http://127.0.0.1:11434".to_string()),
            "model": ol.as_ref().map(|o| o.model.clone()).unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string()),
        },
        "vision": {
            "provider": vision_provider,
            "model": vision_model,
        },
        "embeddings": {
            "provider": embed_provider,
            "model": embed_model,
            "base_url": embed_base,
        },
    })
}
