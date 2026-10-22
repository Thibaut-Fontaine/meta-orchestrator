# OpenCode → Telegram bridge

Sends a short Telegram message only when an OpenCode task finishes or needs
input (a permission or a question/form). Replying to a finished notification
sends the text back to that session as the next prompt; replying to a question
answers the pending form.

This runs as a separate user service instead of an OpenCode plugin, so there is
exactly one notifier no matter how many projects or OpenCode clients are open.

> **Preserved copy.** This directory is a source snapshot of the bridge built
> during the 2026-10-18/21 session. The live install lives in
> `~/.config/opencode/telegram-bridge/` and the service is
> `opencode-telegram-bridge.service`. The wider comparison with Android clients
> and the `mox` implications are in
> [`docs/research-phone-notifications.md`](../../docs/research-phone-notifications.md).

## Files

| File | Purpose |
| --- | --- |
| `bridge.py` | The bridge itself (Python 3.10, standard library only). |
| `setup.py` | Validates the bot token and writes the config. |
| `config.example.json` | Safe template; the live `config.json` is generated and must not be committed. |
| `opencode-telegram-bridge.service` | Portable user-service template. |
| `~/.config/opencode/telegram-bridge/config.json` | Live config: bot token, chat ID, notification switches (mode `600`). |
| `~/.local/state/opencode-telegram-bridge/state.json` | Telegram offset, last session, message → session map (mode `600`). |

## Install from this repository

```bash
mkdir -p ~/.config/opencode/telegram-bridge ~/.config/systemd/user

cp contrib/opencode-telegram-bridge/bridge.py \
   contrib/opencode-telegram-bridge/setup.py \
   ~/.config/opencode/telegram-bridge/

cp contrib/opencode-telegram-bridge/opencode-telegram-bridge.service \
   ~/.config/systemd/user/

# The setup script prompts for the token, then (with --start) enables and
# starts the user service.
python3 ~/.config/opencode/telegram-bridge/setup.py --start
```

Then open Telegram, start a chat with the bot, and send `/start`. The first
private chat is paired automatically.

## How it works

```
OpenCode shared service (:49374)
        │  GET /api/event (SSE, HTTP Basic auth)
        ▼
  bridge.py ──────────────────────────────► Telegram Bot API
  (systemd user service)                    getUpdates / sendMessage /
        ▲                                   editMessage / answerCallbackQuery
        │  POST /api/session/{id}/prompt
        │  POST /api/session/{id}/permission/{rid}/reply
        │  POST /api/session/{id}/form/{fid}/reply
        │
   Telegram chat (phone)
```

Important implementation details:

- **Completion uses `session.execution.succeeded`**, not `session.idle`. A smoke
  test showed `session.idle` does not fire promptly at the end of a turn;
  `session.execution.succeeded` does, after tool steps.
- **Permission replies use `{"decision": "once" | "always" | "reject"}`** on the
  HTTP endpoint, even though the plugin API calls the field `reply`.
- **Summary text comes from**
  `GET /api/session/{id}/message?type=assistant&order=desc&limit=N`, which is much
  lighter than fetching the full `/context`.
- **Prompts are queued** with `{"text": "...", "delivery": "queue"}`.
- **Telegram replies** are mapped by `message_id → session_id`; plain messages
  go to the session selected with `/use`.
- **Event IDs and Telegram `update_id` are persisted** to avoid duplicate
  notifications and replayed prompts after a restart.
- **Completion notifications append the last known DeepSeek balance** from
  `~/.local/state/meta-orchestrator/deepseek-balance.json`; forms and permission
  prompts stay short. This is a read-only snapshot, not a new API call.
- **Pending form and permission routes are persisted** in `state.json`, so a
  bridge restart does not invalidate buttons already sent. On startup, and on
  `/pending`, the bridge scans recent sessions for questions it missed.

## Message style

The bridge is deliberately terse:

- **Finished:** one line with the session title, then at most three main point
  titles extracted from the final answer (Markdown headings or bullet titles).
  No paths, paragraphs, or reply footer.
- **Question/form:** the field title, the question in at most two sentences,
  and inline option buttons. One field is asked at a time.
- **Permission:** the session title, the request message, and the action.
- **No error or interrupted messages by default.**

The whole notification is capped at three sentences.

## Configuration

Copy `config.example.json` to `~/.config/opencode/telegram-bridge/config.json`
and fill in the token, or let `setup.py` write it. The live file is mode `600`.

```json
{
  "botToken": "123456:ABC...",
  "chatID": 123456789,
  "server": null,
  "password": null,
  "notify": {
    "finished": true,
    "permission": true,
    "form": true,
    "error": false,
    "interrupted": false
  },
  "includeSubagents": false,
  "summaryChars": 280,
  "replyConfirmation": false
}
```

- `server` — optional. Defaults to `OPENCODE_SERVER`, then the URL printed by
  `opencode service status`, then `http://127.0.0.1:49374`.
- `password` — optional. Normally read from
  `~/.config/opencode/service.json` or `OPENCODE_PASSWORD`.
- `notify.finished` / `notify.permission` / `notify.form` — the only message
  types enabled by default.
- `notify.error` / `notify.interrupted` — off by default; enable only if you
  want noisy failure/stop notifications.
- `includeSubagents` — also notify for child/subagent sessions (default off).
- `summaryChars` — retained for compatibility; the finished formatter also
  applies the three-point/three-sentence cap.
- `replyConfirmation` — send “Sent to …” after forwarding a prompt (default off
  to avoid extra messages).

## Commands

| Command | Action |
| --- | --- |
| `/start`, `/help` | Show help and pair the first private chat. |
| `/sessions` | List the 10 most recent sessions. |
| `/use N` | Choose the session for plain (non-reply) messages. |
| `/status` | Show the selected session and currently running sessions. |
| `/stop` | Interrupt the selected session. |
| `/id` | Show the paired chat ID. |

Plain messages go to the selected session (or the last session that produced a
notification). Replying to a finished notification goes to the session that
produced it. Replying to a question answers the current form field; option
buttons answer with one tap. Stale button routes are dropped on restart so old
keyboards report an expired question instead of silently doing nothing. A
reply to an unmapped bot message — including a mox notification — is rejected
with a warning; it is never sent to the currently selected session by accident.

## Check and troubleshoot

```bash
python3 ~/.config/opencode/telegram-bridge/bridge.py --check
systemctl --user status opencode-telegram-bridge.service
journalctl --user -u opencode-telegram-bridge -f
```

`--check` verifies the bot token, reaches the OpenCode server, and confirms it
can read a recent session.

If the OpenCode server password changes, the bridge reloads it from
`~/.config/opencode/service.json` on the next request. Restart the service after
changing the server URL.

## Security and limitations

- The bot token is a password. `config.json` must stay `600`; never commit it.
- Only the paired chat can send prompts. Other chats are ignored.
- The bridge reads the **shared** OpenCode service. It does not see
  `opencode --standalone` or `opencode serve` on another port unless `server` is
  configured to that instance. In particular, `mox` agents run
  `opencode run --standalone` by default, so this bridge does not see them;
  `mox` has its own notifier.
- Pending permission and question routes are persisted in `state.json`; after a
  restart, old buttons keep working. If a route is genuinely gone, the bridge
  reports “expired” instead of forwarding the reply as a normal prompt, and
  `/pending` re-checks recent sessions.
- The last 500 notification message IDs are retained; older replies fall back to
  the last selected session.
- Running two bridges with the same bot token causes `409 Conflict` on
  `getUpdates`.
- If the token was ever pasted into a chat or log, revoke it in BotFather and
  rerun `setup.py`.
