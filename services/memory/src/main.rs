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
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS events_raw (
            id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL,
            kind TEXT NOT NULL,
            payload TEXT NOT NULL
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
    let payload = serde_json::json!({ "text": body.text }).to_string();
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    db.execute(
        "INSERT INTO events_raw (id, created_at, kind, payload) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, now, kind, payload],
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
}

async fn list_memories(State(state): State<AppState>) -> Json<Vec<MemoryRow>> {
    let db = state.db.lock().unwrap();
    let mut stmt = db
        .prepare("SELECT id, created_at, kind FROM events_raw ORDER BY created_at DESC LIMIT 200")
        .unwrap();
    let rows: Vec<MemoryRow> = stmt
        .query_map([], |r| {
            Ok(MemoryRow {
                id: r.get(0)?,
                created_at: r.get(1)?,
                kind: r.get(2)?,
            })
        })
        .unwrap()
        .filter_map(|x| x.ok())
        .collect();
    Json(rows)
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
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 1.0, ?7)"#,
        rusqlite::params![
            id,
            body.source_path,
            body.file_hash,
            body.file_ext,
            mime,
            body.ocr_text,
            now
        ],
    )
    .map_err(|e| {
        warn!("register_image: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(RegisterImageResp { id }))
}

async fn clear_all(State(state): State<AppState>) -> Result<StatusCode, StatusCode> {
    let db = state.db.lock().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    db.execute_batch(
        "DELETE FROM image_embeddings; DELETE FROM image_key_values; DELETE FROM image_entities; DELETE FROM images; DELETE FROM session_summaries; DELETE FROM consolidations; DELETE FROM world_model; DELETE FROM events_raw;",
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}
