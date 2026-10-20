//! Prompt construction and brain-response parsing.
//!
//! The brainstorm step is intentionally schema-first: we ask OpenCode for a
//! single JSON object, then parse it defensively. Anything missing falls back to
//! safe defaults so one malformed field never blocks the whole plan.

use crate::config::Config;
use crate::model::{Complexity, ComputeNeed, Idea, MilestoneSpec, ProjectSpec, Question};
use crate::util::{slug_from_idea, slugify, truncate_chars, unique_slug};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::path::Path;

/// Shared context appended to planning prompts.
pub fn planning_context(cfg: &Config) -> String {
    format!(
        "You are the planning brain of `mox`, a meta-orchestrator that will hand \
         each project to a separate autonomous OpenCode agent.\n\
         Global operating rules live in {agents}; every agent must follow them. \
         Repositories are always PRIVATE. Never propose destructive operations, \
         never touch unrelated repositories, and never put secrets in a repo.\n\
         The workstation has limited disk, so designs must keep caches and large \
         artifacts out of the repository.",
        agents = crate::util::display_path(&cfg.agents_global_path())
    )
}

/// Prompt asking for clarifying questions and a first technical read of each
/// idea.
pub fn questions_prompt(cfg: &Config, ideas: &[Idea]) -> String {
    let mut out = String::new();
    out.push_str(&planning_context(cfg));
    out.push_str(
        "\n\n## Task\n\
         For EACH idea below, think critically about what is ambiguous, risky or \
         underspecified. Ask the user 3 to 6 high-value questions whose answers \
         change what gets built. Also propose a short technical assessment.\n\n\
         Rules for questions:\n\
         - Only ask what you cannot safely assume; offer a sensible default.\n\
         - Prefer concrete choices (scope, data source, users, stack, deployment) \
           over generic ones.\n\
         - Write them in plain English, answerable in one or two sentences.\n\
         - Give each an id: q1, q2, q3...\n\n\
         Return ONE JSON object, with no prose around it, shaped exactly like:\n\
         {\n\
           \"projects\": [\n\
             {\n\
               \"slug\": \"kebab-case-slug\",\n\
               \"title\": \"Short human title\",\n\
               \"summary\": \"2-4 sentences: what it is, who it is for, what done means\",\n\
               \"questions\": [{\"id\": \"q1\", \"question\": \"...\", \"why\": \"...\", \"default\": \"...\"}],\n\
               \"assumptions\": [\"...\"],\n\
               \"risks\": [\"...\"],\n\
               \"stack\": [\"Rust\", \"SQLite\"],\n\
               \"compute\": \"light|medium|heavy|gpu\",\n\
               \"needs_vps\": false,\n\
               \"estimated_disk_mb\": 500,\n\
               \"acceptance_criteria\": [\"...\"],\n\
               \"milestones\": [\n\
                 {\"id\": \"m1\", \"title\": \"Foundation and failing tests\",\n\
                  \"description\": \"what this milestone delivers\",\n\
                  \"todos\": [\"write the first failing test\", \"set up the skeleton\"],\n\
                  \"complexity\": \"low|medium|high|very-high\",\n\
                  \"algorithmic_complexity\": \"O(n log n) if relevant, else empty\",\n\
                  \"estimate_minutes\": 45}\n\
               ]\n\
             }\n\
           ]\n\
         }\n\n\
         `compute` means the agent's build/run intensity. Use `heavy` for \
         compilers, data processing or long test suites, and `gpu` only when the \
         project truly needs a GPU. `needs_vps` is true only when the project must \
         run services on the VPS.\n\n\
         Milestones: produce 3 to 6 ordered, independently deliverable \
         milestones that an autonomous agent can finish one at a time. Each one \
         needs 2-5 concrete sub-todos, a complexity class, and a realistic \
         estimate in minutes (20-120). The FIRST milestone must include writing \
         failing tests. Set `algorithmic_complexity` only when the milestone \
         involves a non-trivial algorithm or data structure (for example \
         `O(n log n) per batch` or `O(1) lookups, O(n) memory`); otherwise use an \
         empty string. Estimates drive the ETA shown by `mox status`, so be \
         honest about size.\n\n",
    );
    out.push_str("## Ideas\n");
    for (index, idea) in ideas.iter().enumerate() {
        out.push_str(&format!(
            "\n### Idea {} (slug hint: `{}`)\n{}\n",
            index + 1,
            slug_from_idea(idea.title.as_deref(), idea.text()),
            idea.text()
        ));
        if let Some(title) = &idea.title {
            out.push_str(&format!("Title: {title}\n"));
        }
    }
    out.push_str("\nReturn the JSON object now.\n");
    out
}

/// Prompt for one autonomous project agent.
pub fn execution_prompt(cfg: &Config, spec: &ProjectSpec) -> String {
    let answers = spec.answer_lines();
    let answers = if answers.is_empty() {
        "(none provided; use the assumptions above and choose sensible defaults)".to_string()
    } else {
        answers.join("\n")
    };
    let acceptance = if spec.acceptance_criteria.is_empty() {
        "- The project builds and its tests pass.\n- A README explains how to run it.".to_string()
    } else {
        spec.acceptance_criteria
            .iter()
            .map(|c| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let stack = if spec.stack.is_empty() {
        "choose the simplest stack that fits; prefer Rust or Python for tools".to_string()
    } else {
        spec.stack.join(", ")
    };
    let assumptions = if spec.assumptions.is_empty() {
        "- none".to_string()
    } else {
        spec.assumptions
            .iter()
            .map(|a| format!("- {a}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "You are an autonomous software engineer working on ONE project in the \
         current directory. Work end-to-end: design, implement, test, document, \
         commit. This is an unattended run.\n\n\
         ## Project: {title} (`{slug}`)\n\
         {summary}\n\n\
         ## Original idea\n{idea}\n\n\
         ## Clarifications from the owner\n{answers}\n\n\
         ## Assumptions\n{assumptions}\n\n\
         ## Suggested stack\n{stack}\n\n\
         ## Definition of done\n{acceptance}\n\n\
         ## Milestone plan (work in this order)\n{milestones}\n\n\
         Track progress with the mox command (it updates `MILESTONES.json` for you):\n\
           mox milestone start m1\n\
           mox milestone todo m1 1\n\
           mox milestone done m1 --note \"what was validated\"\n\
           mox milestone block m1 --note \"what is missing\"\n\
         Start a milestone before working on it, tick its sub-todos, and mark it \
         done only after its tests pass and the work is committed and pushed. \
         Never edit `MILESTONES.json` by hand and keep it committed; `mox status` \
         uses it to compute live progress and the ETA.\n\n\
         ## Hard rules\n\
         1. Read {agents} first and follow it. Never touch other repositories or \
            the VPS services mentioned there (nginx, instabot, llmcheck).\n\
         2. The git remote is a PRIVATE repository. Never make it public, never \
            add collaborators, webhooks, deploy keys or secrets.\n\
         3. Keep the repository small. Do not commit build output, `node_modules`, \
            virtualenvs, datasets, binaries or archives. Caches already point \
            outside the repo via environment variables (`CARGO_TARGET_DIR`, \
            `TMPDIR`, `npm_config_cache`, `PIP_CACHE_DIR`).\n\
         4. TDD is mandatory, in this order: write a failing unit test for the \
            next behaviour, run it and confirm it fails for the right reason, \
            implement the smallest change, run the full test suite plus formatter \
            and linter, then commit. Never commit untested code; if a test cannot \
            run here, explain why in `DECISIONS.md`.\n\
         5. Commit in small focused steps. If you work solo, commit on `main` and \
            push normally. Only if you spawn sub-agents, work on a branch, open a \
            PR, and merge after tests pass. Never force-push or rewrite history.\n\
         6. Ask nothing interactively; make a reasonable decision, record it in \
            `DECISIONS.md`, and continue.\n\
         7. Other agents, scripts and OpenCode sessions run in parallel on this \
            machine and on the VPS. Never signal, kill, restart or modify a \
            process, service, container, port, cron job or file you did not \
            create. Never communicate or coordinate with other agents through \
            shared files, sockets or ports. Use only this project directory and \
            the provided `TMPDIR`; prefix temporary resources with `{slug}`.\n\
         8. Do not delete unrelated files. Do not run system-wide upgrades, do not \
            reboot, and do not change SSH, firewall, nginx, systemd or crontab.\n\
         9. VPS access is only through mox, never directly. `ssh`, `scp` and \
            `sftp` are blocked, and a remote step requires a one-time human \
            approval (`mox vps allow {slug}`). Record what you need in \
            `CHECKS.md` with the exact command rather than trying to bypass this.\n\
         10. The PATH contains mox guard wrappers. They block force-push, remote \
             changes, cloning other repos, `gh repo create/delete/edit`, `gh \
             api/auth/search`, ssh/scp, and process/service signals. A refusal is \
             a decision: do not bypass it with an absolute path; add a check.\n\
         11. Whenever the owner must verify something manually (VPS service or \
             cron, API key/quota, deploy, data sample), append a line to \
             `CHECKS.md`: `- [ ] <title> | due: +2h | kind: vps | priority: high | \
             detail: <what to check>`. Use `due` to say when it should be checked. \
             Never put secrets in it. `mox check` shows these to the owner.\n\
         12. You have a wall-clock cap of {cap} seconds. Prefer a small, working, \
             tested slice over a large unfinished one; commit early and often.\n\
         13. Finish by writing `STATUS.md` with: what works, what is untested, and \
             the three most valuable next steps. Commit it (and any `CHECKS.md`, \
             `DECISIONS.md` or `MILESTONES.json` changes) as your final commit, \
             then push. `mox` only counts a run as done when the tree is clean and \
             `origin/main` is up to date.\n\n\
         ## Working notes\n\
         Environment: project directory is {path}. Use `git status` to see what is \
         already committed. The task specification is also in `TASK.md`.\n\
         {extra}\n\
         Start now. Keep going until the definition of done is met or you are \
         genuinely blocked; if blocked, write the blocker in `STATUS.md` and stop.",
        title = spec.title,
        slug = spec.slug,
        summary = if spec.summary.is_empty() {
            spec.idea.as_str()
        } else {
            spec.summary.as_str()
        },
        idea = spec.idea,
        answers = answers,
        assumptions = assumptions,
        stack = stack,
        acceptance = acceptance,
        agents = crate::util::display_path(&cfg.agents_global_path()),
        path = cfg.projects_root_path().join(&spec.slug).display(),
        cap = cfg.run_timeout_secs,
        milestones = render_milestone_plan(spec),
        extra = if cfg.extra_instructions.trim().is_empty() {
            String::new()
        } else {
            format!("Owner's extra instructions: {}\n", cfg.extra_instructions)
        }
    )
}

/// Prompt for a post-run reflection on one project.
pub fn reflection_prompt(spec: &ProjectSpec, status_md: &str, log_tail: &str) -> String {
    format!(
        "You are reviewing the result of an autonomous coding run.\n\n\
         Project: {title} (`{slug}`)\nIdea: {idea}\n\n\
         Final STATUS.md (may be missing):\n{status}\n\n\
         Last log lines:\n{log}\n\n\
         Return a short markdown reflection with exactly these sections:\n\
         ## What was achieved\n## Gaps vs the definition of done\n## Risks\n\
         ## Next three actions\n\
         Be concrete and honest; do not invent completed work.",
        title = spec.title,
        slug = spec.slug,
        idea = spec.idea,
        status = if status_md.trim().is_empty() {
            "(missing)"
        } else {
            status_md
        },
        log = if log_tail.trim().is_empty() {
            "(empty)"
        } else {
            log_tail
        }
    )
}

/// One milestone as returned by the brain.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BrainMilestone {
    pub id: Option<String>,
    pub title: String,
    pub description: String,
    pub todos: Vec<String>,
    pub complexity: Option<String>,
    pub algorithmic_complexity: Option<String>,
    pub estimate_minutes: Option<u64>,
}

/// One project as returned by the brain.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct BrainProject {
    pub slug: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub questions: Vec<Question>,
    pub assumptions: Vec<String>,
    pub risks: Vec<String>,
    pub stack: Vec<String>,
    pub compute: Option<String>,
    pub needs_vps: Option<bool>,
    pub estimated_disk_mb: Option<u64>,
    pub acceptance_criteria: Vec<String>,
    pub milestones: Vec<BrainMilestone>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct BrainResponse {
    projects: Vec<BrainProject>,
}

/// Parse the brain's reply into project assessments.
pub fn parse_brain_response(text: &str) -> Result<Vec<BrainProject>> {
    let json = crate::util::extract_json_object(text)
        .context("the planning model returned no JSON object")?;
    let value: serde_json::Value =
        serde_json::from_str(json).context("the planning model returned invalid JSON")?;
    if value.get("projects").is_some() {
        let response: BrainResponse =
            serde_json::from_value(value).context("planning JSON does not match the schema")?;
        if response.projects.is_empty() {
            bail!("planning JSON contains no projects");
        }
        Ok(response.projects)
    } else if value.get("title").is_some() || value.get("questions").is_some() {
        let single: BrainProject =
            serde_json::from_value(value).context("planning JSON does not match the schema")?;
        Ok(vec![single])
    } else {
        bail!("planning JSON has no `projects` array")
    }
}

/// Merge raw ideas with brain assessments into final specs. Always returns one
/// spec per idea, in the original order.
pub fn merge_specs(ideas: &[Idea], mut assessed: Vec<BrainProject>) -> Vec<ProjectSpec> {
    // Match by slug first, then fall back to positional order.
    let mut specs = Vec::with_capacity(ideas.len());
    let mut used_slugs: Vec<String> = Vec::new();

    for (index, idea) in ideas.iter().enumerate() {
        let hint = slug_from_idea(idea.title.as_deref(), idea.text());
        let candidate = assessed
            .iter()
            .position(|p| {
                p.slug
                    .as_deref()
                    .map(slugify)
                    .map(|clean| {
                        clean == hint
                            || clean.starts_with(&format!("{hint}-"))
                            || hint.starts_with(&format!("{clean}-"))
                    })
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| index.min(assessed.len().saturating_sub(1)));
        let brain = if assessed.is_empty() {
            None
        } else {
            Some(assessed.remove(candidate))
        };

        let mut spec = ProjectSpec::from_idea(idea, hint.clone());
        let mut brain_milestones = Vec::new();
        if let Some(brain) = brain {
            if let Some(slug) = brain.slug.as_deref() {
                let clean = slugify(slug);
                if clean != "project" {
                    spec.slug = clean;
                }
            }
            if let Some(title) = brain.title.filter(|t| !t.trim().is_empty()) {
                spec.title = title;
            }
            if let Some(summary) = brain.summary.filter(|s| !s.trim().is_empty()) {
                spec.summary = summary;
            }
            spec.questions = normalise_questions(brain.questions);
            spec.assumptions = clean_list(brain.assumptions);
            spec.risks = clean_list(brain.risks);
            spec.stack = clean_list(brain.stack);
            spec.acceptance_criteria = clean_list(brain.acceptance_criteria);
            if let Some(compute) = brain.compute.as_deref() {
                if let Ok(need) = compute.parse::<ComputeNeed>() {
                    spec.compute = need;
                }
            }
            spec.needs_vps = brain.needs_vps.unwrap_or(matches!(
                spec.compute,
                ComputeNeed::Heavy | ComputeNeed::Gpu
            ));
            spec.estimated_disk_mb = brain.estimated_disk_mb.unwrap_or(512).clamp(64, 200_000);
            brain_milestones = brain.milestones;
        }
        if spec.summary.is_empty() {
            spec.summary = truncate_chars(&idea.details, 280);
        }
        if spec.acceptance_criteria.is_empty() {
            spec.acceptance_criteria = default_acceptance();
        }
        spec.milestones = normalise_milestones(brain_milestones, &spec);
        spec.slug = unique_slug(&spec.slug, &used_slugs);
        used_slugs.push(spec.slug.clone());
        specs.push(spec);
    }
    specs
}

/// Normalise brain milestones: stable ids, bounded estimates, clean todos.
/// Falls back to a 3-milestone plan when the brain returned none.
pub fn normalise_milestones(raw: Vec<BrainMilestone>, spec: &ProjectSpec) -> Vec<MilestoneSpec> {
    let mut out: Vec<MilestoneSpec> = Vec::new();
    for milestone in raw {
        let title = milestone.title.trim();
        if title.is_empty() {
            continue;
        }
        let complexity = milestone
            .complexity
            .as_deref()
            .and_then(|c| c.parse::<Complexity>().ok())
            .unwrap_or(Complexity::Medium);
        let minutes = milestone.estimate_minutes.unwrap_or(0);
        let estimate_secs = if minutes == 0 {
            (complexity.weight() as u64) * 900
        } else {
            minutes.clamp(5, 480) * 60
        };
        out.push(MilestoneSpec {
            id: format!("m{}", out.len() + 1),
            title: title.to_string(),
            description: milestone.description.trim().to_string(),
            todos: clean_list(milestone.todos),
            complexity,
            algorithmic_complexity: milestone
                .algorithmic_complexity
                .unwrap_or_default()
                .trim()
                .to_string(),
            estimate_secs,
        });
    }
    if out.is_empty() {
        crate::milestones::fallback_for(spec)
    } else {
        out
    }
}

/// Markdown bullet list of the milestone plan used by the execution prompt.
fn render_milestone_plan(spec: &ProjectSpec) -> String {
    if spec.milestones.is_empty() {
        return "(no milestones were planned; work in small, tested steps)".to_string();
    }
    let mut out = String::new();
    for milestone in &spec.milestones {
        out.push_str(&format!(
            "- {} [{}] ~{}: {}\n",
            milestone.id,
            milestone.complexity.as_str(),
            crate::util::format_secs(milestone.estimate_secs),
            milestone.title
        ));
        if !milestone.algorithmic_complexity.is_empty() {
            out.push_str(&format!(
                "  algorithm: {}\n",
                milestone.algorithmic_complexity
            ));
        }
        for todo in &milestone.todos {
            out.push_str(&format!("  - [ ] {todo}\n"));
        }
    }
    out
}

fn normalise_questions(questions: Vec<Question>) -> Vec<Question> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (index, mut question) in questions.into_iter().enumerate() {
        let text = question.question.trim().to_string();
        if text.is_empty() {
            continue;
        }
        question.question = text;
        let mut id = question.id.trim().to_string();
        if id.is_empty() || seen.contains(&id) {
            id = format!("q{}", index + 1);
        }
        while seen.contains(&id) {
            id.push('_');
        }
        seen.insert(id.clone());
        question.id = id;
        question.why = question
            .why
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty());
        question.default = question
            .default
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty());
        out.push(question);
    }
    out
}

fn clean_list(items: Vec<String>) -> Vec<String> {
    items
        .into_iter()
        .map(|i| i.trim().to_string())
        .filter(|i| !i.is_empty())
        .collect()
}

fn default_acceptance() -> Vec<String> {
    vec![
        "The project builds and its test suite passes".to_string(),
        "A README explains what it does and how to run it".to_string(),
        "The work is committed and pushed to the private remote".to_string(),
    ]
}

/// Markdown rendering of all questions, written to `QUESTIONS.md`.
pub fn render_questions_markdown(specs: &[ProjectSpec]) -> String {
    let mut out = String::from("# Questions from the orchestrator\n\n");
    out.push_str(
        "Answer in `answers.toml` (path printed by `mox plan`), then run \
         `mox plan --answers <file>`.\n",
    );
    for spec in specs {
        out.push_str(&format!(
            "\n## {} (`{}`)\n\n{}\n\n",
            spec.title,
            spec.slug,
            if spec.summary.is_empty() {
                spec.idea.as_str()
            } else {
                spec.summary.as_str()
            }
        ));
        if spec.questions.is_empty() {
            out.push_str("_No blocking questions._\n");
            continue;
        }
        for question in &spec.questions {
            out.push_str(&format!("- **{}** {}\n", question.id, question.question));
            if let Some(why) = &question.why {
                out.push_str(&format!("  - Why: {why}\n"));
            }
            if let Some(default) = &question.default {
                out.push_str(&format!("  - Default: {default}\n"));
            }
        }
    }
    out
}

/// TOML template the user can edit.
pub fn render_answers_template(specs: &[ProjectSpec]) -> String {
    let mut out = String::from(
        "# Answers for the meta-orchestrator.\n\
         # Fill what you care about; blank answers fall back to the default.\n\n",
    );
    for spec in specs {
        out.push_str(&format!("[projects.{}]\n", spec.slug));
        if spec.questions.is_empty() {
            out.push_str("# (no questions)\n\n");
            continue;
        }
        for question in &spec.questions {
            out.push_str(&format!("# {}\n", question.question));
            if let Some(default) = &question.default {
                out.push_str(&format!("# default: {}\n", default));
            }
            out.push_str(&format!("{} = \"\"\n", question.id));
        }
        out.push('\n');
    }
    out
}

/// True when the answers table covers at least one question.
pub fn answers_are_empty(specs: &[ProjectSpec]) -> bool {
    specs
        .iter()
        .all(|s| s.answers.values().all(|a| a.trim().is_empty()))
}

/// Convenience for tests and callers that only have a path.
pub fn agents_path_display(path: &Path) -> String {
    crate::util::display_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ProjectSpec;

    fn idea(title: &str, details: &str) -> Idea {
        Idea {
            title: Some(title.into()),
            details: details.into(),
            raw: details.into(),
        }
    }

    #[test]
    fn questions_prompt_lists_every_idea_and_schema() {
        let cfg = Config::default();
        let prompt = questions_prompt(
            &cfg,
            &[idea("One", "first idea"), idea("Two", "second idea")],
        );
        assert!(prompt.contains("first idea"));
        assert!(prompt.contains("second idea"));
        assert!(prompt.contains("\"questions\""));
        assert!(prompt.contains("\"compute\""));
        assert!(prompt.contains("PRIVATE"));
        assert!(prompt.contains("\"milestones\""));
        assert!(prompt.contains("\"estimate_minutes\""));
        assert!(prompt.contains("\"algorithmic_complexity\""));
    }

    #[test]
    fn execution_prompt_contains_constraints_and_answers() {
        let cfg = Config {
            projects_root: "/tmp/projects".into(),
            ..Config::default()
        };
        let mut spec = ProjectSpec::from_idea(&idea("Tool", "do a thing"), "tool".into());
        spec.questions.push(Question {
            id: "q1".into(),
            question: "Which database?".into(),
            why: None,
            default: None,
        });
        spec.answers.insert("q1".into(), "SQLite".into());
        let prompt = execution_prompt(&cfg, &spec);
        assert!(prompt.contains("Which database?"));
        assert!(prompt.contains("SQLite"));
        assert!(prompt.contains("PRIVATE repository"));
        assert!(prompt.contains("CARGO_TARGET_DIR"));
        assert!(prompt.contains("STATUS.md"));
        assert!(prompt.contains("CHECKS.md"));
        assert!(prompt.contains("due: +2h"));
        assert!(prompt.contains("TDD is mandatory"));
        assert!(prompt.contains("wall-clock cap"));
        assert!(prompt.contains("run in parallel"));
        assert!(prompt.contains("mox vps allow"));
        assert!(prompt.contains("mox guard wrappers"));
        assert!(prompt.contains("sub-agents"));
        assert!(prompt.contains("7200 seconds"));
        assert!(prompt.contains("Commit it"));
        assert!(prompt.contains("mox milestone start"));
        assert!(prompt.contains("Milestone plan"));
        assert!(prompt.contains("/tmp/projects/tool"));
        assert!(prompt.contains("AGENTS-GLOBAL.md"));
    }

    #[test]
    fn parses_projects_and_single_object() {
        let multi =
            parse_brain_response(r#"blah {"projects": [{"title": "A", "compute": "heavy"}]} blah"#)
                .unwrap();
        assert_eq!(multi.len(), 1);
        assert_eq!(multi[0].title.as_deref(), Some("A"));
        let single = parse_brain_response(r#"{"title": "B", "questions": []}"#).unwrap();
        assert_eq!(single[0].title.as_deref(), Some("B"));
    }

    #[test]
    fn rejects_non_json_and_empty_projects() {
        assert!(parse_brain_response("no json").is_err());
        assert!(parse_brain_response(r#"{"projects": []}"#).is_err());
        assert!(parse_brain_response(r#"{"other": 1}"#).is_err());
    }

    #[test]
    fn merge_keeps_one_spec_per_idea_and_order() {
        let ideas = vec![idea("Alpha", "a"), idea("Beta", "b"), idea("Gamma", "c")];
        let assessed = parse_brain_response(
            r#"{"projects": [
                {"slug": "beta-thing", "title": "Beta!", "questions": []},
                {"slug": "alpha-thing", "title": "Alpha!", "questions": []}
            ]}"#,
        )
        .unwrap();
        let specs = merge_specs(&ideas, assessed);
        assert_eq!(specs.len(), 3);
        assert_eq!(specs[0].title, "Alpha!");
        assert_eq!(specs[0].slug, "alpha-thing");
        assert_eq!(specs[1].title, "Beta!");
        assert_eq!(specs[2].slug, "gamma"); // fallback
    }

    #[test]
    fn merge_normalises_milestones_and_falls_back() {
        let ideas = vec![idea("X", "x")];
        let assessed = parse_brain_response(
            r#"{"projects": [{
                "slug": "x",
                "milestones": [
                    {"title": "Foundation", "todos": ["write test"], "complexity": "low", "estimate_minutes": 30, "algorithmic_complexity": "O(1)"},
                    {"title": "Core", "complexity": "very-high", "estimate_minutes": 999},
                    {"title": "  "}
                ]
            }]}"#,
        )
        .unwrap();
        let specs = merge_specs(&ideas, assessed);
        let milestones = &specs[0].milestones;
        assert_eq!(milestones.len(), 2);
        assert_eq!(milestones[0].id, "m1");
        assert_eq!(milestones[1].id, "m2");
        assert_eq!(milestones[0].estimate_secs, 1800);
        assert_eq!(milestones[1].estimate_secs, 480 * 60);
        assert_eq!(milestones[1].complexity, Complexity::VeryHigh);
        assert_eq!(milestones[0].algorithmic_complexity, "O(1)");

        let specs = merge_specs(&ideas_v2(), Vec::new());
        assert_eq!(specs[0].milestones.len(), 3);
        assert!(specs[0].milestones[0]
            .todos
            .iter()
            .any(|t| t.contains("failing")));
    }

    fn ideas_v2() -> Vec<Idea> {
        vec![idea("Y", "y")]
    }

    #[test]
    fn merge_makes_slugs_unique() {
        let ideas = vec![idea("Same", "a"), idea("Same", "b")];
        let specs = merge_specs(&ideas, Vec::new());
        assert_eq!(specs[0].slug, "same");
        assert_eq!(specs[1].slug, "same-2");
    }

    #[test]
    fn merge_normalises_questions_and_defaults() {
        let ideas = vec![idea("X", "x")];
        let assessed = parse_brain_response(
            r#"{"projects": [{
                "slug": "x",
                "questions": [
                    {"id": "", "question": "First?"},
                    {"id": "", "question": "Second?"},
                    {"id": "q1", "question": "Third?"},
                    {"id": "q4", "question": "   "}
                ],
                "compute": "GPU",
                "estimated_disk_mb": 1
            }]}"#,
        )
        .unwrap();
        let specs = merge_specs(&ideas, assessed);
        let spec = &specs[0];
        assert_eq!(spec.questions.len(), 3);
        let ids: Vec<&str> = spec.questions.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(ids, vec!["q1", "q2", "q3"]);
        assert_eq!(spec.compute, ComputeNeed::Gpu);
        assert!(spec.needs_vps);
        assert_eq!(spec.estimated_disk_mb, 64);
        assert!(!spec.acceptance_criteria.is_empty());
    }

    #[test]
    fn answers_template_is_valid_toml() {
        let specs = vec![ProjectSpec::from_idea(&idea("X", "x"), "x".into())];
        let template = render_answers_template(&specs);
        let parsed: toml::Value = toml::from_str(&template).unwrap();
        assert!(parsed.get("projects").is_some());
    }

    #[test]
    fn questions_markdown_mentions_every_question() {
        let mut spec = ProjectSpec::from_idea(&idea("X", "x"), "x".into());
        spec.questions.push(Question {
            id: "q1".into(),
            question: "Which database?".into(),
            why: Some("storage choice".into()),
            default: Some("SQLite".into()),
        });
        let md = render_questions_markdown(&specs(&spec));
        assert!(md.contains("Which database?"));
        assert!(md.contains("storage choice"));
        assert!(md.contains("SQLite"));
        fn specs(spec: &ProjectSpec) -> Vec<ProjectSpec> {
            vec![spec.clone()]
        }
    }
}
