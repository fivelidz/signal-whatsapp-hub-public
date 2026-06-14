//! Low-energy Signal backend: ONE long-lived `signal-cli daemon` JVM exposing a
//! JSON-RPC interface over a Unix socket, instead of spawning a fresh JVM every
//! receive cycle.
//!
//! Why: a cold JVM start costs ~40% CPU + 220 MB every ~10 s under the old
//! polling loop. The daemon keeps one warm JVM that streams inbound messages as
//! JSON-RPC notifications (zero polling) and accepts `send` requests on the same
//! socket — so it's both far lower energy AND removes the DB-lock contention
//! that used to fight sends against receives.

use crate::bridge::Inbound;
use crate::config::Config;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Socket path the daemon listens on.
fn socket_path() -> std::path::PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(base).join("signal-cli/socket")
}

/// Clear the signal-cli msg-cache of "poison" envelopes.
///
/// A single undecryptable message cached for retry (NullPointerException on
/// `getServerGuid`/`getSender`) crashes the ENTIRE receive operation every
/// cycle, so NO inbound messages get through until it's removed. signal-cli
/// caches these under <config>/data/<id>.d/msg-cache/. On daemon (re)start we
/// move any cached envelopes aside so a stuck one can never wedge receiving.
/// Backed up (not deleted) so nothing is lost.
fn clear_poison_msg_cache(cfg: &Config) {
    let data_dir = cfg.signal_config_dir.join("data");
    let entries = match std::fs::read_dir(&data_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        // account data dirs look like "<id>.d"
        if p.is_dir() && p.extension().and_then(|e| e.to_str()) == Some("d") {
            let cache = p.join("msg-cache");
            let count = std::fs::read_dir(&cache)
                .map(|d| d.flatten().count())
                .unwrap_or(0);
            if count == 0 {
                continue;
            }
            // back up the whole cache, then clear it
            let backup = cfg.signal_config_dir.join(format!(
                "msg-cache-quarantine-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
            ));
            let _ = std::fs::create_dir_all(&backup);
            if let Ok(files) = std::fs::read_dir(&cache) {
                for f in files.flatten() {
                    let src = f.path();
                    if let Some(name) = src.file_name() {
                        let _ = std::fs::copy(&src, backup.join(name));
                        let _ = std::fs::remove_file(&src);
                    }
                }
            }
            eprintln!(
                "[hub] cleared {count} cached signal envelope(s) (quarantined to {})",
                backup.display()
            );
        }
    }
}

/// A machine-wide spawn lock so two app instances (e.g. one per agent session)
/// can't both spawn a signal-cli daemon for the same account. Implemented with
/// atomic O_EXCL file creation; auto-expires if stale (>30s) so a crash can't
/// wedge it forever. Returns a guard that removes the lock file on drop.
struct SpawnLock {
    path: std::path::PathBuf,
    held: bool,
}
impl Drop for SpawnLock {
    fn drop(&mut self) {
        if self.held {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
fn acquire_spawn_lock() -> SpawnLock {
    let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::PathBuf::from(base).join("signal-cli/swhub-spawn.lock");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    for _ in 0..100 {
        // remove stale lock (older than 30s)
        if let Ok(meta) = std::fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                if modified.elapsed().map(|e| e.as_secs() > 30).unwrap_or(true) {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return SpawnLock { path, held: true },
            Err(_) => std::thread::sleep(Duration::from_millis(300)),
        }
    }
    // gave up waiting — proceed without holding (best effort)
    SpawnLock { path, held: false }
}

/// Quick liveness probe: is there a working signal-cli daemon on this socket?
/// Sends a `version` JSON-RPC and waits briefly for a reply.
fn socket_responds(sock: &std::path::Path) -> bool {
    let mut stream = match UnixStream::connect(sock) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));
    if stream
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"probe\",\"method\":\"version\"}\n")
        .is_err()
    {
        return false;
    }
    let reader = BufReader::new(stream);
    for line in reader.lines().map_while(Result::ok) {
        if line.contains("\"version\"") || line.contains("\"result\"") {
            return true;
        }
    }
    false
}

pub struct SignalDaemon {
    cfg: Config,
    child: Mutex<Option<Child>>,
    /// write half of the JSON-RPC connection used for `send` requests
    req_id: AtomicU64,
}

impl SignalDaemon {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(SignalDaemon {
            cfg,
            child: Mutex::new(None),
            req_id: AtomicU64::new(1),
        })
    }

    /// Start the daemon JVM (idempotent) and return the socket path once ready.
    fn ensure_daemon(&self) -> Option<std::path::PathBuf> {
        let sock = socket_path();
        if !self.cfg.signal_cli.exists() {
            return None;
        }
        // Hold the child lock for the ENTIRE check-and-spawn so two threads
        // (the receiver and a send) can never both spawn a daemon.
        let mut guard = self.child.lock().unwrap();
        if let Some(c) = guard.as_mut() {
            if matches!(c.try_wait(), Ok(None)) && sock.exists() {
                return Some(sock); // already have a live daemon + socket
            }
        }
        // CRITICAL single-account guard: signal-cli allows only ONE process per
        // account. Multiple daemons fighting for one account corrupt the receive
        // websocket (no inbound messages). Two layers of protection:
        //   1) reuse a working daemon already on the socket;
        //   2) a CROSS-PROCESS file lock (flock) so even a second copy of this
        //      app (e.g. launched by another agent) cannot spawn a competitor.
        if sock.exists() && socket_responds(&sock) {
            eprintln!("[hub] reusing existing signal daemon on socket");
            return Some(sock);
        }
        // Acquire the machine-wide spawn lock. Whoever holds it spawns; everyone
        // else waits and then reuses the resulting socket.
        let _spawn_lock = acquire_spawn_lock();
        // Re-check after acquiring the lock — another process may have just
        // started the daemon while we were waiting.
        if sock.exists() && socket_responds(&sock) {
            eprintln!("[hub] reusing signal daemon started by another process");
            return Some(sock);
        }
        // Defensive: kill any stray daemon WE spawned, and a dead socket, before
        // starting a fresh one.
        if let Some(c) = guard.as_mut() {
            let _ = c.kill();
        }
        if let Some(dir) = sock.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::remove_file(&sock);

        // Before starting the daemon, clear any poison envelope that would
        // otherwise crash receive on every cycle (single stuck message blocks
        // ALL inbound). This is the fix for "Signal messages never arrive".
        clear_poison_msg_cache(&self.cfg);

        let mut cmd = Command::new(&self.cfg.signal_cli);
        cmd.args([
            "--log-file",
            "/tmp/swhub-signal-daemon.log",
            "--config",
            &self.cfg.signal_config_dir.to_string_lossy(),
            "-a",
            &self.cfg.signal_account,
            "daemon",
            "--socket",
            &sock.to_string_lossy(),
            "--receive-mode",
            "on-start",
            "--send-read-receipts",
        ]);
        let jh = self.cfg.java_home.to_string_lossy().to_string();
        if self.cfg.java_home.is_dir() {
            cmd.env("JAVA_HOME", &jh);
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}/bin:{}", jh, path));
        }
        // keep the JVM gentle: cap heap so it doesn't balloon. 0.14.5 +
        // libsignal-client 0.87 (Java 25) needs a little more headroom than the
        // old 256m or the receive websocket can OOM/stall under load.
        cmd.env("JAVA_TOOL_OPTIONS", "-Xms48m -Xmx384m -XX:+UseSerialGC");
        cmd.stdout(Stdio::null()).stderr(Stdio::null());

        match cmd.spawn() {
            Ok(child) => {
                eprintln!("[hub] signal daemon started (one warm JVM)");
                *guard = Some(child);
            }
            Err(e) => {
                eprintln!("[hub] failed to start signal daemon: {e}");
                return None;
            }
        }
        drop(guard); // release before the (slow) socket wait
        // wait for the socket to appear (daemon boot)
        for _ in 0..60 {
            if sock.exists() {
                std::thread::sleep(Duration::from_millis(500));
                return Some(sock);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        None
    }

    /// Send a Signal message over the daemon socket (JSON-RPC `send`).
    pub fn send(&self, recipient: &str, message: &str) -> Result<(), String> {
        let sock = self.ensure_daemon().ok_or("signal daemon not available")?;
        let id = self.req_id.fetch_add(1, Ordering::SeqCst);
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "send",
            "params": { "recipient": [recipient], "message": message }
        });
        // dedicated short-lived connection for the request/response
        let mut stream =
            UnixStream::connect(&sock).map_err(|e| format!("connect: {e}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .ok();
        let line = format!("{}\n", req);
        stream
            .write_all(line.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        // read until we get our id's response
        let reader = BufReader::new(stream);
        for line in reader.lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if v.get("id").and_then(|x| x.as_u64()) == Some(id) {
                    if let Some(err) = v.get("error") {
                        return Err(err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("send error")
                            .to_string());
                    }
                    return Ok(());
                }
            }
        }
        Err("no response from daemon".into())
    }

    /// Connect to the daemon socket and stream inbound messages forever,
    /// calling `on_msg` for each. Reconnects if the connection drops. This is
    /// the SINGLE Signal receiver — no per-cycle JVM, no polling.
    pub fn receive_forever<F: FnMut(Inbound)>(&self, mut on_msg: F, running: &dyn Fn() -> bool) {
        loop {
            if !running() {
                return;
            }
            let sock = match self.ensure_daemon() {
                Some(s) => s,
                None => {
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            let mut stream = match UnixStream::connect(&sock) {
                Ok(s) => s,
                Err(_) => {
                    std::thread::sleep(Duration::from_secs(3));
                    continue;
                }
            };
            // CRITICAL: the daemon only PUSHES `receive` notifications to a
            // connection that has called `subscribeReceive`. Without this the
            // socket is silent and no inbound messages ever arrive.
            let sub = b"{\"jsonrpc\":\"2.0\",\"id\":\"hub-sub\",\"method\":\"subscribeReceive\"}\n";
            if stream.write_all(sub).is_err() {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            let _ = stream.flush();
            eprintln!("[hub] signal receiver subscribed (streaming)");
            let reader = BufReader::new(stream);
            for line in reader.lines().map_while(Result::ok) {
                if !running() {
                    return;
                }
                let line = line.trim();
                if !line.starts_with('{') {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    // JSON-RPC notification: {"method":"receive","params":{"envelope":{...}}}
                    if v.get("method").and_then(|m| m.as_str()) == Some("receive") {
                        if let Some(env) = v.get("params").and_then(|p| p.get("envelope")) {
                            if let Some(m) = norm_envelope(env) {
                                on_msg(m);
                            }
                        }
                    }
                }
            }
            // connection dropped — small backoff then reconnect
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// Stop the daemon JVM (on app shutdown).
    #[allow(dead_code)]
    pub fn stop(&self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
        }
    }
}

/// Normalize a daemon `receive` envelope into our Inbound shape.
fn norm_envelope(env: &serde_json::Value) -> Option<Inbound> {
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
