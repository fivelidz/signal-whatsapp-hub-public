//! The CLI bridge: everything that shells out to whatsapp-cli and signal-cli.
//!
//! Handles the CLI quirks: QR expiry/regen, whatsmeow's single-session
//! constraint, and signal-cli needing a recent Java (0.14.x → Java 25).

use crate::config::Config;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

/// Build the base signal-cli command: [cli, --config, dir, -a, account].
pub fn signal_base(cfg: &Config) -> Vec<String> {
    vec![
        cfg.signal_cli.to_string_lossy().to_string(),
        "--config".into(),
        cfg.signal_config_dir.to_string_lossy().to_string(),
        "-a".into(),
        cfg.signal_account.clone(),
    ]
}

/// Apply JAVA_HOME / PATH to a Command for signal-cli (needs Java 21+).
fn signal_env(cmd: &mut Command, cfg: &Config) {
    let jh = cfg.java_home.to_string_lossy().to_string();
    if cfg.java_home.is_dir() {
        cmd.env("JAVA_HOME", &jh);
        let path = std::env::var("PATH").unwrap_or_default();
        cmd.env("PATH", format!("{}/bin:{}", jh, path));
    }
}

/// WHATSAPP_DATA_DIR env for whatsapp-cli.
fn wa_env(cmd: &mut Command, cfg: &Config) {
    cmd.env("WHATSAPP_DATA_DIR", &cfg.whatsapp_auth);
    cmd.env("WHATSAPP_LOG_LEVEL", "ERROR");
}

// ─────────────────────────────── STATUS ────────────────────────────────

#[derive(serde::Serialize, Clone)]
pub struct ChannelStatus {
    pub channel: String,
    pub ok: bool,
    pub account: String,
    pub detail: String,
}

/// WhatsApp liveness: `whatsapp-cli status` → {"logged_in":bool,"phone":...}.
pub fn wa_status(cfg: &Config) -> ChannelStatus {
    let mut out = ChannelStatus {
        channel: "whatsapp".into(),
        ok: false,
        account: cfg.whatsapp_account.clone(),
        detail: String::new(),
    };
    if !cfg.whatsapp_cli.exists() {
        out.detail = "whatsapp-cli not found".into();
        return out;
    }
    let mut cmd = Command::new(&cfg.whatsapp_cli);
    cmd.arg("status");
    wa_env(&mut cmd, cfg);
    match cmd.output() {
        Ok(o) => {
            let s = String::from_utf8_lossy(&o.stdout);
            if let Some(line) = s.lines().find(|l| l.trim_start().starts_with('{')) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                    out.ok = v.get("logged_in").and_then(|x| x.as_bool()).unwrap_or(false);
                    if let Some(p) = v.get("phone").and_then(|x| x.as_str()) {
                        if !p.is_empty() {
                            out.account = p.to_string();
                        }
                    }
                    out.detail = if out.ok { "logged in".into() } else { "not linked".into() };
                    return out;
                }
            }
            out.detail = "unparseable status".into();
        }
        Err(e) => out.detail = format!("status error: {e}"),
    }
    out
}

/// Signal liveness: `signal-cli listGroups` returns 0 when registered.
pub fn signal_status(cfg: &Config) -> ChannelStatus {
    let mut out = ChannelStatus {
        channel: "signal".into(),
        ok: false,
        account: cfg.signal_account.clone(),
        detail: String::new(),
    };
    if !cfg.signal_cli.exists() {
        out.detail = "signal-cli not found".into();
        return out;
    }
    // IMPORTANT: do NOT shell out to `signal-cli listGroups` here. signal-cli
    // locks the account SQLite DB, and the long-lived `receive` loop already
    // holds that lock — a concurrent listGroups blocks indefinitely, which
    // froze the whole app. Instead read accounts.json directly: it's the
    // source of truth for "is an account linked", needs no JVM and no lock.
    let accounts_path = cfg.signal_config_dir.join("data/accounts.json");
    match std::fs::read_to_string(&accounts_path) {
        Ok(content) => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&content) {
                let accounts = v.get("accounts").and_then(|a| a.as_array());
                if let Some(arr) = accounts {
                    // linked if any account entry exists; prefer one matching
                    // our configured number, else take the first.
                    let found = arr.iter().find(|a| {
                        a.get("number").and_then(|n| n.as_str()) == Some(cfg.signal_account.as_str())
                    });
                    let acct = found.or_else(|| arr.first());
                    if let Some(a) = acct {
                        out.ok = true;
                        if let Some(num) = a.get("number").and_then(|n| n.as_str()) {
                            out.account = num.to_string();
                        }
                        out.detail = "linked".into();
                        return out;
                    }
                }
            }
            out.detail = "no account linked".into();
        }
        Err(_) => out.detail = "not linked".into(),
    }
    out
}

// ─────────────────────────────── SEND ──────────────────────────────────

/// Send a WhatsApp message. `recipient` should be the number WITHOUT '+'.
pub fn wa_send(cfg: &Config, recipient: &str, message: &str) -> Result<(), String> {
    if !cfg.whatsapp_cli.exists() {
        return Err("whatsapp-cli not found".into());
    }
    let recip = recipient.trim_start_matches('+');
    let mut cmd = Command::new(&cfg.whatsapp_cli);
    cmd.args(["send", recip, message]);
    wa_env(&mut cmd, cfg);
    let o = cmd.output().map_err(|e| format!("spawn error: {e}"))?;
    if o.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&o.stderr).trim().to_string())
    }
}

/// Send a Signal message to a +E.164 recipient.
pub fn signal_send(cfg: &Config, recipient: &str, message: &str) -> Result<(), String> {
    if !cfg.signal_cli.exists() {
        return Err("signal-cli not found".into());
    }
    let base = signal_base(cfg);
    let mut cmd = Command::new(&base[0]);
    cmd.args(&base[1..]);
    cmd.args(["send", "-m", message, recipient]);
    signal_env(&mut cmd, cfg);
    let o = cmd.output().map_err(|e| format!("spawn error: {e}"))?;
    let err = String::from_utf8_lossy(&o.stderr);
    if o.status.success() && !err.contains("Authorization failed") {
        Ok(())
    } else {
        Err(err.trim().to_string())
    }
}

// ─────────────────────────────── QR PNG ────────────────────────────────

/// Render a QR payload string to a base64 PNG data URI.
pub fn qr_data_uri(code: &str) -> Result<String, String> {
    use base64::Engine;
    use image::Luma;
    let qr = qrcode::QrCode::new(code.as_bytes()).map_err(|e| e.to_string())?;
    let img = qr
        .render::<Luma<u8>>()
        .quiet_zone(true)
        .module_dimensions(6, 6)
        .build();
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(buf.into_inner());
    Ok(format!("data:image/png;base64,{b64}"))
}

// ─────────────────────────────── RECEIVE STREAM ────────────────────────

/// An inbound message normalized from a CLI receive stream.
#[derive(Clone, Debug)]
pub struct Inbound {
    pub platform: String,
    pub peer: String,
    pub peer_name: String,
    pub text: String,
}

/// Spawn `whatsapp-cli receive` and call `on_msg` for each inbound message.
/// Blocks; intended to run on its own thread with a reconnect loop outside.
pub fn wa_receive_loop<F: FnMut(Inbound)>(cfg: &Config, mut on_msg: F, running: &dyn Fn() -> bool) {
    if !cfg.whatsapp_cli.exists() {
        return;
    }
    let mut cmd = Command::new(&cfg.whatsapp_cli);
    cmd.arg("receive");
    wa_env(&mut cmd, cfg);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Some(out) = child.stdout.take() {
        let reader = BufReader::new(out);
        for line in reader.lines().map_while(Result::ok) {
            if !running() {
                break;
            }
            let line = line.trim();
            if !(line.starts_with('{') && line.ends_with('}')) {
                continue;
            }
            if let Ok(d) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(m) = norm_wa(&d) {
                    on_msg(m);
                }
            }
        }
    }
    let _ = child.kill();
}

fn norm_wa(d: &serde_json::Value) -> Option<Inbound> {
    // whatsapp-cli emits typed objects: {"type":"message"|"connected"|
    // "disconnected", "sender":"61..","sender_name":"..","chat":"..",
    // "message":"..","is_group":bool,...}. Only real messages matter.
    let typ = d.get("type").and_then(|x| x.as_str()).unwrap_or("");
    if !typ.is_empty() && typ != "message" {
        return None;
    }
    let text = d
        .get("message")
        .or_else(|| d.get("text"))
        .or_else(|| d.get("body"))
        .and_then(|x| x.as_str())
        .unwrap_or("");
    if text.is_empty() {
        return None;
    }
    // For 1:1 chats peer == sender. For groups, key off the chat (group) JID
    // so the conversation groups correctly, but keep the sender's name.
    let is_group = d.get("is_group").and_then(|x| x.as_bool()).unwrap_or(false);
    let peer = if is_group {
        d.get("chat").and_then(|x| x.as_str()).unwrap_or("unknown")
    } else {
        d.get("sender")
            .or_else(|| d.get("from"))
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
    }
    .to_string();
    let peer_name = d
        .get("sender_name")
        .or_else(|| d.get("pushName"))
        .or_else(|| d.get("notify"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Some(Inbound {
        platform: "whatsapp".into(),
        peer,
        peer_name,
        text: text.to_string(),
    })
}

/// Run ONE finite-timeout `signal-cli receive` cycle (JSON lines), calling
/// `on_msg` per inbound. Returns when the cycle ends (timeout reached) so the
/// caller can loop. Using a FINITE timeout is critical: signal-cli locks the
/// account DB while running, so a `--timeout -1` streaming receive would hold
/// the lock forever and block every `send`/`status`. A finite timeout releases
/// the lock between cycles, letting sends slip in.
pub fn signal_receive_loop<F: FnMut(Inbound)>(
    cfg: &Config,
    mut on_msg: F,
    running: &dyn Fn() -> bool,
) {
    if !cfg.signal_cli.exists() {
        return;
    }
    let base = signal_base(cfg);
    let mut cmd = Command::new(&base[0]);
    cmd.args(&base[1..]);
    // wait up to 8s for messages this cycle, then exit (releasing the DB lock).
    cmd.args(["-o", "json", "receive", "--timeout", "8"]);
    signal_env(&mut cmd, cfg);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Some(out) = child.stdout.take() {
        let reader = BufReader::new(out);
        for line in reader.lines().map_while(Result::ok) {
            if !running() {
                break;
            }
            let line = line.trim();
            if !line.starts_with('{') {
                continue;
            }
            eprintln!("[hub] signal RAW: {}", line.chars().take(600).collect::<String>());
            if let Ok(d) = serde_json::from_str::<serde_json::Value>(line) {
                if let Some(m) = norm_signal(&d) {
                    on_msg(m);
                }
            }
        }
    }
    let _ = child.wait();
}

fn norm_signal(d: &serde_json::Value) -> Option<Inbound> {
    // signal-cli -o json: {"envelope":{"source":"+..","sourceName":"..",
    //   "dataMessage":{"message":".."}}}
    let env = d.get("envelope")?;
    let data = env.get("dataMessage")?;
    let text = data.get("message").and_then(|x| x.as_str()).unwrap_or("");
    if text.is_empty() {
        return None;
    }
    let peer = env
        .get("sourceNumber")
        .or_else(|| env.get("source"))
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let peer_name = env
        .get("sourceName")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Some(Inbound {
        platform: "signal".into(),
        peer,
        peer_name,
        text: text.to_string(),
    })
}

// ─────────────────────────────── QR LINK ───────────────────────────────

/// Run `whatsapp-cli login`, streaming QR payloads and success via callbacks.
/// `on_qr(code)` fires for each fresh QR; `on_linked(detail)` fires on success.
pub fn wa_login<FQ: FnMut(String), FL: FnMut(String)>(
    cfg: &Config,
    mut on_qr: FQ,
    mut on_linked: FL,
    running: &dyn Fn() -> bool,
) {
    if !cfg.whatsapp_cli.exists() {
        return;
    }
    let mut cmd = Command::new(&cfg.whatsapp_cli);
    cmd.arg("login");
    wa_env(&mut cmd, cfg);
    cmd.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Some(out) = child.stdout.take() {
        let reader = BufReader::new(out);
        for line in reader.lines().map_while(Result::ok) {
            if !running() {
                let _ = child.kill();
                return;
            }
            let line = line.trim();
            if let Some(code) = line.strip_prefix("QRCODE:") {
                on_qr(code.to_string());
            } else if line.contains("login_success") || line.contains("already_logged_in") {
                let phone = extract_json_str(line, "phone").unwrap_or_else(|| "linked".into());
                on_linked(phone);
                // CRITICAL: do NOT kill the process here. whatsmeow's login
                // handler sleeps ~5s after "success" to let the session
                // stabilize and PERSIST to whatsapp.db before it disconnects
                // and exits on its own. Killing it now loses the session and
                // the link silently fails. Wait for it to exit cleanly.
                let _ = child.wait();
                return;
            }
        }
    }
    let _ = child.wait();
}

/// Run `signal-cli link -n <name>`, capturing the sgnl:// URI and success.
/// NOTE: signal-cli prints the URI ONCE then blocks until linked — never kill
/// and respawn while waiting or you invalidate the QR being scanned.
pub fn signal_link<FQ: FnMut(String), FL: FnMut(String)>(
    cfg: &Config,
    device_name: &str,
    mut on_qr: FQ,
    mut on_linked: FL,
    running: &dyn Fn() -> bool,
) {
    if !cfg.signal_cli.exists() {
        return;
    }
    let mut cmd = Command::new(&cfg.signal_cli);
    // link is account-agnostic; don't pass -a (the account is assigned on link)
    cmd.args([
        "--config",
        &cfg.signal_config_dir.to_string_lossy(),
        "link",
        "-n",
        device_name,
    ]);
    signal_env(&mut cmd, cfg);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return,
    };

    // Drain stderr on a side thread; capture it so we can scan for the
    // "Associated with: +<number>" success line, which signal-cli may print
    // to EITHER stream depending on version.
    let stderr_buf = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let buf = stderr_buf.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(err);
            for line in reader.lines().map_while(Result::ok) {
                if let Ok(mut b) = buf.lock() {
                    b.push_str(&line);
                    b.push('\n');
                }
            }
        });
    }

    let mut saw_uri = false;
    if let Some(out) = child.stdout.take() {
        let reader = BufReader::new(out);
        for line in reader.lines().map_while(Result::ok) {
            if !running() {
                let _ = child.kill();
                return;
            }
            let line = line.trim();
            if line.starts_with("sgnl://") || line.starts_with("tsdevice:") {
                saw_uri = true;
                on_qr(line.to_string());
            } else if line.to_lowercase().contains("associated with") {
                // signal-cli persists the account BEFORE printing this and
                // exiting 0 — wait for clean exit so the store is flushed.
                let num = extract_phone(line).unwrap_or_else(|| "linked".into());
                let _ = child.wait();
                on_linked(num);
                return;
            }
        }
    }

    // stdout closed -> process exiting. Inspect exit code + captured stderr.
    let status = child.wait();
    let err = stderr_buf.lock().map(|b| b.clone()).unwrap_or_default();
    if err.to_lowercase().contains("associated with") {
        let num = extract_phone(&err).unwrap_or_else(|| "linked".into());
        on_linked(num);
        return;
    }
    if let Ok(s) = status {
        if s.success() && saw_uri {
            // exited cleanly after showing the URI = linked (number not parsed)
            on_linked("linked".into());
        }
    }
}

fn extract_json_str(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let i = line.find(&pat)?;
    let rest = &line[i + pat.len()..];
    let colon = rest.find(':')?;
    let after = &rest[colon + 1..];
    let q1 = after.find('"')?;
    let after2 = &after[q1 + 1..];
    let q2 = after2.find('"')?;
    let v = &after2[..q2];
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

fn extract_phone(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    if let Some(plus) = line.find('+') {
        let mut end = plus + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end - plus >= 7 {
            return Some(line[plus..end].to_string());
        }
    }
    None
}
