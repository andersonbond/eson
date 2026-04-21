//! Claude tool implementations scoped to [`WorkspaceRoot`] (sandbox) plus SQLite agent memory and
//! optional workspace-scoped shell (Unix / macOS).

use crate::agent_memory::{AgentMemory, MemoryType};
use crate::embedder::EmbedClient;
use crate::learnings;
use crate::memory_client::MemoryClient;
use crate::skills::{load_skill_body, list_all_skills, parse_page_list};
use crate::vision;
use crate::workspace::WorkspaceRoot;
use rust_xlsxwriter::Workbook;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

const DEFAULT_READ_MAX: usize = 200_000;

/// Everything tool dispatch needs for one turn.
pub struct ToolContext<'a> {
    pub workspace: &'a WorkspaceRoot,
    pub memory: &'a AgentMemory,
    pub skills_root: &'a Path,
    pub socket_queue: &'a Mutex<Vec<(String, Value)>>,
    /// Per-turn multimodal routing — chosen via UI Settings ("Vision")
    /// then merged with `ESON_VISION_*` env vars by the caller. Lets the
    /// user pick Ollama / Anthropic / OpenAI without restarting the agent.
    pub vision: &'a vision::VisionConfig,
    /// Text embedder used by `search_images` to vectorize the natural
    /// language query before scoring it against `image_embeddings`.
    pub embedder: &'a EmbedClient,
    /// HTTP client for the `eson-memory` sidecar; used by
    /// `search_images` to run the cosine top-K query. When the sidecar
    /// is down the tool returns an explanatory error instead of
    /// silently returning 0 hits.
    pub memory_client: &'a MemoryClient,
}

fn queue_socket(ctx: &ToolContext, event: &str, payload: Value) {
    if let Ok(mut g) = ctx.socket_queue.lock() {
        g.push((event.to_string(), payload));
    }
}

fn sanitize_export_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    s = s.trim_matches('-').chars().take(80).collect();
    if s.is_empty() {
        "export".into()
    } else {
        s
    }
}

fn pdf_max_pages() -> u32 {
    std::env::var("ESON_VISION_PDF_MAX_PAGES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5u32)
        .clamp(1, 50)
}

/// Anthropic Messages API `tools` entries (JSON schema).
pub fn tool_specs() -> Vec<Value> {
    let mut tools = vec![
        json!({
            "name": "workspace_list",
            "description": "List files and folders inside the Eson sandboxed workspace only. Paths are relative to the workspace root (e.g. \"images\", \"notes\"). Use \"\" or omit path for the workspace root. Do not use ~, /Users, or absolute paths — they are rejected.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative directory path; empty string lists root."
                    }
                },
                "required": []
            }
        }),
        json!({
            "name": "workspace_read",
            "description": "Read a file from the sandboxed workspace as UTF-8 text (lossy if not valid UTF-8). Path is workspace-relative only. For images or binaries, list the folder instead.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative file path, e.g. \"notes/daily.md\""
                    },
                    "max_bytes": {
                        "type": "integer",
                        "description": "Optional max size in bytes (default 200000)."
                    }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "workspace_grep",
            "description": "Search file paths and UTF-8 file contents under the sandboxed workspace. Default **flexible** mode understands natural phrases (e.g. \"atd negative file\" matches `atd_negative` paths) by matching meaningful words after stripping fillers (the, look, file, …) and ignoring `_`/`-`/spaces between letters. Prefer this over run_terminal/grep. Paths are workspace-relative; \"\" = whole workspace. Skips large files, likely-binary content, and heavy dirs (.git, node_modules, target).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "What to search for: literal substring, or with flexible=true also natural-language paraphrases (see tool description)."
                    },
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative directory to search under; empty = entire workspace."
                    },
                    "case_insensitive": {
                        "type": "boolean",
                        "description": "If true, match ignoring ASCII case (default false)."
                    },
                    "flexible": {
                        "type": "boolean",
                        "description": "If true (default): also match when all meaningful word pieces appear in the path or line after folding `atd_negative` → letters `atdnegative`; strips common English filler words from the pattern. Set false for strict substring-only matching."
                    },
                    "max_matches": {
                        "type": "integer",
                        "description": "Stop after this many line hits (default 80, max 300)."
                    },
                    "max_file_bytes": {
                        "type": "integer",
                        "description": "Skip files larger than this (default 400000, max 1000000)."
                    }
                },
                "required": ["pattern"]
            }
        }),
        json!({
            "name": "store_memory",
            "description": "Persist a durable memory in the workspace SQLite database (db/memory.db) for future sessions. Use for user preferences, facts to remember, decisions, or reminders — like an always-on memory layer. Include a short summary; optional longer body and topic tags. Set `memory_type` to `episodic` for a time-bound event / window summary or `semantic` for a generalized rule / fact (default: `episodic`).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "summary": { "type": "string", "description": "Short headline (required)." },
                    "body": { "type": "string", "description": "Optional longer text." },
                    "topics": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags for recall."
                    },
                    "importance": {
                        "type": "number",
                        "description": "0.0–1.0 (default 0.5)."
                    },
                    "source": { "type": "string", "description": "Optional provenance label." },
                    "memory_type": {
                        "type": "string",
                        "enum": ["episodic", "semantic"],
                        "description": "`episodic` = time-bound event/digest; `semantic` = generalized durable rule. Defaults to `episodic` when omitted."
                    }
                },
                "required": ["summary"]
            }
        }),
        json!({
            "name": "recall_memory",
            "description": "Search stored memories (db/memory.db) by keywords. Returns the best-matching rows with id, summary, body, topics, importance, memory_type. Use before answering when prior preferences or facts may apply. Pass `memory_type` (`episodic` or `semantic`) to restrict the scan to one track.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Search terms; empty returns most recent memories."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max rows (default 20, max 100)."
                    },
                    "memory_type": {
                        "type": "string",
                        "enum": ["episodic", "semantic"],
                        "description": "Optional filter: only return rows of this memory type."
                    }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "skill_list",
            "description": "List  skills from the skills/ folder (cron, inbox, user, auto) with id, name, description, triggers.",
            "input_schema": { "type": "object", "properties": {}, "required": [] }
        }),
        json!({
            "name": "skill_run",
            "description": "Load the markdown body of one skill by id (e.g. cron/consolidate-memory, inbox/pdf). The model should follow these instructions in the current turn.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "skill_id": { "type": "string", "description": "Category/name without .md, e.g. inbox/pdf" }
                },
                "required": ["skill_id"]
            }
        }),
        json!({
            "name": "update_user_model",
            "description": "Set a key in the Honcho-style user_model table (db/memory.db) for long-lived preferences.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string" },
                    "value": { "type": "string" }
                },
                "required": ["key", "value"]
            }
        }),
        json!({
            "name": "recall_user_model",
            "description": "Read user_model keys. Omit key to list all (up to 200).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Optional exact key." }
                },
                "required": []
            }
        }),
        json!({
            "name": "summarize_session",
            "description": "Append a compressed session summary row for RAG-style recall later.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "summary": { "type": "string" },
                    "token_estimate": { "type": "integer", "description": "Optional rough token count." }
                },
                "required": ["session_id", "summary"]
            }
        }),
        json!({
            "name": "record_learning",
            "description": "Append a structured learning (LRN), error (ERR), or feature (FEAT) to workspace/.learnings/.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "description": "lrn | err | feat" },
                    "summary": { "type": "string" },
                    "body": { "type": "string" },
                    "tags": { "type": "string" }
                },
                "required": ["kind", "summary"]
            }
        }),
        json!({
            "name": "propose_skill",
            "description": "Write a draft SKILL.md under skills/auto/ (disabled by default) for human promotion.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "when_to_use": { "type": "string" },
                    "instructions": { "type": "string" }
                },
                "required": ["name", "when_to_use", "instructions"]
            }
        }),
        json!({
            "name": "render_chart",
            "description": "Build a standalone Chart.js HTML file under workspace/exports/charts/ and notify the UI via chart_render.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "chart_type": { "type": "string", "description": "bar | line | area | pie | scatter | radar" },
                    "title": { "type": "string" },
                    "labels_json": { "type": "string", "description": "JSON array of x labels as a string." },
                    "series_json": { "type": "string", "description": "JSON array of {name, values} as a string." },
                    "output_name": { "type": "string" }
                },
                "required": ["title", "labels_json", "series_json"]
            }
        }),
        json!({
            "name": "search_images",
            "description": "Semantic search over indexed workspace images (uses captions + OCR embedded with the local text model). Returns the top-K matches with workspace-relative path, caption, OCR snippet, and similarity score. Run `POST /ingestion/scan-images` first if you expect results but get none.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Natural-language description of what you are looking for (e.g. 'invoices from April', 'screenshots with error dialogs')."
                    },
                    "top_k": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "description": "How many hits to return (default 5)."
                    }
                },
                "required": ["query"]
            }
        }),
        json!({
            "name": "analyze_visual",
            "description": "Local multimodal understanding via Ollama: images (png/jpg/jpeg/gif/webp) or PDF pages rasterized with pdftoppm. Returns JSON text summary.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workspace-relative file path." },
                    "question": { "type": "string" },
                    "pages": { "type": "string", "description": "For PDFs: e.g. 1-3 or 1,4 (optional)." }
                },
                "required": ["path"]
            }
        }),
        json!({
            "name": "pdf_to_table",
            "description": "Rasterize PDF pages and ask the local vision model for JSON rows; writes CSV or XLSX under exports/tables/.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "pdf_path": { "type": "string" },
                    "output_format": { "type": "string", "description": "csv or xlsx (default csv)." },
                    "output_name": { "type": "string" },
                    "pages": { "type": "string" }
                },
                "required": ["pdf_path"]
            }
        }),
    ];

    #[cfg(unix)]
    if terminal_tool_enabled() {
        tools.push(json!({
            "name": "run_terminal",
            "description": "Run a shell command in zsh (macOS) or sh (other Unix) with working directory set to the sandboxed workspace root. stdout/stderr are captured. Dangerous patterns (e.g. rm -rf /, raw dd to disks) are blocked. Set ESON_TERMINAL_ENABLED=0 to disable.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Single shell command or pipeline as you would type in terminal (e.g. \"ls -la\", \"git status\")."
                    }
                },
                "required": ["command"]
            }
        }));
    }

    tools
}

/// Run one tool; returns a string passed back to Claude as `tool_result` content.
pub fn dispatch(ctx: &ToolContext, name: &str, input: &Value) -> String {
    match name {
        "workspace_list" => tool_list(ctx.workspace, input),
        "workspace_read" => tool_read(ctx.workspace, input),
        "workspace_grep" => tool_workspace_grep(ctx.workspace, input),
        "store_memory" => tool_store_memory(ctx.memory, input),
        "recall_memory" => tool_recall_memory(ctx.memory, input),
        "skill_list" => tool_skill_list(ctx),
        "skill_run" => tool_skill_run(ctx, input),
        "update_user_model" => tool_update_user_model(ctx.memory, input),
        "recall_user_model" => tool_recall_user_model(ctx.memory, input),
        "summarize_session" => tool_summarize_session(ctx.memory, input),
        "record_learning" => tool_record_learning(ctx, input),
        "propose_skill" => tool_propose_skill(ctx, input),
        "render_chart" => tool_render_chart(ctx, input),
        "search_images" => tool_search_images(ctx, input),
        "analyze_visual" => tool_analyze_visual(ctx, input),
        "pdf_to_table" => tool_pdf_to_table(ctx, input),
        #[cfg(unix)]
        "run_terminal" => tool_run_terminal(ctx.workspace, input),
        _ => format!("error: unknown tool `{name}`"),
    }
}

fn tool_list(ws: &WorkspaceRoot, input: &Value) -> String {
    let path = input
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match ws.list_directory(&path) {
        Ok((rel, entries)) => match serde_json::to_string_pretty(&json!({ "path": rel, "entries": entries })) {
            Ok(s) => s,
            Err(e) => format!("error: serialize listing: {e}"),
        },
        Err(e) => format!("error: {e}"),
    }
}

fn tool_read(ws: &WorkspaceRoot, input: &Value) -> String {
    let Some(path) = input.get("path").and_then(|v| v.as_str()) else {
        return "error: missing required field `path`".to_string();
    };
    let max = input
        .get("max_bytes")
        .and_then(|v| v.as_u64())
        .map(|n| n.min(500_000) as usize)
        .unwrap_or(DEFAULT_READ_MAX);
    match ws.read_text_file(path, max) {
        Ok(text) => text,
        Err(e) => format!("error: {e}"),
    }
}

fn grep_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | "node_modules" | "target" | ".svn" | "__pycache__" | ".venv" | "dist" | "build"
    )
}

/// Lowercase letters and digits only — `atd_negative` and `atd negative` both become `atdnegative`.
fn alphanumeric_fold(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric())
        .collect()
}

/// Filler words dropped so \"look for the atd negative file\" → tokens [atd, negative].
const GREP_STOP_WORDS: &[&str] = &[
    "a", "an", "the", "for", "to", "of", "in", "on", "at", "by", "as", "is", "are", "was", "were",
    "be", "been", "being", "and", "or", "not", "no", "it", "its", "if", "into", "with", "from",
    "this", "that", "these", "those", "there", "here", "look", "looks", "looking", "find", "finds",
    "finding", "search", "searches", "searching", "show", "shows", "give", "gives", "get", "gets",
    "list", "lists", "open", "opens", "read", "reads", "please", "can", "could", "would", "should",
    "about", "how", "what", "when", "where", "which", "who", "whom", "file", "files", "folder",
    "folders", "directory", "directories", "name", "named", "called", "call", "me", "my", "your",
    "our", "we", "you", "i", "do", "does", "did", "done", "any", "some", "all", "just", "like",
    "need", "want", "help", "see", "tell", "use", "using", "used", "thing", "things", "stuff",
];

fn nl_meaningful_tokens(pattern: &str) -> Vec<String> {
    pattern
        .split(|c: char| !c.is_alphanumeric())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .filter(|t| t.len() >= 2 && !GREP_STOP_WORDS.contains(&t.as_str()))
        .collect()
}

/// Match `pattern` against `hay` (a line of text or a relative path). When `flexible`, also match
/// if folding underscores/spaces away links the query to identifiers like `atd_negative`.
fn flexible_hay_match(hay: &str, pattern: &str, case_insensitive: bool, flexible: bool) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if case_insensitive {
        let h = hay.to_lowercase();
        let p = pattern.to_lowercase();
        if h.contains(&p) {
            return true;
        }
    } else if hay.contains(pattern) {
        return true;
    }
    if !flexible {
        return false;
    }
    let hf = alphanumeric_fold(hay);
    let pf = alphanumeric_fold(pattern);
    if pf.len() >= 2 && hf.contains(&pf) {
        return true;
    }
    let tokens = nl_meaningful_tokens(pattern);
    if tokens.is_empty() {
        return false;
    }
    tokens.iter().all(|t| {
        let tf: String = t.chars().filter(|c| c.is_alphanumeric()).collect();
        tf.len() >= 2 && hf.contains(&tf)
    })
}

#[allow(clippy::too_many_arguments)]
fn grep_collect(
    ws_root: &Path,
    dir: &Path,
    pattern: &str,
    case_insensitive: bool,
    flexible: bool,
    max_hits: usize,
    max_file_bytes: usize,
    hits: &mut Vec<Value>,
    files_scanned: &mut u32,
) {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return;
    };
    let mut stack: Vec<PathBuf> = read_dir.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    stack.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    for path in stack {
        if hits.len() >= max_hits {
            return;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let meta = match fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let rel_path = path
            .strip_prefix(ws_root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| "?".into());

        if meta.is_dir() {
            if grep_skip_dir(&name) {
                continue;
            }
            if hits.len() < max_hits
                && flexible_hay_match(&rel_path, pattern, case_insensitive, flexible)
            {
                hits.push(json!({
                    "path": rel_path,
                    "line": 0,
                    "text": "(directory path match)",
                    "match_kind": "path",
                }));
            }
            grep_collect(
                ws_root,
                &path,
                pattern,
                case_insensitive,
                flexible,
                max_hits,
                max_file_bytes,
                hits,
                files_scanned,
            );
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let size = meta.len() as usize;
        if size > max_file_bytes {
            continue;
        }
        if hits.len() < max_hits && flexible_hay_match(&rel_path, pattern, case_insensitive, flexible)
        {
            hits.push(json!({
                "path": rel_path,
                "line": 0,
                "text": "(file path match)",
                "match_kind": "path",
            }));
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if bytes.contains(&0) {
            continue;
        }
        *files_scanned += 1;
        let text = String::from_utf8_lossy(&bytes);
        for (i, line) in text.lines().enumerate() {
            if hits.len() >= max_hits {
                return;
            }
            if flexible_hay_match(line, pattern, case_insensitive, flexible) {
                let snippet: String = line.chars().take(480).collect();
                hits.push(json!({
                    "path": rel_path,
                    "line": i + 1,
                    "text": snippet,
                    "match_kind": "content",
                }));
            }
        }
    }
}

fn tool_workspace_grep(ws: &WorkspaceRoot, input: &Value) -> String {
    let Some(pattern) = input.get("pattern").and_then(|v| v.as_str()) else {
        return "error: missing required field `pattern`".to_string();
    };
    if pattern.is_empty() {
        return "error: `pattern` must be non-empty".to_string();
    }
    let rel = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let case_insensitive = input
        .get("case_insensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let flexible = input
        .get("flexible")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let max_hits = input
        .get("max_matches")
        .and_then(|v| v.as_u64())
        .unwrap_or(80)
        .min(300) as usize;
    let max_file_bytes = input
        .get("max_file_bytes")
        .and_then(|v| v.as_u64())
        .unwrap_or(400_000)
        .min(1_000_000) as usize;

    let (base_rel, abs_base) = match ws.resolve_directory(rel) {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };
    let ws_root = ws.root().to_path_buf();
    let mut hits: Vec<Value> = Vec::new();
    let mut files_scanned: u32 = 0;
    grep_collect(
        &ws_root,
        &abs_base,
        pattern,
        case_insensitive,
        flexible,
        max_hits,
        max_file_bytes,
        &mut hits,
        &mut files_scanned,
    );
    let truncated = hits.len() >= max_hits;
    match serde_json::to_string_pretty(&json!({
        "pattern": pattern,
        "path": base_rel,
        "flexible": flexible,
        "hits": hits,
        "hit_count": hits.len(),
        "files_scanned": files_scanned,
        "truncated": truncated,
    })) {
        Ok(s) => s,
        Err(e) => format!("error: serialize: {e}"),
    }
}

fn topics_json(input: &Value) -> String {
    match input.get("topics") {
        Some(Value::Array(a)) => json!(a).to_string(),
        Some(Value::String(s)) => json!([s]).to_string(),
        _ => "[]".to_string(),
    }
}

fn tool_store_memory(mem: &AgentMemory, input: &Value) -> String {
    let Some(summary) = input.get("summary").and_then(|v| v.as_str()) else {
        return "error: missing required field `summary`".to_string();
    };
    let body = input
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let topics = topics_json(input);
    let importance = input
        .get("importance")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.5);
    let source = input
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("agent");
    // `memory_type` is optional on the API, but when the caller sends
    // a non-empty value that doesn't parse to one of the known kinds
    // we reject the write rather than silently defaulting — a typo
    // here would otherwise bypass the episodic/semantic distinction
    // the 12h consolidation pass relies on.
    let memory_type = match input.get("memory_type").and_then(|v| v.as_str()) {
        None | Some("") => MemoryType::default(),
        Some(raw) => match MemoryType::parse(raw) {
            Some(mt) => mt,
            None => return format!(
                "error: invalid memory_type `{raw}` (expected `episodic` or `semantic`)"
            ),
        },
    };
    match mem.store_typed(summary, body, &topics, importance, source, memory_type) {
        Ok(id) => match serde_json::to_string_pretty(&json!({
            "ok": true,
            "id": id,
            "memory_type": memory_type.as_str(),
            "db": mem.db_path().display().to_string(),
        })) {
            Ok(s) => s,
            Err(e) => format!("error: serialize: {e}"),
        },
        Err(e) => format!("error: {e}"),
    }
}

fn tool_recall_memory(mem: &AgentMemory, input: &Value) -> String {
    let query = input
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let limit = input
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(20) as usize;
    let filter = match input.get("memory_type").and_then(|v| v.as_str()) {
        None | Some("") => None,
        Some(raw) => match MemoryType::parse(raw) {
            Some(mt) => Some(mt),
            None => return format!(
                "error: invalid memory_type `{raw}` (expected `episodic` or `semantic`)"
            ),
        },
    };
    match mem.recall_filtered(query, limit, filter) {
        Ok(rows) => match serde_json::to_string_pretty(&json!({
            "memories": rows,
            "memory_type_filter": filter.map(|m| m.as_str()),
            "db": mem.db_path().display().to_string(),
        })) {
            Ok(s) => s,
            Err(e) => format!("error: serialize: {e}"),
        },
        Err(e) => format!("error: {e}"),
    }
}

fn tool_skill_list(ctx: &ToolContext) -> String {
    let items: Vec<Value> = list_all_skills(ctx.skills_root)
        .into_iter()
        .map(|s| {
            json!({
                "id": s.id,
                "name": s.name,
                "description": s.description,
                "enabled": s.enabled,
                "category": s.category,
                "cron": s.cron,
                "inbox_ext": s.inbox_ext,
            })
        })
        .collect();
    serde_json::to_string_pretty(&json!({ "skills": items })).unwrap_or_else(|e| format!("error: {e}"))
}

fn tool_skill_run(ctx: &ToolContext, input: &Value) -> String {
    let Some(id) = input.get("skill_id").and_then(|v| v.as_str()) else {
        return "error: missing skill_id".to_string();
    };
    match load_skill_body(ctx.skills_root, id) {
        Ok(body) => serde_json::to_string_pretty(&json!({
            "skill_id": id,
            "markdown": body,
        }))
        .unwrap_or_else(|e| format!("error: {e}")),
        Err(e) => format!("error: {e}"),
    }
}

fn tool_update_user_model(mem: &AgentMemory, input: &Value) -> String {
    let Some(key) = input.get("key").and_then(|v| v.as_str()) else {
        return "error: missing key".to_string();
    };
    let Some(value) = input.get("value").and_then(|v| v.as_str()) else {
        return "error: missing value".to_string();
    };
    match mem.user_model_set(key, value) {
        Ok(()) => json!({"ok": true, "key": key}).to_string(),
        Err(e) => format!("error: {e}"),
    }
}

fn tool_recall_user_model(mem: &AgentMemory, input: &Value) -> String {
    let key = input.get("key").and_then(|v| v.as_str());
    match mem.user_model_get(key) {
        Ok(rows) => serde_json::to_string_pretty(&json!({ "entries": rows.iter().map(|(k,v)| json!({"key": k, "value": v})).collect::<Vec<_>>() }))
            .unwrap_or_else(|e| format!("error: {e}")),
        Err(e) => format!("error: {e}"),
    }
}

fn tool_summarize_session(mem: &AgentMemory, input: &Value) -> String {
    let Some(sid) = input.get("session_id").and_then(|v| v.as_str()) else {
        return "error: missing session_id".to_string();
    };
    let Some(summary) = input.get("summary").and_then(|v| v.as_str()) else {
        return "error: missing summary".to_string();
    };
    let te = input
        .get("token_estimate")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    match mem.session_summary_append(sid, summary, te) {
        Ok(id) => json!({"ok": true, "row_id": id}).to_string(),
        Err(e) => format!("error: {e}"),
    }
}

fn tool_record_learning(ctx: &ToolContext, input: &Value) -> String {
    let kind = input.get("kind").and_then(|v| v.as_str()).unwrap_or("lrn");
    let summary = input.get("summary").and_then(|v| v.as_str()).unwrap_or("");
    if summary.is_empty() {
        return "error: missing summary".to_string();
    }
    let body = input.get("body").and_then(|v| v.as_str());
    let tags = input.get("tags").and_then(|v| v.as_str());
    match learnings::append_learning(ctx.workspace.root(), kind, summary, body, tags) {
        Ok(id) => json!({"ok": true, "id": id}).to_string(),
        Err(e) => format!("error: {e}"),
    }
}

fn tool_propose_skill(ctx: &ToolContext, input: &Value) -> String {
    let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("skill");
    let when = input
        .get("when_to_use")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let instr = input
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let slug = learnings::slugify(name);
    match learnings::write_proposed_skill(ctx.skills_root, &slug, name, when, instr) {
        Ok(path) => json!({"ok": true, "path": path}).to_string(),
        Err(e) => format!("error: {e}"),
    }
}

fn tool_render_chart(ctx: &ToolContext, input: &Value) -> String {
    let chart_type = input
        .get("chart_type")
        .and_then(|v| v.as_str())
        .unwrap_or("bar");
    let title = input.get("title").and_then(|v| v.as_str()).unwrap_or("Chart");
    let labels_s = input
        .get("labels_json")
        .and_then(|v| v.as_str())
        .unwrap_or("[]");
    let series_s = input
        .get("series_json")
        .and_then(|v| v.as_str())
        .unwrap_or("[]");
    let labels: Value = serde_json::from_str(labels_s).unwrap_or(json!([]));
    let series: Value = serde_json::from_str(series_s).unwrap_or(json!([]));
    let out = input
        .get("output_name")
        .and_then(|v| v.as_str())
        .unwrap_or("chart");
    let safe = sanitize_export_name(out);
    let rel = format!("exports/charts/{safe}.html");
    let html = build_chart_html(chart_type, title, &labels, &series);
    if let Err(e) = ctx.workspace.write_bytes_file(&rel, html.as_bytes()) {
        return format!("error: write {rel}: {e}");
    }
    queue_socket(
        ctx,
        "chart_render",
        json!({
            "chart_type": chart_type,
            "title": title,
            "labels": labels,
            "series": series,
            "workspace_path": rel,
        }),
    );
    json!({"ok": true, "path": rel, "note": "Chart.js loaded from CDN in exported HTML; inline preview uses app bundle."}).to_string()
}

fn build_chart_html(chart_type: &str, title: &str, labels: &Value, series: &Value) -> String {
    let labels_json = serde_json::to_string(labels).unwrap_or_else(|_| "[]".into());
    let series_json = serde_json::to_string(series).unwrap_or_else(|_| "[]".into());
    let title_json = serde_json::to_string(title).unwrap_or_else(|_| "\"\"".into());
    let ctype = serde_json::to_string(chart_type).unwrap_or_else(|_| "\"bar\"".into());
    // NOTE: every `{labels_json}` / `{series_json}` / `{title_json}` / `{ctype}`
    // value below is the output of `serde_json::to_string(...)`, which is a
    // *valid JS literal* on its own (e.g. `"Cebu Holdings Trends"`,
    // `["Q1","Q2"]`). Do NOT wrap them in `JSON.parse(...)` — that double-
    // decodes the string and throws "Unexpected token … is not valid JSON",
    // which silently aborts the chart script and leaves a blank canvas.
    format!(
        r##"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>{title_plain}</title>
<meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap" rel="stylesheet">
<script src="https://cdn.jsdelivr.net/npm/chart.js@4.4.1/dist/chart.umd.min.js"></script>
<style>
  :root {{
    --bg-0: #0b0f14;
    --bg-1: #121821;
    --bg-2: #1b2330;
    --border: rgba(255, 255, 255, 0.08);
    --text: #e6ebf2;
    --text-dim: #8b95a7;
    --brand: #7aa2f7;
  }}
  * {{ box-sizing: border-box; }}
  html, body {{ height: 100%; }}
  body {{
    margin: 0;
    padding: 40px 32px;
    font-family: 'Inter', system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif;
    background:
      radial-gradient(1200px 500px at 10% -10%, rgba(122, 162, 247, 0.10), transparent 60%),
      radial-gradient(1000px 600px at 100% 110%, rgba(187, 154, 247, 0.08), transparent 55%),
      var(--bg-0);
    color: var(--text);
    -webkit-font-smoothing: antialiased;
    min-height: 100vh;
  }}
  .card {{
    max-width: 1100px;
    margin: 0 auto;
    background: linear-gradient(180deg, var(--bg-1), var(--bg-0));
    border: 1px solid var(--border);
    border-radius: 16px;
    padding: 28px 32px 32px;
    box-shadow: 0 20px 60px rgba(0, 0, 0, 0.45), 0 1px 0 rgba(255, 255, 255, 0.04) inset;
  }}
  .eyebrow {{
    font-family: 'JetBrains Mono', ui-monospace, monospace;
    font-size: 11px;
    font-weight: 500;
    letter-spacing: 0.18em;
    text-transform: uppercase;
    color: var(--brand);
    opacity: 0.85;
    margin: 0 0 8px;
  }}
  h1 {{
    margin: 0 0 20px;
    font-size: 22px;
    font-weight: 700;
    letter-spacing: -0.01em;
    color: var(--text);
  }}
  .chart-wrap {{
    position: relative;
    height: min(62vh, 520px);
  }}
  canvas {{ width: 100% !important; height: 100% !important; }}
  .footer {{
    margin-top: 18px;
    padding-top: 14px;
    border-top: 1px solid var(--border);
    font-family: 'JetBrains Mono', ui-monospace, monospace;
    font-size: 11px;
    color: var(--text-dim);
    display: flex;
    justify-content: space-between;
    flex-wrap: wrap;
    gap: 8px;
  }}
  .footer a {{ color: var(--text-dim); text-decoration: none; border-bottom: 1px dotted currentColor; }}
  .error {{ color: #f87171; font-size: 13px; margin-top: 12px; }}
</style>
</head><body>
<div class="card">
  <p class="eyebrow">Eson chart</p>
  <h1>{title_plain}</h1>
  <div class="chart-wrap"><canvas id="c"></canvas></div>
  <div class="footer">
    <span>Generated by Eson · <span id="stamp"></span></span>
    <span>Chart.js · Inter</span>
  </div>
</div>
<noscript><p class="error">This chart needs JavaScript and the Chart.js CDN to render.</p></noscript>
<script>
(function() {{
  const stamp = document.getElementById('stamp');
  if (stamp) {{
    try {{ stamp.textContent = new Date().toLocaleString(); }} catch (e) {{}}
  }}
  if (typeof Chart === 'undefined') {{
    const card = document.querySelector('.card');
    if (card) card.insertAdjacentHTML('beforeend',
      '<p class="error">Chart.js failed to load. Check your network and reopen this file.</p>');
    return;
  }}

  const labels = {labels_json};
  const seriesRaw = {series_json};
  const chartType = {ctype};
  const titleText = {title_json};

  // Curated palette (Tableau 10 + tweaks) — distinguishable, dark-friendly,
  // and kind to colorblind eyes.
  const PALETTE = [
    '#7aa2f7', // blue
    '#ff9e64', // orange
    '#9ece6a', // green
    '#f7768e', // red
    '#bb9af7', // purple
    '#e0af68', // amber
    '#7dcfff', // cyan
    '#c0caf5', // lavender
    '#ff007c', // magenta
    '#73daca', // teal
  ];
  const hexToRgba = function(hex, a) {{
    const h = hex.replace('#', '');
    const r = parseInt(h.substring(0, 2), 16);
    const g = parseInt(h.substring(2, 4), 16);
    const b = parseInt(h.substring(4, 6), 16);
    return 'rgba(' + r + ',' + g + ',' + b + ',' + a + ')';
  }};

  const isArea = chartType === 'area';
  const isPieLike = (chartType === 'pie' || chartType === 'doughnut' || chartType === 'polarArea');

  const datasets = (Array.isArray(seriesRaw) ? seriesRaw : []).map(function(s, i) {{
    const color = PALETTE[i % PALETTE.length];
    if (isPieLike) {{
      const values = (s && Array.isArray(s.values)) ? s.values : [];
      return {{
        label: (s && s.name) ? s.name : ('Series ' + (i + 1)),
        data: values,
        backgroundColor: values.map(function(_, j) {{ return PALETTE[j % PALETTE.length]; }}),
        borderColor: 'rgba(11,15,20,0.9)',
        borderWidth: 2,
        hoverOffset: 6,
      }};
    }}
    return {{
      label: (s && s.name) ? s.name : ('Series ' + (i + 1)),
      data: (s && Array.isArray(s.values)) ? s.values : [],
      borderColor: color,
      backgroundColor: isArea ? hexToRgba(color, 0.22) : hexToRgba(color, 0.78),
      borderWidth: 2,
      tension: 0.35,
      fill: isArea,
      pointRadius: 0,
      pointHoverRadius: 5,
      pointBackgroundColor: color,
      pointBorderColor: color,
      pointHoverBorderColor: '#0b0f14',
      pointHoverBorderWidth: 2,
      borderRadius: chartType === 'bar' ? 6 : undefined,
      maxBarThickness: 48,
      categoryPercentage: 0.75,
      barPercentage: 0.85,
    }};
  }});

  const chartJsType = isArea ? 'line' : chartType;

  Chart.defaults.font.family = "'Inter', system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif";
  Chart.defaults.font.size = 12;
  Chart.defaults.color = '#c8d1e0';

  const gridColor = 'rgba(255, 255, 255, 0.06)';
  const tickColor = '#8b95a7';
  const legendColor = '#e6ebf2';
  const tooltipBg = '#121821';

  const baseOpts = {{
    responsive: true,
    maintainAspectRatio: false,
    interaction: {{ mode: 'index', intersect: false }},
    plugins: {{
      title: {{ display: false }},
      legend: {{
        position: 'bottom',
        align: 'start',
        labels: {{
          color: legendColor,
          usePointStyle: true,
          pointStyle: 'circle',
          boxWidth: 8,
          boxHeight: 8,
          padding: 18,
          font: {{ size: 12, weight: '500' }},
        }},
      }},
      tooltip: {{
        backgroundColor: tooltipBg,
        borderColor: 'rgba(255,255,255,0.08)',
        borderWidth: 1,
        padding: 10,
        titleColor: '#e6ebf2',
        bodyColor: '#c8d1e0',
        titleFont: {{ family: "'Inter'", weight: '600', size: 12 }},
        bodyFont: {{ family: "'JetBrains Mono'", size: 12 }},
        cornerRadius: 8,
        displayColors: true,
        boxPadding: 4,
      }},
    }},
    scales: isPieLike ? undefined : {{
      x: {{
        grid: {{ color: gridColor, drawTicks: false }},
        border: {{ color: gridColor }},
        ticks: {{ color: tickColor, padding: 6, maxRotation: 0, autoSkipPadding: 16 }},
      }},
      y: {{
        beginAtZero: true,
        grid: {{ color: gridColor, drawTicks: false }},
        border: {{ display: false }},
        ticks: {{
          color: tickColor,
          padding: 8,
          callback: function(v) {{
            if (typeof v !== 'number' || !isFinite(v)) return v;
            const abs = Math.abs(v);
            if (abs >= 1e9) return (v / 1e9).toFixed(abs < 1e10 ? 2 : 1) + 'B';
            if (abs >= 1e6) return (v / 1e6).toFixed(abs < 1e7 ? 2 : 1) + 'M';
            if (abs >= 1e3) return (v / 1e3).toFixed(abs < 1e4 ? 2 : 1) + 'k';
            return v.toLocaleString();
          }},
        }},
      }},
    }},
  }};

  try {{
    new Chart(document.getElementById('c'), {{
      type: chartJsType,
      data: {{ labels: labels, datasets: datasets }},
      options: baseOpts,
    }});
  }} catch (err) {{
    const card = document.querySelector('.card');
    if (card) card.insertAdjacentHTML('beforeend',
      '<p class="error">Failed to render chart: ' + (err && err.message ? err.message : err) + '</p>');
  }}
}})();
</script></body></html>"##,
        title_plain = title.replace('&', "&amp;").replace('<', "&lt;"),
        labels_json = labels_json,
        series_json = series_json,
        ctype = ctype,
        title_json = title_json,
    )
}

/// Handler for the `search_images` tool.
///
/// Two async calls hide behind this synchronous facade — embedding
/// the query and running the cosine search via the sidecar. Both are
/// bridged onto the current Tokio runtime via `block_on`, which is
/// safe because the tool dispatch is already wrapped in
/// `tokio::task::block_in_place` (see `main.rs`).
fn tool_search_images(ctx: &ToolContext, input: &Value) -> String {
    let Some(query) = input.get("query").and_then(|v| v.as_str()) else {
        return "error: missing required field `query`".to_string();
    };
    let query = query.trim();
    if query.is_empty() {
        return "error: `query` must be non-empty".to_string();
    }
    let top_k = input
        .get("top_k")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .clamp(1, 20) as usize;

    let rt = tokio::runtime::Handle::current();
    let vec = match rt.block_on(ctx.embedder.embed(query)) {
        Ok(v) => v,
        Err(e) => return format!("error: embed query: {e}"),
    };
    let hits = match rt.block_on(ctx.memory_client.search_images(
        ctx.embedder.model(),
        &vec,
        top_k,
    )) {
        Ok(h) => h,
        Err(e) => return format!(
            "error: sidecar search_images: {e} (is eson-memory running?)"
        ),
    };

    let ws_root = ctx.workspace.root();
    let results: Vec<Value> = hits
        .into_iter()
        .map(|h| {
            // `source_path` is stored as workspace-relative already, but
            // some legacy rows may have been written with absolute
            // paths — normalize defensively so the LLM always sees a
            // relative path it can hand to `workspace_read` / display.
            let rel = match std::path::Path::new(&h.source_path).strip_prefix(ws_root) {
                Ok(p) => p.to_string_lossy().replace('\\', "/"),
                Err(_) => h.source_path.clone(),
            };
            json!({
                "rel_path": rel,
                "image_id": h.image_id,
                "caption": h.caption,
                "ocr_snippet": h.ocr_snippet,
                "score": h.score,
            })
        })
        .collect();

    serde_json::to_string_pretty(&json!({
        "query": query,
        "top_k": top_k,
        "model": ctx.embedder.model(),
        "dim": vec.len(),
        "results": results,
    }))
    .unwrap_or_else(|e| format!("error: serialize: {e}"))
}

fn tool_analyze_visual(ctx: &ToolContext, input: &Value) -> String {
    let Some(rel) = input.get("path").and_then(|v| v.as_str()) else {
        return "error: missing path".to_string();
    };
    let question = input
        .get("question")
        .and_then(|v| v.as_str())
        .unwrap_or("Describe this content in detail. Extract any visible text.");
    let pages = input.get("pages").and_then(|v| v.as_str());
    let abs = match ctx.workspace.resolve(rel.trim()) {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };
    if !abs.is_file() {
        return "error: path is not a file".to_string();
    }
    let ext = abs
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let img_exts = ["png", "jpg", "jpeg", "gif", "webp"];
    let max_bytes = 25_000_000usize;

    if img_exts.contains(&ext.as_str()) {
        let bytes = match ctx.workspace.read_file_bytes(rel.trim(), max_bytes) {
            Ok(b) => b,
            Err(e) => return format!("error: {e}"),
        };
        let mime = vision::mime_for_extension(&ext);
        match vision::analyze_image(ctx.vision, &bytes, mime, question) {
            Ok(text) => serde_json::to_string_pretty(&json!({
                "summary": text,
                "pages": [],
                "vision_provider": ctx.vision.provider.label(),
                "vision_model": ctx.vision.model,
            }))
            .unwrap_or_else(|e| format!("error: {e}")),
            Err(e) => format!("error: {e}"),
        }
    } else if ext == "pdf" {
        if !vision::pdftoppm_available() {
            return "error: pdftoppm not found (brew install poppler)".to_string();
        }
        let max_p = pdf_max_pages();
        let page_nums = parse_page_list(pages, max_p, max_p);
        let tmp = match tempfile::tempdir() {
            Ok(t) => t,
            Err(e) => return format!("error: tempdir: {e}"),
        };
        let paths = match vision::rasterize_pdf_pages(&abs, &page_nums, tmp.path()) {
            Ok(p) => p,
            Err(e) => return format!("error: {e}"),
        };
        let mut per_page = Vec::new();
        for (i, p) in paths.iter().enumerate() {
            let bytes = match fs::read(p) {
                Ok(b) => b,
                Err(e) => return format!("error: read png: {e}"),
            };
            let prompt = format!(
                "PDF page {}. {}",
                page_nums.get(i).copied().unwrap_or((i + 1) as u32),
                question
            );
            match vision::analyze_image(ctx.vision, &bytes, "image/png", &prompt) {
                Ok(t) => per_page.push(json!({"page": page_nums.get(i).copied().unwrap_or(i as u32 + 1), "text": t})),
                Err(e) => return format!("error: page {}: {e}", page_nums.get(i).copied().unwrap_or(i as u32 + 1)),
            }
        }
        let joined: String = per_page
            .iter()
            .filter_map(|v| v.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n\n");
        serde_json::to_string_pretty(&json!({
            "summary": joined.chars().take(8000).collect::<String>(),
            "per_page": per_page,
            "vision_provider": ctx.vision.provider.label(),
            "vision_model": ctx.vision.model,
        }))
        .unwrap_or_else(|e| format!("error: {e}"))
    } else {
        format!("error: unsupported extension `{ext}` (use png/jpg/jpeg/gif/webp/pdf)")
    }
}

fn tool_pdf_to_table(ctx: &ToolContext, input: &Value) -> String {
    let Some(rel) = input.get("pdf_path").and_then(|v| v.as_str()) else {
        return "error: missing pdf_path".to_string();
    };
    let fmt = input
        .get("output_format")
        .and_then(|v| v.as_str())
        .unwrap_or("csv")
        .to_ascii_lowercase();
    let out = input
        .get("output_name")
        .and_then(|v| v.as_str())
        .unwrap_or("extracted");
    let safe = sanitize_export_name(out);
    let pages = input.get("pages").and_then(|v| v.as_str());
    let abs = match ctx.workspace.resolve(rel.trim()) {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };
    if !abs
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .eq_ignore_ascii_case("pdf")
    {
        return "error: not a pdf".to_string();
    }
    if !vision::pdftoppm_available() {
        return "error: pdftoppm not found (brew install poppler)".to_string();
    }
    let max_p = pdf_max_pages();
    let page_nums = parse_page_list(pages, max_p, max_p);
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => return format!("error: tempdir: {e}"),
    };
    let paths = match vision::rasterize_pdf_pages(&abs, &page_nums, tmp.path()) {
        Ok(p) => p,
        Err(e) => return format!("error: {e}"),
    };
    let mut all_columns: Vec<String> = Vec::new();
    let mut all_rows: Vec<Vec<String>> = Vec::new();
    for (i, png_path) in paths.iter().enumerate() {
        let bytes = match fs::read(png_path) {
            Ok(b) => b,
            Err(e) => return format!("error: {e}"),
        };
        let hint = format!(
            "Extract the main data table from this PDF page as JSON. Page index {}.",
            page_nums.get(i).copied().unwrap_or(i as u32 + 1)
        );
        let v = match vision::analyze_table(ctx.vision, &bytes, &hint) {
            Ok(j) => j,
            Err(e) => return format!("error: {e}"),
        };
        let cols: Vec<String> = v
            .get("columns")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect();
        let rows_val = v.get("rows").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        let mut rows: Vec<Vec<String>> = Vec::new();
        for r in rows_val {
            let row: Vec<String> = r
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|c| match c {
                    Value::String(s) => s,
                    Value::Number(n) => n.to_string(),
                    Value::Null => String::new(),
                    o => o.to_string(),
                })
                .collect();
            if !row.is_empty() {
                rows.push(row);
            }
        }
        if all_columns.is_empty() && !cols.is_empty() {
            all_columns = cols.clone();
        }
        if !cols.is_empty() && cols == all_columns {
            for r in rows {
                all_rows.push(r);
            }
        } else if all_columns.is_empty() {
            all_columns = vec!["col0".into()];
            for r in rows {
                all_rows.push(r);
            }
        } else {
            for r in rows {
                if r.len() == all_columns.len() {
                    all_rows.push(r);
                }
            }
        }
    }
    if all_columns.is_empty() {
        return "error: no columns extracted".to_string();
    }
    if fmt == "xlsx" {
        let rel_out = format!("exports/tables/{safe}.xlsx");
        let mut wb = Workbook::new();
        let sheet = wb.add_worksheet();
        for (c, name) in all_columns.iter().enumerate() {
            let _ = sheet.write_string(0, c as u16, name);
        }
        for (r, row) in all_rows.iter().enumerate() {
            for (c, cell) in row.iter().enumerate().take(all_columns.len()) {
                let _ = sheet.write_string((r + 1) as u32, c as u16, cell);
            }
        }
        let buf = match wb.save_to_buffer() {
            Ok(b) => b,
            Err(e) => return format!("error: xlsx: {e}"),
        };
        if let Err(e) = ctx.workspace.write_bytes_file(&rel_out, &buf) {
            return format!("error: {e}");
        }
        return json!({"ok": true, "path": rel_out, "rows": all_rows.len()}).to_string();
    }
    let rel_out = format!("exports/tables/{safe}.csv");
    let mut wtr = csv::Writer::from_writer(Vec::new());
    if wtr.write_record(&all_columns).is_err() {
        return "error: csv header".to_string();
    }
    for row in &all_rows {
        let mut padded = row.clone();
        while padded.len() < all_columns.len() {
            padded.push(String::new());
        }
        padded.truncate(all_columns.len());
        if wtr.write_record(&padded).is_err() {
            return "error: csv row".to_string();
        }
    }
    let inner = match wtr.into_inner() {
        Ok(v) => v,
        Err(e) => return format!("error: csv finalize: {e}"),
    };
    let data = String::from_utf8_lossy(&inner).into_owned();
    if let Err(e) = ctx.workspace.write_bytes_file(&rel_out, data.as_bytes()) {
        return format!("error: {e}");
    }
    json!({"ok": true, "path": rel_out, "rows": all_rows.len(), "columns": all_columns}).to_string()
}

#[cfg(unix)]
fn terminal_tool_enabled() -> bool {
    std::env::var("ESON_TERMINAL_ENABLED")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

#[cfg(unix)]
fn terminal_timeout_sec() -> u64 {
    std::env::var("ESON_TERMINAL_TIMEOUT_SEC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600)
        .clamp(1, 3600)
}

#[cfg(unix)]
fn terminal_max_output_bytes() -> usize {
    std::env::var("ESON_TERMINAL_MAX_OUTPUT_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500_000)
        .clamp(4_096, 2_000_000)
}

#[cfg(unix)]
fn terminal_allow_sudo() -> bool {
    std::env::var("ESON_TERMINAL_ALLOW_SUDO")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false)
}

/// Returns Err(reason) if the command should not run.
#[cfg(unix)]
fn terminal_blocked_reason(cmd: &str) -> Option<&'static str> {
    let t = cmd.trim();
    if t.is_empty() {
        return Some("empty command");
    }
    let lower = t.to_lowercase();
    if !terminal_allow_sudo() && (lower.contains("sudo ") || lower.starts_with("sudo")) {
        return Some("sudo is disabled (set ESON_TERMINAL_ALLOW_SUDO=1 to allow)");
    }
    let bad = [
        ("rm -rf /", "blocked pattern: rm -rf /"),
        ("rm -rf /*", "blocked pattern: rm -rf /*"),
        ("rm -fr /", "blocked pattern"),
        ("mkfs.", "blocked: mkfs"),
        ("dd if=/dev/", "blocked: dd from device"),
        (":(){", "blocked: fork bomb pattern"),
        (">& /dev/sd", "blocked: redirect to raw disk"),
        ("> /dev/sd", "blocked: redirect to raw disk"),
        ("/dev/disk", "blocked: direct disk access"),
    ];
    for (pat, msg) in bad {
        if lower.contains(pat) {
            return Some(msg);
        }
    }
    None
}

#[cfg(unix)]
fn shell_for_terminal() -> &'static str {
    if cfg!(target_os = "macos") {
        "/bin/zsh"
    } else {
        "/bin/sh"
    }
}

#[cfg(unix)]
fn tool_run_terminal(ws: &WorkspaceRoot, input: &Value) -> String {
    if !terminal_tool_enabled() {
        return "error: run_terminal is disabled (unset ESON_TERMINAL_ENABLED or set to 1)".to_string();
    }
    let Some(command) = input.get("command").and_then(|v| v.as_str()) else {
        return "error: missing required field `command`".to_string();
    };
    if let Some(reason) = terminal_blocked_reason(command) {
        return format!("error: {reason}");
    }
    let cwd = ws.root().to_path_buf();
    let cwd_display = cwd.display().to_string();
    let timeout = Duration::from_secs(terminal_timeout_sec());
    let max_out = terminal_max_output_bytes();
    let shell = shell_for_terminal();
    let cmd_owned = command.to_string();

    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let out = Command::new(shell)
            .args(["-lc", &cmd_owned])
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        let _ = tx.send(out);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(out)) => {
            let code = out.status.code().unwrap_or(-1);
            let cap = (max_out / 2).max(2048);
            let mut so = String::from_utf8_lossy(&out.stdout).into_owned();
            let mut se = String::from_utf8_lossy(&out.stderr).into_owned();
            if so.len() > cap {
                so.truncate(cap);
                so.push_str("… (stdout truncated)");
            }
            if se.len() > cap {
                se.truncate(cap);
                se.push_str("… (stderr truncated)");
            }
            match serde_json::to_string_pretty(&json!({
                "exit_code": code,
                "cwd": cwd_display,
                "stdout": so,
                "stderr": se,
            })) {
                Ok(s) => s,
                Err(e) => format!("error: serialize: {e}"),
            }
        }
        Ok(Err(e)) => format!("error: failed to run command: {e}"),
        Err(mpsc::RecvTimeoutError::Timeout) => format!(
            "error: command timed out after {}s (subprocess may still be running)",
            terminal_timeout_sec()
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            "error: internal error (terminal runner disconnected)".to_string()
        }
    }
}

#[cfg(test)]
mod grep_flex_tests {
    use super::*;

    #[test]
    fn natural_language_matches_slug_in_path() {
        assert!(flexible_hay_match(
            "exports/atd_negative.csv",
            "look for the atd negative file",
            false,
            true,
        ));
        assert!(!flexible_hay_match(
            "exports/other.csv",
            "look for the atd negative file",
            false,
            true,
        ));
    }

    #[test]
    fn strict_substring_requires_exact_when_flexible_off() {
        assert!(!flexible_hay_match(
            "atd_negative.txt",
            "atd negative",
            false,
            false,
        ));
        assert!(flexible_hay_match(
            "atd_negative.txt",
            "atd_negative",
            false,
            false,
        ));
    }

    #[test]
    fn folded_full_pattern_matches_slug() {
        assert!(flexible_hay_match(
            "prefix_atd_negative_suffix",
            "atd negative",
            false,
            true,
        ));
    }
}
