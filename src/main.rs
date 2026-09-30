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

use std::path::PathBuf;

use iced::window;

use app::MarkdownEditor;

/// Wayland `app_id` / X11 `WM_CLASS` reported for our window.
///
/// Must stay equal to the basename of `packaging/md_editor.desktop`: that is
/// the only thing tying a running window back to its installed desktop entry,
/// and therefore to its icon in docks, task switchers and panels.
const APPLICATION_ID: &str = "md_editor";

fn main() -> iced::Result {
    // Read the configuration once, here rather than in the editor's
    // constructor: the highlighter warm-up below needs the *persisted* theme,
    // and warming the wrong one would compile every language's contexts twice.
    let config = config::load();

    // Pay the syntax highlighter's one-off initialization off the UI thread,
    // before the first frame ever asks for it.
    markdown_view::warm_up(theme::code_highlighter(&config.settings.theme));

    let initial_file = initial_file_from_args();

    iced::application(
        move || MarkdownEditor::new(config.clone(), initial_file.clone()),
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

/// Return the first command-line argument as a file path, if any.
///
/// Flags (arguments starting with `-`) are skipped so an eventual `--help` or
/// `--` separator is not mistaken for a file name.
fn initial_file_from_args() -> Option<PathBuf> {
    std::env::args_os()
        .skip(1)
        .find(|arg| !arg.to_string_lossy().starts_with('-'))
        .map(PathBuf::from)
}
