//! `meta-orchestrator` (binary: `mox`) turns rough project ideas into
//! clarifying questions, specifications, private GitHub repositories and
//! isolated, parallel OpenCode agents.
//!
//! Layers:
//! * [`exec`] — the only place that spawns processes (fakeable in tests);
//! * [`brain`] — the planning model (real OpenCode or scripted in tests);
//! * [`prompts`] — prompt text and defensive parsing of the brain's JSON;
//! * [`scheduler`] / [`resources`] — CPU pinning and disk safety;
//! * [`scaffold`] / [`github`] / [`runner`] — the side-effecting workflow;
//! * [`app`] / [`cli`] — command implementations and argument parsing.

pub mod app;
pub mod brain;
pub mod checks;
pub mod clean;
pub mod cli;
pub mod config;
pub mod exec;
pub mod github;
pub mod guard;
pub mod ideas;
pub mod lock;
pub mod milestones;
pub mod model;
pub mod notify;
pub mod prompts;
pub mod resources;
pub mod runner;
pub mod scaffold;
pub mod scheduler;
pub mod state;
pub mod util;
pub mod verify;
pub mod vps;

pub use config::Config;
pub use model::{ComputeNeed, Idea, Plan, ProjectSpec, ProjectState, ProjectStatus, Question};
