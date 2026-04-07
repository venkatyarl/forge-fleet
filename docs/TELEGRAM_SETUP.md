# Telegram Setup (Taylor Runtime)

This guide wires Telegram into ForgeFleet so `forgefleetd` on **Taylor** can run two-way Telegram messaging (polling mode) and media ingest.

## 1) Create Telegram bot (BotFather)

1. Open Telegram and chat with **@BotFather**.
2. Run `/newbot`.
3. Choose:
   - Bot display name (example: `Taylor ForgeFleet`)
   - Bot username ending in `bot` (example: `taylor_forgefleet_bot`)
4. Copy the bot token BotFather returns.
5. (Recommended) disable privacy mode for richer group behavior:
   - `/setprivacy` → select your bot → `Disable`

## 2) Configure `~/.forgefleet/fleet.toml`

Add (or update) this block:

```toml
[transport.telegram]
enabled = true
bot_token_env = "FORGEFLEET_TELEGRAM_BOT_TOKEN"
allowed_chat_ids = [8496613333, 8622294597]
polling_interval_secs = 2
polling_timeout_secs = 15
media_download_dir = "/Users/venkat/.forgefleet/telegram-media"
```

Notes:
- `allowed_chat_ids` is the safety allowlist.
- `polling_timeout_secs` is Telegram long-poll timeout.
- `polling_interval_secs` is post-poll sleep delay.
- `media_download_dir` enables local media ingest/download.

## 3) Provide bot token securely

Prefer env var over inline token in TOML:

```bash
export FORGEFLEET_TELEGRAM_BOT_TOKEN="<bot-token-from-botfather>"
```

If you run ForgeFleet as a service, set this env var in the service environment (LaunchAgent/systemd), not just an interactive shell.

## 3b) Persist token for daemon/runtime restarts (recommended)

If you only `export` in a shell, Telegram will stop working after process restart.
Set env var in your service manager.

### macOS (LaunchAgent)

In your `~/Library/LaunchAgents/<forgefleet-plist>.plist` add:

```xml
<key>EnvironmentVariables</key>
<dict>
  <key>FORGEFLEET_TELEGRAM_BOT_TOKEN</key>
  <string>YOUR_BOT_TOKEN</string>
</dict>
```

Then reload:

```bash
launchctl unload ~/Library/LaunchAgents/<forgefleet-plist>.plist
launchctl load ~/Library/LaunchAgents/<forgefleet-plist>.plist
```

### Linux (systemd)

Use an env file, e.g. `/etc/forgefleet/forgefleet.env`:

```bash
FORGEFLEET_TELEGRAM_BOT_TOKEN=YOUR_BOT_TOKEN
```

And in your service unit:

```ini
[Service]
EnvironmentFile=/etc/forgefleet/forgefleet.env
```

Then reload/restart:

```bash
sudo systemctl daemon-reload
sudo systemctl restart forgefleet
```

## 4) Create media directory (if using media ingest)

```bash
mkdir -p /Users/venkat/.forgefleet/telegram-media
```

Incoming Telegram attachments are downloaded there when `media_download_dir` is set.

## 5) Start ForgeFleet in polling mode

Telegram polling starts automatically inside `forgefleetd` when:
- `transport.telegram.enabled = true`
- bot token resolves successfully

Start daemon:

```bash
forgefleetd --config ~/.forgefleet/fleet.toml start
```

Startup wiring notes:
- `forgefleetd` launches the Telegram polling subsystem during boot when `transport.telegram.enabled = true`.
- On startup, transport runtime state is written under `transport.telegram.*` in ForgeFleet config state (enabled/running/started_at/last_update_id/last_message_at/last_error).
- In logs, look for `starting subsystem: telegram transport` (from daemon startup) followed by `telegram polling transport started` (from gateway transport runtime).

## 6) Verify runtime status/health

### Transport status endpoint

```bash
curl -s http://127.0.0.1:51801/api/transports/telegram/status | jq
```

Expected key fields:
- `telegram.enabled`
- `telegram.running`
- `telegram.last_update_id`
- `telegram.last_message_at`
- `telegram.last_error`

### Gateway health endpoint

```bash
curl -s http://127.0.0.1:51801/health | jq
```

Health includes `telegram_transport` with the same runtime status snapshot.

## 7) Validate two-way comms

1. Send a Telegram message from an allowed chat.
2. Confirm `last_update_id` / `last_message_at` move forward.
3. Send outbound test via gateway API:

```bash
curl -s -X POST http://127.0.0.1:51801/api/send \
  -H 'content-type: application/json' \
  -d '{
    "channel": "telegram",
    "chat_id": "8496613333",
    "text": "ForgeFleet Telegram transport test ✅"
  }' | jq
```

Successful send returns `delivery.status = "sent"`.

## 8) Getting chat IDs

For private chats, your Telegram user id works (example: `8496613333`).
For groups/supergroups, use the numeric group chat id (often negative). Add that id to `allowed_chat_ids`.

## 9) Safety notes

- Keep bot token out of git and docs; use env var.
- Restrict `allowed_chat_ids` to trusted chats only.
- Keep `media_download_dir` under private storage (not public web root).
- Rotate token immediately if leaked (`/revoke` in BotFather).
- If `last_error` reports auth failures, verify token and bot permissions first.
