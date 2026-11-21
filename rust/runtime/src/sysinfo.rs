//! `GET /system/info` — the machine snapshot behind OWUI 0.13's System-info
//! modal (open-terminal 0.17.x `utils/system.py`, polled every 5 s).
//!
//! Hand-rolled `/proc` + cgroup-v2 parsing replaces psutil; everything is
//! read-only and gVisor-safe (both filesystems are emulated under `runsc`).
//! The sampled snapshot is cached for 2 s (upstream `_sampled_at` parity) and
//! CPU percentages come from deltas between samples (the FIRST call reports
//! `null` — upstream does the same via `_previous`).
//!
//! Scopes: memory/cpu prefer the stricter visible cgroup limit (upstream
//! walks cgroup ancestors the same way), so a sandbox reports its pod budget
//! rather than the host. Processes are scoped `environment` (we run one user
//! per pod — the `username` narrowing upstream does for multi-user servers is
//! unnecessary here) and the top-10 CPU + top-10 RSS lists are merged, owners
//! stripped — upstream's exact response policy.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::auth::Authed;
use crate::error::ApiError;
use crate::state::AppState;

/// Seconds a sampled snapshot stays cached (upstream `collect_system_info`).
const CACHE_TTL: Duration = Duration::from_secs(2);
/// How many top processes per sort key survive the response filter.
const TOP_PROCESSES: usize = 10;

/// Service start anchor (upstream module-level `_started`).
fn service_start() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

/// Cached snapshot + the previous counters for delta math.
struct Sampler {
    snapshot: Option<Value>,
    sampled_at: Option<Instant>,
    previous: Option<Counters>,
}

#[derive(Clone)]
struct Counters {
    at: Instant,
    cpu_total: Option<f64>,
    cpu_idle: Option<f64>,
    cgroup_path: Option<String>,
    cgroup_cpu_seconds: Option<f64>,
    process_cpu: BTreeMap<u32, f64>,
}

impl Counters {
    fn new(at: Instant) -> Self {
        Self {
            at,
            cpu_total: None,
            cpu_idle: None,
            cgroup_path: None,
            cgroup_cpu_seconds: None,
            process_cpu: BTreeMap::new(),
        }
    }
}

fn sampler() -> &'static Mutex<Sampler> {
    static S: OnceLock<Mutex<Sampler>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(Sampler {
            snapshot: None,
            sampled_at: None,
            previous: None,
        })
    })
}

/// `GET /system/info` — the machine snapshot (auth-protected like upstream).
///
/// # Errors
///
/// Never fails on missing /proc fields — optional sections degrade to `null`
/// exactly like upstream's psutil `except` paths. The `Result` keeps the
/// handler signature uniform.
#[utoipa::path(
    get,
    path = "/system/info",
    operation_id = "get_system_info",
    tag = "meta",
    security(("brokerBearer" = [])),
    responses(
        (status = 200, description = "Machine snapshot (2 s cached)", body = serde_json::Value),
        (status = 401, body = shared::ErrorResponse)
    )
)]
pub async fn get_system_info(
    _auth: Authed,
    State(state): State<AppState>,
) -> Result<Json<Value>, ApiError> {
    // The workspace root is the `disk` anchor (upstream uses the session cwd;
    // our whole workspace IS the session scope).
    let workdir = state.config.workdir.clone();
    let snapshot = tokio::task::spawn_blocking(move || sample(&workdir))
        .await
        .map_err(|e| ApiError::Internal(format!("system info join failed: {e}")))?;
    Ok(Json(snapshot))
}

/// Sample the machine state (cached 2 s), blocking — call from spawn_blocking.
fn sample(workdir: &Path) -> Value {
    let mut guard = sampler().lock().expect("sampler lock");
    let now = Instant::now();
    if let (Some(snapshot), Some(at)) = (&guard.snapshot, guard.sampled_at) {
        if now.duration_since(at) < CACHE_TTL {
            return snapshot.clone();
        }
    }
    let mut counters = Counters::new(now);
    let snapshot = build_sample(workdir, guard.previous.as_ref(), &mut counters);
    guard.previous = Some(counters);
    guard.sampled_at = Some(now);
    guard.snapshot = Some(snapshot.clone());
    snapshot
}

/// One full sample: static facts + the delta-able sections.
fn build_sample(workdir: &Path, previous: Option<&Counters>, counters: &mut Counters) -> Value {
    let uname = nix::sys::utsname::uname();
    let (os, arch, hostname) = match uname {
        Ok(u) => (
            format!(
                "{} {}",
                u.sysname().to_string_lossy(),
                u.release().to_string_lossy()
            ),
            u.machine().to_string_lossy().into_owned(),
            u.nodename().to_string_lossy().into_owned(),
        ),
        Err(_) => (String::new(), String::new(), String::new()),
    };
    let container = Path::new("/.dockerenv").exists()
        || Path::new("/run/.containerenv").exists()
        || std::env::var_os("container").is_some();

    let mut data = json!({
        "sampled_at": unix_now(),
        "hostname": hostname,
        "os": os,
        "arch": arch,
        "environment": if container { "container" } else { "host" },
        "service_uptime_seconds": service_start().elapsed().as_secs(),
        "uptime_seconds": Value::Null,
        "cpu": { "percent": Value::Null, "cores": Value::Null, "scope": "host" },
        "memory": Value::Null,
        "disk": Value::Null,
        "network": Value::Null,
        "process_scope": "environment",
        "processes": Value::Null,
    });

    // Host uptime via /proc/uptime ("SECONDS.NANOSECONDS idle" — gVisor
    // emulates /proc).
    if let Some(raw) = read_text("/proc/uptime") {
        if let Some(up) = raw
            .split_whitespace()
            .next()
            .and_then(|s| s.split('.').next())
            .and_then(|s| s.parse::<u64>().ok())
        {
            data["uptime_seconds"] = json!(up);
        }
    }

    // cgroup v2 limits (memory/cpu quota) — the stricter visible ancestor.
    let group = read_cgroup();

    // --- CPU ---------------------------------------------------------------
    if let Some(stat) = read_cpu_stat() {
        // guest time is already included in user/nice on Linux (upstream).
        let total: f64 = stat
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 8 && *i != 9) // guest, guest_nice
            .map(|(_, v)| *v)
            .sum();
        let idle = stat.get(3).copied().unwrap_or(0.0) + stat.get(4).copied().unwrap_or(0.0); // idle + iowait
        counters.cpu_total = Some(total);
        counters.cpu_idle = Some(idle);
        let host_cores = f64::from(u8::try_from(cpu_count()).unwrap_or(u8::MAX));
        let mut cores = host_cores;
        let mut scope = "host";
        if let Some(cg) = &group {
            if let Some(cg_cores) = cg.cores {
                cores = cores.min(cg_cores);
            }
            if cg.cpu_seconds.is_some() {
                cores = cores.max(1.0);
                scope = "cgroup";
            }
        }
        let mut percent: Option<f64> = None;
        if let Some(prev) = previous {
            if let (Some(pt), Some(ci)) = (prev.cpu_total, prev.cpu_idle) {
                let dt = total - ci; // placeholder — recomputed below
                let _ = dt;
                let d_total = total - pt;
                let d_idle = idle - ci;
                percent = percent_of(d_total - d_idle, d_total);
            }
        }
        if scope == "cgroup" {
            if let Some(cg) = &group {
                if let Some(secs) = cg.cpu_seconds {
                    if let Some(prev) = previous {
                        if prev.cgroup_path == cg.path {
                            let d = secs - prev.cgroup_cpu_seconds.unwrap_or(secs);
                            percent = percent_of(d, counters.at.elapsed().as_secs_f64() * cores);
                        }
                    }
                }
                if cg.cpu_seconds.is_some() {
                    counters.cgroup_cpu_seconds = cg.cpu_seconds;
                    counters.cgroup_path.clone_from(&cg.path);
                }
            }
        }
        data["cpu"] = json!({ "percent": percent, "cores": cores, "scope": scope });
    }

    // --- memory ------------------------------------------------------------
    if let Some((total, available)) = read_meminfo() {
        data["memory"] = json!({
            "total": total,
            "used": total.saturating_sub(available),
            "scope": "host",
        });
    }
    if let Some(cg) = &group {
        if let Some(memory_total) = cg.memory_total {
            data["memory"] = json!({
                "total": memory_total,
                "used": cg.memory_used.unwrap_or(memory_total),
                "scope": "cgroup",
            });
        }
    }

    // --- disk (workspace anchor; nix statvfs = psutil.disk_usage) -----------
    if let Ok(fs) = nix::sys::statvfs::statvfs(workdir) {
        // The statvfs block size conventions: multiply blocks by
        // f_frsize (fragment size), the psutil equivalent. nix already
        // returns `u64` blocks — multiply, no conversions needed.
        let total = fs.blocks() * fs.fragment_size();
        let free = fs.blocks_available() * fs.fragment_size();
        data["disk"] = json!({
            "path": workdir.display().to_string(),
            "total": total,
            "used": total.saturating_sub(free),
            "free": free,
        });
    }

    // --- network (getifaddrs = psutil.net_if_addrs + net_if_stats) ---------
    let mut interfaces: Vec<Value> = Vec::new();
    if let Ok(addrs) = nix::ifaddrs::getifaddrs() {
        let mut by_name: BTreeMap<String, (Option<bool>, Vec<String>)> = BTreeMap::new();
        for ia in addrs {
            let name = ia.interface_name.clone();
            let entry = by_name.entry(name).or_insert_with(|| (None, Vec::new()));
            if entry.0.is_none() && ia.flags.contains(nix::net::if_::InterfaceFlags::IFF_UP) {
                entry.0 = Some(true);
            }
            if let Some(sa) = ia.address {
                if let Some(v) = sa.as_sockaddr_in().and_then(|a| a.to_string().into()) {
                    entry.1.push(v);
                } else if let Some(v) = sa.as_sockaddr_in6().map(std::string::ToString::to_string) {
                    entry.1.push(v);
                }
            }
        }
        for (name, (up, addresses)) in by_name {
            interfaces.push(json!({ "name": name, "up": up, "addresses": addresses }));
        }
        data["network"] = Value::Array(interfaces);
    }

    // --- processes (all of /proc; owner stripped on the wire) ---------------
    let page_size = 4096_u64; // statm pages → bytes; gVisor uses 4 KiB pages
    let mut processes: Vec<Value> = Vec::new();
    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        // No /proc: the processes list degrades to null (upstream's except path).
        return data;
    };
    for entry in proc_dir.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        // comm: the process name (first 15 chars, kernel-truncated like psutil).
        let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
            continue; // vanished mid-scan — upstream skips psutil.Error too
        };
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok();
        let rss = std::fs::read_to_string(format!("/proc/{pid}/statm"))
            .ok()
            .and_then(|s| {
                s.split_whitespace()
                    .nth(1)
                    .and_then(|p| p.parse::<u64>().ok())
                    .map(|pages| pages * page_size)
            });
        let seconds = stat.as_deref().and_then(parse_cpu_seconds);
        if let Some(secs) = seconds {
            counters.process_cpu.insert(pid, secs);
        }
        let cpu_percent = match (seconds, previous) {
            (Some(secs), Some(prev)) => prev
                .process_cpu
                .get(&pid)
                .and_then(|before| percent_of(secs - *before, counters.at.elapsed().as_secs_f64())),
            _ => None,
        };
        processes.push(json!({
            "pid": pid,
            "name": comm.trim(),
            "cpu_percent": cpu_percent,
            "memory_bytes": rss.unwrap_or(0),
        }));
    }
    // Top-10 by cpu and by RSS merged (upstream's exact filter; owners stripped).
    let mut by_cpu = processes.clone();
    by_cpu.sort_by_key(|p| p["cpu_percent"].as_f64().unwrap_or(0.0) as u64);
    by_cpu.reverse();
    let mut by_rss = processes.clone();
    by_rss.sort_by_key(|p| p["memory_bytes"].as_u64().unwrap_or(0));
    by_rss.reverse();
    let mut top: Vec<Value> = Vec::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    for group in [by_cpu, by_rss] {
        for p in group.into_iter().take(TOP_PROCESSES) {
            let pid = p["pid"].as_u64().unwrap_or(0);
            if seen.insert(pid) {
                top.push(p);
            }
        }
    }
    data["processes"] = Value::Array(top);

    // Setup-script status rides along only when the knob is active (task 13).
    if let Some(setup) = crate::sysinfo_setup::status() {
        data["setup"] = setup;
    }

    data
}

/// `round(max(0, min(100, x/y*100)), 1)`; `None` when the denominator is 0.
fn percent_of(part: f64, total: f64) -> Option<f64> {
    if total <= 0.0 {
        return None;
    }
    let pct = (part / total * 100.0).clamp(0.0, 100.0);
    Some((pct * 10.0).round() / 10.0)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Read a small file as a trimmed String.
fn read_text(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Logical CPU count (`psutil.cpu_count()` — configured, not online).
fn cpu_count() -> u64 {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|text| {
            let n = text.lines().filter(|l| l.starts_with("processor")).count() as u64;
            (n > 0).then_some(n)
        })
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get() as u64))
}

/// `/proc/stat` `cpu` line as f64 fields (`user nice system idle iowait irq
/// softirq steal guest guest_nice`).
fn read_cpu_stat() -> Option<Vec<f64>> {
    let line = read_text("/proc/stat")?;
    let cpu = line.lines().next()?;
    let mut fields = cpu.split_whitespace();
    fields.next()?; // "cpu"
    let values: Vec<f64> = fields.filter_map(|f| f.parse().ok()).collect();
    (!values.is_empty()).then_some(values)
}

/// `/proc/meminfo` → `(MemTotal, MemAvailable)` in bytes.
fn read_meminfo() -> Option<(u64, u64)> {
    let text = read_text("/proc/meminfo")?;
    let mut total = None;
    let mut available = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        match parts.next()? {
            "MemTotal:" => total = parts.next().and_then(|v| v.parse::<u64>().ok()),
            "MemAvailable:" => available = parts.next().and_then(|v| v.parse::<u64>().ok()),
            _ => {}
        }
    }
    let total = total? * 1024;
    let available = available? * 1024;
    Some((total, available))
}

/// utime + stime in seconds from a `/proc/<pid>/stat` line (100 Hz jiffies).
fn parse_cpu_seconds(stat: &str) -> Option<f64> {
    let tail = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = tail.split_whitespace().collect();
    // fields[0] = state (field 3); utime = field 14 → index 11; stime = field 15.
    let utime: f64 = fields.get(11)?.parse().ok()?;
    let stime: f64 = fields.get(12)?.parse().ok()?;
    Some((utime + stime) / 100.0)
}

/// The cgroup-v2 subtree limit: unified hierarchy via `/proc/self/cgroup`
/// (`0::/path`), then walk ANCESTORS from the leaf to `/` collecting the
/// strictest `cpu.max` / `memory.max` / `memory.current` and `cpu.stat`
/// usage — upstream `_cgroup` parity.
#[derive(Debug, Default, Clone)]
struct Cgroup {
    path: Option<String>,
    /// `cpu.max` quota/period as a FLOAT — fractional core limits (500m →
    /// 0.5) truncate under integer division (upstream: `int/int` division;
    /// review finding 12).
    cores: Option<f64>,
    cpu_seconds: Option<f64>,
    memory_total: Option<u64>,
    memory_used: Option<u64>,
}

fn read_cgroup() -> Option<Cgroup> {
    let self_cgroup = read_text("/proc/self/cgroup")?;
    let relative = self_cgroup
        .lines()
        .find(|l| l.starts_with("0::"))
        .map(|l| l[3..].to_string())?;
    let root = PathBuf::from("/sys/fs/cgroup");
    let dir = root.join(relative.trim_start_matches('/'));
    // The path must stay under the root (no `..` escapes) — upstream guard.
    if !dir.starts_with(&root) || relative.contains("..") {
        return None;
    }
    if !dir.join("cpu.stat").exists() {
        // No per-leaf cpu.stat: use the root hierarchy (upstream falls back).
        if !root.join("cpu.stat").exists() {
            return None;
        }
    }
    let mut cg = Cgroup::default();
    let mut leaf = dir.clone();
    let mut ancestors: Vec<PathBuf> = vec![leaf.clone()];
    while let Some(parent) = leaf.parent() {
        if parent == root || parent == Path::new("/") {
            break;
        }
        ancestors.push(parent.to_path_buf());
        leaf = parent.to_path_buf();
    }
    ancestors.push(root.clone());
    for a in &ancestors {
        // cpu.max: "<quota> <period>" or "max <period>".
        if let Some(line) = read_text(&a.join("cpu.max").to_string_lossy()) {
            let mut parts = line.split_whitespace();
            let quota = parts.next().unwrap_or("max");
            let period: u64 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(100_000);
            if let Ok(q) = quota.parse::<f64>() {
                if q > 0.0 && period > 0 {
                    let cores = q / period as f64;
                    cg.cores = Some(cg.cores.map_or(cores, |c| c.min(cores)));
                }
            }
        }
        if let Some(line) = read_text(&a.join("memory.max").to_string_lossy()) {
            if let Ok(max) = line.parse::<u64>() {
                if max > 0 {
                    cg.memory_total = Some(cg.memory_total.map_or(max, |m| m.min(max)));
                }
            }
        }
        if let Some(line) = read_text(&a.join("memory.current").to_string_lossy()) {
            if let Ok(cur) = line.parse::<u64>() {
                cg.memory_used = Some(cg.memory_used.map_or(cur, |u| u.max(cur)));
            }
        }
    }
    // usage_usec from the LEAF's cpu.stat.
    if let Some(stat) = read_text(&dir.join("cpu.stat").to_string_lossy()) {
        for line in stat.lines() {
            let mut parts = line.split_whitespace();
            if parts.next() == Some("usage_usec") {
                if let Some(usec) = parts.next().and_then(|v| v.parse::<u64>().ok()) {
                    cg.cpu_seconds = Some(usec as f64 / 1_000_000.0);
                }
            }
        }
    }
    cg.path = Some(dir.display().to_string());
    Some(cg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_of_clamps_and_rounds() {
        assert_eq!(percent_of(0.0, 0.0), None);
        assert_eq!(percent_of(50.0, 100.0), Some(50.0));
        assert_eq!(percent_of(150.0, 100.0), Some(100.0));
        assert_eq!(percent_of(-5.0, 100.0), Some(0.0));
        assert_eq!(percent_of(1.0, 3.0), Some(33.3));
    }

    #[test]
    fn stat_cpu_seconds_parses_jiffies() {
        // utime=100 stime=50 jiffies → 1.5 s at 100 Hz.
        let stat = "1234 (cat) S 1 1 1 0 -1 4194304 0 0 0 0 100 50 0 0 20 0 1 0 123 456 1 18446744073709551615 1 1 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let secs = parse_cpu_seconds(stat).unwrap();
        assert!((secs - 1.5).abs() < 1e-9, "{secs}");
    }

    #[test]
    fn sample_produces_the_upstream_shape() {
        let dir = tempfile::TempDir::new().unwrap();
        let v = sample(dir.path());
        for key in [
            "sampled_at",
            "hostname",
            "os",
            "arch",
            "environment",
            "service_uptime_seconds",
            "uptime_seconds",
            "cpu",
            "memory",
            "disk",
            "network",
            "process_scope",
            "processes",
        ] {
            assert!(v.get(key).is_some(), "missing {key}: {v}");
        }
        assert_eq!(v["process_scope"], "environment");
        // Cache: a second call within TTL returns the IDENTICAL snapshot.
        let again = sample(dir.path());
        assert_eq!(v, again, "2 s cache must pin the snapshot");
    }
}
