//! Project-level effort estimates.
//!
//! The planning brain is asked for a difficulty and a total token budget before
//! it writes the milestone plan. These helpers keep the display and fallback
//! logic deterministic: a missing token estimate is derived from the planned
//! milestone minutes, never from runtime measurements.

use crate::config::Config;
use crate::model::{Complexity, MilestoneSpec, ProjectSpec};

/// Fallback token burn when the model does not return a token estimate.
pub const FALLBACK_TOKENS_PER_MINUTE: u64 = 2_000;
pub const MIN_TOKENS: u64 = 2_000;
pub const MAX_TOKENS: u64 = 50_000_000;

/// Total planned seconds for a project, using the same fallback milestones as
/// `mox up` when the spec has none.
pub fn planned_secs(spec: &ProjectSpec) -> u64 {
    let milestones = if spec.milestones.is_empty() {
        crate::milestones::fallback_for(spec)
    } else {
        spec.milestones.clone()
    };
    milestones.iter().map(|m| m.estimate_secs).sum()
}

/// Deterministic fallback from planned minutes to a token budget.
pub fn tokens_from_secs(secs: u64) -> u64 {
    let minutes = secs.saturating_add(59) / 60;
    minutes
        .saturating_mul(FALLBACK_TOKENS_PER_MINUTE)
        .clamp(MIN_TOKENS, MAX_TOKENS)
}

/// The estimate to display: the brain's value when present, otherwise the
/// deterministic fallback.
pub fn planned_tokens(spec: &ProjectSpec) -> u64 {
    if spec.estimated_tokens > 0 {
        spec.estimated_tokens.clamp(MIN_TOKENS, MAX_TOKENS)
    } else {
        tokens_from_secs(planned_secs(spec))
    }
}

/// Clamp a model-provided token estimate to a sane range.
pub fn clamp_tokens(tokens: u64) -> u64 {
    tokens.clamp(MIN_TOKENS, MAX_TOKENS)
}

/// Infer a project difficulty from its milestones (used for adopted projects
/// and as a fallback when the brain omits the field).
pub fn difficulty_from_milestones(milestones: &[MilestoneSpec]) -> Complexity {
    milestones
        .iter()
        .map(|m| m.complexity)
        .max_by_key(|c| c.weight())
        .unwrap_or(Complexity::Medium)
}

/// Model label used only in token estimates.
pub fn model_label(spec: &ProjectSpec, cfg: &Config) -> String {
    let spec_model = spec.token_estimate_model.trim();
    if !spec_model.is_empty() {
        return spec_model.to_string();
    }
    let configured = cfg.estimate_model.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    "current OpenCode model".to_string()
}

/// One-line summary used by `mox plan`, `mox up` and `TASK.md`.
pub fn describe(spec: &ProjectSpec, cfg: &Config) -> String {
    format!(
        "{} \u{00b7} {} \u{00b7} ~{} tokens ({})",
        spec.difficulty.as_str(),
        crate::util::format_secs(planned_secs(spec)),
        crate::util::format_tokens(planned_tokens(spec)),
        model_label(spec, cfg)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Idea;

    fn spec() -> ProjectSpec {
        ProjectSpec::from_idea(
            &Idea {
                title: Some("Demo".into()),
                details: "demo".into(),
                raw: String::new(),
            },
            "demo".into(),
        )
    }

    #[test]
    fn planned_secs_sums_milestones_and_falls_back() {
        let mut spec = spec();
        assert_eq!(planned_secs(&spec), 6_600);
        spec.milestones = vec![MilestoneSpec {
            estimate_secs: 600,
            ..MilestoneSpec::default()
        }];
        assert_eq!(planned_secs(&spec), 600);
    }

    #[test]
    fn token_estimates_are_bounded_and_have_a_fallback() {
        let mut spec = spec();
        assert_eq!(planned_tokens(&spec), 220_000);
        spec.estimated_tokens = 1;
        assert_eq!(planned_tokens(&spec), MIN_TOKENS);
        spec.estimated_tokens = MAX_TOKENS + 1;
        assert_eq!(planned_tokens(&spec), MAX_TOKENS);
        assert_eq!(clamp_tokens(0), MIN_TOKENS);
    }

    #[test]
    fn difficulty_uses_the_hardest_milestone() {
        let milestones = vec![
            MilestoneSpec {
                complexity: Complexity::Low,
                ..MilestoneSpec::default()
            },
            MilestoneSpec {
                complexity: Complexity::VeryHigh,
                ..MilestoneSpec::default()
            },
        ];
        assert_eq!(
            difficulty_from_milestones(&milestones),
            Complexity::VeryHigh
        );
        assert_eq!(difficulty_from_milestones(&[]), Complexity::Medium);
    }

    #[test]
    fn model_label_prefers_the_spec_then_the_config() {
        let mut spec = spec();
        let mut cfg = Config {
            estimate_model: "deepseek-v4.1-flash".into(),
            ..Config::default()
        };
        assert_eq!(model_label(&spec, &cfg), "deepseek-v4.1-flash");
        spec.token_estimate_model = "other/model".into();
        assert_eq!(model_label(&spec, &cfg), "other/model");
        cfg.estimate_model.clear();
        spec.token_estimate_model.clear();
        assert_eq!(model_label(&spec, &cfg), "current OpenCode model");
    }

    #[test]
    fn describe_is_compact() {
        let spec = spec();
        let cfg = Config::default();
        let text = describe(&spec, &cfg);
        assert!(text.contains("medium"));
        assert!(text.contains("1h50m"));
        assert!(text.contains("220k"));
        assert!(text.contains("deepseek-v4.1-flash"));
    }
}
