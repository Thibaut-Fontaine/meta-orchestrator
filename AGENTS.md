# AGENTS.md — meta-orchestrator

Project rules for this repository. They are stricter than, and take precedence
over, `~/AGENTS-GLOBAL.md`, which still applies.

## Scope

* **This repository only.** Do not enumerate, read, clone or modify other
  repositories.
* Do not touch the VPS beyond read-only checks (`nproc`, `hostname`, `free`,
  `df`, `/proc/loadavg`). Never start/stop/edit `nginx`, `instabot` or
  `llmcheck`, never change systemd, SSH, firewall or ports.
* Never print, commit or paste tokens, keys, `.env` files or `config.toml` from
  other projects.

## Non-negotiables for the product

1. Generated repositories are **always private**. The only creation path is
   `gh repo create … --private`, followed by a read-only visibility check.
   `--public` must never appear in the code or in a test expectation.
2. `mox` must never delete, rename, transfer or change the visibility of a
   repository, and must never reuse an existing repository name.
3. Destructive disk operations are allowlisted and dry-run by default.
4. `mox stop` must verify the process is an OpenCode agent before signalling it.
5. Secrets are never read by the program. Authentication is delegated to `gh`
   and the existing SSH configuration.
6. Human checks (`CHECKS.md`, `mox check`) must never contain tokens, keys or
   personal data. The `CHECKS.md` line format is a public contract with agents:
   keep it backwards compatible and covered by parser tests.
7. Secrets never appear on a command line. Notification tokens and secret
   webhook URLs are passed through 0600 curl config files; tests must assert
   that argv does not contain them.
8. Agent guard decisions are a tested contract. Any new allow/deny rule needs a
   positive and a negative test, and must never be bypassable by the wrapper
   itself (no relative "real" binary, no recursion).
9. Milestone boards (`MILESTONES.json`) are the progress contract: agents
   update them only through `mox milestone`, and the ETA math (weights,
   calibration, elapsed time, blocked state) stays deterministic and tested
   with a fixed `now`.
10. Cross-project stats are append-only and written under a lock. The learned
    prior must stay clamped (0.25–4×) and must never make a command fail.
11. `./build.sh --test` is the supported build gate: fmt, clippy `-D warnings`,
    unit + end-to-end tests, then the release binary.
12. `mox` must stay an orchestrator: never run project builds, tests, code or
    CPU benchmarks inside mox. Agents compute; mox schedules, tracks and
    reports. Host probing only reads `/proc`.
13. Remote execution must never copy SSH keys, tokens or model credentials to
    the VPS. The VPS needs its own `opencode` and model auth. The workstation is
    the source of truth; remote work is fetched as a bundle and only
    fast-forward merged (a diverged remote branch is reported, never forced).
14. `mox bot` is read-only. It must enforce the configured chat-id allowlist,
    never expose tokens in argv, and never mutate projects, start agents or
    answer questions.

## Engineering rules

* **TDD.** Every behaviour change comes with a test. Unit tests live next to the
  code; end-to-end tests live in `tests/` and must not hit the network.
* All external processes go through `exec::CommandRunner`. Do not call
  `std::process::Command` from `app`, `runner`, `github`, `vps` or `clean`
  directly — that would make the workflow untestable.
* Never shell out through `sh -c` with interpolated user input.
* Keep the dependency surface small (clap, serde, serde_json, toml, chrono,
  anyhow, tempfile for tests).
* `cargo fmt`, `cargo clippy --all-targets` and `cargo test` must be clean
  before a commit.
* Disk: no datasets, binaries or build output in git. `target/` is ignored.

## Definition of done

* New behaviour has unit tests and, when it touches the workflow, an end-to-end
  test.
* Failure modes are reported per project without aborting sibling projects.
* `README.md` documents any new command, flag or config key.
