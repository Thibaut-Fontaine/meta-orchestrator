# ROADMAP — meta-orchestrator (`mox`)

Status: **v0.6.0** · 287 unit tests + 8 end-to-end tests · private repo,
`main` pushed.

## Decisions locked in (2026-10-20)

| Question | Decision | Implemented |
|---|---|---|
| VPS access from agents | Only through `mox`; direct `ssh`/`scp` is refused by guard wrappers | ✅ |
| Git workflow | Solo agent pushes `main`; an agent that spawns sub-agents uses a branch + PR | ✅ prompt + `AGENTS.md` |
| Development method | TDD, mandatory order: failing test → code → tests/fmt/lint → commit | ✅ prompt + `AGENTS.md` |
| VPS deployment | Explicit target, gated by a **one-time human approval** (`mox vps allow <slug>`) | ✅ ledger, remote execution in M4 |
| Parallel awareness | Every agent is told other agents/scripts run locally and on the VPS; never interfere or communicate | ✅ prompt + `AGENTS.md` |
| Wall-clock cap | Per-run cap, default 7200 s, `--run-timeout` override, duration recorded | ✅ |
| Notifications | Telegram bot + optional desktop/webhook; token only via env + 0600 curl config | ✅ |
| Reminder delivery | `mox check notify` one-shot digest, suitable for a systemd **user** timer | ✅ |
| Project tracking | Ordered milestones with sub-todos, complexity weights, estimates, transition times and a calibrated live ETA | ✅ |
| Machine-aware planning | CPU model, physical/logical cores, max GHz and RAM are passed to the planner; algorithmic complexity + input scale inform estimates | ✅ |
| Cross-project statistics | Completed milestone timings are stored under a lock; median/p90 actual/planned ratios seed the first-milestone ETA | ✅ |
| VPS execution | Explicit `--target vps`, one-time approval, rsync mirror, ssh run, git-bundle fetch-back, OVH checklist | ✅ code (VPS needs opencode + auth) |
| Always-on remote | `--detach` starts a setsid run that survives the workstation; `mox vps sync` fetches progress/results | ✅ code |
| Telegram bot | Read-only long-poll bot: projects, milestones, todos, checks, questions, ETA, logs, report | ✅ |
| Easy build | `./build.sh` (release, `--test`, `--debug`, `--install`, `--offline`) | ✅ |

This document is the honest gap analysis and the plan from v0.1 to v1.0. It is
ordered by risk first, then by the original product vision.

---

## 1. Where we are

| Capability | State |
|---|---|
| Ideas → brain questions → answers → specs | ✅ one planning pass, cached plan |
| Parallel OpenCode agents in separate folders | ✅ waves of `max_parallel`, one session each |
| Private GitHub repos via `gh` | ✅ `--private` only, visibility verified, no reuse |
| Scaffolding and agent rules | ✅ `AGENTS.md`, `TASK.md`, `CHECKS.md`, `.gitignore` |
| CPU isolation | ✅ disjoint core sets, `nice`, `ionice`, per-agent caches |
| Disk safety | ✅ floor check, per-project soft budget, dry-run `clean` |
| Human check queue | ✅ NOW/TODAY/SOON/LATER, `--watch`, JSON, agent-written `CHECKS.md` |
| VPS **read-only** awareness | ✅ probe and local/VPS/OVH recommendation |
| VPS **execution** | ❌ not implemented (recommendation only) |
| OVH remote provisioning | ❌ deliberately manual (suggestion only) |
| Agent sandboxing | ❌ prompt-only rules + `--auto` |

Safety invariants covered by tests: repositories are always private, existing
repos are never reused, `clean` is allowlisted/dry-run, `stop` checks the process
before signalling, VPS access is read-only.

---

## 2. Known gaps, by severity

**Resolved in v0.5.0:** all P0 items, P1 items 9–16, and P2 items 16–19 and 21
(partly). The list below is kept as the original gap analysis; remaining work is
tracked under “Residual after v0.5.0”.

### P0 — correctness and safety (must fix before regular use)

1. **No lock between `mox` invocations.** Two `mox up` or an `up` plus a `clean`
   can race on `state.json`, `checks.json` and pid files. Need an exclusive
   lock file with a clear "already running" error, released on exit.
2. **Runtime files are world-readable.** `logs/`, `run/`, `checks.json`,
   `state.json` are created with the default umask (files 0644, dirs 0755).
   Agent logs can contain secrets or private data. Need `0700` dirs and `0600`
   files (`.mox`, logs, prompts, state, checks).
3. **Guardrails are prompt-only.** With `auto_approve = true` an agent can run
   any command: create a **public** repo with `gh`, push with `--force`, SSH to
   the VPS, run `rm`, read `~/.ssh` or `~/.config`. Prompt rules are not a
   boundary. Need an enforced policy (OpenCode permission config and/or a
   restricted `PATH` wrapper that rejects mutating `gh`/`ssh` commands).
4. **No post-run verification.** `mox` records the agent's exit code but never
   checks `git status`, unpushed commits, remote HEAD, or whether the agent
   actually followed the private-repo rule after starting.
5. **No resume / retry.** Re-running `mox up` relaunches **completed** projects
   and has no per-project retry/backoff. A failure loses the remaining queue.
6. **Batch barrier.** Agents run in strict waves; one long project blocks every
   later project in the next wave. Need a rolling worker pool.
7. **Compute need is ignored.** `ComputeNeed::preferred_cores` exists but the
   scheduler gives every project the same share. Heavy/GPU projects should get
   more cores (or be routed elsewhere).
8. **Interrupt handling.** Ctrl-C during `up` leaves state `running`, pid files
   behind and no summary. `stop` signals only the direct pid, not descendants.

### P1 — product gaps versus the original vision

9. **No VPS execution.** The VPS can only be inspected. There is no
   `mox up --target vps`, no remote bootstrap (git clone + opencode on the VPS),
   no remote log streaming, and no way to bring results back.
10. **OVH remote plans are only suggested.** No checklist, no provisioning, no
    "new ssh alias + rerun here" flow. (Deliberate: ordering is billing.)
11. **Planning is a single pass.** Answers are fed to the agent but the spec
    (summary, acceptance criteria, risks, task breakdown) is not re-synthesized
    after answers. No second self-reflection pass, no subtask plan (`TASKS.md`).
12. **Checks are UTC-only and passive.** No local timezone, recurrence
    ("every day"), remove/reopen/archive, ICS/Markdown export, or reminders when
    the user is not looking at the terminal.
13. **Project lifecycle is missing.** No `list`, `remove`/`archive`, `adopt`
    existing directory, rename, or surfacing of the agent's `STATUS.md`.
14. **No source-change detection.** Editing `ideas.txt` and running `mox plan`
    silently reuses the cached plan unless `--refresh` is passed.
15. **Org repositories unsupported.** `gh repo create <slug>` always creates
    under the authenticated user; no `--owner ORG` / `OWNER/NAME`.

### P2 — scale, quality, polish

16. **No CI.** No GitHub Actions for `fmt`/`clippy`/`test`, no coverage, no
    release artifacts, no shell completions, no man page.
17. **RAM is ignored.** 8 cores but ~6.7 GiB RAM; four agents can OOM. Admission
    should consider `MemAvailable`, not just cores.
18. **No budgets.** Per-project time is capped by one global timeout; no token
    or cost ceiling, no per-run summary of duration and exit reason.
19. **No observability.** No structured event log, no `mox report` across
    projects, no cost/duration trends, no auto-reflection after runs.
20. **Disk enforcement is pre-flight only.** No watchdog during long builds, no
    log rotation/retention, no automatic prune when approaching the floor.
21. **Single machine only.** No worker abstraction for a second machine or the
    VPS beyond read-only checks.

---

## 3. Milestones

Sizes are rough solo-with-agent estimates. Every behaviour change ships with
unit tests and, when it touches the workflow, an end-to-end test (project rule).

### M0 — v0.1.1 “Harden” (done in v0.2.0)

Goal: make the current tool safe to leave unattended.

- [x] Exclusive lock (`up.lock`) for `plan`/`up`/`clean --apply`, separate
      `checks.lock`, stale-owner reclaim. *(P0, S)*
- [x] `0700` directories and `0600` files for `.mox/`, logs, prompts, pid files,
      `state.json`, `checks.json`, `plan.json`, `answers.toml`. *(P0, S)*
- [x] Logs/prompts are private; no redaction of model output (documented risk).
      *(P0, S)*
- [x] Post-run verification: dirty tree and commits ahead of the upstream /
      `origin/main` mark the project `failed`. Missing upstream is reported as
      unverified, not failed. *(P0, M)*
- [x] `mox status` explicitly flags interrupted `running` entries as stale and
      suggests `mox stop`/`mox up --resume`. *(P0, S)*
- [x] Clean shutdown on Ctrl-C: SIGINT/SIGTERM kills the process group after a
      grace period, clears pid files, saves a `stopped` status. *(P0, M)*
- [x] Tests: lock contention, permissions, push verification, stale/cancelled runs.

Exit criteria: two concurrent `mox up` cannot corrupt state; logs are 0600; a
run that did not push is reported as failed.

### M1 — v0.2 “Guardrails” (partly done in v0.2.0)

Goal: turn prompt rules into enforceable boundaries.

- [ ] Generate a per-project OpenCode permission config (`opencode.json` or
      agent config) that denies edits outside the project directory and blocks
      destructive/remote commands (`gh repo delete|edit`, `gh api` mutations,
      `git push --force`, `rm -rf` outside cwd, `ssh`/`scp` except read-only
      probes). *(P0, L)*
- [ ] Put a **restricted `gh` wrapper** first on the agent's `PATH`: allow
      `repo view`, `issue`, `pr` read/create, `git` push to its own origin;
      reject `repo create --public`, `repo delete/edit/transfer`, `auth token`.
      Test the wrapper's argv handling directly. *(P0, M)*
- [ ] Optional read-only SSH wrapper for agents: only the `vps check` probe
      command is allowed; everything else is refused. *(P0, M)*
- [ ] Install a git `pre-commit` hook per project rejecting files >2 MiB and
      obvious secret patterns; test with fixtures. *(P1, M)*
- [ ] `mox doctor` verifies the guards are active and reports their paths.
      *(P1, S)*
- [x] Threat-model section in README: wrappers are defence in depth, not a
      kernel sandbox; absolute paths can still bypass them. *(P1, S)*

Exit criteria: a malicious prompt cannot create a public repo, force-push, or
run `ssh vps-ovh` through the wrappers; oversized/secret files are blocked at
commit time.

### M2 — v0.3 “Lifecycle & resumability” (3–4 days)

Goal: stop wasting tokens and wall-clock time; make projects manageable.

- [x] Rolling worker pool (no batch barrier): workers pull from one queue.
      *(P0, M)*
- [x] `mox up --resume` / `--retry-failed`; per-project attempt counter.
      Backoff is not implemented yet. *(P0, M)*
- [x] Compute-aware admission: heavy/GPU workloads reduce concurrency so each
      agent gets its preferred core share. *(P0, S)*
- [x] Wall-clock cap per run, configurable with `--run-timeout`, duration
      recorded and shown in `status`. *(P1, S)*
- [x] Milestone boards with sub-todos, complexity weights, algorithmic notes,
      estimates, transition timestamps and a calibrated ETA in `status`,
      `status --json`, `status --watch` and after each `up` wave. *(P1, M)*
- [ ] Post-answers **second brain pass**: synthesize final summary, acceptance
      criteria, risks and a `TASKS.md` checklist from questions + answers.
      `mox plan --refresh` re-runs it; `mox spec show --slug` prints it.
      *(P1, M)*
- [x] Changed-ideas detection via a content hash; stale cache is refused.
      *(P1, S)*
- [x] `mox status --verbose` (STATUS.md excerpts) and `mox report`.
      *(P1, M)*
- [~] `mox project list|archive` implemented (workspace moves to
      `.mox/archive`, repo untouched); `remove`/`adopt` pending. *(P1, M)*
- [x] `--owner ORG` for private repo creation. *(P1, S)*

Exit criteria: finishing one project immediately starts the next; `--resume`
launches only unfinished work; heavy projects get more cores.

### M3 — v0.4 “Checks 2.0 & reminders” (partly done in v0.2.0)

Goal: the attention queue becomes proactive.

- [~] Local time is used for ETA display; a configurable timezone and check
      timestamps in local time remain. *(P1, S)*
- [ ] Recurring checks (`every: 1d`) with next occurrence on completion; snooze
      history. *(P1, M)*
- [x] `mox check remove|reopen|export --format md|json` and `--top N`. *(P1, M)*
- [x] `mox check daemon --interval` periodic reminders. *(P2, S)*
- [x] Notifications: Telegram bot (token via env, never argv), desktop
      (`notify-send`) and JSON webhook; `mox notify status|test`.
      *(P1, M)*
- [x] `mox check notify` one-shot digest + systemd **user** timer example.
      *(P1, S)*
- [ ] **VPS probes in checks** (not implemented): `probe: systemctl is-active fetcher` /
      `probe: disk_free > 5G` / `probe: http 200 https://…` (read-only). A failed
      probe moves the check to OVERDUE with the observed value. *(P1, L)*
- [ ] `mox check` shows the linked project path and commit for context.
      *(P2, S)*

Exit criteria: a check can remind me tomorrow without me opening the terminal;
a broken VPS cron surfaces as OVERDUE with evidence.

### M4 — v0.5 “Execution targets” (5–8 days; approval ledger already done)

Goal: actually use the VPS and remote plans, not just recommend them.

- [x] One-time human approval per project (`mox vps allow/revoke/approvals`),
      stored in state and shown by `mox status`; agents cannot grant it.
      *(P1, S)*
- [x] Persist the execution target per project; `--target vps` refuses without
      one-time approval. *(P1, M)*
- [x] `--target auto` uses compute/VPS capacity and falls back to local.
      *(P1, M)*
- [x] Remote bootstrap: `mox vps setup` verifies rsync/git/opencode/disk;
      projects are rsynced to `~/mox-remote/<slug>` (no GitHub credentials, no
      keys copied). *(P1, L)*
- [x] Remote run via ssh with a remote `timeout`, project-local caches and a
      remote log; `--detach` (setsid) survives the local machine shutting down
      and `mox vps sync` fetches progress/results. `mox stop` kills the remote
      process group. *(P1, L)*
- [x] Fetch back with `git bundle` into `refs/remotes/mox-remote/*` and
      fast-forward merge; divergence is reported, never forced. *(P1, M)*
- [x] `mox vps plan --compute heavy` OVHcloud checklist. *(P2, S)*
- [x] Tests: remote command builders + fake-runner end-to-end; no real remote
      mutation in tests. Real VPS run additionally needs `opencode` and model
      auth on the VPS.

Exit criteria: a heavy project can run end-to-end on a remote target and land
its commits back on the workstation, with no secrets copied.

### M5 — v1.0 “Scale & polish” (4–6 days)

Goal: predictable, observable, installable.

- [x] RAM-aware admission (`MemAvailable / mem_per_agent_mb`). *(P2, M)*
- [~] Per-project budget: wall-clock timeout, attempt counter and duration are
      recorded; no token/cost ceiling yet. *(P2, M)*
- [~] `mox report` with durations, failures and stats is implemented;
      `events.jsonl` and disk deltas are pending. *(P2, M)*
- [ ] Disk watchdog during runs (check every N seconds; pause admission and warn
      below the floor) and log retention (`--keep-days`). *(P2, M)*
- [x] CI: GitHub Actions `fmt --check`, `clippy -D warnings`, tests. Release
      workflow/sha256 pending. *(P2, S)*
- [ ] `mox completions bash|zsh|fish`, man page, `--version` includes commit.
      *(P2, S)*
- [ ] `mox init --profile minimal|standard|paranoid` presets. *(P2, S)*
- [ ] Coverage measurement and property tests. *(P2, S)*
- [~] README/AGENTS/ROADMAP updated; `docs/architecture.md`,
      `docs/security.md`, `CHANGELOG.md` pending.

Exit criteria: one command installs and configures; CI is green; a week of runs
can be reviewed from `mox report`.

---

## 4. Explicitly out of scope (unless the owner asks)

- **TUI/web dashboard.** `mox check --watch` plus `--oneline` covers the
  sidebar need without a new stack. Revisit only if checks become interactive.
- **Ordering/provisioning OVH automatically.** Billing and cloud mutations stay
  human. `mox` only produces checklists and recommendations.
- **Multi-user / SaaS.** Single-owner workstation tool.
- **Auto-merging agent branches.** Projects are independent; PR automation per
  project can be added later if the workflow demands it.

---

## 5. Suggested next sprint

Already shipped in v0.5.0: locks, private permissions, post-run verification,
`gh`/`ssh`/signal wrappers, one-time VPS approval, wall-clock cap, Telegram
notifications, TDD/parallel/VPS rules, milestone boards with live ETA, host
profiling, cross-project calibration, OpenCode permission configs, pre-commit
hooks, rolling pools, resume/retry, reports, CI, and remote VPS execution.

Next, in this order:

1. **Remote owner setup:** install `opencode` on the VPS and configure model
   auth there; run `mox vps setup` until it is green.
2. **Planning second pass** from answers into a refined spec and `TASKS.md`.
3. **Checks:** recurrence, VPS health probes, configurable timezone, project
   path/commit context.
4. **Project lifecycle:** `remove` and `adopt`.
5. **Resilience:** retry backoff, disk watchdog, log retention, `events.jsonl`,
   token/cost budgets.
6. **Packaging:** completions, man page, init profiles, release artifacts,
   coverage, architecture/security docs, changelog.

---

## 6. Residual after v0.5.0

- **Remote execution needs owner setup:** install `opencode` on the VPS and
  configure model auth there. mox deliberately never copies credentials.
  `mox vps setup` reports exactly what is missing.
- **24/7 Telegram bot:** the bot sees state where it runs. For notifications
  while the workstation is off, run the bot on the VPS once remote state lives
  there (or keep a small always-on controller).
- **Planning second pass** from answers into a refined spec/`TASKS.md`.
- **Checks:** recurrence (`every: 1d`), VPS health probes, local-timezone config,
  linked project path/commit.
- **Project lifecycle:** `remove` and `adopt` (list/archive exist).
- **Resilience/polish:** retry backoff, disk watchdog, log retention,
  `events.jsonl`, token/cost caps.
- **Packaging:** completions, man page, init profiles, release artifacts,
  coverage, `docs/architecture.md`, `docs/security.md`, `CHANGELOG.md`.

## 7. Open questions for the owner

1. **Agent autonomy:** should agents ever be allowed to `ssh` to the VPS
   themselves, or only through `mox` (recommended: only through `mox`)?
2. **Branches:** direct pushes to `main` (current) or a branch + PR per run?
   PRs are safer for review but add merge work.
3. **Reminders:** desktop only, or also email/ntfy/webhook? Any preference?
4. **Remote execution:** should `mox up` ever auto-deploy to the VPS, or always
   require an explicit `--target vps` (recommended: explicit)?
5. **Budgets:** a default wall-clock cap per project (e.g. 2 h) and a maximum
   number of retries?
6. **Checks retention:** how long to keep completed checks (30 days, 90, never)?
