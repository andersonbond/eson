//! HTTP client for eson-memory sidecar (graceful degradation if down).

use reqwest::Client;
use serde::Deserialize;

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

    pub async fn register_image(
        &self,
        source_path: &str,
        file_hash: &str,
        file_ext: &str,
        ocr_text: Option<&str>,
    ) -> Result<(), String> {
        let url = format!("{}/images/register", self.base);
        let body = serde_json::json!({
            "source_path": source_path,
            "file_hash": file_hash,
            "file_ext": file_ext,
            "ocr_text": ocr_text,
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
        Ok(())
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
