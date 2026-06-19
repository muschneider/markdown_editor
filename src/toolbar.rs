//! Formatting toolbar for common Markdown operations.

use iced::widget::svg::Handle;
use iced::widget::{button, container, row, svg, text, tooltip};
use iced::{Center, Element, Length};

use crate::app::Message;
use crate::{icons, theme};

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

/// Build the formatting toolbar: icon buttons grouped by purpose and divided by
/// thin separators (inline marks, headings, lists, blocks).
pub fn view<'a>() -> Element<'a, Message> {
    row![
        format_button(icons::bold(), "Bold", FormatAction::Bold),
        format_button(icons::italic(), "Italic", FormatAction::Italic),
        format_button(icons::code(), "Inline Code", FormatAction::InlineCode),
        separator(),
        format_button(icons::heading(), "Heading", FormatAction::Heading),
        separator(),
        format_button(
            icons::bullet_list(),
            "Bullet List",
            FormatAction::BulletList
        ),
        format_button(
            icons::numbered_list(),
            "Numbered List",
            FormatAction::NumberedList
        ),
        separator(),
        format_button(icons::quote(), "Quote", FormatAction::Quote),
        format_button(
            icons::horizontal_rule(),
            "Horizontal Rule",
            FormatAction::HorizontalRule
        ),
        format_button(icons::link(), "Link", FormatAction::Link),
    ]
    .spacing(4)
    .align_y(Center)
    .into()
}

/// Create a formatting button that emits a [`Message::Format`].
fn format_button<'a>(icon: Handle, tip: &'a str, action: FormatAction) -> Element<'a, Message> {
    icon_button(icon, tip, Message::Format(action))
}

/// Build an icon-only toolbar button with a tooltip describing its action.
///
/// Shared by the formatting toolbar and the file controls in [`crate::app`].
pub(crate) fn icon_button<'a>(
    icon: Handle,
    tip: &'a str,
    message: Message,
) -> Element<'a, Message> {
    tooltip(
        button(svg(icon).width(18).height(18).style(theme::toolbar_icon))
            .padding(6)
            .style(theme::toolbar_button)
            .on_press(message),
        tip,
        tooltip::Position::Bottom,
    )
    .style(container::rounded_box)
    .into()
}

/// A thin vertical divider used to group related toolbar buttons.
pub(crate) fn separator<'a>() -> Element<'a, Message> {
    container(text(""))
        .width(Length::Fixed(1.0))
        .height(Length::Fixed(22.0))
        .style(theme::toolbar_separator)
        .into()
}
