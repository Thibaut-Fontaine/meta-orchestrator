# mox architecture

`mox` has one job: turn ideas into **private, isolated projects** worked on by
parallel agents, and report progress. It is an orchestrator, not a worker: it
never builds or runs project code and never benchmarks the CPU.

## Layers

| Layer | Module | Responsibility |
|---|---|---|
| CLI | `cli`, `main`, `app` | argument parsing and command orchestration |
| Process | `exec` | the only place that spawns processes; timeouts, pid files, logging; `FakeRunner` for tests |
| Planning | `ideas`, `brain`, `prompts`, `estimate` | idea parsing, questions, answers, specs, difficulty/time/token budget, milestones |
| Scheduling | `scheduler`, `runner` | core partitions, RAM/compute admission, rolling pool, isolation env |
| Workspaces | `scaffold`, `github`, `hooks` | non-overwriting files, private repos, pre-commit guard |
| Progress | `milestones`, `stats`, `verify` | boards, weighted ETA, cross-project priors, push verification |
| Safety | `guard`, `lock`, `shutdown` | agent permission wall, locks, process-group cleanup |
| Human I/O | `checks`, `notify`, `bot` | attention queue, Telegram/webhook, read-only bot |
| Remote | `remote` | VPS mirror, detached runs, bundle fetch-back, process audit |
| State | `state`, `config` | atomic JSON state, configuration |

## Local flow

1. `mox plan` calls the brain once per idea set and writes `plan.json`.
2. `mox up` scaffolds each project, creates a private repo, and starts a rolling
   worker pool.
3. Each agent runs `opencode run --standalone` in its project, with guard
   wrappers first on `PATH`, project-local caches and its own core set.
4. Agents update `MILESTONES.json` via `mox milestone`, write `CHECKS.md` and
   `STATUS.md`, commit and push.
5. Post-run verification requires a clean tree and no commits ahead of upstream.

## Remote flow

`--target vps` mirrors the project to `~/mox-remote/<slug>` with rsync, ships
the mox binary for milestone tracking, and runs OpenCode on the VPS. `--detach`
starts it with `setsid`, records the remote pid, and returns. `mox vps sync`
polls, fetches the board/log, creates a git bundle, fetches it into
`refs/remotes/mox-remote/*`, and fast-forward merges. Divergence is reported,
never forced.

## State

- `~/.local/state/meta-orchestrator/state.json` — project lifecycle, pids,
  targets, attempts.
- `plan.json`, `answers.toml`, `checks.json`, `stats.json` — planning and human
  queues.
- `<project>/MILESTONES.json` — committed progress board.
- `.mox/` — private logs, prompts, caches, guard wrappers, archive.

Writes are atomic; mutating commands share `up.lock`, checks use `checks.lock`.

## Testing

Everything external goes through `CommandRunner`. Unit tests inject
`FakeRunner`/`ScriptedBrain`; end-to-end tests run the real binary with fake
`gh`/`opencode` scripts, real git, and no network.
