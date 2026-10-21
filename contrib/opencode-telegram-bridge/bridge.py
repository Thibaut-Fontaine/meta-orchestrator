#!/usr/bin/env python3
"""OpenCode -> Telegram bridge.

Watches the OpenCode server event stream and sends a short Telegram message when
a session finishes or needs input (permission or form question). Replies to
finished messages become the next prompt; replies to questions answer the
pending request.

Only the Python standard library is used.
"""

from __future__ import annotations

import argparse
import base64
import copy
import html
import json
import logging
import os
import queue
import re
import secrets
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import deque
from pathlib import Path

VERSION = "1.1.2"

CONFIG_HOME = Path(os.environ.get("XDG_CONFIG_HOME") or (Path.home() / ".config"))
BRIDGE_DIR = CONFIG_HOME / "opencode" / "telegram-bridge"
CONFIG_PATH = Path(os.environ.get("OPENCODE_TELEGRAM_CONFIG") or (BRIDGE_DIR / "config.json"))
STATE_HOME = Path(os.environ.get("XDG_STATE_HOME") or (Path.home() / ".local" / "state"))
STATE_PATH = Path(os.environ.get("OPENCODE_TELEGRAM_STATE") or (STATE_HOME / "opencode-telegram-bridge" / "state.json"))
BALANCE_STATE_PATH = Path(
    os.environ.get("MOX_BALANCE_STATE")
    or (STATE_HOME / "meta-orchestrator" / "deepseek-balance.json")
)

DEFAULT_CONFIG = {
    "botToken": "",
    "chatID": None,
    "server": None,
    "password": None,
    "notify": {
        "finished": True,
        "permission": True,
        "form": True,
        "error": False,
        "interrupted": False,
    },
    "includeSubagents": False,
    "summaryChars": 280,
    "replyConfirmation": False,
}

DEFAULT_STATE = {
    "updateOffset": 0,
    "lastSession": None,
    "messages": {},
    "sessionChoices": [],
    "seenUpdates": [],
}

log = logging.getLogger("opencode-telegram")


# ---------------------------------------------------------------------------
# small utilities


def load_json(path: Path, default):
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return json.loads(json.dumps(default))
    except Exception as exc:  # noqa: BLE001 - config is user supplied
        log.warning("could not read %s: %s", path, exc)
        return json.loads(json.dumps(default))


def save_json(path: Path, value) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        os.chmod(path.parent, 0o700)
    except OSError:
        pass
    tmp = path.with_name(f"{path.name}.tmp.{os.getpid()}.{threading.get_ident()}")
    tmp.write_text(json.dumps(value, indent=2, ensure_ascii=False) + "\n")
    os.chmod(tmp, 0o600)
    os.replace(tmp, path)


def load_config() -> dict:
    raw = load_json(CONFIG_PATH, {})
    config = dict(DEFAULT_CONFIG)
    config.update({key: value for key, value in raw.items() if key != "notify"})
    config["notify"] = {**DEFAULT_CONFIG["notify"], **(raw.get("notify") or {})}
    return config


def load_state() -> dict:
    raw = load_json(STATE_PATH, DEFAULT_STATE)
    state = {**DEFAULT_STATE, **raw}
    if not isinstance(state.get("messages"), dict):
        state["messages"] = {}
    if not isinstance(state.get("sessionChoices"), list):
        state["sessionChoices"] = []
    if not isinstance(state.get("seenUpdates"), list):
        state["seenUpdates"] = []
    return state


def basic_auth(password: str) -> str:
    token = base64.b64encode(f"opencode:{password}".encode()).decode()
    return f"Basic {token}"


def http_json(method: str, url: str, body=None, headers=None, timeout: float = 30):
    """Perform an HTTP request and decode a JSON response.

    Returns (status, payload). HTTP errors are returned, not raised, so callers
    can make decisions based on the status code.
    """
    data = None
    request_headers = {"Accept": "application/json"}
    if headers:
        request_headers.update(headers)
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        request_headers["Content-Type"] = "application/json"

    request = urllib.request.Request(url, data=data, headers=request_headers, method=method)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            raw = response.read()
            return response.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as exc:
        raw = exc.read()
        try:
            payload = json.loads(raw) if raw else None
        except Exception:  # noqa: BLE001 - non-JSON error body
            payload = raw.decode("utf-8", "replace")
        return exc.code, payload


def service_config_paths() -> list[Path]:
    candidates = [
        CONFIG_HOME / "opencode" / "service.json",
        Path.home() / ".config" / "opencode" / "service.json",
    ]
    unique: list[Path] = []
    for path in candidates:
        if path not in unique:
            unique.append(path)
    return unique


def read_password(config: dict) -> str:
    password = os.environ.get("OPENCODE_PASSWORD")
    if password:
        return password

    for path in service_config_paths():
        try:
            value = json.loads(path.read_text()).get("password")
        except Exception:  # noqa: BLE001 - missing or unreadable is fine
            value = None
        if value:
            return value

    if config.get("password"):
        return str(config["password"])

    raise RuntimeError(
        "could not read the OpenCode server password. Expected it in "
        f"{service_config_paths()[0]} (or set OPENCODE_PASSWORD)."
    )


def resolve_server(config: dict) -> str:
    configured = (config.get("server") or os.environ.get("OPENCODE_SERVER") or "").strip()
    if configured:
        return configured.rstrip("/")

    try:
        output = subprocess.check_output(
            ["opencode", "service", "status"],
            text=True,
            timeout=5,
            stderr=subprocess.DEVNULL,
        )
        for word in output.split():
            if word.startswith("http://") or word.startswith("https://"):
                return word.rstrip("/")
    except Exception:  # noqa: BLE001 - fall back to the documented default
        pass

    return "http://127.0.0.1:49374"


def shorten(value: str, limit: int) -> str:
    value = re.sub(r"\s+", " ", value or "").strip()
    if len(value) <= limit:
        return value
    return value[: max(0, limit - 1)].rstrip() + "\u2026"


def strip_markdown(value: str) -> str:
    """Turn a small Markdown snippet into plain text."""
    value = value or ""
    value = re.sub(r"(?m)^\s{0,3}#{1,6}\s*", "", value)
    value = re.sub(r"!\[([^\]]*)\]\([^)]+\)", r"\1", value)
    value = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", value)
    value = re.sub(r"<[^>]+>", "", value)
    value = re.sub(r"[*_`~]+", "", value)
    return re.sub(r"\s+", " ", value).strip()


def first_sentence(value: str, limit: int = 120) -> str:
    value = strip_markdown(value)
    if not value:
        return ""
    match = re.match(r"(.+?[.!?])(?:\s|$)", value)
    if match:
        value = match.group(1)
    if len(value) > limit:
        value = value[: max(0, limit - 1)].rstrip() + "\u2026"
    return value.strip(" \t-:;")


def point_title(value: str, limit: int = 90) -> str:
    """Keep a heading-like title, not the paragraph below it."""
    value = first_sentence(value, limit)
    if ":" in value:
        head = value.split(":", 1)[0].strip()
        if 2 <= len(head) <= 60:
            value = head
    return value.strip(" \t-:;.")


def compact_text(value: str, max_sentences: int = 3, limit: int = 300) -> str:
    """Plain text with at most a few sentences and a hard character cap."""
    value = strip_markdown(value)
    if not value:
        return ""
    sentences = re.split(r"(?<=[.!?])\s+", value)
    value = " ".join(sentences[: max(1, max_sentences)]).strip()
    if len(value) > limit:
        value = value[: max(0, limit - 1)].rstrip() + "\u2026"
    return value


GENERIC_POINTS = {
    "summary",
    "overview",
    "notes",
    "note",
    "result",
    "results",
    "details",
    "changes",
    "what changed",
    "status",
}


def extract_points(text: str, limit: int = 3) -> list[str]:
    """Extract short headings or bullet titles from the final assistant text.

    Falls back to the first few sentences when the answer is plain prose.
    """
    points: list[str] = []
    for line in (text or "").splitlines():
        stripped = line.strip()
        if not stripped:
            continue
        candidate = None
        heading = re.match(r"^#{1,6}\s+(.+?)\s*#*\s*$", stripped)
        if heading:
            candidate = heading.group(1)
        else:
            bold_line = re.match(r"^(?:\*\*|__)(.+?)(?:\*\*|__)\s*:?\s*$", stripped)
            if bold_line:
                candidate = bold_line.group(1)
            else:
                bullet = re.match(r"^(?:[-*+]|\d+[.)])\s+(.+)$", stripped)
                if bullet:
                    item = bullet.group(1)
                    bold = re.match(r"^(?:\*\*|__)(.+?)(?:\*\*|__)", item)
                    candidate = bold.group(1) if bold else item
        if not candidate:
            continue
        candidate = point_title(candidate)
        if not candidate:
            continue
        if candidate.casefold() in GENERIC_POINTS:
            continue
        if candidate.casefold() in {point.casefold() for point in points}:
            continue
        points.append(candidate)
        if len(points) >= limit:
            break

    if points:
        return points[:limit]

    plain = strip_markdown(text)
    for generic in GENERIC_POINTS:
        plain = re.sub(rf"^{re.escape(generic)}\b[:\s-]*", "", plain, flags=re.IGNORECASE)
    plain = plain.strip()
    if not plain:
        return []
    result = []
    for sentence in re.split(r"(?<=[.!?])\s+", plain)[:limit]:
        sentence = first_sentence(sentence, 100).rstrip(" .;:")
        if sentence:
            result.append(sentence)
    return result


def finished_text(title: str, text: str, failed: bool = False) -> str:
    """Title plus up to three main point titles; nothing else."""
    icon = "\u26a0\ufe0f" if failed else "\u2705"
    lines = [f"{icon} <b>{esc(shorten(strip_markdown(title), 120))}</b>"]
    for point in extract_points(text, 3):
        lines.append(f"\u2022 {esc(point)}")
    return "\n".join(lines)


def is_internal_session_title(title: str) -> bool:
    """Planning/reflection calls are mox internals, not user-facing tasks."""
    return str(title or "").strip().lower().startswith("mox: planning")


def esc(value) -> str:
    """Escape text for Telegram HTML parse mode."""
    return html.escape(str(value), quote=False)


def quote_id(value: str) -> str:
    return urllib.parse.quote(str(value), safe="")


# ---------------------------------------------------------------------------
# API clients


class ApiError(Exception):
    def __init__(self, status: int, body=None):
        self.status = status
        self.body = body
        super().__init__(f"HTTP {status}: {body}")


class OpenCode:
    def __init__(self, server: str, config: dict):
        self.server = server.rstrip("/")
        self.config = config
        self.password: str | None = None
        self.lock = threading.Lock()

    def _auth(self) -> str:
        with self.lock:
            if not self.password:
                self.password = read_password(self.config)
            return basic_auth(self.password)

    def request(self, method: str, path: str, body=None, timeout: float = 30):
        for attempt in (1, 2):
            auth = self._auth()
            status, payload = http_json(
                method,
                self.server + path,
                body,
                {"Authorization": auth},
                timeout,
            )
            if status == 401 and attempt == 1:
                with self.lock:
                    self.password = None
                continue
            if status >= 400:
                raise ApiError(status, payload)
            if isinstance(payload, dict) and "data" in payload:
                return payload["data"]
            return payload
        raise ApiError(401, "unauthorized")

    def session(self, session_id: str):
        return self.request("GET", f"/api/session/{quote_id(session_id)}")

    def context(self, session_id: str):
        return self.request("GET", f"/api/session/{quote_id(session_id)}/context", timeout=60)

    def messages(self, session_id: str, message_type: str, limit: int = 10):
        query = urllib.parse.urlencode(
            {"type": message_type, "order": "desc", "limit": str(limit)}
        )
        return self.request(
            "GET",
            f"/api/session/{quote_id(session_id)}/message?{query}",
            timeout=30,
        ) or []

    def recent_text(self, session_id: str) -> str:
        """Return the newest assistant text, preferring the current turn."""
        try:
            users = self.messages(session_id, "user", 1)
        except ApiError:
            users = []
        since = 0
        if users:
            since = (users[0].get("time") or {}).get("created") or 0

        assistants = self.messages(session_id, "assistant", 10)
        fallback = ""
        for message in assistants:
            text = ""
            for part in reversed(message.get("content") or []):
                if part.get("type") == "text" and (part.get("text") or "").strip():
                    text = part["text"].strip()
                    break
            if text and not fallback:
                fallback = text
            if text and ((message.get("time") or {}).get("created") or 0) >= since:
                return text
        return fallback

    def prompt(self, session_id: str, text: str):
        return self.request(
            "POST",
            f"/api/session/{quote_id(session_id)}/prompt",
            {"text": text, "delivery": "queue"},
        )

    def interrupt(self, session_id: str):
        return self.request("POST", f"/api/session/{quote_id(session_id)}/interrupt")

    def form(self, session_id: str, form_id: str):
        return self.request(
            "GET",
            f"/api/session/{quote_id(session_id)}/form/{quote_id(form_id)}",
        )

    def reply_form(self, session_id: str, form_id: str, answer: dict):
        return self.request(
            "POST",
            f"/api/session/{quote_id(session_id)}/form/{quote_id(form_id)}/reply",
            {"answer": answer},
        )

    def cancel_form(self, session_id: str, form_id: str):
        return self.request(
            "DELETE",
            f"/api/session/{quote_id(session_id)}/form/{quote_id(form_id)}",
        )

    def reply_permission(self, session_id: str, request_id: str, decision: str, message: str | None = None):
        body = {"decision": decision}
        if message:
            body["message"] = message
        return self.request(
            "POST",
            f"/api/session/{quote_id(session_id)}/permission/{quote_id(request_id)}/reply",
            body,
        )


class TelegramError(Exception):
    pass


class Telegram:
    def __init__(self, token: str, chat_id=None):
        self.token = token
        self.chat_id = chat_id

    def _scrub(self, value) -> str:
        return str(value).replace(self.token, "<token>")

    def call(self, method: str, params=None, timeout: float = 45):
        url = f"https://api.telegram.org/bot{self.token}/{method}"
        try:
            status, payload = http_json("POST", url, params or {}, timeout=timeout)
        except Exception as exc:  # noqa: BLE001 - network failure
            raise TelegramError(self._scrub(exc)) from exc

        if not isinstance(payload, dict) or not payload.get("ok"):
            description = payload.get("description") if isinstance(payload, dict) else payload
            raise TelegramError(self._scrub(f"{method}: {description}"))
        return payload.get("result")

    def send(self, text: str, chat_id=None, reply_to=None, keyboard=None):
        target = chat_id or self.chat_id
        if target is None:
            raise TelegramError("not paired with a chat yet")
        params = {
            "chat_id": target,
            "text": text[:4096],
            "parse_mode": "HTML",
            "disable_web_page_preview": True,
        }
        if reply_to is not None:
            params["reply_to_message_id"] = reply_to
            params["allow_sending_without_reply"] = True
        if keyboard:
            params["reply_markup"] = {"inline_keyboard": keyboard}
        return self.call("sendMessage", params)

    def edit(self, message_id: int, text: str):
        if self.chat_id is None:
            return
        try:
            self.call(
                "editMessageText",
                {
                    "chat_id": self.chat_id,
                    "message_id": message_id,
                    "text": text[:4096],
                    "parse_mode": "HTML",
                    "disable_web_page_preview": True,
                    "reply_markup": {"inline_keyboard": []},
                },
            )
        except TelegramError as exc:
            log.debug("could not edit message %s: %s", message_id, exc)

    def answer_callback(self, callback_id: str, text: str = ""):
        try:
            self.call("answerCallbackQuery", {"callback_query_id": callback_id, "text": text[:200]})
        except TelegramError as exc:
            log.debug("could not answer callback: %s", exc)

    def get_updates(self, offset: int, timeout: int = 30):
        return self.call(
            "getUpdates",
            {
                "offset": offset,
                "timeout": timeout,
                "allowed_updates": ["message", "callback_query"],
            },
            timeout=timeout + 15,
        ) or []


# ---------------------------------------------------------------------------
# bridge


def balance_footer(path: Path | None = None) -> str:
    """Return a compact footer from the mox balance state, or an empty string."""
    state_path = path or BALANCE_STATE_PATH
    try:
        payload = json.loads(state_path.read_text())
    except (OSError, json.JSONDecodeError):
        return ""
    total = payload.get("last_balance")
    if total is None:
        return ""
    try:
        total = float(total)
    except (TypeError, ValueError):
        return ""
    currency = str(payload.get("currency") or "USD")
    checked = str(payload.get("checked_at") or "unknown time").replace("T", " ")[:16]
    return f"DeepSeek: ${total:.2f} {currency} left (checked {checked} UTC)"


HELP_TEXT = """<b>OpenCode Telegram bridge</b>

Reply to a notification to send the next prompt to that session.
Plain messages go to the session selected with /use.

/sessions \u2014 recent sessions
/use N \u2014 select the session for plain messages
/status \u2014 selected session and running work
/pending \u2014 re-check and re-send pending questions
/stop \u2014 interrupt the selected session
/id \u2014 show this chat ID
/help \u2014 this message"""


class Bridge:
    def __init__(self, config: dict):
        self.config = config
        self.server = resolve_server(config)
        self.oc = OpenCode(self.server, config)
        self.tg = Telegram(config.get("botToken", ""), config.get("chatID"))
        self.state = load_state()
        self.state_lock = threading.RLock()
        self.pending_lock = threading.Lock()
        self.stop = threading.Event()
        self.recent_events: deque[str] = deque(maxlen=300)
        self.finished_seen: dict[str, float] = {}
        self.perm_seen: set[str] = set()
        self.perm_by_token: dict[str, dict] = {}
        self.perm_by_message: dict[str, str] = {}
        self.form_seen: set[str] = set()
        self.form_closed: set[str] = set()
        self.form_by_token: dict[str, dict] = {}
        self.form_by_message: dict[str, str] = {}
        self.outbox: queue.Queue = queue.Queue()
        self.load_pending_state()

    # -- lifecycle ---------------------------------------------------------

    def run(self):
        log.info("bridge %s starting (server %s)", VERSION, self.server)
        if not self.config.get("botToken"):
            raise SystemExit(
                "no Telegram bot token configured. Run setup.py or put botToken in "
                f"{CONFIG_PATH}"
            )

        threading.Thread(target=self.sender_loop, name="sender", daemon=True).start()
        threading.Thread(target=self.telegram_loop, name="telegram", daemon=True).start()
        threading.Thread(target=self.event_loop, name="events", daemon=True).start()
        threading.Thread(target=self.recover_safely, name="recover", daemon=True).start()

        while not self.stop.is_set():
            self.stop.wait(1)

    def shutdown(self):
        self.stop.set()

    def sender_loop(self):
        while not self.stop.is_set():
            try:
                item = self.outbox.get(timeout=1)
            except queue.Empty:
                continue
            text, session_id, keyboard, permission_token, form_token = item
            try:
                result = self.tg.send(text, keyboard=keyboard)
                message_id = result.get("message_id") if isinstance(result, dict) else None
                if message_id is not None:
                    if session_id:
                        self.remember_message(message_id, session_id)
                    if permission_token:
                        with self.pending_lock:
                            entry = self.perm_by_token.get(permission_token)
                            if entry is not None:
                                entry["messageID"] = message_id
                            self.perm_by_message[str(message_id)] = permission_token
                    if form_token:
                        with self.pending_lock:
                            entry = self.form_by_token.get(form_token)
                            if entry is not None:
                                entry["messageID"] = message_id
                            self.form_by_message[str(message_id)] = form_token
                    if permission_token or form_token:
                        self.persist_pending()
            except TelegramError as exc:
                log.warning("telegram send failed: %s", exc)
            except Exception as exc:  # noqa: BLE001 - keep the worker alive
                log.exception("unexpected send error: %s", exc)

    # -- state -------------------------------------------------------------

    def save_state(self):
        with self.state_lock:
            state = copy.deepcopy(self.state)
            messages = dict(state.get("messages") or {})
            if len(messages) > 500:
                for key in list(messages)[: len(messages) - 500]:
                    messages.pop(key, None)
            state["messages"] = messages
            state["seenUpdates"] = list(state.get("seenUpdates") or [])[-200:]
            save_json(STATE_PATH, state)

    def load_pending_state(self):
        """Restore pending form/permission routes so old Telegram buttons still work."""
        with self.state_lock:
            forms = self.state.get("forms") or {}
            form_messages = self.state.get("formMessages") or {}
            permissions = self.state.get("permissions") or {}
            permission_messages = self.state.get("permissionMessages") or {}
        if isinstance(forms, dict):
            self.form_by_token = {
                str(token): entry for token, entry in forms.items() if isinstance(entry, dict)
            }
        if isinstance(form_messages, dict):
            self.form_by_message = {str(key): str(value) for key, value in form_messages.items()}
        if isinstance(permissions, dict):
            self.perm_by_token = {
                str(token): entry
                for token, entry in permissions.items()
                if isinstance(entry, dict)
            }
        if isinstance(permission_messages, dict):
            self.perm_by_message = {
                str(key): str(value) for key, value in permission_messages.items()
            }
        self.form_seen = {
            str(entry.get("formID"))
            for entry in self.form_by_token.values()
            if entry.get("formID")
        }
        self.perm_seen = {
            str(entry.get("requestID"))
            for entry in self.perm_by_token.values()
            if entry.get("requestID")
        }

    def persist_pending(self):
        with self.pending_lock:
            state = {
                "forms": copy.deepcopy(self.form_by_token),
                "formMessages": dict(self.form_by_message),
                "permissions": copy.deepcopy(self.perm_by_token),
                "permissionMessages": dict(self.perm_by_message),
            }
        with self.state_lock:
            self.state.update(state)
        self.save_state()

    def recover_safely(self):
        self.stop.wait(2)
        if self.stop.is_set():
            return
        try:
            self.recover_pending()
        except Exception as exc:  # noqa: BLE001 - recovery is best effort
            log.warning("pending recovery failed: %s", exc)

    def recover_pending(self):
        """Find forms/permissions created while the bridge was down."""
        sessions = self.oc.request("GET", "/api/session?limit=40") or []
        if not isinstance(sessions, list):
            return
        recovered = 0
        for session in sessions:
            if not isinstance(session, dict):
                continue
            session_id = str(session.get("id") or "")
            if not session_id:
                continue
            if is_internal_session_title(str(session.get("title") or "")):
                continue
            try:
                forms = self.oc.request(
                    "GET", f"/api/session/{quote_id(session_id)}/form"
                ) or []
            except ApiError as exc:
                log.debug("could not list forms for %s: %s", session_id, exc)
                forms = []
            if isinstance(forms, list):
                for form in forms:
                    if not isinstance(form, dict) or not form.get("id"):
                        continue
                    before = len(self.form_seen)
                    self.schedule_form({**form, "sessionID": form.get("sessionID") or session_id})
                    if len(self.form_seen) > before:
                        recovered += 1
            try:
                permissions = self.oc.request(
                    "GET", f"/api/session/{quote_id(session_id)}/permission"
                ) or []
            except ApiError as exc:
                log.debug("could not list permissions for %s: %s", session_id, exc)
                permissions = []
            if isinstance(permissions, list):
                for request in permissions:
                    if not isinstance(request, dict) or not request.get("id"):
                        continue
                    self.schedule_permission(
                        {**request, "sessionID": request.get("sessionID") or session_id}
                    )
        self.prune_pending()
        log.info("pending recovery: %d new form(s)", recovered)

    def prune_pending(self):
        changed = False
        with self.pending_lock:
            entries = list(self.form_by_token.items())
        for token, entry in entries:
            session_id = str(entry.get("sessionID") or "")
            form_id = str(entry.get("formID") or "")
            if not session_id or not form_id:
                with self.pending_lock:
                    self.form_by_token.pop(token, None)
                changed = True
                continue
            try:
                detail = self.oc.form(session_id, form_id)
            except ApiError as exc:
                if exc.status not in (404, 409):
                    continue
                detail = None
            status = None
            if isinstance(detail, dict):
                state = detail.get("state") or {}
                if isinstance(state, dict):
                    status = state.get("status")
            if detail is None or status not in (None, "pending"):
                with self.pending_lock:
                    self.form_by_token.pop(token, None)
                    for message_id, mapped in list(self.form_by_message.items()):
                        if mapped == token:
                            self.form_by_message.pop(message_id, None)
                changed = True
        if changed:
            self.persist_pending()

    def remember_message(self, message_id: int, session_id: str):
        with self.state_lock:
            self.state.setdefault("messages", {})[str(message_id)] = session_id
            self.state["lastSession"] = session_id
        self.save_state()

    def set_last_session(self, session_id: str):
        with self.state_lock:
            self.state["lastSession"] = session_id
        self.save_state()

    def get_last_session(self):
        with self.state_lock:
            return self.state.get("lastSession")

    def session_for_message(self, message_id):
        with self.state_lock:
            return (self.state.get("messages") or {}).get(str(message_id))

    # -- OpenCode event stream --------------------------------------------

    def event_loop(self):
        attempt = 0
        while not self.stop.is_set():
            try:
                auth = self.oc._auth()
                request = urllib.request.Request(
                    self.server + "/api/event",
                    headers={"Authorization": auth, "Accept": "text/event-stream"},
                    method="GET",
                )
                log.info("subscribing to OpenCode events")
                with urllib.request.urlopen(request, timeout=120) as response:
                    attempt = 0
                    for raw in response:
                        if self.stop.is_set():
                            return
                        line = raw.decode("utf-8", "replace").rstrip("\r\n")
                        if not line.startswith("data:"):
                            continue
                        payload = line[5:].strip()
                        if not payload:
                            continue
                        try:
                            event = json.loads(payload)
                        except json.JSONDecodeError:
                            continue
                        try:
                            self.handle_event(event)
                        except Exception as exc:  # noqa: BLE001 - keep streaming
                            log.exception("event handler failed: %s", exc)
            except urllib.error.HTTPError as exc:
                if exc.code == 401:
                    with self.oc.lock:
                        self.oc.password = None
                log.warning("event stream HTTP %s; retrying", exc.code)
            except Exception as exc:  # noqa: BLE001 - reconnect
                if self.stop.is_set():
                    return
                log.warning("event stream disconnected: %s", exc)
            attempt += 1
            self.stop.wait(min(30, 5 * attempt))

    def handle_event(self, event: dict):
        event_id = event.get("id")
        event_type = event.get("type") or ""
        data = event.get("data") or {}

        if event_id:
            with self.state_lock:
                if event_id in self.recent_events:
                    return
                self.recent_events.append(event_id)

        if event_type == "session.execution.succeeded":
            session_id = data.get("sessionID")
            if session_id and self.config["notify"].get("finished", True):
                self.schedule_finished(session_id)
            return

        if event_type == "session.execution.failed":
            session_id = data.get("sessionID")
            if session_id and self.config["notify"].get("error", True):
                self.schedule_error(session_id, data.get("error"))
            return

        if event_type == "session.execution.interrupted":
            session_id = data.get("sessionID")
            if session_id and self.config["notify"].get("interrupted", False):
                self.schedule_interrupted(session_id, data.get("reason"))
            return

        if event_type == "form.created":
            form = data.get("form") or data
            if form and self.config["notify"].get("form", True):
                self.schedule_form(form)
            return

        if event_type in ("form.replied", "form.cancelled"):
            self.clear_form(data.get("id") or data.get("formID"))
            return

        if event_type == "permission.asked":
            if self.config["notify"].get("permission", True):
                self.schedule_permission(data)

    def schedule_finished(self, session_id: str):
        now = time.time()
        with self.state_lock:
            if now - self.finished_seen.get(session_id, 0) < 1.5:
                return
            self.finished_seen[session_id] = now
        threading.Thread(target=self.notify_finished, args=(session_id,), daemon=True).start()

    def notify_finished(self, session_id: str):
        time.sleep(0.35)  # let the final message settle
        try:
            session = self.oc.session(session_id)
        except ApiError as exc:
            if exc.status != 404:
                log.warning("could not read session %s: %s", session_id, exc)
            return

        if session.get("parentID") and not self.config.get("includeSubagents"):
            return

        summary = ""
        try:
            summary = self.oc.recent_text(session_id)
        except ApiError as exc:
            log.debug("could not read recent messages for %s: %s", session_id, exc)

        title = session.get("title") or session_id
        if is_internal_session_title(str(title)):
            return
        failed = session.get("outcome") == "failed"
        self.enqueue(finished_text(title, summary, failed), session_id=session_id)

    def schedule_error(self, session_id: str, error):
        threading.Thread(target=self.notify_error, args=(session_id, error), daemon=True).start()

    def notify_error(self, session_id: str, error):
        title = session_id
        directory = None
        try:
            session = self.oc.session(session_id)
            title = session.get("title") or session_id
            directory = (session.get("location") or {}).get("directory")
        except ApiError:
            pass

        message = ""
        if isinstance(error, dict):
            message = error.get("message") or error.get("type") or json.dumps(error)
        elif error:
            message = str(error)

        lines = [f"\u26a0\ufe0f <b>OpenCode error</b> \u2014 {esc(title)}"]
        if directory:
            lines.append(f"\U0001f4c1 <code>{esc(directory)}</code>")
        if message:
            lines.append("")
            lines.append(esc(shorten(message, 500)))
        lines.append("")
        lines.append("\u21a9\ufe0f <i>Reply to this message with your next prompt.</i>")
        self.enqueue("\n".join(lines), session_id=session_id)

    def schedule_interrupted(self, session_id: str, reason):
        threading.Thread(
            target=self.notify_interrupted,
            args=(session_id, reason),
            daemon=True,
        ).start()

    def notify_interrupted(self, session_id: str, reason):
        title = session_id
        directory = None
        try:
            session = self.oc.session(session_id)
            title = session.get("title") or session_id
            directory = (session.get("location") or {}).get("directory")
        except ApiError:
            pass

        lines = [f"\u23f9\ufe0f <b>Task interrupted</b> \u2014 {esc(str(title))}"]
        if directory:
            lines.append(f"\U0001f4c1 <code>{esc(directory)}</code>")
        if reason:
            lines.append(f"Reason: {esc(str(reason))}")
        lines.append("")
        lines.append("\u21a9\ufe0f <i>Reply to this message with your next prompt.</i>")
        self.enqueue("\n".join(lines), session_id=session_id)

    def schedule_permission(self, data: dict):
        request_id = data.get("id")
        session_id = data.get("sessionID")
        if not request_id or not session_id:
            return
        with self.pending_lock:
            if request_id in self.perm_seen:
                return
            self.perm_seen.add(request_id)
        threading.Thread(
            target=self.notify_permission,
            args=(session_id, request_id, data),
            daemon=True,
        ).start()

    def notify_permission(self, session_id: str, request_id: str, data: dict):
        title = session_id
        try:
            session = self.oc.session(session_id)
            title = session.get("title") or session_id
        except ApiError:
            pass

        action = data.get("action") or "action"
        resources = data.get("resources") or []
        if isinstance(resources, str):
            resources = [resources]

        lines = [f"\U0001f510 <b>{esc(shorten(str(title), 100))}</b>"]
        message = data.get("message")
        if message:
            lines.append(esc(compact_text(str(message), 2, 220)))
        resource = resources[0] if resources else ""
        detail = f"<b>{esc(shorten(str(action), 60))}</b>"
        if resource:
            detail += f": <code>{esc(shorten(str(resource), 140))}</code>"
        if len(resources) > 1:
            detail += f" (+{len(resources) - 1})"
        lines.append(detail)

        token = secrets.token_hex(4)
        with self.pending_lock:
            now = time.time()
            for key, entry in list(self.perm_by_token.items()):
                if now - entry.get("created", 0) > 24 * 60 * 60:
                    self.perm_by_token.pop(key, None)
            self.perm_by_token[token] = {
                "sessionID": session_id,
                "requestID": request_id,
                "created": now,
            }
        self.persist_pending()

        keyboard = [
            [
                {"text": "\u2705 Allow once", "callback_data": f"perm:{token}:once"},
                {"text": "\u267e\ufe0f Always", "callback_data": f"perm:{token}:always"},
                {"text": "\u26d4 Reject", "callback_data": f"perm:{token}:reject"},
            ]
        ]
        self.enqueue("\n".join(lines), session_id=None, keyboard=keyboard, permission_token=token)

    # -- form questions ----------------------------------------------------

    def schedule_form(self, form: dict):
        form_id = str(form.get("id") or "")
        session_id = str(form.get("sessionID") or "")
        if not form_id or not session_id:
            return
        with self.pending_lock:
            if form_id in self.form_seen:
                return
            self.form_seen.add(form_id)
        threading.Thread(target=self.notify_form, args=(form,), daemon=True).start()

    def notify_form(self, form: dict):
        form_id = str(form.get("id") or "")
        session_id = str(form.get("sessionID") or "")
        try:
            session = self.oc.session(session_id)
            if is_internal_session_title(session.get("title") or ""):
                return
        except ApiError:
            pass
        try:
            detail = self.oc.form(session_id, form_id)
            if isinstance(detail, dict) and detail.get("fields"):
                form = {**form, **detail}
        except ApiError as exc:
            if exc.status != 404:
                log.debug("could not read form %s: %s", form_id, exc)

        state = form.get("state") or {}
        if isinstance(state, dict) and state.get("status") not in (None, "pending"):
            return

        with self.pending_lock:
            if form_id in self.form_closed:
                return

        fields = [field for field in (form.get("fields") or []) if isinstance(field, dict)]
        if not fields:
            return

        token = secrets.token_hex(4)
        entry = {
            "sessionID": session_id,
            "formID": form_id,
            "form": form,
            "fields": fields,
            "answers": {},
            "index": 0,
            "created": time.time(),
        }
        with self.pending_lock:
            self.form_by_token[token] = entry
        self.persist_pending()

        if not self.send_form_field(token, entry):
            with self.pending_lock:
                self.form_by_token.pop(token, None)
            self.persist_pending()

    def send_form_field(self, token: str, entry: dict) -> bool:
        fields = entry["fields"]
        while entry["index"] < len(fields):
            field = fields[entry["index"]]
            if not self.form_field_visible(field, entry["answers"]):
                entry["index"] += 1
                continue
            text, keyboard = self.form_question(token, field)
            self.enqueue(text, keyboard=keyboard, form_token=token)
            return True
        return False

    @staticmethod
    def form_field_visible(field: dict, answers: dict) -> bool:
        if field.get("hidden"):
            return False
        for condition in field.get("when") or []:
            key = condition.get("key")
            if key not in answers:
                return False
            actual = answers.get(key)
            expected = condition.get("value")
            if condition.get("op") == "eq" and actual != expected:
                return False
            if condition.get("op") == "neq" and actual == expected:
                return False
        return True

    def form_question(self, token: str, field: dict):
        title = field.get("title") or field.get("key") or "Question"
        lines = [f"\u2753 <b>{esc(shorten(str(title), 80))}</b>"]
        description = field.get("description") or ""
        if description:
            lines.append(esc(compact_text(str(description), 2, 220)))
        lines.append("\u21a9\ufe0f Tap an option or reply to this message.")

        keyboard = []
        field_type = field.get("type")
        options = field.get("options") or []
        if field_type == "boolean":
            keyboard.append(
                [
                    {"text": "Yes", "callback_data": f"form:{token}:bool:1"},
                    {"text": "No", "callback_data": f"form:{token}:bool:0"},
                ]
            )
        elif options and field_type in ("string", "multiselect"):
            row = []
            for index, option in enumerate(options[:8]):
                label = option.get("label") or option.get("value") or str(index)
                row.append(
                    {
                        "text": shorten(str(label), 40),
                        "callback_data": f"form:{token}:opt:{index}",
                    }
                )
                if len(row) == 2:
                    keyboard.append(row)
                    row = []
            if row:
                keyboard.append(row)

        keyboard.append([{"text": "\u26d4 Cancel", "callback_data": f"form:{token}:cancel"}])
        return "\n".join(lines), keyboard

    def handle_form_callback(self, callback: dict, data: str):
        callback_id = callback.get("id")
        message = callback.get("message") or {}
        message_id = message.get("message_id")
        parts = data.split(":")
        if len(parts) < 3:
            if callback_id:
                self.tg.answer_callback(callback_id, "Invalid action")
            return

        token = parts[1]
        action = parts[2]
        if action == "cancel":
            self.cancel_form(token, message_id)
            if callback_id:
                self.tg.answer_callback(callback_id, "Cancelled")
            return

        with self.pending_lock:
            entry = self.form_by_token.get(token)
        if not entry or entry.get("index", 0) >= len(entry.get("fields") or []):
            if callback_id:
                self.tg.answer_callback(callback_id, "This question has expired")
            if message_id:
                self.tg.edit(message_id, "\u231b This question has expired.")
            return

        field = entry["fields"][entry["index"]]
        if action == "bool" and len(parts) > 3:
            value = parts[3] == "1"
        elif action == "opt" and len(parts) > 3:
            try:
                option = (field.get("options") or [])[int(parts[3])]
            except (ValueError, IndexError):
                if callback_id:
                    self.tg.answer_callback(callback_id, "Invalid option")
                return
            value = option.get("value") or option.get("label") or parts[3]
            if field.get("type") == "multiselect":
                value = [value]
        else:
            if callback_id:
                self.tg.answer_callback(callback_id, "Invalid action")
            return

        self.record_form_answer(token, entry, message_id, value)
        if callback_id:
            self.tg.answer_callback(callback_id, "Answered")

    def cancel_form(self, token: str, message_id=None):
        with self.pending_lock:
            entry = self.form_by_token.pop(token, None)
            if entry:
                self.form_closed.add(entry["formID"])
            for mapped_message, mapped_token in list(self.form_by_message.items()):
                if mapped_token == token:
                    self.form_by_message.pop(mapped_message, None)
        self.persist_pending()
        if not entry:
            if message_id:
                self.tg.edit(message_id, "\u231b This question has expired.")
            return
        try:
            self.oc.cancel_form(entry["sessionID"], entry["formID"])
        except ApiError as exc:
            if exc.status not in (404, 409):
                if message_id:
                    self.tg.edit(message_id, f"\u26a0\ufe0f Could not cancel: HTTP {exc.status}")
                return
        if message_id:
            self.tg.edit(message_id, "\u26d4 Question cancelled.")

    def record_form_answer(self, token: str, entry: dict, message_id, value):
        if entry.get("index", 0) >= len(entry.get("fields") or []):
            return
        field = entry["fields"][entry["index"]]
        entry["answers"][field.get("key")] = value
        entry["index"] += 1

        if message_id:
            label = field.get("title") or field.get("key") or "Question"
            self.tg.edit(message_id, f"\u2705 {esc(shorten(str(label), 80))}")

        if self.send_form_field(token, entry):
            return

        try:
            self.oc.reply_form(entry["sessionID"], entry["formID"], entry["answers"])
        except ApiError as exc:
            if message_id:
                self.tg.edit(message_id, f"\u26a0\ufe0f Could not send answer: HTTP {exc.status}")
            with self.pending_lock:
                self.form_by_token.pop(token, None)
            self.persist_pending()
            return

        with self.pending_lock:
            self.form_by_token.pop(token, None)
            self.form_closed.add(entry["formID"])
            for mapped_message, mapped_token in list(self.form_by_message.items()):
                if mapped_token == token:
                    self.form_by_message.pop(mapped_message, None)
        self.persist_pending()
        if message_id:
            self.tg.edit(message_id, "\u2705 Answer sent.")

    def handle_form_text(self, token: str, message_id, text: str):
        with self.pending_lock:
            entry = self.form_by_token.get(token)
        if not entry or entry.get("index", 0) >= len(entry.get("fields") or []):
            self.tg.send("\u231b This question has already been answered or expired.")
            return

        field = entry["fields"][entry["index"]]
        value, error = self.parse_form_value(field, text)
        if error:
            self.tg.send(error)
            return
        self.record_form_answer(token, entry, message_id, value)

    @staticmethod
    def parse_form_value(field: dict, text: str):
        raw = text.strip()
        if not raw:
            return None, "Answer cannot be empty."

        field_type = field.get("type")
        if field_type == "boolean":
            normalized = raw.casefold()
            if normalized in ("yes", "y", "true", "1", "ok"):
                return True, None
            if normalized in ("no", "n", "false", "0"):
                return False, None
            return None, "Reply with yes or no."

        if field_type == "integer":
            try:
                return int(raw), None
            except ValueError:
                return None, "Reply with a whole number."

        if field_type == "number":
            try:
                return float(raw), None
            except ValueError:
                return None, "Reply with a number."

        options = field.get("options") or []

        def match_option(item: str):
            for option in options:
                label = str(option.get("label") or "")
                value = str(option.get("value") or "")
                if item.casefold() in (label.casefold(), value.casefold()):
                    return option.get("value") or option.get("label") or item
            return None

        if field_type == "multiselect":
            answers = []
            for item in (part.strip() for part in raw.split(",")):
                if not item:
                    continue
                value = match_option(item)
                if value is None:
                    if not field.get("custom"):
                        return None, "Choose from the options, separated by commas."
                    value = item
                answers.append(value)
            if not answers:
                return None, "Choose at least one option."
            return answers, None

        if field_type == "external":
            return None, "This question must be answered in the OpenCode TUI."

        if options:
            value = match_option(raw)
            if value is not None:
                return value, None
            if not field.get("custom"):
                return None, "Choose one of the options."

        return raw, None

    def clear_form(self, form_id):
        form_id = str(form_id or "")
        if not form_id:
            return
        changed = False
        with self.pending_lock:
            self.form_closed.add(form_id)
            for token, entry in list(self.form_by_token.items()):
                if entry.get("formID") == form_id:
                    self.form_by_token.pop(token, None)
                    changed = True
                    for mapped_message, mapped_token in list(self.form_by_message.items()):
                        if mapped_token == token:
                            self.form_by_message.pop(mapped_message, None)
        if changed:
            self.persist_pending()

    @staticmethod
    def last_assistant_text(messages) -> str:
        if not isinstance(messages, list):
            return ""
        last_user = -1
        for index, message in enumerate(messages):
            if message.get("type") == "user":
                last_user = index
        relevant = messages[last_user + 1 :] if last_user >= 0 else messages
        for message in reversed(relevant):
            if message.get("type") != "assistant":
                continue
            for part in reversed(message.get("content") or []):
                if part.get("type") == "text" and (part.get("text") or "").strip():
                    return part["text"].strip()
        return ""

    def enqueue(self, text: str, session_id=None, keyboard=None, permission_token=None, form_token=None):
        if self.tg.chat_id is None:
            log.info("no chat paired yet; dropping notification")
            return
        # Completion/error notifications carry the last known DeepSeek balance.
        # Forms and permissions stay short and do not get the footer.
        if session_id is not None:
            footer = balance_footer()
            if footer:
                text = f"{text}\n\n{footer}"
        self.outbox.put((text, session_id, keyboard, permission_token, form_token))

    # -- Telegram ---------------------------------------------------------

    def telegram_loop(self):
        offset = int(self.state.get("updateOffset") or 0)
        if not offset:
            # Do not replay history on first start: confirm the latest update.
            try:
                latest = self.tg.get_updates(-1, timeout=0)
                if latest:
                    offset = int(latest[-1]["update_id"]) + 1
            except TelegramError as exc:
                log.warning("could not read Telegram backlog: %s", exc)

        while not self.stop.is_set():
            try:
                updates = self.tg.get_updates(offset, timeout=30)
            except TelegramError as exc:
                log.warning("telegram poll failed: %s", exc)
                self.stop.wait(3)
                continue
            except Exception as exc:  # noqa: BLE001 - keep polling
                log.warning("telegram poll error: %s", exc)
                self.stop.wait(3)
                continue

            for update in updates:
                update_id = int(update.get("update_id", 0))
                seen = self.state.setdefault("seenUpdates", [])
                if update_id and update_id in seen:
                    offset = max(offset, update_id + 1)
                    continue
                try:
                    self.handle_update(update)
                except Exception as exc:  # noqa: BLE001 - keep polling
                    log.exception("could not handle update: %s", exc)
                if update_id:
                    with self.state_lock:
                        seen.append(update_id)
                    offset = max(offset, update_id + 1)
                    with self.state_lock:
                        self.state["updateOffset"] = offset
                    self.save_state()

    def handle_update(self, update: dict):
        if "callback_query" in update:
            self.handle_callback(update["callback_query"])
            return
        message = update.get("message") or update.get("edited_message")
        if not message:
            return

        chat = message.get("chat") or {}
        chat_id = chat.get("id")
        if chat_id is None:
            return

        if self.tg.chat_id is None:
            if chat.get("type") != "private":
                return
            self.config["chatID"] = chat_id
            self.tg.chat_id = chat_id
            save_json(CONFIG_PATH, self.config)
            log.info("paired with chat %s", chat_id)
            self.tg.send(
                f"Paired with this chat (ID <code>{chat_id}</code>).\n\n{HELP_TEXT}",
                chat_id=chat_id,
            )
        elif chat_id != self.tg.chat_id:
            log.info("ignoring message from unauthorized chat %s", chat_id)
            return

        text = (message.get("text") or "").strip()
        if not text:
            return
        log.info(
            "telegram inbound message%s",
            " (reply)" if message.get("reply_to_message") else "",
        )

        if text.startswith("/"):
            self.handle_command(text, message)
            return

        self.handle_prompt(text, message)

    def handle_callback(self, callback: dict):
        callback_id = callback.get("id")
        data = callback.get("data") or ""
        message = callback.get("message") or {}
        message_id = message.get("message_id")

        if self.tg.chat_id is None or (message.get("chat") or {}).get("id") != self.tg.chat_id:
            return

        if data.startswith("form:"):
            parts = data.split(":")
            log.info(
                "telegram form callback action=%s",
                parts[2] if len(parts) > 2 else "unknown",
            )
            self.handle_form_callback(callback, data)
            return

        if not data.startswith("perm:"):
            if callback_id:
                self.tg.answer_callback(callback_id, "Unknown action")
            return

        try:
            _, token, decision = data.split(":", 2)
        except ValueError:
            if callback_id:
                self.tg.answer_callback(callback_id, "Invalid action")
            return

        with self.pending_lock:
            entry = self.perm_by_token.get(token)
        if not entry:
            if callback_id:
                self.tg.answer_callback(callback_id, "This request has expired")
            if message_id:
                self.tg.edit(message_id, "\u231b This permission request has expired.")
            return

        session_id = entry["sessionID"]
        request_id = entry["requestID"]

        try:
            self.oc.reply_permission(session_id, request_id, decision)
            with self.pending_lock:
                self.perm_by_token.pop(token, None)
            label = {"once": "Allowed once", "always": "Always allowed", "reject": "Rejected"}[decision]
            if callback_id:
                self.tg.answer_callback(callback_id, label)
            if message_id:
                self.tg.edit(message_id, f"\u2705 {label}.")
        except ApiError as exc:
            if exc.status in (404, 409):
                with self.pending_lock:
                    self.perm_by_token.pop(token, None)
            if callback_id:
                self.tg.answer_callback(callback_id, "Already handled")
            if message_id:
                self.tg.edit(
                    message_id,
                    "\u231b This permission request was already handled or has expired."
                    if exc.status in (404, 409)
                    else f"\u26a0\ufe0f Could not reply: HTTP {exc.status}",
                )

    def handle_command(self, text: str, message: dict):
        command, _, argument = text.partition(" ")
        command = command.split("@", 1)[0].lower()
        argument = argument.strip()

        if command in ("/start", "/help"):
            self.tg.send(HELP_TEXT)
            return

        if command == "/id":
            self.tg.send(f"Chat ID: <code>{self.tg.chat_id}</code>")
            return

        if command == "/sessions":
            self.show_sessions()
            return

        if command == "/use":
            self.use_session(argument)
            return

        if command == "/status":
            self.show_status()
            return

        if command == "/pending":
            threading.Thread(target=self.recover_pending, daemon=True).start()
            self.tg.send("Checking for pending questions\u2026")
            return

        if command == "/stop":
            self.stop_session()
            return

        self.tg.send("Unknown command.\n\n" + HELP_TEXT)

    def show_sessions(self):
        try:
            sessions = self.oc.request("GET", "/api/session?limit=10") or []
        except ApiError as exc:
            self.tg.send(f"\u26a0\ufe0f Could not list sessions: HTTP {exc.status}")
            return

        if not sessions:
            self.tg.send("No sessions found.")
            return

        choices = [str(item.get("id")) for item in sessions if item.get("id")]
        with self.state_lock:
            self.state["sessionChoices"] = choices
        self.save_state()

        last = self.get_last_session()
        lines = ["<b>Recent sessions</b>"]
        for index, session in enumerate(sessions, start=1):
            title = session.get("title") or session.get("id")
            marker = " \u2b05\ufe0f" if str(session.get("id")) == str(last) else ""
            directory = (session.get("location") or {}).get("directory") or ""
            lines.append(f"{index}. {esc(str(title))}{marker}")
            if directory:
                lines.append(f"   <code>{esc(directory)}</code>")
        lines.append("")
        lines.append("Select one with /use N.")
        self.tg.send("\n".join(lines))

    def use_session(self, argument: str):
        with self.state_lock:
            choices = list(self.state.get("sessionChoices") or [])
        try:
            index = int(argument) - 1
        except (TypeError, ValueError):
            index = -1
        if index < 0 or index >= len(choices):
            self.tg.send("Usage: /use N (run /sessions first)")
            return

        session_id = choices[index]
        self.set_last_session(session_id)
        try:
            session = self.oc.session(session_id)
            title = session.get("title") or session_id
        except ApiError:
            title = session_id
        self.tg.send(f"Now using <b>{esc(str(title))}</b>.\nPlain messages go to this session.")

    def show_status(self):
        session_id = self.get_last_session()
        lines = []

        if session_id:
            try:
                session = self.oc.session(session_id)
                title = session.get("title") or session_id
                directory = (session.get("location") or {}).get("directory") or ""
                outcome = session.get("outcome") or "unknown"
                lines.append(f"<b>Selected:</b> {esc(str(title))}")
                if directory:
                    lines.append(f"\U0001f4c1 <code>{esc(directory)}</code>")
                lines.append(f"Last outcome: {esc(str(outcome))}")
            except ApiError as exc:
                if exc.status == 404:
                    lines.append("Selected session no longer exists.")
                    self.set_last_session(None)
                else:
                    lines.append(f"Selected session: HTTP {exc.status}")
        else:
            lines.append("No session selected. Use /sessions and /use N.")

        try:
            active = self.oc.request("GET", "/api/session/active") or {}
            if isinstance(active, dict) and active:
                lines.append("")
                lines.append("<b>Running now</b>")
                for sid in list(active)[:5]:
                    try:
                        session = self.oc.session(sid)
                        title = session.get("title") or sid
                    except ApiError:
                        title = sid
                    lines.append(f"\u2022 {esc(str(title))}")
        except ApiError:
            pass

        self.tg.send("\n".join(lines))

    def stop_session(self):
        session_id = self.get_last_session()
        if not session_id:
            self.tg.send("No session selected. Use /sessions and /use N.")
            return
        try:
            self.oc.interrupt(session_id)
            self.tg.send("\u23f9\ufe0f Interrupt requested.")
        except ApiError as exc:
            self.tg.send(f"\u26a0\ufe0f Could not interrupt: HTTP {exc.status}")

    def handle_prompt(self, text: str, message: dict):
        reply_to = message.get("reply_to_message") or {}
        reply_to_id = reply_to.get("message_id")

        if reply_to_id is not None:
            with self.pending_lock:
                form_token = self.form_by_message.get(str(reply_to_id))
                permission_token = self.perm_by_message.get(str(reply_to_id))
            if form_token:
                with self.pending_lock:
                    live = form_token in self.form_by_token
                if not live:
                    self.tg.send(
                        "\u231b This question has expired. Wait for a new question notification."
                    )
                    return
                self.handle_form_text(form_token, reply_to_id, text)
                return
            if permission_token:
                self.handle_permission_text(permission_token, reply_to_id, text)
                return

        session_id = self.session_for_message(reply_to_id) if reply_to_id is not None else None
        if not session_id:
            session_id = self.get_last_session()
        if not session_id:
            self.tg.send(
                "No session selected yet. Wait for a task notification and reply to it, "
                "or use /sessions then /use N."
            )
            return

        log.info("telegram prompt -> %s (%d chars)", session_id, len(text))
        try:
            self.oc.prompt(session_id, text)
        except ApiError as exc:
            if exc.status == 404:
                self.tg.send("\u26a0\ufe0f That session no longer exists.")
            else:
                self.tg.send(f"\u26a0\ufe0f Could not send the prompt: HTTP {exc.status}")
            return

        self.set_last_session(session_id)
        if self.config.get("replyConfirmation", True):
            try:
                session = self.oc.session(session_id)
                title = session.get("title") or session_id
            except ApiError:
                title = session_id
            self.tg.send(f"\U0001f4e8 Sent to <b>{esc(str(title))}</b>.")

    def handle_permission_text(self, token: str, message_id: int, text: str):
        normalized = text.strip().lower()
        decision = None
        if normalized in ("yes", "y", "ok", "okay", "allow", "once", "1"):
            decision = "once"
        elif normalized in ("always", "all", "2"):
            decision = "always"
        elif normalized in ("no", "n", "deny", "reject", "3"):
            decision = "reject"

        if decision is None:
            self.tg.send(
                "Reply with <b>yes</b>, <b>always</b>, or <b>no</b> to this permission request."
            )
            return

        with self.pending_lock:
            entry = self.perm_by_token.get(token)
        if not entry:
            self.tg.send("\u231b This permission request has expired.")
            return

        try:
            self.oc.reply_permission(entry["sessionID"], entry["requestID"], decision)
            with self.pending_lock:
                self.perm_by_token.pop(token, None)
            label = {"once": "Allowed once", "always": "Always allowed", "reject": "Rejected"}[decision]
            self.tg.edit(message_id, f"\u2705 {label}.")
        except ApiError as exc:
            if exc.status in (404, 409):
                with self.pending_lock:
                    self.perm_by_token.pop(token, None)
            self.tg.send(
                "\u231b Already handled or expired."
                if exc.status in (404, 409)
                else f"\u26a0\ufe0f Could not reply: HTTP {exc.status}"
            )


# ---------------------------------------------------------------------------
# entrypoint


def check(config: dict) -> int:
    if not config.get("botToken"):
        print(f"No bot token in {CONFIG_PATH}")
        return 2

    tg = Telegram(config["botToken"], config.get("chatID"))
    try:
        me = tg.call("getMe")
        print(f"Telegram bot: @{me.get('username')} ({me.get('first_name')})")
    except TelegramError as exc:
        print(f"Telegram check failed: {exc}")
        return 1

    try:
        server = resolve_server(config)
        client = OpenCode(server, config)
        info = client.request("GET", "/api/info")
        print(f"OpenCode server: {server} ({info})")
        sessions = client.request("GET", "/api/session?limit=1") or []
        if sessions:
            session_id = sessions[0].get("id")
            summary = client.recent_text(session_id)
            print(f"Last session: {sessions[0].get('title') or session_id}")
            print(f"Last text chars: {len(summary)}")
    except Exception as exc:  # noqa: BLE001 - diagnostic mode
        print(f"OpenCode check failed: {exc}")
        return 1

    if config.get("chatID"):
        print(f"Paired chat: {config['chatID']}")
    else:
        print("Not paired yet: message the bot from Telegram and it will pair automatically.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="OpenCode to Telegram bridge")
    parser.add_argument("--check", action="store_true", help="validate configuration and exit")
    parser.add_argument("--version", action="store_true", help="print the version and exit")
    parser.add_argument("--verbose", action="store_true", help="enable debug logging")
    args = parser.parse_args()

    logging.basicConfig(
        level=logging.DEBUG if args.verbose else logging.INFO,
        format="%(asctime)s %(levelname)s %(message)s",
    )

    if args.version:
        print(VERSION)
        return 0

    config = load_config()
    if args.check:
        return check(config)

    bridge = Bridge(config)
    try:
        bridge.run()
    except KeyboardInterrupt:
        bridge.shutdown()
    return 0


if __name__ == "__main__":
    sys.exit(main())
