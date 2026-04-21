//! Policy: paths must remain inside workspace (eval / safety harness).

use eson_agent::workspace::WorkspaceRoot;
use std::fs;
use std::path::Path;
use uuid::Uuid;

#[test]
fn rejects_escape_via_dotdot() {
    let dir = std::env::temp_dir().join(format!("eson-ws-{}", Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let ws = WorkspaceRoot::new(&dir).unwrap();
    assert!(ws.resolve("../outside").is_err());
}

#[test]
fn allows_file_under_workspace() {
    let dir = std::env::temp_dir().join(format!("eson-ws-{}", Uuid::new_v4()));
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("sub/hello.txt"), b"hi").unwrap();
    let ws = WorkspaceRoot::new(&dir).unwrap();
    let p = ws.resolve("sub/hello.txt").expect("inside workspace");
    assert!(p.is_file());
}

#[test]
fn path_escape_probe_double_dot() {
    let dir = std::env::temp_dir().join(format!("eson-ws-{}", Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let ws = WorkspaceRoot::new(&dir).unwrap();
    let malicious = Path::new("sub").join("..").join("..").join("etc").join("passwd");
    assert!(ws.resolve(malicious.to_str().unwrap()).is_err());
}
