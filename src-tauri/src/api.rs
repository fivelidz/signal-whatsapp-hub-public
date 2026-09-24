//! Local HTTP integration API for Signal WhatsApp Hub.
//!
//! Other programs (agents, bots, automation) route their outbound messages
//! THROUGH the Hub instead of fighting it for the single-session signal-cli /
//! whatsapp-cli. They also read inbound either from this API or from the
//! `messages.jsonl` the Hub already writes. See AGENTS.md for the full contract.
//!
//! Bound to 127.0.0.1:8769 (localhost only — never exposed off-box).
//!
//! Endpoints:
//!   GET  /health                      -> {"ok":true,"signal":bool,"whatsapp":bool}
//!   POST /send  {platform,recipient,text,source?}
//!                                      -> {"ok":true,"id":..,"ts":..} | {"ok":false,"error":..}
//!   GET  /messages?platform=&peer=&limit=
//!                                      -> [Message,...]
//!   GET  /conversations?platform=     -> [Conversation,...]

use crate::config::Config;
use crate::do_send;
use crate::signal_daemon::SignalDaemon;
use crate::store::Store;
use std::io::Read;
use std::sync::Arc;
use tauri::{AppHandle, Emitter};
use tiny_http::{Header, Method, Response, Server};

pub const API_ADDR: &str = "127.0.0.1:8769";

#[derive(Clone)]
pub struct ApiCtx {
    pub app: AppHandle,
    /// H1 fix: bearer token required on every endpoint.
    pub token: String,
    pub cfg: Config,
    pub store: Arc<Store>,
    pub signal: Arc<SignalDaemon>,
}

/// Spawn the HTTP server on its own thread. Never blocks the app; if the port
/// is taken it logs and gives up (the app still works without the API).
pub fn spawn(ctx: ApiCtx) {
    std::thread::spawn(move || {
        let server = match Server::http(API_ADDR) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[hub] integration API could not bind {API_ADDR}: {e}");
                return;
            }
        };
        eprintln!("[hub] integration API listening on http://{API_ADDR}");
        for request in server.incoming_requests() {
            handle(&ctx, request);
        }
    });
}

fn json_response(code: u16, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    Response::from_string(body)
        .with_status_code(code)
        .with_header(header)
}

fn handle(ctx: &ApiCtx, mut request: tiny_http::Request) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let query = url.splitn(2, '?').nth(1).unwrap_or("").to_string();

    // H2 fix: DNS-rebinding/CSRF defence — a browser hitting us via a rebound
    // hostname sends a Host header that isn't ours. Reject anything but our
    // own bind address forms.
    if !host_allowed(&request) {
        let _ = request.respond(json_response(403, r#"{"ok":false,"error":"bad host"}"#.into()));
        return;
    }
    // H1 fix: bearer token on every endpoint (HUB_API_TOKEN, or the
    // auto-generated token at ~/.local/share/signal-whatsapp-hub/api-token).
    if !authorized(ctx, &request) {
        let _ = request.respond(json_response(
            401,
            r#"{"ok":false,"error":"unauthorized — send Authorization: Bearer <token> (see AGENTS.md)"}"#.into(),
        ));
        return;
    }

    let resp = match (&method, path.as_str()) {
        (Method::Get, "/health") => {
            let sig = crate::bridge::signal_status(&ctx.cfg).ok;
            let wa = crate::bridge::wa_status(&ctx.cfg).ok;
            json_response(
                200,
                serde_json::json!({"ok": true, "signal": sig, "whatsapp": wa,
                    "forwarding": crate::forward::enabled().map(|(u, _)| u)}).to_string(),
            )
        }
        (Method::Post, "/send") => {
            // L3 fix: cap request body (memory-DoS via unbounded read)
            let ctype = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Content-Type"))
                .map(|h| h.value.as_str().to_string())
                .unwrap_or_default();
            let mut body = String::new();
            let _ = request.as_reader().take(65_536).read_to_string(&mut body);
            handle_send(ctx, &body, &ctype)
        }
        (Method::Get, "/messages") => {
            let params = parse_query(&query);
            let platform = params.get("platform").cloned().unwrap_or_default();
            let peer = params.get("peer").cloned();
            let limit = params
                .get("limit")
                .and_then(|s| s.parse::<usize>().ok())
                .map(|n| n.min(1000)) // L3 fix: cap page size
                .unwrap_or(200);
            let msgs = ctx.store.list(&platform, peer.as_deref(), limit);
            json_response(200, serde_json::to_string(&msgs).unwrap_or("[]".into()))
        }
        (Method::Get, "/conversations") => {
            let params = parse_query(&query);
            let platform = params.get("platform").cloned().unwrap_or_default();
            let convs = ctx.store.conversations(&platform);
            json_response(200, serde_json::to_string(&convs).unwrap_or("[]".into()))
        }
        _ => json_response(404, r#"{"ok":false,"error":"not found"}"#.into()),
    };

    let _ = request.respond(resp);
}

/// M1 fix: browsers cannot forge this content-type cross-origin without a
/// preflight we would never satisfy — kills the text/plain form-POST trick.
fn handle_send(ctx: &ApiCtx, body: &str, ctype: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    if !ctype.to_ascii_lowercase().contains("application/json") {
        return json_response(
            415,
            r#"{"ok":false,"error":"Content-Type must be application/json"}"#.into(),
        );
    }
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            return json_response(
                400,
                serde_json::json!({"ok": false, "error": format!("bad json: {e}")}).to_string(),
            )
        }
    };
    let platform = v.get("platform").and_then(|x| x.as_str()).unwrap_or("");
    let recipient = v.get("recipient").and_then(|x| x.as_str()).unwrap_or("");
    let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
    // external callers default to "ai" — they're automated systems.
    let source = v.get("source").and_then(|x| x.as_str()).unwrap_or("ai");

    if platform.is_empty() || recipient.is_empty() || text.is_empty() {
        return json_response(
            400,
            r#"{"ok":false,"error":"platform, recipient and text are required"}"#.into(),
        );
    }
    // L4 fix: never let a recipient masquerade as a CLI flag
    if !valid_recipient(recipient) {
        return json_response(
            400,
            r#"{"ok":false,"error":"recipient must be +E.164 or a platform jid"}"#.into(),
        );
    }

    match do_send(
        &ctx.cfg, &ctx.store, &ctx.signal, platform, recipient, text, source,
    ) {
        Ok(stored) => {
            // push to the desktop UI too, so externally-sent messages appear live.
            let _ = ctx.app.emit("message", &stored);
            json_response(
                200,
                serde_json::json!({"ok": true, "id": stored.id, "ts": stored.ts}).to_string(),
            )
        }
        Err(e) => json_response(
            502,
            serde_json::json!({"ok": false, "error": e}).to_string(),
        ),
    }
}

fn host_allowed(request: &tiny_http::Request) -> bool {
    match request.headers().iter().find(|h| h.field.equiv("Host")) {
        Some(h) => {
            let v = h.value.as_str().to_ascii_lowercase();
            v == "127.0.0.1:8769" || v == "localhost:8769"
        }
        None => false,
    }
}

fn authorized(ctx: &ApiCtx, request: &tiny_http::Request) -> bool {
    let expected = format!("Bearer {}", ctx.token);
    request
        .headers()
        .iter()
        .any(|h| h.field.equiv("Authorization") && h.value.as_str() == expected)
}

fn valid_recipient(r: &str) -> bool {
    if r.starts_with('-') {
        return false;
    }
    match r.strip_prefix('+') {
        Some(digits) => !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()),
        None => r.contains('@'), // platform jid form
    }
}

fn parse_query(q: &str) -> std::collections::HashMap<String, String> {
    let mut m = std::collections::HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("").to_string();
        let val = it.next().unwrap_or("");
        m.insert(k, urldecode(val));
    }
    m
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
