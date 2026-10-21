# Changelog

## 0.10.0

- Project-level assessment before implementation: difficulty, planned time and a
  total token budget for the current estimate model (`deepseek-v4.1-flash` by
  default).
- `mox plan` shows difficulty/time/tokens; `mox up` and `TASK.md` repeat them.
- Token estimates come from the planning brain, with a deterministic fallback of
  2,000 tokens per planned minute (clamped to 2k–50M).
- `estimate_model` / `MOX_ESTIMATE_MODEL`; adopted projects get a fallback
  assessment from their milestones.
- Plan approval gate: `mox up` refuses an unapproved plan, shows the assessment
  and milestones, and asks `y/N`; `mox plan --approve` approves after review and
  `mox up --yes` bypasses for automation. Re-planning or adopting resets approval.
- Explicit `--max-parallel N` now overrides the compute-aware concurrency cap,
  so a heavy project no longer serializes every other selected project.
- Every project gets `MANUAL.md` and a "Continue without mox" README section;
  `AGENTS.md` documents direct `MILESTONES.json` edits when mox is unavailable.

## 0.9.0

- Disaster recovery: per-project and global `RECOVERY.md` on the VPS, written by
  `mox milestone`, pushed before/during/after runs, with milestones, todos,
  checks, repo URL and resume steps. No secrets.
- `mox recovery [--push|--check]`; configurable sync interval.

## 0.8.0

- Checks recurrence, read-only VPS probes, `--context` path/commit.
- Project adopt; detach/sync watch; disk watchdog; log retention; events.jsonl;
  max attempts/backoff.
- Second planning pass (`mox plan --deep`), `TASKS.md`, init profiles,
  completions, man page, systemd units, property tests, release workflow.
- Opt-in auto-reflection after successful runs.
- No mox code TODO remains; only owner-side OpenCode setup on the VPS.

## 0.7.0

- Ghost wall: `mox bot` refuses remote contexts unless explicitly allowed;
  `mox vps ps` audits remote processes and flags `GHOST`/`BOT`;
  `mox vps cleanup --apply --yes` removes only untracked mox processes.
- Detached VPS runs (`--detach`) survive the workstation shutting down;
  `mox vps sync` fetches board, log, bundle and merges.
- Read-only Telegram bot with project navigation, milestones, todos, checks,
  questions, ETA, logs and report.
- Retry backoff, `project remove` alias, architecture/security docs, release workflow.

## 0.6.0

- Telegram bot, detached VPS runs, remote process audit/wall.

## 0.5.0

- P0 hardening: process-group shutdown, stale detection, OpenCode permissions,
  pre-commit hook, rolling pool, resume/retry, compute/RAM admission.
- Reports, project archive, ideas hash, owner, checks extras, stats p90/reset,
  CI.

## 0.4.0

- Host CPU topology/GHz, cross-project stats and first-milestone priors,
  `build.sh`.

## 0.3.0

- Milestones with sub-todos, complexity weights, calibrated live ETA.

## 0.2.0

- Locks, private permissions, post-run verification, guard wrappers,
  VPS approval ledger, wall-clock cap, Telegram/desktop/webhook notifications.

## 0.1.0

- Initial orchestrator: plan/up/status/logs/clean, private repos, parallel
  agents with core pinning, disk safety, human check queue.
