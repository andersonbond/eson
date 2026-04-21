//! 12-hour memory consolidation digest builder.
//!
//! The background `cron/memory-12h-consolidation` skill fires every 12
//! hours and asks the LLM to distill the last 12 h of lived experience
//! into a small durable memory footprint — one **episodic** summary
//! plus up to a few **semantic** rules. To keep that pass deterministic
//! and cheap, the orchestrator pre-builds the evidence bundle here so
//! the model never has to scan state on its own: we walk the in-memory
//! event buffer, recent session messages, workspace artifact mtimes,
//! the `.learnings/` journal, and a "already-stored" memory snapshot
//! for dedup, then emit a single Markdown block with hard caps.
//!
//! This module is intentionally `std`-only and has **no** LLM or
//! network side effects — it just collects strings. The caller
//! (`main.rs::background_loops`) decides when to inject the bundle
//! into a background cron turn and when to emit observability markers.

use crate::agent_memory::{AgentMemory, MemoryRow};
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Skill id that triggers the consolidation pass. Matches the
/// `skills/cron/memory-12h-consolidation.md` file stem.
pub const CONSOLIDATION_SKILL_ID: &str = "cron/memory-12h-consolidation";

/// Default 12-hour window (in seconds).
pub const DEFAULT_WINDOW_SECS: u64 = 12 * 3600;

/// Safety caps on per-bundle size — keep the LLM prompt bounded so a
/// busy day can't blow up background turn latency. These are tuned
/// conservatively; the skill body tells the model the bundle is
/// already filtered.
const MAX_EVENTS: usize = 40;
const MAX_CHAT_TURNS: usize = 30;
const MAX_ARTIFACTS: usize = 30;
const MAX_LEARNINGS: usize = 12;
const MAX_STORED_SNAPSHOT: usize = 30;
const TEXT_MAX: usize = 240;
const BODY_MAX: usize = 400;

/// One normalized orchestrator event retained in the rolling
/// in-memory buffer. Only events that could matter for later
/// consolidation are pushed (see
/// [`should_record_event_kind`]); everything else (pure streaming
/// deltas, progress pings) is discarded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentEvent {
    pub ts_ms: u64,
    pub kind: String,
    pub session_id: Option<String>,
    pub summary: String,
    /// Optional provenance: e.g. tool name, provider, skill id.
    pub tag: Option<String>,
}

/// Rolling bounded buffer of recent orchestrator events. Caller
/// wraps this in `Arc<Mutex<_>>`; we intentionally don't expose
/// interior mutability here so the consumer can choose locking
/// granularity.
#[derive(Debug, Clone)]
pub struct RecentEventsBuffer {
    deque: VecDeque<RecentEvent>,
    max_len: usize,
    retain_ms: u64,
}

impl Default for RecentEventsBuffer {
    fn default() -> Self {
        // Retain 48 h so a missed cron (e.g. laptop closed) still has a
        // window to pull from on the next fire. `max_len` is the hard
        // ring-buffer cap in case an event storm hits faster than the
        // retention window prunes it.
        Self {
            deque: VecDeque::new(),
            max_len: 2000,
            retain_ms: 48 * 3600 * 1000,
        }
    }
}

impl RecentEventsBuffer {
    pub fn new(max_len: usize, retain_secs: u64) -> Self {
        Self {
            deque: VecDeque::new(),
            max_len,
            retain_ms: retain_secs.saturating_mul(1000),
        }
    }

    pub fn push(&mut self, mut ev: RecentEvent) {
        if ev.ts_ms == 0 {
            ev.ts_ms = now_ms();
        }
        self.deque.push_back(ev);
        // Trim by length first (fast), then by age.
        while self.deque.len() > self.max_len {
            self.deque.pop_front();
        }
        let cutoff = now_ms().saturating_sub(self.retain_ms);
        while self
            .deque
            .front()
            .map(|e| e.ts_ms < cutoff)
            .unwrap_or(false)
        {
            self.deque.pop_front();
        }
    }

    pub fn snapshot_since(&self, cutoff_ms: u64) -> Vec<RecentEvent> {
        self.deque
            .iter()
            .filter(|e| e.ts_ms >= cutoff_ms)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.deque.len()
    }

    pub fn is_empty(&self) -> bool {
        self.deque.is_empty()
    }
}

/// Which orchestrator `kind` values are worth retaining. We skip
/// high-frequency streaming kinds to keep the buffer bounded.
pub fn should_record_event_kind(kind: &str) -> bool {
    matches!(
        kind,
        "tool"
            | "tool_begin"
            | "turn_begin"
            | "turn_end"
            | "turn_cancel"
            | "background_turn"
            | "inbox_finalize"
            | "provider_fallback"
            | "llm_call_end"
            | "chart_render"
            | "consolidation_begin"
            | "consolidation_end"
    )
}

/// One recent chat message surfaced from the live session cache.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ChatSnippet {
    pub session_id: String,
    pub role: String,
    pub text: String,
}

/// One file artifact found on disk with mtime in the window.
#[derive(Debug, Clone, Serialize)]
pub struct ArtifactRecord {
    pub rel_path: String,
    pub ts_ms: u64,
    pub size_bytes: u64,
}

/// Per-category counts surfaced to the orchestrator marker so the UI
/// can report "considered N / kept M" without reading the bundle text.
#[derive(Debug, Default, Clone, Serialize)]
pub struct DigestCounts {
    pub events: usize,
    pub chat_turns: usize,
    pub artifacts: usize,
    pub learnings: usize,
    pub stored_snapshot: usize,
}

/// The compiled bundle. `markdown` is the LLM-facing evidence block;
/// `counts` is small and cheap to include in the `consolidation_begin`
/// marker for UI display.
#[derive(Debug, Clone, Serialize)]
pub struct DigestBundle {
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    pub counts: DigestCounts,
    pub markdown: String,
    /// True when every category came up empty — caller can short-circuit
    /// and skip the LLM turn entirely.
    pub empty: bool,
}

/// Compile the evidence bundle for the 12h consolidation skill.
///
/// The caller supplies the rolling events, recent chat snippets, and
/// the workspace root; we do the filtering, dedup-snapshotting, and
/// size capping here. Pure function modulo the filesystem walk under
/// `workspace/exports/` and `workspace/.learnings/`.
pub fn build_digest_bundle(
    workspace_root: &Path,
    agent_memory: &AgentMemory,
    events: &[RecentEvent],
    chat: &[ChatSnippet],
    window_secs: u64,
) -> DigestBundle {
    let window_end_ms = now_ms();
    let window_start_ms = window_end_ms.saturating_sub(window_secs.saturating_mul(1000));

    let mut events_in_window: Vec<&RecentEvent> = events
        .iter()
        .filter(|e| e.ts_ms >= window_start_ms)
        .collect();
    events_in_window.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
    let events_kept: Vec<&RecentEvent> = events_in_window.into_iter().take(MAX_EVENTS).collect();

    let chat_kept: Vec<ChatSnippet> = chat
        .iter()
        .rev()
        .take(MAX_CHAT_TURNS)
        .map(|s| ChatSnippet {
            session_id: s.session_id.clone(),
            role: s.role.clone(),
            text: truncate(&s.text, TEXT_MAX),
        })
        .collect();

    let artifacts = recent_artifacts(workspace_root, window_start_ms, MAX_ARTIFACTS);
    let learnings = recent_learnings(workspace_root, window_start_ms, MAX_LEARNINGS);
    let stored_snapshot = stored_memory_snapshot(agent_memory, MAX_STORED_SNAPSHOT);

    let counts = DigestCounts {
        events: events_kept.len(),
        chat_turns: chat_kept.len(),
        artifacts: artifacts.len(),
        learnings: learnings.len(),
        stored_snapshot: stored_snapshot.len(),
    };

    let empty = counts.events == 0
        && counts.chat_turns == 0
        && counts.artifacts == 0
        && counts.learnings == 0;

    let markdown = render_markdown(
        window_start_ms,
        window_end_ms,
        &events_kept,
        &chat_kept,
        &artifacts,
        &learnings,
        &stored_snapshot,
    );

    DigestBundle {
        window_start_ms,
        window_end_ms,
        counts,
        markdown,
        empty,
    }
}

/// Compose the final LLM-facing user message for the cron skill,
/// wrapping the evidence bundle with explicit instructions.
pub fn build_llm_message(bundle: &DigestBundle) -> String {
    format!(
        "Background cron: execute skill `{skill}`. First call **skill_run** with skill_id `{skill}` to load its contract, then use the **12h digest bundle** below to decide what (if anything) to persist via **store_memory** (with `memory_type` set to either `episodic` or `semantic`) and optional **record_learning**. Do not call recall/list tools to rebuild this bundle — it is already filtered and capped.\n\n{bundle}",
        skill = CONSOLIDATION_SKILL_ID,
        bundle = bundle.markdown,
    )
}

fn render_markdown(
    start_ms: u64,
    end_ms: u64,
    events: &[&RecentEvent],
    chat: &[ChatSnippet],
    artifacts: &[ArtifactRecord],
    learnings: &[String],
    stored_snapshot: &[MemoryRow],
) -> String {
    let mut out = String::new();
    out.push_str("## 12h digest bundle\n\n");
    out.push_str(&format!(
        "- **window:** {} → {} (UTC)\n",
        fmt_ts(start_ms),
        fmt_ts(end_ms)
    ));
    out.push_str(&format!(
        "- **counts:** events={} chat={} artifacts={} learnings={} stored_snapshot={}\n\n",
        events.len(),
        chat.len(),
        artifacts.len(),
        learnings.len(),
        stored_snapshot.len(),
    ));

    out.push_str("### Orchestrator milestones (most recent first)\n");
    if events.is_empty() {
        out.push_str("_(none recorded in window)_\n");
    } else {
        for ev in events {
            let sid = ev
                .session_id
                .as_deref()
                .map(|s| s.chars().take(8).collect::<String>())
                .unwrap_or_default();
            let tag = ev.tag.as_deref().unwrap_or("");
            out.push_str(&format!(
                "- `{}` [{kind}{tag_sep}{tag}]{sid_sep}{sid} — {summary}\n",
                fmt_ts(ev.ts_ms),
                kind = ev.kind,
                tag_sep = if tag.is_empty() { "" } else { ":" },
                tag = tag,
                sid_sep = if sid.is_empty() { "" } else { " sid=" },
                sid = sid,
                summary = truncate(&ev.summary, TEXT_MAX),
            ));
        }
    }
    out.push('\n');

    out.push_str("### Recent chat turns (most recent last)\n");
    if chat.is_empty() {
        out.push_str("_(no live chat turns cached)_\n");
    } else {
        for c in chat.iter().rev() {
            let sid = c.session_id.chars().take(8).collect::<String>();
            out.push_str(&format!(
                "- **{role}** [sid={sid}]: {text}\n",
                role = c.role,
                sid = sid,
                text = c.text,
            ));
        }
    }
    out.push('\n');

    out.push_str("### Workspace artifacts created / touched in window\n");
    if artifacts.is_empty() {
        out.push_str("_(no new exports / inbox items)_\n");
    } else {
        for a in artifacts {
            out.push_str(&format!(
                "- `{}` ({} bytes, {})\n",
                a.rel_path,
                a.size_bytes,
                fmt_ts(a.ts_ms)
            ));
        }
    }
    out.push('\n');

    out.push_str("### Journal entries in window (`.learnings/`)\n");
    if learnings.is_empty() {
        out.push_str("_(no new entries)_\n");
    } else {
        for l in learnings {
            out.push_str(&format!("- {}\n", truncate(l, BODY_MAX)));
        }
    }
    out.push('\n');

    out.push_str("### Already-stored memory snapshot (skip near-duplicates)\n");
    if stored_snapshot.is_empty() {
        out.push_str("_(memory is empty)_\n");
    } else {
        for row in stored_snapshot {
            out.push_str(&format!(
                "- id={id} type={mt} imp={imp:.2} — {sum}\n",
                id = row.id,
                mt = row.memory_type,
                imp = row.importance,
                sum = truncate(&row.summary, TEXT_MAX),
            ));
        }
    }
    out.push('\n');

    out.push_str("### Retention guidance (reiterated)\n");
    out.push_str("- Write **one** `episodic` summary of this 12h window (topics include `digest`, `12h`, `episodic`).\n");
    out.push_str("- Write **zero to ~5** `semantic` rules only for signals that recurred or were explicitly confirmed by the user.\n");
    out.push_str("- Skip one-off chatter, transient errors with clean recovery, and anything near-duplicate to the snapshot above.\n");
    out.push_str("- Every `store_memory` call MUST set `memory_type` explicitly.\n");

    out
}

fn recent_artifacts(workspace_root: &Path, cutoff_ms: u64, max: usize) -> Vec<ArtifactRecord> {
    let mut out: Vec<ArtifactRecord> = Vec::new();
    // Only scan directories where we expect durable agent-authored
    // artifacts. Arbitrary workspace churn (user-dropped source files)
    // is out of scope for consolidation.
    for sub in [
        "exports/reports",
        "exports/charts",
        "exports/tables",
        "exports",
        "inbox/processed",
        "inbox/failed",
    ] {
        let dir = workspace_root.join(sub);
        if !dir.is_dir() {
            continue;
        }
        walk_dir_shallow(&dir, 3, &mut |path| {
            if !path.is_file() {
                return;
            }
            let meta = match fs::metadata(path) {
                Ok(m) => m,
                Err(_) => return,
            };
            let ts_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            if ts_ms < cutoff_ms {
                return;
            }
            let rel = path
                .strip_prefix(workspace_root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| path.to_string_lossy().into_owned());
            out.push(ArtifactRecord {
                rel_path: rel,
                ts_ms,
                size_bytes: meta.len(),
            });
        });
    }
    out.sort_by(|a, b| b.ts_ms.cmp(&a.ts_ms));
    out.truncate(max);
    out
}

/// Bounded BFS with max depth. Avoid recursion to keep the stack
/// predictable even if a user nests folders aggressively.
fn walk_dir_shallow(root: &Path, max_depth: usize, visit: &mut dyn FnMut(&Path)) {
    let mut stack: Vec<(std::path::PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if depth + 1 <= max_depth {
                    stack.push((p, depth + 1));
                }
            } else {
                visit(&p);
            }
        }
    }
}

fn recent_learnings(workspace_root: &Path, cutoff_ms: u64, max: usize) -> Vec<String> {
    let dir = workspace_root.join(".learnings");
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut out: Vec<(u64, String)> = Vec::new();
    for name in ["LEARNINGS.md", "ERRORS.md", "FEATURE_REQUESTS.md"] {
        let p = dir.join(name);
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        };
        for chunk in text.split("\n### ").skip(1) {
            let first_line = chunk.lines().next().unwrap_or("").trim();
            // IDs are `{PREFIX}-{millis}` — extract the millis suffix
            // to time-filter without relying on filesystem mtime.
            let ts_ms = first_line
                .rsplit_once('-')
                .and_then(|(_, ms)| ms.parse::<u64>().ok())
                .unwrap_or(0);
            if ts_ms < cutoff_ms {
                continue;
            }
            let mut summary = String::new();
            for line in chunk.lines().skip(1) {
                if let Some(rest) = line.trim().strip_prefix("- **summary:**") {
                    summary = rest.trim().to_string();
                    break;
                }
            }
            if summary.is_empty() {
                continue;
            }
            out.push((ts_ms, format!("{name} {first_line}: {summary}")));
        }
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.truncate(max);
    out.into_iter().map(|(_, s)| s).collect()
}

fn stored_memory_snapshot(mem: &AgentMemory, max: usize) -> Vec<MemoryRow> {
    mem.recall("", max).unwrap_or_default()
}

fn fmt_ts(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let nsec = ((ms % 1000) * 1_000_000) as u32;
    Utc.timestamp_opt(secs, nsec)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| format!("{ms}ms"))
}

fn truncate(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        s.to_string()
    } else {
        let head: String = s.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn mk_buffer() -> RecentEventsBuffer {
        RecentEventsBuffer::new(5, 60)
    }

    #[test]
    fn buffer_caps_by_length() {
        let mut b = mk_buffer();
        for i in 0..10u64 {
            b.push(RecentEvent {
                ts_ms: now_ms() + i,
                kind: "tool".into(),
                session_id: None,
                summary: format!("#{i}"),
                tag: None,
            });
        }
        assert_eq!(b.len(), 5);
    }

    #[test]
    fn should_record_event_kind_allows_key_kinds() {
        assert!(should_record_event_kind("tool"));
        assert!(should_record_event_kind("turn_begin"));
        assert!(should_record_event_kind("llm_call_end"));
        assert!(!should_record_event_kind("llm_thinking_delta"));
        assert!(!should_record_event_kind("llm_content_delta"));
    }

    #[test]
    fn bundle_empty_when_nothing_happened() {
        let tmp = TempDir::new().unwrap();
        let mem = AgentMemory::open(tmp.path().join("memory.db")).unwrap();
        let bundle = build_digest_bundle(tmp.path(), &mem, &[], &[], DEFAULT_WINDOW_SECS);
        assert!(bundle.empty);
        assert_eq!(bundle.counts.events, 0);
        assert!(bundle.markdown.contains("12h digest bundle"));
    }

    #[test]
    fn bundle_counts_include_artifacts_and_events() {
        let tmp = TempDir::new().unwrap();
        let mem = AgentMemory::open(tmp.path().join("memory.db")).unwrap();
        let exports = tmp.path().join("exports/reports");
        fs::create_dir_all(&exports).unwrap();
        fs::write(exports.join("r1.md"), b"hello").unwrap();

        let events = vec![RecentEvent {
            ts_ms: now_ms(),
            kind: "tool".into(),
            session_id: Some("sess-abc".into()),
            summary: "store_memory ran".into(),
            tag: Some("store_memory".into()),
        }];
        let chat = vec![ChatSnippet {
            session_id: "sess-abc".into(),
            role: "user".into(),
            text: "remember I like terse replies".into(),
        }];
        let bundle = build_digest_bundle(tmp.path(), &mem, &events, &chat, DEFAULT_WINDOW_SECS);
        assert!(!bundle.empty);
        assert!(bundle.counts.artifacts >= 1);
        assert_eq!(bundle.counts.events, 1);
        assert_eq!(bundle.counts.chat_turns, 1);
        assert!(bundle.markdown.contains("exports/reports/r1.md"));
        assert!(bundle.markdown.contains("terse replies"));
    }

    #[test]
    fn window_filters_old_events() {
        let tmp = TempDir::new().unwrap();
        let mem = AgentMemory::open(tmp.path().join("memory.db")).unwrap();
        let old = RecentEvent {
            ts_ms: 1_000, // essentially epoch; far outside any window
            kind: "tool".into(),
            session_id: None,
            summary: "ancient".into(),
            tag: None,
        };
        let fresh = RecentEvent {
            ts_ms: now_ms(),
            kind: "turn_begin".into(),
            session_id: Some("s".into()),
            summary: "new".into(),
            tag: None,
        };
        let bundle = build_digest_bundle(tmp.path(), &mem, &[old, fresh], &[], DEFAULT_WINDOW_SECS);
        assert_eq!(bundle.counts.events, 1);
        assert!(bundle.markdown.contains("new"));
        assert!(!bundle.markdown.contains("ancient"));
    }

    #[test]
    fn build_llm_message_references_skill_id() {
        let tmp = TempDir::new().unwrap();
        let mem = AgentMemory::open(tmp.path().join("memory.db")).unwrap();
        let bundle = build_digest_bundle(tmp.path(), &mem, &[], &[], DEFAULT_WINDOW_SECS);
        let msg = build_llm_message(&bundle);
        assert!(msg.contains(CONSOLIDATION_SKILL_ID));
        assert!(msg.contains("12h digest bundle"));
    }

    #[test]
    fn bundle_respects_event_cap_and_recency_order() {
        let tmp = TempDir::new().unwrap();
        let mem = AgentMemory::open(tmp.path().join("memory.db")).unwrap();
        let base = now_ms();
        // Push 3x the cap to force truncation; timestamps increasing
        // so we can verify the bundle keeps the newest ones.
        let total = MAX_EVENTS * 3;
        let events: Vec<RecentEvent> = (0..total as u64)
            .map(|i| RecentEvent {
                ts_ms: base + i,
                kind: "tool".into(),
                session_id: Some("s".into()),
                summary: format!("evt-{i}"),
                tag: Some("x".into()),
            })
            .collect();
        let bundle = build_digest_bundle(tmp.path(), &mem, &events, &[], DEFAULT_WINDOW_SECS);
        assert_eq!(bundle.counts.events, MAX_EVENTS);
        // Most recent (highest i) MUST appear; the oldest 2/3 must not.
        let newest_tag = format!("evt-{}", total - 1);
        let oldest_tag = "evt-0".to_string();
        assert!(bundle.markdown.contains(&newest_tag));
        assert!(!bundle.markdown.contains(&oldest_tag));
    }

    #[test]
    fn bundle_chat_snippets_are_truncated() {
        let tmp = TempDir::new().unwrap();
        let mem = AgentMemory::open(tmp.path().join("memory.db")).unwrap();
        let long_text = "x".repeat(2_000);
        let chat = vec![ChatSnippet {
            session_id: "s".into(),
            role: "user".into(),
            text: long_text,
        }];
        let bundle = build_digest_bundle(tmp.path(), &mem, &[], &chat, DEFAULT_WINDOW_SECS);
        assert_eq!(bundle.counts.chat_turns, 1);
        // Truncation marker must appear (TEXT_MAX < 2_000).
        assert!(bundle.markdown.contains('…'));
    }

    #[test]
    fn bundle_includes_memory_type_in_snapshot() {
        use crate::agent_memory::MemoryType;
        let tmp = TempDir::new().unwrap();
        let mem = AgentMemory::open(tmp.path().join("memory.db")).unwrap();
        mem.store_typed(
            "rule: prefer terse replies",
            "",
            r#"["pref"]"#,
            0.8,
            "12h-cron",
            MemoryType::Semantic,
        )
        .unwrap();
        let bundle = build_digest_bundle(tmp.path(), &mem, &[], &[], DEFAULT_WINDOW_SECS);
        assert!(bundle.markdown.contains("type=semantic"));
        assert!(bundle.markdown.contains("prefer terse replies"));
    }
}
