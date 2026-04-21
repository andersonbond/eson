//! Strict workspace boundary: all resolved paths must stay under `ESON_WORKSPACE_ROOT`.

use std::fs;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// One entry from [`WorkspaceRoot::list_directory`].
#[derive(Debug, Clone, serde::Serialize)]
pub struct DirEntryInfo {
    pub name: String,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct WorkspaceRoot {
    canonical: PathBuf,
}

#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error("path escapes workspace boundary: {0}")]
    OutsideWorkspace(String),
    #[error("invalid path: {0}")]
    InvalidPath(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

impl WorkspaceRoot {
    pub fn from_env() -> Result<Self, WorkspaceError> {
        let raw = std::env::var("ESON_WORKSPACE_ROOT").unwrap_or_else(|_| "./workspace".to_string());
        Self::new(Path::new(&raw))
    }

    pub fn new(root: &Path) -> Result<Self, WorkspaceError> {
        std::fs::create_dir_all(root)?;
        let canonical = root.canonicalize()?;
        Ok(Self { canonical })
    }

    pub fn root(&self) -> &Path {
        &self.canonical
    }

    /// Resolve a user-supplied path (relative to workspace or absolute) and ensure it stays inside workspace.
    pub fn resolve(&self, user_path: &str) -> Result<PathBuf, WorkspaceError> {
        let only = std::env::var("ESON_WORKSPACE_ONLY_PATHS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(true);
        let allow_abs = std::env::var("ESON_ALLOW_ABSOLUTE_PATHS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        let candidate = if Path::new(user_path).is_absolute() {
            if !allow_abs && only {
                return Err(WorkspaceError::OutsideWorkspace(
                    "absolute paths disabled".into(),
                ));
            }
            PathBuf::from(user_path)
        } else {
            self.canonical.join(user_path)
        };

        let resolved = match candidate.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                let parent = candidate
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(self.canonical.as_path());
                std::fs::create_dir_all(parent)?;
                let fname = candidate
                    .file_name()
                    .ok_or_else(|| WorkspaceError::InvalidPath(user_path.to_string()))?;
                let mut p = parent.canonicalize()?;
                p.push(fname);
                if !p.starts_with(&self.canonical) {
                    return Err(WorkspaceError::OutsideWorkspace(p.display().to_string()));
                }
                p
            }
        };

        if !resolved.starts_with(&self.canonical) {
            return Err(WorkspaceError::OutsideWorkspace(
                resolved.display().to_string(),
            ));
        }
        Ok(resolved)
    }

    pub fn is_workspace_only() -> bool {
        std::env::var("ESON_WORKSPACE_ONLY_PATHS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(true)
    }

    /// Resolve `rel` as an existing directory under the workspace root (read-only navigation).
    /// Empty `rel` is the workspace root. `..` cannot escape above the workspace root.
    /// Returns `(normalized_rel, absolute_path)`.
    pub fn resolve_directory(&self, rel: &str) -> Result<(String, PathBuf), WorkspaceError> {
        if rel.contains(':') {
            return Err(WorkspaceError::InvalidPath(
                "path must be relative".into(),
            ));
        }
        let norm = rel.replace('\\', "/");
        let mut parts: Vec<String> = Vec::new();
        for seg in norm.split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            if seg == ".." {
                parts.pop();
                continue;
            }
            parts.push(seg.to_string());
        }

        let rel_display = parts.join("/");

        let mut target = self.canonical.clone();
        for p in &parts {
            target.push(p);
        }

        let resolved = target.canonicalize().map_err(WorkspaceError::Io)?;
        if !resolved.starts_with(&self.canonical) {
            return Err(WorkspaceError::OutsideWorkspace(
                resolved.display().to_string(),
            ));
        }
        if !resolved.is_dir() {
            return Err(WorkspaceError::InvalidPath(format!(
                "not a directory: {}",
                resolved.display()
            )));
        }

        Ok((rel_display, resolved))
    }

    /// Resolve `rel` as a path under the workspace root (read-only navigation). Empty `rel` lists the root.
    /// `..` cannot escape above the workspace root. Returns `(normalized_rel, entries)`.
    pub fn list_directory(&self, rel: &str) -> Result<(String, Vec<DirEntryInfo>), WorkspaceError> {
        let (rel_display, resolved) = self.resolve_directory(rel)?;

        let mut entries: Vec<DirEntryInfo> = fs::read_dir(&resolved)
            .map_err(WorkspaceError::Io)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let meta = e.metadata().ok()?;
                Some(DirEntryInfo {
                    name,
                    is_dir: meta.is_dir(),
                })
            })
            .collect();

        entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        });

        Ok((rel_display, entries))
    }

    /// Read a file under the workspace as text. `rel` is workspace-relative (forward slashes;
    /// `..` cannot escape the root). Rejects absolute paths and paths larger than `max_bytes`.
    pub fn read_text_file(&self, rel: &str, max_bytes: usize) -> Result<String, WorkspaceError> {
        if rel.contains(':') {
            return Err(WorkspaceError::InvalidPath(
                "path must be workspace-relative (no schemes or drive letters)".into(),
            ));
        }
        let norm = rel.replace('\\', "/");
        if norm.starts_with('/') {
            return Err(WorkspaceError::InvalidPath(
                "use a workspace-relative path, not an absolute path".into(),
            ));
        }
        let resolved = self.resolve(&norm)?;
        if !resolved.is_file() {
            return Err(WorkspaceError::InvalidPath(format!(
                "not a file: {}",
                resolved.display()
            )));
        }
        let len = fs::metadata(&resolved)?.len() as usize;
        if len > max_bytes {
            return Err(WorkspaceError::InvalidPath(format!(
                "file is {len} bytes (max {max_bytes})"
            )));
        }
        let bytes = fs::read(&resolved)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Read a file as raw bytes (workspace-relative). Same path rules as [`Self::read_text_file`].
    pub fn read_file_bytes(&self, rel: &str, max_bytes: usize) -> Result<Vec<u8>, WorkspaceError> {
        if rel.contains(':') {
            return Err(WorkspaceError::InvalidPath(
                "path must be workspace-relative (no schemes or drive letters)".into(),
            ));
        }
        let norm = rel.replace('\\', "/");
        if norm.starts_with('/') {
            return Err(WorkspaceError::InvalidPath(
                "use a workspace-relative path, not an absolute path".into(),
            ));
        }
        let resolved = self.resolve(&norm)?;
        if !resolved.is_file() {
            return Err(WorkspaceError::InvalidPath(format!(
                "not a file: {}",
                resolved.display()
            )));
        }
        let len = fs::metadata(&resolved)?.len() as usize;
        if len > max_bytes {
            return Err(WorkspaceError::InvalidPath(format!(
                "file is {len} bytes (max {max_bytes})"
            )));
        }
        Ok(fs::read(&resolved)?)
    }

    /// Write bytes to a path under the workspace (creates parent dirs). `rel` uses forward slashes; `..` cannot escape.
    pub fn write_bytes_file(&self, rel: &str, bytes: &[u8]) -> Result<(), WorkspaceError> {
        if rel.contains(':') {
            return Err(WorkspaceError::InvalidPath(
                "path must be workspace-relative".into(),
            ));
        }
        let norm = rel.replace('\\', "/");
        if norm.starts_with('/') {
            return Err(WorkspaceError::InvalidPath(
                "use a workspace-relative path".into(),
            ));
        }
        let resolved = self.resolve(&norm)?;
        if let Some(parent) = resolved.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&resolved, bytes)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn rejects_parent_escape() {
        let dir = std::env::temp_dir().join(format!("eson-ws-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let ws = WorkspaceRoot::new(&dir).unwrap();
        let r = ws.resolve("../outside");
        assert!(r.is_err());
    }

    #[test]
    fn list_directory_root_and_nested() {
        let dir = std::env::temp_dir().join(format!("eson-ws-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("a/b")).unwrap();
        fs::write(dir.join("a/file.txt"), b"x").unwrap();
        let ws = WorkspaceRoot::new(&dir).unwrap();
        let (rel, e) = ws.list_directory("").expect("root");
        assert!(rel.is_empty());
        assert!(e.iter().any(|x| x.name == "a" && x.is_dir));
        let (rel2, e2) = ws.list_directory("a").expect("a");
        assert_eq!(rel2, "a");
        assert!(e2.iter().any(|x| x.name == "b" && x.is_dir));
        assert!(e2.iter().any(|x| x.name == "file.txt" && !x.is_dir));
    }

    #[test]
    fn list_directory_dotdot_stays_in_root() {
        let dir = std::env::temp_dir().join(format!("eson-ws-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let ws = WorkspaceRoot::new(&dir).unwrap();
        let (rel, _) = ws.list_directory("..").expect("root equiv");
        assert!(rel.is_empty());
    }

    #[test]
    fn read_text_file_under_workspace() {
        let dir = std::env::temp_dir().join(format!("eson-ws-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/hello.txt"), b"hello utf8").unwrap();
        let ws = WorkspaceRoot::new(&dir).unwrap();
        let s = ws.read_text_file("sub/hello.txt", 1000).expect("read");
        assert_eq!(s, "hello utf8");
    }
}
