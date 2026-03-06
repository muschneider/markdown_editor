//! Markdown Editor — a simple side-by-side Markdown editor with live preview.

mod app;
mod file_ops;
mod theme;
mod toolbar;

use app::MarkdownEditor;

fn main() -> iced::Result {
    iced::application(
        MarkdownEditor::new,
        MarkdownEditor::update,
        MarkdownEditor::view,
    )
    .theme(MarkdownEditor::theme)
    .title(MarkdownEditor::title)
    .run()
}
