//! Small, dependency-free helpers shared by the whole crate.
//!
//! Everything here is pure (no I/O) except the path helpers, which only read
//! environment variables, so it is all trivially unit-testable.

use std::path::{Path, PathBuf};

/// Maximum length of a generated project slug.
pub const SLUG_MAX_LEN: usize = 48;

/// Turn free-form text into a GitHub-safe slug.
///
/// Rules: ASCII lowercase, every run of non-alphanumeric characters becomes a
/// single `-`, leading/trailing dashes are trimmed, and the result is capped at
/// [`SLUG_MAX_LEN`] characters. An empty result falls back to `project`.
pub fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut last_dash = true; // suppress a leading dash
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= SLUG_MAX_LEN {
            break;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed
    }
}

/// Derive a slug from an idea: use the explicit title when present, else the
/// first few significant words of the idea text.
pub fn slug_from_idea(title: Option<&str>, fallback_text: &str) -> String {
    let source = title
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(fallback_text);
    let slug = slugify(source);
    if slug == "project" {
        let words: String = fallback_text
            .split_whitespace()
            .take(6)
            .collect::<Vec<_>>()
            .join(" ");
        slugify(&words)
    } else {
        slug
    }
}

/// Ensure `slug` is unique against `taken`, appending `-2`, `-3`, ...
pub fn unique_slug(slug: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == slug) {
        return slug.to_string();
    }
    for n in 2..10_000u32 {
        let candidate = format!("{slug}-{n}");
        if !taken.iter().any(|t| t == &candidate) {
            return candidate;
        }
    }
    format!("{slug}-{}", timestamp_millis())
}

/// Expand a leading `~` using `$HOME`. Leaves other paths untouched.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    PathBuf::from(path)
}

/// `$HOME`, falling back to `/root` only if the variable is absent (it never is
/// in the environments this tool runs in).
pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/root"))
}

/// Compact a path for display, replacing `$HOME` with `~`.
pub fn display_path(path: &Path) -> String {
    let home = home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) => {
            if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            }
        }
        Err(_) => path.display().to_string(),
    }
}

/// RFC3339 UTC timestamp with second precision.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Milliseconds since the Unix epoch (used for unique temp names).
pub fn timestamp_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Human-readable byte count (base 1024, one decimal).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Human-readable MiB value.
pub fn human_mb(mb: u64) -> String {
    human_bytes(mb.saturating_mul(1024 * 1024))
}

/// Extract the first balanced JSON object from arbitrary text.
///
/// LLMs often wrap JSON in prose or fenced code blocks. This scans for the
/// first `{` and returns through its matching `}`, ignoring braces inside
/// strings (with escape handling) and nested objects.
pub fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &b) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Quote one argument for display/logging (not for shell execution; we always
/// spawn processes without a shell).
pub fn quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_-./:=,@+".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}

/// Render a command for logs, redacting nothing because we never place secrets
/// on command lines. Used for dry-run output and test assertions.
pub fn render_command(program: &str, args: &[String]) -> String {
    let mut out = quote_arg(program);
    for arg in args {
        out.push(' ');
        out.push_str(&quote_arg(arg));
    }
    out
}

/// Read at most the last `n` lines of a file, returning an empty vec when the
/// file is missing. Never loads the whole file.
pub fn tail_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let window = 64 * 1024u64;
    let start = len.saturating_sub(window);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let mut lines: Vec<String> = buf.lines().map(str::to_string).collect();
    if lines.len() > n {
        lines.drain(0..lines.len() - n);
    }
    Ok(lines)
}

/// Truncate a string to `max` characters, appending an ellipsis when cut.
pub fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// True when `path` is inside `root` after lexical normalisation and neither
/// path escapes through `..` components. This is a guard for destructive
/// operations; it deliberately does not resolve symlinks.
pub fn is_inside(root: &Path, path: &Path) -> bool {
    fn normalise(p: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for comp in p.components() {
            match comp {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        out
    }
    let root = normalise(root);
    let path = normalise(path);
    path.starts_with(&root) && path != root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello World!"), "hello-world");
        assert_eq!(slugify("  --Weird__name--  "), "weird-name");
        assert_eq!(slugify("Rust + OpenCode = ❤"), "rust-opencode");
        assert_eq!(slugify(""), "project");
        assert_eq!(slugify("!!!"), "project");
    }

    #[test]
    fn slugify_is_bounded() {
        let long = "a".repeat(200);
        assert_eq!(slugify(&long).len(), SLUG_MAX_LEN);
        assert!(!slugify(&format!("{}-", "b".repeat(200))).ends_with('-'));
    }

    #[test]
    fn slug_from_idea_prefers_title() {
        assert_eq!(
            slug_from_idea(Some("My Tool"), "some long description"),
            "my-tool"
        );
        assert_eq!(
            slug_from_idea(None, "A CLI to track expenses quickly"),
            "a-cli-to-track-expenses-quickly"
        );
    }

    #[test]
    fn unique_slug_appends_counter() {
        let taken = vec!["foo".to_string(), "foo-2".to_string()];
        assert_eq!(unique_slug("foo", &taken), "foo-3");
        assert_eq!(unique_slug("bar", &taken), "bar");
    }

    #[test]
    fn extract_json_handles_prose_and_strings() {
        let text = r#"Sure! Here is the plan:
```json
{"projects": [{"summary": "uses } and { inside", "n": 1}], "ok": true}
```
Thanks."#;
        let json = extract_json_object(text).unwrap();
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(value["projects"][0]["summary"], "uses } and { inside");
    }

    #[test]
    fn extract_json_returns_none_without_object() {
        assert!(extract_json_object("no json here").is_none());
        assert!(extract_json_object("{unbalanced").is_none());
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_mb(2048), "2.0 GiB");
    }

    #[test]
    fn quoting_round_trip_style() {
        assert_eq!(quote_arg("simple"), "simple");
        assert_eq!(quote_arg("with space"), "'with space'");
        assert_eq!(
            render_command("taskset", &["-c".into(), "0,1".into()]),
            "taskset -c 0,1"
        );
    }

    #[test]
    fn truncates_on_char_boundary() {
        assert_eq!(truncate_chars("héllo", 10), "héllo");
        assert_eq!(truncate_chars("héllo", 3), "hé…");
    }

    #[test]
    fn inside_root_detection() {
        let root = Path::new("/home/u/projects");
        assert!(is_inside(root, Path::new("/home/u/projects/a")));
        assert!(!is_inside(root, Path::new("/home/u/projects")));
        assert!(!is_inside(root, Path::new("/home/u/other")));
        assert!(!is_inside(root, Path::new("/home/u/projects/../secret")));
    }

    #[test]
    fn tilde_expansion() {
        let home = home_dir();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/x"), home.join("x"));
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
    }
}
