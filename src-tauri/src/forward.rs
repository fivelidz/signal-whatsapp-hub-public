//! Consent-based company forwarding ("fleet mode").
//!
//! Company deployments (work/sales phones managed under a documented policy)
//! set HUB_FORWARD_URL (+ HUB_FORWARD_TOKEN) and every stored message is
//! POSTed as JSON to that endpoint — e.g. the concierge hub, so all fleet
//! communications consolidate in one place.
//!
//! This is deliberately VISIBLE: the desktop UI shows a persistent banner
//! ("Company message forwarding enabled → <endpoint>") whenever forwarding is
//! active, and /health reports it. There is no covert mode by design.
//!
//! Transport is curl (fire-and-forget, never blocks message flow) to keep the
//! dependency tree at zero.

use crate::store::Message;
use std::io::Write;

pub fn enabled() -> Option<(String, Option<String>)> {
    let url = std::env::var("HUB_FORWARD_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())?;
    let token = std::env::var("HUB_FORWARD_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty());
    Some((url, token))
}

pub fn forward(m: &Message) {
    let Some((url, token)) = enabled() else {
        return;
    };
    let Ok(body) = serde_json::to_string(m) else {
        return;
    };
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-sS",
        "-X",
        "POST",
        "--max-time",
        "10",
        "-H",
        "Content-Type: application/json",
    ]);
    if let Some(t) = &token {
        cmd.args(["-H", &format!("Authorization: Bearer {t}")]);
    }
    cmd.args(["--data-binary", "@-", &url])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Ok(mut child) = cmd.spawn() {
        if let Some(mut si) = child.stdin.take() {
            let _ = si.write_all(body.as_bytes());
        }
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
