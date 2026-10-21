# Phone notifications and remote control for OpenCode

**Research note — 2026-10-21**

Context: OpenCode `2.0.24` on the workstation, plus the `mox`
orchestrator. The goal was to get a phone notification when an OpenCode task
finishes and, ideally, send the next prompt from that notification.

This note compares building a small Telegram bridge against using an existing
Android client. It also records what was actually implemented and tested, and
what it means for `mox`.

> This is a research note, not a product decision. It contains no tokens,
> passwords, or personal data. External projects are described from their public
> documentation; none were cloned or modified.

---

## TL;DR

| Question | Short answer |
|---|---|
| Is there an official OpenCode mobile app? | No. V2 ships a terminal UI, a desktop app, and a web UI. Phone access is through `opencode pair` + a browser/PWA. |
| Is there a good Android client? | Yes: **OpenCode Mobile** (`getopencode.app`, `alvarolorentedev/opencode-mobile`) explicitly targets OpenCode 2. Android + iOS TestFlight, open source, not official. |
| Does it support notifications? | Yes: completion, failure, and “needs input”. Tapping a notification opens the app/session; there is no inline reply in the notification. Background delivery is best-effort. |
| Do existing Telegram plugins work? | The best-known one, **OpenCode Pilot** (`lesquel/open-remote-control`), is built on the V1 plugin API and is unlikely to load in V2. `opencode-notify` is Pushover-only and one-way. |
| What did we build? | A V2-native Telegram bridge: Python 3 stdlib only, user service, server-side event subscription, inline Telegram replies routed back to the session. |
| Does it fit `mox`? | Not directly. `mox` already has one-way notifications and a read-only bot. Its agents run `opencode run --standalone` by default, so the bridge (which watches the shared service) does **not** see them. |
| Recommendation | Keep the Telegram bridge for interactive sessions and true inline replies. Use OpenCode Mobile for a full phone UI. Keep `mox bot` read-only; if steering `mox` agents from a phone becomes a product goal, design a separate guarded command instead of making the bot writable. |

---

## Current status (at the time of writing)

- The bridge is installed at `~/.config/opencode/telegram-bridge/`; a source
  snapshot is preserved in `contrib/opencode-telegram-bridge/`.
- The systemd user unit is **enabled and active**. Manage it with
  `systemctl --user status opencode-telegram-bridge.service`.
- The Telegram token is configured and `getMe` succeeds; the owner's private
  chat is **paired**.
- A real finished-notification round-trip was tested against Telegram.
- The Android app has not been installed or tested on a device.
- The temporary OpenCode sessions used for smoke tests were deleted.

---

## 1. The problem

An OpenCode turn can run for a long time. While it runs, the user is away from
the terminal. Two distinct needs appear:

1. **Attention**: tell me when the turn finished, failed, or is blocked on a
   permission/question.
2. **Control**: let me reply with the next instruction without walking back to
   the computer.

Those needs are not the same:

- A one-way notifier is enough for (1).
- (2) requires a bidirectional channel that knows **which session** the
  notification belongs to.

OpenCode V2 is client/server: the TUI connects to a background service that owns
sessions, permissions, and tool execution. That makes remote control possible
over the HTTP API, but it also means the notifier must attach to the right
server.

---

## 2. Option A — build a Telegram bridge

### 2.1 Why a standalone service instead of an OpenCode plugin

The obvious approach is a V2 plugin under `~/.config/opencode/plugins/`. It has
access to the event stream and `ctx.session.prompt`, and it needs no auth setup.

The problem is scope: V2 plugins are **location-scoped**. The same global plugin
can be instantiated for several projects/locations. A naïve Telegram bridge in a
plugin would:

- send duplicate notifications for the same event;
- run several `getUpdates` long-polls for the same bot, which Telegram rejects
  with `409 Conflict`;
- keep separate message→session maps that disagree.

A single user-level service avoids all three. It also survives OpenCode
restarts and works for every client connected to the shared service.

The trade-off: the service needs the OpenCode server URL and password, and it
only sees the **shared background service** (see §2.7).

### 2.2 Architecture

```
OpenCode shared service (:49374)
        │
        │  GET /api/event        (SSE, HTTP Basic auth)
        ▼
  bridge.py  ──────────────────────────────►  Telegram Bot API
  (systemd user service)                     (getUpdates long-poll,
        ▲                                     sendMessage, editMessage,
        │                                     answerCallbackQuery)
        │  POST /api/session/{id}/prompt
        │  POST /api/session/{id}/permission/{rid}/reply
        │
   Telegram chat (phone)
```

The bridge is deliberately thin:

- subscribe to the server event stream;
- on a terminal event, read the session and its newest assistant text;
- send a Telegram message and remember `message_id → session_id`;
- on a Telegram reply, post the text back to that session;
- on a permission event, send inline buttons and reply with the decision.

### 2.3 Files and runtime

The implementation is preserved in this repository at
[`contrib/opencode-telegram-bridge/`](../contrib/opencode-telegram-bridge/):
`bridge.py`, `setup.py`, `config.example.json`, the user-service template, and
an implementation README. The live install is a copy under
`~/.config/opencode/telegram-bridge/`; the live `config.json` and state file are
intentionally not in the repository.

| Path | Purpose |
|---|---|
| `contrib/opencode-telegram-bridge/bridge.py` | Source snapshot of the bridge (single-file Python 3.10, stdlib only). |
| `contrib/opencode-telegram-bridge/setup.py` | Source snapshot of the setup helper. |
| `contrib/opencode-telegram-bridge/config.example.json` | Safe config template. |
| `~/.config/opencode/telegram-bridge/config.json` | Live bot token, chat ID, notification switches (mode `600`). |
| `~/.local/state/opencode-telegram-bridge/state.json` | Telegram offset, last session, message→session map (mode `600`). |
| `~/.config/systemd/user/opencode-telegram-bridge.service` | Installed user service. |

No Python packages are required. `curl` is not used for Telegram; the bridge uses
`urllib`. The service runs as the logged-in user and restarts on failure.

### 2.4 OpenCode API details that matter

| Need | Endpoint / detail |
|---|---|
| Auth | HTTP Basic: user `opencode`, password from `~/.config/opencode/service.json` (or `OPENCODE_PASSWORD`). |
| Event stream | `GET /api/event` — SSE, `data: {json}` lines plus heartbeat comments. |
| Session info | `GET /api/session/{id}` — title, directory, outcome, `parentID`. |
| Recent text | `GET /api/session/{id}/message?type=assistant&order=desc&limit=N` — much lighter than `/context`. |
| Send prompt | `POST /api/session/{id}/prompt` with `{"text": "...", "delivery": "queue"}`. |
| Reply to permission | `POST /api/session/{id}/permission/{requestID}/reply` with `{"decision": "once" \| "always" \| "reject"}`. |
| Reply to a question/form | `POST /api/session/{id}/form/{formID}/reply` with `{"answer": {fieldKey: value}}`. |
| Cancel a question/form | `DELETE /api/session/{id}/form/{formID}`. |
| Interrupt | `POST /api/session/{id}/interrupt`. |

Two findings were important:

1. **`session.idle` is not the completion signal.** A smoke test with a
   temporary session saw `session.execution.succeeded` immediately at the end of
   the turn, but no `session.idle` within 45 seconds. The bridge therefore
   listens to:
   - `session.execution.succeeded` → short “task finished”;
   - `permission.asked` → compact permission request;
   - `form.created` → agent question, delivered as a short form prompt.
   Error and interrupted events are disabled by default.

2. **The permission reply field is `decision`, not `reply`.** The plugin API
   exposes `ctx.permission.reply({ reply: ... })`, but the HTTP endpoint expects
   `{"decision": "once"}`. Using the wrong field silently fails validation.

   **Agent questions use the form API, not `question.asked`.** OpenCode 2.0.24
   emits `form.created` with `data.form` (`id`, `sessionID`, `title`, `fields`).
   The form reply body is `{"answer": {"fieldKey": value}}`.

The `/context` endpoint returns the whole active context and can be large. For a
notification summary, the paginated message endpoint is the right tool.

### 2.5 Telegram behavior

- **Pairing**: the first private chat that messages the bot is stored as
  `chatID` and persisted. Every other chat is ignored.
- **Notifications**:
  - finished: session title plus at most three extracted point titles; no path,
    paragraph, or footer; three sentences maximum;
  - permission: title, short message, action, with **Allow once / Always /
    Reject** inline buttons;
  - question/form: field title, question in at most two sentences, option
    buttons, and a Cancel button; multi-field forms are asked one field at a
    time.
- **Replies**: a Telegram reply to a finished notification is routed to that
  session as the next prompt. A reply to a question answers the current form
  field. Plain messages go to the session selected with `/use`.
- **Commands**: `/start`, `/help`, `/sessions`, `/use N`, `/status`, `/stop`,
  `/id`.
- **Feedback**: optional “Sent to <session>” confirmation, off by default.
- **Deduplication**: event IDs are kept in a small LRU; idle/finished events are
  throttled per session; Telegram `update_id` is persisted so a restart does not
  replay old prompts.

### 2.6 What was tested

- HTTP Basic auth against `/api/info`, `/api/session`, `/api/session/{id}`.
- SSE subscription; observed `session.execution.succeeded` after a turn with
  tool calls.
- Lightweight text extraction from assistant messages.
- Reply path: a synthetic Telegram update was fed to the bridge; the prompt
  reached the intended session and the assistant replied correctly.
- Permission callback mapping with a fake OpenCode client.
- Real Telegram delivery: direct message plus a finished-session notification.
- Form API: created a synthetic form, verified `GET`, `reply`, and state
  transition with a live OpenCode 2.0.24 server.
- Form callback and multi-field flow with a fake OpenCode client.
- All temporary sessions used for testing were deleted.

### 2.7 Limitations

- **Shared service only.** The bridge watches `http://127.0.0.1:49374`. A
  private `opencode --standalone` server, or `opencode serve` on another port, is
  invisible unless `server` is configured to that instance.
- **`mox` agents are invisible by default.** `mox` runs agents with
  `opencode run --standalone` (`standalone = true`), so they do not publish
  events to the shared service. The bridge cannot notify for them. `mox` already
  has its own notifier for that (see §4).
- **One bot, one bridge.** Running two bridges with the same token causes
  `409 Conflict` on `getUpdates`.
- **Permission and question buttons expire on restart.** Pending request tokens
  are kept in memory. After a bridge restart, old buttons report “expired”; a
  text reply to an old permission or question message can fall through as a
  normal prompt. The user can still answer in the TUI.
- **Message map is capped.** The last 500 notification message IDs are retained;
  older replies fall back to the last selected session.
- **Notification latency is server-side and reliable**, but Telegram itself must
  be reachable. A network outage delays delivery; the bridge reconnects.

### 2.8 Security

- The bot token is a password. `config.json` is `600`; the token is never put in
  a command line or logged.
- The chat ID is an allowlist. A different chat cannot send prompts.
- The OpenCode server password is read from the existing `service.json`; the
  bridge never stores it in its own config by default.
- The OpenCode server stays on loopback. Remote access should go through
  Tailscale, Cloudflare Tunnel, or `opencode pair --remote`, never an
  unauthenticated public port.
- **The token used during this experiment was pasted into the chat transcript.**
  It should be rotated with BotFather `/revoke`, then `setup.py` rerun. Treat the
  current token as exposed.

---

## 3. Option B — use an existing Android app

### 3.1 OpenCode Mobile (`getopencode.app`)

- Source: <https://github.com/alvarolorentedev/opencode-mobile>
- Site/docs: <https://getopencode.app>
- Android: Google Play and direct APK.
- iOS: TestFlight beta.
- License: open source, community-built, **not affiliated with OpenCode**.

It is the only Android client found that explicitly targets **OpenCode 2**:

- uses `@opencode/client` for native OpenCode 2, with the V1 SDK isolated to a
  compatibility path;
- pairs with `opencode pair` (one-time link/QR), or connects manually with a
  server URL, username, and password;
- supports the shared background service; `opencode serve` is a manual
  connection.

Features: sessions, streaming chat, permission approvals, diff review, file
browser/patches, project terminal, voice input, usage/cost, multiple servers.

**Notifications**

- completion, failure, and needs-input alerts;
- Android foreground service / headless JS keeps SSE/polling alive while tracked
  work is active;
- background checks are periodic; iOS background delivery is best-effort;
- tapping a notification opens the owning connection, project, and session;
- **no inline reply from the notification**, and Android widgets deliberately do
  not include approve/deny/reply/abort controls.

**Setup shape**

```bash
opencode service set password 'a-strong-password'
opencode service start
opencode service status
opencode pair
# scan the QR with the app
```

For remote access: Tailscale, Cloudflare Tunnel, a reverse proxy, or
`opencode pair --url https://your-host`.

**Caveats**

- Third-party app; review its privacy/security model before exposing a server.
- Notifications are not guaranteed when the app is force-stopped or the OS
  suspends it.
- No notification-inline reply; you tap through to the app.
- It sees the shared service, not `mox`’s private `--standalone` agents.

### 3.2 dzianisv/opencode-mobile

- Source: <https://github.com/dzianisv/opencode-mobile>
- Android only (Google Play, self-hosted F-Droid repo, APK).
- React Native/Expo; streaming chat, diffs, tool approval, biometrics.
- Documentation still shows the V1 flow: `npm install -g opencode-ai`,
  `OPENCODE_SERVER_PASSWORD=... opencode serve`. It may work manually with a V2
  server, but V2 compatibility is not stated.

### 3.3 OpenCode Pilot (`lesquel/open-remote-control`)

- Source: <https://github.com/lesquel/open-remote-control>
- A web dashboard + optional Telegram notifications + remote permission
  approvals + QR pairing + tunnel.
- Runs as a V1 plugin (`@opencode-ai/plugin`, `opencode.json::plugin` and
  `tui.json::plugin`). V2 changed the plugin API and uses `@opencode/plugin` and
  the `plugins` array, so this is likely V1-only.
- Useful as a design reference (dashboard, permission flow, notification
  channels), not as a drop-in V2 solution.

### 3.4 Other options

| Project | What it is | V2 note |
|---|---|---|
| `opencode-notify` (Pushover) | Tiny plugin, one-way “session idle” push | V1-era, no reply |
| OpenChamber | Web/desktop/VS Code UI, can be hosted and opened on a phone | Web UI, not a native app; check V2 support |
| CodeNomad | Desktop/web/mobile/remote client listed in the ecosystem | Check V2 support |
| Paseo | Multi-agent mobile/desktop client with a relay | Broader than OpenCode; check V2 support |
| Home Assistant actionable notifications | Webhook + HA automation, buttons can call back | Fully custom; not an OpenCode client |

### 3.5 Comparison

| Option | V2 support | Push while app closed | Reply from notification | Full mobile UI | Setup effort | Fits `mox` agents? |
|---|---|---|---|---|---|---|
| Telegram bridge (this experiment) | Yes, tested 2.0.24 | Yes (Telegram server) | **Yes, inline** | No | Medium (service + bot) | No (`--standalone`) |
| OpenCode Mobile | Yes, stated | Best-effort (app process) | No (tap to app) | **Yes** | Medium (pair + tunnel) | No (`--standalone`) |
| OpenCode web UI via `opencode pair` | Yes | No (browser notification only while open) | No | Web UI | Low | No |
| `mox notify` + `mox bot` | N/A (mox) | Yes (Telegram/webhook) | No (read-only bot) | No | Already built | **Yes** |
| OpenCode Pilot | Likely no (V1 plugin) | Via web push/Telegram | Via dashboard, not inline | Dashboard | Medium | No |
| `opencode-notify` | V1-era | Yes (Pushover) | No | No | Low | No |

---

## 4. What this means for `mox`

`mox` already has the two pieces that matter for orchestrated agents:

- **`mox notify`** (`src/notify.rs`): one-way notifications for `questions`,
  `progress`, `finished`, `failed`, `checks`, `digest`, and `test`, with
  Telegram, desktop, and JSON-webhook channels. It follows the project’s security
  rules: token from `MOX_TELEGRAM_TOKEN`, 0600 curl config, never argv.
- **`mox bot`** (`src/bot.rs`): a read-only Telegram long-poll with chat
  allowlisting. It exposes projects, milestones, todos, checks, questions, stats,
  reports, and logs. Per `AGENTS.md` rule 14, it must never mutate projects,
  start agents, or answer questions.

That is the correct layer for `mox`’s own agents. The OpenCode bridge is a
different tool:

- it watches the shared OpenCode service, not `mox`’s private `--standalone`
  servers;
- `mox` already emits a `finished`/`failed` event at the orchestration layer;
- steering a `mox` agent from a phone would mean injecting a prompt into a
  running `opencode run` process, which is not a supported `mox` workflow and
  would need its own allowlist, state, tests, and security review.

Setting `standalone = false` would make `mox` agents share the background
service, so the bridge could observe their sessions. That is not a reason to
switch: `mox` already emits its own events, and a finished `opencode run` process
is not a live chat you can usefully steer.

### If phone control of `mox` becomes a product goal

Do **not** quietly make `mox bot` writable. Rule 14 is a tested product
contract. A safer design would be a separate command, for example `mox steer`,
with:

- an explicit `--allow-write`/config gate (default off);
- the same chat-ID allowlist;
- a hard cap on what can be sent (prompt text only, no shell, no project
  mutation);
- state that records who sent what and when;
- unit tests and an e2e test proving that the read-only bot remains read-only;
- an update to `AGENTS.md`, `README.md`, and `docs/security.md`.

Until that exists, the phone workflow for `mox` is: receive `mox notify`,
inspect with `mox bot`, and act locally.

---

## 5. Recommendation

1. **For the original goal — “notify me when an interactive OpenCode task
   finishes, and let me reply from the notification”** — keep the Telegram
   bridge. It is the only option tested against V2.0.24 that provides true inline
   replies, and it works while the phone app is closed.
2. **For a full mobile UI** — install OpenCode Mobile (Android Play/APK, iOS
   TestFlight), pair with `opencode pair`, and expose the server through
   Tailscale or a tunnel. Accept that notifications open the app rather than
   offering inline reply.
3. **For `mox`** — keep `mox notify` + `mox bot`. The bridge does not see
   `--standalone` agents, and making the bot writable would violate the current
   product contract. If steering is needed, design a separate guarded command.
4. **Do not adopt OpenCode Pilot or other V1 plugins** for V2 without verifying
   compatibility; the V2 plugin API is a breaking change.

---

## 6. Next actions

- [ ] Start the bridge (`systemctl --user start opencode-telegram-bridge.service`)
      and send `/start` to the bot from the phone to pair the chat.
- [ ] Rotate the Telegram token that appeared in the chat transcript, then rerun
      `setup.py` and restart the service.
- [ ] Try OpenCode Mobile on Android if a full mobile UI is more valuable than
      inline replies; pair it with `opencode pair` and a Tailscale/Cloudflare
      route.
- [ ] If steering `mox` agents from a phone is a real requirement, write a
      short design for a separate guarded command (`mox steer` or similar)
      before touching `mox bot`.

---

## 7. Security checklist

- [ ] Rotate the Telegram bot token that appeared in the chat transcript
      (BotFather `/revoke`), then rerun `setup.py`.
- [ ] Keep `~/.config/opencode/telegram-bridge/config.json` at `600`.
- [ ] Keep the OpenCode server on loopback; use Tailscale/Cloudflare/OpenTunnel
      for remote access.
- [ ] Use a strong `opencode service set password`.
- [ ] Keep the bot’s chat allowlist to one private chat.
- [ ] Review the Android client’s permissions and privacy before installing.
- [ ] Never put tokens in `mox` argv; follow `src/notify.rs`’s 0600 curl config
      pattern if integrating anything.
- [ ] Do not expose an unauthenticated OpenCode port to the internet.

---

## 8. References

- OpenCode V2 web/pairing: <https://opencode.ai/v2/docs/cli/web/>
- OpenCode V2 API: <https://opencode.ai/v2/docs/api/>
- OpenCode V2 plugins: <https://opencode.ai/v2/docs/build/plugins/>
- OpenCode Mobile: <https://getopencode.app> and
  <https://github.com/alvarolorentedev/opencode-mobile>
- dzianisv/opencode-mobile: <https://github.com/dzianisv/opencode-mobile>
- OpenCode Pilot: <https://github.com/lesquel/open-remote-control>
- `opencode-notify` write-up:
  <https://dev.to/tobrun/mobile-push-notifications-with-opencode-1agg>
- Local `mox` references:
  - `src/bot.rs` — read-only Telegram bot
  - `src/notify.rs` — Telegram/desktop/webhook notifier
  - `README.md` — “Telegram bot”, “Reminders outside the terminal”,
    `standalone` configuration
  - `docs/security.md` — secrets and remote-bot rules
  - `AGENTS.md` — rule 14 (`mox bot` is read-only)

---

## Appendix — bridge operations

```bash
# Status
systemctl --user status opencode-telegram-bridge.service

# Start / enable on login
systemctl --user start opencode-telegram-bridge.service
systemctl --user enable opencode-telegram-bridge.service

# Logs
journalctl --user -u opencode-telegram-bridge -f

# Validate token, server, and session access
python3 ~/.config/opencode/telegram-bridge/bridge.py --check

# Restart after a config change
systemctl --user restart opencode-telegram-bridge.service

# Disable temporarily (e.g. while using the Android app)
systemctl --user disable --now opencode-telegram-bridge.service
```

Bot commands: `/start`, `/help`, `/sessions`, `/use N`, `/status`, `/stop`,
`/id`.
