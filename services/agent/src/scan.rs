//! Workspace-scoped image discovery (no OCR in v0.1 — indexing + hash stub).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const IMAGE_EXT: &[&str] = &["jpg", "jpeg", "png", "gif", "heic"];

pub fn collect_image_files(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if root.is_file() {
        if is_image(root) {
            out.push(root.to_path_buf());
        }
        return Ok(());
    }
    if !root.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            collect_image_files(&p, out)?;
        } else if is_image(&p) {
            out.push(p);
        }
    }
    Ok(())
}

fn is_image(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            IMAGE_EXT.contains(&e.as_str())
        })
        .unwrap_or(false)
}

pub fn stub_hash(path: &Path, meta: &std::fs::Metadata) -> String {
    let mut h = DefaultHasher::new();
    path.to_string_lossy().hash(&mut h);
    meta.len().hash(&mut h);
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    modified.hash(&mut h);
    format!("{:016x}", h.finish())
}
