//! Markdown Editor — a simple Markdown editor that renders Markdown inline,
//! showing raw source only on the line being edited.

mod app;
mod color_picker;
mod config;
mod file_ops;
mod history;
mod icons;
mod markdown_view;
mod theme;
mod toolbar;

use std::path::PathBuf;

use app::MarkdownEditor;

fn main() -> iced::Result {
    // Pay the syntax highlighter's one-off initialization off the UI thread,
    // before the first frame ever asks for it.
    markdown_view::warm_up(theme::code_highlighter(&app::DEFAULT_THEME));

    let initial_file = initial_file_from_args();

    iced::application(
        move || MarkdownEditor::new_with_file(initial_file.clone()),
        MarkdownEditor::update,
        MarkdownEditor::view,
    )
    .theme(MarkdownEditor::theme)
    .title(MarkdownEditor::title)
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
