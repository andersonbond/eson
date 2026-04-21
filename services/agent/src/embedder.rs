//! Text embedding client for the Eson agent.
//!
//! Targets the OpenAI-compatible `POST /v1/embeddings` shape because
//! Ollama (≥ 0.1.30) serves that endpoint natively and it keeps the
//! door open for swapping to OpenAI `text-embedding-3-*` later by
//! flipping `ESON_EMBED_PROVIDER` + `ESON_EMBED_BASE_URL`.
//!
//! Env resolution (first match wins):
//!
//! | Setting                 | Env var                                        | Default                   |
//! | ----------------------- | ---------------------------------------------- | ------------------------- |
//! | Model                   | `ESON_EMBED_MODEL`                             | `qwen3-embedding:4b`      |
//! | Base URL                | `ESON_EMBED_BASE_URL`, else `OLLAMA_BASE_URL`  | `http://127.0.0.1:11434`  |
//! | Request timeout (secs)  | `ESON_EMBED_TIMEOUT_SEC`                       | `60`                      |
//!
//! The response parser accepts both the OpenAI shape
//! (`{data: [{embedding: [...]}]}`) and Ollama's native
//! `{embeddings: [[...]]}` shape — resilient across Ollama versions.

use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

const DEFAULT_MODEL: &str = "qwen3-embedding:4b";
const DEFAULT_BASE: &str = "http://127.0.0.1:11434";
const DEFAULT_TIMEOUT_SEC: u64 = 60;

#[derive(Clone)]
pub struct EmbedClient {
    http: Client,
    base: String,
    model: String,
}

impl EmbedClient {
    pub fn from_env() -> Self {
        let base_raw = std::env::var("ESON_EMBED_BASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("OLLAMA_BASE_URL").ok())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BASE.to_string());
        // Allow users to paste URLs with or without the `/v1` suffix.
        // We always post to `{base}/v1/embeddings`, so strip a trailing
        // `/v1` if present to avoid `/v1/v1/embeddings`.
        let trimmed = base_raw.trim_end_matches('/').to_string();
        let base = if trimmed.ends_with("/v1") {
            trimmed.trim_end_matches("/v1").to_string()
        } else {
            trimmed
        };
        let model = std::env::var("ESON_EMBED_MODEL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let timeout = std::env::var("ESON_EMBED_TIMEOUT_SEC")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SEC);
        let http = Client::builder()
            .timeout(Duration::from_secs(timeout))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { http, base, model }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Embed a single input string. Empty/whitespace-only input is
    /// rejected loudly so upstream callers don't silently store zero
    /// vectors.
    pub async fn embed(&self, input: &str) -> Result<Vec<f32>, String> {
        if input.trim().is_empty() {
            return Err("embed: input is empty".into());
        }
        let url = format!("{}/v1/embeddings", self.base);
        let body = json!({ "model": self.model, "input": input });
        let res = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("embed request: {e}"))?;
        let status = res.status();
        let text = res
            .text()
            .await
            .map_err(|e| format!("embed body: {e}"))?;
        if !status.is_success() {
            return Err(format!("embed {status}: {}", snippet(&text)));
        }
        parse_embedding_response(&text)
    }
}

/// Parses either:
///   * OpenAI-compatible  `{"data":[{"embedding":[..]}], ...}`
///   * Ollama-native      `{"embeddings":[[..]], ...}`
///   * Single-vector      `{"embedding":[..], ...}`
///
/// Returns the first embedding vector. Kept as a free function so the
/// unit tests can exercise it without going through the HTTP layer.
fn parse_embedding_response(body: &str) -> Result<Vec<f32>, String> {
    // Try the OpenAI shape first — it's what `/v1/embeddings` emits.
    if let Ok(v) = serde_json::from_str::<OpenAiEmbeddings>(body) {
        if let Some(first) = v.data.into_iter().next() {
            if !first.embedding.is_empty() {
                return Ok(first.embedding);
            }
        }
    }
    // Ollama native multi-embedding shape.
    if let Ok(v) = serde_json::from_str::<OllamaEmbeddingsMulti>(body) {
        if let Some(first) = v.embeddings.into_iter().next() {
            if !first.is_empty() {
                return Ok(first);
            }
        }
    }
    // Older Ollama singular shape.
    if let Ok(v) = serde_json::from_str::<OllamaEmbeddingSingle>(body) {
        if !v.embedding.is_empty() {
            return Ok(v.embedding);
        }
    }
    Err(format!(
        "embed: unrecognized response body: {}",
        snippet(body)
    ))
}

fn snippet(s: &str) -> String {
    const MAX: usize = 256;
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("{}…", &s[..MAX])
    }
}

#[derive(Deserialize)]
struct OpenAiEmbeddings {
    data: Vec<OpenAiEmbeddingRow>,
}

#[derive(Deserialize)]
struct OpenAiEmbeddingRow {
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct OllamaEmbeddingsMulti {
    embeddings: Vec<Vec<f32>>,
}

#[derive(Deserialize)]
struct OllamaEmbeddingSingle {
    embedding: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_shape() {
        let body = r#"{
            "object":"list",
            "data":[{"object":"embedding","index":0,"embedding":[0.1,0.2,-0.3]}],
            "model":"qwen3-embedding:4b",
            "usage":{"prompt_tokens":3,"total_tokens":3}
        }"#;
        let v = parse_embedding_response(body).unwrap();
        assert_eq!(v, vec![0.1, 0.2, -0.3]);
    }

    #[test]
    fn parses_ollama_multi_shape() {
        let body = r#"{"model":"qwen3-embedding:4b","embeddings":[[0.5,0.5,0.5]]}"#;
        let v = parse_embedding_response(body).unwrap();
        assert_eq!(v, vec![0.5, 0.5, 0.5]);
    }

    #[test]
    fn parses_ollama_singular_shape() {
        let body = r#"{"embedding":[1.0,2.0,3.0]}"#;
        let v = parse_embedding_response(body).unwrap();
        assert_eq!(v, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn rejects_empty_or_unknown() {
        assert!(parse_embedding_response("{}").is_err());
        assert!(parse_embedding_response(r#"{"data":[]}"#).is_err());
        assert!(parse_embedding_response(r#"{"embeddings":[]}"#).is_err());
        assert!(parse_embedding_response("not-json").is_err());
    }

    #[test]
    fn from_env_honors_overrides() {
        // Not perfectly hermetic (process-global env), but fine for a
        // one-shot assertion of env-driven defaults.
        std::env::set_var("ESON_EMBED_MODEL", "test-model");
        std::env::set_var("ESON_EMBED_BASE_URL", "http://example.invalid:9999/v1");
        let c = EmbedClient::from_env();
        assert_eq!(c.model(), "test-model");
        // The `/v1` suffix must be stripped so we don't end up posting
        // to `/v1/v1/embeddings`.
        assert_eq!(c.base(), "http://example.invalid:9999");
        std::env::remove_var("ESON_EMBED_MODEL");
        std::env::remove_var("ESON_EMBED_BASE_URL");
    }

    #[tokio::test]
    async fn empty_input_errors_without_network() {
        let c = EmbedClient::from_env();
        let err = c.embed("   ").await.unwrap_err();
        assert!(err.contains("empty"));
    }
}
