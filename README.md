# meta-orchestrator (`mox`)

Turn a rough list of project ideas into **specified, private GitHub repositories**
worked on by **several isolated OpenCode agents in parallel**.

`mox` is a Rust CLI (TDD, 318 unit + 4 property + 8 end-to-end tests) built for this
workstation's reality: 8 cores, limited RAM/disk, an existing `vps-ovh`, and a
`gh` CLI that is already authenticated.

```
ideas.txt ──► brain (opencode) ──► questions ──► your answers
                                                     │
                                                     ▼
                        ~/mox-projects/<slug> ──► git + private gh repo
                                                     │
                          ┌──────────────────────────┴───────────────────┐
                          ▼                          ▼                   ▼
                    opencode agent 1          opencode agent 2     opencode agent N
                    cores 0-2 (nice 10)       cores 3-4            cores 5-7
```

## What it does

1. **`mox plan`** reads your ideas, asks an OpenCode "brain" to think critically
   about each one, and produces 3–6 clarifying questions per project plus a
   summary, assumptions, risks, stack, compute class and disk estimate.
2. **You answer** — interactively, or by editing `answers.toml` and re-running
   `mox plan --answers`.
3. **`mox up`** scaffolds one git repository per project, commits it, creates a
   **private** GitHub repository with `gh`, pushes, then launches one OpenCode
   agent per project with `opencode run --auto`.
4. **`mox status` / `mox logs` / `mox reflect`** follow progress; `mox clean`
   reclaims disk; `mox stop` stops one agent safely.
5. **`mox milestone`** tracks each project as ordered milestones with sub-todos,
   complexity weights and time estimates; `mox status` turns that into live
   progress and an ETA. Agents start/finish milestones as they work.
6. **`mox check`** is the human-attention queue: agents append what they cannot
   verify (VPS cron, API key/quota, deploy, data sample) to `CHECKS.md`, and the
   queue shows what to check **now, today, soon, later**, with a live
   `--watch` sidebar.
7. **`mox vps check` / `mox vps suggest`** decide whether a workload fits the
   workstation, the existing VPS, or needs an OVHcloud remote plan.
8. **`mox up --target vps`** runs approved projects on the VPS (rsync + ssh +
   OpenCode) and fetches their commits back as a git bundle. `mox vps setup`
   checks the remote prerequisites; no keys are ever copied.

## Install

```bash
cd ~/meta-orchestrator
./build.sh                 # release build
./build.sh --test          # fmt + clippy + tests + release
./build.sh --install       # build and copy to ~/.local/bin/mox
./build.sh --debug         # fast debug build
./build.sh --offline       # use the local cargo cache only
```

`build.sh` only builds mox; it does not run project code or a benchmark.
After building: `mox init && mox doctor`.

`mox init` writes `~/.config/meta-orchestrator/config.toml` (mode 0600) and
creates `~/mox-projects` plus its internal `.mox/` cache/log tree.

Add it to `PATH` if you like:

```bash
install -m 755 target/release/mox ~/.local/bin/mox
```

The planned work from v0.1 to v1.0 is tracked in [ROADMAP.md](ROADMAP.md).

## Quick start

```bash
$ cat ideas.txt
# one idea per line; "Title :: details" is optional
- Expense tracker :: a CLI that logs expenses and prints monthly reports
- Note search :: semantic search over my markdown notes, runs locally

$ mox plan --ideas ideas.txt
asking the planning model about 2 project idea(s)...
...
questions: ~/.local/state/meta-orchestrator/QUESTIONS.md
answers:   ~/.local/state/meta-orchestrator/answers.toml
next: mox up

$ $EDITOR ~/.local/state/meta-orchestrator/answers.toml
$ mox plan --answers ~/.local/state/meta-orchestrator/answers.toml --deep
$ mox up                       # scaffold + private repos + parallel agents
$ mox status
$ mox logs expense-tracker -n 80
```

Useful flags:

| Command | Meaning |
|---|---|
| `mox plan --yes` | accept defaults, never ask |
| `mox plan --refresh` | ask the brain again |
| `mox plan --deep` | second pass: refine summary/acceptance/risks from answers |
| `mox up --dry-run` (or global `--dry-run`) | show everything, touch nothing |
| `mox up --only slug` | one project only (repeatable) |
| `mox up --no-github` | local work only, no repo creation |
| `mox up --no-run` | scaffold + publish, launch nothing |
| `mox up --max-parallel 3` | override concurrency |
| `mox up --resume` / `--retry-failed` | skip completed / retry failed or stopped only |
| `mox up --target vps` / `--target auto` | run on the VPS (approval required) / choose per project |
| `mox up --target vps --detach` | start a VPS run that survives shutting the workstation down |
| `mox vps sync [--slug slug]` | poll detached runs, fetch finished work back, update status |
| `mox bot` / `mox bot --once` | read-only Telegram bot (long-poll / one batch) |
| `mox completions bash\|zsh\|fish` | print a shell completion script |
| `mox events [--tail N]` | local event log |
| `mox recovery [--push] [--check] [--slug slug]` | show/push/verify disaster-recovery snapshots |
| `mox init --profile paranoid` | stricter defaults (1 agent, no auto-approve) |
| `mox vps ps` | audit remote mox/agent processes, flag GHOST/BOT |
| `mox vps cleanup --apply --yes` | kill only untracked mox-remote/bot pids |
| `mox up --owner ORG` | create the private repo under an org |
| `mox --run-timeout 1800 up` | wall-clock cap per agent run (seconds) |
| `mox status` | table with milestone progress and ETA |
| `mox status --watch` | live table, refreshed every 10 s |
| `mox stats` | cross-project milestone timing statistics (`--json`, `--reset --yes`) |
| `mox status --verbose` | also print each project's `STATUS.md` |
| `mox report [--out FILE]` | Markdown digest across all projects |
| `mox project list` / `mox project archive <slug>` | list / move a workspace to `.mox/archive` |
| `mox vps setup` | check rsync/git/opencode/disk on the VPS |
| `mox vps plan --compute heavy` | OVHcloud provisioning checklist |
| `mox milestone status` | detailed progress/ETA for the project in the current dir |
| `mox milestone list` | milestones, todos, estimates, complexity, algorithms |
| `mox milestone start m1` / `done m1 --note …` | record a transition |
| `mox milestone todo m1 1` / `add-todo m1 "…"` | track sub-todos |
| `mox milestone block m2 --note "…"` | mark blocked (ETA becomes unavailable) |
| `mox check` (`mox attention`) | human checks ordered by when they are due |
| `mox check --watch` | live sidebar, refreshed every 10 s (Ctrl-C to quit) |
| `mox check --oneline` | one line for tmux/waybar/polybar |
| `mox check --json` | machine-readable views with `bucket` and `due_in_seconds` |
| `mox check add "Verify cron" --project slug --due +2h --kind vps --priority high` | add/refresh a check |
| `mox check later <id> 2h` | postpone a check |
| `mox check done <id> --note "looked fine"` | complete a check |
| `mox check add … --every 1d --probe "systemctl is-active fetcher"` | recurring / probed check |
| `mox check probe` | run read-only VPS probes; failures become due now |
| `mox check --context` | show project path and short commit per check |
| `mox check remove <id>` / `reopen <id>` | delete / reopen a check |
| `mox check export --format md\|json` | export the queue |
| `mox check --top 3` | only the three most urgent checks |
| `mox check daemon --interval 300` | periodic reminder loop |
| `mox check sync` | import every project's `CHECKS.md` |
| `mox check notify` | send a reminder digest (for a timer/cron) |
| `mox clean --apply [--all]` | delete caches; `--all` also deletes `target/`, `dist/`, `build/` |
| `mox vps suggest --compute heavy` | target recommendation (local/VPS/OVH plan) |
| `mox vps allow <slug> [--note "…"]` | one-time human approval for VPS deployment |
| `mox vps revoke <slug>` / `mox vps approvals` | revoke / list approvals |
| `mox notify status` / `mox notify test` | show channels / send a test message |

## Human check queue (the "sidebar")

Projects that run fetchers, cron jobs, API clients or services on the VPS need a
human sooner or later. Agents cannot verify those things, so they record them in
`CHECKS.md` inside their project:

```text
- [ ] Verify the fetcher cron started | due: +2h | kind: vps | priority: high | detail: systemctl status
- [ ] Review the first data batch | due: +3d | kind: data
```

`mox up` imports them after every run, and `mox check` refreshes them on demand.
The dashboard buckets everything by **when** it must be checked:

```
CHECKS — 2026-10-20 15:04 UTC
1 overdue · 2 now · 1 today · 3 later

NOW (2)
! a1b2c3d4  fetcher      vps       Verify the cron started          in 42m   high
  e5f6a7b8  dashboard    api       Confirm the API quota            in 3h    normal
  ... detail line

TODAY (1)
  ...
```

* `due` accepts `+30m`, `+2h`, `+3d`, `+1w`, `now`, or RFC3339; omit it for
  "whenever".
* `priority` can push an unscheduled check into **NOW** (`high`/`critical`).
* `every: 1d` makes a check recur; `probe:` attaches a read-only VPS check
  (`systemctl is-active`, `disk_free >`, `http <code>`). `mox check probe` runs
  them and promotes failures to **NOW**; `mox check --context` adds the project
  path and short commit.
* `mox check --watch` refreshes the view in place (sidebar in a terminal pane);
  `--oneline` is designed for a status bar; `--json` for scripts.
* Completed checks stay visible with `--all` and are never re-opened by a stale
  `CHECKS.md`.

### Reminders outside the terminal

`mox check notify` sends a digest of overdue/now items through every configured
channel. Put it on a timer if you want to be reminded without opening a
terminal, for example a **user** timer (no system-wide changes):

```ini
# ~/.config/systemd/user/mox-check.service
[Service]
Type=oneshot
ExecStart=%h/.local/bin/mox check notify

# ~/.config/systemd/user/mox-check.timer
[Timer]
OnCalendar=hourly
Persistent=true

[Install]
WantedBy=timers.target
```

```bash
systemctl --user enable --now mox-check.timer
```

Channels (all off by default):

* **Telegram** (recommended): `notify_telegram = true`, `telegram_chat_id = "…"`,
  then `export MOX_TELEGRAM_TOKEN=…` (token from `@BotFather`). The token is read
  from the environment and only written to a 0600 curl config file, never to a
  command line.
* **Desktop**: `notify_desktop = true` (uses `notify-send`).
* **Webhook**: `notify_webhook = true` + `webhook_url = "https://…"`; the URL is
  kept out of argv too (ntfy, a custom bot, a Slack proxy...).

## Design boundary

`mox` is an orchestrator, not a worker: it plans, schedules, pins agents to
threads, records progress and ETA, and reports. It never builds or runs project
code and never runs a CPU benchmark. All actual computation happens in OpenCode
agents, each on its own cores. The only "measurement" mox does is reading
`/proc` for the host profile and observing real milestone durations after the
fact.

## Milestones, progress and ETA

Every project is planned as **3–6 ordered milestones**. Each milestone has:

* a title, description and 2–5 concrete sub-todos;
* a complexity class (`low` = 1, `medium` = 2, `high` = 3, `very-high` = 5);
* a planned duration in minutes;
* an optional algorithmic note (`O(n log n) per batch`, `O(n) memory`) when the
  work is non-trivial.

The plan is written to `MILESTONES.json` in the project (committed). Agents move
it forward with mox, which also records transition times:

```bash
cd ~/mox-projects/fetcher
mox milestone list
mox milestone start m2
mox milestone todo m2 1
mox milestone done m2 --note "parser tests pass, pushed"
mox milestone block m3 --note "waiting for the API key"
mox milestone status
```

`mox status` shows the current milestone, weighted progress and ETA:

```text
slug        status   compute  progress      eta                repo
fetcher     running  light    m2/4 45%      15:42/17:10        https://github.com/…
```

The `eta` column reads `current milestone / project` (local time); the detailed
view spells out the countdown in minutes.

How the ETA is computed:

1. progress is weighted by complexity, not by raw milestone count;
2. completed milestones calibrate the plan: `actual / planned`, clamped to
   0.25–4×, so a project that runs 2× slower gets a 2× longer estimate;
3. the current milestone's elapsed time is subtracted from its estimate, and
   both the **current milestone ETA** and the **project ETA** are reported;
4. a blocked milestone makes the ETA unavailable (`blocked`);
5. a milestone without an estimate falls back to 30 minutes per weight unit.

Planning is **machine-aware**: the planning prompt receives the CPU model,
physical/logical cores, maximum clock (GHz), RAM and architecture, so the model
sizes milestones for the hardware it will actually run on (`mox plan` prints
`estimates for: <CPU> · 4 physical / 8 logical cores · up to 3.5 GHz · …`).
**Algorithmic complexity is a planning input, not a runtime formula**: the model
is asked to state the expected input scale (for example `O(n log n), n≈10k rows`)
and derive its minute estimate from it. The ETA itself uses that estimate plus
measured calibration. There is no synthetic CPU benchmark: different workloads
(compilation, network, data volume) do not correlate with one score, and real
milestone timings are the better signal.

### Cross-project calibration

Every completed milestone is appended to `~/.local/state/meta-orchestrator/stats.json`
under a lock (safe when several agents finish at once). `mox stats` summarises
the median `actual / planned` ratio overall and per complexity class. New
projects start with a weighted prior from that history, so the **first**
milestone ETA is already informed by how long similar work took on this machine;
once the project finishes its own milestones, its local calibration takes over.
Recording is silent (errors never fail a milestone) and capped at 2000 samples.

The board is the contract: `mox status --watch` polls it (side-by-side with the
checks sidebar), `mox up` prints it after each wave, `mox status --json` exposes
`milestone_progress`, and milestone transitions can be pushed to Telegram if
notifications are enabled. Agents are told to mark a milestone done only after
its tests pass and the work is committed and pushed; the post-run check then
verifies the tree is clean.

## Remote execution (VPS / OVH)

`mox up --target vps` mirrors an approved project to `~/mox-remote/<slug>` on
the VPS with `rsync`, runs OpenCode there under a remote `timeout`, and brings
the git history back as a bundle:

1. one-time human approval: `mox vps allow <slug>`;
2. `mox vps setup` must report `rsync`, `git` and `opencode` (plus the model auth
   configured **on the VPS** — mox never copies keys or tokens);
3. `mox up --target vps --only <slug>`; `--target auto` chooses per project and
   falls back to local when approval/prerequisites are missing;
4. remote commits are fetched into `refs/remotes/mox-remote/*`; a fast-forward
   merge is attempted. If it does not fast-forward, the project is marked
   `failed` with a "merge required" note — the workstation is never overwritten.

For always-on work use `--detach`: mox uploads a small runner script plus the
mox binary, starts it with `setsid nohup` on the VPS, records the remote pid and
returns. The run survives your workstation shutting down. Later:

```bash
mox vps sync                # all detached runs
mox vps sync --slug demo    # one project
```

`sync` fetches the live milestone board while running, or the final board, log,
git bundle and fast-forward merge when done. `mox stop <slug>` sends SIGTERM to
the remote process group. A remote `timeout` still bounds every run.

The VPS needs `opencode` and model auth configured **on the VPS**; mox never
copies keys or tokens. `mox vps setup` reports exactly what is missing. The VPS
is not cloned from GitHub, so no `gh` credentials are needed there.

### No hidden remote ghosts

- `mox` never launches `mox bot` on the VPS. The bot refuses to start in a
  remote context (`MOX_REMOTE_RUN`, `.mox/remote.owner`, `mox-remote` path)
  unless a human explicitly passes `--allow-remote` or sets
  `allow_remote_bots = true` (default false).
- Every detached run is recorded in local state with its remote pid.
- `mox vps ps` lists mox/agent processes, marks tracked runs, and flags
  untracked `GHOST` processes and any `BOT`.
- `mox vps cleanup` is a dry run; `--apply --yes` kills only untracked
  `mox-remote` or bot pids, never tracked runs or unrelated processes.

## Telegram bot

`mox bot` is a read-only Telegram bot. It long-polls `getUpdates` (token from
`MOX_TELEGRAM_TOKEN`, chat allowlisted by `telegram_chat_id`):

* `/projects` — inline buttons per project;
* `/project <slug>` — status, target, current milestone, ETA, open todos,
  blockers, `STATUS.md`, last error;
* `/status`, `/milestones [slug]`, `/todos [slug]`;
* `/checks`, `/questions` — due checks, plan questions and blockers;
* `/stats`, `/report`, `/logs <slug> [n]`;
* buttons for Milestones / Todos / Checks / Logs / Report.

It never mutates projects, starts agents, answers questions or reads secrets.
Run it under a systemd **user** service if you want it always available; it can
only see state where it runs. For a bot that stays up while this workstation is
off, run it on the VPS once mox state lives there (future work).

## Disaster recovery (if this computer is destroyed)

mox keeps a human-readable recovery copy **on the VPS**, not only here:

* every `mox milestone` transition writes `RECOVERY.md` in the project
  (gitignored); remote agents do this on the VPS itself;
* `mox up` pushes `~/mox-remote/<slug>/RECOVERY.md` and a global
  `~/mox-remote/RECOVERY.md` before, during (every `recovery_interval_secs`,
  default 120 s) and after runs;
* `mox recovery --push` does it manually, `mox recovery --check` reports remote
  file ages/missing files, and `mox recovery --slug demo` prints one project.

Each file contains: title, status/target/attempts, private repo URL, VPS path,
detached pid, milestone table with ETA, open todos, pending human checks,
`STATUS.md` excerpt, and exact steps to resume on a new machine. It contains
**no tokens, answers, environment variables or logs**. Files are 0600.

If the workstation is gone: `ssh vps-ovh`, read `~/mox-remote/RECOVERY.md` and
`~/mox-remote/<slug>/RECOVERY.md`, clone the private repo, then run
`mox project adopt` + `mox up --resume`.

`recovery_enabled = true` and `recovery_interval_secs = 120` are configurable;
a VPS outage only prints a warning and never fails a run.

## Operations

- `mox init --profile minimal|standard|paranoid` writes a stricter preset
  (`paranoid`: one agent, no auto-approve, 4 GiB floor).
- `mox events` shows the local JSONL event log; `mox report` includes recent
  events and timing statistics.
- `mox clean --logs-older-than-days 30 --apply` prunes old logs/prompts.
- `mox vps sync --watch --interval 300` keeps polling detached runs; a systemd
  user timer example is in `contrib/`.
- `reflect_after_run = true` writes an automatic reflection under
  `.mox/reflections/` (never into the repo).
- `mox bot` can run under a user service (`contrib/mox-bot.service`). It must
  run where state lives; mox does not deploy it to the VPS.
- Timezone: chrono uses the process `TZ` (for example `TZ=Europe/Paris mox status`).
- Budgets: wall-clock `--run-timeout`, per-project maximum attempts, retry
  backoff and disk watchdog are enforced; token/cost ceilings are not exposed by
  `opencode run` and are not guessed.
- Shell completions: `mox completions bash`; man page: `docs/mox.1`.

## Safety rules (enforced, not just documented)

* **Repositories are always private.** `gh repo create … --private` is the only
  creation path, and the result is verified with a read-only
  `gh repo view <slug> --json visibility`. If it is not `PRIVATE`, the agent does
  not start.
* **An existing repository is never reused, renamed, deleted or made public.**
  Creation fails, the project is marked `failed`, and the other projects keep
  going.
* **Nothing is destructive by default.** `mox clean` is a dry run unless
  `--apply`; it only deletes allowlisted cache/build directory names located
  inside the projects root, and `--all` is required for build output.
* **`mox stop` checks `/proc/<pid>/cmdline`** contains `opencode` before sending
  SIGTERM, so a recycled pid cannot be hit.
* **One mutating command at a time.** `plan`, `up` and `clean --apply` share an
  exclusive lock (`up.lock`); checks use a separate lock, so you can add a check
  while agents run. A lock left by a crashed process is reclaimed.
* **Runtime data is private.** `.mox/`, logs, prompts, pid files, `state.json`,
  `plan.json`, `checks.json` and `answers.toml` are 0700/0600.
* **Agents cannot do dangerous things by accident.** Each project gets wrapper
  scripts first on `PATH`: force-push, remote changes, cloning other repos,
  `gh repo create/delete/edit`, `gh api/auth/search`, `ssh`/`scp`, and
  process/service signals are refused. It is defence in depth, not a kernel
  sandbox: a prompt that calls `/usr/bin/gh` directly is still possible, so the
  wrappers are paired with strict project rules.
* **"Completed" means pushed.** After a run, `mox` checks for uncommitted files
  and commits ahead of `origin/main`; dirty or unpushed work marks the project
  `failed`.
* **Agent boundaries are enforced twice.** Generated `opencode.json` denies
  edits/reads outside the project, blocks `gh repo create/delete/edit`,
  `gh api/auth`, force-push, ssh/scp, signals and service changes; per-project
  PATH wrappers enforce the same rules for tools that bypass the model.
* **Commits are checked before they exist.** A generated git pre-commit hook
  rejects files over 2 MiB and obvious secret patterns (private keys, GitHub/
  Slack/OpenAI/AWS tokens). Existing hooks are never overwritten.
* **Ctrl-C is clean.** Agents get their own process group; SIGINT/SIGTERM kills
  the group after a short grace period, saves a `stopped` status and clears pid
  files. Stale `running` entries are flagged by `mox status`.
* **VPS deployment needs a one-time human approval.** `mox vps allow <slug>`
  records it in state; agents cannot grant it. Direct `ssh` from agents is
  blocked. Remote execution uses that approval; without it, `--target vps`
  refuses and `--target auto` stays local.
* **VPS checks are read-only.** `mox vps check/suggest` runs only `nproc`,
  `hostname`, `free`, `df -Pm /` and `/proc/loadavg`. It never touches `nginx`,
  `instabot`, `llmcheck`, systemd, SSH or the firewall. OVH plans are
  *suggestions*; `mox` never orders anything.
* **No secrets are read or printed.** `mox` relies on the existing `gh` keyring
  auth and SSH config. It never copies keys to the VPS or into a repository.

## Disk and CPU strategy

Disk (the workstation already runs hot):

* work refuses to start below `min_free_disk_mb` (default 2 GiB) unless `--force`;
* each project has a soft budget (`project_disk_budget_mb`, default 4 GiB) shown
  with a `!` in `mox status`;
* agents get `CARGO_TARGET_DIR`, `TMPDIR`, `npm_config_cache` and `PIP_CACHE_DIR`
  pointing **outside** the repository, under `~/mox-projects/.mox/`;
* `.gitignore` blocks build output, virtualenvs, datasets, archives, model
  weights, secrets and logs;
* `mox clean` reports and removes caches, never sources.

CPU:

* the scheduler reserves cores (`reserve_cores`, default 1) and splits the rest
  into disjoint contiguous sets;
* a rolling pool keeps `max_parallel` agents busy (no wave barrier); heavy
  projects reduce concurrency, and `MemAvailable / mem_per_agent_mb` caps it;
* compute class decides the minimum core share (light 1, medium 2, heavy/GPU 4);
* each agent is pinned with `taskset -c …`, runs at `nice 10` and `ionice -c 3`,
  so the desktop stays usable;
* heavy/GPU projects print a pointer to `mox vps suggest` before starting.

## Layout

```
~/mox-projects/
├── <slug>/                  # independent git repo, private GitHub remote
│   ├── AGENTS.md            # project rules for the agent
│   ├── TASK.md              # spec, answers, definition of done
│   ├── TASKS.md             # milestone/sub-todo checklist
│   ├── RECOVERY.md          # local snapshot (gitignored)
│   ├── CHECKS.md            # human checks the agent requests
│   ├── MILESTONES.json      # plan + live progress board
│   ├── README.md
│   └── .gitignore
└── .mox/                    # never committed
    ├── logs/<slug>.log      # full agent output
    ├── logs/<slug>.prompt.txt
    ├── run/<slug>.pid
    ├── cache/{cargo-target/<slug>,npm,pip}
    └── tmp/<slug>

~/.local/state/meta-orchestrator/
├── state.json               # statuses, cores, pids, repo URLs
├── plan.json                # cached specs
├── answers.toml             # editable answers
├── checks.json              # human attention queue
├── stats.json               # cross-project milestone timings
└── QUESTIONS.md
```

## Configuration

`~/.config/meta-orchestrator/config.toml` (all keys have defaults; `MOX_*`
environment variables override them, `--config` picks another file):

```toml
projects_root = "~/mox-projects"
state_dir = "~/.local/state/meta-orchestrator"
opencode_bin = "opencode"
gh_bin = "gh"
git_bin = "git"
taskset_bin = "taskset"
nice_bin = "nice"
ionice_bin = "ionice"
ssh_alias = "vps-ovh"
agents_global = "~/AGENTS-GLOBAL.md"
ssh_bin = "ssh"
rsync_bin = "rsync"
remote_root = "~/mox-remote"
remote_opencode_bin = "opencode"
max_parallel = 4
reserve_cores = 1
nice_level = 10
min_free_disk_mb = 2048
mem_per_agent_mb = 768
reflect_after_run = false  # write .mox/reflections/<slug>.md after success
recovery_enabled = true    # push RECOVERY.md snapshots to the VPS
recovery_interval_secs = 120
retry_backoff_secs = 5
project_disk_budget_mb = 4096
brain_timeout_secs = 900
run_timeout_secs = 7200   # wall-clock cap per agent run
model = ""            # e.g. "anthropic/claude-sonnet-4"
auto_approve = true   # required for unattended runs
standalone = true     # private server per agent: env/PATH guardrails apply (more RAM)
extra_instructions = ""

# Notifications (optional, off by default)
notify_desktop = false
notify_telegram = false
allow_remote_bots = false   # hard wall: mox never runs its bot remotely by default
telegram_chat_id = ""
telegram_token_env = "MOX_TELEGRAM_TOKEN"
notify_webhook = false
webhook_url = ""
```

`MOX_DRY_RUN=1` is equivalent to `--dry-run`; `--run-timeout` overrides the cap
for one command. ETA calibration (0.25–4×) and the cross-project prior are
internal; no configuration is needed. Delete `stats.json` to reset the learned
prior.

## Architecture (why it is testable)

| Module | Responsibility |
|---|---|
| `exec` | the only place that spawns processes; `RealRunner` + `FakeRunner`, timeouts, pid files, log streaming |
| `brain` | `Brain` trait; `OpenCodeBrain` in production, `ScriptedBrain` in tests |
| `prompts` | prompt text + defensive JSON parsing (prose, fences, missing fields) |
| `scheduler` | disjoint core partitions and sequential waves |
| `resources` | `DiskProbe`, free-space guard, size accounting, allowlisted pruning |
| `scaffold` | non-overwriting project files (`AGENTS.md`, `TASK.md`, `.gitignore`, `README.md`) |
| `github` | `git init/add/commit` + `gh repo create --private` + visibility verification |
| `runner` | builds `taskset → nice → ionice → opencode run`, cache env, batch execution |
| `state` | atomic `state.json` / `plan.json` |
| `vps` | read-only probe + target recommendation matrix |
| `guard` | agent wrappers and the allow/deny rules for git/gh/ssh/signals |
| `lock` | exclusive locks with stale-owner reclaim |
| `verify` | post-run dirty-tree / unpushed-commit verification |
| `milestones` | boards, transitions, weighted progress and ETA |
| `stats` | cross-project timing history and the first-milestone prior |
| `remote` | rsync/ssh run, detached runner, git bundle fetch-back, preflight |
| `bot` | read-only Telegram long-poll, navigation and ETA views |
| `hooks` | pre-commit guard for large files and secrets |
| `host` | CPU model/cores/GHz/RAM passed to the planner |
| `notify` | Telegram, desktop and webhook channels (secrets out of argv) |
| `app` / `cli` | commands and argument parsing |

Testing:

```bash
cargo test                 # 318 unit + 4 property + 8 end-to-end tests
cargo test --test cli_e2e  # end-to-end only
cargo clippy --all-targets
```

The end-to-end tests run the real binary with a temporary config, real `git`,
fake `gh` and fake `opencode`, and assert on the recorded `gh` arguments —
including that **`--private` is always used and `--public` never is**. Another
e2e test runs the guard wrappers and verifies that the Telegram token never
appears in `curl`'s argv.

## Parallelism

Agents are told, in `AGENTS.md` and in their prompt, that other agents and
scripts run in parallel on the workstation **and on the VPS**: never signal or
modify a process, service, port or file they did not create, never communicate
with other agents through shared files/sockets/ports, and use only their project
directory and `TMPDIR`.

By default each agent runs with `--standalone`, i.e. a private OpenCode server
that inherits the per-project `PATH` (guard wrappers) and cache variables. This
is what makes the guardrails and disk isolation effective. It costs one server
process per active agent; set `standalone = false` to share the background
service when RAM is tight, accepting that tool processes inherit the shared
server's environment instead. Set `standalone = true` for a private server per agent
when you have RAM to spare; on this machine the shared service is the safer
default.

## Troubleshooting

* `mox doctor` fails on disk → `mox clean --apply` or free space.
* `mox doctor` fails on `opencode` → check `opencode --version` and
  `MOX_OPENCODE_BIN`.
* A project is `failed` with “repository may already exist” → choose another
  slug; `mox` will not touch the existing repository.
* An agent produced nothing → `mox logs <slug>` then `mox reflect <slug>`.
* Run was interrupted → `mox status`; stale pid files are ignored by `stop` and
  can be removed with `mox clean`.
