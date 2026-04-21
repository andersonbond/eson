//! HTTP client for eson-memory sidecar (graceful degradation if down).

use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct MemoryClient {
    base: String,
    http: Client,
}

impl MemoryClient {
    pub fn from_env() -> Self {
        let base = std::env::var("ESON_MEMORY_URL").unwrap_or_else(|_| "http://127.0.0.1:8888".to_string());
        Self {
            base: base.trim_end_matches('/').to_string(),
            http: Client::new(),
        }
    }

    pub async fn status_ok(&self) -> bool {
        let url = format!("{}/status", self.base);
        self.http.get(url).send().await.map(|r| r.status().is_success()).unwrap_or(false)
    }

    pub async fn ingest(&self, text: &str, kind: &str) -> Result<String, String> {
        let url = format!("{}/ingest", self.base);
        let body = serde_json::json!({ "text": text, "kind": kind });
        let res = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("memory ingest {}", res.status()));
        }
        let v: IngestResponse = res.json().await.map_err(|e| e.to_string())?;
        Ok(v.id)
    }

    pub async fn query(&self, q: &str) -> Result<String, String> {
        let mut u = reqwest::Url::parse(&format!("{}/query", self.base)).map_err(|e| e.to_string())?;
        u.query_pairs_mut().append_pair("q", q);
        let url = u.to_string();
        let res = self.http.get(url).send().await.map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("memory query {}", res.status()));
        }
        let v: QueryResponse = res.json().await.map_err(|e| e.to_string())?;
        Ok(v.answer)
    }

    /// Registers (or re-registers) an image row and returns the
    /// server-generated `image_id`. `ocr_text` and `caption` are both
    /// optional — callers that only have one can pass `None` for the
    /// other and the sidecar will store NULL.
    pub async fn register_image(
        &self,
        source_path: &str,
        file_hash: &str,
        file_ext: &str,
        ocr_text: Option<&str>,
        caption: Option<&str>,
    ) -> Result<String, String> {
        let url = format!("{}/images/register", self.base);
        let body = serde_json::json!({
            "source_path": source_path,
            "file_hash": file_hash,
            "file_ext": file_ext,
            "ocr_text": ocr_text,
            "caption": caption,
        });
        let res = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("register_image {}", res.status()));
        }
        let v: RegisterImageResponse = res.json().await.map_err(|e| e.to_string())?;
        Ok(v.id)
    }

    /// Stores an embedding vector for an image. `chunk_id` lets us
    /// extend to per-region embeddings later without changing callers.
    pub async fn put_image_embedding(
        &self,
        image_id: &str,
        chunk_id: &str,
        model_name: &str,
        vector: &[f32],
    ) -> Result<(), String> {
        let url = format!("{}/images/embed", self.base);
        let body = serde_json::json!({
            "image_id": image_id,
            "chunk_id": chunk_id,
            "model_name": model_name,
            "dim": vector.len(),
            "vector": vector,
        });
        let res = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("put_image_embedding {}", res.status()));
        }
        Ok(())
    }

    /// Runs a cosine top-K search against the sidecar and returns the
    /// matching image rows (already sorted by descending score).
    pub async fn search_images(
        &self,
        model_name: &str,
        query_vector: &[f32],
        top_k: usize,
    ) -> Result<Vec<ImageHit>, String> {
        let url = format!("{}/images/search", self.base);
        let body = serde_json::json!({
            "model_name": model_name,
            "dim": query_vector.len(),
            "vector": query_vector,
            "top_k": top_k,
        });
        let res = self
            .http
            .post(url)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("search_images {}", res.status()));
        }
        let hits: Vec<ImageHit> = res.json().await.map_err(|e| e.to_string())?;
        Ok(hits)
    }
}

#[derive(Deserialize)]
struct IngestResponse {
    id: String,
}

#[derive(Deserialize)]
struct QueryResponse {
    answer: String,
}

#[derive(Deserialize)]
struct RegisterImageResponse {
    id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageHit {
    pub image_id: String,
    pub source_path: String,
    pub caption: Option<String>,
    pub ocr_snippet: Option<String>,
    pub score: f32,
}
