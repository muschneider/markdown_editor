//! Markdown Editor — a simple Markdown editor that renders Markdown inline,
//! showing raw source only on the line being edited.

mod app;
mod color_picker;
mod config;
mod document;
mod fences;
mod file_ops;
mod highlighter;
mod history;
mod html;
mod icons;
mod markdown_view;
mod search;
mod settings;
mod source_map;
mod syntax;
mod theme;
mod toolbar;
mod wrap;

use std::ffi::OsString;
use std::path::PathBuf;

use iced::window;

use app::MarkdownEditor;
use settings::Mode;

/// Wayland `app_id` / X11 `WM_CLASS` reported for our window.
///
/// Must stay equal to the basename of `packaging/md_editor.desktop`: that is
/// the only thing tying a running window back to its installed desktop entry,
/// and therefore to its icon in docks, task switchers and panels.
const APPLICATION_ID: &str = "md_editor";

/// Help text printed for `--help`.
const USAGE: &str = "\
Markdown Editor — renders Markdown inline, showing raw source only on the
line being edited.

Usage: md_editor [OPTIONS] [FILE]

Arguments:
  [FILE]  Markdown file to open

Options:
      --visual   Start in visual mode: every line renders, nothing is editable
      --editor   Start in editor mode: the cursor's line shows raw source
  -h, --help     Print this help and exit
  -V, --version  Print version information and exit

--visual and --editor apply to this run only; neither changes the mode
remembered in the configuration file. Ctrl+E toggles the mode at any time.
";

fn main() -> iced::Result {
    let args = match Args::parse(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };

    if args.help {
        print!("{USAGE}");
        return Ok(());
    }

    if args.version {
        println!("md_editor {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // Read the configuration once, here rather than in the editor's
    // constructor: the highlighter warm-up below needs the *persisted* theme,
    // and warming the wrong one would compile every language's contexts twice.
    let config = config::load();

    // Pay the syntax highlighter's one-off initialization off the UI thread,
    // before the first frame ever asks for it.
    markdown_view::warm_up(theme::code_highlighter(&config.settings.theme));

    let Args { file, mode, .. } = args;

    iced::application(
        move || MarkdownEditor::new(config.clone(), file.clone(), mode),
        MarkdownEditor::update,
        MarkdownEditor::view,
    )
    .subscription(MarkdownEditor::subscription)
    .theme(MarkdownEditor::theme)
    .title(MarkdownEditor::title)
    .window(window::Settings {
        platform_specific: window::settings::PlatformSpecific {
            application_id: APPLICATION_ID.to_owned(),
            ..Default::default()
        },
        ..Default::default()
    })
    .run()
}

/// What the command line asked for.
#[derive(Debug, Default, PartialEq, Eq)]
struct Args {
    /// The file to open, if a path was given.
    file: Option<PathBuf>,
    /// A one-off startup mode from `--visual` / `--editor`.
    mode: Option<Mode>,
    /// `--help` was given; print [`USAGE`] and exit.
    help: bool,
    /// `--version` was given; print the version and exit.
    version: bool,
}

impl Args {
    /// Parse command-line arguments, which must already have the program name
    /// removed.
    ///
    /// Returns `Err` with a human-readable reason for an unrecognised option:
    /// a mistyped flag is far more likely than a file genuinely named
    /// `--visul`, and silently opening the welcome document would hide the
    /// mistake. Everything after a `--` separator is taken as a path, so files
    /// whose names begin with `-` are still reachable.
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut paths_only = false;

        for arg in args {
            if paths_only {
                parsed.take_file(arg);
                continue;
            }

            match arg.to_str() {
                Some("--") => paths_only = true,
                Some("--visual") => parsed.mode = Some(Mode::Visual),
                Some("--editor") => parsed.mode = Some(Mode::Editor),
                Some("-h" | "--help") => parsed.help = true,
                Some("-V" | "--version") => parsed.version = true,
                // A lone "-" is a conventional file name, not a flag.
                Some(flag) if flag.starts_with('-') && flag != "-" => {
                    return Err(format!("unrecognised option '{flag}'"));
                }
                _ => parsed.take_file(arg),
            }
        }

        Ok(parsed)
    }

    /// Record `path` as the file to open, keeping the first one given.
    fn take_file(&mut self, path: OsString) {
        if self.file.is_none() {
            self.file = Some(PathBuf::from(path));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse from string literals, as a shell would hand them over.
    fn parse(args: &[&str]) -> Result<Args, String> {
        Args::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn no_arguments_asks_for_nothing() {
        assert_eq!(parse(&[]).unwrap(), Args::default());
    }

    #[test]
    fn a_bare_path_is_the_file_to_open() {
        let args = parse(&["notes.md"]).unwrap();

        assert_eq!(args.file, Some(PathBuf::from("notes.md")));
        assert_eq!(args.mode, None);
    }

    #[test]
    fn the_mode_flags_select_a_startup_mode() {
        assert_eq!(parse(&["--visual"]).unwrap().mode, Some(Mode::Visual));
        assert_eq!(parse(&["--editor"]).unwrap().mode, Some(Mode::Editor));
    }

    /// The flag may come before or after the path, as either reads naturally.
    #[test]
    fn a_mode_flag_combines_with_a_path_in_either_order() {
        let before = parse(&["--visual", "notes.md"]).unwrap();
        let after = parse(&["notes.md", "--visual"]).unwrap();

        assert_eq!(before, after);
        assert_eq!(before.file, Some(PathBuf::from("notes.md")));
        assert_eq!(before.mode, Some(Mode::Visual));
    }

    #[test]
    fn the_last_mode_flag_wins() {
        assert_eq!(
            parse(&["--visual", "--editor"]).unwrap().mode,
            Some(Mode::Editor)
        );
    }

    #[test]
    fn help_and_version_are_recognised_both_ways() {
        assert!(parse(&["-h"]).unwrap().help);
        assert!(parse(&["--help"]).unwrap().help);
        assert!(parse(&["-V"]).unwrap().version);
        assert!(parse(&["--version"]).unwrap().version);
    }

    #[test]
    fn an_unrecognised_option_is_an_error() {
        let error = parse(&["--visul"]).unwrap_err();

        assert!(error.contains("--visul"), "{error}");
    }

    /// Without this, a file named `-weird.md` would be unreachable.
    #[test]
    fn a_separator_makes_everything_after_it_a_path() {
        let args = parse(&["--", "-weird.md"]).unwrap();

        assert_eq!(args.file, Some(PathBuf::from("-weird.md")));
        assert_eq!(args.mode, None);
    }

    #[test]
    fn a_lone_dash_is_treated_as_a_path() {
        assert_eq!(parse(&["-"]).unwrap().file, Some(PathBuf::from("-")));
    }

    /// The first path wins, matching the single-document window.
    #[test]
    fn extra_paths_are_ignored() {
        let args = parse(&["first.md", "second.md"]).unwrap();

        assert_eq!(args.file, Some(PathBuf::from("first.md")));
    }
}
