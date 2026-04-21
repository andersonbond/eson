//! Load IDENTITY.md, SOUL.md, and Eson.md into the system prompt.

use std::path::{Path, PathBuf};

const FILES: [(&str, &str); 3] = [
    ("IDENTITY", "IDENTITY.md"),
    ("SOUL", "SOUL.md"),
    ("ESON_CAPABILITIES", "Eson.md"),
];

/// Resolve directory containing `IDENTITY.md`, `SOUL.md`, `Eson.md`.
pub fn resolve_persona_dir() -> PathBuf {
    if let Ok(p) = std::env::var("ESON_PERSONA_DIR") {
        return PathBuf::from(p);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let from_cwd = cwd.join("persona");
    if from_cwd.join("SOUL.md").is_file() {
        return from_cwd;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../persona")
}

/// Load and concatenate persona files; substitute `{{WORKSPACE}}` with the real path.
pub fn load_persona_bundle(persona_dir: &Path, workspace_root: &Path) -> String {
    let ws = workspace_root.display().to_string();
    let mut blocks = Vec::new();
    for (heading, filename) in FILES {
        let path = persona_dir.join(filename);
        match std::fs::read_to_string(&path) {
            Ok(raw) => {
                let body = raw.replace("{{WORKSPACE}}", &ws);
                blocks.push(format!("## {heading}\n\n{body}"));
            }
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "persona file missing or unreadable"
            ),
        }
    }
    blocks.join("\n\n---\n\n")
}
