//! Signal WhatsApp Hub — unified Signal + WhatsApp control center (Tauri 2, all-Rust).

mod api;
mod bridge;
mod config;
mod signal_daemon;
mod store;

use signal_daemon::SignalDaemon;

use config::Config;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use store::{Conversation, Message, Store};
use tauri::{AppHandle, Emitter, Manager, State};

/// Per-platform runtime flags.
///
/// `linking` is true while a login/link CLI is running for that platform — the
/// receiver loop MUST pause then, because both whatsmeow (WhatsApp) and
/// signal-cli only allow a SINGLE session against the store at a time. Running
/// `receive` and `login`/`link` concurrently is exactly what broke QR linking.
#[derive(Default)]
pub struct PlatformFlags {
    linking: AtomicBool,
    receiver_spawned: AtomicBool,
    /// set true just before a send so the receiver yields the signal-cli DB
    /// lock (signal-cli is single-process-per-account).
    send_pending: AtomicBool,
}

/// Latest QR data-URI + link status per platform, so the frontend can POLL
/// for it (robust against missed events / listener races).
#[derive(Default)]
pub struct QrState {
    wa_qr: std::sync::Mutex<Option<String>>,
    wa_linked: std::sync::Mutex<Option<String>>,
    sig_qr: std::sync::Mutex<Option<String>>,
    sig_linked: std::sync::Mutex<Option<String>>,
}

/// Shared app state.
pub struct AppState {
    cfg: Config,
    store: Arc<Store>,
    wa: Arc<PlatformFlags>,
    sig: Arc<PlatformFlags>,
    qr: Arc<QrState>,
    /// One warm signal-cli daemon JVM (low energy) shared everywhere.
    signal: Arc<SignalDaemon>,
}

fn log(msg: &str) {
    eprintln!("[hub] {msg}");
}

// ─────────────────────────── Commands ───────────────────────────────────

#[tauri::command]
fn get_config(state: State<AppState>) -> serde_json::Value {
    serde_json::json!({
        "whatsapp_account": state.cfg.whatsapp_account,
        "signal_account": state.cfg.signal_account,
        "whatsapp_cli": state.cfg.whatsapp_cli.to_string_lossy(),
        "signal_cli": state.cfg.signal_cli.to_string_lossy(),
        "whatsapp_cli_present": state.cfg.whatsapp_cli.exists(),
        "signal_cli_present": state.cfg.signal_cli.exists(),
        "java_home": state.cfg.java_home.to_string_lossy(),
        "java_home_present": state.cfg.java_home.exists(),
    })
}

#[tauri::command]
fn get_status(state: State<AppState>) -> serde_json::Value {
    let wa = bridge::wa_status(&state.cfg);
    let sig = bridge::signal_status(&state.cfg);
    serde_json::json!({ "whatsapp": wa, "signal": sig })
}

#[tauri::command]
fn list_conversations(state: State<AppState>, platform: String) -> Vec<Conversation> {
    state.store.conversations(&platform)
}

#[tauri::command]
fn get_stats(state: State<AppState>, platform: String) -> store::PlatformStats {
    state.store.stats(&platform)
}

#[tauri::command]
fn list_messages(
    state: State<AppState>,
    platform: String,
    peer: Option<String>,
) -> Vec<Message> {
    state.store.list(&platform, peer.as_deref(), 500)
}

/// Send a message. `source` is "manual" (operator) or "ai".
/// Shared send logic used by BOTH the Tauri command and the HTTP integration
/// API. Sends the message, stores it (tagged with `source`), and returns the
/// stored Message. Caller is responsible for emitting the UI event if it has
/// an AppHandle.
pub fn do_send(
    cfg: &Config,
    store: &Arc<Store>,
    signal: &Arc<SignalDaemon>,
    platform: &str,
    recipient: &str,
    text: &str,
    source: &str,
) -> Result<Message, String> {
    log(&format!("send {platform} -> {recipient} ({source})"));
    let result = match platform {
        "whatsapp" => bridge::wa_send(cfg, recipient, text),
        // Signal sends go over the warm daemon socket — no JVM spawn, no
        // DB-lock dance with the receiver.
        "signal" => signal.send(recipient, text),
        _ => Err(format!("unknown platform {platform}")),
    };
    if let Err(e) = &result {
        log(&format!("send FAILED: {e}"));
    }
    result?;
    let m = Message::new(platform, "out", source, recipient, "", text);
    Ok(store.add(m))
}

#[tauri::command]
fn send_message(
    app: AppHandle,
    state: State<AppState>,
    platform: String,
    recipient: String,
    text: String,
    source: Option<String>,
) -> Result<Message, String> {
    let src = source.unwrap_or_else(|| "manual".into());
    let stored = do_send(
        &state.cfg, &state.store, &state.signal, &platform, &recipient, &text, &src,
    )?;
    let _ = app.emit("message", &stored);
    Ok(stored)
}

/// Start linking WhatsApp. Pauses the WA receiver first, runs `whatsapp-cli
/// login`, emits "wa_qr" (data-URI) per QR and "wa_linked" on success.
#[tauri::command]
fn start_whatsapp_link(app: AppHandle, state: State<AppState>) {
    let wa = state.wa.clone();
    let cfg = state.cfg.clone();
    let store = state.store.clone();
    let qr = state.qr.clone();
    // clear any stale QR/linked state for a fresh attempt
    *qr.wa_qr.lock().unwrap() = None;
    *qr.wa_linked.lock().unwrap() = None;
    // mark linking -> the receiver loop will see this and stop spawning receive
    wa.linking.store(true, Ordering::SeqCst);
    log("WhatsApp link requested — pausing receiver, launching login");
    std::thread::spawn(move || {
        // give any in-flight receive process a moment to be told to stop
        std::thread::sleep(std::time::Duration::from_millis(400));
        let running = {
            let wa = wa.clone();
            move || wa.linking.load(Ordering::SeqCst)
        };
        bridge::wa_login(
            &cfg,
            |code| {
                match bridge::qr_data_uri(&code) {
                    Ok(uri) => {
                        log(&format!("WhatsApp QR emitted (uri {} bytes)", uri.len()));
                        *qr.wa_qr.lock().unwrap() = Some(uri.clone());
                        let _ = app.emit("wa_qr", uri);
                    }
                    Err(e) => log(&format!("QR render error: {e}")),
                }
            },
            |detail| {
                log(&format!("WhatsApp LINKED: {detail}"));
                *qr.wa_linked.lock().unwrap() = Some(detail.clone());
                let _ = app.emit("wa_linked", detail);
            },
            &running,
        );
        // linking finished (success, cancel, or error) — clear flag & (re)start receiver
        wa.linking.store(false, Ordering::SeqCst);
        ensure_wa_receiver(app, cfg, store, wa);
    });
}

/// Start linking Signal as a secondary device.
#[tauri::command]
fn start_signal_link(app: AppHandle, state: State<AppState>, device_name: Option<String>) {
    let sig = state.sig.clone();
    let cfg = state.cfg.clone();
    let store = state.store.clone();
    let signal = state.signal.clone();
    let qr = state.qr.clone();
    *qr.sig_qr.lock().unwrap() = None;
    *qr.sig_linked.lock().unwrap() = None;
    let name = device_name.unwrap_or_else(|| "Signal WhatsApp Hub".into());
    sig.linking.store(true, Ordering::SeqCst);
    log("Signal link requested — pausing receiver, launching link");
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(400));
        let running = {
            let sig = sig.clone();
            move || sig.linking.load(Ordering::SeqCst)
        };
        bridge::signal_link(
            &cfg,
            &name,
            |code| {
                match bridge::qr_data_uri(&code) {
                    Ok(uri) => {
                        log(&format!("Signal QR emitted (uri {} bytes)", uri.len()));
                        *qr.sig_qr.lock().unwrap() = Some(uri.clone());
                        let _ = app.emit("signal_qr", uri);
                    }
                    Err(e) => log(&format!("QR render error: {e}")),
                }
            },
            |detail| {
                log(&format!("Signal LINKED: {detail}"));
                *qr.sig_linked.lock().unwrap() = Some(detail.clone());
                let _ = app.emit("signal_linked", detail);
            },
            &running,
        );
        sig.linking.store(false, Ordering::SeqCst);
        ensure_sig_receiver(app, store, sig, signal);
    });
}

/// Poll for the latest QR / linked status (robust fallback to events).
#[tauri::command]
fn get_link_state(state: State<AppState>, platform: String) -> serde_json::Value {
    let (qr, linked) = match platform.as_str() {
        "whatsapp" => (
            state.qr.wa_qr.lock().unwrap().clone(),
            state.qr.wa_linked.lock().unwrap().clone(),
        ),
        "signal" => (
            state.qr.sig_qr.lock().unwrap().clone(),
            state.qr.sig_linked.lock().unwrap().clone(),
        ),
        _ => (None, None),
    };
    serde_json::json!({ "qr": qr, "linked": linked })
}

#[tauri::command]
fn cancel_links(state: State<AppState>) {
    log("cancel links");
    state.wa.linking.store(false, Ordering::SeqCst);
    state.sig.linking.store(false, Ordering::SeqCst);
}

/// Kick off receivers for whichever platforms are already linked.
#[tauri::command]
fn start_receivers(app: AppHandle, state: State<AppState>) {
    spawn_linked_receivers(
        app,
        state.cfg.clone(),
        state.store.clone(),
        state.wa.clone(),
        state.sig.clone(),
        state.signal.clone(),
    );
}

// ─────────────────────────── Receivers ──────────────────────────────────

/// Only start a receiver for a platform that reports linked/registered, so we
/// don't tight-loop `receive` against an unlinked store (which contends with
/// the login/link flow).
fn spawn_linked_receivers(
    app: AppHandle,
    cfg: Config,
    store: Arc<Store>,
    wa: Arc<PlatformFlags>,
    sig: Arc<PlatformFlags>,
    signal: Arc<SignalDaemon>,
) {
    // probe status off-thread so we never block the UI
    std::thread::spawn(move || {
        let wa_ok = bridge::wa_status(&cfg).ok;
        let sig_ok = bridge::signal_status(&cfg).ok;
        log(&format!("startup status — whatsapp:{wa_ok} signal:{sig_ok}"));
        if wa_ok {
            ensure_wa_receiver(app.clone(), cfg.clone(), store.clone(), wa);
        }
        if sig_ok {
            ensure_sig_receiver(app, store, sig, signal);
        }
    });
}

fn ensure_wa_receiver(app: AppHandle, cfg: Config, store: Arc<Store>, wa: Arc<PlatformFlags>) {
    if wa.receiver_spawned.swap(true, Ordering::SeqCst) {
        return; // already running
    }
    log("starting WhatsApp receiver loop");
    std::thread::spawn(move || loop {
        // pause while a link is in progress (single-session constraint)
        if wa.linking.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }
        let not_linking = {
            let wa = wa.clone();
            move || !wa.linking.load(Ordering::SeqCst)
        };
        bridge::wa_receive_loop(
            &cfg,
            |inb| {
                log(&format!("WA inbound from {} ({}): {}", inb.peer, inb.peer_name, inb.text.chars().take(40).collect::<String>()));
                let m = Message::new(
                    &inb.platform, "in", "human", &inb.peer, &inb.peer_name, &inb.text,
                );
                let stored = store.add(m);
                let _ = app.emit("message", &stored);
            },
            &not_linking,
        );
        std::thread::sleep(std::time::Duration::from_secs(3));
    });
}

fn ensure_sig_receiver(
    app: AppHandle,
    store: Arc<Store>,
    sig: Arc<PlatformFlags>,
    signal: Arc<SignalDaemon>,
) {
    if sig.receiver_spawned.swap(true, Ordering::SeqCst) {
        return;
    }
    log("starting Signal receiver (warm daemon, streaming)");
    std::thread::spawn(move || {
        // One persistent connection to the warm daemon JVM. Streams inbound
        // forever; pauses only while a (re)link is happening.
        let not_linking = {
            let sig = sig.clone();
            move || !sig.linking.load(Ordering::SeqCst)
        };
        signal.receive_forever(
            |inb| {
                log(&format!(
                    "Signal inbound from {} ({}): {}",
                    inb.peer,
                    inb.peer_name,
                    inb.text.chars().take(40).collect::<String>()
                ));
                let m = Message::new(
                    &inb.platform, "in", "human", &inb.peer, &inb.peer_name, &inb.text,
                );
                let stored = store.add(m);
                let _ = app.emit("message", &stored);
            },
            &not_linking,
        );
    });
}

// ─────────────────────────── Entry point ────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let cfg = Config::load();
    log(&format!(
        "boot — wa_cli:{} sig_cli:{} java:{}",
        cfg.whatsapp_cli.exists(),
        cfg.signal_cli.exists(),
        cfg.java_home.exists()
    ));
    let store = Arc::new(Store::new());
    // Optional one-time import of pre-existing chat history from a CSV. Point
    // HUB_IMPORT_CSV at a file to seed conversations on first launch; the import
    // is idempotent (guarded by a marker file). See store::import_legacy_csv for
    // the expected row format.
    if let Ok(csv) = std::env::var("HUB_IMPORT_CSV") {
        let path = std::path::PathBuf::from(csv);
        let n = store.import_legacy_csv(&path);
        if n > 0 {
            log(&format!("imported {n} messages from {}", path.display()));
        }
    }
    let signal = SignalDaemon::new(cfg.clone());
    let state = AppState {
        cfg,
        store,
        wa: Arc::new(PlatformFlags::default()),
        sig: Arc::new(PlatformFlags::default()),
        qr: Arc::new(QrState::default()),
        signal,
    };

    tauri::Builder::default()
        .manage(state)
        .setup(|app| {
            let handle = app.handle().clone();
            let st = app.state::<AppState>();
            spawn_linked_receivers(
                handle.clone(),
                st.cfg.clone(),
                st.store.clone(),
                st.wa.clone(),
                st.sig.clone(),
                st.signal.clone(),
            );
            // Start the localhost integration API so other programs (agents,
            // bots, automation) can route sends through the Hub and read messages.
            api::spawn(api::ApiCtx {
                app: handle,
                cfg: st.cfg.clone(),
                store: st.store.clone(),
                signal: st.signal.clone(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_status,
            get_stats,
            list_conversations,
            list_messages,
            send_message,
            start_whatsapp_link,
            start_signal_link,
            get_link_state,
            cancel_links,
            start_receivers,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Signal WhatsApp Hub");
}
