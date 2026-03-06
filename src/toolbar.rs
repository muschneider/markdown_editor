//! Formatting toolbar for common Markdown operations.

use iced::Element;
use iced::widget::{button, container, row, text, tooltip};

use crate::app::Message;
use crate::theme;

/// Markdown formatting actions that can be applied to selected text.
#[derive(Debug, Clone)]
pub enum FormatAction {
    /// Wrap selection with `**` for bold.
    Bold,
    /// Wrap selection with `*` for italic.
    Italic,
    /// Wrap selection with `` ` `` for inline code.
    InlineCode,
    /// Prefix line with `# ` for heading.
    Heading,
    /// Prefix line with `- ` for a bullet list item.
    BulletList,
    /// Prefix line with `1. ` for a numbered list item.
    NumberedList,
    /// Prefix line with `> ` for a blockquote.
    Quote,
    /// Insert a horizontal rule `---`.
    HorizontalRule,
    /// Insert a link template `[text](url)`.
    Link,
}

impl FormatAction {
    /// Return the prefix/suffix pair for wrapping, or the text to insert.
    pub fn apply(&self, selected_text: &str) -> String {
        match self {
            FormatAction::Bold => format!("**{selected_text}**"),
            FormatAction::Italic => format!("*{selected_text}*"),
            FormatAction::InlineCode => format!("`{selected_text}`"),
            FormatAction::Heading => format!("# {selected_text}"),
            FormatAction::BulletList => format!("- {selected_text}"),
            FormatAction::NumberedList => format!("1. {selected_text}"),
            FormatAction::Quote => format!("> {selected_text}"),
            FormatAction::HorizontalRule => String::from("\n---\n"),
            FormatAction::Link => format!("[{selected_text}](url)"),
        }
    }
}

/// Build the formatting toolbar as a row of buttons.
pub fn view<'a>() -> Element<'a, Message> {
    let buttons = row![
        format_button("B", "Bold (Ctrl+B)", FormatAction::Bold),
        format_button("I", "Italic (Ctrl+I)", FormatAction::Italic),
        format_button("</>", "Code", FormatAction::InlineCode),
        format_button("H", "Heading", FormatAction::Heading),
        format_button("UL", "Bullet List", FormatAction::BulletList),
        format_button("OL", "Numbered List", FormatAction::NumberedList),
        format_button(">", "Quote", FormatAction::Quote),
        format_button("---", "Horizontal Rule", FormatAction::HorizontalRule),
        format_button("Lnk", "Link", FormatAction::Link),
    ]
    .spacing(4);

    buttons.into()
}

/// Create a single toolbar button with a tooltip.
fn format_button<'a>(label: &'a str, tip: &'a str, action: FormatAction) -> Element<'a, Message> {
    tooltip(
        button(text(label).size(13))
            .padding([4, 8])
            .style(theme::toolbar_button)
            .on_press(Message::Format(action)),
        tip,
        tooltip::Position::Bottom,
    )
    .style(container::rounded_box)
    .into()
}
