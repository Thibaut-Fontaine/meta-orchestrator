//! Project scaffolding: the files every project starts with.
//!
//! Scaffolding never overwrites an existing file, so re-running `mox up` after a
//! human or an agent edited the project is safe.

use crate::config::Config;
use crate::model::ProjectSpec;
use crate::util::display_path;
use anyhow::{Context, Result};
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
4. **Inspect, back up, test, change.** No destructive action, no force-push, no
   history rewrite, no system upgrade, no reboot, no SSH/firewall changes.
5. **Tests.** Add and run tests with every feature. A change is not done until
   the suite passes or the reason is documented.
6. **Commits.** Small, focused commits with meaningful messages.
7. **Unattended.** Do not ask interactive questions. Decide, write the decision
   in `DECISIONS.md`, and continue.

## Definition of done

See the "Definition of done" section of `TASK.md`. Finish by updating
`STATUS.md` with what works, what is untested, and the next three actions.
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
    let files = [
        ("README.md", readme(spec)),
        ("AGENTS.md", AGENTS_MD.to_string()),
        (".gitignore", GITIGNORE.to_string()),
        ("TASK.md", task_markdown(cfg, spec)),
    ];
    for (name, content) in files {
        let file = path.join(name);
        if !file.exists() {
            write_new(&file, &content)?;
            created.push(file);
        }
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
        assert_eq!(scaffolding.created.len(), 4);
        let project = dir.path().join("demo");
        assert!(project.join("README.md").exists());
        assert!(project.join("AGENTS.md").exists());
        assert!(project.join(".gitignore").exists());
        assert!(project.join("TASK.md").exists());
        let task = std::fs::read_to_string(project.join("TASK.md")).unwrap();
        assert!(task.contains("PostgreSQL"));
        assert!(task.contains("Private repository"));
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
    }
}
