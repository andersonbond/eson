//! Heartbeat + inbox scheduling helpers (gateway binary wires `tokio::spawn` + I/O).

use chrono::{Datelike, Local, Timelike};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::skills::{list_all_skills, SkillEntry};

#[derive(Debug, Default, Serialize, Deserialize)]
struct CronState {
    /// skill id -> last fired unix ms
    last_fired_ms: HashMap<String, u64>,
    /// skill id -> last fired slot for daily HH:MM (e.g. "2026-04-19-09:30")
    last_daily_slot: HashMap<String, String>,
}

fn state_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join("db").join("cron_skills_state.json")
}

fn load_state(path: &Path) -> CronState {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, st: &CronState) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(path, serde_json::to_string_pretty(st).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse `every:15m`, `every:2h`, or `HH:MM` (24h local).
pub fn parse_interval_seconds(spec: &str) -> Option<u64> {
    let s = spec.trim();
    if let Some(rest) = s.strip_prefix("every:") {
        let rest = rest.trim();
        if let Some(n) = rest.strip_suffix('m') {
            return n.trim().parse::<u64>().ok().map(|m| m.saturating_mul(60));
        }
        if let Some(n) = rest.strip_suffix('h') {
            return n.trim().parse::<u64>().ok().map(|h| h.saturating_mul(3600));
        }
    }
    None
}

/// Local-time slot for an `HH:MM` cron, used to dedupe daily fires within the same minute.
fn daily_slot_for(spec: &str) -> Option<(u32, u32, String)> {
    let parts: Vec<&str> = spec.trim().split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let hh: u32 = parts[0].parse().ok()?;
    let mm: u32 = parts[1].parse().ok()?;
    if hh > 23 || mm > 59 {
        return None;
    }
    let local = Local::now();
    let d = local.date_naive();
    let slot = format!(
        "{}-{:02}-{:02}-{:02}:{:02}",
        d.year(),
        d.month(),
        d.day(),
        hh,
        mm
    );
    Some((hh, mm, slot))
}

/// Pure predicate: does this skill's cron spec want to fire right now?
pub fn cron_skill_due(skill: &SkillEntry, workspace_root: &Path) -> bool {
    let Some(spec) = skill.cron.as_ref() else {
        return false;
    };
    let st = load_state(&state_path(workspace_root));
    let now = now_ms();

    if let Some(every) = parse_interval_seconds(spec) {
        if every == 0 {
            return false;
        }
        let last = st.last_fired_ms.get(&skill.id).copied().unwrap_or(0);
        return now.saturating_sub(last) >= every.saturating_mul(1000);
    }

    let Some((hh, mm, slot)) = daily_slot_for(spec) else {
        return false;
    };
    let local = Local::now();
    if local.hour() != hh || local.minute() != mm {
        return false;
    }
    st.last_daily_slot.get(&skill.id) != Some(&slot)
}

pub fn mark_cron_fired(skill_id: &str, workspace_root: &Path) {
    let path = state_path(workspace_root);
    let mut st = load_state(&path);
    st.last_fired_ms.insert(skill_id.to_string(), now_ms());
    let _ = save_state(&path, &st);
}

fn mark_daily_slot(skill_id: &str, slot: String, workspace_root: &Path) {
    let path = state_path(workspace_root);
    let mut st = load_state(&path);
    st.last_daily_slot.insert(skill_id.to_string(), slot);
    let _ = save_state(&path, &st);
}

/// Skills whose cron trigger is due.
pub fn due_cron_skills(workspace_root: &Path, skills_root: &Path) -> Vec<SkillEntry> {
    let mut out = Vec::new();
    for skill in list_all_skills(skills_root) {
        if skill.category != "cron" || !skill.enabled {
            continue;
        }
        if skill.cron.is_none() {
            continue;
        }
        if cron_skill_due(&skill, workspace_root) {
            out.push(skill);
        }
    }
    out
}

/// Persist whichever bookkeeping the spec uses: `every:Nm/h` updates `last_fired_ms`,
/// daily `HH:MM` updates the `last_daily_slot` for today's slot.
pub fn after_cron_run(skill: &SkillEntry, workspace_root: &Path) {
    let Some(spec) = skill.cron.as_ref() else {
        return;
    };
    if parse_interval_seconds(spec).is_some() {
        mark_cron_fired(&skill.id, workspace_root);
        return;
    }
    if let Some((_, _, slot)) = daily_slot_for(spec) {
        mark_daily_slot(&skill.id, slot, workspace_root);
    }
}

pub fn debounce_duration() -> Duration {
    std::env::var("ESON_INBOX_DEBOUNCE_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(500))
}

pub fn heartbeat_interval() -> Duration {
    let sec = std::env::var("ESON_HEARTBEAT_SEC")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60u64)
        .clamp(10, 3600);
    Duration::from_secs(sec)
}

pub fn background_loop_enabled() -> bool {
    std::env::var("ESON_BACKGROUND_LOOP_ENABLED")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub fn inbox_auto_enabled() -> bool {
    std::env::var("ESON_INBOX_AUTO_PROCESS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(true)
}
