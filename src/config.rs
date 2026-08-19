//! Persistence of the editor's configuration: user preferences and the per-tag
//! Markdown color overrides.
//!
//! The config file lives at `$XDG_CONFIG_HOME/md_editor/config.yaml` (falling
//! back to `$HOME/.config/md_editor/config.yaml` on Linux), and is a flat YAML
//! map of key → string:
//!
//! ```yaml
//! # Markdown Editor configuration
//! theme: "Tokyo Night"
//! show_toolbar: "true"
//! font_size: "16"
//!
//! # Colors — empty values follow the selected theme's palette.
//! h1: "#ff0000"
//! h2: ""
//! strong: "#00ff00"
//! ```
//!
//! The format is simple enough (a flat list of `key: value` string pairs) that
//! it is parsed and serialized by hand here — no YAML dependency needed. This
//! keeps the dependency tree small and avoids the deprecated `serde_yaml` crate.
//!
//! Every value is written as a quoted string, including booleans and numbers.
//! Parsing is total: an unknown key, a malformed line or an out-of-range value
//! falls back to the default for that field, so a hand-edited file can never
//! stop the editor from starting.

use std::fs;
use std::path::PathBuf;

use crate::settings::{FontSize, Settings, WrapWidth};
use crate::theme::MarkdownColors;

/// Header comment written at the top of every config file.
const FILE_HEADER: &str = "# Markdown Editor configuration\n";

/// Comment introducing the color half of the file, so users editing it by hand
/// know what empty values mean there.
const COLORS_HEADER: &str = "\n# Colors — empty values follow the selected theme's palette.\n";

/// The complete on-disk configuration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    /// User preferences: theme, chrome visibility, typography, session restore.
    pub settings: Settings,
    /// Per-tag Markdown color overrides.
    pub colors: MarkdownColors,
}

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

/// Load the configuration from disk.
///
/// Returns [`Config::default`] when the file is missing, unreadable, or
/// malformed — the editor always starts in a usable state, never blocked by a
/// bad config. Malformed lines are skipped (a warning is not surfaced to avoid
/// pulling in a logging dependency).
pub fn load() -> Config {
    let path = match config_path() {
        Some(p) => p,
        None => return Config::default(),
    };

    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Config::default(),
    };

    parse(&contents)
}

/// Save the configuration to disk (fire-and-forget).
///
/// Creates the parent directory if it does not exist. Errors are silently
/// ignored — a failed save must never crash the editor or block the UI.
///
/// **Disabled under `cfg(test)`.** Every preference change calls this, so a
/// test that toggles one would otherwise write the *test's* state over the
/// developer's real `~/.config/md_editor/config.yaml`. The write path itself is
/// covered by [`write_config`]'s own tests.
pub fn save(config: &Config) {
    if cfg!(test) {
        return;
    }

    let path = match config_path() {
        Some(p) => p,
        None => return,
    };

    let contents = serialize(config);

    // Write off the UI thread: the file is tiny but the fs call could still
    // block on a slow disk or a hung mount. Mirrors how `LinkClicked` offloads
    // `webbrowser::open` to a spawned thread.
    std::thread::spawn(move || write_config(&path, &contents));
}

/// Write `contents` to `path`, creating the parent directory first.
///
/// Split out of [`save`] so the actual filesystem work can be exercised
/// synchronously by a test, without a spawned thread to race against.
fn write_config(path: &std::path::Path, contents: &str) {
    let _ = fs::create_dir_all(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(path),
    );
    let _ = fs::write(path, contents);
}

/// Serialize a [`Config`] into the YAML text written to disk.
///
/// Every field is emitted, quoted with double quotes so empty strings and
/// values containing `#` are preserved correctly.
fn serialize(config: &Config) -> String {
    let mut out = String::from(FILE_HEADER);

    for (key, value) in setting_entries(&config.settings) {
        write_entry(&mut out, key, &value);
    }

    out.push_str(COLORS_HEADER);

    for (key, value) in color_entries(&config.colors) {
        write_entry(&mut out, key, value);
    }

    out
}

/// Append a single `key: "value"` line to `out`.
fn write_entry(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(": \"");
    out.push_str(value);
    out.push_str("\"\n");
}

/// Parse YAML config text into a [`Config`].
///
/// Recognizes `key: value` lines where value may be quoted (`"…"` / `'…'`) or
/// bare. Empty values (no value after the colon) map to empty strings. Unknown
/// keys and blank/comment lines are skipped so users can add their own notes.
fn parse(text: &str) -> Config {
    let mut config = Config::default();

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

        // Setting and color keys share one flat namespace; the first assigner
        // that recognizes the key wins.
        if !assign_setting(&mut config.settings, key, value) {
            assign_color(&mut config.colors, key, value);
        }
    }

    config
}

/// Parse a boolean config value, falling back to `default` for anything else.
///
/// Accepts the YAML spellings a hand-editing user is likely to reach for.
fn parse_bool(value: &str, default: bool) -> bool {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => true,
        "false" | "no" | "off" | "0" => false,
        _ => default,
    }
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

/// Render each [`Settings`] field as `(key_name, value)` in declaration order,
/// used by [`serialize`].
///
/// Values are owned because booleans, numbers and the theme name have no
/// borrowable string form on the struct.
fn setting_entries(settings: &Settings) -> Vec<(&'static str, String)> {
    vec![
        ("theme", settings.theme.to_string()),
        ("show_toolbar", settings.show_toolbar.to_string()),
        ("show_status_bar", settings.show_status_bar.to_string()),
        ("font_size", settings.font_size.0.to_string()),
        ("wrap_width", settings.wrap_width.0.to_string()),
        ("restore_session", settings.restore_session.to_string()),
        (
            "last_file",
            settings
                .last_file
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        ),
    ]
}

/// Set the [`Settings`] field named `key` to `value`, returning whether `key`
/// was recognized.
///
/// Out-of-range and unparseable values fall back to that field's default rather
/// than being rejected, so one bad line never discards the rest of the file.
fn assign_setting(settings: &mut Settings, key: &str, value: &str) -> bool {
    match key {
        "theme" => settings.theme = Settings::theme_from_name(value),
        "show_toolbar" => settings.show_toolbar = parse_bool(value, true),
        "show_status_bar" => settings.show_status_bar = parse_bool(value, true),
        "font_size" => settings.font_size = FontSize::from_config(value),
        "wrap_width" => settings.wrap_width = WrapWidth::from_config(value),
        "restore_session" => settings.restore_session = parse_bool(value, false),
        "last_file" => {
            settings.last_file = (!value.is_empty()).then(|| PathBuf::from(value));
        }
        _ => return false,
    }

    true
}

/// Borrow each color field as `(key_name, value)` in declaration order, used by
/// [`serialize`].
fn color_entries(colors: &MarkdownColors) -> Vec<(&'static str, &str)> {
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

/// Set the color field named `key` to `value`, if it matches a known field.
/// Unknown keys are silently ignored.
fn assign_color(colors: &mut MarkdownColors, key: &str, value: &str) {
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

    use std::path::Path;
    use std::sync::{Mutex, PoisonError};

    use iced::Theme;

    use crate::app::DEFAULT_THEME;

    /// Serializes every test that mutates `XDG_CONFIG_HOME`.
    ///
    /// `set_var`/`remove_var` are process-global while cargo runs tests in
    /// parallel threads, so without this two of these tests interleave and one
    /// reads the other's directory — or, once the other has cleaned up, the
    /// developer's real config.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Run `body` with `XDG_CONFIG_HOME` pointed at a private temp directory.
    fn with_config_home(label: &str, body: impl FnOnce(&Path)) {
        // A panicking test poisons the lock; the guard is still valid, and
        // recovering keeps one failure from cascading into every other test.
        let _guard = ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner);

        let dir =
            std::env::temp_dir().join(format!("md_editor_test_{}_{label}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        // SAFETY: `set_var` is unsound only when another thread reads the
        // environment concurrently. `ENV_LOCK` excludes the other tests that
        // touch it, and nothing else in this crate reads the environment
        // outside `config_path`, which only runs inside `body`.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }

        body(&dir);

        // SAFETY: as above.
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn serialize_then_parse_round_trips_colors() {
        let mut original = Config::default();
        original.colors.h1 = "#ff0000".to_owned();
        original.colors.h3 = "#00ff00".to_owned();
        original.colors.strong = "#abcdef".to_owned();
        original.colors.table_border = "#1a2b3c".to_owned();

        let parsed = parse(&serialize(&original));

        assert_eq!(parsed.colors, original.colors);
        // Empty fields stay empty.
        assert_eq!(parsed.colors.h2, "");
        assert_eq!(parsed.colors.link, "");
    }

    #[test]
    fn serialize_then_parse_round_trips_settings() {
        let original = Config {
            settings: Settings {
                theme: Theme::Dracula,
                show_toolbar: false,
                show_status_bar: false,
                font_size: FontSize(20),
                wrap_width: WrapWidth(800),
                restore_session: true,
                last_file: Some(PathBuf::from("/tmp/notes.md")),
            },
            colors: MarkdownColors::default(),
        };

        let parsed = parse(&serialize(&original));

        assert_eq!(parsed, original);
    }

    /// Every preference must survive a round-trip at its non-default value —
    /// a field added to `Settings` but forgotten in `setting_entries` or
    /// `assign_setting` would silently never persist.
    #[test]
    fn every_setting_field_is_persisted() {
        let defaults = Settings::default();
        let changed = Settings {
            theme: Theme::GruvboxLight,
            show_toolbar: !defaults.show_toolbar,
            show_status_bar: !defaults.show_status_bar,
            font_size: FontSize(28),
            wrap_width: WrapWidth(1200),
            restore_session: !defaults.restore_session,
            last_file: Some(PathBuf::from("/tmp/round-trip.md")),
        };

        // Guard the guard: the fixture must actually differ from the defaults,
        // or the round-trip below would pass vacuously.
        assert_ne!(changed, defaults);

        let parsed = parse(&serialize(&Config {
            settings: changed.clone(),
            colors: MarkdownColors::default(),
        }));

        assert_eq!(parsed.settings, changed);
    }

    #[test]
    fn parse_empty_string_returns_defaults() {
        assert_eq!(parse(""), Config::default());
    }

    #[test]
    fn parse_skips_comments_and_blank_lines() {
        let text = "# This is a comment\n\
                    \n\
                    h1: \"#ff0000\"\n\
                    # another comment\n\
                    strong: \"#00ff00\"\n";
        let config = parse(text);
        assert_eq!(config.colors.h1, "#ff0000");
        assert_eq!(config.colors.strong, "#00ff00");
    }

    #[test]
    fn parse_handles_unquoted_values() {
        let config = parse("h1: #ff0000\nstrong: #00ff00\nfont_size: 20\n");
        assert_eq!(config.colors.h1, "#ff0000");
        assert_eq!(config.colors.strong, "#00ff00");
        assert_eq!(config.settings.font_size, FontSize(20));
    }

    #[test]
    fn parse_handles_single_quotes() {
        let config = parse("h1: '#ff0000'\ntheme: 'Dracula'\n");
        assert_eq!(config.colors.h1, "#ff0000");
        assert_eq!(config.settings.theme, Theme::Dracula);
    }

    #[test]
    fn parse_empty_value_means_follow_theme() {
        let config = parse("h1: \"\"\nstrong: \"#00ff00\"\n");
        assert_eq!(config.colors.h1, "");
        assert_eq!(config.colors.strong, "#00ff00");
    }

    #[test]
    fn parse_bare_empty_value() {
        let config = parse("h1:\nstrong: \"#00ff00\"\n");
        assert_eq!(config.colors.h1, "");
        assert_eq!(config.colors.strong, "#00ff00");
    }

    #[test]
    fn parse_ignores_unknown_keys() {
        let config = parse("h1: \"#ff0000\"\nunknown_key: \"whatever\"\nstrong: \"#00ff00\"\n");
        assert_eq!(config.colors.h1, "#ff0000");
        assert_eq!(config.colors.strong, "#00ff00");
    }

    #[test]
    fn parse_skips_malformed_lines() {
        let config = parse("h1: \"#ff0000\"\nthis has no colon\nstrong: \"#00ff00\"\n");
        assert_eq!(config.colors.h1, "#ff0000");
        assert_eq!(config.colors.strong, "#00ff00");
    }

    #[test]
    fn parse_all_color_fields() {
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
        let config = parse(text);
        assert_eq!(config.colors.h1, "#111111");
        assert_eq!(config.colors.h6, "#666666");
        assert_eq!(config.colors.table_header_text, "#ffffff");
    }

    /// A garbage value must degrade that one field to its default and leave
    /// every other field intact.
    #[test]
    fn out_of_range_settings_fall_back_per_field() {
        let config = parse(
            "theme: \"Not A Theme\"\n\
             font_size: \"9999\"\n\
             wrap_width: \"nonsense\"\n\
             show_toolbar: \"maybe\"\n\
             show_status_bar: \"false\"\n",
        );

        assert_eq!(config.settings.theme, DEFAULT_THEME);
        assert_eq!(config.settings.font_size, FontSize::DEFAULT);
        assert_eq!(config.settings.wrap_width, WrapWidth::FULL);
        // Unparseable booleans keep the field's own default…
        assert!(config.settings.show_toolbar);
        // …while a valid one is honoured.
        assert!(!config.settings.show_status_bar);
    }

    #[test]
    fn booleans_accept_common_yaml_spellings() {
        assert!(parse_bool("true", false));
        assert!(parse_bool("TRUE", false));
        assert!(parse_bool("yes", false));
        assert!(parse_bool("on", false));
        assert!(parse_bool("1", false));
        assert!(!parse_bool("false", true));
        assert!(!parse_bool("No", true));
        assert!(!parse_bool("off", true));
        assert!(!parse_bool("0", true));
        // Anything else keeps the caller's default, either way.
        assert!(parse_bool("banana", true));
        assert!(!parse_bool("banana", false));
    }

    #[test]
    fn an_empty_last_file_means_no_remembered_file() {
        assert_eq!(parse("last_file: \"\"\n").settings.last_file, None);
        assert_eq!(
            parse("last_file: \"/tmp/a.md\"\n").settings.last_file,
            Some(PathBuf::from("/tmp/a.md"))
        );
    }

    /// A file written by the previous, colors-only version of this module must
    /// still load: its keys are all recognized and the new settings take their
    /// defaults.
    #[test]
    fn a_legacy_colors_only_file_still_loads() {
        let legacy = "# Markdown Editor color configuration\n\
                      # Empty values follow the selected theme's palette.\n\
                      h1: \"#ff0000\"\n\
                      strong: \"#00ff00\"\n";

        let config = parse(legacy);

        assert_eq!(config.colors.h1, "#ff0000");
        assert_eq!(config.colors.strong, "#00ff00");
        assert_eq!(config.settings, Settings::default());
    }

    #[test]
    fn serialize_includes_header_comments() {
        let text = serialize(&Config::default());
        assert!(text.starts_with("# Markdown Editor"));
        assert!(text.contains("empty values follow"));
    }

    #[test]
    fn load_returns_defaults_when_file_missing() {
        with_config_home("load_missing", |_| {
            assert_eq!(load(), Config::default());
        });
    }

    #[test]
    fn load_reads_existing_file() {
        with_config_home("load_existing", |dir| {
            let config_dir = dir.join("md_editor");
            fs::create_dir_all(&config_dir).unwrap();
            fs::write(
                config_dir.join("config.yaml"),
                "h1: \"#ff0000\"\nstrong: \"#00ff00\"\ntheme: \"Dracula\"\nshow_toolbar: \"false\"\n",
            )
            .unwrap();

            let config = load();
            assert_eq!(config.colors.h1, "#ff0000");
            assert_eq!(config.colors.strong, "#00ff00");
            assert_eq!(config.settings.theme, Theme::Dracula);
            assert!(!config.settings.show_toolbar);
        });
    }

    /// The config directory does not exist on a first run, so the writer must
    /// create it rather than silently dropping the write.
    #[test]
    fn write_then_load_round_trips_through_a_missing_directory() {
        with_config_home("write_load", |_| {
            let mut original = Config::default();
            original.colors.h1 = "#ff0000".to_owned();
            original.colors.table_border = "#1a2b3c".to_owned();
            original.settings.theme = Theme::Nord;
            original.settings.font_size = FontSize(18);
            original.settings.show_status_bar = false;

            let path = config_path().unwrap();
            assert!(!path.exists(), "fixture directory should start empty");

            // `save` spawns a thread (and is disabled under `cfg(test)`), so
            // drive its write path directly and synchronously.
            write_config(&path, &serialize(&original));

            assert!(path.exists(), "the config directory was not created");
            assert_eq!(load(), original);
        });
    }

    /// Guards the `cfg(test)` short-circuit in `save`: without it, every
    /// preference-toggling test in `app` would overwrite the developer's own
    /// config file.
    #[test]
    fn save_is_disabled_under_test() {
        with_config_home("save_disabled", |_| {
            let mut config = Config::default();
            config.settings.font_size = FontSize(24);

            save(&config);

            // `save` would have spawned a thread; give a real write time to
            // land before concluding that none happened.
            std::thread::sleep(std::time::Duration::from_millis(50));

            assert!(!config_path().unwrap().exists());
        });
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
