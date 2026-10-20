# Changelog

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
