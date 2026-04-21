//! Multimodal helpers for `analyze_visual` / `pdf_to_table`.
//!
//! Three provider backends are supported:
//!
//! 1. **Ollama** (default, fully local) — talks to `/api/generate` on a
//!    locally-running Ollama instance. The default model is `gemma4:e4b`.
//! 2. **Anthropic** — Claude vision via `messages` with image content blocks.
//! 3. **OpenAI** — GPT-4o family via `chat/completions` with `image_url`
//!    data URIs.
//!
//! [`VisionConfig`] is built once per turn from the per-session
//! `ProviderSettings` (UI Settings → "Vision") with environment-variable
//! fallback. The chat provider and vision provider are intentionally
//! independent — a user can keep chat on Anthropic while running vision
//! locally on Ollama, or any other combination.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const DEFAULT_OLLAMA_VISION_MODEL: &str = "gemma4:e4b";
const DEFAULT_ANTHROPIC_VISION_MODEL: &str = "claude-haiku-4-5-20251001";
const DEFAULT_OPENAI_VISION_MODEL: &str = "gpt-4o-mini";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisionProvider {
    Ollama,
    Anthropic,
    Openai,
}

impl VisionProvider {
    pub fn label(self) -> &'static str {
        match self {
            VisionProvider::Ollama => "ollama",
            VisionProvider::Anthropic => "anthropic",
            VisionProvider::Openai => "openai",
        }
    }
}

/// Parse a UI / env string into a [`VisionProvider`]. Case-insensitive.
/// `"local"` is accepted as an alias for Ollama, `"claude"` for Anthropic.
pub fn parse_vision_provider(s: &str) -> Option<VisionProvider> {
    match s.trim().to_ascii_lowercase().as_str() {
        "ollama" | "local" => Some(VisionProvider::Ollama),
        "anthropic" | "claude" => Some(VisionProvider::Anthropic),
        "openai" | "gpt" => Some(VisionProvider::Openai),
        _ => None,
    }
}

/// Resolved vision settings used by [`analyze_image`] / [`analyze_table`].
/// Caller builds one of these from per-session UI overrides + env defaults.
#[derive(Debug, Clone)]
pub struct VisionConfig {
    pub provider: VisionProvider,
    pub model: String,
    /// Required for `Ollama` — base URL **without** `/v1` suffix.
    pub ollama_base: String,
    /// Required for `Anthropic` — empty string disables that branch.
    pub anthropic_api_key: String,
    /// Required for `Openai` — empty string disables that branch.
    pub openai_api_key: String,
    /// Base URL for OpenAI-compatible endpoint (defaults to api.openai.com).
    pub openai_base: String,
}

impl VisionConfig {
    /// Build a config exclusively from environment variables. Used when the
    /// per-session `ProviderSettings.vision` is unset.
    ///
    /// Honoured env vars:
    ///
    /// - `ESON_VISION_PROVIDER`  — `"ollama" | "anthropic" | "openai"`
    /// - `ESON_VISION_MODEL`     — model id
    /// - `ESON_VISION_OLLAMA_URL` / `OLLAMA_BASE_URL`
    /// - `ANTHROPIC_API_KEY` / `ANTHROPIC_MODEL`
    /// - `OPENAI_API_KEY` / `OPENAI_MODEL` / `OPENAI_BASE_URL`
    pub fn from_env() -> Self {
        let provider = std::env::var("ESON_VISION_PROVIDER")
            .ok()
            .as_deref()
            .and_then(parse_vision_provider)
            .unwrap_or(VisionProvider::Ollama);
        let model = std::env::var("ESON_VISION_MODEL")
            .ok()
            .or_else(|| std::env::var("OLLAMA_VISION_MODEL").ok())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| match provider {
                VisionProvider::Ollama => DEFAULT_OLLAMA_VISION_MODEL.to_string(),
                VisionProvider::Anthropic => std::env::var("ANTHROPIC_MODEL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_ANTHROPIC_VISION_MODEL.to_string()),
                VisionProvider::Openai => std::env::var("OPENAI_MODEL")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| DEFAULT_OPENAI_VISION_MODEL.to_string()),
            });
        let ollama_base = std::env::var("ESON_VISION_OLLAMA_URL")
            .ok()
            .or_else(|| std::env::var("OLLAMA_BASE_URL").ok())
            .unwrap_or_else(|| "http://127.0.0.1:11434".to_string())
            .trim_end_matches('/')
            .trim_end_matches("/v1")
            .to_string();
        let anthropic_api_key = std::env::var("ANTHROPIC_API_KEY").unwrap_or_default();
        let openai_api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
        let openai_base = std::env::var("OPENAI_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim_end_matches('/').to_string())
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
        VisionConfig {
            provider,
            model,
            ollama_base,
            anthropic_api_key,
            openai_api_key,
            openai_base,
        }
    }

    /// Return a default model for `provider` (used when the user picks a
    /// provider in the UI but leaves the model field blank).
    pub fn default_model_for(provider: VisionProvider) -> &'static str {
        match provider {
            VisionProvider::Ollama => DEFAULT_OLLAMA_VISION_MODEL,
            VisionProvider::Anthropic => DEFAULT_ANTHROPIC_VISION_MODEL,
            VisionProvider::Openai => DEFAULT_OPENAI_VISION_MODEL,
        }
    }

    /// Cheap sanity check used by the orchestrator before kicking off a
    /// long PDF-rasterize loop — surfaces a clear UI error instead of a
    /// confusing HTTP 401 mid-page.
    pub fn validate(&self) -> Result<(), String> {
        match self.provider {
            VisionProvider::Ollama => {
                if self.ollama_base.is_empty() {
                    return Err("Ollama vision base URL is empty".into());
                }
            }
            VisionProvider::Anthropic => {
                if self.anthropic_api_key.is_empty() {
                    return Err(
                        "Anthropic vision selected but ANTHROPIC_API_KEY is not set".into(),
                    );
                }
            }
            VisionProvider::Openai => {
                if self.openai_api_key.is_empty() {
                    return Err("OpenAI vision selected but OPENAI_API_KEY is not set".into());
                }
            }
        }
        Ok(())
    }
}

/// Backwards-compatible Ollama-only base helper. Retained because a few
/// non-tool call sites (e.g. startup probe) still want the vision URL
/// without going through the full per-session settings flow.
pub fn ollama_generate_base() -> String {
    VisionConfig::from_env().ollama_base
}

pub fn vision_model() -> String {
    VisionConfig::from_env().model
}

pub fn vision_allow_cloud() -> bool {
    std::env::var("ESON_VISION_ALLOW_CLOUD")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn blocking_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .expect("blocking reqwest")
}

/// Best-effort MIME guess from an image's file extension. Defaults to PNG
/// because that's the format pdftoppm produces and it's the safest bet for
/// LLM image endpoints when we genuinely don't know.
pub fn mime_for_extension(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

/// Single image (raw bytes) → free-form text response from the configured
/// provider. Mirrors the previous `ollama_vision_prompt` API but routes to
/// Anthropic / OpenAI when the user has picked them in Settings.
pub fn analyze_image(
    cfg: &VisionConfig,
    image_bytes: &[u8],
    mime: &str,
    prompt: &str,
) -> Result<String, String> {
    cfg.validate()?;
    match cfg.provider {
        VisionProvider::Ollama => ollama_generate_image(cfg, image_bytes, prompt),
        VisionProvider::Anthropic => anthropic_image(cfg, image_bytes, mime, prompt),
        VisionProvider::Openai => openai_image(cfg, image_bytes, mime, prompt),
    }
}

/// Ask the configured vision model to return **only** a JSON object
/// `{"columns":[...],"rows":[[...],...]}`. Strips fences and trailing
/// commentary before parsing.
pub fn analyze_table(
    cfg: &VisionConfig,
    page_png: &[u8],
    hint: &str,
) -> Result<Value, String> {
    let prompt = format!(
        "{hint}\n\nReturn ONLY valid JSON with shape {{\"columns\":[\"col1\",...],\"rows\":[[cell,...],...]}}. No markdown fences, no commentary."
    );
    let text = analyze_image(cfg, page_png, "image/png", &prompt)?;
    extract_json_object(&text)
}

// ----- Provider implementations -----

fn ollama_generate_image(
    cfg: &VisionConfig,
    image_bytes: &[u8],
    prompt: &str,
) -> Result<String, String> {
    let b64 = B64.encode(image_bytes);
    let url = format!("{}/api/generate", cfg.ollama_base.trim_end_matches('/'));
    let body = json!({
        "model": cfg.model,
        "prompt": prompt,
        "images": [b64],
        "stream": false,
    });
    let res = blocking_client()
        .post(&url)
        .json(&body)
        .send()
        .map_err(|e| format!("ollama vision request failed: {e}"))?;
    let status = res.status();
    if !status.is_success() {
        let t = res.text().unwrap_or_default();
        return Err(format!("ollama vision HTTP {status}: {t}"));
    }
    let v: Value = res.json().map_err(|e| e.to_string())?;
    Ok(v.get("response")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .trim()
        .to_string())
}

fn anthropic_image(
    cfg: &VisionConfig,
    image_bytes: &[u8],
    mime: &str,
    prompt: &str,
) -> Result<String, String> {
    let b64 = B64.encode(image_bytes);
    let body = json!({
        "model": cfg.model,
        "max_tokens": 4096,
        "messages": [{
            "role": "user",
            "content": [
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": mime,
                        "data": b64,
                    }
                },
                { "type": "text", "text": prompt },
            ]
        }]
    });
    let res = blocking_client()
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &cfg.anthropic_api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| format!("anthropic vision request failed: {e}"))?;
    let status = res.status();
    let body_text = res.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("anthropic vision HTTP {status}: {body_text}"));
    }
    let v: Value =
        serde_json::from_str(&body_text).map_err(|e| format!("anthropic vision parse: {e}"))?;
    let content = v.get("content").and_then(|c| c.as_array());
    let text = match content {
        Some(arr) => arr
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        None => v
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string(),
    };
    if text.trim().is_empty() {
        return Err("anthropic vision returned no text".into());
    }
    Ok(text.trim().to_string())
}

fn openai_image(
    cfg: &VisionConfig,
    image_bytes: &[u8],
    mime: &str,
    prompt: &str,
) -> Result<String, String> {
    let b64 = B64.encode(image_bytes);
    let data_uri = format!("data:{mime};base64,{b64}");
    let body = json!({
        "model": cfg.model,
        "messages": [{
            "role": "user",
            "content": [
                { "type": "text", "text": prompt },
                {
                    "type": "image_url",
                    "image_url": { "url": data_uri }
                }
            ]
        }],
        "max_tokens": 4096,
    });
    let url = format!(
        "{}/chat/completions",
        cfg.openai_base.trim_end_matches('/')
    );
    let res = blocking_client()
        .post(&url)
        .header("authorization", format!("Bearer {}", cfg.openai_api_key))
        .json(&body)
        .send()
        .map_err(|e| format!("openai vision request failed: {e}"))?;
    let status = res.status();
    let body_text = res.text().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("openai vision HTTP {status}: {body_text}"));
    }
    let v: Value =
        serde_json::from_str(&body_text).map_err(|e| format!("openai vision parse: {e}"))?;
    let text = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    if text.trim().is_empty() {
        return Err("openai vision returned no text".into());
    }
    Ok(text.trim().to_string())
}

/// Check `pdftoppm` exists (Poppler).
pub fn pdftoppm_available() -> bool {
    Command::new("pdftoppm")
        .arg("-v")
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success() || s.code().is_some())
        .unwrap_or(false)
}

/// Rasterize PDF pages to PNG files in `out_dir` using `pdftoppm -png -f FIRST -l LAST`.
pub fn rasterize_pdf_pages(
    pdf_abs: &Path,
    pages: &[u32],
    out_dir: &Path,
) -> Result<Vec<PathBuf>, String> {
    if pages.is_empty() {
        return Ok(vec![]);
    }
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let first = *pages.iter().min().ok_or("no pages")?;
    let last = *pages.iter().max().ok_or("no pages")?;
    let prefix = out_dir.join("page");
    let prefix_str = prefix.to_string_lossy();
    let status = Command::new("pdftoppm")
        .arg("-png")
        .arg("-f")
        .arg(first.to_string())
        .arg("-l")
        .arg(last.to_string())
        .arg(pdf_abs.as_os_str())
        .arg(prefix_str.as_ref())
        .status()
        .map_err(|e| format!("pdftoppm failed to start: {e} (install Poppler: brew install poppler)"))?;
    if !status.success() {
        return Err(format!("pdftoppm exited {:?}", status.code()));
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    for p in pages {
        // pdftoppm names: page-1.png, page-01.png depending on version — glob
        let candidates = [
            out_dir.join(format!("page-{}.png", p)),
            out_dir.join(format!("page-{:02}.png", p)),
            out_dir.join(format!("page-{:03}.png", p)),
        ];
        let found = candidates.into_iter().find(|x| x.is_file());
        if let Some(f) = found {
            paths.push(f);
        } else {
            // scan directory for page-* containing page number
            let rd = std::fs::read_dir(out_dir).map_err(|e| e.to_string())?;
            let mut hit: Option<PathBuf> = None;
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_string();
                if n.contains(&format!("-{p}.png")) || n.contains(&format!("-{:02}.png", p)) {
                    hit = Some(e.path());
                    break;
                }
            }
            if let Some(h) = hit {
                paths.push(h);
            } else {
                return Err(format!("pdftoppm did not produce PNG for page {p}"));
            }
        }
    }
    Ok(paths)
}

fn extract_json_object(text: &str) -> Result<Value, String> {
    let t = text.trim();
    // strip ```json ... ```
    let t = if let Some(i) = t.find('{') {
        &t[i..]
    } else {
        t
    };
    let t = if let Some(j) = t.rfind('}') {
        &t[..=j]
    } else {
        t
    };
    serde_json::from_str(t).map_err(|e| format!("invalid JSON from vision model: {e}; raw: {}", text.chars().take(400).collect::<String>()))
}

/// Spawn the vision diagnostic probes on a detached background thread.
///
/// History: this used to run synchronously on the agent's main thread with
/// `blocking_client()`'s **300 s** timeout, which deadlocked startup for up
/// to 5 minutes whenever `OLLAMA_BASE_URL` pointed at an unreachable host
/// (e.g. a sleeping LAN box). Now we (a) use a 1.5 s connect / 2.5 s read
/// budget tailored for an info-only probe, (b) detach the thread so the
/// HTTP listener comes up immediately, and (c) skip the Ollama probe
/// entirely when the user has picked a non-Ollama vision provider.
pub fn log_vision_startup_warnings() {
    let cfg = VisionConfig::from_env();
    if matches!(cfg.provider, VisionProvider::Ollama) {
        std::thread::spawn(|| run_vision_probe());
    } else {
        tracing::info!(
            provider = cfg.provider.label(),
            model = %cfg.model,
            "Vision provider is non-local — skipping Ollama startup probe"
        );
    }
    if !pdftoppm_available() {
        tracing::warn!("`pdftoppm` not found on PATH — PDF tools need Poppler (brew install poppler)");
    }
}

fn run_vision_probe() {
    let base = ollama_generate_base();
    let model = vision_model();
    let url = format!("{}/api/tags", base);
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_millis(1500))
        .timeout(Duration::from_millis(2500))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    match client.get(&url).send() {
        Ok(r) if r.status().is_success() => {
            if let Ok(v) = r.json::<Value>() {
                let models = v
                    .get("models")
                    .and_then(|m| m.as_array())
                    .cloned()
                    .unwrap_or_default();
                let found = models.iter().any(|m| {
                    m.get("name")
                        .and_then(|n| n.as_str())
                        .map(|n| n == model.as_str() || n.starts_with(&format!("{model}:")))
                        .unwrap_or(false)
                });
                if !found {
                    tracing::warn!(
                        model = %model,
                        "Ollama may not have vision model pulled; try: ollama pull {}",
                        model
                    );
                }
            }
        }
        Ok(r) => tracing::warn!(status = %r.status(), "could not reach Ollama at {} for vision check", base),
        Err(e) => tracing::warn!(error = %e, "Ollama unreachable at {} — analyze_visual / pdf_to_table need local Ollama (or pick a non-local vision provider in Settings)", base),
    }
}
