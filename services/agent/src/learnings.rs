//! Append-only `.learnings/` markdown logs (LRN / ERR / FEAT ids).

use std::fs;
use std::path::Path;

fn learnings_dir(workspace_root: &Path) -> std::path::PathBuf {
    workspace_root.join(".learnings")
}

fn ensure_dir(workspace_root: &Path) -> Result<std::path::PathBuf, String> {
    let d = learnings_dir(workspace_root);
    fs::create_dir_all(&d).map_err(|e| e.to_string())?;
    Ok(d)
}

pub fn append_learning(
    workspace_root: &Path,
    kind: &str,
    summary: &str,
    body: Option<&str>,
    tags: Option<&str>,
) -> Result<String, String> {
    let d = ensure_dir(workspace_root)?;
    let file = match kind {
        "err" | "error" => d.join("ERRORS.md"),
        "feat" | "feature" => d.join("FEATURE_REQUESTS.md"),
        _ => d.join("LEARNINGS.md"),
    };
    let prefix = match kind {
        "err" | "error" => "ERR",
        "feat" | "feature" => "FEAT",
        _ => "LRN",
    };
    let id = format!(
        "{}-{}",
        prefix,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let tag_line = tags.unwrap_or("");
    let body = body.unwrap_or("");
    let block = format!(
        "\n### {id}\n- **summary:** {}\n- **tags:** {}\n\n{}\n",
        summary.trim(),
        tag_line,
        body.trim()
    );
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .map_err(|e| e.to_string())?;
    f.write_all(block.as_bytes()).map_err(|e| e.to_string())?;
    Ok(id)
}

pub fn write_proposed_skill(
    skills_root: &Path,
    slug: &str,
    name: &str,
    when_to_use: &str,
    instructions: &str,
) -> Result<String, String> {
    let auto = skills_root.join("auto");
    fs::create_dir_all(&auto).map_err(|e| e.to_string())?;
    let path = auto.join(format!("{slug}.md"));
    let body = format!(
        "---\nname: {name}\ndescription: {when}\nenabled: false\ntriggers: {{}}\n---\n\n{instructions}\n",
        name = name.replace(':', " "),
        when = when_to_use.replace(':', " "),
        instructions = instructions
    );
    fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

pub fn slugify(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .chars()
        .take(64)
        .collect()
}

/// One parsed entry from `.learnings/{LEARNINGS,ERRORS,FEATURE_REQUESTS}.md`.
/// Mirrors the markdown shape produced by [`append_learning`] so the
/// orchestrator can re-inject prior insights into the system prompt
/// every turn (closing the "Eson writes learnings but never reads them
/// back" loop the user pointed out).
#[derive(Debug, Clone)]
pub struct LearningEntry {
    pub id: String,
    pub summary: String,
    pub tags: String,
    pub body: String,
}

/// Parse a single `.learnings/*.md` file into [`LearningEntry`]s, most
/// recent first. Files are append-only with `### {PREFIX}-{millis}`
/// blocks, so the *file order* is the *time order* — we just walk
/// bottom-to-top.
fn parse_learnings_file(path: &Path) -> Vec<LearningEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    // Splitting on the heading marker lets us tolerate stray blank
    // lines / human edits between blocks. The first chunk before the
    // first `### ` is preamble (usually empty) — skip it.
    let mut entries: Vec<LearningEntry> = Vec::new();
    for chunk in text.split("\n### ").skip(1) {
        let mut lines = chunk.lines();
        let id_line = lines.next().unwrap_or("").trim();
        if id_line.is_empty() {
            continue;
        }
        let id = id_line.to_string();
        let mut summary = String::new();
        let mut tags = String::new();
        let mut body = String::new();
        let mut in_body = false;
        for raw in lines {
            let line = raw.trim_end();
            if !in_body {
                if let Some(rest) = line.strip_prefix("- **summary:**") {
                    summary = rest.trim().to_string();
                    continue;
                }
                if let Some(rest) = line.strip_prefix("- **tags:**") {
                    tags = rest.trim().to_string();
                    continue;
                }
                if line.is_empty() {
                    in_body = true;
                    continue;
                }
            } else {
                if !body.is_empty() {
                    body.push('\n');
                }
                body.push_str(line);
            }
        }
        entries.push(LearningEntry {
            id,
            summary,
            tags,
            body: body.trim().to_string(),
        });
    }
    entries.reverse();
    entries
}

/// Build a JSON-serializable snapshot of the most recent learnings,
/// errors, and feature requests for injection into the system prompt.
/// Returns a pretty-printed JSON string ready for a fenced
/// ```` ```json ```` block. Empty when `.learnings/` doesn't exist
/// yet — the caller decides whether to render a "no learnings yet"
/// note or just skip the section.
///
/// Caps are conservative because every turn pays the prompt-token
/// cost: only the most recent entries per kind are included, and each
/// `body` is truncated to `body_max` chars with an ellipsis. The
/// agent can always `workspace_read .learnings/LEARNINGS.md` to see
/// the full history if the snippet isn't enough.
pub fn recent_learnings_snippet(
    workspace_root: &Path,
    max_per_kind: usize,
    body_max: usize,
) -> String {
    let dir = learnings_dir(workspace_root);
    if !dir.exists() {
        return String::new();
    }
    let learnings = parse_learnings_file(&dir.join("LEARNINGS.md"));
    let errors = parse_learnings_file(&dir.join("ERRORS.md"));
    let features = parse_learnings_file(&dir.join("FEATURE_REQUESTS.md"));
    if learnings.is_empty() && errors.is_empty() && features.is_empty() {
        return String::new();
    }
    let entry_to_json = |e: &LearningEntry| {
        let mut body = e.body.clone();
        if body.chars().count() > body_max {
            body = body.chars().take(body_max).collect::<String>();
            body.push('…');
        }
        serde_json::json!({
            "id": e.id,
            "summary": e.summary,
            "tags": e.tags,
            "body": body,
        })
    };
    let payload = serde_json::json!({
        "learnings": learnings.iter().take(max_per_kind).map(entry_to_json).collect::<Vec<_>>(),
        "errors": errors.iter().take(max_per_kind).map(entry_to_json).collect::<Vec<_>>(),
        "features": features.iter().take(max_per_kind).map(entry_to_json).collect::<Vec<_>>(),
        "totals": {
            "learnings": learnings.len(),
            "errors": errors.len(),
            "features": features.len(),
        },
        "files": {
            "learnings": ".learnings/LEARNINGS.md",
            "errors": ".learnings/ERRORS.md",
            "features": ".learnings/FEATURE_REQUESTS.md",
        },
    });
    serde_json::to_string_pretty(&payload).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_append_then_parse() {
        let tmp = TempDir::new().unwrap();
        let id1 = append_learning(
            tmp.path(),
            "lrn",
            "First lesson",
            Some("body of first lesson"),
            Some("tag1, tag2"),
        )
        .unwrap();
        let id2 = append_learning(
            tmp.path(),
            "lrn",
            "Second lesson",
            Some("body of second lesson"),
            Some("tag3"),
        )
        .unwrap();
        let entries = parse_learnings_file(&tmp.path().join(".learnings/LEARNINGS.md"));
        assert_eq!(entries.len(), 2);
        // Most recent first
        assert_eq!(entries[0].id, id2);
        assert_eq!(entries[0].summary, "Second lesson");
        assert_eq!(entries[0].body, "body of second lesson");
        assert_eq!(entries[1].id, id1);
        assert_eq!(entries[1].tags, "tag1, tag2");
    }

    #[test]
    fn snippet_is_empty_when_no_dir() {
        let tmp = TempDir::new().unwrap();
        assert!(recent_learnings_snippet(tmp.path(), 10, 500).is_empty());
    }

    #[test]
    fn snippet_truncates_long_body() {
        let tmp = TempDir::new().unwrap();
        let long_body = "x".repeat(2000);
        append_learning(tmp.path(), "lrn", "long", Some(&long_body), Some("")).unwrap();
        let s = recent_learnings_snippet(tmp.path(), 5, 100);
        assert!(s.contains('…'));
        assert!(!s.contains(&"x".repeat(200)));
    }
}
