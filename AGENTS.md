# Signal · WhatsApp Hub — Agent / Integration API

**For autonomous agents and other programs that want to send & read Signal +
WhatsApp messages.** This Hub owns the single Signal (`signal-cli`) and
WhatsApp (`whatsmeow`) session for one phone number. **Do not run your own
`signal-cli` / `whatsapp-cli` receivers** — you will fight the Hub for the
single companion session and inbound messages will break for everyone. Route
everything through the Hub instead.

- **App version:** 0.2.0
- **Base URL:** `http://127.0.0.1:8769` (localhost only — never exposed off-box, no auth)
- **Inbound log:** `~/.local/share/signal-whatsapp-hub/messages.jsonl` (append-only, one JSON object per line)

---

## TL;DR for an agent

```text
1. Check the Hub is up:        GET  http://127.0.0.1:8769/health
2. Send a message:             POST http://127.0.0.1:8769/send
3. Read a conversation:        GET  http://127.0.0.1:8769/messages?platform=signal&peer=<peer>&limit=50
4. List conversations:         GET  http://127.0.0.1:8769/conversations?platform=signal
5. React to inbound (live):    tail ~/.local/share/signal-whatsapp-hub/messages.jsonl  (filter direction=="in")
```

If `GET /health` fails to connect, the Hub is not running — **fall back to your
own sending only if you must, and stop the moment the Hub comes back.**

---

## Endpoints

### `GET /health`
Liveness + per-platform link status. Use this before sending.
```json
{ "ok": true, "signal": true, "whatsapp": true }
```
- `signal` / `whatsapp` = whether that platform is currently linked & reachable.
- A reachable Hub with an unlinked platform returns `false` for that platform —
  don't send to it.

### `POST /send`
Send a message **out** through the Hub. The Hub does the actual delivery, stores
it, tags it, and shows it live in the desktop UI.

**Request body:**
```json
{
  "platform":  "signal",            // "signal" | "whatsapp"   (required)
  "recipient": "+15551234567",      // required — see "Recipient format" below
  "text":      "your message",      // required, non-empty
  "source":    "ai"                 // optional; defaults to "ai" for API callers
}
```

**Success (HTTP 200):**
```json
{ "ok": true, "id": "1781353639274-60d506", "ts": 1781353639.274 }
```

**Failure:**
- `400` `{ "ok": false, "error": "platform, recipient and text are required" }`
- `400` `{ "ok": false, "error": "bad json: ..." }`
- `502` `{ "ok": false, "error": "<delivery error from the CLI>" }`

**`source` controls how the message is displayed in the desktop UI:**
| `source`  | rendered as |
|-----------|-------------|
| `ai`      | purple bubble with 🤖 (automated / AI reply) — **default for API callers** |
| `manual`  | blue bubble (a human operator) |
| `system`  | system / log line |

> Pick `ai` if your agent generated the text. Use `manual` only if you are
> relaying something a human typed.

### `GET /messages?platform=&peer=&limit=`
Returns an array of stored messages (oldest → newest).

| param | required | meaning |
|-------|----------|---------|
| `platform` | yes | `signal` or `whatsapp` |
| `peer` | no | filter to one conversation (the other party's id) |
| `limit` | no | max messages to return (default `200`) |

```
GET /messages?platform=signal&peer=+15551234567&limit=50
```
```json
[
  { "id":"1781..-ab12","platform":"signal","direction":"in","source":"human",
    "peer":"+15551234567","peer_name":"Alex","text":"hi","ts":1734462003.9 }
]
```

### `GET /conversations?platform=`
Distinct conversations (most-recent first) with the last message + unread count.
```json
[
  { "peer":"+15551234567","peer_name":"Alex","last_text":"hi",
    "last_ts":1734462003.9,"unread":0 }
]
```

---

## Data model

### `Message`
```jsonc
{
  "id":        "string",            // "<ms-timestamp>-<rand>", unique
  "platform":  "signal|whatsapp",
  "direction": "in|out",            // "in" = received, "out" = sent by the Hub
  "source":    "human|manual|ai|system",
  "peer":      "string",            // contact id of the OTHER party
  "peer_name": "string",            // display name if known (may equal peer)
  "text":      "string",
  "ts":        1734462003.9         // unix seconds (float)
}
```
- Inbound messages are written with `direction:"in"` and `source:"human"`.
- Messages you send via `POST /send` come back with `direction:"out"` and the
  `source` you supplied (or `ai`).

### `Conversation`
```jsonc
{ "peer":"string", "peer_name":"string", "last_text":"string",
  "last_ts":1734462003.9, "unread":0 }
```

### Recipient format
- **Signal:** always `+E.164` (e.g. `+15551234567`).
- **WhatsApp:** digits or `+E.164` (e.g. `15551234567` or `+15551234567`);
  group JIDs are also accepted as the `peer` you saw on inbound.
- The safest rule: **reply using the exact `peer` value you received on inbound.**

---

## Reacting to inbound messages (two ways)

The Hub is **push-based** — it streams from both platforms in real time and
writes every message to disk immediately. You have two ways to consume inbound:

### A) Tail the JSONL log (recommended — lowest latency, no polling)
Every message (in and out) is appended as one JSON object per line to:
```
~/.local/share/signal-whatsapp-hub/messages.jsonl
```
Tail it and act on records where `direction == "in"`.

```python
import json, time, os
PATH = os.path.expanduser("~/.local/share/signal-whatsapp-hub/messages.jsonl")

def follow_inbound(on_msg):
    with open(PATH) as f:
        f.seek(0, os.SEEK_END)          # start at the end (only new messages)
        while True:
            line = f.readline()
            if not line:
                time.sleep(0.5); continue
            try:
                m = json.loads(line)
            except ValueError:
                continue
            if m.get("direction") == "in":
                on_msg(m)

follow_inbound(lambda m: print(f"[{m['platform']}] {m['peer_name']}: {m['text']}"))
```

### B) Poll `GET /messages`
Simpler but higher latency. Track the last `id`/`ts` you've seen and ask for
recent messages on an interval. Use only if you can't tail the file.

---

## Copy-paste send examples

### curl
```bash
curl -s -X POST http://127.0.0.1:8769/send \
  -H 'Content-Type: application/json' \
  -d '{"platform":"signal","recipient":"+15551234567","text":"hello from an agent","source":"ai"}'
```

### Python (`requests`)
```python
import requests

def hub_send(platform, recipient, text, source="ai"):
    r = requests.post("http://127.0.0.1:8769/send",
        json={"platform": platform, "recipient": recipient,
              "text": text, "source": source},
        timeout=30)
    data = r.json()
    if not data.get("ok"):
        raise RuntimeError(data.get("error", "send failed"))
    return data            # {"ok":True,"id":...,"ts":...}

def hub_up():
    try:
        return requests.get("http://127.0.0.1:8769/health", timeout=5).json().get("ok", False)
    except requests.RequestException:
        return False
```

### Node / TypeScript (`fetch`)
```ts
async function hubSend(platform: "signal" | "whatsapp", recipient: string,
                       text: string, source = "ai") {
  const res = await fetch("http://127.0.0.1:8769/send", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ platform, recipient, text, source }),
  });
  const data = await res.json();
  if (!data.ok) throw new Error(data.error ?? "send failed");
  return data;
}
```

### Shell helper (read latest inbound)
```bash
# last 5 inbound messages on Signal
curl -s 'http://127.0.0.1:8769/messages?platform=signal&limit=200' \
  | jq '[.[] | select(.direction=="in")] | .[-5:]'
```

---

## Rules of engagement (important for agents)

1. **Never start your own Signal/WhatsApp receiver.** The Hub owns the single
   companion session. Two receivers = broken inbound for both. Always check
   `GET /health` and route through the Hub.
2. **Be idempotent on inbound.** When tailing the JSONL, de-dupe on `id` — a
   reconnect can re-emit a message you've already handled.
3. **Tag honestly with `source`.** Use `ai` for generated text so a human can
   tell at a glance what was automated.
4. **Reply with the exact `peer` you received.** Don't reformat the number.
5. **Handle the Hub being down.** `health` connection-refused = Hub offline.
   Degrade gracefully; resume routing through the Hub when it returns.
6. **Rate-limit yourself.** There's no server-side throttle. Don't blast — you'll
   trip Signal/WhatsApp spam protection on the underlying account.
7. **Localhost only.** The API has no authentication because it never leaves
   `127.0.0.1`. Do not proxy it to a public interface.

---

## Why route through the Hub?

`signal-cli` locks its account DB and `whatsmeow` allows exactly one WhatsApp
session. If two programs run receivers/senders against the same number they
fight, and inbound messages get lost. With the Hub as the single owner:

- one receiver per platform (no contention),
- every system's outbound goes out reliably and is logged/tagged uniformly,
- the desktop UI shows everything (manual, AI, and inbound) in real time.

---

## Port

| Service | Port |
|---------|------|
| **Signal · WhatsApp Hub integration API** | **8769** (localhost) |

For architecture and the build/run runbook see [`README.md`](README.md).
