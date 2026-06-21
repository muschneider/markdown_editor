//! Persistence of the per-tag Markdown color configuration.
//!
//! The config file lives at `$XDG_CONFIG_HOME/md_editor/config.yaml` (falling
//! back to `$HOME/.config/md_editor/config.yaml` on Linux), and is a flat YAML
//! map of tag-name → hex-string:
//!
//! ```yaml
//! # Markdown Editor color configuration
//! # Empty values follow the selected theme's palette.
//! h1: "#ff0000"
//! h2: ""
//! strong: "#00ff00"
//! ```
//!
//! The format is simple enough (a flat list of `key: value` string pairs) that
//! it is parsed and serialized by hand here — no YAML dependency needed. This
//! keeps the dependency tree small and avoids the deprecated `serde_yaml` crate.

use std::fs;
use std::path::PathBuf;

use crate::theme::MarkdownColors;

/// Header comment written at the top of every config file so users editing it
/// by hand know what empty values mean.
const FILE_HEADER: &str = "# Markdown Editor color configuration\n\
                           # Empty values follow the selected theme's palette.\n";

/// Resolve the config file path.
///
/// Honors `XDG_CONFIG_HOME` when set and absolute; otherwise falls back to
/// `$HOME/.config`. On systems where neither is set, returns `None` and config
/// is silently disabled (defaults are used, saves are skipped).
pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|s| {
            let p = PathBuf::from(s);
            p.is_absolute()
        })
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| {
                let mut p = PathBuf::from(home);
                p.push(".config");
                p
            })
        })?;

    let mut path = base;
    path.push("md_editor");
    path.push("config.yaml");
    Some(path)
}

/// Load the color configuration from disk.
///
/// Returns [`MarkdownColors::default`] when the file is missing, unreadable, or
/// malformed — the editor always starts in a usable state, never blocked by a
/// bad config. Malformed lines are skipped (a warning is not surfaced to avoid
/// pulling in a logging dependency).
pub fn load() -> MarkdownColors {
    let path = match config_path() {
        Some(p) => p,
        None => return MarkdownColors::default(),
    };

    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return MarkdownColors::default(),
    };

    parse(&contents)
}

/// Save the color configuration to disk (fire-and-forget).
///
/// Creates the parent directory if it does not exist. Errors are silently
/// ignored — a failed save must never crash the editor or block the UI.
pub fn save(colors: &MarkdownColors) {
    let path = match config_path() {
        Some(p) => p,
        None => return,
    };

    let contents = serialize(colors);

    // Write off the UI thread: the file is tiny but the fs call could still
    // block on a slow disk or a hung mount. Mirrors how `LinkClicked` offloads
    // `webbrowser::open` to a spawned thread.
    std::thread::spawn(move || {
        let _ = fs::create_dir_all(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(&path),
        );
        let _ = fs::write(&path, contents);
    });
}

/// Serialize [`MarkdownColors`] into the YAML text written to disk.
///
/// Every field is emitted, quoted with double quotes so empty strings and
/// values containing `#` are preserved correctly.
fn serialize(colors: &MarkdownColors) -> String {
    let mut out = String::from(FILE_HEADER);

    for (key, value) in entries(colors) {
        out.push_str(key);
        out.push_str(": \"");
        out.push_str(value);
        out.push_str("\"\n");
    }

    out
}

/// Parse YAML config text into [`MarkdownColors`].
///
/// Recognizes `key: value` lines where value may be quoted (`"…"` / `'…'`) or
/// bare. Empty values (no value after the colon) map to empty strings. Unknown
/// keys and blank/comment lines are skipped so users can add their own notes.
fn parse(text: &str) -> MarkdownColors {
    let mut colors = MarkdownColors::default();

    for line in text.lines() {
        let trimmed = line.trim();

        // Skip blank lines and comments.
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Split on the first colon.
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };

        let key = key.trim();
        let value = value.trim();

        // Strip a single layer of surrounding quotes.
        let value = strip_quotes(value);

        assign(&mut colors, key, value);
    }

    colors
}

/// Strip one layer of surrounding double or single quotes from `value`.
fn strip_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Borrow each field as `(key_name, value)` in declaration order, used by
/// [`serialize`].
fn entries(colors: &MarkdownColors) -> Vec<(&'static str, &str)> {
    vec![
        ("h1", colors.h1.as_str()),
        ("h2", colors.h2.as_str()),
        ("h3", colors.h3.as_str()),
        ("h4", colors.h4.as_str()),
        ("h5", colors.h5.as_str()),
        ("h6", colors.h6.as_str()),
        ("strong", colors.strong.as_str()),
        ("emphasis", colors.emphasis.as_str()),
        ("inline_code", colors.inline_code.as_str()),
        (
            "inline_code_background",
            colors.inline_code_background.as_str(),
        ),
        ("link", colors.link.as_str()),
        (
            "code_block_background",
            colors.code_block_background.as_str(),
        ),
        ("table_border", colors.table_border.as_str()),
        (
            "table_header_background",
            colors.table_header_background.as_str(),
        ),
        ("table_header_text", colors.table_header_text.as_str()),
    ]
}

/// Set the field named `key` to `value`, if it matches a known field.
/// Unknown keys are silently ignored.
fn assign(colors: &mut MarkdownColors, key: &str, value: &str) {
    match key {
        "h1" => colors.h1 = value.to_owned(),
        "h2" => colors.h2 = value.to_owned(),
        "h3" => colors.h3 = value.to_owned(),
        "h4" => colors.h4 = value.to_owned(),
        "h5" => colors.h5 = value.to_owned(),
        "h6" => colors.h6 = value.to_owned(),
        "strong" => colors.strong = value.to_owned(),
        "emphasis" => colors.emphasis = value.to_owned(),
        "inline_code" => colors.inline_code = value.to_owned(),
        "inline_code_background" => colors.inline_code_background = value.to_owned(),
        "link" => colors.link = value.to_owned(),
        "code_block_background" => colors.code_block_background = value.to_owned(),
        "table_border" => colors.table_border = value.to_owned(),
        "table_header_background" => colors.table_header_background = value.to_owned(),
        "table_header_text" => colors.table_header_text = value.to_owned(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_then_parse_round_trips() {
        let mut original = MarkdownColors::default();
        original.h1 = "#ff0000".to_owned();
        original.h3 = "#00ff00".to_owned();
        original.strong = "#abcdef".to_owned();
        original.table_border = "#1a2b3c".to_owned();

        let text = serialize(&original);
        let parsed = parse(&text);

        assert_eq!(parsed.h1, original.h1);
        assert_eq!(parsed.h3, original.h3);
        assert_eq!(parsed.strong, original.strong);
        assert_eq!(parsed.table_border, original.table_border);
        // Empty fields stay empty.
        assert_eq!(parsed.h2, "");
        assert_eq!(parsed.link, "");
    }

    #[test]
    fn parse_empty_string_returns_defaults() {
        let colors = parse("");
        assert_eq!(colors, MarkdownColors::default());
    }

    #[test]
    fn parse_skips_comments_and_blank_lines() {
        let text = "# This is a comment\n\
                    \n\
                    h1: \"#ff0000\"\n\
                    # another comment\n\
                    strong: \"#00ff00\"\n";
        let colors = parse(text);
        assert_eq!(colors.h1, "#ff0000");
        assert_eq!(colors.strong, "#00ff00");
    }

    #[test]
    fn parse_handles_unquoted_values() {
        let text = "h1: #ff0000\nstrong: #00ff00\n";
        let colors = parse(text);
        assert_eq!(colors.h1, "#ff0000");
        assert_eq!(colors.strong, "#00ff00");
    }

    #[test]
    fn parse_handles_single_quotes() {
        let text = "h1: '#ff0000'\n";
        let colors = parse(text);
        assert_eq!(colors.h1, "#ff0000");
    }

    #[test]
    fn parse_empty_value_means_follow_theme() {
        let text = "h1: \"\"\nstrong: \"#00ff00\"\n";
        let colors = parse(text);
        assert_eq!(colors.h1, "");
        assert_eq!(colors.strong, "#00ff00");
    }

    #[test]
    fn parse_bare_empty_value() {
        let text = "h1:\nstrong: \"#00ff00\"\n";
        let colors = parse(text);
        assert_eq!(colors.h1, "");
        assert_eq!(colors.strong, "#00ff00");
    }

    #[test]
    fn parse_ignores_unknown_keys() {
        let text = "h1: \"#ff0000\"\nunknown_key: \"whatever\"\nstrong: \"#00ff00\"\n";
        let colors = parse(text);
        assert_eq!(colors.h1, "#ff0000");
        assert_eq!(colors.strong, "#00ff00");
    }

    #[test]
    fn parse_skips_malformed_lines() {
        let text = "h1: \"#ff0000\"\nthis has no colon\nstrong: \"#00ff00\"\n";
        let colors = parse(text);
        assert_eq!(colors.h1, "#ff0000");
        assert_eq!(colors.strong, "#00ff00");
    }

    #[test]
    fn parse_all_fields() {
        let text = "\
h1: \"#111111\"
h2: \"#222222\"
h3: \"#333333\"
h4: \"#444444\"
h5: \"#555555\"
h6: \"#666666\"
strong: \"#777777\"
emphasis: \"#888888\"
inline_code: \"#999999\"
inline_code_background: \"#aaaaaa\"
link: \"#bbbbbb\"
code_block_background: \"#cccccc\"
table_border: \"#dddddd\"
table_header_background: \"#eeeeee\"
table_header_text: \"#ffffff\"
";
        let colors = parse(text);
        assert_eq!(colors.h1, "#111111");
        assert_eq!(colors.h6, "#666666");
        assert_eq!(colors.table_header_text, "#ffffff");
    }

    #[test]
    fn serialize_includes_header_comment() {
        let text = serialize(&MarkdownColors::default());
        assert!(text.starts_with("# Markdown Editor"));
        assert!(text.contains("Empty values follow"));
    }

    #[test]
    fn load_returns_defaults_when_file_missing() {
        // Point XDG_CONFIG_HOME at a nonexistent temp dir.
        let tmp = std::env::temp_dir().join(format!(
            "md_editor_test_{}_load_missing",
            std::process::id()
        ));
        // SAFETY: no other thread is running during this unit test.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &tmp);
        }
        let colors = load();
        assert_eq!(colors, MarkdownColors::default());
        // SAFETY: same as above.
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
    }

    #[test]
    fn load_reads_existing_file() {
        // Create a temp config file and point XDG_CONFIG_HOME at it.
        let tmp = std::env::temp_dir().join(format!(
            "md_editor_test_{}_load_existing",
            std::process::id()
        ));
        let config_dir = tmp.join("md_editor");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.yaml"),
            "h1: \"#ff0000\"\nstrong: \"#00ff00\"\n",
        )
        .unwrap();

        // SAFETY: no other thread is running during this unit test.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &tmp);
        }
        let colors = load();
        assert_eq!(colors.h1, "#ff0000");
        assert_eq!(colors.strong, "#00ff00");

        // SAFETY: same as above.
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn save_and_load_round_trip() {
        let tmp =
            std::env::temp_dir().join(format!("md_editor_test_{}_save_load", std::process::id()));
        // SAFETY: no other thread is running during this unit test.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &tmp);
        }

        let mut original = MarkdownColors::default();
        original.h1 = "#ff0000".to_owned();
        original.table_border = "#1a2b3c".to_owned();
        original.emphasis = "#abcdef".to_owned();

        // `save` spawns a thread, so we call it and wait for the thread to
        // finish by joining on a direct write instead. But `save` is
        // fire-and-forget; for the test we replicate its serialization and
        // write synchronously, then verify `load` reads it back.
        let path = config_path().unwrap();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serialize(&original)).unwrap();

        let loaded = load();
        assert_eq!(loaded.h1, "#ff0000");
        assert_eq!(loaded.table_border, "#1a2b3c");
        assert_eq!(loaded.emphasis, "#abcdef");

        // SAFETY: no other thread is running during this unit test.
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn strip_quotes_works() {
        assert_eq!(strip_quotes("\"hello\""), "hello");
        assert_eq!(strip_quotes("'hello'"), "hello");
        assert_eq!(strip_quotes("hello"), "hello");
        assert_eq!(strip_quotes("\"\""), "");
        assert_eq!(strip_quotes(""), "");
        // Mismatched quotes are not stripped.
        assert_eq!(strip_quotes("\"hello'"), "\"hello'");
    }
}
