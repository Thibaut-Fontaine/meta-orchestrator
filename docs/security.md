# mox security model

## Trust boundaries

- The owner is trusted. Agents are **untrusted** and may be steered by content
  they read.
- The workstation is the source of truth for local mode. Remote mode copies the
  project to a VPS approved by the owner.
- `mox` never reads provider tokens to use them itself; it delegates `gh` auth to
  the existing keyring and OpenCode auth to the environment.

## Agent confinement

1. **OpenCode permissions** (`opencode.json`): external directories denied;
   `.env`, keys, `~/.ssh`, `~/.aws`, gh/opencode config denied; dangerous bash
   patterns denied.
2. **PATH wrappers** (`mox __guard`): `gh` limited to repo view/pr/issue/release;
   force-push, remote changes, clone, global config, ssh/scp, signals and service
   management refused.
3. **Prompt and AGENTS.md rules**: TDD, parallel safety, private repo, no
   secrets, no unrelated repositories.
4. **Pre-commit hook**: rejects files >2 MiB and obvious secret patterns.

Wrappers are defence in depth, not a kernel sandbox: an absolute path can bypass
them. OpenCode permissions are the stronger layer for tool calls.

## Remote rules

- One-time human approval per project (`mox vps allow`).
- No SSH keys, tokens or model credentials are copied to the VPS.
- The VPS needs its own `opencode` and model auth.
- `mox bot` must never be launched on a remote host. `mox bot` refuses when it
  detects a remote context (`MOX_REMOTE_RUN`, `.mox/remote.owner`, `mox-remote`
  path) unless a human passes `--allow-remote` or sets `allow_remote_bots`.
- Detached runs are recorded with a remote pid. `mox vps ps` audits the VPS and
  marks untracked mox processes as `GHOST` and bot processes as `BOT`.
  `mox vps cleanup --apply --yes` kills only untracked `mox-remote`/bot pids;
  it never touches tracked runs or unrelated processes.
- Remote completion is only final after a fast-forward merge and local
  verification.

## Secrets

- Telegram tokens and secret webhook URLs are read from environment/config and
  written only to 0600 curl config files, never argv.
- Runtime logs, state, checks, prompts and caches are 0700/0600.
- Bot chat IDs are allowlisted; the bot is read-only.

## Recovery snapshots

`RECOVERY.md` files are 0600, contain no tokens/answers/logs, and are pushed to
the VPS so a destroyed workstation does not erase the operational picture. They
are gitignored and never part of a commit. `mox recovery --check` reports stale
or missing remote copies.

## Known limits

- A determined agent with access to absolute binaries can bypass PATH wrappers.
- Remote processes are bounded by a remote `timeout`; cleanup is explicit.
- Logs may contain model output; they are private but not redacted.
