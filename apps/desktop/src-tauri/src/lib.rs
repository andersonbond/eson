//! Eson desktop shell.
//!
//! Responsibilities (in order of execution):
//!   1. Hardware preflight (`system_info` Tauri command) — UI shows a
//!      blocker if the host is below 32 GB RAM / 10 logical CPUs.
//!   2. Provision the user-data tree (`workspace/`, `persona/`, `skills/`)
//!      under the platform app-data dir on first launch by copying the
//!      bundled resources.
//!   3. Spawn the `eson-memory` and `eson-agent` sidecars with
//!      `ESON_WORKSPACE_ROOT`, `ESON_PERSONA_DIR`, `ESON_SKILLS_DIR`
//!      pointing at that tree.
//!   4. Tear them down on `RunEvent::ExitRequested`.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use sysinfo::System;
use tauri::async_runtime::JoinHandle;
use tauri::{Emitter, Manager, RunEvent, State};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

// Minimum requirements vary by host OS:
//   * macOS  → 16 GiB RAM, Apple Silicon (M1 or newer). M1 ships 8 logical
//              cores (4P+4E), so we floor cores there too as a sanity check.
//   * other  → 32 GiB RAM, ≥10 logical cores (Windows / Linux baseline).
// The "Apple Silicon" check is `std::env::consts::ARCH == "aarch64"` because
// every arm64 Mac is M1 or newer; Intel Macs report `x86_64` and fail.
#[cfg(target_os = "macos")]
mod platform_reqs {
    pub const MIN_MEMORY_GIB: f64 = 14.5; // 16 GiB nominal reports ~15.x after kernel reservation
    pub const MIN_CPU_LOGICAL: u32 = 8;
    pub const REQUIRE_APPLE_SILICON: bool = true;
    pub const LABEL: &str = "macOS";
}
#[cfg(not(target_os = "macos"))]
mod platform_reqs {
    pub const MIN_MEMORY_GIB: f64 = 30.0; // 32 GiB nominal reports ~31.x after kernel reservation
    pub const MIN_CPU_LOGICAL: u32 = 10;
    pub const REQUIRE_APPLE_SILICON: bool = false;
    #[cfg(target_os = "windows")]
    pub const LABEL: &str = "Windows";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    pub const LABEL: &str = "Linux";
}

#[derive(Serialize)]
struct Requirements {
    min_cpu_logical: u32,
    min_memory_gib: f64,
    require_apple_silicon: bool,
    platform_label: &'static str,
}

#[derive(Serialize)]
struct SystemInfo {
    cpu_logical: u32,
    cpu_physical: Option<u32>,
    memory_total_bytes: u64,
    memory_total_gib: f64,
    os_name: Option<String>,
    os_version: Option<String>,
    host_name: Option<String>,
    arch: String,
    is_apple_silicon: bool,
    meets_requirements: bool,
    failed_checks: Vec<&'static str>,
    requirements: Requirements,
}

#[tauri::command]
fn system_info() -> SystemInfo {
    let mut sys = System::new();
    sys.refresh_memory();
    sys.refresh_cpu_list(sysinfo::CpuRefreshKind::new());
    let cpu_logical = sys.cpus().len() as u32;
    let cpu_physical = sys.physical_core_count().map(|n| n as u32);
    let memory_total_bytes = sys.total_memory();
    let memory_total_gib = memory_total_bytes as f64 / (1024.0_f64.powi(3));
    let arch = std::env::consts::ARCH.to_string();
    let is_apple_silicon = cfg!(target_os = "macos") && arch == "aarch64";

    let mut failed_checks: Vec<&'static str> = Vec::new();
    if memory_total_gib < platform_reqs::MIN_MEMORY_GIB {
        failed_checks.push("memory");
    }
    if cpu_logical < platform_reqs::MIN_CPU_LOGICAL {
        failed_checks.push("cpu");
    }
    if platform_reqs::REQUIRE_APPLE_SILICON && !is_apple_silicon {
        failed_checks.push("chip");
    }

    SystemInfo {
        cpu_logical,
        cpu_physical,
        memory_total_bytes,
        memory_total_gib,
        os_name: System::name(),
        os_version: System::os_version(),
        host_name: System::host_name(),
        arch,
        is_apple_silicon,
        meets_requirements: failed_checks.is_empty(),
        failed_checks,
        requirements: Requirements {
            min_cpu_logical: platform_reqs::MIN_CPU_LOGICAL,
            min_memory_gib: platform_reqs::MIN_MEMORY_GIB,
            require_apple_silicon: platform_reqs::REQUIRE_APPLE_SILICON,
            platform_label: platform_reqs::LABEL,
        },
    }
}

#[derive(Serialize, Clone)]
struct ServicesStatus {
    workspace_root: String,
    persona_dir: String,
    skills_dir: String,
    memory_running: bool,
    agent_running: bool,
    agent_url: String,
    last_event: Option<String>,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum OllamaInstallPhase {
    Idle,
    Checking,
    InstallingOllama,
    StartingOllama,
    PullingModel,
    Ready,
    Failed,
}

#[derive(Serialize, Clone, Debug)]
struct OllamaStatus {
    installed: bool,
    running: bool,
    model_ready: bool,
    phase: OllamaInstallPhase,
    in_progress: bool,
    last_error: Option<String>,
    progress_log_tail: Vec<String>,
}

#[derive(Clone, Debug)]
struct OllamaInstallState {
    installed: bool,
    running: bool,
    model_ready: bool,
    phase: OllamaInstallPhase,
    in_progress: bool,
    last_error: Option<String>,
    progress_log_tail: Vec<String>,
}

impl Default for OllamaInstallState {
    fn default() -> Self {
        Self {
            installed: false,
            running: false,
            model_ready: false,
            phase: OllamaInstallPhase::Idle,
            in_progress: false,
            last_error: None,
            progress_log_tail: Vec::new(),
        }
    }
}

impl OllamaInstallState {
    fn snapshot(&self) -> OllamaStatus {
        OllamaStatus {
            installed: self.installed,
            running: self.running,
            model_ready: self.model_ready,
            phase: self.phase,
            in_progress: self.in_progress,
            last_error: self.last_error.clone(),
            progress_log_tail: self.progress_log_tail.clone(),
        }
    }
}

/// Owns the spawned sidecar children + their stdout/stderr pump tasks so we
/// can kill them cleanly on app exit.
#[derive(Default)]
struct ServicesState {
    memory: Option<CommandChild>,
    agent: Option<CommandChild>,
    pumps: Vec<JoinHandle<()>>,
    workspace_root: PathBuf,
    persona_dir: PathBuf,
    skills_dir: PathBuf,
    last_event: Option<String>,
    ollama: OllamaInstallState,
}

impl ServicesState {
    fn snapshot(&self) -> ServicesStatus {
        ServicesStatus {
            workspace_root: self.workspace_root.display().to_string(),
            persona_dir: self.persona_dir.display().to_string(),
            skills_dir: self.skills_dir.display().to_string(),
            memory_running: self.memory.is_some(),
            agent_running: self.agent.is_some(),
            agent_url: agent_url(),
            last_event: self.last_event.clone(),
        }
    }
}

const OLLAMA_TARGET_MODEL: &str = "gemma4:e4b";

/// Desktop app bundles often inherit a **minimal** `PATH` (especially on macOS),
/// so `ollama` installed via Homebrew (`/opt/homebrew/bin`) is not found unless
/// we prepend the usual locations. Optional `OLLAMA_PATH`: absolute path to the
/// `ollama` binary **or** to the directory containing it.
fn augmented_path_for_ollama_cli() -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    let mut prefixes: Vec<String> = Vec::new();
    if let Ok(raw) = std::env::var("OLLAMA_PATH") {
        let p = PathBuf::from(raw.trim());
        if p.is_file() {
            if let Some(parent) = p.parent() {
                prefixes.push(parent.to_string_lossy().into_owned());
            }
        } else if p.is_dir() {
            prefixes.push(p.to_string_lossy().into_owned());
        }
    }
    #[cfg(target_os = "macos")]
    {
        prefixes.push("/opt/homebrew/bin".into());
        prefixes.push("/usr/local/bin".into());
    }
    #[cfg(target_os = "linux")]
    {
        prefixes.push("/usr/local/bin".into());
        prefixes.push("/usr/bin".into());
        if let Ok(home) = std::env::var("HOME") {
            prefixes.push(format!("{home}/.local/bin"));
            prefixes.push(format!("{home}/.linuxbrew/bin"));
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(la) = std::env::var("LOCALAPPDATA") {
            prefixes.push(format!("{la}\\Programs\\Ollama"));
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            prefixes.push(format!("{pf}\\Ollama"));
        }
    }
    if prefixes.is_empty() {
        return base;
    }
    #[cfg(target_os = "windows")]
    {
        let sep = ';';
        format!("{}{}{}", prefixes.join(&sep.to_string()), sep, base)
    }
    #[cfg(not(target_os = "windows"))]
    {
        format!("{}:{}", prefixes.join(":"), base)
    }
}

fn ollama_cmd_exists() -> bool {
    std::process::Command::new("ollama")
        .env("PATH", augmented_path_for_ollama_cli())
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn ollama_running_probe() -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], 11434)),
        std::time::Duration::from_millis(900),
    )
    .is_ok()
}

fn ollama_model_exists(model: &str) -> bool {
    let Ok(out) = std::process::Command::new("ollama")
        .env("PATH", augmented_path_for_ollama_cli())
        .arg("list")
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let txt = String::from_utf8_lossy(&out.stdout).to_ascii_lowercase();
    txt.contains(&model.to_ascii_lowercase())
}

fn run_shell(script: &str) -> Result<String, String> {
    let out = std::process::Command::new("sh")
        .env("PATH", augmented_path_for_ollama_cli())
        .arg("-lc")
        .arg(script)
        .output()
        .map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    if out.status.success() {
        Ok(if stdout.trim().is_empty() { stderr } else { stdout })
    } else {
        Err(format!(
            "{}{}{}",
            if stderr.trim().is_empty() {
                ""
            } else {
                stderr.trim()
            },
            if stderr.trim().is_empty() || stdout.trim().is_empty() {
                ""
            } else {
                "\n"
            },
            stdout.trim()
        ))
    }
}

fn ollama_install_script() -> &'static str {
    "curl -fsSL https://ollama.com/install.sh | sh"
}

fn ollama_start_script() -> &'static str {
    "nohup ollama serve >/tmp/eson-ollama.log 2>&1 &"
}

fn log_ollama(
    app: &tauri::AppHandle,
    phase: OllamaInstallPhase,
    msg: impl Into<String>,
    err: bool,
) {
    let msg = msg.into();
    {
        let state = app.state::<Mutex<ServicesState>>();
        let mut g = state.lock().unwrap();
        g.ollama.phase = phase;
        let line = if msg.len() > 280 {
            format!("{}…", &msg[..280])
        } else {
            msg.clone()
        };
        g.ollama.progress_log_tail.push(line);
        if g.ollama.progress_log_tail.len() > 60 {
            let drain = g.ollama.progress_log_tail.len().saturating_sub(60);
            g.ollama.progress_log_tail.drain(0..drain);
        }
        if err {
            g.ollama.last_error = Some(msg.clone());
        }
    }
    let _ = app.emit(
        "ollama:status",
        serde_json::json!({
            "phase": phase,
            "message": msg,
            "error": err,
        }),
    );
}

fn refresh_ollama_state(app: &tauri::AppHandle) -> OllamaStatus {
    let installed = ollama_cmd_exists();
    let running = installed && ollama_running_probe();
    let model_ready = installed && running && ollama_model_exists(OLLAMA_TARGET_MODEL);
    let state = app.state::<Mutex<ServicesState>>();
    let mut g = state.lock().unwrap();
    g.ollama.installed = installed;
    g.ollama.running = running;
    g.ollama.model_ready = model_ready;
    if !g.ollama.in_progress {
        g.ollama.phase = if model_ready {
            OllamaInstallPhase::Ready
        } else {
            OllamaInstallPhase::Idle
        };
        if model_ready {
            g.ollama.last_error = None;
        }
    }
    g.ollama.snapshot()
}

#[tauri::command]
fn ollama_status(app: tauri::AppHandle) -> OllamaStatus {
    refresh_ollama_state(&app)
}

#[tauri::command]
async fn install_ollama_with_model(app: tauri::AppHandle) -> Result<OllamaStatus, String> {
    #[cfg(not(target_os = "macos"))]
    {
        return Err("Ollama auto-install is currently supported on macOS only.".into());
    }
    #[cfg(target_os = "macos")]
    {
        {
            let state = app.state::<Mutex<ServicesState>>();
            let mut g = state.lock().unwrap();
            if g.ollama.in_progress {
                return Ok(g.ollama.snapshot());
            }
            g.ollama.in_progress = true;
            g.ollama.phase = OllamaInstallPhase::Checking;
            g.ollama.last_error = None;
            g.ollama.progress_log_tail.clear();
        }
        log_ollama(&app, OllamaInstallPhase::Checking, "Checking Ollama install…", false);

        let task_app = app.clone();
        tauri::async_runtime::spawn(async move {
            let task_app_blocking = task_app.clone();
            let task = tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
                if !ollama_cmd_exists() {
                    log_ollama(
                        &task_app_blocking,
                        OllamaInstallPhase::InstallingOllama,
                        "Ollama not found. Installing via official script…",
                        false,
                    );
                    match run_shell(ollama_install_script()) {
                        Ok(out) => {
                            if !out.trim().is_empty() {
                                log_ollama(
                                    &task_app_blocking,
                                    OllamaInstallPhase::InstallingOllama,
                                    out,
                                    false,
                                );
                            }
                        }
                        Err(e) => return Err(format!("Ollama install failed: {e}")),
                    }
                } else {
                    log_ollama(
                        &task_app_blocking,
                        OllamaInstallPhase::Checking,
                        "Ollama already installed.",
                        false,
                    );
                }

                log_ollama(
                    &task_app_blocking,
                    OllamaInstallPhase::StartingOllama,
                    "Ensuring Ollama service is running…",
                    false,
                );
                if !ollama_running_probe() {
                    if let Err(e) = run_shell(ollama_start_script()) {
                        return Err(format!("Failed to start `ollama serve`: {e}"));
                    }
                    let start = std::time::Instant::now();
                    let timeout = std::time::Duration::from_secs(35);
                    while !ollama_running_probe() {
                        if start.elapsed() > timeout {
                            return Err(
                                "Timed out waiting for Ollama service at 127.0.0.1:11434".into(),
                            );
                        }
                        std::thread::sleep(std::time::Duration::from_millis(700));
                    }
                }
                log_ollama(
                    &task_app_blocking,
                    OllamaInstallPhase::StartingOllama,
                    "Ollama service is up.",
                    false,
                );

                if !ollama_model_exists(OLLAMA_TARGET_MODEL) {
                    log_ollama(
                        &task_app_blocking,
                        OllamaInstallPhase::PullingModel,
                        format!("Pulling model `{OLLAMA_TARGET_MODEL}` (this can take a while)…"),
                        false,
                    );
                    let pull = std::process::Command::new("ollama")
                        .env("PATH", augmented_path_for_ollama_cli())
                        .arg("pull")
                        .arg(OLLAMA_TARGET_MODEL)
                        .output()
                        .map_err(|e| format!("Failed to launch `ollama pull`: {e}"))?;
                    let out = format!(
                        "{}{}{}",
                        String::from_utf8_lossy(&pull.stdout),
                        if pull.stdout.is_empty() || pull.stderr.is_empty() {
                            ""
                        } else {
                            "\n"
                        },
                        String::from_utf8_lossy(&pull.stderr),
                    );
                    if !pull.status.success() {
                        return Err(format!("Model pull failed: {}", out.trim()));
                    }
                    if !out.trim().is_empty() {
                        log_ollama(&task_app_blocking, OllamaInstallPhase::PullingModel, out, false);
                    }
                } else {
                    log_ollama(
                        &task_app_blocking,
                        OllamaInstallPhase::PullingModel,
                        format!("Model `{OLLAMA_TARGET_MODEL}` already available."),
                        false,
                    );
                }
                Ok(())
            })
            .await;

            let result = match task {
                Ok(r) => r,
                Err(e) => Err(format!("Installer task join failed: {e}")),
            };
            {
                let state = task_app.state::<Mutex<ServicesState>>();
                let mut g = state.lock().unwrap();
                g.ollama.in_progress = false;
                match result {
                    Ok(()) => {
                        g.ollama.installed = true;
                        g.ollama.running = true;
                        g.ollama.model_ready = true;
                        g.ollama.phase = OllamaInstallPhase::Ready;
                        g.ollama.last_error = None;
                    }
                    Err(ref e) => {
                        g.ollama.phase = OllamaInstallPhase::Failed;
                        g.ollama.last_error = Some(e.clone());
                        g.ollama.installed = ollama_cmd_exists();
                        g.ollama.running = g.ollama.installed && ollama_running_probe();
                        g.ollama.model_ready =
                            g.ollama.installed && g.ollama.running && ollama_model_exists(OLLAMA_TARGET_MODEL);
                    }
                }
            }
            match result {
                Ok(()) => log_ollama(
                    &task_app,
                    OllamaInstallPhase::Ready,
                    format!("Ollama ready with `{OLLAMA_TARGET_MODEL}`."),
                    false,
                ),
                Err(e) => log_ollama(&task_app, OllamaInstallPhase::Failed, e, true),
            }
        });

        Ok(refresh_ollama_state(&app))
    }
}

fn agent_url() -> String {
    std::env::var("VITE_ESON_AGENT_URL").unwrap_or_else(|_| "http://127.0.0.1:8787".to_string())
}

#[tauri::command]
fn services_status(state: State<Mutex<ServicesState>>) -> ServicesStatus {
    state.lock().unwrap().snapshot()
}

/// Mirror a directory recursively, **always overwriting** the destination.
/// Used for `persona/` and `skills/` which are app-shipped (effectively
/// part of the system prompt) and must refresh whenever the user installs
/// a new build — otherwise edits to `persona/Eson.md` between releases
/// would never reach the running agent.
///
/// Files that exist *only* in the destination are left alone, so anyone
/// dropping a custom persona file into the data dir keeps it; matching
/// filenames from the bundle take precedence.
fn copy_dir_overwrite(src: &Path, dst: &Path) -> std::io::Result<()> {
    if !src.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_overwrite(&from, &to)?;
        } else {
            // Best-effort overwrite. `std::fs::copy` truncates the
            // destination, so this is safe for in-place upgrades.
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Resolve a bundled resource by trying multiple candidate paths:
///   1. `resources/<rel>` under the Tauri Resource dir (production layout
///      written by `scripts/build-installer.sh`).
///   2. `<rel>` directly under the Resource dir (legacy / fallback).
///   3. `<cwd>/<rel>` for `cargo tauri dev` runs from the repo root.
///   4. Walk up from `current_exe` looking for `<rel>` (covers
///      `target/debug` development layouts).
fn locate_resource(app: &tauri::AppHandle, rel: &str) -> Option<PathBuf> {
    let staged = format!("resources/{rel}");
    for candidate in [staged.as_str(), rel] {
        if let Ok(p) = app
            .path()
            .resolve(candidate, tauri::path::BaseDirectory::Resource)
        {
            if p.exists() {
                return Some(p);
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join(rel);
        if p.exists() {
            return Some(p);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut cur = exe.parent().map(Path::to_path_buf);
        while let Some(dir) = cur {
            let candidate = dir.join(rel);
            if candidate.exists() {
                return Some(candidate);
            }
            cur = dir.parent().map(Path::to_path_buf);
        }
    }
    None
}

fn provision_user_data(
    app: &tauri::AppHandle,
) -> Result<(PathBuf, PathBuf, PathBuf), String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    let workspace_root = app_data.join("workspace");
    let persona_dir = app_data.join("persona");
    let skills_dir = app_data.join("skills");

    std::fs::create_dir_all(&workspace_root)
        .map_err(|e| format!("create workspace: {e}"))?;
    for sub in ["db", "docs", "exports", "images", "inbox", "index", "logs"] {
        let _ = std::fs::create_dir_all(workspace_root.join(sub));
    }

    // `persona/` and `skills/` are app-shipped (effectively part of the
    // system prompt) — overwrite on every launch so edits to
    // `persona/Eson.md` etc. between releases actually reach the agent.
    // See `copy_dir_overwrite` for the file-survival semantics.
    if let Some(src) = locate_resource(app, "persona") {
        copy_dir_overwrite(&src, &persona_dir)
            .map_err(|e| format!("seed persona: {e}"))?;
    } else {
        std::fs::create_dir_all(&persona_dir).ok();
    }
    if let Some(src) = locate_resource(app, "skills") {
        copy_dir_overwrite(&src, &skills_dir)
            .map_err(|e| format!("seed skills: {e}"))?;
    } else {
        std::fs::create_dir_all(&skills_dir).ok();
    }

    let secrets_path = app_data.join("secrets.env");
    if !secrets_path.exists() {
        let stub = "\
# Eson secrets file (KEY=VALUE per line). Loaded by the desktop shell on
# launch and forwarded as env vars to the eson-agent + eson-memory sidecars.
# Restart the app after editing.
#
# ANTHROPIC_API_KEY=sk-ant-...
# ANTHROPIC_MODEL=claude-haiku-4-5-20251001
# OPENAI_API_KEY=sk-...
# OPENAI_MODEL=gpt-4o-mini
# OLLAMA_BASE_URL=http://127.0.0.1:11434
# OLLAMA_MODEL=gemma4:e4b
#
# Anthropic extended-thinking budget (tokens). 0 disables, default 1024.
# Emits an `llm_thinking` event per round shown in the chat reasoning panel.
# ANTHROPIC_THINKING_BUDGET=1024
#
# Vision routing for analyze_visual / pdf_to_table. The user can also pick
# the provider live in Settings → Vision (no restart). Values:
#   ESON_VISION_PROVIDER = ollama | anthropic | openai     (default: ollama)
#   ESON_VISION_MODEL    = e.g. gemma4:e4b | claude-haiku-4-5-20251001 | gpt-4o-mini
# ESON_VISION_PROVIDER=ollama
# ESON_VISION_MODEL=gemma4:e4b
#
# Per-call HTTP timeout for every LLM request (connect + streaming body).
# Default 600 s (10 min). Raise for slow local models that take longer to
# finish one round; cloud providers stay well under this in practice. Can
# also be overridden per session from Settings → AI Provider → Advanced
# (no restart needed).
# ESON_LLM_HTTP_TIMEOUT_SECS=600
#
# Max LLM↔tool orchestration rounds per user message (each round is at
# least one model call). Default 1000 — effectively unlimited for local
# models. Hard-capped at 1000.
# ESON_MAX_LLM_TOOL_ROUNDS=1000
";
        let _ = std::fs::write(&secrets_path, stub);
    }
    Ok((workspace_root, persona_dir, skills_dir))
}

/// Parse a tiny KEY=VALUE env file (supporting `#` line comments and
/// optional surrounding quotes) into a vector of (key, value) pairs.
/// We intentionally do *not* shell-expand or interpret `export ` prefixes
/// — this is meant for plain secrets.
fn parse_env_file(path: &Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        let key = k.trim().to_string();
        if key.is_empty() {
            continue;
        }
        let mut val = v.trim().to_string();
        if (val.starts_with('"') && val.ends_with('"') && val.len() >= 2)
            || (val.starts_with('\'') && val.ends_with('\'') && val.len() >= 2)
        {
            val = val[1..val.len() - 1].to_string();
        }
        out.push((key, val));
    }
    out
}

fn spawn_sidecar(
    app: &tauri::AppHandle,
    bin: &'static str,
    workspace_root: &Path,
    persona_dir: &Path,
    skills_dir: &Path,
    extra_env: &[(String, String)],
) -> Result<CommandChild, String> {
    let mut cmd = app
        .shell()
        .sidecar(bin)
        .map_err(|e| format!("sidecar({bin}): {e}"))?
        // Pin cwd to the user-data workspace so the sidecar's `dotenvy`
        // search (if it ever runs) can't accidentally walk into a developer
        // checkout's `.env.local` and override our `ESON_WORKSPACE_ROOT`.
        .current_dir(workspace_root)
        .env("ESON_WORKSPACE_ROOT", workspace_root.display().to_string())
        .env("ESON_PERSONA_DIR", persona_dir.display().to_string())
        .env("ESON_SKILLS_DIR", skills_dir.display().to_string())
        // Belt-and-suspenders: tell modern agent binaries to skip dotenv
        // entirely. The pinned cwd above protects older binaries.
        .env("ESON_SKIP_DOTENV", "1");
    for (k, v) in extra_env {
        cmd = cmd.env(k, v);
    }
    let (mut rx, child) = cmd.spawn().map_err(|e| format!("spawn {bin}: {e}"))?;
    let label = bin.to_string();
    let app_handle = app.clone();
    let pump = tauri::async_runtime::spawn(async move {
        while let Some(ev) = rx.recv().await {
            match ev {
                CommandEvent::Stdout(line) => {
                    let s = String::from_utf8_lossy(&line).to_string();
                    tracing::info!(target: "sidecar", "{label}: {s}");
                    let _ = app_handle.emit("sidecar:log", (&label, "stdout", s));
                }
                CommandEvent::Stderr(line) => {
                    let s = String::from_utf8_lossy(&line).to_string();
                    tracing::info!(target: "sidecar", "{label}: {s}");
                    let _ = app_handle.emit("sidecar:log", (&label, "stderr", s));
                }
                CommandEvent::Terminated(payload) => {
                    let code = payload.code.unwrap_or_default();
                    tracing::warn!(target: "sidecar", "{label} exited code={code}");
                    let _ = app_handle.emit("sidecar:exit", (&label, code));
                    break;
                }
                _ => {}
            }
        }
    });
    let state = app.state::<Mutex<ServicesState>>();
    state.lock().unwrap().pumps.push(pump);
    Ok(child)
}

fn start_services(app: &tauri::AppHandle) -> Result<(), String> {
    let (workspace_root, persona_dir, skills_dir) = provision_user_data(app)?;

    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("app_data_dir: {e}"))?;
    let secrets_path = app_data.join("secrets.env");
    let extra_env = parse_env_file(&secrets_path);
    if !extra_env.is_empty() {
        let keys: Vec<&str> = extra_env.iter().map(|(k, _)| k.as_str()).collect();
        tracing::info!(
            path = %secrets_path.display(),
            keys = ?keys,
            "loaded sidecar env from secrets.env"
        );
    } else {
        tracing::info!(
            path = %secrets_path.display(),
            "no secrets.env entries (edit this file to set provider API keys)"
        );
    }

    {
        let state = app.state::<Mutex<ServicesState>>();
        let mut g = state.lock().unwrap();
        g.workspace_root = workspace_root.clone();
        g.persona_dir = persona_dir.clone();
        g.skills_dir = skills_dir.clone();
        g.last_event = Some("starting".to_string());
    }

    let memory_child = spawn_sidecar(
        app,
        "eson-memory",
        &workspace_root,
        &persona_dir,
        &skills_dir,
        &extra_env,
    )?;
    {
        let state = app.state::<Mutex<ServicesState>>();
        state.lock().unwrap().memory = Some(memory_child);
    }

    // Tiny stagger so memory's port-listen log fires before the agent looks it up.
    std::thread::sleep(std::time::Duration::from_millis(750));

    let agent_child = spawn_sidecar(
        app,
        "eson-agent",
        &workspace_root,
        &persona_dir,
        &skills_dir,
        &extra_env,
    )?;
    {
        let state = app.state::<Mutex<ServicesState>>();
        let mut g = state.lock().unwrap();
        g.agent = Some(agent_child);
        g.last_event = Some("started".to_string());
    }
    let _ = app.emit("services:status", app.state::<Mutex<ServicesState>>().lock().unwrap().snapshot());
    Ok(())
}

fn stop_services(app: &tauri::AppHandle) {
    let state = app.state::<Mutex<ServicesState>>();
    let mut g = state.lock().unwrap();
    if let Some(child) = g.agent.take() {
        let _ = child.kill();
    }
    if let Some(child) = g.memory.take() {
        let _ = child.kill();
    }
    for pump in g.pumps.drain(..) {
        pump.abort();
    }
    g.last_event = Some("stopped".to_string());
}

/// User can also click "Continue anyway" (or hit Retry on transient errors)
/// from the preflight overlay; both routes call this command.
#[tauri::command]
fn start_services_cmd(app: tauri::AppHandle) -> Result<ServicesStatus, String> {
    if app
        .state::<Mutex<ServicesState>>()
        .lock()
        .unwrap()
        .agent
        .is_some()
    {
        return Ok(app
            .state::<Mutex<ServicesState>>()
            .lock()
            .unwrap()
            .snapshot());
    }
    start_services(&app)?;
    Ok(app
        .state::<Mutex<ServicesState>>()
        .lock()
        .unwrap()
        .snapshot())
}

/// Reveal a workspace path in the OS file manager (Finder / Explorer /
/// xdg-open). The path is resolved relative to the workspace root and must
/// stay inside it — we never expose arbitrary filesystem reveal to the UI.
#[tauri::command]
fn reveal_workspace_path(
    app: tauri::AppHandle,
    rel: Option<String>,
) -> Result<String, String> {
    let workspace_root = {
        let state = app.state::<Mutex<ServicesState>>();
        let g = state.lock().unwrap();
        g.workspace_root.clone()
    };
    if workspace_root.as_os_str().is_empty() {
        return Err("workspace not initialized yet".into());
    }
    let canonical_root = workspace_root
        .canonicalize()
        .map_err(|e| format!("canonicalize root: {e}"))?;

    let target = match rel.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(r) => {
            if Path::new(r).is_absolute() || r.contains("..") {
                return Err("path must be a relative subpath inside the workspace".into());
            }
            canonical_root.join(r)
        }
        None => canonical_root.clone(),
    };
    let canonical_target = target
        .canonicalize()
        .map_err(|e| format!("canonicalize target: {e}"))?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err("path escapes workspace root".into());
    }

    let path_str = canonical_target.to_string_lossy().to_string();
    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("open").arg(&path_str).status();
    #[cfg(target_os = "windows")]
    let status = std::process::Command::new("explorer").arg(&path_str).status();
    #[cfg(all(unix, not(target_os = "macos")))]
    let status = std::process::Command::new("xdg-open").arg(&path_str).status();

    match status {
        Ok(s) if s.success() => Ok(path_str),
        Ok(s) => Err(format!("file manager exited with status {s}")),
        Err(e) => Err(format!("failed to spawn file manager: {e}")),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("eson_desktop=info")),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        // Routes `fetch` from `@tauri-apps/plugin-http` through Rust's
        // `reqwest` instead of the WebKit `URLSession`. Critical for
        // long-running `/session/message` POSTs against `eson-agent`:
        // WebKit's `URLSession.timeoutIntervalForRequest` defaults to
        // 60 s and we can't override it from JS, so multi-round
        // local-Ollama turns get a generic "Load failed" mid-stream.
        // The plugin's transport has no such cap.
        .plugin(tauri_plugin_http::init())
        .manage(Mutex::new(ServicesState::default()))
        .invoke_handler(tauri::generate_handler![
            system_info,
            services_status,
            start_services_cmd,
            reveal_workspace_path,
            ollama_status,
            install_ollama_with_model
        ])
        .setup(|app| {
            // Auto-start sidecars only if hardware passes preflight; the UI
            // can also call `start_services_cmd` after user override.
            let info = system_info();
            if info.meets_requirements {
                let handle = app.handle().clone();
                if let Err(e) = start_services(&handle) {
                    tracing::error!(error = %e, "failed to start sidecars");
                    let _ = handle.emit("services:error", e);
                }
            } else {
                tracing::warn!(
                    cpu = info.cpu_logical,
                    mem_gib = info.memory_total_gib,
                    "hardware below minimum — deferring sidecar start"
                );
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error building Eson")
        .run(|app, event| {
            if let RunEvent::ExitRequested { .. } = event {
                stop_services(app);
            }
        });
}
