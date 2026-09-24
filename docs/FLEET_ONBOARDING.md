# Fleet onboarding — adding a sales phone to the concierge stream

This is the documented, consent-first way to consolidate messaging from
multiple work/sales phones into the concierge hub. It uses only shipped
features: the hub's integration API (token-authenticated) and fleet mode
(`HUB_FORWARD_URL` / `HUB_FORWARD_TOKEN`).

## Consent requirement

Forwarding must be visible and agreed before it is enabled. Minimum bar:

1. The person uses the phone and sees the **permanent banner** the hub shows
   while forwarding is on ("Company message forwarding enabled → <endpoint>").
2. A one-paragraph policy acknowledging which conversations are in scope
   (work/sales lines), where they go, and who can read them.
3. A signed or emailed "I agree" record kept with the deployment inventory.

Covert forwarding — enabling any of this without the user's knowledge — is
not a supported mode and must not be attempted (interception/surveillance law
applies to communications captured without consent).

## Per-device steps (10 minutes)

1. **Provision the device token** on the concierge hub:
   `concierge: fleet add <person> --device <hostname>` → yields a per-device
   bearer token (store it in the deployment inventory).
2. **Install the hub** on the device and link Signal/WhatsApp to the **work
   numbers** (not personal ones).
3. **Configure fleet mode** — either:
   - in the app: **⚑ Fleet panel** → endpoint + device token → Save → enabled ✓, or
   - headless (`.env` / systemd unit — first-run seed):
     ```bash
     HUB_FORWARD_URL=https://<concierge-endpoint>/api/hub-ingest
     HUB_FORWARD_TOKEN=<device token>
     ```
4. **Restart the hub.** Verify:
   - banner visible in the desktop UI,
   - `curl -H "Authorization: Bearer $(cat api-token)" localhost:8769/health`
     reports `"forwarding":"<endpoint>"`,
   - a test message arrives in the concierge stream.
5. **Record** in the inventory: device, person, work numbers, policy version,
   consent date, token fingerprint (never the token).

## Offboarding

Remove the device from the concierge inventory (token dies), revoke the
`.env` entries, purge the local store (`rm ~/.local/share/signal-whatsapp-hub/messages.jsonl`),
unlink the messaging sessions from the work numbers.

## Security notes

- The forwarder authenticates with a **per-device token** — one compromised
  device can be revoked without touching the fleet.
- Forwarding transport is HTTPS to the concierge endpoint; the hub never
  stores the token anywhere but the local env.
- The hub's own integration API requires its bearer token (v0.2.1+) — fleet
  devices must keep the API token file owner-only.
