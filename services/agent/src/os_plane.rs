//! OS health and process listing — macOS v1, stubs elsewhere (Windows Phase 9).

use serde_json::{json, Value};

#[cfg(target_os = "macos")]
pub fn get_os_health() -> Value {
    use sysinfo::System;

    let mut sys = System::new_all();
    sys.refresh_all();

    let total_mem = sys.total_memory();
    let used_mem = sys.used_memory();
    let cpus: Vec<f32> = sys.cpus().iter().map(|c| c.cpu_usage()).collect();
    let cpu_avg = if cpus.is_empty() {
        0.0
    } else {
        cpus.iter().sum::<f32>() / cpus.len() as f32
    };

    json!({
        "platform": "macos",
        "cpu_usage_percent_avg": cpu_avg,
        "memory_total_bytes": total_mem,
        "memory_used_bytes": used_mem,
        "memory_used_percent": if total_mem > 0 { (used_mem as f64 / total_mem as f64) * 100.0 } else { 0.0 },
        "gpu": { "status": "unsupported_detail", "note": "Metal metrics optional follow-up" }
    })
}

#[cfg(not(target_os = "macos"))]
pub fn get_os_health() -> Value {
    json!({
        "platform": "other",
        "note": "Windows adapters ship in Phase 9; use stubs on this build target.",
        "cpu_usage_percent_avg": null,
        "memory_total_bytes": null,
        "memory_used_bytes": null
    })
}

#[cfg(target_os = "macos")]
pub fn list_processes(limit: usize) -> Value {
    use sysinfo::System;

    let mut sys = System::new_all();
    sys.refresh_all();

    let mut rows: Vec<Value> = sys
        .processes()
        .iter()
        .take(limit)
        .map(|(pid, p)| {
            json!({
                "pid": pid.as_u32(),
                "name": p.name().to_string_lossy(),
                "memory_bytes": p.memory(),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        let ma = a["memory_bytes"].as_u64().unwrap_or(0);
        let mb = b["memory_bytes"].as_u64().unwrap_or(0);
        mb.cmp(&ma)
    });
    json!({ "processes": rows })
}

#[cfg(not(target_os = "macos"))]
pub fn list_processes(_limit: usize) -> Value {
    json!({ "processes": [], "note": "stub until Windows port" })
}
