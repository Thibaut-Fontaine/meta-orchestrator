#!/usr/bin/env python3
"""Interactive setup for the OpenCode Telegram bridge."""

from __future__ import annotations

import argparse
import getpass
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from bridge import (  # noqa: E402
    CONFIG_PATH,
    Telegram,
    TelegramError,
    load_config,
    load_json,
    save_json,
)


def main() -> int:
    parser = argparse.ArgumentParser(description="Configure the OpenCode Telegram bridge")
    parser.add_argument("--token", help="Telegram bot token (otherwise prompted)")
    parser.add_argument("--chat-id", type=int, help="Telegram chat ID (optional; pairs on first message)")
    parser.add_argument("--server", help="OpenCode server URL (optional)")
    parser.add_argument("--start", action="store_true", help="enable and start the systemd user service")
    args = parser.parse_args()

    config = load_config()

    token = (args.token or getpass.getpass("Telegram bot token (from @BotFather): ")).strip()
    if not token:
        print("A bot token is required.", file=sys.stderr)
        return 2

    telegram = Telegram(token)
    try:
        me = telegram.call("getMe")
    except TelegramError as exc:
        print(f"Telegram rejected the token: {exc}", file=sys.stderr)
        return 1

    config["botToken"] = token
    if args.chat_id:
        config["chatID"] = args.chat_id
    if args.server:
        config["server"] = args.server.rstrip("/")

    # Keep any existing notification preferences.
    previous = load_json(CONFIG_PATH, {})
    if "notify" in previous:
        config["notify"] = previous["notify"]

    save_json(CONFIG_PATH, config)

    print(f"Saved {CONFIG_PATH} (mode 600).")
    print(f"Bot: @{me.get('username')} ({me.get('first_name')})")
    print()

    if args.start:
        print("Starting the bridge...")
        subprocess.run(["systemctl", "--user", "daemon-reload"], check=False)
        result = subprocess.run(
            ["systemctl", "--user", "enable", "--now", "opencode-telegram-bridge.service"],
            check=False,
        )
        if result.returncode == 0:
            print("Bridge started.")
        else:
            print("Could not start the service automatically; see the commands below.")
        print()

    print("Next steps:")
    if not args.start:
        print("  1. Start the bridge:")
        print("       systemctl --user daemon-reload")
        print("       systemctl --user enable --now opencode-telegram-bridge.service")
        step = 2
    else:
        step = 1
    print(f"  {step}. Open Telegram, start a chat with the bot, and send /start.")
    print("     The first private chat is paired automatically.")
    print(f"  {step + 1}. Run a task in OpenCode. When it finishes, reply to the Telegram")
    print("     notification with your next prompt.")
    print()
    print("Check it with:")
    print("  python3 ~/.config/opencode/telegram-bridge/bridge.py --check")
    print("  journalctl --user -u opencode-telegram-bridge -f")
    return 0


if __name__ == "__main__":
    sys.exit(main())
