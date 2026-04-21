//! Persistent agent memory in SQLite under the workspace (`db/memory.db`), inspired by
//! structured recall/store flows (e.g. always-on memory agents) without a separate vector DB.

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

// NOTE: the `idx_agent_memory_type` index references the
// `memory_type` column and is created **after** the additive migration
// in `open()`. Pre-existing `db/memory.db` files still have the legacy
// schema without the column until we run `ALTER TABLE`, so creating the
// index inline here would blow up the first boot after an upgrade.
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_memory (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at_ms INTEGER NOT NULL,
    summary TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    topics TEXT NOT NULL DEFAULT '[]',
    importance REAL NOT NULL DEFAULT 0.5,
    source TEXT NOT NULL DEFAULT 'agent',
    memory_type TEXT NOT NULL DEFAULT 'episodic'
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
    /// `episodic` (time-bound lived events) or `semantic` (generalized
    /// durable knowledge). Mirrors the cognitive inspiration behind the
    /// 12h consolidation pass; defaults to `episodic` for legacy rows
    /// written before the schema migration landed.
    pub memory_type: String,
}

/// Allowed `memory_type` values. Kept as a tiny enum to avoid
/// string-typing drift across the store / recall / consolidation code
/// paths; new kinds (e.g. `procedural`) can be added here first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryType {
    Episodic,
    Semantic,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::Episodic => "episodic",
            MemoryType::Semantic => "semantic",
        }
    }

    /// Parse a user-facing string (`episodic` / `semantic`) with
    /// tolerant aliasing. Returns `None` for anything we don't want
    /// to silently accept — the caller should then reject the write.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "episodic" | "episode" | "event" => Some(MemoryType::Episodic),
            "semantic" | "rule" | "fact" => Some(MemoryType::Semantic),
            _ => None,
        }
    }
}

impl Default for MemoryType {
    fn default() -> Self {
        MemoryType::Episodic
    }
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
        // Additive migration for existing `db/memory.db` files created
        // before the `memory_type` column existed. SQLite's
        // `ALTER TABLE … ADD COLUMN` is idempotent-by-intent here: we
        // ignore a "duplicate column" error so fresh installs (which
        // already created the column via `SCHEMA`) and upgrades both
        // succeed. Any other failure propagates so schema drift is
        // loud rather than silent.
        if let Err(e) = conn.execute(
            "ALTER TABLE agent_memory ADD COLUMN memory_type TEXT NOT NULL DEFAULT 'episodic'",
            [],
        ) {
            let msg = e.to_string();
            if !msg.contains("duplicate column name") {
                return Err(format!("migrate memory_type: {msg}"));
            }
        }
        // Index creation is deferred until after the migration so
        // pre-existing DBs gain the column first.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_agent_memory_type \
             ON agent_memory(memory_type, created_at_ms DESC);",
        )
        .map_err(|e| format!("create memory_type index: {e}"))?;
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

    /// Legacy helper: stores a row with the default memory type
    /// (`episodic`). New callers should prefer
    /// [`Self::store_typed`] so episodic vs semantic writes are
    /// distinguishable in later queries / consolidation passes.
    pub fn store(
        &self,
        summary: &str,
        body: &str,
        topics_json: &str,
        importance: f64,
        source: &str,
    ) -> Result<i64, String> {
        self.store_typed(
            summary,
            body,
            topics_json,
            importance,
            source,
            MemoryType::Episodic,
        )
    }

    /// Canonical write path. `memory_type` controls whether this row
    /// represents a time-bound episode or a generalized rule; the
    /// 12h consolidation skill is expected to call this directly
    /// with an explicit value.
    pub fn store_typed(
        &self,
        summary: &str,
        body: &str,
        topics_json: &str,
        importance: f64,
        source: &str,
        memory_type: MemoryType,
    ) -> Result<i64, String> {
        let summary = summary.trim();
        if summary.is_empty() {
            return Err("summary must not be empty".into());
        }
        let imp = importance.clamp(0.0, 1.0);
        let g = self.conn.lock().map_err(|e| e.to_string())?;
        g.execute(
            "INSERT INTO agent_memory (created_at_ms, summary, body, topics, importance, source, memory_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                Self::now_ms(),
                summary,
                body.trim(),
                topics_json.trim(),
                imp,
                source.trim(),
                memory_type.as_str(),
            ],
        )
        .map_err(|e| e.to_string())?;
        Ok(g.last_insert_rowid())
    }

    /// Rank rows by simple token overlap on summary + body (no embeddings).
    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryRow>, String> {
        self.recall_filtered(query, limit, None)
    }

    /// Same as [`Self::recall`] but with an optional `memory_type`
    /// filter (e.g. only `semantic` rules). Used by the 12h
    /// consolidation pass to focus on one memory track at a time and
    /// by the LLM tool when the caller asks for a specific kind.
    pub fn recall_filtered(
        &self,
        query: &str,
        limit: usize,
        memory_type: Option<MemoryType>,
    ) -> Result<Vec<MemoryRow>, String> {
        let limit = limit.clamp(1, 100);
        let g = self.conn.lock().map_err(|e| e.to_string())?;
        let q = query.trim();
        if q.is_empty() {
            let (sql, filter): (&str, Option<&'static str>) = match memory_type {
                Some(_) => (
                    "SELECT id, created_at_ms, summary, body, topics, importance, source, memory_type
                     FROM agent_memory WHERE memory_type = ?2 ORDER BY created_at_ms DESC LIMIT ?1",
                    memory_type.map(|m| m.as_str()),
                ),
                None => (
                    "SELECT id, created_at_ms, summary, body, topics, importance, source, memory_type
                     FROM agent_memory ORDER BY created_at_ms DESC LIMIT ?1",
                    None,
                ),
            };
            let mut stmt = g.prepare(sql).map_err(|e| e.to_string())?;
            let rows = if let Some(mt) = filter {
                stmt.query_map(params![limit as i64, mt], row_to_memory)
                    .map_err(|e| e.to_string())?
                    .collect::<Vec<_>>()
            } else {
                stmt.query_map(params![limit as i64], row_to_memory)
                    .map_err(|e| e.to_string())?
                    .collect::<Vec<_>>()
            };
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

        let (scan_sql, filter): (&str, Option<&'static str>) = match memory_type {
            Some(_) => (
                "SELECT id, created_at_ms, summary, body, topics, importance, source, memory_type
                 FROM agent_memory WHERE memory_type = ?1",
                memory_type.map(|m| m.as_str()),
            ),
            None => (
                "SELECT id, created_at_ms, summary, body, topics, importance, source, memory_type
                 FROM agent_memory",
                None,
            ),
        };

        let mut stmt = g.prepare(scan_sql).map_err(|e| e.to_string())?;
        let rows = if let Some(mt) = filter {
            stmt.query_map(params![mt], row_to_memory)
                .map_err(|e| e.to_string())?
                .collect::<Vec<_>>()
        } else {
            stmt.query_map([], row_to_memory)
                .map_err(|e| e.to_string())?
                .collect::<Vec<_>>()
        };

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
                "SELECT id, created_at_ms, summary, body, topics, importance, source, memory_type
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
    // Tolerate rows read via queries that don't project `memory_type`
    // (older callers / get_by_id variants). SQLite returns an index
    // error when the column is absent; fall back to the default so
    // legacy paths keep working.
    let memory_type: String = row.get::<_, String>(7).unwrap_or_else(|_| "episodic".into());
    Ok(MemoryRow {
        id: row.get(0)?,
        created_at_ms: row.get(1)?,
        summary: row.get(2)?,
        body: row.get(3)?,
        topics: row.get(4)?,
        importance: row.get(5)?,
        source: row.get(6)?,
        memory_type,
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
        assert_eq!(rows[0].memory_type, "episodic");
    }

    #[test]
    fn memory_type_round_trip_and_filter() {
        let dir = std::env::temp_dir().join(format!("eson-mem-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let m = AgentMemory::open(dir.join("memory.db")).unwrap();
        m.store_typed(
            "user likes terse replies",
            "",
            r#"["pref"]"#,
            0.8,
            "12h-cron",
            MemoryType::Semantic,
        )
        .unwrap();
        m.store_typed(
            "window: morning shift",
            "ran 3 tool calls",
            r#"["digest"]"#,
            0.5,
            "12h-cron",
            MemoryType::Episodic,
        )
        .unwrap();
        let all = m.recall("", 10).unwrap();
        assert_eq!(all.len(), 2);
        let sem_only = m
            .recall_filtered("", 10, Some(MemoryType::Semantic))
            .unwrap();
        assert_eq!(sem_only.len(), 1);
        assert_eq!(sem_only[0].memory_type, "semantic");
        let epi_only = m
            .recall_filtered("", 10, Some(MemoryType::Episodic))
            .unwrap();
        assert_eq!(epi_only.len(), 1);
        assert_eq!(epi_only[0].memory_type, "episodic");
    }

    #[test]
    fn memory_type_parse_is_tolerant() {
        assert_eq!(MemoryType::parse("Semantic"), Some(MemoryType::Semantic));
        assert_eq!(MemoryType::parse("  EPISODIC "), Some(MemoryType::Episodic));
        assert_eq!(MemoryType::parse("rule"), Some(MemoryType::Semantic));
        assert_eq!(MemoryType::parse("weird"), None);
    }
}

