//! Consent-based company forwarding ("fleet mode").
//!
//! Company deployments (work/sales phones managed under a documented policy)
//! configure a forwarding endpoint and every stored message is POSTed to it —
//! e.g. the concierge hub, so all fleet communications consolidate in one
//! place.
//!
//! VISIBILITY IS THE CONSENT MODEL: while forwarding is enabled the desktop UI
//! shows a persistent banner ("Company message forwarding enabled →
//! <endpoint>"), `/health` reports the endpoint, and there is no covert mode.
//!
//! Configuration is runtime + persisted (`settings.json`, 0600): the operator
//! toggles forwarding from the UI with no restart. `HUB_FORWARD_URL` /
//! `HUB_FORWARD_TOKEN` act as a first-run seed only (MDM/headless
//! provisioning); after that the settings file wins.
//!
//! Transport is curl (fire-and-forget, never blocks message flow) to keep the
//! dependency tree at zero.

use crate::store::Message;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::{Mutex, OnceLock, RwLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardCfg {
    pub enabled: bool,
    pub url: String,
    #[serde(default)]
    pub token: String,
}

impl Default for ForwardCfg {
    fn default() -> Self {
        Self { enabled: false, url: String::new(), token: String::new() }
    }
}

/// Runtime stats surfaced in the UI.
pub struct Stats {
    pub forwarded: AtomicU64,
    pub failed: AtomicU64,
    pub last_ts: Mutex<Option<f64>>,
    pub last_error: Mutex<Option<String>>,
}

impl Stats {
    fn new() -> Self {
        Self {
            forwarded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            last_ts: Mutex::new(None),
            last_error: Mutex::new(None),
        }
    }
}

fn cfg_slot() -> &'static RwLock<ForwardCfg> {
    static CFG: OnceLock<RwLock<ForwardCfg>> = OnceLock::new();
    CFG.get_or_init(|| RwLock::new(ForwardCfg::default()))
}

fn stats_slot() -> &'static Stats {
    static STATS: OnceLock<Stats> = OnceLock::new();
    STATS.get_or_init(Stats::new)
}

fn settings_path() -> std::path::PathBuf {
    let dir = dirs::data_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("signal-whatsapp-hub");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::set_permissions(
        &dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    );
    dir.join("settings.json")
}

/// Seed order on boot: settings.json wins; HUB_FORWARD_URL/TOKEN seed a first
/// run only (MDM/headless provisioning). Called once from `run()`.
pub fn init() {
    let mut cfg = ForwardCfg::default();
    if let Ok(text) = std::fs::read_to_string(settings_path()) {
        if let Ok(parsed) = serde_json::from_str::<ForwardCfg>(&text) {
            cfg = parsed;
        }
    } else if let Ok(url) = std::env::var("HUB_FORWARD_URL") {
        if !url.trim().is_empty() {
            cfg = ForwardCfg {
                enabled: true,
                url: url.trim().to_string(),
                token: std::env::var("HUB_FORWARD_TOKEN").unwrap_or_default(),
            };
        }
    }
    persist(&cfg);
    *cfg_slot().write().unwrap() = cfg;
}

fn persist(cfg: &ForwardCfg) {
    let path = settings_path();
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600) // contains the endpoint token
            .open(&path)
        {
            let _ = f.write_all(json.as_bytes());
        }
    }
}

pub fn get() -> ForwardCfg {
    cfg_slot().read().unwrap().clone()
}

/// Replace the config (endpoint/token/enabled). Validates the URL shape and
/// persists to settings.json. Returns Err(user-facing message) on bad input.
pub fn configure(enabled: bool, url: &str, token: &str) -> Result<(), String> {
    let url = url.trim().to_string();
    if enabled {
        if url.is_empty() {
            return Err("endpoint URL is required to enable forwarding".into());
        }
        let ok = url.starts_with("https://")
            || url.starts_with("http://127.0.0.1")
            || url.starts_with("http://localhost");
        if !ok {
            return Err("endpoint must be https:// (or localhost for testing)".into());
        }
    }
    let cfg = ForwardCfg { enabled, url, token: token.to_string() };
    persist(&cfg);
    *cfg_slot().write().unwrap() = cfg;
    Ok(())
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let mut cfg = cfg_slot().write().unwrap();
    if enabled && cfg.url.trim().is_empty() {
        return Err("endpoint URL is required to enable forwarding".into());
    }
    cfg.enabled = enabled;
    let cfg = cfg.clone();
    drop(cfg_slot());
    persist(&cfg);
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ForwardStatus {
    pub enabled: bool,
    pub configured: bool,
    pub url: String,
    pub forwarded_total: u64,
    pub failed_total: u64,
    pub last_ts: Option<f64>,
    pub last_error: Option<String>,
}

pub fn status() -> ForwardStatus {
    let cfg = get();
    let stats = stats_slot();
    ForwardStatus {
        enabled: cfg.enabled && !cfg.url.trim().is_empty(),
        configured: !cfg.url.trim().is_empty(),
        url: cfg.url.clone(),
        forwarded_total: stats.forwarded.load(Ordering::Relaxed),
        failed_total: stats.failed.load(Ordering::Relaxed),
        last_ts: stats.last_ts.lock().unwrap().clone(),
        last_error: stats.last_error.lock().unwrap().clone(),
    }
}

/// One-shot test fire from the UI: a synthetic message to the endpoint.
pub fn test_fire() -> Result<(), String> {
    let cfg = get();
    if cfg.url.trim().is_empty() {
        return Err("endpoint URL is required".into());
    }
    send(&cfg, &serde_json::json!({
        "platform": "test",
        "direction": "out",
        "source": "forward-test",
        "peer": "n/a",
        "peer_name": "n/a",
        "text": "forwarding test from Signal WhatsApp Hub",
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0),
    }))
}

fn send(cfg: &ForwardCfg, body: &serde_json::Value) -> Result<(), String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-sS",
        "-X",
        "POST",
        "--max-time",
        "10",
        "-w",
        "%{http_code}",
        "-o",
        "/dev/null",
        "-H",
        "Content-Type: application/json",
    ]);
    if !cfg.token.is_empty() {
        cmd.args(["-H", &format!("Authorization: Bearer {}", cfg.token)]);
    }
    cmd.args(["--data-binary", "@-", &cfg.url])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("curl spawn failed: {e}"))?;
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(body.to_string().as_bytes());
    }
    let out = child.wait_with_output().map_err(|e| format!("curl failed: {e}"))?;
    let code = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !code.starts_with('2') {
        return Err(format!("endpoint returned {code}"));
    }
    Ok(())
}

/// Called from the store for every persisted message. No-ops unless fleet
/// mode is enabled. Fire-and-forget: never blocks message flow.
pub fn forward(m: &Message) {
    let cfg = get();
    if !cfg.enabled || cfg.url.trim().is_empty() {
        return;
    }
    let stats = stats_slot();
    match send(&cfg, &serde_json::to_value(m).unwrap_or(serde_json::Value::Null)) {
        Ok(()) => {
            stats.forwarded.fetch_add(1, Ordering::Relaxed);
            *stats.last_ts.lock().unwrap() = Some(m.ts);
            *stats.last_error.lock().unwrap() = None;
        }
        Err(e) => {
            stats.failed.fetch_add(1, Ordering::Relaxed);
            *stats.last_error.lock().unwrap() = Some(e);
        }
    }
}
