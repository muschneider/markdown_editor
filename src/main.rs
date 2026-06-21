//! Markdown Editor — a simple Markdown editor that renders Markdown inline,
//! showing raw source only on the line being edited.

mod app;
mod color_picker;
mod config;
mod file_ops;
mod icons;
mod markdown_view;
mod theme;
mod toolbar;

use std::path::PathBuf;

use app::MarkdownEditor;

fn main() -> iced::Result {
    let initial_file = initial_file_from_args();

    iced::application(
        move || MarkdownEditor::new_with_file(initial_file.clone()),
        MarkdownEditor::update,
        MarkdownEditor::view,
    )
    .subscription(MarkdownEditor::subscription)
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
