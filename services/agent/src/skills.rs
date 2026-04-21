//!  `SKILL.md` discovery under `skills/` (cron, inbox, user, auto).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const CATEGORIES: &[&str] = &["cron", "inbox", "user", "auto"];

/// Resolve the skills root: `ESON_SKILLS_DIR`, else `./skills` from cwd, else next to repo `skills/`.
pub fn resolve_skills_dir() -> PathBuf {
    if let Ok(p) = std::env::var("ESON_SKILLS_DIR") {
        return PathBuf::from(p);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let from_cwd = cwd.join("skills");
    if from_cwd.join("README.md").is_file() || from_cwd.join("cron").is_dir() {
        return from_cwd;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../skills")
}

#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub id: String,
    pub category: String,
    pub rel_path: String,
    pub name: String,
    pub description: String,
    pub enabled: bool,
    /// e.g. `every:15m`, `every:6h`, or `14:30` (local time once per day)
    pub cron: Option<String>,
    /// comma-separated lowercase extensions without dot, e.g. `png,jpg`
    pub inbox_ext: Option<String>,
}

fn parse_frontmatter(raw: &str) -> (HashMap<String, String>, String) {
    let mut map = HashMap::new();
    if !raw.starts_with("---\n") {
        return (map, raw.to_string());
    }
    let rest = raw.trim_start_matches("---\n");
    if let Some(idx) = rest.find("\n---\n") {
        let header = &rest[..idx];
        let body = rest[idx + 5..].to_string();
        for line in header.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                let key = k.trim().to_lowercase();
                let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
                map.insert(key, val);
            }
        }
        return (map, body);
    }
    (map, raw.to_string())
}

fn load_skill_at(path: &Path, category: &str, rel_path: &str) -> Option<SkillEntry> {
    let raw = fs::read_to_string(path).ok()?;
    let (fm, _body) = parse_frontmatter(&raw);
    let enabled = fm
        .get("enabled")
        .map(|s| s != "false" && s != "0")
        .unwrap_or(true);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "skill".into());
    let name = fm.get("name").cloned().unwrap_or(stem.clone());
    let description = fm.get("description").cloned().unwrap_or_default();
    let cron = fm.get("cron").cloned().filter(|s| !s.is_empty());
    let inbox_ext = fm.get("inbox_ext").cloned().and_then(|s| {
        let joined = s
            .split(',')
            .map(|t| t.trim().trim_start_matches('.').to_lowercase())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(",");
        if joined.is_empty() {
            None
        } else {
            Some(joined)
        }
    });
    let id = format!("{category}/{stem}");
    Some(SkillEntry {
        id,
        category: category.to_string(),
        rel_path: rel_path.to_string(),
        name,
        description,
        enabled,
        cron,
        inbox_ext,
    })
}

/// List all skill metadata (enabled and disabled).
pub fn list_all_skills(skills_root: &Path) -> Vec<SkillEntry> {
    let mut out = Vec::new();
    for cat in CATEGORIES {
        let dir = skills_root.join(cat);
        if !dir.is_dir() {
            continue;
        }
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if p.extension().and_then(|s| s.to_str()).map(|e| e.eq_ignore_ascii_case("md"))
                != Some(true)
            {
                continue;
            }
            let rel = format!("{cat}/{}", p.file_name().and_then(|n| n.to_str()).unwrap_or("?"));
            if let Some(entry) = load_skill_at(&p, cat, &rel) {
                out.push(entry);
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Load full markdown body (after frontmatter) for `id` like `cron/consolidate-memory`.
pub fn load_skill_body(skills_root: &Path, id: &str) -> Result<String, String> {
    let (cat, rest) = id
        .split_once('/')
        .ok_or_else(|| "skill id must be category/name, e.g. cron/consolidate-memory".to_string())?;
    if !CATEGORIES.contains(&cat) {
        return Err(format!("unknown skill category `{cat}`"));
    }
    let file = skills_root.join(cat).join(format!("{rest}.md"));
    if !file.is_file() {
        return Err(format!("skill file not found: {}", file.display()));
    }
    let raw = fs::read_to_string(&file).map_err(|e| e.to_string())?;
    let (_fm, body) = parse_frontmatter(&raw);
    Ok(body)
}

/// Match inbox file extension (no dot) to first enabled inbox skill.
pub fn match_inbox_skill(skills_root: &Path, ext: &str) -> Option<SkillEntry> {
    let ext = ext.trim_start_matches('.').to_lowercase();
    list_all_skills(skills_root)
        .into_iter()
        .filter(|s| s.enabled && s.category == "inbox")
        .find(|s| {
            s.inbox_ext.as_ref().is_some_and(|e| {
                e.split(',')
                    .map(|x| x.trim())
                    .any(|x| !x.is_empty() && x == ext.as_str())
            })
        })
}

/// Parse `pages` like `1-3`, `1,4`, or empty = all (caller caps max pages).
pub fn parse_page_list(pages: Option<&str>, max_page: u32, default_max: u32) -> Vec<u32> {
    let cap = max_page.min(default_max);
    let Some(p) = pages.filter(|s| !s.trim().is_empty()) else {
        return (1..=cap).collect();
    };
    let mut out: Vec<u32> = Vec::new();
    for part in p.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            let start: u32 = a.trim().parse().unwrap_or(1);
            let end: u32 = b.trim().parse().unwrap_or(start);
            for n in start.min(end)..=start.max(end).min(cap) {
                if n >= 1 && !out.contains(&n) {
                    out.push(n);
                }
            }
        } else if let Ok(n) = part.parse::<u32>() {
            if n >= 1 && n <= cap && !out.contains(&n) {
                out.push(n);
            }
        }
    }
    if out.is_empty() {
        (1..=cap).collect()
    } else {
        out.sort_unstable();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pages_range() {
        let v = parse_page_list(Some("1-3"), 10, 10);
        assert_eq!(v, vec![1, 2, 3]);
    }

    /// Pins the shipped 12h consolidation cron skill so the scheduler
    /// can always find and enable it. Guards against accidental
    /// frontmatter drift (typos in `cron:`, `enabled:`, or filename).
    #[test]
    fn consolidation_cron_skill_is_discoverable() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../skills");
        let all = list_all_skills(&root);
        let entry = all
            .iter()
            .find(|s| s.id == "cron/memory-12h-consolidation")
            .expect("consolidation skill must ship and be discoverable");
        assert!(entry.enabled, "consolidation skill must be enabled by default");
        assert_eq!(entry.cron.as_deref(), Some("every:12h"));
        assert_eq!(entry.category, "cron");
    }
}
