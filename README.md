# meta-orchestrator (`mox`)

Turn a rough list of project ideas into **specified, private GitHub repositories**
worked on by **several isolated OpenCode agents in parallel**.

`mox` is a Rust CLI (TDD, 127 unit tests + 4 end-to-end tests) built for this
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
5. **`mox vps check` / `mox vps suggest`** decide whether a workload fits the
   workstation, the existing VPS, or needs an OVHcloud remote plan.

## Install

```bash
cd ~/meta-orchestrator
cargo build --release
./target/release/mox init
./target/release/mox doctor
```

`mox init` writes `~/.config/meta-orchestrator/config.toml` (mode 0600) and
creates `~/mox-projects` plus its internal `.mox/` cache/log tree.

Add it to `PATH` if you like:

```bash
install -m 755 target/release/mox ~/.local/bin/mox
```

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
$ mox plan --answers ~/.local/state/meta-orchestrator/answers.toml
$ mox up                       # scaffold + private repos + parallel agents
$ mox status
$ mox logs expense-tracker -n 80
```

Useful flags:

| Command | Meaning |
|---|---|
| `mox plan --yes` | accept defaults, never ask |
| `mox plan --refresh` | ask the brain again |
| `mox up --dry-run` (or global `--dry-run`) | show everything, touch nothing |
| `mox up --only slug` | one project only (repeatable) |
| `mox up --no-github` | local work only, no repo creation |
| `mox up --no-run` | scaffold + publish, launch nothing |
| `mox up --max-parallel 3` | override concurrency |
| `mox clean --apply [--all]` | delete caches; `--all` also deletes `target/`, `dist/`, `build/` |
| `mox vps suggest --compute heavy` | target recommendation (local/VPS/OVH plan) |

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
* **The VPS is read-only.** `mox vps check/suggest` runs only `nproc`, `hostname`,
  `free`, `df -Pm /` and `/proc/loadavg`. It never touches `nginx`, `instabot`,
  `llmcheck`, systemd, SSH or the firewall. OVH plans are *suggestions*; `mox`
  never orders anything.
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
* `max_parallel` (default: cores/2, capped at 4) agents run per wave;
* each agent is pinned with `taskset -c …`, runs at `nice 10` and `ionice -c 3`,
  so the desktop stays usable;
* heavy/GPU projects print a pointer to `mox vps suggest` before starting.

## Layout

```
~/mox-projects/
├── <slug>/                  # independent git repo, private GitHub remote
│   ├── AGENTS.md            # project rules for the agent
│   ├── TASK.md              # spec, answers, definition of done
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
max_parallel = 4
reserve_cores = 1
nice_level = 10
min_free_disk_mb = 2048
project_disk_budget_mb = 4096
brain_timeout_secs = 900
run_timeout_secs = 7200
model = ""            # e.g. "anthropic/claude-sonnet-4"
auto_approve = true   # required for unattended runs
standalone = false    # true = one private opencode server per agent (more RAM)
extra_instructions = ""
```

`MOX_DRY_RUN=1` is equivalent to `--dry-run`.

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
| `app` / `cli` | commands and argument parsing |

Testing:

```bash
cargo test                 # 127 unit + 4 end-to-end tests
cargo test --test cli_e2e  # end-to-end only
cargo clippy --all-targets
```

The end-to-end tests run the real binary with a temporary config, real `git`,
fake `gh` and fake `opencode`, and assert on the recorded `gh` arguments —
including that **`--private` is always used and `--public` never is**.

## Parallelism caveat

By default all agents share the background OpenCode service (one session each,
separate working directories, separate logs). They are isolated at the
filesystem, process, core and log level, but they do share the model service and
`~/.local/share/opencode`. Set `standalone = true` for a private server per agent
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
