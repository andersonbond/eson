//! Persistent agent memory in SQLite under the workspace (`db/memory.db`), inspired by
//! structured recall/store flows (e.g. always-on memory agents) without a separate vector DB.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_memory (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at_ms INTEGER NOT NULL,
    summary TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    topics TEXT NOT NULL DEFAULT '[]',
    importance REAL NOT NULL DEFAULT 0.5,
    source TEXT NOT NULL DEFAULT 'agent'
);
CREATE INDEX IF NOT EXISTS idx_agent_memory_created ON agent_memory(created_at_ms DESC);

CREATE TABLE IF NOT EXISTS user_model (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS session_summaries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    summary TEXT NOT NULL,
    token_estimate INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_session_summaries_session ON session_summaries(session_id, created_at_ms DESC);
"#;

#[derive(Debug, Serialize)]
pub struct MemoryRow {
    pub id: i64,
    pub created_at_ms: i64,
    pub summary: String,
    pub body: String,
    pub topics: String,
    pub importance: f64,
    pub source: String,
}

pub struct AgentMemory {
    path: PathBuf,
    conn: Mutex<Connection>,
}

impl AgentMemory {
    /// Opens or creates `path` (typically `<workspace>/db/memory.db`).
    pub fn open(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(&path).map_err(|e| e.to_string())?;
        conn.execute_batch(SCHEMA).map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        Ok(Self {
            path,
            conn: Mutex::new(conn),
        })
    }

    pub fn db_path(&self) -> &PathBuf {
        &self.path
    }

    fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    pub fn store(
        &self,
        summary: &str,
        body: &str,
        topics_json: &str,
        importance: f64,
        source: &str,
    ) -> Result<i64, String> {
        let summary = summary.trim();
        if summary.is_empty() {
            return Err("summary must not be empty".into());
        }
        let imp = importance.clamp(0.0, 1.0);
        let g = self.conn.lock().map_err(|e| e.to_string())?;
        g.execute(
            "INSERT INTO agent_memory (created_at_ms, summary, body, topics, importance, source)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                Self::now_ms(),
                summary,
                body.trim(),
                topics_json.trim(),
                imp,
                source.trim(),
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(g.last_insert_rowid())
    }

    /// Rank rows by simple token overlap on summary + body (no embeddings).
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryRow>, String> {
        let limit = limit.clamp(1, 100);
        let g = self.conn.lock().map_err(|e| e.to_string())?;
        let q = query.trim();
        if q.is_empty() {
            let mut stmt = g
                .prepare(
                    "SELECT id, created_at_ms, summary, body, topics, importance, source
                     FROM agent_memory ORDER BY created_at_ms DESC LIMIT ?1",
                )
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![limit as i64], row_to_memory)
                .map_err(|e| e.to_string())?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r.map_err(|e| e.to_string())?);
            }
            return Ok(v);
        }

        let tokens: Vec<String> = q
            .split_whitespace()
            .map(|t| t.to_lowercase())
            .filter(|t| !t.is_empty())
            .take(32)
            .collect();

        let mut stmt = g
            .prepare(
                "SELECT id, created_at_ms, summary, body, topics, importance, source
                 FROM agent_memory",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], row_to_memory)
            .map_err(|e| e.to_string())?;

        let mut scored: Vec<(i32, MemoryRow)> = Vec::new();
        for r in rows {
            let row = r.map_err(|e| e.to_string())?;
            let hay = format!(
                "{} {} {}",
                row.summary.to_lowercase(),
                row.body.to_lowercase(),
                row.topics.to_lowercase()
            );
            let score = if tokens.is_empty() {
                0
            } else {
                tokens.iter().filter(|t| hay.contains(t.as_str())).count() as i32
            };
            if score > 0 || hay.contains(&q.to_lowercase()) {
                scored.push((score, row));
            }
        }
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.created_at_ms.cmp(&a.1.created_at_ms))
        });
        scored.truncate(limit);
        Ok(scored.into_iter().map(|(_, r)| r).collect())
    }

    pub fn get_by_id(&self, id: i64) -> Result<Option<MemoryRow>, String> {
        let g = self.conn.lock().map_err(|e| e.to_string())?;
        let row = g
            .query_row(
                "SELECT id, created_at_ms, summary, body, topics, importance, source
                 FROM agent_memory WHERE id = ?1",
                params![id],
                row_to_memory,
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(row)
    }

    pub fn user_model_set(&self, key: &str, value: &str) -> Result<(), String> {
        let key = key.trim();
        if key.is_empty() {
            return Err("key must not be empty".into());
        }
        let g = self.conn.lock().map_err(|e| e.to_string())?;
        g.execute(
            "INSERT INTO user_model (key, value, updated_at_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at_ms = excluded.updated_at_ms",
            params![key, value, Self::now_ms()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn user_model_get(&self, key: Option<&str>) -> Result<Vec<(String, String)>, String> {
        let g = self.conn.lock().map_err(|e| e.to_string())?;
        if let Some(k) = key.filter(|s| !s.trim().is_empty()) {
            let mut stmt = g
                .prepare("SELECT key, value FROM user_model WHERE key = ?1")
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map(params![k.trim()], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(|e| e.to_string())?;
            let mut v = Vec::new();
            for r in rows {
                v.push(r.map_err(|e| e.to_string())?);
            }
            return Ok(v);
        }
        let mut stmt = g
            .prepare("SELECT key, value FROM user_model ORDER BY key ASC LIMIT 200")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r.map_err(|e| e.to_string())?);
        }
        Ok(v)
    }

    pub fn user_model_prompt_block(&self) -> String {
        match self.user_model_get(None) {
            Ok(rows) if rows.is_empty() => "(empty)".to_string(),
            Ok(rows) => rows
                .into_iter()
                .map(|(k, v)| format!("- **{k}:** {}", v.chars().take(500).collect::<String>()))
                .collect::<Vec<_>>()
                .join("\n"),
            Err(e) => format!("(error loading user_model: {e})"),
        }
    }

    pub fn session_summary_append(
        &self,
        session_id: &str,
        summary: &str,
        token_estimate: i64,
    ) -> Result<i64, String> {
        let g = self.conn.lock().map_err(|e| e.to_string())?;
        g.execute(
            "INSERT INTO session_summaries (session_id, created_at_ms, summary, token_estimate)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, Self::now_ms(), summary, token_estimate],
        )
        .map_err(|e| e.to_string())?;
        Ok(g.last_insert_rowid())
    }

    pub fn session_summaries_recent(&self, limit: usize) -> Result<Vec<(String, String)>, String> {
        let limit = limit.clamp(1, 100) as i64;
        let g = self.conn.lock().map_err(|e| e.to_string())?;
        let mut stmt = g
            .prepare(
                "SELECT session_id, summary FROM session_summaries ORDER BY created_at_ms DESC LIMIT ?1",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![limit], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        let mut v = Vec::new();
        for r in rows {
            v.push(r.map_err(|e| e.to_string())?);
        }
        Ok(v)
    }

    /// Bump a counter in `user_model` (e.g. after-action cadence).
    pub fn bump_user_turn_counter(&self) -> Result<i64, String> {
        let g = self.conn.lock().map_err(|e| e.to_string())?;
        let cur: i64 = g
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM user_model WHERE key = 'eson_user_turn_count'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let next = cur + 1;
        g.execute(
            "INSERT INTO user_model (key, value, updated_at_ms) VALUES ('eson_user_turn_count', ?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at_ms = excluded.updated_at_ms",
            params![next.to_string(), Self::now_ms()],
        )
        .map_err(|e| e.to_string())?;
        Ok(next)
    }

    pub fn user_turn_count(&self) -> i64 {
        let g = match self.conn.lock() {
            Ok(g) => g,
            Err(_) => return 0,
        };
        g.query_row(
            "SELECT CAST(value AS INTEGER) FROM user_model WHERE key = 'eson_user_turn_count'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0)
    }
}

fn row_to_memory(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        id: row.get(0)?,
        created_at_ms: row.get(1)?,
        summary: row.get(2)?,
        body: row.get(3)?,
        topics: row.get(4)?,
        importance: row.get(5)?,
        source: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn store_and_recall_roundtrip() {
        let dir = std::env::temp_dir().join(format!("eson-mem-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let m = AgentMemory::open(dir.join("memory.db")).unwrap();
        m.store("greet", "say hi", r#"["t"]"#, 0.6, "test").unwrap();
        let rows = m.recall("greet", 5).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].summary, "greet");
    }
}

