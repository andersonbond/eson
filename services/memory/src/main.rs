//! Eson always-on memory sidecar: SQLite + HTTP API.

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("eson_memory=info".parse().unwrap()),
        )
        .init();

    let port: u16 = std::env::var("ESON_MEMORY_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8888);

    let workspace = workspace_root();
    let db_path = workspace.join("db").join("eson_memory.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent).expect("create db dir");
    }

    let conn = Connection::open(&db_path).expect("open sqlite");
    init_schema(&conn).expect("schema");
    info!(path = %db_path.display(), "eson-memory ready");

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
    };

    let app = Router::new()
        .route("/status", get(status))
        .route("/ingest", post(ingest))
        .route("/query", get(query))
        .route("/consolidate", post(consolidate))
        .route("/memories", get(list_memories))
        .route("/delete", post(delete_memory))
        .route("/clear", post(clear_all))
        .route("/images/register", post(register_image))
        .route("/images/embed", post(put_image_embedding))
        .route("/images/search", post(search_image_embeddings))
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("bind memory port");
    info!(%port, "listening");
    axum::serve(listener, app).await.expect("serve");
}

fn workspace_root() -> PathBuf {
    let raw = std::env::var("ESON_WORKSPACE_ROOT").unwrap_or_else(|_| "./workspace".to_string());
    PathBuf::from(raw)
}

fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    // NOTE: the `memory_type` index is created *after* the additive
    // `ALTER TABLE` migration below so that pre-existing databases (whose
    // `events_raw` table lacks the column) do not fail the batch during
    // `CREATE INDEX`. Fresh databases still get the column via the
    // `CREATE TABLE` default.
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS events_raw (
            id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL,
            kind TEXT NOT NULL,
            payload TEXT NOT NULL,
            memory_type TEXT NOT NULL DEFAULT 'episodic'
        );

        CREATE TABLE IF NOT EXISTS world_model (
            id TEXT PRIMARY KEY,
            updated_at TEXT NOT NULL,
            key TEXT NOT NULL UNIQUE,
            value TEXT NOT NULL,
            source_event_id TEXT
        );

        CREATE TABLE IF NOT EXISTS consolidations (
            id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL,
            summary TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS session_summaries (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            summary TEXT NOT NULL
        );

        -- Image intelligence (Milestone 1 alignment)
        CREATE TABLE IF NOT EXISTS images (
            id TEXT PRIMARY KEY,
            source_path TEXT NOT NULL,
            file_hash TEXT NOT NULL,
            file_ext TEXT,
            mime_type TEXT,
            width INTEGER,
            height INTEGER,
            ocr_text TEXT,
            caption TEXT,
            confidence REAL,
            created_at TEXT NOT NULL,
            UNIQUE(file_hash, source_path)
        );

        CREATE TABLE IF NOT EXISTS image_entities (
            id TEXT PRIMARY KEY,
            image_id TEXT NOT NULL REFERENCES images(id) ON DELETE CASCADE,
            entity_type TEXT NOT NULL,
            entity_value TEXT NOT NULL,
            confidence REAL
        );

        CREATE TABLE IF NOT EXISTS image_key_values (
            id TEXT PRIMARY KEY,
            image_id TEXT NOT NULL REFERENCES images(id) ON DELETE CASCADE,
            kv_key TEXT NOT NULL,
            kv_value TEXT NOT NULL,
            confidence REAL
        );

        CREATE TABLE IF NOT EXISTS image_embeddings (
            id TEXT PRIMARY KEY,
            image_id TEXT NOT NULL REFERENCES images(id) ON DELETE CASCADE,
            chunk_id TEXT NOT NULL,
            model_name TEXT NOT NULL,
            dim INTEGER NOT NULL,
            vector BLOB NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS image_ingestion_runs (
            id TEXT PRIMARY KEY,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            status TEXT NOT NULL,
            files_seen INTEGER DEFAULT 0,
            files_indexed INTEGER DEFAULT 0,
            error TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_events_created ON events_raw(created_at);
        CREATE INDEX IF NOT EXISTS idx_images_path ON images(source_path);
        "#,
    )?;
    // Additive migration for older databases that predate the
    // `memory_type` column. SQLite has no `IF NOT EXISTS` for
    // columns so we try and swallow the "duplicate column" error.
    if let Err(e) = conn.execute(
        "ALTER TABLE events_raw ADD COLUMN memory_type TEXT NOT NULL DEFAULT 'episodic'",
        [],
    ) {
        let msg = e.to_string();
        if !msg.contains("duplicate column name") {
            return Err(e);
        }
    }
    // Index creation is deferred until after the migration so
    // pre-existing DBs successfully gain the column first.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_events_raw_type \
         ON events_raw(memory_type, created_at DESC);",
    )?;
    Ok(())
}

#[derive(Serialize)]
struct StatusResponse {
    ok: bool,
    db_path: String,
    event_count: i64,
    image_count: i64,
}

async fn status(State(state): State<AppState>) -> impl IntoResponse {
    let db = state.db.lock().unwrap();
    let event_count: i64 = db
        .query_row("SELECT COUNT(*) FROM events_raw", [], |r| r.get(0))
        .unwrap_or(0);
    let image_count: i64 = db
        .query_row("SELECT COUNT(*) FROM images", [], |r| r.get(0))
        .unwrap_or(0);
    let db_path = workspace_root().join("db").join("eson_memory.db");
    Json(StatusResponse {
        ok: true,
        db_path: db_path.display().to_string(),
        event_count,
        image_count,
    })
}

#[derive(Deserialize)]
struct IngestBody {
    text: String,
    #[serde(default)]
    kind: String,
    /// Cognitive memory tier: `episodic` (time-bound event) or
    /// `semantic` (generalized knowledge). Defaults to `episodic`.
    /// Invalid values are rejected so schema drift is loud.
    #[serde(default)]
    memory_type: Option<String>,
}

#[derive(Serialize)]
struct IngestResponse {
    id: String,
}

async fn ingest(
    State(state): State<AppState>,
    Json(body): Json<IngestBody>,
) -> Result<Json<IngestResponse>, StatusCode> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let kind = if body.kind.is_empty() {
        "note".into()
    } else {
        body.kind
    };
    let memory_type = match body.memory_type.as_deref().map(str::trim) {
        None | Some("") => "episodic".to_string(),
        Some(raw) => match raw.to_ascii_lowercase().as_str() {
            "episodic" | "semantic" => raw.to_ascii_lowercase(),
            _ => return Err(StatusCode::BAD_REQUEST),
        },
    };
    let payload = serde_json::json!({ "text": body.text }).to_string();
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    db.execute(
        "INSERT INTO events_raw (id, created_at, kind, payload, memory_type) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, now, kind, payload, memory_type],
    )
    .map_err(|e| {
        warn!("ingest: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(IngestResponse { id }))
}

#[derive(Deserialize)]
struct QueryParams {
    q: String,
}

#[derive(Serialize)]
struct QueryResponse {
    answer: String,
    referenced_ids: Vec<String>,
}

async fn query(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<QueryParams>,
) -> Json<QueryResponse> {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare(
            "SELECT id, payload FROM events_raw WHERE payload LIKE ?1 ORDER BY created_at DESC LIMIT 50",
        )
        .unwrap();
    let needle = format!("%{}%", params.q.replace('%', "\\%"));
    let rows: Vec<(String, String)> = stmt
        .query_map([&needle], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .filter_map(|x| x.ok())
        .collect();
    let referenced_ids: Vec<String> = rows.iter().map(|(id, _)| id.clone()).collect();
    let answer = if rows.is_empty() {
        "No matching memories.".into()
    } else {
        format!("Found {} event(s) matching (local FTS-style scan).", rows.len())
    };
    Json(QueryResponse {
        answer,
        referenced_ids,
    })
}

#[derive(Serialize)]
struct ConsolidateResponse {
    id: String,
    patterns: usize,
}

async fn consolidate(State(state): State<AppState>) -> Result<Json<ConsolidateResponse>, StatusCode> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let n: i64 = db
        .query_row("SELECT COUNT(*) FROM events_raw", [], |r| r.get(0))
        .unwrap_or(0);
    let summary = format!("Consolidation placeholder: {n} raw events indexed.");
    db.execute(
        "INSERT INTO consolidations (id, created_at, summary) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, now, summary],
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(ConsolidateResponse {
        id,
        patterns: n as usize,
    }))
}

#[derive(Serialize)]
struct MemoryRow {
    id: String,
    created_at: String,
    kind: String,
    memory_type: String,
}

#[derive(Deserialize)]
struct ListMemoriesParams {
    /// Optional filter: return only rows of this memory type
    /// (`episodic` or `semantic`). Unknown values return 400.
    #[serde(default)]
    memory_type: Option<String>,
}

async fn list_memories(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<ListMemoriesParams>,
) -> Result<Json<Vec<MemoryRow>>, StatusCode> {
    let filter = match params.memory_type.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => match raw.to_ascii_lowercase().as_str() {
            "episodic" | "semantic" => Some(raw.to_ascii_lowercase()),
            _ => return Err(StatusCode::BAD_REQUEST),
        },
    };
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mapper = |r: &rusqlite::Row<'_>| {
        Ok(MemoryRow {
            id: r.get(0)?,
            created_at: r.get(1)?,
            kind: r.get(2)?,
            memory_type: r.get(3)?,
        })
    };
    let rows: Vec<MemoryRow> = if let Some(mt) = filter {
        let mut stmt = db
            .prepare(
                "SELECT id, created_at, kind, memory_type FROM events_raw
                 WHERE memory_type = ?1 ORDER BY created_at DESC LIMIT 200",
            )
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let iter = stmt
            .query_map([mt], mapper)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let collected: Vec<MemoryRow> = iter.filter_map(|x| x.ok()).collect();
        collected
    } else {
        let mut stmt = db
            .prepare(
                "SELECT id, created_at, kind, memory_type FROM events_raw
                 ORDER BY created_at DESC LIMIT 200",
            )
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let iter = stmt
            .query_map([], mapper)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let collected: Vec<MemoryRow> = iter.filter_map(|x| x.ok()).collect();
        collected
    };
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct DeleteBody {
    id: String,
}

async fn delete_memory(
    State(state): State<AppState>,
    Json(body): Json<DeleteBody>,
) -> Result<StatusCode, StatusCode> {
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let n = db
        .execute("DELETE FROM events_raw WHERE id = ?1", [&body.id])
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if n == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct RegisterImageBody {
    source_path: String,
    file_hash: String,
    file_ext: String,
    #[serde(default)]
    ocr_text: Option<String>,
    /// Human-readable caption produced by the vision LLM. Stored in
    /// `images.caption` so downstream readers (search UI, future
    /// recall tools) can render a one-line description alongside the
    /// OCR text without re-running the LLM.
    #[serde(default)]
    caption: Option<String>,
}

#[derive(Serialize)]
struct RegisterImageResp {
    id: String,
}

async fn register_image(
    State(state): State<AppState>,
    Json(body): Json<RegisterImageBody>,
) -> Result<Json<RegisterImageResp>, StatusCode> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let mime = match body.file_ext.to_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "heic" => "image/heic",
        _ => "application/octet-stream",
    };
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    db.execute(
        "DELETE FROM images WHERE file_hash = ?1 AND source_path = ?2",
        rusqlite::params![body.file_hash, body.source_path],
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    db.execute(
        r#"INSERT INTO images (id, source_path, file_hash, file_ext, mime_type, ocr_text, caption, confidence, created_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1.0, ?8)"#,
        rusqlite::params![
            id,
            body.source_path,
            body.file_hash,
            body.file_ext,
            mime,
            body.ocr_text,
            body.caption,
            now
        ],
    )
    .map_err(|e| {
        warn!("register_image: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(RegisterImageResp { id }))
}

/// Body for `POST /images/embed`. We upsert by
/// `(image_id, chunk_id, model_name)` so re-running the indexer with
/// the same model on an unchanged image is idempotent.
#[derive(Deserialize)]
struct PutImageEmbeddingBody {
    image_id: String,
    #[serde(default = "default_chunk_id")]
    chunk_id: String,
    model_name: String,
    dim: usize,
    /// f32 components. We pack them little-endian into `BLOB` so the
    /// read side can decode regardless of host endianness (SQLite
    /// databases are routinely copied between machines).
    vector: Vec<f32>,
}

fn default_chunk_id() -> String {
    "full".to_string()
}

async fn put_image_embedding(
    State(state): State<AppState>,
    Json(body): Json<PutImageEmbeddingBody>,
) -> Result<StatusCode, StatusCode> {
    if body.vector.is_empty() || body.vector.len() != body.dim {
        return Err(StatusCode::BAD_REQUEST);
    }
    if body.model_name.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let blob = encode_vector(&body.vector);
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Ensure the referenced image exists; otherwise the client is racing
    // or sending stale ids. Fail loudly with 404 so the caller logs it.
    let exists: Option<i64> = db
        .query_row(
            "SELECT 1 FROM images WHERE id = ?1",
            [&body.image_id],
            |r| r.get(0),
        )
        .ok();
    if exists.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    // Idempotent upsert: drop any prior embedding for this
    // (image, chunk, model) triplet before inserting the new one.
    db.execute(
        "DELETE FROM image_embeddings \
         WHERE image_id = ?1 AND chunk_id = ?2 AND model_name = ?3",
        rusqlite::params![body.image_id, body.chunk_id, body.model_name],
    )
    .map_err(|e| {
        warn!("put_image_embedding delete: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    db.execute(
        r#"INSERT INTO image_embeddings
               (id, image_id, chunk_id, model_name, dim, vector, created_at)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"#,
        rusqlite::params![
            id,
            body.image_id,
            body.chunk_id,
            body.model_name,
            body.dim as i64,
            blob,
            now
        ],
    )
    .map_err(|e| {
        warn!("put_image_embedding insert: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SearchImageEmbeddingsBody {
    vector: Vec<f32>,
    model_name: String,
    dim: usize,
    /// Clamped to `[1, 20]` server-side so a chatty client can't force
    /// the sidecar to return huge payloads.
    #[serde(default = "default_top_k")]
    top_k: usize,
}

fn default_top_k() -> usize {
    5
}

#[derive(Serialize)]
struct ImageSearchHit {
    image_id: String,
    source_path: String,
    caption: Option<String>,
    ocr_snippet: Option<String>,
    score: f32,
}

async fn search_image_embeddings(
    State(state): State<AppState>,
    Json(body): Json<SearchImageEmbeddingsBody>,
) -> Result<Json<Vec<ImageSearchHit>>, StatusCode> {
    if body.vector.is_empty() || body.vector.len() != body.dim {
        return Err(StatusCode::BAD_REQUEST);
    }
    if body.model_name.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let top_k = body.top_k.clamp(1, 20);
    let query_norm = l2_norm(&body.vector);
    if query_norm == 0.0 {
        return Err(StatusCode::BAD_REQUEST);
    }
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut stmt = db
        .prepare(
            r#"SELECT e.vector, i.id, i.source_path, i.caption, i.ocr_text
               FROM image_embeddings e
               JOIN images i ON i.id = e.image_id
               WHERE e.model_name = ?1 AND e.dim = ?2"#,
        )
        .map_err(|e| {
            warn!("search_image_embeddings prepare: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let iter = stmt
        .query_map(
            rusqlite::params![body.model_name, body.dim as i64],
            |r| {
                let vector: Vec<u8> = r.get(0)?;
                let image_id: String = r.get(1)?;
                let source_path: String = r.get(2)?;
                let caption: Option<String> = r.get(3)?;
                let ocr_text: Option<String> = r.get(4)?;
                Ok((vector, image_id, source_path, caption, ocr_text))
            },
        )
        .map_err(|e| {
            warn!("search_image_embeddings query: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let mut scored: Vec<(f32, ImageSearchHit)> = Vec::new();
    for row in iter.flatten() {
        let (vector_bytes, image_id, source_path, caption, ocr_text) = row;
        let Some(doc) = decode_vector(&vector_bytes, body.dim) else {
            continue;
        };
        let score = cosine_with_query_norm(&body.vector, query_norm, &doc);
        if !score.is_finite() {
            continue;
        }
        let hit = ImageSearchHit {
            image_id,
            source_path,
            caption,
            ocr_snippet: ocr_text.map(|t| snippet_text(&t, 240)),
            score,
        };
        scored.push((score, hit));
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let hits: Vec<ImageSearchHit> = scored.into_iter().take(top_k).map(|(_, h)| h).collect();
    Ok(Json(hits))
}

fn encode_vector(vec: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vec.len() * 4);
    for v in vec {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

fn decode_vector(bytes: &[u8], expected_dim: usize) -> Option<Vec<f32>> {
    if bytes.len() != expected_dim * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(expected_dim);
    for chunk in bytes.chunks_exact(4) {
        let arr: [u8; 4] = chunk.try_into().ok()?;
        out.push(f32::from_le_bytes(arr));
    }
    Some(out)
}

fn l2_norm(v: &[f32]) -> f32 {
    let sum: f32 = v.iter().map(|x| x * x).sum();
    sum.sqrt()
}

fn cosine_with_query_norm(q: &[f32], q_norm: f32, d: &[f32]) -> f32 {
    if q.len() != d.len() {
        return f32::NAN;
    }
    let mut dot = 0.0f32;
    let mut d_sq = 0.0f32;
    for i in 0..q.len() {
        dot += q[i] * d[i];
        d_sq += d[i] * d[i];
    }
    let d_norm = d_sq.sqrt();
    if d_norm == 0.0 || q_norm == 0.0 {
        return 0.0;
    }
    dot / (q_norm * d_norm)
}

fn snippet_text(s: &str, max_chars: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

async fn clear_all(State(state): State<AppState>) -> Result<StatusCode, StatusCode> {
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    db.execute_batch(
        "DELETE FROM image_embeddings; DELETE FROM image_key_values; DELETE FROM image_entities; DELETE FROM images; DELETE FROM session_summaries; DELETE FROM consolidations; DELETE FROM world_model; DELETE FROM events_raw;",
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_round_trip_is_lossless() {
        let v: Vec<f32> = vec![-1.5, 0.0, 0.25, 3.125, 42.0];
        let bytes = encode_vector(&v);
        assert_eq!(bytes.len(), v.len() * 4);
        let decoded = decode_vector(&bytes, v.len()).unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn decode_rejects_wrong_length() {
        let bytes = encode_vector(&[1.0, 2.0, 3.0]);
        assert!(decode_vector(&bytes, 4).is_none());
        assert!(decode_vector(&bytes[..bytes.len() - 1], 3).is_none());
    }

    #[test]
    fn cosine_of_identical_vectors_is_one() {
        let v = vec![0.1, 0.2, 0.3, 0.4];
        let n = l2_norm(&v);
        let s = cosine_with_query_norm(&v, n, &v);
        assert!((s - 1.0).abs() < 1e-6, "got {s}");
    }

    #[test]
    fn cosine_of_orthogonal_vectors_is_zero() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        let s = cosine_with_query_norm(&a, l2_norm(&a), &b);
        assert!(s.abs() < 1e-6);
    }

    #[test]
    fn cosine_handles_zero_vector_gracefully() {
        let q = vec![1.0, 0.0];
        let z = vec![0.0, 0.0];
        let s = cosine_with_query_norm(&q, l2_norm(&q), &z);
        assert_eq!(s, 0.0);
    }
}
