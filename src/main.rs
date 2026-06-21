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

use app::MarkdownEditor;

fn main() -> iced::Result {
    iced::application(
        MarkdownEditor::new,
        MarkdownEditor::update,
        MarkdownEditor::view,
    )
    .subscription(MarkdownEditor::subscription)
    .theme(MarkdownEditor::theme)
    .title(MarkdownEditor::title)
    .run()
}
