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

/// Stable short hash (FNV-1a, 12 hex chars) for content change detection.
pub fn short_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")[..12].to_string()
}

/// Compact human duration: `45s`, `12m`, `2h05m`, `3d4h`.
pub fn format_secs(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 24 {
        let rem = minutes % 60;
        return if rem == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{rem:02}m")
        };
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if rem_hours == 0 {
        format!("{days}d")
    } else {
        format!("{days}d{rem_hours}h")
    }
}

/// Compact token count: `950`, `2k`, `220k`, `1.2M`.
pub fn format_tokens(tokens: u64) -> String {
    let value = tokens as f64;
    if tokens >= 1_000_000 {
        let text = format!("{:.1}", value / 1_000_000.0);
        return format!("{}M", text.strip_suffix(".0").unwrap_or(&text));
    }
    if tokens >= 1_000 {
        let text = format!("{:.1}", value / 1_000.0);
        return format!("{}k", text.strip_suffix(".0").unwrap_or(&text));
    }
    tokens.to_string()
}

/// Standard base64 (with padding). Used to pass long prompts through `ssh`
/// without shell-quoting hazards.
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[((n >> 18) & 63) as usize] as char);
        out.push(TABLE[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
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

/// Set a file or directory mode (no-op on non-Unix platforms).
#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Create a directory (and parents) with mode 0700.
pub fn ensure_dir_private(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    set_mode(path, 0o700)
}

/// Write a file with mode 0600 (used for state, logs, prompts, checks).
pub fn write_private(path: &Path, contents: impl AsRef<[u8]>) -> std::io::Result<()> {
    std::fs::write(path, contents)?;
    set_mode(path, 0o600)
}

/// Open a file for appending, creating it with mode 0600 and enforcing the mode
/// even if it already existed with looser permissions.
pub fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::fs::OpenOptions;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(not(unix))]
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let _ = set_mode(path, 0o600);
    Ok(file)
}

/// Resolve an executable name against `PATH` (absolute paths are returned as-is).
pub fn resolve_executable(name: &str) -> PathBuf {
    let path = Path::new(name);
    if path.is_absolute() || name.contains('/') {
        return path.to_path_buf();
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    path.to_path_buf()
}

/// Current `PATH` with `dir` prepended.
pub fn path_with_prefix(dir: &Path) -> String {
    let existing = std::env::var("PATH").unwrap_or_default();
    format!("{}:{}", dir.display(), existing)
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
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[test]
    fn short_hash_is_stable() {
        assert_eq!(short_hash("abc"), short_hash("abc"));
        assert_ne!(short_hash("abc"), short_hash("abd"));
        assert_eq!(short_hash("abc").len(), 12);
    }

    #[test]
    fn human_durations() {
        assert_eq!(format_secs(42), "42s");
        assert_eq!(format_secs(90), "1m");
        assert_eq!(format_secs(3600), "1h");
        assert_eq!(format_secs(7500), "2h05m");
        assert_eq!(format_secs(90_000), "1d1h");
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_mb(2048), "2.0 GiB");
    }

    #[test]
    fn compact_token_counts() {
        assert_eq!(format_tokens(950), "950");
        assert_eq!(format_tokens(2_000), "2k");
        assert_eq!(format_tokens(12_500), "12.5k");
        assert_eq!(format_tokens(220_000), "220k");
        assert_eq!(format_tokens(1_250_000), "1.2M");
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
    fn private_helpers_set_modes() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().unwrap();
            let nested = dir.path().join("a/b");
            ensure_dir_private(&nested).unwrap();
            assert_eq!(
                std::fs::metadata(&nested).unwrap().permissions().mode() & 0o777,
                0o700
            );
            let file = nested.join("secret.txt");
            write_private(&file, b"x").unwrap();
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let append = nested.join("log.txt");
            open_private_append(&append).unwrap();
            assert_eq!(
                std::fs::metadata(&append).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn resolves_executables_from_path() {
        let resolved = resolve_executable("sh");
        assert!(resolved.is_absolute() || resolved == Path::new("sh"));
        assert_eq!(resolve_executable("/bin/sh"), PathBuf::from("/bin/sh"));
        assert!(path_with_prefix(Path::new("/tmp/g")).starts_with("/tmp/g:"));
    }

    #[test]
    fn tilde_expansion() {
        let home = home_dir();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/x"), home.join("x"));
        assert_eq!(expand_tilde("/abs"), PathBuf::from("/abs"));
    }
}
