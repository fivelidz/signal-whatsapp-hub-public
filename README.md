# Signal · WhatsApp Hub

> **v0.2.0** — a single, lightweight desktop app (Tauri 2, all-Rust, ~4 MB binary)
> that unifies **Signal** and **WhatsApp** for one phone number into one
> control center, and exposes a tiny localhost HTTP API so bots and agents can
> send and read messages through it.

It drives two existing CLIs directly — no Python runtime, no extra services:

- **WhatsApp** → [`whatsmeow`](https://github.com/tulir/whatsmeow)-based `whatsapp-cli` (Go)
- **Signal**   → [`signal-cli`](https://github.com/AsamK/signal-cli) (Java 25+ for 0.14.x)

The app links itself as a **companion/secondary device** to your real phone on
both platforms, so it never holds your primary registration.

---

## Why this exists

`signal-cli` locks its account database and `whatsmeow` allows exactly **one**
WhatsApp session per number. If two programs run their own receivers against the
same account they fight, and inbound messages get lost for both. This Hub is the
**single owner** of those sessions. Everything else (your AI agent, a chatbot, a
notifier) talks to the Hub's HTTP API instead of spawning its own CLI — so there
is no contention, outbound is logged uniformly, and the desktop UI shows
everything live.

## Features

- **Tabs for both platforms** — Signal and WhatsApp side by side.
- **QR linking** — link this app as a companion device from either tab. QR codes
  are generated natively in Rust and refresh automatically when they expire.
- **Manual + programmatic sending** — type in the UI, or `POST /send`.
- **AI vs human visual distinction** — every message is colour-coded by `source`:
  | colour | meaning |
  |--------|---------|
  | grey, left  | **received** from a contact (`human`) |
  | blue, right | **sent manually** by you (`manual`) |
  | purple 🤖, right | **sent by an agent/bot** (`ai`) |
- **Live inbound (push, not polling)** — a persistent `signal-cli` daemon and a
  streaming `whatsapp-cli receive` push new messages into the UI instantly.
- **Persistent history** — messages are line-delimited JSON at
  `~/.local/share/signal-whatsapp-hub/messages.jsonl`; reloaded on startup.
- **Localhost integration API** on `127.0.0.1:8769` — see **[AGENTS.md](AGENTS.md)**.
- **Low idle cost** — one warm Signal JVM asleep on a socket (~0% CPU when idle).

---

## Prerequisites

You provide the two CLI binaries and a JDK; the Hub orchestrates them.

| Dependency | Notes |
|------------|-------|
| **Rust** + [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) | to build the app |
| **`signal-cli`** ≥ 0.14.5 | [releases](https://github.com/AsamK/signal-cli/releases). 0.14.x needs **Java 25**. |
| **Java 25** (OpenJDK) | required by signal-cli 0.14.x (libsignal-client 0.87.x) |
| **`whatsapp-cli`** | a whatsmeow-based CLI exposing `link`, `send`, `receive`, `status` (see "WhatsApp CLI contract" below) |

> **Why Java 25?** Around mid-2026 the Signal server stopped sending `serverGuid`
> on sealed-sender envelopes; older `libsignal-service-java` (bundled with
> signal-cli ≤ 0.13.x) threw on every inbound message and silently dropped them.
> signal-cli **0.14.5+** fixes this — and it requires Java 25. If Signal *sends*
> but never *receives*, you're almost certainly on an old signal-cli.

---

## Configuration

Everything is environment-driven — nothing is hardcoded to a machine or user.
Set these before launching (shell profile, a `.env`, or your service unit):

| env var | default | meaning |
|---------|---------|---------|
| `HUB_WA_CLI` | `whatsapp-cli` (PATH) | path to the whatsapp-cli binary |
| `HUB_WA_AUTH` | `~/.local/share/whatsapp-hub/auth` | WhatsApp session dir (`whatsapp.db`) |
| `HUB_WA_ACCOUNT` | *(empty)* | your WhatsApp number, `+E.164` |
| `HUB_SIGNAL_CLI` | `signal-cli` (PATH) | path to the signal-cli launcher |
| `HUB_SIGNAL_CONFIG` | `~/.local/share/signal-cli` | signal-cli `--config` dir |
| `HUB_SIGNAL_ACCOUNT` | *(empty)* | your Signal number, `+E.164` |
| `HUB_JAVA_HOME` | autodetected | `JAVA_HOME` for signal-cli (point at Java 25) |
| `HUB_IMPORT_CSV` | *(unset)* | optional CSV of prior history to import once |

Example:

```bash
export HUB_SIGNAL_CLI="$HOME/tools/signal-cli-0.14.5/bin/signal-cli"
export HUB_JAVA_HOME="/usr/lib/jvm/java-25-openjdk"
export HUB_SIGNAL_ACCOUNT="+15551234567"
export HUB_WA_CLI="$HOME/tools/whatsapp-cli/whatsapp-cli"
export HUB_WA_ACCOUNT="+15551234567"
```

---

## Build & run

```bash
# dev (hot-reload the webview)
cargo tauri dev

# release binary
cargo build --release --manifest-path src-tauri/Cargo.toml
./src-tauri/target/release/signal-whatsapp-hub
```

> `Cargo.lock` is committed (this is an application — reproducible builds). It
> pins `brotli 8.0.3`; don't `cargo update` it blindly.

Then **link your devices**: open the app, click *Link / QR* in each tab, and scan
the code from Signal/WhatsApp ▸ *Settings ▸ Linked devices* on your phone.

---

## Architecture

```
Tauri webview (src/index.html, vanilla JS)
   │  invoke() / listen()         localhost HTTP :8769
   ▼                                   ▲
Rust core (src-tauri/src/)              │
   ├─ lib.rs           Tauri commands, state, receivers, entry point
   ├─ bridge.rs        shells out to whatsapp-cli + signal-cli (QR, send, status, WA receive)
   ├─ signal_daemon.rs persistent signal-cli daemon (JSON-RPC over a unix socket)
   ├─ store.rs         message store (jsonl), conversations, stats, CSV import
   ├─ api.rs           the localhost integration API  ◄── AGENTS.md
   └─ config.rs        all paths/accounts/JAVA_HOME, env-driven
```

**Inbound is push-based.** Signal: one warm daemon JVM, `subscribeReceive` over a
unix socket, streamed forever. WhatsApp: `whatsapp-cli receive` streaming JSON
lines on stdout. Each message is written to disk, then emitted to the UI. There
is **no message poll loop** — only a 15 s status-dot refresh.

---

## Integration API (for agents & bots)

The Hub exposes `http://127.0.0.1:8769` (localhost only, no auth):

```
GET  /health                                  -> {ok, signal, whatsapp}
POST /send  {platform, recipient, text, source?} -> {ok, id, ts}
GET  /messages?platform=&peer=&limit=         -> [Message, ...]
GET  /conversations?platform=                 -> [Conversation, ...]
```

Inbound can also be tailed from `~/.local/share/signal-whatsapp-hub/messages.jsonl`.

👉 **Full contract, data model, and copy-paste examples (curl / Python / Node):
see [AGENTS.md](AGENTS.md).**

---

## WhatsApp CLI contract

This repo orchestrates a whatsmeow CLI but does not ship one. Any binary that
implements these subcommands works (`HUB_WA_CLI` points at it):

| subcommand | behaviour |
|------------|-----------|
| `link`     | prints a QR/login URI to stdout for pairing |
| `send <recipient> <text>` | sends a message |
| `receive`  | streams inbound messages as one JSON object per line on stdout |
| `status`   | prints link/connection status (JSON) |

The expected inbound JSON shape is normalized in `bridge.rs::norm_wa`. Sending
`PresenceAvailable` on connect (and periodically) keeps the linked device
"online" so WhatsApp pushes messages live instead of batching them.

---

## Security notes

- The HTTP API has **no authentication** because it binds to `127.0.0.1` only.
  **Do not** expose it on a public interface or proxy it off-box.
- Session/auth data (`*.db`, `accounts.json`, `auth/`, `data/`) and message
  history (`*.jsonl`, CSVs) are **git-ignored** and must never be committed.
- This project links as a *companion* device; your primary registration stays on
  your phone.

## License

MIT — see [LICENSE](LICENSE).

This project is not affiliated with or endorsed by Signal or WhatsApp. "Signal"
and "WhatsApp" are trademarks of their respective owners.

---

## Security

**Integration API authentication (v0.2.1+).** Every `/api` request now requires
`Authorization: Bearer <token>`. The token is read from `HUB_API_TOKEN`, or
auto-generated on first run and stored (0600) at
`~/.local/share/signal-whatsapp-hub/api-token`:

```bash
TOKEN=$(cat ~/.local/share/signal-whatsapp-hub/api-token)
curl -s -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8769/health
curl -s -X POST -H "Authorization: Bearer $TOKEN" \
     -H "Content-Type: application/json" \
     -d '{"platform":"signal","recipient":"+15551234567","text":"hi"}' \
     http://127.0.0.1:8769/send
```

Other hardening in v0.2.1: `Host` header allow-list (DNS-rebinding defence),
`Content-Type: application/json` enforcement on `POST /send` (JSON-CSRF
defence), request-body and page-size caps, recipient format validation,
`0600`/`0700` permissions on the message store, a committed least-privilege
Tauri capabilities file, a content-security-policy, and the real bundle
identifier. See `SECURITY_REVIEW` methodology and the `redteam/rebind_demo/`
kit — a self-test anyone can run against their own instance.

**Message store:** `~/.local/share/signal-whatsapp-hub/messages.jsonl`,
owner-only permissions, unencrypted. Treat it as sensitive.

## Fleet mode (company forwarding — opt-in, visible)

Run the hub on each fleet device and set:

```bash
HUB_FORWARD_URL=https://concierge.yourcompany.example/api/hub-ingest
HUB_FORWARD_TOKEN=<per-device token from your concierge hub>
```

Every stored message is then POSTed to your endpoint (consolidating work and
sales phones into one concierge stream), and the desktop UI shows a permanent
banner — **forwarding is always visible to the person using the phone**. Enable
it only under a documented policy with the user's consent; the feature has no
covert mode by design.
