//! Eson agent gateway: HTTP control plane + Socket.IO transport + workspace policy.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use dashmap::DashMap;
use eson_agent::{
    agent_loop,
    agent_memory::{AgentMemory, MemoryRow},
    consolidation::{
        build_digest_bundle, build_llm_message, should_record_event_kind, ChatSnippet,
        RecentEvent, RecentEventsBuffer, CONSOLIDATION_SKILL_ID, DEFAULT_WINDOW_SECS,
    },
    embedder,
    llm::{
        anthropic_client_or_none, max_llm_tool_rounds, ollama_client_or_none, openai_client_or_none,
        provider_ui_defaults, AnthropicClient, AnthropicConfig, ApiMessage, OpenAiCompatClient,
        OpenAiCompatConfig,
    },
    memory_client::MemoryClient, os_plane, persona, policy::ConcurrencyPolicy, scan,
    skills::{self, match_inbox_skill},
    vision,
    workspace::WorkspaceRoot,
    workspace_tools,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use socketioxide::{extract::SocketRef, SocketIo};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    workspace: Arc<WorkspaceRoot>,
    agent_memory: Arc<AgentMemory>,
    memory: MemoryClient,
    policy: ConcurrencyPolicy,
    sessions: Arc<DashMap<String, SessionState>>,
    /// Per-session cancellation flags flipped by `POST /session/cancel`.
    /// Checked between LLM rounds and tool dispatches in [`execute_llm`] +
    /// [`dispatch_agent_tool`] so an in-flight turn bails out at the next
    /// safe boundary. The flag is cleared at the start of each new turn.
    cancellations: Arc<DashMap<String, Arc<AtomicBool>>>,
    io: SocketIo,
    anthropic: Option<AnthropicClient>,
    openai: Option<OpenAiCompatClient>,
    ollama: Option<OpenAiCompatClient>,
    /// IDENTITY + SOUL + Eson.md, with {{WORKSPACE}} resolved; empty if files missing.
    persona_bundle: Arc<String>,
    skills_dir: PathBuf,
    tool_socket_queue: Arc<Mutex<Vec<(String, Value)>>>,
    background_config: Arc<RwLock<BackgroundConfig>>,
    /// Bounded rolling log of orchestrator events (tool calls, turn
    /// boundaries, provider fallbacks, etc.) used by the 12h memory
    /// consolidation pass to build its evidence bundle without having
    /// to persist a heavy activity log to disk. See
    /// [`consolidation::should_record_event_kind`] for the kinds
    /// that are retained.
    recent_events: Arc<Mutex<RecentEventsBuffer>>,
    /// Text embedder used by `scan_images` (doc side) and the
    /// `search_images` tool (query side). Defaults to Ollama
    /// `qwen3-embedding:4b` via the OpenAI-compatible
    /// `/v1/embeddings` endpoint — see [`embedder::EmbedClient`].
    embedder: Arc<embedder::EmbedClient>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
enum ChatProvider {
    #[default]
    Anthropic,
    Openai,
    Ollama,
}

fn provider_label(p: ChatProvider) -> &'static str {
    match p {
        ChatProvider::Anthropic => "anthropic",
        ChatProvider::Openai => "openai",
        ChatProvider::Ollama => "ollama",
    }
}

fn truncate_preview(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

fn json_value_preview(v: &Value, max: usize) -> String {
    truncate_preview(
        &serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
        max,
    )
}

fn json_pretty_preview(v: &Value, max: usize) -> String {
    serde_json::to_string_pretty(v)
        .map(|s| truncate_preview(&s, max))
        .unwrap_or_else(|_| "{}".to_string())
}

/// Low-level socket emit. Prefer [`orchestrator_emit`] (which also
/// records events into the consolidation buffer) for new call sites.
async fn orchestrator_emit_raw(io: &SocketIo, payload: &Value) {
    let _ = io.emit("orchestrator", payload).await;
}

/// Pull the consolidation-relevant fields out of an orchestrator
/// payload and push them into the rolling event buffer. We
/// intentionally summarize to short strings rather than cloning the
/// whole payload so the buffer stays cheap even under event storms
/// (see [`consolidation::RecentEventsBuffer`]).
fn record_event_for_consolidation(state: &AppState, payload: &Value) {
    let kind = payload.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    if !should_record_event_kind(kind) {
        return;
    }
    let session_id = payload
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let (summary, tag) = summarize_event(kind, payload);
    let Ok(mut buf) = state.recent_events.lock() else {
        return;
    };
    buf.push(RecentEvent {
        ts_ms: 0, // filled in by the buffer to honor now_ms()
        kind: kind.to_string(),
        session_id,
        summary,
        tag,
    });
}

/// Per-kind textual summary used in the rolling event buffer. The
/// goal is a one-line excerpt the LLM can reason about later — long
/// enough to recognize recurring patterns, short enough to keep the
/// buffer bounded.
fn summarize_event(kind: &str, payload: &Value) -> (String, Option<String>) {
    let get = |k: &str| payload.get(k).and_then(|v| v.as_str()).unwrap_or("");
    match kind {
        "tool" | "tool_begin" => {
            let name = get("tool");
            let cmd = get("command");
            let preview = get("result_preview");
            let ok = payload.get("ok").and_then(|v| v.as_bool());
            let status = match ok {
                Some(true) => "ok",
                Some(false) => "fail",
                None => "run",
            };
            let body = if !preview.is_empty() {
                format!("{name} ({cmd}) [{status}] — {}", truncate_preview(preview, 140))
            } else {
                format!("{name} ({cmd}) [{status}]")
            };
            (body, Some(name.to_string()))
        }
        "turn_begin" => {
            let u = get("user_preview");
            (format!("user: {}", truncate_preview(u, 200)), None)
        }
        "turn_end" => {
            let rounds = payload.get("rounds").and_then(|v| v.as_i64()).unwrap_or(0);
            let chars = payload
                .get("answer_chars")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            (
                format!("assistant turn finished (rounds={rounds}, chars={chars})"),
                None,
            )
        }
        "turn_cancel" => ("turn cancelled".to_string(), None),
        "background_turn" => {
            let skill = get("skill_id");
            (format!("background turn · {skill}"), Some(skill.to_string()))
        }
        "inbox_finalize" => {
            let path = get("rel_path");
            let outcome = get("outcome");
            (format!("inbox {outcome}: {path}"), Some(outcome.to_string()))
        }
        "provider_fallback" => {
            let from = get("from");
            let to = get("to");
            (format!("fallback {from} → {to}"), Some(to.to_string()))
        }
        "llm_call_end" => {
            let provider = get("provider");
            let model = get("model");
            let ok = payload.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
            let err = get("error");
            let tail = if ok {
                "ok".to_string()
            } else if !err.is_empty() {
                format!("err: {}", truncate_preview(err, 120))
            } else {
                "err".to_string()
            };
            (format!("{provider}/{model} {tail}"), Some(provider.to_string()))
        }
        "chart_render" => {
            let path = get("rel_path");
            (format!("chart rendered: {path}"), Some("chart".to_string()))
        }
        "consolidation_begin" | "consolidation_end" => {
            let phase = kind;
            let kept = payload.get("kept").and_then(|v| v.as_i64()).unwrap_or(-1);
            let considered = payload
                .get("considered_total")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            (
                format!("{phase} considered={considered} kept={kept}"),
                Some("consolidation".into()),
            )
        }
        _ => (kind.to_string(), None),
    }
}

/// Emit an orchestrator event **and** record it into the rolling
/// consolidation buffer. This is the canonical emit path — every
/// `kind` worth retaining for the 12h memory consolidation pass
/// ends up in the rolling buffer automatically; low-signal kinds
/// (streaming deltas) are filtered by
/// [`consolidation::should_record_event_kind`].
async fn orchestrator_emit(state: &AppState, payload: &Value) {
    record_event_for_consolidation(state, payload);
    orchestrator_emit_raw(&state.io, payload).await;
}

/// Snapshot the rolling event buffer within the requested window.
/// Returns an owned `Vec` so the caller can drop the lock before
/// doing any expensive formatting.
fn snapshot_events_since(state: &AppState, window_secs: u64) -> Vec<RecentEvent> {
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
        .saturating_sub(window_secs.saturating_mul(1000));
    let Ok(buf) = state.recent_events.lock() else {
        return Vec::new();
    };
    buf.snapshot_since(cutoff)
}

/// Flatten the tail of every live session's message log into recent
/// chat snippets. We don't have per-message timestamps, so "recent"
/// here is approximated by "the last few turns in each live
/// session" — good enough for the 12h digest because the orchestrator
/// also injects event timestamps for cross-reference.
fn snapshot_recent_chat(state: &AppState, per_session_tail: usize) -> Vec<ChatSnippet> {
    let mut out: Vec<ChatSnippet> = Vec::new();
    for entry in state.sessions.iter() {
        let sid = entry.key().clone();
        let msgs = &entry.value().messages;
        let tail_start = msgs.len().saturating_sub(per_session_tail);
        for m in &msgs[tail_start..] {
            let Some(text) = flatten_message_text(&m.content) else {
                continue;
            };
            if text.trim().is_empty() {
                continue;
            }
            out.push(ChatSnippet {
                session_id: sid.clone(),
                role: m.role.clone(),
                text,
            });
        }
    }
    out
}

/// Best-effort plain-text extraction from an `ApiMessage::content`
/// value. Handles the two shapes the chat providers return:
/// a bare string (user turns) and an array of content blocks
/// (assistant tool_use / tool_result turns). We stringify the first
/// text block we find; anything else is ignored.
fn flatten_message_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        for block in arr {
            if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Cheap count of durable memory rows — used to diff around the
/// consolidation LLM turn so the orchestrator marker can report
/// exactly how many new rows were persisted.
fn count_memory_rows(state: &AppState) -> usize {
    state
        .agent_memory
        .recall("", 100)
        .map(|rows| rows.len())
        .unwrap_or(0)
}

/// Count the number of entries in the `.learnings/` journals. Used
/// for the "kept" stat; checks summary line counts across all three
/// files.
fn count_learning_entries(state: &AppState) -> usize {
    let dir = state.workspace.root().join(".learnings");
    let mut n = 0usize;
    for name in ["LEARNINGS.md", "ERRORS.md", "FEATURE_REQUESTS.md"] {
        if let Ok(text) = std::fs::read_to_string(dir.join(name)) {
            // Each entry starts with `### {PREFIX}-{millis}` on its
            // own line. Counting the heading prefix is cheaper than
            // full parsing and sufficient for "did new entries land?".
            n += text.matches("\n### ").count();
        }
    }
    n
}

#[derive(Clone, Default)]
struct SessionState {
    messages: Vec<ApiMessage>,
    provider: ChatProvider,
    settings: ProviderSettings,
    #[allow(dead_code)]
    last_branch: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct ProviderSettings {
    anthropic: Option<AnthropicOverrides>,
    openai: Option<OpenAiOverrides>,
    ollama: Option<OllamaOverrides>,
    /// Per-session vision routing for `analyze_visual` / `pdf_to_table`.
    /// Independent of the chat provider so a user can run chat on Ollama
    /// while letting Anthropic do the multimodal heavy lifting (or vice
    /// versa). Falls back to the `ESON_VISION_PROVIDER` env when unset.
    vision: Option<VisionOverrides>,
    /// Per-session override for the LLM HTTP request timeout (seconds).
    /// Set from the UI's Settings → AI Provider → Advanced panel; falls
    /// back to `ESON_LLM_HTTP_TIMEOUT_SECS` (default 600 s = 10 min) when
    /// unset. Hard-capped at 1 hour by [`llm::resolve_http_timeout`] to
    /// guard against a runaway value pinning a worker thread on a hung
    /// connection.
    http_timeout_secs: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct VisionOverrides {
    /// `"ollama"`, `"anthropic"`, or `"openai"` — case-insensitive. Anything
    /// else is rejected by [`parse_vision_provider`] and falls back to env.
    provider: Option<String>,
    /// Model id to send to the chosen provider (e.g. `gemma4:e4b`,
    /// `claude-haiku-4-5-20251001`, `gpt-4o-mini`).
    model: Option<String>,
    /// When provider is Ollama (or for native + OpenAI-compat vision routes),
    /// optional base URL **without** requiring `/v1` — same rules as the chat
    /// Ollama URL. Overrides the AI Provider Ollama URL for vision only.
    url: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AnthropicOverrides {
    model: Option<String>,
    api_key: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct OpenAiOverrides {
    model: Option<String>,
    api_key: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct OllamaOverrides {
    model: Option<String>,
    url: Option<String>,
}

fn non_empty_opt(s: Option<String>) -> Option<String> {
    s.filter(|t| !t.trim().is_empty())
}

fn merge_provider_settings(target: &mut ProviderSettings, patch: ProviderSettings) {
    if let Some(p) = patch.anthropic {
        let cur = target.anthropic.take().unwrap_or_default();
        target.anthropic = Some(AnthropicOverrides {
            model: non_empty_opt(p.model).or(cur.model),
            api_key: non_empty_opt(p.api_key).or(cur.api_key),
        });
    }
    if let Some(p) = patch.openai {
        let cur = target.openai.take().unwrap_or_default();
        target.openai = Some(OpenAiOverrides {
            model: non_empty_opt(p.model).or(cur.model),
            api_key: non_empty_opt(p.api_key).or(cur.api_key),
        });
    }
    if let Some(p) = patch.ollama {
        let cur = target.ollama.take().unwrap_or_default();
        target.ollama = Some(OllamaOverrides {
            model: non_empty_opt(p.model).or(cur.model),
            url: non_empty_opt(p.url).or(cur.url),
        });
    }
    if let Some(p) = patch.vision {
        let cur = target.vision.take().unwrap_or_default();
        // When the client sends `"url": ""`, clear the override so vision
        // inherits the chat Ollama URL again (same pattern as an empty
        // string in the UI).
        let url = match p.url {
            None => cur.url,
            Some(ref s) if s.trim().is_empty() => None,
            Some(s) => Some(s.trim().to_string()),
        };
        target.vision = Some(VisionOverrides {
            provider: non_empty_opt(p.provider).or(cur.provider),
            model: non_empty_opt(p.model).or(cur.model),
            url,
        });
    }
    if let Some(secs) = patch.http_timeout_secs.filter(|n| *n > 0) {
        target.http_timeout_secs = Some(secs);
    }
}

/// Build the per-turn [`vision::VisionConfig`]. **Non-empty** values from
/// Settings (Vision model / Vision URL / chat Ollama URL / Vision provider)
/// take precedence over `ESON_VISION_*` in `.env` so the user can manage
/// routing in the app without editing or commenting out env.
fn vision_config_for_session(settings: &ProviderSettings) -> vision::VisionConfig {
    let mut cfg = vision::VisionConfig::from_session_picks(vision::SessionVisionPicks {
        provider: settings.vision.as_ref().and_then(|v| v.provider.as_deref()),
        model: settings.vision.as_ref().and_then(|v| v.model.as_deref()),
        vision_ollama_url: settings.vision.as_ref().and_then(|v| v.url.as_deref()),
        chat_ollama_url: settings.ollama.as_ref().and_then(|o| o.url.as_deref()),
    });
    if let Some(a) = settings.anthropic.as_ref() {
        if let Some(k) = a.api_key.as_deref() {
            let k = k.trim();
            if !k.is_empty() {
                cfg.anthropic_api_key = k.to_string();
            }
        }
    }
    if let Some(o) = settings.openai.as_ref() {
        if let Some(k) = o.api_key.as_deref() {
            let k = k.trim();
            if !k.is_empty() {
                cfg.openai_api_key = k.to_string();
            }
        }
    }
    cfg
}

fn expose_llm_secrets_to_ui() -> bool {
    std::env::var("ESON_EXPOSE_LLM_SECRETS_TO_UI")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

fn load_env_files() {
    // The bundled desktop shell sets `ESON_SKIP_DOTENV=1` on the sidecars
    // so the agent never picks up a developer's repo-local `.env`/`.env.local`
    // when its inherited cwd happens to sit inside a checkout. All env in
    // bundled mode flows through the desktop's `secrets.env`.
    if std::env::var("ESON_SKIP_DOTENV")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        return;
    }
    let _ = dotenvy::dotenv();
    let _ = dotenvy::from_filename_override(".env.local");
}

/// Keyword / recency recall from `db/memory.db` for injection into the system prompt every turn.
fn agent_memory_prompt_snippet(mem: &AgentMemory, user_message: &str) -> String {
    const LIMIT: usize = 15;
    const BODY_MAX: usize = 500;
    let q = user_message.trim();
    match mem.recall(q, LIMIT) {
        Ok(rows) if rows.is_empty() => {
            let note = if q.is_empty() {
                "No rows in agent memory yet. When the user asks to remember something, call store_memory (Anthropic)."
            } else {
                "No rows matched this message’s keywords; try recall_memory with other search terms."
            };
            serde_json::to_string_pretty(&json!({ "matches": [], "note": note }))
                .unwrap_or_else(|_| "{\"matches\":[]}".to_string())
        }
        Ok(rows) => format_agent_memory_rows_json(&rows, BODY_MAX),
        Err(e) => serde_json::to_string_pretty(&json!({ "error": e.to_string() }))
            .unwrap_or_else(|_| format!("{{\"error\":\"{e}\"}}")),
    }
}

fn format_agent_memory_rows_json(rows: &[MemoryRow], body_max: usize) -> String {
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            let mut body = r.body.clone();
            if body.len() > body_max {
                body.truncate(body_max);
                body.push('…');
            }
            json!({
                "id": r.id,
                "summary": r.summary,
                "body": body,
                "topics": r.topics,
                "importance": r.importance,
            })
        })
        .collect();
    serde_json::to_string_pretty(&json!({ "matches": items }))
        .unwrap_or_else(|_| "{}".to_string())
}

/// Persistent runtime config for cron + inbox automation. Persists to
/// `<workspace>/db/background_settings.json` so picks survive restarts.
///
/// Optional fields (`None`) fall back to the corresponding `agent_loop` env
/// helpers, so an unset UI value mirrors the env-driven default exactly.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct BackgroundConfig {
    provider: ChatProvider,
    #[serde(default)]
    settings: ProviderSettings,
    #[serde(default)]
    loop_enabled: Option<bool>,
    #[serde(default)]
    inbox_auto: Option<bool>,
    #[serde(default)]
    heartbeat_sec: Option<u64>,
    #[serde(default)]
    inbox_debounce_ms: Option<u64>,
}

impl BackgroundConfig {
    fn loop_enabled_resolved(&self) -> bool {
        self.loop_enabled
            .unwrap_or_else(agent_loop::background_loop_enabled)
    }

    fn inbox_auto_resolved(&self) -> bool {
        self.inbox_auto
            .unwrap_or_else(agent_loop::inbox_auto_enabled)
    }

    fn heartbeat_resolved(&self) -> std::time::Duration {
        let env_default = agent_loop::heartbeat_interval().as_secs();
        let sec = self.heartbeat_sec.unwrap_or(env_default).clamp(10, 3600);
        std::time::Duration::from_secs(sec)
    }

    fn debounce_resolved(&self) -> std::time::Duration {
        let env_default = agent_loop::debounce_duration().as_millis() as u64;
        std::time::Duration::from_millis(self.inbox_debounce_ms.unwrap_or(env_default))
    }
}

fn background_settings_path(ws: &WorkspaceRoot) -> PathBuf {
    ws.root().join("db").join("background_settings.json")
}

fn parse_provider_label(s: &str) -> Option<ChatProvider> {
    match s.trim().to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => Some(ChatProvider::Anthropic),
        "openai" | "gpt" => Some(ChatProvider::Openai),
        "ollama" | "local" => Some(ChatProvider::Ollama),
        _ => None,
    }
}

fn background_provider_from_env() -> ChatProvider {
    std::env::var("ESON_BACKGROUND_PROVIDER")
        .ok()
        .and_then(|v| parse_provider_label(&v))
        .unwrap_or(ChatProvider::Ollama)
}

fn load_background_config(ws: &WorkspaceRoot) -> BackgroundConfig {
    let path = background_settings_path(ws);
    if let Ok(raw) = std::fs::read_to_string(&path) {
        if let Ok(cfg) = serde_json::from_str::<BackgroundConfig>(&raw) {
            return cfg;
        }
    }
    BackgroundConfig {
        provider: background_provider_from_env(),
        settings: ProviderSettings::default(),
        ..BackgroundConfig::default()
    }
}

fn save_background_config(ws: &WorkspaceRoot, cfg: &BackgroundConfig) -> Result<(), String> {
    let path = background_settings_path(ws);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| e.to_string())
}

/// Pick the provider to use for one background turn: configured choice if available,
/// otherwise transparently fall back to Anthropic (then OpenAI) so the loop keeps working.
fn resolve_background_provider(state: &AppState) -> Option<(ChatProvider, ProviderSettings)> {
    let cfg = state
        .background_config
        .read()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default();
    if provider_can_message(state, cfg.provider, &cfg.settings) {
        return Some((cfg.provider, cfg.settings));
    }
    let empty = ProviderSettings::default();
    for fallback in [ChatProvider::Anthropic, ChatProvider::Openai, ChatProvider::Ollama] {
        if fallback == cfg.provider {
            continue;
        }
        if provider_can_message(state, fallback, &empty) {
            tracing::warn!(
                requested = ?cfg.provider,
                using = ?fallback,
                "background provider not configured \u{2014} falling back"
            );
            return Some((fallback, empty));
        }
    }
    None
}

fn agent_memory_db_path(ws: &WorkspaceRoot) -> PathBuf {
    match std::env::var("ESON_AGENT_MEMORY_DB") {
        Ok(p) if !p.trim().is_empty() => {
            let pb = PathBuf::from(p.trim());
            if pb.is_absolute() {
                pb
            } else {
                ws.root().join(pb)
            }
        }
        _ => ws.root().join("db/memory.db"),
    }
}

#[tokio::main]
async fn main() {
    load_env_files();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("eson_agent=info".parse().unwrap())
                .add_directive("socketioxide=warn".parse().unwrap()),
        )
        .init();

    info!(
        cap = max_llm_tool_rounds(),
        "LLM tool orchestration round cap per message (ESON_MAX_LLM_TOOL_ROUNDS)"
    );

    let workspace = Arc::new(
        WorkspaceRoot::from_env().expect("ESON_WORKSPACE_ROOT must be valid"),
    );
    info!(root = %workspace.root().display(), "workspace root");

    let agent_memory = Arc::new(
        AgentMemory::open(agent_memory_db_path(workspace.as_ref())).expect("open agent memory sqlite"),
    );
    info!(path = %agent_memory.db_path().display(), "agent memory (store_memory / recall_memory)");

    let memory = MemoryClient::from_env();
    let policy = ConcurrencyPolicy::from_env();
    let sessions: Arc<DashMap<String, SessionState>> = Arc::new(DashMap::new());
    let anthropic = anthropic_client_or_none();
    let openai = openai_client_or_none();
    let ollama = ollama_client_or_none();
    if anthropic.is_some() {
        info!("Anthropic Claude API enabled");
    } else {
        info!("ANTHROPIC_API_KEY not set — /session/message will return 503 until configured (.env.local)");
    }
    if openai.is_some() {
        info!("OpenAI API enabled");
    } else {
        info!("OPENAI_API_KEY not set — OpenAI provider disabled");
    }
    if ollama.is_some() {
        info!("Ollama provider enabled");
    } else {
        info!("OLLAMA config missing — Ollama provider disabled");
    }

    let persona_dir = persona::resolve_persona_dir();
    let persona_bundle = Arc::new(persona::load_persona_bundle(
        &persona_dir,
        workspace.root(),
    ));
    let skills_dir = skills::resolve_skills_dir();
    info!(dir = %skills_dir.display(), "skills directory");
    vision::log_vision_startup_warnings();
    let tool_socket_queue = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let _ = std::fs::create_dir_all(workspace.root().join("inbox"));
    let background_config = Arc::new(RwLock::new(load_background_config(workspace.as_ref())));
    info!(
        provider = ?background_config.read().map(|g| g.provider).unwrap_or_default(),
        path = %background_settings_path(workspace.as_ref()).display(),
        "background automation provider (env ESON_BACKGROUND_PROVIDER overrides; UI saves to disk)"
    );
    if persona_bundle.is_empty() {
        tracing::warn!(
            dir = %persona_dir.display(),
            "persona bundle empty — add persona/IDENTITY.md, SOUL.md, Eson.md (or set ESON_PERSONA_DIR)"
        );
    } else {
        info!(dir = %persona_dir.display(), chars = persona_bundle.len(), "persona bundle loaded");
    }

    let (layer, io) = SocketIo::new_layer();
    let io_clone = io.clone();
    io.ns("/", |s: SocketRef| {
        info!(sid = %s.id, "socket connected");
    });

    let embedder = Arc::new(embedder::EmbedClient::from_env());
    info!(
        model = embedder.model(),
        base = embedder.base(),
        "text embedder (search_images query + scan_images indexing)"
    );

    let state = AppState {
        workspace: workspace.clone(),
        agent_memory: agent_memory.clone(),
        memory: memory.clone(),
        policy: policy.clone(),
        sessions,
        cancellations: Arc::new(DashMap::new()),
        io: io_clone,
        anthropic,
        openai,
        ollama,
        persona_bundle,
        skills_dir: skills_dir.clone(),
        tool_socket_queue: tool_socket_queue.clone(),
        background_config: background_config.clone(),
        recent_events: Arc::new(Mutex::new(RecentEventsBuffer::default())),
        embedder,
    };

    let bg_state = state.clone();
    tokio::spawn(async move {
        background_loops(bg_state).await;
    });

    let app = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/session/start", post(session_start))
        .route("/session/providers", get(session_providers))
        .route("/session/provider-defaults", get(session_provider_defaults))
        .route("/background/settings", get(background_settings_get).post(background_settings_post))
        .route("/session/message", post(session_message))
        .route("/session/cancel", post(session_cancel))
        .route("/session/branch", post(session_branch))
        .route("/session/merge", post(session_merge))
        .route("/session/terminate", post(session_terminate))
        .route("/system/health", get(system_health))
        .route("/system/processes", get(system_processes))
        .route("/workspace/info", get(workspace_info))
        .route("/workspace/browse", get(workspace_browse))
        .route("/workspace/preview", get(workspace_preview))
        .route("/ingestion/scan-images", post(scan_images))
        .with_state(state)
        .layer(layer)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    let port: u16 = std::env::var("ESON_AGENT_HTTP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8787);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind agent port");
    info!(%port, "eson-agent listening (HTTP + Socket.IO on same port)");
    axum::serve(listener, app).await.expect("serve");
}

async fn health() -> &'static str {
    "ok"
}

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    if state.memory.status_ok().await {
        (StatusCode::OK, Json(json!({ "ready": true })))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "ready": false, "reason": "memory unreachable" })),
        )
    }
}

#[derive(Deserialize)]
struct SessionStart {
    provider: Option<ChatProvider>,
    settings: Option<ProviderSettings>,
}

#[derive(Serialize)]
struct SessionStartResp {
    session_id: String,
    provider: ChatProvider,
}

async fn session_start(
    State(state): State<AppState>,
    Json(body): Json<SessionStart>,
) -> Result<Json<SessionStartResp>, (StatusCode, Json<Value>)> {
    let provider = body.provider.unwrap_or(ChatProvider::Anthropic);
    let settings = body.settings.unwrap_or_default();
    if !provider_start_available(&state, provider, &settings) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": format!("provider `{provider:?}` is not configured on this agent")
            })),
        ));
    }
    let id = Uuid::new_v4().to_string();
    state.sessions.insert(
        id.clone(),
        SessionState {
            provider,
            settings,
            ..Default::default()
        },
    );
    Ok(Json(SessionStartResp {
        session_id: id,
        provider,
    }))
}

#[derive(Deserialize)]
struct SessionMessage {
    session_id: String,
    message: String,
    settings: Option<ProviderSettings>,
    /// Per-message override of the session's primary provider. Lets the
    /// desktop UI flip the primary mid-conversation (Settings → AI Provider
    /// → "Set as primary") without forcing the user to start a new chat.
    /// When `None` we keep whatever the session was created with.
    provider: Option<ChatProvider>,
}

#[derive(Serialize)]
struct SessionMessageResp {
    answer: String,
    session_id: String,
}

type MsgErr = (StatusCode, Json<Value>);

/// One-line description of what the orchestrator is doing (shown in the Activity panel).
fn tool_human_command(name: &str, input: &Value) -> String {
    match name {
        "workspace_list" => {
            let p = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if p.is_empty() {
                "List files in workspace root".to_string()
            } else {
                format!("List files in workspace folder \"{p}\"")
            }
        }
        "workspace_read" => {
            let p = input.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Read workspace file \"{p}\"")
        }
        "workspace_grep" => {
            let pat = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            let p = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if p.is_empty() {
                format!(
                    "Grep workspace (substring) for \"{}\"",
                    truncate_preview(pat, 160)
                )
            } else {
                format!(
                    "Grep under \"{}\" for \"{}\"",
                    truncate_preview(p, 100),
                    truncate_preview(pat, 160)
                )
            }
        }
        "store_memory" => {
            let s = input.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            let mt = input
                .get("memory_type")
                .and_then(|v| v.as_str())
                .unwrap_or("episodic");
            format!(
                "Store memory [{mt}] · {}",
                truncate_preview(s, 100)
            )
        }
        "recall_memory" => {
            let q = input.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let mt = input
                .get("memory_type")
                .and_then(|v| v.as_str())
                .unwrap_or("any");
            format!("Search stored memories [{mt}] · {}", truncate_preview(q, 120))
        }
        "run_terminal" => {
            let c = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            format!("Run shell (cwd = workspace) · {}", truncate_preview(c, 2000))
        }
        "skill_list" => "List skills".to_string(),
        "skill_run" => {
            let id = input.get("skill_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Load skill `{id}`")
        }
        "render_chart" => "Render Chart.js chart".to_string(),
        "analyze_visual" => {
            let p = input.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Analyze visual `{p}` (local Ollama)")
        }
        "pdf_to_table" => {
            let p = input.get("pdf_path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("PDF → table `{p}`")
        }
        _ => format!("Tool `{name}`"),
    }
}

fn flush_tool_socket_queue(
    queue: &Arc<Mutex<Vec<(String, Value)>>>,
    io: &SocketIo,
    session_id: &str,
) {
    let pending: Vec<(String, Value)> = match queue.lock() {
        Ok(mut g) => std::mem::take(&mut *g),
        Err(_) => return,
    };
    for (ev, mut pl) in pending {
        if let Value::Object(ref mut m) = pl {
            m.entry("session_id".to_string())
                .or_insert_with(|| Value::String(session_id.to_string()));
        }
        let io_c = io.clone();
        tokio::spawn(async move {
            let _ = io_c.emit(&ev, &pl).await;
        });
    }
}

fn dispatch_agent_tool(
    workspace: &WorkspaceRoot,
    agent_memory: &AgentMemory,
    skills_root: &Path,
    socket_queue: &Arc<Mutex<Vec<(String, Value)>>>,
    io: &SocketIo,
    vision: &vision::VisionConfig,
    embedder: &embedder::EmbedClient,
    memory_client: &MemoryClient,
    cancel: &Arc<AtomicBool>,
    session_id: &str,
    name: &str,
    input: &Value,
) -> String {
    // Bail out *before* doing any work when the user has clicked Stop.
    // Returning a structured error gives the LLM a chance to terminate the
    // tool loop on its own; combined with the [`execute_llm`] check this is
    // usually enough to free the session within one round.
    if cancel.load(Ordering::SeqCst) {
        return "{\"error\":\"cancelled by user\"}".to_string();
    }
    let start = std::time::Instant::now();
    let tool_id = Uuid::new_v4().to_string();
    let command = tool_human_command(name, input);
    let args_preview = json_value_preview(input, 720);
    let args_pretty = json_pretty_preview(input, 3200);
    let ctx = workspace_tools::ToolContext {
        workspace,
        memory: agent_memory,
        skills_root,
        socket_queue,
        vision,
        embedder,
        memory_client,
    };
    // Announce the tool *before* the (potentially multi-minute) call so
    // the user sees an in-flight step in the reasoning panel + Activity
    // log. Without this, `analyze_visual` on a slow CPU + Ollama looks
    // identical to the agent being hung — there are no events between
    // the model finishing its reasoning and the tool result coming back.
    {
        let io_b = io.clone();
        let sid_b = session_id.to_string();
        let tool_id_b = tool_id.clone();
        let tool_b = name.to_string();
        let command_b = command.clone();
        let args_preview_b = args_preview.clone();
        let args_pretty_b = args_pretty.clone();
        tokio::spawn(async move {
            let _ = io_b
                .emit(
                    "orchestrator",
                    &json!({
                        "kind": "tool_begin",
                        "session_id": sid_b,
                        "tool_id": tool_id_b,
                        "tool": tool_b,
                        "command": command_b,
                        "args_preview": args_preview_b,
                        "args_pretty": args_pretty_b,
                    }),
                )
                .await;
        });
    }
    // Periodic heartbeat so the user sees "still running · 25 s elapsed"
    // while a slow tool grinds. The UI coalesces these into a single
    // updating row keyed by `tool_id` — no spam. Aborted as soon as the
    // tool returns.
    let heartbeat = {
        let io_h = io.clone();
        let sid_h = session_id.to_string();
        let tool_id_h = tool_id.clone();
        let tool_h = name.to_string();
        let command_h = command.clone();
        tokio::spawn(async move {
            let tick = std::time::Duration::from_secs(5);
            let mut elapsed: u64 = 0;
            loop {
                tokio::time::sleep(tick).await;
                elapsed += 5;
                let _ = io_h
                    .emit(
                        "orchestrator",
                        &json!({
                            "kind": "tool_progress",
                            "session_id": sid_h,
                            "tool_id": tool_id_h,
                            "tool": tool_h,
                            "command": command_h,
                            "elapsed_ms": elapsed * 1000,
                        }),
                    )
                    .await;
            }
        })
    };
    // The tool dispatch is synchronous and several tools — most notably
    // `analyze_visual` (which posts to Ollama's `/api/generate` via the
    // **blocking** `reqwest` client and waits up to 5 min) and
    // `run_terminal` (which `wait()`s on a child process) — block the
    // calling thread for tens of seconds to minutes. We're invoked from
    // inside the LLM tool-loop closure, which runs on a Tokio worker
    // thread; without `block_in_place` that worker stops servicing the
    // socket.io engine + heartbeat for the duration, and the desktop UI
    // sees "Realtime disconnected" / "System metrics unavailable" while
    // the agent appears frozen. `block_in_place` tells Tokio to migrate
    // the rest of its tasks to other workers so the runtime keeps
    // breathing while the tool runs.
    let out = tokio::task::block_in_place(|| workspace_tools::dispatch(&ctx, name, input));
    heartbeat.abort();
    flush_tool_socket_queue(socket_queue, io, session_id);
    let duration_ms = start.elapsed().as_millis() as u64;
    let ok = !out.trim_start().to_lowercase().starts_with("error:");
    let result_chars = out.chars().count() as u64;
    let result_preview = truncate_preview(&out, 12_000);
    let shell_command = if name == "run_terminal" {
        input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| truncate_preview(s, 12_000))
    } else {
        None
    };
    let io_e = io.clone();
    let tool = name.to_string();
    let command_owned = command.clone();
    let sid = session_id.to_string();
    let tool_id_e = tool_id;
    tokio::spawn(async move {
        let payload = match shell_command {
            Some(sc) => json!({
                "kind": "tool",
                "session_id": sid,
                "tool_id": tool_id_e,
                "tool": tool,
                "command": command_owned,
                "args_preview": args_preview,
                "args_pretty": args_pretty,
                "shell_command": sc,
                "duration_ms": duration_ms,
                "ok": ok,
                "result_chars": result_chars,
                "result_preview": result_preview,
            }),
            None => json!({
                "kind": "tool",
                "session_id": sid,
                "tool_id": tool_id_e,
                "tool": tool,
                "command": command_owned,
                "args_preview": args_preview,
                "args_pretty": args_pretty,
                "duration_ms": duration_ms,
                "ok": ok,
                "result_chars": result_chars,
                "result_preview": result_preview,
            }),
        };
        let _ = io_e.emit("orchestrator", &payload).await;
    });
    out
}

async fn build_system_prompt(state: &AppState, user_message: &str) -> String {
    let mem_snippet = if state.memory.status_ok().await {
        state
            .memory
            .query(user_message)
            .await
            .unwrap_or_else(|e| format!("(memory query error: {e})"))
    } else {
        "(memory sidecar offline — start eson-memory for retrieval)".to_string()
    };

    let agent_mem_snippet = agent_memory_prompt_snippet(state.agent_memory.as_ref(), user_message);
    let user_model_block = state.agent_memory.user_model_prompt_block();
    let tool_round_cap = max_llm_tool_rounds();
    // Workspace `.learnings/` snapshot — closes the loop so prior LRN /
    // ERR / FEAT entries the agent recorded actually inform the next
    // turn, instead of being write-only journals. Caps:
    // up to 10 entries per kind, body trimmed to 600 chars each
    // (~6 KB of prompt budget worst-case, vs unbounded growth on the
    // disk file). The agent can `workspace_read .learnings/...` to see
    // full history when an entry is too big to fit here.
    let learnings_snippet = eson_agent::learnings::recent_learnings_snippet(
        state.workspace.root(),
        10,
        600,
    );

    let learnings_section = if learnings_snippet.is_empty() {
        // First-run / empty journal — nudge the agent to start writing
        // entries so the next turn has context. Without this hint a
        // brand-new install would never bootstrap the journal.
        "_(no entries yet — call **record_learning** with `kind: lrn` after notable insights, `kind: err` after recoverable failures, `kind: feat` for user feature requests. Keep summaries one sentence; put detail in `body`.)_".to_string()
    } else {
        format!(
            "The following JSON is **loaded automatically every message** (most recent first, capped per kind). Treat these as durable lessons from prior turns: re-apply protocols you previously documented, avoid mistakes you previously logged, and surface pending feature requests when relevant. If an entry's `body` is truncated with `…`, call **workspace_read** on the listed file path for the full text.\n```json\n{learnings_snippet}\n```\nWhen you finish a turn that produced a new insight (a recurring pattern, an OCR/financial heuristic, a tool-use refinement), call **record_learning** so future turns inherit it."
        )
    };

    let runtime = format!(
        "You are **Eson** in a local-first desktop app (macOS). Follow the IDENTITY, SOUL, and Eson capability sections above.\n\n\
         ### Orchestration\n\
         You may chain **many** tool calls in one turn (the host allows up to **{tool_round_cap}** sequential model↔tool rounds per user message). Prefer **finishing the job**, not stopping after a single tool: gather context, verify assumptions, fix errors, and add follow-up tools until you can answer confidently. When helpful, **anticipate** what the user would want next (e.g. list before read, recall_memory before personalized advice, run checks after edits) without waiting to be asked.\n\n\
         Use **skill_list** and **skill_run** to load  runbooks from the `skills/` directory.\n\n\
         Sandboxed workspace directory: `{}` — use **workspace_list**, **workspace_read**, and **workspace_grep** for finding files or text; pass the user’s wording — **workspace_grep** (flexible, default on) matches natural phrases to names like `snake_case` (e.g. “atd negative” finds `atd_negative`). Paths must be workspace-relative.\n\n\
         ### Agent memory (`db/memory.db`)\n\
         The following JSON is **loaded automatically every message** from keyword/recency search on the user’s text (so new chats still see prior stored memories when they match).\n\
         **store_memory** and **recall_memory** (and other tools) run when **you** invoke them — supported on **Anthropic**, **OpenAI**, **Ollama**, and any future provider wired to the same tool loop. Nothing is written to this database unless you call **store_memory** — e.g. when the user states a name, preference, or fact they want remembered for later.\n\
         Use **recall_memory** with a different `query` if you need a targeted search beyond this snippet.\n\
         ```json\n{}\n```\n\n\
         ### Self-learning journal (`workspace/.learnings/`)\n\
         {}\n\n\
         ### User model (long-lived keys in `user_model`)\n\
         ```markdown\n{}\n```\n\n\
         On Unix/macOS, **run_terminal** runs shell commands with cwd set to the workspace (see tool description for safety limits).\n\n\
         Memory sidecar (`eson_memory.db` / HTTP) snippet for this turn (may be empty if offline):\n{}",
        state.workspace.root().display(),
        agent_mem_snippet,
        learnings_section,
        user_model_block,
        mem_snippet
    );
    if state.persona_bundle.trim().is_empty() {
        runtime
    } else {
        format!(
            "{}\n\n---\n\n### Session runtime\n{}",
            state.persona_bundle.as_str(),
            runtime
        )
    }
}

/// Run the LLM for one user turn.
///
/// Behaviour:
///   1. Try the session's primary `provider`.
///   2. On *unconfigured* or *upstream/network* failure, walk the fallback
///      chain `[Anthropic, OpenAI, Ollama]` (skipping providers we already
///      tried) and retry. Each attempt emits `llm_call_begin` / `llm_call_end`
///      orchestrator events so the UI's Activity panel can show what
///      provider/model/endpoint is being called and how it ended.
///   3. If a fallback succeeds, also emit `provider_fallback` so the user
///      sees the swap.
async fn execute_llm(
    state: &AppState,
    turns: &mut Vec<ApiMessage>,
    provider: ChatProvider,
    provider_settings: &ProviderSettings,
    system: &str,
    session_id: &str,
) -> Result<String, String> {
    let mut tried: Vec<ChatProvider> = Vec::new();
    let mut last_error: Option<String> = None;

    // Per-turn cancel flag installed by `session_message`. Falls back to a
    // no-op flag for callers (e.g. background loops) that don't register
    // one — keeps the type non-optional in the hot path.
    let cancel = state
        .cancellations
        .get(session_id)
        .map(|entry| entry.value().clone())
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));

    let order = std::iter::once(provider).chain(
        [
            ChatProvider::Anthropic,
            ChatProvider::Openai,
            ChatProvider::Ollama,
        ]
        .into_iter()
        .filter(move |p| *p != provider),
    );

    for candidate in order {
        if cancel.load(Ordering::SeqCst) {
            return Err("cancelled by user".into());
        }
        if tried.contains(&candidate) {
            continue;
        }
        tried.push(candidate);

        match try_provider(state, turns, candidate, provider_settings, system, session_id, &cancel).await {
            Ok(answer) => {
                if candidate != provider {
                    orchestrator_emit(
                        state,
                        &json!({
                            "kind": "provider_fallback",
                            "session_id": session_id,
                            "requested": provider_label(provider),
                            "using": provider_label(candidate),
                            "reason": last_error.clone().unwrap_or_else(|| "primary provider unavailable".into()),
                        }),
                    )
                    .await;
                }
                return Ok(answer);
            }
            Err(e) => {
                tracing::warn!(provider = %provider_label(candidate), error = %e, "LLM call failed");
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| "no provider available".into()))
}

/// Single-attempt LLM call against a specific provider, with begin/end
/// telemetry events on the orchestrator stream.
async fn try_provider(
    state: &AppState,
    turns: &mut Vec<ApiMessage>,
    provider: ChatProvider,
    provider_settings: &ProviderSettings,
    system: &str,
    session_id: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<String, String> {
    if cancel.load(Ordering::SeqCst) {
        return Err("cancelled by user".into());
    }
    let started = std::time::Instant::now();
    // Resolve once per turn: the closures captured into each provider
    // branch take ownership of a clone, so vision routing stays consistent
    // even if `provider_settings` is mutated mid-turn (it isn't today, but
    // the borrow checker enforces this regardless).
    let vision_cfg = vision_config_for_session(provider_settings);
    // One id per LLM call (one provider attempt). Threaded through
    // begin/end + every thinking_delta so the UI can group streamed
    // reasoning chunks into the right inline step even across provider
    // fallbacks within the same turn.
    let call_id = Uuid::new_v4().to_string();
    let (model, endpoint, exec): (String, String, _) = match provider {
        ChatProvider::Anthropic => {
            let Some(claude) = anthropic_client_for_session(state, provider_settings) else {
                let err = "Anthropic not configured (ANTHROPIC_API_KEY)".to_string();
                emit_llm_call_begin(state, session_id, &call_id, provider, "?", "https://api.anthropic.com/v1/messages").await;
                emit_llm_call_end(state, session_id, &call_id, provider, "?", "https://api.anthropic.com/v1/messages", started, Err(&err)).await;
                return Err(err);
            };
            let model = claude.model().to_string();
            let endpoint = claude.endpoint().to_string();
            emit_llm_call_begin(state, session_id, &call_id, provider, &model, &endpoint).await;
            let tools = workspace_tools::tool_specs();
            let ws = state.workspace.clone();
            let mem = state.agent_memory.clone();
            let io = state.io.clone();
            let skills_dir = state.skills_dir.clone();
            let sq = state.tool_socket_queue.clone();
            let sid = session_id.to_string();
            let vc = vision_cfg.clone();
            let ec = state.embedder.clone();
            let mc = state.memory.clone();
            let cancel_tool = cancel.clone();
            let think_io = state.io.clone();
            let think_sid = session_id.to_string();
            let think_call = call_id.clone();
            let think_model = model.clone();
            let content_io = state.io.clone();
            let content_sid = session_id.to_string();
            let content_call = call_id.clone();
            let content_model = model.clone();
            let round_io = state.io.clone();
            let round_sid = session_id.to_string();
            let round_call = call_id.clone();
            let round_model = model.clone();
            let round_endpoint = endpoint.clone();
            let result = claude
                .complete_with_tools(
                    turns,
                    Some(system),
                    &tools,
                    |name, input| {
                        dispatch_agent_tool(
                            ws.as_ref(),
                            mem.as_ref(),
                            skills_dir.as_path(),
                            &sq,
                            &io,
                            &vc,
                            ec.as_ref(),
                            &mc,
                            &cancel_tool,
                            &sid,
                            name,
                            input,
                        )
                    },
                    |delta| {
                        emit_llm_thinking_delta(
                            &think_io,
                            &think_sid,
                            &think_call,
                            "anthropic",
                            &think_model,
                            delta,
                        );
                    },
                    |delta| {
                        emit_llm_content_delta(
                            &content_io,
                            &content_sid,
                            &content_call,
                            "anthropic",
                            &content_model,
                            delta,
                        );
                    },
                    |round| {
                        emit_llm_round_begin(
                            &round_io,
                            &round_sid,
                            &round_call,
                            "anthropic",
                            &round_model,
                            &round_endpoint,
                            round,
                        );
                    },
                )
                .await
                .map_err(|e| e.to_string());
            (model, endpoint, result)
        }
        ChatProvider::Openai => {
            let Some(client) = openai_client_for_session(state, provider_settings) else {
                let err = "OpenAI not configured (OPENAI_API_KEY)".to_string();
                emit_llm_call_begin(state, session_id, &call_id, provider, "?", "https://api.openai.com/v1/chat/completions").await;
                emit_llm_call_end(state, session_id, &call_id, provider, "?", "https://api.openai.com/v1/chat/completions", started, Err(&err)).await;
                return Err(err);
            };
            let model = client.model().to_string();
            let endpoint = client.endpoint();
            emit_llm_call_begin(state, session_id, &call_id, provider, &model, &endpoint).await;
            let tools = workspace_tools::tool_specs();
            let ws = state.workspace.clone();
            let mem = state.agent_memory.clone();
            let io = state.io.clone();
            let skills_dir = state.skills_dir.clone();
            let sq = state.tool_socket_queue.clone();
            let sid = session_id.to_string();
            let vc = vision_cfg.clone();
            let ec = state.embedder.clone();
            let mc = state.memory.clone();
            let cancel_tool = cancel.clone();
            let think_io = state.io.clone();
            let think_sid = session_id.to_string();
            let think_call = call_id.clone();
            let think_model = model.clone();
            let content_io = state.io.clone();
            let content_sid = session_id.to_string();
            let content_call = call_id.clone();
            let content_model = model.clone();
            let round_io = state.io.clone();
            let round_sid = session_id.to_string();
            let round_call = call_id.clone();
            let round_model = model.clone();
            let round_endpoint = endpoint.clone();
            let result = client
                .complete_with_tools(
                    turns,
                    Some(system),
                    &tools,
                    |name, input| {
                        dispatch_agent_tool(
                            ws.as_ref(),
                            mem.as_ref(),
                            skills_dir.as_path(),
                            &sq,
                            &io,
                            &vc,
                            ec.as_ref(),
                            &mc,
                            &cancel_tool,
                            &sid,
                            name,
                            input,
                        )
                    },
                    |delta| {
                        emit_llm_thinking_delta(
                            &think_io,
                            &think_sid,
                            &think_call,
                            "openai",
                            &think_model,
                            delta,
                        );
                    },
                    |delta| {
                        emit_llm_content_delta(
                            &content_io,
                            &content_sid,
                            &content_call,
                            "openai",
                            &content_model,
                            delta,
                        );
                    },
                    |round| {
                        emit_llm_round_begin(
                            &round_io,
                            &round_sid,
                            &round_call,
                            "openai",
                            &round_model,
                            &round_endpoint,
                            round,
                        );
                    },
                )
                .await
                .map_err(|e| e.to_string());
            (model, endpoint, result)
        }
        ChatProvider::Ollama => {
            let Some(client) = ollama_client_for_session(state, provider_settings) else {
                let err = "Ollama not configured (OLLAMA_BASE_URL / OLLAMA_MODEL)".to_string();
                emit_llm_call_begin(state, session_id, &call_id, provider, "?", "http://127.0.0.1:11434").await;
                emit_llm_call_end(state, session_id, &call_id, provider, "?", "http://127.0.0.1:11434", started, Err(&err)).await;
                return Err(err);
            };
            let model = client.model().to_string();
            let endpoint = client.endpoint();
            emit_llm_call_begin(state, session_id, &call_id, provider, &model, &endpoint).await;
            // Fast-fail on an unreachable Ollama host (short probe: GET /v1/models,
            // then GET /) instead of letting the full `ESON_LLM_HTTP_TIMEOUT_SECS`
            // (default 10 min) reqwest timeout block fallback.
            if !probe_ollama(&client).await {
                let err = format!(
                    "Ollama unreachable (reachability check: GET /v1/models then / — no response within ~5 s). Configured API: {endpoint}"
                );
                emit_llm_call_end(state, session_id, &call_id, provider, &model, &endpoint, started, Err(&err)).await;
                return Err(err);
            }
            let tools = workspace_tools::tool_specs();
            let ws = state.workspace.clone();
            let mem = state.agent_memory.clone();
            let io = state.io.clone();
            let skills_dir = state.skills_dir.clone();
            let sq = state.tool_socket_queue.clone();
            let sid = session_id.to_string();
            let vc = vision_cfg.clone();
            let ec = state.embedder.clone();
            let mc = state.memory.clone();
            let cancel_tool = cancel.clone();
            let think_io = state.io.clone();
            let think_sid = session_id.to_string();
            let think_call = call_id.clone();
            let think_model = model.clone();
            let content_io = state.io.clone();
            let content_sid = session_id.to_string();
            let content_call = call_id.clone();
            let content_model = model.clone();
            let round_io = state.io.clone();
            let round_sid = session_id.to_string();
            let round_call = call_id.clone();
            let round_model = model.clone();
            let round_endpoint = endpoint.clone();
            let result = client
                .complete_with_tools(
                    turns,
                    Some(system),
                    &tools,
                    |name, input| {
                        dispatch_agent_tool(
                            ws.as_ref(),
                            mem.as_ref(),
                            skills_dir.as_path(),
                            &sq,
                            &io,
                            &vc,
                            ec.as_ref(),
                            &mc,
                            &cancel_tool,
                            &sid,
                            name,
                            input,
                        )
                    },
                    |delta| {
                        emit_llm_thinking_delta(
                            &think_io,
                            &think_sid,
                            &think_call,
                            "ollama",
                            &think_model,
                            delta,
                        );
                    },
                    |delta| {
                        emit_llm_content_delta(
                            &content_io,
                            &content_sid,
                            &content_call,
                            "ollama",
                            &content_model,
                            delta,
                        );
                    },
                    |round| {
                        emit_llm_round_begin(
                            &round_io,
                            &round_sid,
                            &round_call,
                            "ollama",
                            &round_model,
                            &round_endpoint,
                            round,
                        );
                    },
                )
                .await
                .map_err(|e| e.to_string());
            (model, endpoint, result)
        }
    };
    emit_llm_call_end(state, session_id, &call_id, provider, &model, &endpoint, started, exec.as_deref().map_err(String::as_str)).await;
    exec
}

/// Stream a single reasoning chunk to the UI.
///
/// Fired for every `thinking_delta` (Anthropic) / `reasoning_content`
/// delta (OpenAI o1+) / `<think>` chunk (Ollama, deepseek-r1, qwq, …) as
/// they arrive on the SSE stream. The frontend appends each delta to the
/// current LLM call's "thinking" step in the inline reasoning panel,
/// keyed by `call_id` so a chunk that races a `provider_fallback` lands
/// in the right place.
///
/// Synchronous from the LLM client's perspective (the closure is
/// `FnMut`), so we spawn the actual `socket.io` emit on the tokio runtime.
fn emit_llm_thinking_delta(
    io: &SocketIo,
    session_id: &str,
    call_id: &str,
    provider: &str,
    model: &str,
    delta: &str,
) {
    if delta.is_empty() {
        return;
    }
    let payload = json!({
        "kind": "llm_thinking_delta",
        "session_id": session_id,
        "call_id": call_id,
        "provider": provider,
        "model": model,
        "delta": delta,
    });
    let io = io.clone();
    tokio::spawn(async move {
        let _ = io.emit("orchestrator", &payload).await;
    });
}

/// Stream a single chunk of the **user-visible answer** to the UI.
///
/// Fired for every `text_delta` (Anthropic) / non-`<think>` `delta.content`
/// (OpenAI/Ollama) on the SSE stream. The frontend appends each delta to
/// the in-flight assistant bubble so the chat fills *as the model writes*
/// instead of waiting for `turn_end` (a slow local round can take minutes
/// after the last `</think>`, which previously left the UI stuck on
/// "Working…" the whole time).
///
/// Same shape and same `call_id` semantics as `emit_llm_thinking_delta`,
/// just a different `kind` so the UI can route content vs. reasoning.
fn emit_llm_content_delta(
    io: &SocketIo,
    session_id: &str,
    call_id: &str,
    provider: &str,
    model: &str,
    delta: &str,
) {
    if delta.is_empty() {
        return;
    }
    let payload = json!({
        "kind": "llm_content_delta",
        "session_id": session_id,
        "call_id": call_id,
        "provider": provider,
        "model": model,
        "delta": delta,
    });
    let io = io.clone();
    tokio::spawn(async move {
        let _ = io.emit("orchestrator", &payload).await;
    });
}

/// Per-round signal — fires every time the LLM client opens a new HTTP
/// streaming request inside `complete_with_tools`. Lets the UI show
/// "API → ollama · gemma4:e4b · round 2" so the user can see *something*
/// happening between a tool returning and the next batch of deltas
/// arriving (which on a slow local model can be 30-90 s of pure CPU
/// burn with no visible output).
fn emit_llm_round_begin(
    io: &SocketIo,
    session_id: &str,
    call_id: &str,
    provider: &str,
    model: &str,
    endpoint: &str,
    round: u32,
) {
    // Round 1 is already announced by `emit_llm_call_begin` — suppress
    // the duplicate to keep the Activity panel clean. The signal still
    // matters for round ≥ 2, which is exactly the silent gap after a
    // tool returns that the user sees as "stuck".
    if round <= 1 {
        return;
    }
    let payload = json!({
        "kind": "llm_round_begin",
        "session_id": session_id,
        "call_id": call_id,
        "provider": provider,
        "model": model,
        "endpoint": endpoint,
        "round": round,
    });
    let io = io.clone();
    tokio::spawn(async move {
        let _ = io.emit("orchestrator", &payload).await;
    });
}

async fn emit_llm_call_begin(
    state: &AppState,
    session_id: &str,
    call_id: &str,
    provider: ChatProvider,
    model: &str,
    endpoint: &str,
) {
    orchestrator_emit(
        state,
        &json!({
            "kind": "llm_call_begin",
            "session_id": session_id,
            "call_id": call_id,
            "provider": provider_label(provider),
            "model": model,
            "endpoint": endpoint,
        }),
    )
    .await;
}

async fn emit_llm_call_end(
    state: &AppState,
    session_id: &str,
    call_id: &str,
    provider: ChatProvider,
    model: &str,
    endpoint: &str,
    started: std::time::Instant,
    result: Result<&str, &str>,
) {
    let duration_ms = started.elapsed().as_millis();
    let payload = match result {
        Ok(_) => json!({
            "kind": "llm_call_end",
            "session_id": session_id,
            "call_id": call_id,
            "provider": provider_label(provider),
            "model": model,
            "endpoint": endpoint,
            "duration_ms": duration_ms,
            "ok": true,
        }),
        Err(err) => json!({
            "kind": "llm_call_end",
            "session_id": session_id,
            "call_id": call_id,
            "provider": provider_label(provider),
            "model": model,
            "endpoint": endpoint,
            "duration_ms": duration_ms,
            "ok": false,
            "error": err,
        }),
    };
    orchestrator_emit(state, &payload).await;
}

fn workspace_rel_path(state: &AppState, abs: &std::path::Path) -> Option<String> {
    abs.strip_prefix(state.workspace.root())
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

async fn run_background_llm_turn(state: &AppState, session_id: &str, user_text: &str) -> Result<(), String> {
    {
        let mut s = state
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| "session missing".to_string())?;
        s.messages.push(ApiMessage {
            role: "user".into(),
            content: json!(user_text),
        });
    }
    let mut turns = state
        .sessions
        .get(session_id)
        .ok_or_else(|| "session missing".to_string())?
        .messages
        .clone();
    let provider = state
        .sessions
        .get(session_id)
        .ok_or_else(|| "session missing".to_string())?
        .provider;
    let settings = state
        .sessions
        .get(session_id)
        .ok_or_else(|| "session missing".to_string())?
        .settings
        .clone();
    if !provider_can_message(state, provider, &settings) {
        return Err("background provider not configured".into());
    }
    let system = build_system_prompt(state, user_text).await;
    let answer = execute_llm(state, &mut turns, provider, &settings, &system, session_id).await?;
    if let Some(mut s) = state.sessions.get_mut(session_id) {
        s.messages = turns;
    }
    let _ = answer;
    Ok(())
}

async fn background_loops(state: AppState) {
    tracing::info!(
        "Background loop runtime ready (toggle via Settings panel or ESON_BACKGROUND_LOOP_ENABLED)"
    );
    let rt = tokio::runtime::Handle::current();
    // Always spawn the inbox watcher so toggling auto-process at runtime takes
    // effect without restarting; per-event dispatch checks `inbox_auto_resolved`.
    let st_inbox = state.clone();
    let h = rt.clone();
    std::thread::spawn(move || inbox_notify_thread(st_inbox, h));
    loop {
        let cfg_pre = state
            .background_config
            .read()
            .ok()
            .map(|g| g.clone())
            .unwrap_or_default();
        tokio::time::sleep(cfg_pre.heartbeat_resolved()).await;
        let cfg = state
            .background_config
            .read()
            .ok()
            .map(|g| g.clone())
            .unwrap_or_default();
        if !cfg.loop_enabled_resolved() {
            continue;
        }
        let ws_root = state.workspace.root().to_path_buf();
        let skills_root = state.skills_dir.clone();
        for skill in agent_loop::due_cron_skills(&ws_root, &skills_root) {
            let Some((provider, settings)) = resolve_background_provider(&state) else {
                tracing::warn!(
                    skill = %skill.id,
                    "no background provider available (configure Anthropic, OpenAI or Ollama) \u{2014} skipping cron"
                );
                continue;
            };
            let sid = format!("background-{}", Uuid::new_v4());
            state.sessions.insert(
                sid.clone(),
                SessionState {
                    provider,
                    settings: settings.clone(),
                    ..Default::default()
                },
            );

            // The 12h memory consolidation skill gets a richer prompt:
            // the orchestrator pre-builds the evidence bundle so the
            // LLM turn is deterministic and bounded. Every other cron
            // skill keeps the legacy "run skill_run then follow the
            // markdown" message.
            let is_consolidation = skill.id == CONSOLIDATION_SKILL_ID;
            let (msg, considered) = if is_consolidation {
                let events = snapshot_events_since(&state, DEFAULT_WINDOW_SECS);
                let chat = snapshot_recent_chat(&state, 60);
                let bundle = build_digest_bundle(
                    state.workspace.root(),
                    state.agent_memory.as_ref(),
                    &events,
                    &chat,
                    DEFAULT_WINDOW_SECS,
                );
                let considered_total = bundle.counts.events
                    + bundle.counts.chat_turns
                    + bundle.counts.artifacts
                    + bundle.counts.learnings;

                orchestrator_emit(
                    &state,
                    &json!({
                        "kind": "consolidation_begin",
                        "session_id": sid,
                        "skill_id": skill.id,
                        "provider": provider_label(provider),
                        "window_hours": DEFAULT_WINDOW_SECS / 3600,
                        "window_start_ms": bundle.window_start_ms,
                        "window_end_ms": bundle.window_end_ms,
                        "considered_total": considered_total,
                        "considered": {
                            "events": bundle.counts.events,
                            "chat_turns": bundle.counts.chat_turns,
                            "artifacts": bundle.counts.artifacts,
                            "learnings": bundle.counts.learnings,
                            "stored_snapshot": bundle.counts.stored_snapshot,
                        },
                        "empty": bundle.empty,
                    }),
                )
                .await;

                // Skip the LLM round entirely when nothing durable
                // happened — the skill contract already says to reply
                // "No action" in that case, so we save the round trip.
                if bundle.empty {
                    orchestrator_emit(
                        &state,
                        &json!({
                            "kind": "consolidation_end",
                            "session_id": sid,
                            "skill_id": skill.id,
                            "status": "skipped_empty_window",
                            "considered_total": considered_total,
                            "kept": 0,
                        }),
                    )
                    .await;
                    state.sessions.remove(&sid);
                    agent_loop::after_cron_run(&skill, &ws_root);
                    continue;
                }
                (build_llm_message(&bundle), considered_total)
            } else {
                let msg = format!(
                    "Background cron: execute skill `{}`. Call **skill_run** with skill_id `{}` then follow the markdown.",
                    skill.id, skill.id
                );
                (msg, 0)
            };

            if !is_consolidation {
                orchestrator_emit(
                    &state,
                    &json!({
                        "kind": "background_turn",
                        "trigger": "cron",
                        "skill_id": skill.id,
                        "session_id": sid,
                        "provider": provider_label(provider),
                    }),
                )
                .await;
            }

            // Snapshot store_memory / record_learning counts BEFORE the
            // turn so we can compute "kept" by diffing after it returns.
            let (pre_mem, pre_lrn) = if is_consolidation {
                (
                    count_memory_rows(&state),
                    count_learning_entries(&state),
                )
            } else {
                (0, 0)
            };

            let _permit = state.policy.acquire_tool_permit().await;
            let turn_result = run_background_llm_turn(&state, &sid, &msg).await;
            if let Err(ref e) = turn_result {
                tracing::warn!(error = %e, skill = %skill.id, "background cron LLM failed");
            }

            if is_consolidation {
                let kept_mem = count_memory_rows(&state).saturating_sub(pre_mem);
                let kept_lrn = count_learning_entries(&state).saturating_sub(pre_lrn);
                let status = match &turn_result {
                    Ok(_) => "ok",
                    Err(_) => "llm_error",
                };
                orchestrator_emit(
                    &state,
                    &json!({
                        "kind": "consolidation_end",
                        "session_id": sid,
                        "skill_id": skill.id,
                        "status": status,
                        "considered_total": considered,
                        "kept": kept_mem + kept_lrn,
                        "kept_memory": kept_mem,
                        "kept_learnings": kept_lrn,
                    }),
                )
                .await;
            }

            state.sessions.remove(&sid);
            agent_loop::after_cron_run(&skill, &ws_root);
        }
    }
}

fn inbox_notify_thread(state: AppState, rt: tokio::runtime::Handle) {
    use notify::{RecursiveMode, Watcher};
    use std::collections::HashMap;
    use std::time::Instant;

    let inbox = state.workspace.root().join("inbox");
    let _ = std::fs::create_dir_all(&inbox);
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = match notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    }) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(%e, "recommended_watcher failed");
            return;
        }
    };
    if let Err(e) = watcher.watch(&inbox, RecursiveMode::NonRecursive) {
        tracing::error!(%e, "watch inbox");
        return;
    }
    let mut last: HashMap<String, Instant> = HashMap::new();
    for res in rx {
        let Ok(event) = res else { continue };
        let cfg = state
            .background_config
            .read()
            .ok()
            .map(|g| g.clone())
            .unwrap_or_default();
        if !cfg.loop_enabled_resolved() || !cfg.inbox_auto_resolved() {
            continue;
        }
        let deb = cfg.debounce_resolved();
        for p in event.paths {
            if !p.is_file() {
                continue;
            }
            let Some(rel) = workspace_rel_path(&state, &p) else {
                continue;
            };
            if !rel.starts_with("inbox/") {
                continue;
            }
            let now = Instant::now();
            if last
                .get(&rel)
                .map(|t| now.duration_since(*t) < deb)
                .unwrap_or(false)
            {
                continue;
            }
            last.insert(rel.clone(), now);
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let Some(skill) = (if ext.is_empty() {
                None
            } else {
                match_inbox_skill(&state.skills_dir, &ext)
            }) else {
                continue;
            };
            let st = state.clone();
            let rel_c = rel.clone();
            let skill_id = skill.id.clone();
            rt.spawn(async move {
                let _permit = st.policy.acquire_tool_permit().await;
                let Some((provider, settings)) = resolve_background_provider(&st) else {
                    tracing::warn!(
                        path = %rel_c,
                        "no background provider available \u{2014} skipping inbox event"
                    );
                    let _ = finalize_inbox_dispatch(
                        &st,
                        &rel_c,
                        &skill_id,
                        false,
                        Some("no background provider configured".into()),
                    )
                    .await;
                    return;
                };
                let sid = format!("background-{}", Uuid::new_v4());
                st.sessions.insert(
                    sid.clone(),
                    SessionState {
                        provider,
                        settings,
                        ..Default::default()
                    },
                );
                let basename = std::path::Path::new(&rel_c)
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| rel_c.clone());
                let msg = format!(
                    "Background inbox dispatch.\n\n\
                     Source file: `{rel_c}`\n\
                     Basename: `{basename}` (use this for any report file names)\n\
                     Skill: `{skill_id}` — call **skill_run** with id `{skill_id}` directly (no need to list first), then follow the body.\n\n\
                     The orchestrator will move the source to `inbox/processed/<today>/` (or `inbox/failed/<today>/` on error) and append a one-liner to today's digest at `exports/reports/digest-<today>.md` — do NOT move or delete the source file yourself, and do NOT write the digest. Focus on producing the report and any artifacts. Reply with one short line summarizing what you did."
                );
                let _ = orchestrator_emit(
                    &st,
                    &json!({
                        "kind": "background_turn",
                        "trigger": "inbox",
                        "path": rel_c,
                        "skill_id": skill_id,
                        "session_id": sid,
                        "provider": provider_label(provider),
                    }),
                )
                .await;
                let result = run_background_llm_turn(&st, &sid, &msg).await;
                let success = result.is_ok();
                let err_msg = result.as_ref().err().cloned();
                if let Err(e) = result {
                    tracing::warn!(error = %e, path = %rel_c, "inbox background LLM failed");
                }
                let _ = finalize_inbox_dispatch(
                    &st,
                    &rel_c,
                    &skill_id,
                    success,
                    err_msg,
                )
                .await;
                st.sessions.remove(&sid);
            });
        }
    }
}

/// Post-skill hook: move the source file out of `inbox/` into a dated
/// `inbox/processed/` (success) or `inbox/failed/` (error) subfolder, and
/// append a one-liner to the day's digest under `exports/reports/`. Returns
/// the new workspace-relative path on success, or `None` if the source has
/// vanished or the move failed.
async fn finalize_inbox_dispatch(
    state: &AppState,
    rel_path: &str,
    skill_id: &str,
    success: bool,
    err: Option<String>,
) -> Option<String> {
    let root = state.workspace.root().to_path_buf();
    let src_abs = root.join(rel_path);
    if !src_abs.is_file() {
        // Source already moved/removed (e.g. the skill body cleaned up before
        // we made this hook idempotent). Still record the digest line so the
        // user has a trace.
        let dest_rel: Option<String> = None;
        let _ = append_inbox_digest(&root, rel_path, skill_id, success, dest_rel.as_deref(), err.as_deref());
        let _ = orchestrator_emit(
            state,
            &json!({
                "kind": "inbox_finalize",
                "path": rel_path,
                "skill_id": skill_id,
                "success": success,
                "moved_to": serde_json::Value::Null,
                "note": "source file already gone",
            }),
        )
        .await;
        return None;
    }

    let now_local = chrono::Local::now();
    let date = now_local.format("%Y-%m-%d").to_string();
    let bucket = if success { "processed" } else { "failed" };
    let file_name = match src_abs.file_name() {
        Some(n) => n.to_string_lossy().to_string(),
        None => return None,
    };
    let dest_dir = root.join("inbox").join(bucket).join(&date);
    if let Err(e) = std::fs::create_dir_all(&dest_dir) {
        tracing::warn!(error = %e, "inbox finalize: create dest dir");
        return None;
    }
    let dest_path = unique_dest_path(&dest_dir, &file_name);
    if let Err(e) = move_file_cross_fs(&src_abs, &dest_path) {
        tracing::warn!(error = %e, src = %src_abs.display(), dst = %dest_path.display(), "inbox finalize: move failed");
        return None;
    }
    let dest_rel = dest_path
        .strip_prefix(&root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"));

    if let Err(e) = append_inbox_digest(
        &root,
        rel_path,
        skill_id,
        success,
        dest_rel.as_deref(),
        err.as_deref(),
    ) {
        tracing::warn!(error = %e, "inbox finalize: digest append failed");
    }

    let _ = orchestrator_emit(
        state,
        &json!({
            "kind": "inbox_finalize",
            "path": rel_path,
            "skill_id": skill_id,
            "success": success,
            "moved_to": dest_rel.clone(),
            "error": err,
        }),
    )
    .await;

    dest_rel
}

/// Pick `dir/name`; if it exists, append ` (N)` before the extension until a
/// free slot is found. Caps at 999 attempts to avoid pathological loops.
fn unique_dest_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let p = std::path::Path::new(name);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    for n in 1..1000 {
        let c = dir.join(format!("{stem} ({n}){ext}"));
        if !c.exists() {
            return c;
        }
    }
    dir.join(name)
}

/// `rename` first (atomic on same FS), fall back to copy + remove for
/// cross-filesystem moves.
fn move_file_cross_fs(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    if std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dest)?;
    std::fs::remove_file(src)
}

/// Append one line to `exports/reports/digest-<today>.md`. Creates parents +
/// header on first write of the day.
fn append_inbox_digest(
    workspace_root: &std::path::Path,
    rel_path: &str,
    skill_id: &str,
    success: bool,
    moved_to: Option<&str>,
    err: Option<&str>,
) -> std::io::Result<()> {
    use std::io::Write;
    let now = chrono::Local::now();
    let date = now.format("%Y-%m-%d").to_string();
    let time = now.format("%H:%M").to_string();
    let digest_dir = workspace_root.join("exports").join("reports");
    std::fs::create_dir_all(&digest_dir)?;
    let path = digest_dir.join(format!("digest-{date}.md"));
    let header_needed = !path.exists();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if header_needed {
        writeln!(
            f,
            "# Inbox digest · {date}\n\n_Generated by the Eson background loop. One line per inbox dispatch._\n"
        )?;
    }
    let line = if success {
        format!(
            "- [{time}] ✓ `{rel_path}` → `{}` ({skill_id})",
            moved_to.unwrap_or("(in place)"),
        )
    } else {
        let reason = err.unwrap_or("unknown error");
        let dest = moved_to.unwrap_or("(not moved)");
        format!(
            "- [{time}] ✗ `{rel_path}` → `{dest}` ({skill_id}) — {reason}",
        )
    };
    writeln!(f, "{line}")?;
    Ok(())
}

async fn session_message(
    State(state): State<AppState>,
    Json(body): Json<SessionMessage>,
) -> Result<Json<SessionMessageResp>, MsgErr> {
    if !state.sessions.contains_key(&body.session_id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        ));
    }

    let _permit = state.policy.acquire_tool_permit().await;

    {
        let mut s = state.sessions.get_mut(&body.session_id).ok_or((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        ))?;
        s.messages.push(ApiMessage {
            role: "user".into(),
            content: json!(body.message),
        });
        if let Some(patch) = body.settings {
            merge_provider_settings(&mut s.settings, patch);
        }
        // Apply the per-message primary override before we read `s.provider`
        // below. Without this, the session is forever pinned to whatever
        // provider it was created with and changing the primary in the UI
        // is silently ignored until the user starts a new chat.
        if let Some(new_provider) = body.provider {
            s.provider = new_provider;
        }
    }

    let turns = state
        .sessions
        .get(&body.session_id)
        .map(|s| s.messages.clone())
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        ))?;
    let provider = state
        .sessions
        .get(&body.session_id)
        .map(|s| s.provider)
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        ))?;
    let provider_settings = state
        .sessions
        .get(&body.session_id)
        .map(|s| s.settings.clone())
        .ok_or((
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session not found" })),
        ))?;
    if !provider_can_message(&state, provider, &provider_settings) {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": format!("provider `{provider:?}` is not configured for this session") })),
        ));
    }

    let system = build_system_prompt(&state, &body.message).await;

    // Reset / install a fresh cancellation flag for this turn. The previous
    // turn's flag (if any) is replaced — we never resurrect a stale cancel.
    let cancel_flag = Arc::new(AtomicBool::new(false));
    state
        .cancellations
        .insert(body.session_id.clone(), cancel_flag.clone());

    let turn_id = Uuid::new_v4().to_string();
    let orchestration_start = std::time::Instant::now();
    orchestrator_emit(
        &state,
        &json!({
            "kind": "turn_begin",
            "id": &turn_id,
            "session_id": &body.session_id,
            "provider": provider_label(provider),
            "session_prefix": body.session_id.chars().take(8).collect::<String>(),
            "user_preview": truncate_preview(&body.message, 220),
            "user_chars": body.message.chars().count(),
        }),
    )
    .await;

    // Detach the orchestration into a `tokio::spawn` so the WebKit
    // ~60 s fetch timeout doesn't kill an in-flight Round 2+ when the
    // browser drops the long POST. Axum cancels a request future when
    // the client disconnects; that previously cascaded into dropping
    // `execute_llm` mid-stream and stranding the UI on "Working…".
    //
    // The spawned future owns its own clones, so dropping the join
    // handle (which happens when axum cancels us) does NOT cancel the
    // task — it keeps running, fills the chat bubble via
    // `llm_content_delta`, and emits `turn_end` over the socket. Fast
    // turns still get a normal JSON response because we await the join
    // handle below.
    let bg_state = state.clone();
    let bg_session_id = body.session_id.clone();
    let bg_turn_id = turn_id.clone();
    let bg_cancel = cancel_flag.clone();
    let bg_handle = tokio::spawn(async move {
        run_session_turn(
            bg_state,
            bg_session_id,
            provider,
            provider_settings,
            system,
            turns,
            bg_turn_id,
            orchestration_start,
            bg_cancel,
        )
        .await
    });

    match bg_handle.await {
        Ok(Ok(answer)) => Ok(Json(SessionMessageResp {
            answer,
            session_id: body.session_id,
        })),
        Ok(Err((status, err_text))) => {
            Err((status, Json(json!({ "error": err_text }))))
        }
        Err(join_err) => {
            // The spawned task panicked — surface a 5xx but the
            // socket still got a `turn_error` from the panic-catch
            // path inside the task, so the UI clears its placeholder.
            tracing::error!(%join_err, "orchestrator task panicked");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("orchestrator panicked: {join_err}") })),
            ))
        }
    }
}

/// Runs one user turn end-to-end: invokes `execute_llm`, persists the
/// resulting message tape back into `state.sessions`, and emits the
/// terminal `turn_end` / `turn_error` / `turn_cancel` socket event.
///
/// Lives in its own function so [`session_message`] can hand the work
/// off to `tokio::spawn` — see the call site for the "survive HTTP
/// cancellation" rationale.
#[allow(clippy::too_many_arguments)]
async fn run_session_turn(
    state: AppState,
    session_id: String,
    provider: ChatProvider,
    provider_settings: ProviderSettings,
    system: String,
    mut turns: Vec<ApiMessage>,
    turn_id: String,
    orchestration_start: std::time::Instant,
    cancel_flag: Arc<AtomicBool>,
) -> Result<String, (StatusCode, String)> {
    let answer = match execute_llm(
        &state,
        &mut turns,
        provider,
        &provider_settings,
        &system,
        &session_id,
    )
    .await
    {
        Ok(a) => a,
        Err(e) => {
            // If cancellation flipped the flag, prefer a friendly error so
            // the client can show "Stopped" rather than a confusing 502.
            let cancelled = cancel_flag.load(Ordering::SeqCst);
            let (status, err_text) = if cancelled {
                (StatusCode::OK, "cancelled by user".to_string())
            } else if e.contains("not configured") {
                (StatusCode::SERVICE_UNAVAILABLE, e.clone())
            } else {
                (StatusCode::BAD_GATEWAY, e.clone())
            };
            tracing::error!(%e, cancelled, "LLM complete failed");
            rollback_session_user_turn(&state, &session_id);
            state.cancellations.remove(&session_id);
            orchestrator_emit(
                &state,
                &json!({
                    "kind": if cancelled { "turn_cancel" } else { "turn_error" },
                    "id": &turn_id,
                    "session_id": &session_id,
                    "duration_ms": orchestration_start.elapsed().as_millis(),
                    "error": &err_text,
                }),
            )
            .await;
            return Err((status, err_text));
        }
    };

    state.cancellations.remove(&session_id);

    if let Some(mut s) = state.sessions.get_mut(&session_id) {
        s.messages = turns;
    } else {
        // Session was deleted mid-turn (rare). The answer is already in
        // the bubble via `llm_content_delta`; just emit `turn_end` so
        // the placeholder clears even though we can't persist the tape.
        tracing::warn!(%session_id, "session removed before turn could persist");
    }

    orchestrator_emit(
        &state,
        &json!({
            "kind": "turn_end",
            "id": &turn_id,
            "session_id": &session_id,
            "duration_ms": orchestration_start.elapsed().as_millis(),
            "answer_chars": answer.chars().count(),
            "ok": true,
            "answer_preview": truncate_preview(&answer, 320),
            "answer": &answer,
        }),
    )
    .await;

    Ok(answer)
}

fn rollback_session_user_turn(state: &AppState, session_id: &str) {
    if let Some(mut s) = state.sessions.get_mut(session_id) {
        s.messages.pop();
    }
}

fn provider_can_message(
    state: &AppState,
    provider: ChatProvider,
    settings: &ProviderSettings,
) -> bool {
    match provider {
        ChatProvider::Anthropic => anthropic_client_for_session(state, settings).is_some(),
        ChatProvider::Openai => openai_client_for_session(state, settings).is_some(),
        ChatProvider::Ollama => ollama_client_for_session(state, settings).is_some(),
    }
}

fn provider_start_available(state: &AppState, p: ChatProvider, s: &ProviderSettings) -> bool {
    match p {
        ChatProvider::Anthropic => {
            state.anthropic.is_some()
                || s.anthropic
                    .as_ref()
                    .and_then(|x| x.api_key.as_ref())
                    .is_some_and(|k| !k.is_empty())
        }
        ChatProvider::Openai => {
            state.openai.is_some()
                || s.openai
                    .as_ref()
                    .and_then(|x| x.api_key.as_ref())
                    .is_some_and(|k| !k.is_empty())
        }
        ChatProvider::Ollama => state.ollama.is_some() || s.ollama.is_some(),
    }
}

fn anthropic_client_for_session(
    state: &AppState,
    settings: &ProviderSettings,
) -> Option<AnthropicClient> {
    let timeout = eson_agent::llm::resolve_http_timeout(settings.http_timeout_secs);
    if let Some(o) = settings.anthropic.clone() {
        let base = AnthropicConfig::from_env().unwrap_or(AnthropicConfig {
            api_key: String::new(),
            model: "claude-haiku-4-5-20251001".to_string(),
            max_tokens: 8192,
        });
        let api_key = o.api_key.filter(|s| !s.is_empty()).unwrap_or(base.api_key);
        let model = o.model.filter(|s| !s.is_empty()).unwrap_or(base.model);
        if api_key.is_empty() {
            return None;
        }
        return Some(AnthropicClient::new_with_timeout(
            AnthropicConfig {
                api_key,
                model,
                max_tokens: base.max_tokens,
            },
            timeout,
        ));
    }
    // No per-session overrides — but the user may still have bumped the
    // per-request timeout under Settings → Advanced. Rebuild the cached
    // startup client with the new timeout if so; otherwise hand back the
    // shared one to avoid pointless reqwest::Client churn.
    if settings.http_timeout_secs.is_some() {
        let cfg = AnthropicConfig::from_env()?;
        return Some(AnthropicClient::new_with_timeout(cfg, timeout));
    }
    state.anthropic.clone()
}

fn openai_client_for_session(
    state: &AppState,
    settings: &ProviderSettings,
) -> Option<OpenAiCompatClient> {
    let timeout = eson_agent::llm::resolve_http_timeout(settings.http_timeout_secs);
    if let Some(o) = settings.openai.clone() {
        let base = OpenAiCompatConfig::from_openai_env().unwrap_or(OpenAiCompatConfig {
            api_key: String::new(),
            model: "gpt-4o-mini".to_string(),
            max_tokens: 8192,
            base_url: "https://api.openai.com/v1".to_string(),
        });
        let api_key = o.api_key.filter(|s| !s.is_empty()).unwrap_or(base.api_key);
        let model = o.model.filter(|s| !s.is_empty()).unwrap_or(base.model);
        if api_key.is_empty() {
            return None;
        }
        return Some(OpenAiCompatClient::new_with_timeout(
            OpenAiCompatConfig {
                api_key,
                model,
                max_tokens: base.max_tokens,
                base_url: base.base_url,
            },
            timeout,
        ));
    }
    if settings.http_timeout_secs.is_some() {
        let cfg = OpenAiCompatConfig::from_openai_env()?;
        return Some(OpenAiCompatClient::new_with_timeout(cfg, timeout));
    }
    state.openai.clone()
}

fn ollama_client_for_session(
    state: &AppState,
    settings: &ProviderSettings,
) -> Option<OpenAiCompatClient> {
    let timeout = eson_agent::llm::resolve_http_timeout(settings.http_timeout_secs);
    if let Some(o) = settings.ollama.clone() {
        let base = OpenAiCompatConfig::from_ollama_env().unwrap_or(OpenAiCompatConfig {
            api_key: "ollama".to_string(),
            model: "gemma4:e4b".to_string(),
            max_tokens: 8192,
            base_url: "http://127.0.0.1:11434/v1".to_string(),
        });
        let model = o.model.filter(|s| !s.is_empty()).unwrap_or(base.model);
        let base_url = o
            .url
            .filter(|s| !s.is_empty())
            .map(|u| {
                let trimmed = u.trim_end_matches('/');
                if trimmed.ends_with("/v1") {
                    trimmed.to_string()
                } else {
                    format!("{trimmed}/v1")
                }
            })
            .unwrap_or(base.base_url);
        return Some(OpenAiCompatClient::new_with_timeout(
            OpenAiCompatConfig {
                api_key: base.api_key,
                model,
                max_tokens: base.max_tokens,
                base_url,
            },
            timeout,
        ));
    }
    if settings.http_timeout_secs.is_some() {
        let cfg = OpenAiCompatConfig::from_ollama_env()?;
        return Some(OpenAiCompatClient::new_with_timeout(cfg, timeout));
    }
    state.ollama.clone()
}

/// `http://host[:port]` or `https://…` for a reachability fallback when a
/// server does not implement OpenAI `GET /v1/models` (common on some LAN MLX /
/// custom stacks that still serve `POST /v1/chat/completions`).
fn http_scheme_and_authority(url: &str) -> Option<String> {
    const HTTP: &str = "http://";
    const HTTPS: &str = "https://";
    let (prefix, rest) = if let Some(r) = url.strip_prefix(HTTP) {
        (HTTP, r)
    } else if let Some(r) = url.strip_prefix(HTTPS) {
        (HTTPS, r)
    } else {
        return None;
    };
    let authority = if let Some(i) = rest.find('/') {
        &rest[..i]
    } else {
        rest
    };
    if authority.is_empty() {
        return None;
    }
    Some(format!("{prefix}{authority}"))
}

/// Quick TCP/HTTP reachability probe for an Ollama base URL.
///
/// Used to flip a configured-but-unreachable Ollama from `ready=true` to
/// `ready=false` so the UI can pre-emptively pick a different provider
/// (otherwise the user waits the full `ESON_LLM_HTTP_TIMEOUT_SECS`, default
/// 600 s, for `reqwest`'s timeout to fire before our chat-time fallback
/// kicks in).
async fn probe_ollama(client: &OpenAiCompatClient) -> bool {
    let chat_endpoint = client.endpoint();
    let base = chat_endpoint
        .trim_end_matches("/chat/completions")
        .trim_end_matches('/');

    let http = match reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_millis(2500))
        .timeout(std::time::Duration::from_millis(5000))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    let mut candidates: Vec<String> = vec![format!("{base}/models")];
    if let Some(origin) = http_scheme_and_authority(&chat_endpoint) {
        let root = format!("{origin}/");
        if !candidates.iter().any(|u| u == &root) {
            candidates.push(root);
        }
    }
    // Any HTTP response (200, 401, 404, …) means the host answered. Only
    // transport-level errors (DNS / refused / timeout) count as down.
    for url in &candidates {
        if http.get(url).send().await.is_ok() {
            return true;
        }
    }
    false
}

/// `settings` should include any per-session Ollama URL/model the desktop
/// typed in Settings (same as `/session/message`); when empty, probes
/// env-time `OLLAMA_BASE_URL` only — which missed LAN/Exo URLs that only
/// exist in the UI.
async fn provider_ready_ollama_with_settings(
    state: &AppState,
    settings: &ProviderSettings,
) -> bool {
    if !provider_can_message(state, ChatProvider::Ollama, settings) {
        return false;
    }
    match ollama_client_for_session(state, settings) {
        Some(c) => probe_ollama(&c).await,
        None => false,
    }
}

async fn provider_ready(state: &AppState, p: ChatProvider) -> bool {
    let empty = ProviderSettings::default();
    if !provider_can_message(state, p, &empty) {
        return false;
    }
    match p {
        // Cloud providers: trust the key for the UI gate; the actual call
        // will surface auth failures via the API ✗ Activity row.
        ChatProvider::Anthropic | ChatProvider::Openai => true,
        ChatProvider::Ollama => provider_ready_ollama_with_settings(state, &empty).await,
    }
}

/// Optional query mirrors the desktop Settings → AI Provider → Ollama fields
/// so `/session/providers` probes the same host chat will use.
#[derive(Deserialize, Default)]
struct SessionProvidersQuery {
    #[serde(default)]
    ollama_url: Option<String>,
    #[serde(default)]
    ollama_model: Option<String>,
}

async fn session_providers(
    State(state): State<AppState>,
    Query(q): Query<SessionProvidersQuery>,
) -> Json<Value> {
    let anthropic_avail = state.anthropic.is_some();
    let openai_avail = state.openai.is_some();
    let ollama_avail = state.ollama.is_some();

    let ollama_probe_settings = if let Some(url) = q
        .ollama_url
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    {
        ProviderSettings {
            ollama: Some(OllamaOverrides {
                model: non_empty_opt(q.ollama_model.clone()),
                url: Some(url),
            }),
            ..Default::default()
        }
    } else {
        ProviderSettings::default()
    };

    let (anthropic_ready, openai_ready) = tokio::join!(
        provider_ready(&state, ChatProvider::Anthropic),
        provider_ready(&state, ChatProvider::Openai),
    );
    let ollama_ready =
        provider_ready_ollama_with_settings(&state, &ollama_probe_settings).await;

    // Pick a sensible default the UI can latch onto when the user's
    // persisted choice is dead: prefer whichever ready provider exists in
    // the order Anthropic → OpenAI → Ollama.
    let default_label = if anthropic_ready {
        "anthropic"
    } else if openai_ready {
        "openai"
    } else if ollama_ready {
        "ollama"
    } else {
        "anthropic"
    };

    Json(json!({
        "available": {
            "anthropic": anthropic_avail,
            "openai": openai_avail,
            "ollama": ollama_avail,
        },
        "ready": {
            "anthropic": anthropic_ready,
            "openai": openai_ready,
            "ollama": ollama_ready,
        },
        "default": default_label,
    }))
}

async fn session_provider_defaults() -> Json<Value> {
    let expose = expose_llm_secrets_to_ui();
    let mut v = provider_ui_defaults(expose);
    if let Value::Object(ref mut m) = v {
        m.insert("expose_secrets".into(), json!(expose));
    }
    Json(v)
}

#[derive(Deserialize)]
struct BackgroundSettingsPatch {
    provider: Option<String>,
    settings: Option<ProviderSettings>,
    loop_enabled: Option<bool>,
    inbox_auto: Option<bool>,
    heartbeat_sec: Option<u64>,
    inbox_debounce_ms: Option<u64>,
}

fn background_settings_payload(state: &AppState) -> Value {
    let cfg = state
        .background_config
        .read()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default();
    let resolved = resolve_background_provider(state).map(|(p, _)| provider_label(p));
    json!({
        "provider": provider_label(cfg.provider),
        "settings": cfg.settings,
        "loop_enabled": cfg.loop_enabled,
        "inbox_auto": cfg.inbox_auto,
        "heartbeat_sec": cfg.heartbeat_sec,
        "inbox_debounce_ms": cfg.inbox_debounce_ms,
        "resolved": {
            "loop_enabled": cfg.loop_enabled_resolved(),
            "inbox_auto": cfg.inbox_auto_resolved(),
            "heartbeat_sec": cfg.heartbeat_resolved().as_secs(),
            "inbox_debounce_ms": cfg.debounce_resolved().as_millis() as u64,
            "provider": resolved,
        },
        "env_defaults": {
            "loop_enabled": agent_loop::background_loop_enabled(),
            "inbox_auto": agent_loop::inbox_auto_enabled(),
            "heartbeat_sec": agent_loop::heartbeat_interval().as_secs(),
            "inbox_debounce_ms": agent_loop::debounce_duration().as_millis() as u64,
            "provider": provider_label(background_provider_from_env()),
        },
        "available": {
            "anthropic": state.anthropic.is_some(),
            "openai": state.openai.is_some(),
            "ollama": state.ollama.is_some(),
        },
        "fallback_chain": ["anthropic", "openai", "ollama"],
    })
}

async fn background_settings_get(State(state): State<AppState>) -> Json<Value> {
    Json(background_settings_payload(&state))
}

async fn background_settings_post(
    State(state): State<AppState>,
    Json(patch): Json<BackgroundSettingsPatch>,
) -> Result<Json<Value>, (axum::http::StatusCode, String)> {
    {
        let mut guard = state
            .background_config
            .write()
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        if let Some(p) = patch.provider.as_deref() {
            match parse_provider_label(p) {
                Some(prov) => guard.provider = prov,
                None => {
                    return Err((
                        axum::http::StatusCode::BAD_REQUEST,
                        format!("unknown provider `{p}` (expected anthropic|openai|ollama)"),
                    ));
                }
            }
        }
        if let Some(patch_settings) = patch.settings {
            merge_provider_settings(&mut guard.settings, patch_settings);
        }
        if let Some(v) = patch.loop_enabled {
            guard.loop_enabled = Some(v);
        }
        if let Some(v) = patch.inbox_auto {
            guard.inbox_auto = Some(v);
        }
        if let Some(v) = patch.heartbeat_sec {
            if !(10..=3600).contains(&v) {
                return Err((
                    axum::http::StatusCode::BAD_REQUEST,
                    "heartbeat_sec must be between 10 and 3600".to_string(),
                ));
            }
            guard.heartbeat_sec = Some(v);
        }
        if let Some(v) = patch.inbox_debounce_ms {
            if !(50..=10_000).contains(&v) {
                return Err((
                    axum::http::StatusCode::BAD_REQUEST,
                    "inbox_debounce_ms must be between 50 and 10000".to_string(),
                ));
            }
            guard.inbox_debounce_ms = Some(v);
        }
        save_background_config(&state.workspace, &guard).map_err(|e| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("persist failed: {e}"),
            )
        })?;
        tracing::info!(provider = ?guard.provider, "background automation settings updated");
    }
    Ok(Json(background_settings_payload(&state)))
}

#[derive(Deserialize)]
struct SessionBranch {
    session_id: String,
    label: Option<String>,
}

#[derive(Serialize)]
struct SessionBranchResp {
    branch_id: String,
}

async fn session_branch(
    State(state): State<AppState>,
    Json(body): Json<SessionBranch>,
) -> Result<Json<SessionBranchResp>, StatusCode> {
    if !state.sessions.contains_key(&body.session_id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let branch_id = Uuid::new_v4().to_string();
    if let Some(mut s) = state.sessions.get_mut(&body.session_id) {
        s.last_branch = Some(branch_id.clone());
    }
    let _ = body.label;
    Ok(Json(SessionBranchResp { branch_id }))
}

#[derive(Deserialize)]
struct SessionMerge {
    session_id: String,
    branch_id: String,
}

async fn session_merge(
    State(state): State<AppState>,
    Json(body): Json<SessionMerge>,
) -> Result<StatusCode, StatusCode> {
    if !state.sessions.contains_key(&body.session_id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let _ = body.branch_id;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SessionTerminate {
    session_id: String,
}

async fn session_terminate(
    State(state): State<AppState>,
    Json(body): Json<SessionTerminate>,
) -> Result<StatusCode, StatusCode> {
    state.sessions.remove(&body.session_id);
    state.cancellations.remove(&body.session_id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SessionCancel {
    session_id: String,
}

/// User clicked **Stop** in the chat UI. Flips the per-session cancellation
/// flag so [`execute_llm`] (between provider attempts) and
/// [`dispatch_agent_tool`] (before each tool runs) bail out at the next
/// safe boundary. The currently-blocked LLM HTTP call may still finish on
/// the upstream side — we trade exact-stop semantics for a guarantee that
/// no further work is queued for this turn.
///
/// Returns `204 No Content` whether or not a turn was in flight (idempotent
/// from the client's perspective; the UI has already locally finalized the
/// placeholder bubble before this fires). Also broadcasts a `turn_cancel`
/// event so any other listeners can update their view.
async fn session_cancel(
    State(state): State<AppState>,
    Json(body): Json<SessionCancel>,
) -> StatusCode {
    let was_pending = if let Some(entry) = state.cancellations.get(&body.session_id) {
        entry.value().store(true, Ordering::SeqCst);
        true
    } else {
        false
    };
    orchestrator_emit(
        &state,
        &json!({
            "kind": "turn_cancel",
            "session_id": &body.session_id,
            "had_pending_turn": was_pending,
            "source": "user",
        }),
    )
    .await;
    StatusCode::NO_CONTENT
}

async fn system_health(State(_state): State<AppState>) -> Json<Value> {
    Json(os_plane::get_os_health())
}

async fn system_processes(State(_state): State<AppState>) -> Json<Value> {
    Json(os_plane::list_processes(50))
}

async fn workspace_info(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "root": state.workspace.root().display().to_string(),
        "workspace_only": WorkspaceRoot::is_workspace_only(),
    }))
}

#[derive(Deserialize)]
struct BrowseQuery {
    path: Option<String>,
}

type BrowseErr = (StatusCode, Json<Value>);

async fn workspace_browse(
    State(state): State<AppState>,
    Query(q): Query<BrowseQuery>,
) -> Result<Json<Value>, BrowseErr> {
    let path = q.path.unwrap_or_default();
    match state.workspace.list_directory(&path) {
        Ok((rel_path, entries)) => Ok(Json(json!({
            "root": state.workspace.root().display().to_string(),
            "path": rel_path,
            "entries": entries,
        }))),
        Err(e) => {
            let (code, msg) = match &e {
                eson_agent::workspace::WorkspaceError::OutsideWorkspace(m) => {
                    (StatusCode::BAD_REQUEST, m.clone())
                }
                eson_agent::workspace::WorkspaceError::InvalidPath(m) => {
                    (StatusCode::BAD_REQUEST, m.clone())
                }
                eson_agent::workspace::WorkspaceError::Io(io) => {
                    tracing::warn!("workspace browse: {io}");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        io.to_string(),
                    )
                }
            };
            Err((code, Json(json!({ "error": msg }))))
        }
    }
}

/// Maximum bytes we'll read for any preview (hard ceiling, regardless of kind).
const PREVIEW_HARD_CAP_BYTES: u64 = 32 * 1024 * 1024;
/// Text/code/markdown previews truncated past this many bytes.
const PREVIEW_TEXT_BYTES: usize = 256 * 1024;
/// CSV / Excel preview row cap (header + this many data rows).
const PREVIEW_TABLE_ROWS: usize = 200;
/// CSV / Excel preview column cap.
const PREVIEW_TABLE_COLS: usize = 32;
/// Image previews skipped above this size (UI just shows metadata).
const PREVIEW_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Deserialize)]
struct PreviewQuery {
    path: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreviewKind {
    Markdown,
    Text,
    Code,
    Csv,
    Excel,
    Image,
    Pdf,
    Binary,
}

fn classify_extension(name: &str) -> (PreviewKind, &'static str) {
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    match ext {
        "md" | "markdown" | "mdx" => (PreviewKind::Markdown, "text/markdown"),
        "txt" | "log" | "csv_raw" | "rtf" => (PreviewKind::Text, "text/plain"),
        "json" => (PreviewKind::Code, "application/json"),
        "yaml" | "yml" => (PreviewKind::Code, "text/yaml"),
        "toml" => (PreviewKind::Code, "text/toml"),
        "ini" | "cfg" | "conf" => (PreviewKind::Code, "text/plain"),
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java" | "kt" | "swift"
        | "c" | "h" | "cpp" | "hpp" | "rb" | "sh" | "bash" | "zsh" | "ps1" | "html"
        | "css" | "scss" | "sass" | "svelte" | "vue" | "sql" | "xml" | "svg" => {
            // svg is text but we render as image below; handled there explicitly
            (PreviewKind::Code, "text/plain")
        }
        "csv" | "tsv" => (PreviewKind::Csv, "text/csv"),
        "xlsx" | "xlsm" | "xls" | "xlsb" | "ods" => (PreviewKind::Excel, "application/vnd.ms-excel"),
        "png" => (PreviewKind::Image, "image/png"),
        "jpg" | "jpeg" => (PreviewKind::Image, "image/jpeg"),
        "gif" => (PreviewKind::Image, "image/gif"),
        "webp" => (PreviewKind::Image, "image/webp"),
        "bmp" => (PreviewKind::Image, "image/bmp"),
        "tiff" | "tif" => (PreviewKind::Image, "image/tiff"),
        "pdf" => (PreviewKind::Pdf, "application/pdf"),
        _ => (PreviewKind::Binary, "application/octet-stream"),
    }
}

async fn workspace_preview(
    State(state): State<AppState>,
    Query(q): Query<PreviewQuery>,
) -> Result<Json<Value>, BrowseErr> {
    let rel = q.path.trim().to_string();
    if rel.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "path is required" })),
        ));
    }

    let resolved = state.workspace.resolve(&rel).map_err(map_workspace_err)?;
    if !resolved.is_file() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("not a file: {}", resolved.display()) })),
        ));
    }
    let meta = std::fs::metadata(&resolved).map_err(|e| {
        tracing::warn!("workspace preview metadata: {e}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;
    let size = meta.len();
    let name = resolved
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| rel.clone());

    let (kind, mime) = {
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".svg") {
            (PreviewKind::Image, "image/svg+xml")
        } else {
            classify_extension(&name)
        }
    };

    // SVG handled specially (text → returned as image data URI)
    if mime == "image/svg+xml" {
        if size > PREVIEW_IMAGE_BYTES {
            return Ok(Json(json!({
                "kind": "image",
                "name": name,
                "path": rel,
                "size": size,
                "mime": mime,
                "skipped": true,
                "note": "SVG is too large to preview inline.",
            })));
        }
        let bytes = std::fs::read(&resolved).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
        return Ok(Json(json!({
            "kind": "image",
            "name": name,
            "path": rel,
            "size": size,
            "mime": mime,
            "data_base64": b64,
        })));
    }

    match kind {
        PreviewKind::Markdown | PreviewKind::Text | PreviewKind::Code => {
            let cap = std::cmp::min(PREVIEW_HARD_CAP_BYTES as usize, PREVIEW_TEXT_BYTES);
            let read_size = std::cmp::min(size as usize, cap);
            let raw = std::fs::read(&resolved).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
            })?;
            let truncated = (raw.len() as u64) < size || (size as usize) > cap;
            let slice = &raw[..std::cmp::min(raw.len(), read_size)];
            let text = String::from_utf8_lossy(slice).into_owned();
            let kind_label = match kind {
                PreviewKind::Markdown => "markdown",
                PreviewKind::Text => "text",
                _ => "code",
            };
            Ok(Json(json!({
                "kind": kind_label,
                "name": name,
                "path": rel,
                "size": size,
                "mime": mime,
                "text": text,
                "truncated": truncated,
                "preview_max_bytes": cap,
            })))
        }
        PreviewKind::Csv => preview_csv(&resolved, &name, &rel, size).map(Json),
        PreviewKind::Excel => preview_excel(&resolved, &name, &rel, size).map(Json),
        PreviewKind::Image => {
            if size > PREVIEW_IMAGE_BYTES {
                return Ok(Json(json!({
                    "kind": "image",
                    "name": name,
                    "path": rel,
                    "size": size,
                    "mime": mime,
                    "skipped": true,
                    "note": format!(
                        "Image is {} (preview cap is {}).",
                        humanize_bytes(size),
                        humanize_bytes(PREVIEW_IMAGE_BYTES),
                    ),
                })));
            }
            let bytes = std::fs::read(&resolved).map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": e.to_string() })),
                )
            })?;
            let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes);
            Ok(Json(json!({
                "kind": "image",
                "name": name,
                "path": rel,
                "size": size,
                "mime": mime,
                "data_base64": b64,
            })))
        }
        PreviewKind::Pdf => Ok(Json(json!({
            "kind": "pdf",
            "name": name,
            "path": rel,
            "size": size,
            "mime": mime,
            "note": "PDF preview isn't built into the workspace browser yet — ask Eson to summarize it (analyze_visual / pdf_to_table) or open it in Finder.",
        }))),
        PreviewKind::Binary => Ok(Json(json!({
            "kind": "binary",
            "name": name,
            "path": rel,
            "size": size,
            "mime": mime,
            "note": "Binary file — open it in Finder to view.",
        }))),
    }
}

fn map_workspace_err(e: eson_agent::workspace::WorkspaceError) -> BrowseErr {
    let (code, msg) = match e {
        eson_agent::workspace::WorkspaceError::OutsideWorkspace(m) => (StatusCode::BAD_REQUEST, m),
        eson_agent::workspace::WorkspaceError::InvalidPath(m) => (StatusCode::BAD_REQUEST, m),
        eson_agent::workspace::WorkspaceError::Io(io) => {
            tracing::warn!("workspace preview: {io}");
            (StatusCode::INTERNAL_SERVER_ERROR, io.to_string())
        }
    };
    (code, Json(json!({ "error": msg })))
}

fn humanize_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut idx = 0;
    while value >= 1024.0 && idx + 1 < UNITS.len() {
        value /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.1} {}", value, UNITS[idx])
    }
}

fn preview_csv(
    abs: &Path,
    name: &str,
    rel: &str,
    size: u64,
) -> Result<Value, BrowseErr> {
    if size > PREVIEW_HARD_CAP_BYTES {
        return Ok(json!({
            "kind": "csv",
            "name": name,
            "path": rel,
            "size": size,
            "mime": "text/csv",
            "skipped": true,
            "note": format!(
                "CSV is {} (preview cap is {}). Open it in Finder or ask Eson to summarize.",
                humanize_bytes(size),
                humanize_bytes(PREVIEW_HARD_CAP_BYTES),
            ),
        }));
    }
    let lower = name.to_ascii_lowercase();
    let delim = if lower.ends_with(".tsv") { b'\t' } else { b',' };
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim)
        .has_headers(false)
        .flexible(true)
        .from_path(abs)
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("csv open failed: {e}") })),
            )
        })?;
    let mut headers: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut total_rows: usize = 0;
    let mut max_cols: usize = 0;
    let mut iter = rdr.records();
    if let Some(first) = iter.next() {
        let rec = first.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("csv read failed: {e}") })),
            )
        })?;
        headers = rec.iter().take(PREVIEW_TABLE_COLS).map(|s| s.to_string()).collect();
        max_cols = max_cols.max(headers.len());
    }
    for rec in iter {
        total_rows += 1;
        if rows.len() >= PREVIEW_TABLE_ROWS {
            // keep counting but stop pushing
            continue;
        }
        match rec {
            Ok(r) => {
                let row: Vec<String> = r.iter().take(PREVIEW_TABLE_COLS).map(|s| s.to_string()).collect();
                max_cols = max_cols.max(row.len());
                rows.push(row);
            }
            Err(_) => continue,
        }
    }
    Ok(json!({
        "kind": "csv",
        "name": name,
        "path": rel,
        "size": size,
        "mime": "text/csv",
        "delimiter": if delim == b'\t' { "\t" } else { "," },
        "headers": headers,
        "rows": rows,
        "max_cols": max_cols,
        "total_data_rows": total_rows,
        "rows_truncated": total_rows > rows.len(),
        "preview_row_cap": PREVIEW_TABLE_ROWS,
        "preview_col_cap": PREVIEW_TABLE_COLS,
    }))
}

fn preview_excel(
    abs: &Path,
    name: &str,
    rel: &str,
    size: u64,
) -> Result<Value, BrowseErr> {
    if size > PREVIEW_HARD_CAP_BYTES {
        return Ok(json!({
            "kind": "excel",
            "name": name,
            "path": rel,
            "size": size,
            "skipped": true,
            "note": format!(
                "Workbook is {} (preview cap is {}). Open it in Finder or ask Eson to analyze it.",
                humanize_bytes(size),
                humanize_bytes(PREVIEW_HARD_CAP_BYTES),
            ),
        }));
    }
    use calamine::{open_workbook_auto, Reader};
    let mut wb = open_workbook_auto(abs).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("excel open failed: {e}") })),
        )
    })?;
    let sheet_names = wb.sheet_names();
    let mut sheets: Vec<Value> = Vec::new();
    for sname in sheet_names.iter().take(8) {
        let range = match wb.worksheet_range(sname) {
            Ok(r) => r,
            Err(e) => {
                sheets.push(json!({
                    "name": sname,
                    "error": e.to_string(),
                }));
                continue;
            }
        };
        let total_rows = range.height();
        let total_cols = range.width();
        let row_cap = PREVIEW_TABLE_ROWS;
        let col_cap = PREVIEW_TABLE_COLS;
        let mut headers: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut iter = range.rows();
        if let Some(first) = iter.next() {
            headers = first
                .iter()
                .take(col_cap)
                .map(|c| cell_to_string(c))
                .collect();
        }
        for r in iter.take(row_cap) {
            let row: Vec<String> = r.iter().take(col_cap).map(|c| cell_to_string(c)).collect();
            rows.push(row);
        }
        sheets.push(json!({
            "name": sname,
            "headers": headers,
            "rows": rows,
            "total_rows": total_rows,
            "total_cols": total_cols,
            "rows_truncated": total_rows.saturating_sub(1) > rows.len(),
            "cols_truncated": total_cols > col_cap,
        }));
    }
    Ok(json!({
        "kind": "excel",
        "name": name,
        "path": rel,
        "size": size,
        "sheets": sheets,
        "sheet_count": sheet_names.len(),
        "preview_row_cap": PREVIEW_TABLE_ROWS,
        "preview_col_cap": PREVIEW_TABLE_COLS,
    }))
}

fn cell_to_string(c: &calamine::Data) -> String {
    use calamine::Data;
    match c {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e16 {
                format!("{}", *f as i64)
            } else {
                format!("{}", f)
            }
        }
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => dt.to_string(),
        Data::DateTimeIso(s) => s.clone(),
        Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#ERR:{e:?}"),
    }
}

/// Prompt fed to the vision model. The explicit `OCR:` marker lets us
/// cheaply split the reply into a human caption and verbatim OCR
/// without a second LLM turn.
const SCAN_IMAGE_PROMPT: &str = "Describe the image in ~2 sentences focusing on subject, context, and any notable objects. Then, on a new line, write OCR: followed by any readable text verbatim (or OCR: none if there is none).";

/// Split a vision reply into `(caption, ocr_text)`. If the `OCR:`
/// marker is missing we treat the whole reply as the caption — this
/// keeps graceful behavior if a provider ignores the prompt format.
fn split_vision_reply(reply: &str) -> (String, Option<String>) {
    let trimmed = reply.trim();
    // Match "OCR:" at the start of a line, case-insensitive.
    let lower = trimmed.to_ascii_lowercase();
    let marker = lower
        .match_indices("ocr:")
        .find(|(idx, _)| *idx == 0 || trimmed.as_bytes().get(idx - 1) == Some(&b'\n'));
    match marker {
        Some((idx, _)) => {
            let caption = trimmed[..idx].trim().trim_end_matches(['\n', '\r']).to_string();
            let ocr_raw = trimmed[idx + 4..].trim();
            let ocr = if ocr_raw.is_empty() || ocr_raw.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(ocr_raw.to_string())
            };
            (caption, ocr)
        }
        None => (trimmed.to_string(), None),
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default)
}

async fn scan_images(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    let _permit = state.policy.acquire_tool_permit().await;
    let _ = state
        .io
        .emit("image_scan_started", &json!({ "scope": "workspace" }))
        .await;

    let mut paths = Vec::new();
    scan::collect_image_files(state.workspace.root(), &mut paths).map_err(|e| {
        tracing::error!("scan: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    orchestrator_emit(
        &state,
        &json!({
            "kind": "image_scan_begin",
            "files_total": paths.len(),
            "scope": "workspace",
        }),
    )
    .await;

    let root = state.workspace.root();
    let memory_up = state.memory.status_ok().await;

    // Guardrails: caller can tune via env without a rebuild. Defaults:
    // 25 MiB skip threshold, 60s per-image vision timeout.
    let max_bytes = env_usize("ESON_IMAGE_MAX_BYTES", 25 * 1024 * 1024);
    let analyze_timeout =
        std::time::Duration::from_secs(env_u64("ESON_IMAGE_ANALYZE_TIMEOUT_SEC", 60));
    let embed_model = state.embedder.model().to_string();
    const EMBED_CHUNK_ID: &str = "full";

    let mut indexed = 0u32;
    let mut analyzed = 0u32;
    let mut embedded = 0u32;
    let mut skipped_large = 0u32;
    let mut vision_errors = 0u32;
    let mut embed_errors = 0u32;

    for abs in paths {
        let rel = abs.strip_prefix(root).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let meta = std::fs::metadata(&abs).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let hash = scan::stub_hash(&abs, &meta);
        let ext = abs
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin")
            .to_ascii_lowercase();
        let size_bytes = meta.len() as usize;

        // ---- 1. Size guardrail --------------------------------------------------
        if size_bytes > max_bytes {
            skipped_large += 1;
            let _ = state
                .io
                .emit(
                    "image_file_processed",
                    &json!({
                        "path": rel_str,
                        "status": "skipped_large",
                        "bytes": size_bytes,
                        "limit_bytes": max_bytes,
                    }),
                )
                .await;
            continue;
        }

        // ---- 2. Vision analyze (caption + OCR) ----------------------------------
        // Read file bytes once; reuse for both register + analyze path.
        let bytes = match tokio::fs::read(&abs).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %rel_str, error = %e, "scan_images: read failed");
                let _ = state
                    .io
                    .emit(
                        "image_file_processed",
                        &json!({ "path": rel_str, "status": "read_error" }),
                    )
                    .await;
                continue;
            }
        };
        let mime = match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "heic" => "image/heic",
            "webp" => "image/webp",
            _ => "application/octet-stream",
        }
        .to_string();

        // `vision::analyze_image` uses a blocking HTTP client, so wrap
        // it in `spawn_blocking` + a tokio timeout. That way a slow
        // vision model can't stall the whole tokio worker nor hang
        // the scan indefinitely.
        let prompt = SCAN_IMAGE_PROMPT.to_string();
        let analyze = tokio::task::spawn_blocking(move || {
            let cfg = vision::VisionConfig::from_env();
            vision::analyze_image(&cfg, &bytes, &mime, &prompt)
        });
        let analyze_result = match tokio::time::timeout(analyze_timeout, analyze).await {
            Ok(Ok(inner)) => inner,
            Ok(Err(join_err)) => Err(format!("join: {join_err}")),
            Err(_) => Err(format!(
                "timeout after {}s",
                analyze_timeout.as_secs()
            )),
        };

        let (caption, ocr_text, vision_ok) = match analyze_result {
            Ok(reply) => {
                analyzed += 1;
                let (cap, ocr) = split_vision_reply(&reply);
                (cap, ocr, true)
            }
            Err(err) => {
                vision_errors += 1;
                tracing::warn!(path = %rel_str, error = %err, "scan_images: vision failed");
                (String::new(), None, false)
            }
        };

        // ---- 3. Register row ---------------------------------------------------
        let image_id = if memory_up {
            match state
                .memory
                .register_image(
                    &rel_str,
                    &hash,
                    &ext,
                    ocr_text.as_deref(),
                    if caption.is_empty() { None } else { Some(&caption) },
                )
                .await
            {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::warn!(path = %rel_str, error = %e, "scan_images: register failed");
                    None
                }
            }
        } else {
            None
        };
        indexed += 1;

        // ---- 4. Embed (caption + OCR) ------------------------------------------
        let embed_input = format!(
            "{}\n\n{}",
            caption,
            ocr_text.as_deref().unwrap_or("")
        )
        .trim()
        .to_string();

        let mut embed_dim: Option<usize> = None;
        let mut embed_status = "skipped";
        if let (true, Some(image_id)) = (!embed_input.is_empty(), image_id.as_ref()) {
            match state.embedder.embed(&embed_input).await {
                Ok(vec) => {
                    let dim = vec.len();
                    match state
                        .memory
                        .put_image_embedding(image_id, EMBED_CHUNK_ID, &embed_model, &vec)
                        .await
                    {
                        Ok(()) => {
                            embedded += 1;
                            embed_dim = Some(dim);
                            embed_status = "ok";
                        }
                        Err(e) => {
                            embed_errors += 1;
                            embed_status = "store_error";
                            tracing::warn!(
                                path = %rel_str, error = %e,
                                "scan_images: put_image_embedding failed"
                            );
                        }
                    }
                }
                Err(e) => {
                    embed_errors += 1;
                    embed_status = "embed_error";
                    tracing::warn!(
                        path = %rel_str, error = %e,
                        "scan_images: embed failed"
                    );
                }
            }
        }

        let caption_chars = caption.chars().count();
        let ocr_chars = ocr_text.as_ref().map(|s| s.chars().count()).unwrap_or(0);
        let status = if !vision_ok {
            "vision_error"
        } else if !memory_up {
            "memory_down"
        } else {
            "indexed"
        };

        let _ = state
            .io
            .emit(
                "image_file_processed",
                &json!({
                    "path": rel_str,
                    "status": status,
                    "caption_chars": caption_chars,
                    "ocr_chars": ocr_chars,
                    "embed_status": embed_status,
                    "embed_dim": embed_dim,
                    "embed_model": if embed_dim.is_some() { Some(embed_model.clone()) } else { None },
                    "memory": memory_up,
                }),
            )
            .await;
    }

    let _ = state
        .io
        .emit(
            "image_scan_completed",
            &json!({
                "indexed": indexed,
                "analyzed": analyzed,
                "embedded": embedded,
                "skipped_large": skipped_large,
                "vision_errors": vision_errors,
                "embed_errors": embed_errors,
                "memory": memory_up,
            }),
        )
        .await;

    orchestrator_emit(
        &state,
        &json!({
            "kind": "image_scan_end",
            "indexed": indexed,
            "analyzed": analyzed,
            "embedded": embedded,
            "skipped_large": skipped_large,
            "vision_errors": vision_errors,
            "embed_errors": embed_errors,
            "memory_reachable": memory_up,
            "embed_model": embed_model,
        }),
    )
    .await;

    Ok(Json(json!({
        "indexed": indexed,
        "analyzed": analyzed,
        "embedded": embedded,
        "skipped_large": skipped_large,
        "vision_errors": vision_errors,
        "embed_errors": embed_errors,
        "memory_reachable": memory_up,
        "embed_model": embed_model,
    })))
}

#[cfg(test)]
mod split_vision_reply_tests {
    use super::split_vision_reply;

    #[test]
    fn splits_caption_and_ocr() {
        let reply = "A cat on a red rug near a window.\nOCR: Welcome Home";
        let (caption, ocr) = split_vision_reply(reply);
        assert_eq!(caption, "A cat on a red rug near a window.");
        assert_eq!(ocr.as_deref(), Some("Welcome Home"));
    }

    #[test]
    fn handles_missing_marker() {
        let reply = "A wide landscape photo.";
        let (caption, ocr) = split_vision_reply(reply);
        assert_eq!(caption, "A wide landscape photo.");
        assert!(ocr.is_none());
    }

    #[test]
    fn treats_none_as_absent_ocr() {
        let reply = "An abstract painting.\nOCR: none";
        let (caption, ocr) = split_vision_reply(reply);
        assert_eq!(caption, "An abstract painting.");
        assert!(ocr.is_none());
    }

    #[test]
    fn only_splits_on_line_start_marker() {
        // An inline "OCR:" inside the caption must not be split on —
        // we only accept it at the start of the reply or a new line.
        let reply = "Note: the author writes OCR: is fine in-line.";
        let (caption, ocr) = split_vision_reply(reply);
        assert_eq!(caption, "Note: the author writes OCR: is fine in-line.");
        assert!(ocr.is_none());
    }
}
