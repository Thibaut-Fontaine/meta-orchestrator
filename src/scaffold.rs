//! Project scaffolding: the files every project starts with.
//!
//! Scaffolding never overwrites an existing file, so re-running `mox up` after a
//! human or an agent edited the project is safe.

use crate::config::Config;
use crate::model::ProjectSpec;
use crate::util::display_path;
use anyhow::{Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};

/// Result of preparing one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scaffold {
    pub path: PathBuf,
    pub created: Vec<PathBuf>,
    pub existed: bool,
}

/// Directories/files that must never be committed.
pub const GITIGNORE: &str = r#"# Build output and caches (kept outside the repo when possible)
target/
node_modules/
.venv/
venv/
__pycache__/
*.py[cod]
.pytest_cache/
.mypy_cache/
.ruff_cache/
.benchmarks/
dist/
build/
out/
.next/
.nuxt/
.turbo/
.parcel-cache/
coverage/
htmlcov/
.gradle/
.terraform/

# Local state
.mox/
.env
.env.*
*.local
*.log
*.tmp
*.pid

# Secrets and keys
*.pem
*.key
id_rsa*
id_ed25519*
credentials*
*.secret

# Large files: never commit datasets, archives, media or binaries
*.zip
*.tar
*.tar.gz
*.tgz
*.7z
*.iso
*.bin
*.db
*.sqlite
*.sqlite3
*.parquet
*.csv.gz
*.mp4
*.mov
*.mkv
*.onnx
*.pt
*.safetensors

# OS noise
.DS_Store
Thumbs.db
"#;

const CHECKS_MD: &str = r#"# CHECKS.md — human checks

`mox check` imports the open items below into the attention queue (nothing else
in this file is parsed). Add a line whenever a human must verify something the
agent cannot: a VPS service or cron, an API key/quota, a deploy, a data sample.

Format — one check per line, only the title is required:

```text
- [ ] Short imperative title | due: +2h | kind: vps | priority: high | detail: what to look at | every: 1d | probe: systemctl is-active fetcher
```

* `due`: `+30m`, `+2h`, `+3d`, `+1w`, `now`, or an RFC3339 timestamp. Omit for
  "whenever you get to it".
* `kind`: `vps`, `api`, `deploy`, `manual`, `data`, `security`, `other`.
* `priority`: `low`, `normal`, `high`, `critical`.
* `detail`: one extra sentence, no secrets.
* `every`: recurrence (`1d`, `12h`); marking it done reschedules it.
* `probe`: read-only VPS check. Supported forms:
  `systemctl is-active <unit>`, `disk_free > 5G`, `http 200 https://…`.
  `mox check probe` marks failures due now with high priority.

Rules:

* Never put tokens, keys, passwords or personal data in this file.
* Mark a line `- [x]` only after the human confirmed; `mox` ignores those.
* Keep titles short and actionable ("Verify the fetcher cron ran", not "VPS").
* Re-running sync refreshes an open item with the same title; done items stay
  done.
"#;

const AGENTS_MD: &str = r#"# AGENTS.md — project rules

This project was scaffolded by `mox` (meta-orchestrator) and is worked on by an
autonomous agent. Read `~/AGENTS-GLOBAL.md` first; project rules win where they
are stricter.

## Non-negotiables

1. **Private repository.** Never change visibility, add collaborators, deploy
   keys, webhooks or secrets. Never publish it.
2. **One repository.** Only this project. Do not read, clone or modify other
   repositories, and do not touch `nginx`, `instabot` or `llmcheck` on the VPS.
3. **Small repository.** No build output, `node_modules`, virtualenvs, datasets,
   model weights, archives or binaries in git. Caches belong in the paths given
   by `CARGO_TARGET_DIR`, `TMPDIR`, `npm_config_cache` and `PIP_CACHE_DIR`.
4. **Inspect, test, change.** No destructive action, no force-push, no history
   rewrite, no system upgrade, no reboot, no SSH/firewall changes.
5. **Unattended.** Do not ask interactive questions. Decide, write the decision
   in `DECISIONS.md`, and continue.
6. **Human checks.** When something needs a human (VPS service/cron, API key or
   quota, deploy, data sample), add a line to `CHECKS.md` in the documented
   format. `mox check` shows these to the owner, ordered by time. Never put
   secrets or personal data in that file.
7. **Wall-clock cap.** Each run has a time limit. Prefer a small, working,
   tested slice over a large unfinished one; commit early and often.

## TDD workflow (mandatory, in this order)

1. Write a failing unit test for the next behaviour.
2. Run it and confirm it fails for the right reason.
3. Implement the smallest change that makes it pass.
4. Run the full test suite, formatter and linter; fix everything.
5. Commit with a focused message.
6. Repeat.

Do not commit untested code. If a test cannot run in this environment, record
why in `DECISIONS.md`.

## Milestones (mandatory)

`MILESTONES.json` is the plan and the live progress board. Work in order and
update it only through mox:

* before starting: `mox milestone start m1`
* tick a sub-todo: `mox milestone todo m1 1` (1-based, see `mox milestone list`)
* done only after tests pass and the work is committed and pushed:
  `mox milestone done m1 --note "what was validated"`
* blocked: `mox milestone block m1 --note "what is missing"`
* skip only with a recorded reason: `mox milestone skip m1`

Never edit `MILESTONES.json` by hand. Commit it with the milestone's work; mox
uses it to report live progress and the ETA. If the plan no longer fits, record
why in `DECISIONS.md` and add sub-todos rather than silently skipping ahead.

## Parallel safety

Other agents, scripts and OpenCode sessions run at the same time on this
workstation **and on the VPS**.

* Never signal, kill, restart or modify a process, service, container, port,
  cron job or file you did not create.
* Never communicate or coordinate with another agent through shared files,
  sockets, queues or ports. Do not read other projects.
* Use only this project directory and the provided `TMPDIR`; prefix temporary
  resources with the project slug.
* Never bind a well-known or shared port. Prefer an ephemeral port and record
  it in `STATUS.md`.
* On the VPS, assume other workloads are running: no global restarts, no
  service changes, no firewall/SSH changes, no `crontab`.

## Git workflow

* **Solo:** commit on `main` and push normally.
* **If you spawn sub-agents:** create a branch, open a PR, and merge only after
  the full test suite passes.
* Never force-push, never rewrite shared history, never add or change remotes,
  never clone another repository.

## VPS and remote work

* You cannot `ssh`, `scp` or `sftp`; the guardrails block them.
* All remote work is executed by `mox`, and only after a one-time human approval
  (`mox vps allow <slug>`). If something must run on the VPS, add a check with
  the exact command and expected output.
* Never copy keys, tokens or `.env` files anywhere.

## Guardrails

The agent `PATH` starts with wrappers generated by `mox`. They block
force/mirror/delete pushes, remote changes, cloning other repos, `gh repo
create/delete/edit`, `gh api/auth/search`, ssh/scp, and process/service signals.
A refusal is a decision, not an obstacle: do not bypass it with an absolute
path; record the need in `CHECKS.md`.

## Definition of done

See the "Definition of done" section of `TASK.md`. Finish by updating
`STATUS.md` with what works, what is untested, and the next three actions, then
commit it together with any `CHECKS.md`/`DECISIONS.md`/`MILESTONES.json` changes
and push. `mox` treats uncommitted or unpushed work as a failed run.
"#;

/// Create (or complete) one project workspace on disk.
pub fn scaffold_project(cfg: &Config, spec: &ProjectSpec) -> Result<Scaffold> {
    let root = cfg.projects_root_path();
    let path = root.join(&spec.slug);
    let existed = path.exists();

    if !crate::util::is_inside(&root, &path) {
        anyhow::bail!(
            "refusing to scaffold outside the projects root: {}",
            path.display()
        );
    }
    std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;

    let mut created = Vec::new();
    let opencode_permissions = crate::guard::opencode_permissions(&path);
    let files = vec![
        ("README.md", readme(spec)),
        ("AGENTS.md", AGENTS_MD.to_string()),
        (".gitignore", GITIGNORE.to_string()),
        ("TASK.md", task_markdown(cfg, spec)),
        ("CHECKS.md", CHECKS_MD.to_string()),
        ("TASKS.md", tasks_markdown(spec)),
        (
            "opencode.json",
            serde_json::to_string_pretty(&opencode_permissions).unwrap() + "\n",
        ),
    ];
    for (name, content) in files {
        let file = path.join(name);
        if !file.exists() {
            write_new(&file, &content)?;
            created.push(file);
        }
    }

    // The live progress board. Agents update it with `mox milestone`.
    let board_file = crate::milestones::board_path(&path);
    if !board_file.exists() {
        // Start from cross-project calibration when history exists.
        let prior = crate::stats::load_optional(&cfg.stats_file())
            .map(|store| crate::stats::summarize(&store))
            .map(|summary| crate::stats::prior_for_spec(&summary, spec))
            .unwrap_or(1.0);
        let board = crate::milestones::board_from_spec_with_prior(spec, Utc::now(), prior);
        crate::milestones::save(&board_file, &board)?;
        created.push(board_file);
    }

    Ok(Scaffold {
        path,
        created,
        existed,
    })
}

fn write_new(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

fn tasks_markdown(spec: &ProjectSpec) -> String {
    let milestones = if spec.milestones.is_empty() {
        crate::milestones::fallback_for(spec)
    } else {
        spec.milestones.clone()
    };
    let mut out = format!("# Tasks — {}\n\n", spec.title);
    for milestone in milestones {
        out.push_str(&format!(
            "## {} · {} (~{})\n\n",
            milestone.id,
            milestone.title,
            crate::util::format_secs(milestone.estimate_secs)
        ));
        if milestone.todos.is_empty() {
            out.push_str("- [ ] Complete the milestone and commit\n");
        } else {
            for todo in milestone.todos {
                out.push_str(&format!("- [ ] {todo}\n"));
            }
        }
        out.push('\n');
    }
    out
}

fn readme(spec: &ProjectSpec) -> String {
    format!(
        "# {}\n\n{}\n\n---\n\nScaffolded by `mox`. See `TASK.md` for the brief and \
         `AGENTS.md` for the rules.\n",
        spec.title,
        if spec.summary.is_empty() {
            spec.idea.as_str()
        } else {
            spec.summary.as_str()
        }
    )
}

fn task_markdown(cfg: &Config, spec: &ProjectSpec) -> String {
    let mut out = format!(
        "# {title}\n\n**Slug:** `{slug}`  \n**Compute:** {compute}  \n\
         **Needs VPS:** {vps}  \n**Estimated disk:** {disk} MB  \n\
         **Private repository:** yes  \n\n## Idea\n\n{idea}\n\n## Summary\n\n{summary}\n",
        title = spec.title,
        slug = spec.slug,
        compute = spec.compute.as_str(),
        vps = spec.needs_vps,
        disk = spec.estimated_disk_mb,
        idea = spec.idea,
        summary = if spec.summary.is_empty() {
            "(none)"
        } else {
            spec.summary.as_str()
        },
    );
    if !spec.stack.is_empty() {
        out.push_str(&format!(
            "\n## Suggested stack\n\n{}\n",
            spec.stack.join(", ")
        ));
    }
    if !spec.questions.is_empty() {
        out.push_str("\n## Questions and answers\n\n");
        for question in &spec.questions {
            out.push_str(&format!("- **{}** {}\n", question.id, question.question));
            match spec.answers.get(&question.id) {
                Some(answer) if !answer.trim().is_empty() => {
                    out.push_str(&format!("  - Answer: {answer}\n"))
                }
                _ => {
                    out.push_str("  - Answer: (not provided; use the default or your judgement)\n")
                }
            }
            if let Some(default) = &question.default {
                out.push_str(&format!("  - Default: {default}\n"));
            }
        }
    }
    if !spec.assumptions.is_empty() {
        out.push_str("\n## Assumptions\n\n");
        for assumption in &spec.assumptions {
            out.push_str(&format!("- {assumption}\n"));
        }
    }
    if !spec.risks.is_empty() {
        out.push_str("\n## Risks\n\n");
        for risk in &spec.risks {
            out.push_str(&format!("- {risk}\n"));
        }
    }
    let milestones = if spec.milestones.is_empty() {
        crate::milestones::fallback_for(spec)
    } else {
        spec.milestones.clone()
    };
    out.push_str("\n## Milestones\n\n");
    out.push_str("Track them with `mox milestone` (do not edit `MILESTONES.json` by hand).\n\n");
    out.push_str("| id | milestone | complexity | estimate | algorithm |\n");
    out.push_str("|---|---|---|---|---|\n");
    for milestone in &milestones {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            milestone.id,
            milestone.title,
            milestone.complexity.as_str(),
            crate::util::format_secs(milestone.estimate_secs),
            if milestone.algorithmic_complexity.is_empty() {
                "-"
            } else {
                &milestone.algorithmic_complexity
            }
        ));
        for todo in &milestone.todos {
            out.push_str(&format!("|  | - [ ] {} |  |  |  |\n", todo));
        }
    }

    out.push_str("\n## Definition of done\n\n");
    if spec.acceptance_criteria.is_empty() {
        out.push_str("- The project builds and its tests pass.\n");
    } else {
        for criterion in &spec.acceptance_criteria {
            out.push_str(&format!("- {criterion}\n"));
        }
    }
    out.push_str(&format!(
        "\n## Environment\n\n\
         - Global rules: `{}`\n\
         - Cargo target dir: `{}`\n\
         - Temp dir: `{}`\n\
         - Keep caches out of the repository.\n",
        display_path(&cfg.agents_global_path()),
        display_path(&cfg.cargo_target_for(&spec.slug)),
        display_path(&cfg.tmp_for(&spec.slug)),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Idea;

    fn cfg(root: &Path) -> Config {
        Config {
            projects_root: root.to_string_lossy().to_string(),
            state_dir: root.join("state").to_string_lossy().to_string(),
            ..Config::default()
        }
    }

    fn spec() -> ProjectSpec {
        let idea = Idea {
            title: Some("Demo".into()),
            details: "Build a demo tool.".into(),
            raw: String::new(),
        };
        let mut spec = ProjectSpec::from_idea(&idea, "demo".into());
        spec.summary = "A demo tool for testing.".into();
        spec.questions.push(crate::model::Question {
            id: "q1".into(),
            question: "Which storage?".into(),
            why: None,
            default: Some("SQLite".into()),
        });
        spec.answers.insert("q1".into(), "PostgreSQL".into());
        spec
    }

    #[test]
    fn creates_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let scaffolding = scaffold_project(&cfg(dir.path()), &spec()).unwrap();
        assert!(!scaffolding.existed);
        assert_eq!(scaffolding.created.len(), 8);
        let project = dir.path().join("demo");
        assert!(project.join("README.md").exists());
        assert!(project.join("AGENTS.md").exists());
        assert!(project.join(".gitignore").exists());
        assert!(project.join("TASK.md").exists());
        assert!(project.join("CHECKS.md").exists());
        assert!(project.join("MILESTONES.json").exists());
        assert!(project.join("opencode.json").exists());
        assert!(project.join("TASKS.md").exists());
        assert!(std::fs::read_to_string(project.join("TASKS.md"))
            .unwrap()
            .contains("m1"));
        let perms = crate::guard::opencode_permissions(&project);
        assert_eq!(perms["permission"]["bash"]["ssh *"], "deny");
        let board = crate::milestones::load(&project.join("MILESTONES.json")).unwrap();
        assert_eq!(board.slug, "demo");
        assert_eq!(board.milestones.len(), 3);
        assert!(board
            .milestones
            .iter()
            .all(|m| m.status == crate::milestones::MilestoneStatus::Pending));
        let task = std::fs::read_to_string(project.join("TASK.md")).unwrap();
        assert!(task.contains("PostgreSQL"));
        assert!(task.contains("Private repository"));
        assert!(task.contains("## Milestones"));
        assert!(task.contains("m1"));
        assert!(task.contains("mox milestone"));
    }

    #[test]
    fn board_starts_with_cross_project_prior() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        let mut store = crate::stats::StatsStore::default();
        store.records.push(crate::stats::MilestoneRecord {
            slug: "old".into(),
            milestone: "m1".into(),
            title: "old".into(),
            complexity: crate::model::Complexity::Medium,
            estimate_secs: 100,
            actual_secs: 200,
            finished_at: chrono::Utc::now(),
            cpu_ghz: 3.5,
            logical_cores: 8,
        });
        crate::stats::save(&cfg.stats_file(), &store).unwrap();

        scaffold_project(&cfg, &spec()).unwrap();
        let board = crate::milestones::load(&dir.path().join("demo/MILESTONES.json")).unwrap();
        assert_eq!(board.prior_factor, 2.0);
    }

    #[test]
    fn never_overwrites_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("demo");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), "human edited").unwrap();

        let scaffolding = scaffold_project(&cfg(dir.path()), &spec()).unwrap();
        assert!(scaffolding.existed);
        assert_eq!(
            std::fs::read_to_string(project.join("README.md")).unwrap(),
            "human edited"
        );
        // Missing files are still created.
        assert!(project.join("TASK.md").exists());
    }

    #[test]
    fn checks_file_documents_the_format() {
        assert!(CHECKS_MD.contains("- [ ]"));
        assert!(CHECKS_MD.contains("due:"));
        assert!(CHECKS_MD.contains("Never put tokens"));
        assert!(AGENTS_MD.contains("`CHECKS.md`"));
    }

    #[test]
    fn gitignore_covers_heavy_artifacts() {
        for pattern in [
            "target/",
            "node_modules/",
            ".venv/",
            "*.zip",
            ".env",
            "*.pem",
        ] {
            assert!(GITIGNORE.contains(pattern), "missing {pattern}");
        }
    }

    #[test]
    fn agents_md_forbids_public_and_other_repos() {
        assert!(AGENTS_MD.contains("Private repository"));
        assert!(AGENTS_MD.contains("One repository"));
        assert!(AGENTS_MD.contains("no force-push"));
        assert!(AGENTS_MD.contains("TDD workflow (mandatory"));
        assert!(AGENTS_MD.contains("Parallel safety"));
        assert!(AGENTS_MD.contains("mox vps allow"));
        assert!(AGENTS_MD.contains("Guardrails"));
        assert!(AGENTS_MD.contains("If you spawn sub-agents"));
        assert!(AGENTS_MD.contains("## Milestones (mandatory)"));
        assert!(AGENTS_MD.contains("mox milestone done m1"));
    }
}
