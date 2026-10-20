//! Parsing and validation of the user's idea list.
//!
//! Accepted input format (all lines optional, order preserved):
//!
//! ```text
//! # comments are ignored
//! - A CLI that tracks expenses :: optional title before "::"
//!   continuation lines are appended to the previous idea
//! - Another idea
//! ```

use crate::model::Idea;
use anyhow::{bail, Context, Result};
use std::path::Path;

/// Parse an ideas file's content into [`Idea`]s.
///
/// * Blank lines and `#` comments are ignored.
/// * A leading `- ` or `* ` marks the start of a new idea.
/// * `Title :: details` sets an explicit title.
/// * Any other non-empty line is a continuation of the current idea (or a new
///   idea when no idea is open).
pub fn parse_ideas(content: &str) -> Result<Vec<Idea>> {
    struct Pending {
        title: Option<String>,
        details: String,
        from_bullet: bool,
    }

    let mut ideas: Vec<Idea> = Vec::new();
    let mut current: Option<Pending> = None;

    fn flush(current: &mut Option<Pending>, ideas: &mut Vec<Idea>) {
        if let Some(pending) = current.take() {
            let details = pending.details.trim().to_string();
            if !details.is_empty() || pending.title.is_some() {
                let raw = match &pending.title {
                    Some(t) => format!("{t} :: {details}"),
                    None => details.clone(),
                };
                ideas.push(Idea {
                    title: pending.title,
                    details,
                    raw,
                });
            }
        }
    }

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush(&mut current, &mut ideas);
            continue;
        }
        if trimmed.starts_with('#') || trimmed.starts_with("//") {
            continue;
        }
        let bullet = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "));

        if let Some(rest) = bullet {
            flush(&mut current, &mut ideas);
            let (title, details) = split_title(rest);
            current = Some(Pending {
                title,
                details,
                from_bullet: true,
            });
        } else if let Some(pending) = &mut current {
            let indented = line.starts_with(' ') || line.starts_with('\t');
            if pending.from_bullet || indented {
                if !pending.details.is_empty() {
                    pending.details.push(' ');
                }
                pending.details.push_str(trimmed);
            } else {
                // A new unindented paragraph starts a new idea.
                flush(&mut current, &mut ideas);
                let (title, details) = split_title(trimmed);
                current = Some(Pending {
                    title,
                    details,
                    from_bullet: false,
                });
            }
        } else {
            let (title, details) = split_title(trimmed);
            current = Some(Pending {
                title,
                details,
                from_bullet: false,
            });
        }
    }
    flush(&mut current, &mut ideas);

    if ideas.is_empty() {
        bail!("no project ideas found (expected at least one non-empty line)");
    }
    for idea in &ideas {
        if idea.details.trim().is_empty() && idea.title.is_none() {
            bail!("an idea has no description");
        }
    }
    Ok(ideas)
}

fn split_title(text: &str) -> (Option<String>, String) {
    if let Some((title, details)) = text.split_once("::") {
        let title = title.trim();
        let details = details.trim();
        if !title.is_empty() && !details.is_empty() {
            return (Some(title.to_string()), details.to_string());
        }
    }
    (None, text.trim().to_string())
}

/// Stable content hash used to detect changes to an ideas file.
pub fn content_hash(content: &str) -> String {
    crate::util::short_hash(content)
}

/// Hash an ideas file; `None` for stdin or unreadable files.
pub fn file_hash(path: &Path) -> Option<String> {
    if path == Path::new("-") {
        return None;
    }
    std::fs::read_to_string(path)
        .ok()
        .map(|text| content_hash(&text))
}

/// Read and parse an ideas file. `-` means standard input.
pub fn load_ideas(path: &Path) -> Result<Vec<Idea>> {
    let content = if path == Path::new("-") {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading ideas from stdin")?;
        buf
    } else {
        std::fs::read_to_string(path)
            .with_context(|| format!("reading ideas file {}", path.display()))?
    };
    parse_ideas(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bullets_and_comments() {
        let ideas = parse_ideas(
            r#"
# my ideas
- Expense tracker :: A CLI to log expenses and produce monthly reports
- Static site generator for notes
"#,
        )
        .unwrap();
        assert_eq!(ideas.len(), 2);
        assert_eq!(ideas[0].title.as_deref(), Some("Expense tracker"));
        assert_eq!(
            ideas[0].details,
            "A CLI to log expenses and produce monthly reports"
        );
        assert_eq!(ideas[1].title, None);
        assert_eq!(ideas[1].details, "Static site generator for notes");
    }

    #[test]
    fn joins_continuation_lines() {
        let ideas =
            parse_ideas("- A monitoring tool\n  for my VPS\n  with alerts\n- Second idea").unwrap();
        assert_eq!(ideas.len(), 2);
        assert_eq!(ideas[0].details, "A monitoring tool for my VPS with alerts");
    }

    #[test]
    fn accepts_plain_lines() {
        let ideas = parse_ideas("Build a recipe manager\nBuild a budget app\n").unwrap();
        assert_eq!(ideas.len(), 2);
    }

    #[test]
    fn bullet_without_details_is_details() {
        let ideas = parse_ideas("- Just a title\n").unwrap();
        assert_eq!(ideas.len(), 1);
        assert_eq!(ideas[0].title, None);
        assert_eq!(ideas[0].details, "Just a title");
    }

    #[test]
    fn empty_input_is_an_error() {
        let err = parse_ideas("\n# only comments\n").unwrap_err();
        assert!(err.to_string().contains("no project ideas"));
    }

    #[test]
    fn content_hash_detects_changes() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
        assert!(content_hash("").len() >= 8);
    }

    #[test]
    fn double_colon_without_title_is_plain_text() {
        let ideas = parse_ideas("- :: bad\n").unwrap();
        assert_eq!(ideas[0].title, None);
        assert_eq!(ideas[0].details, ":: bad");
    }
}
