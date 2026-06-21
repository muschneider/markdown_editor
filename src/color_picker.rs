//! Color picker widget for per-tag Markdown color customization.
//!
//! Each customizable [`MarkdownTag`] gets a row in the customization panel built
//! by [`color_row`]: a label, a live preview swatch, a hex text input, a strip
//! of preset color swatches, and a reset button that clears the override so the
//! tag follows the theme again.

use iced::widget::{button, container, row, text, text_input};
use iced::{Background, Border, Color, Element, Length};

use crate::app::Message;
use crate::theme::{self, MarkdownTag};

/// Build one customization row for `tag`.
///
/// * `hex` — the current text in the hex input (may be empty or invalid).
/// * `effective` — the color actually used for rendering (the override if set,
///   else the theme default), shown as the preview swatch.
pub fn color_row<'a>(tag: MarkdownTag, hex: &str, effective: Color) -> Element<'a, Message> {
    let label = text(tag.label()).size(13).width(Length::Fixed(110.0));

    let swatch = container(space())
        .width(Length::Fixed(22.0))
        .height(Length::Fixed(22.0))
        .style(move |_theme| swatch_style(effective));

    let input = text_input("#rrggbb", hex)
        .on_input(move |s| Message::ColorHexChanged(tag, s))
        .width(Length::Fixed(90.0))
        .size(13);

    let presets: Element<'a, Message> = row(theme::PRESET_COLORS
        .iter()
        .map(|&color| {
            button(space())
                .width(Length::Fixed(16.0))
                .height(Length::Fixed(16.0))
                .padding(0)
                .style(move |_theme, _status| preset_button_style(color))
                .on_press(Message::ColorPicked(tag, color))
                .into()
        })
        .collect::<Vec<_>>())
    .spacing(3)
    .into();

    let reset = button(text("Reset").size(12))
        .padding([3, 8])
        .style(theme::toolbar_button)
        .on_press(Message::ColorReset(tag));

    row![label, swatch, input, presets, reset]
        .spacing(8)
        .align_y(iced::Center)
        .into()
}

/// A tiny invisible spacer used to give swatches and preset buttons their size.
fn space() -> Element<'static, Message> {
    iced::widget::space().into()
}

/// Container style for the preview swatch: a filled rounded square with a thin
/// border so light colors are visible on a light background.
fn swatch_style(color: Color) -> container::Style {
    container::Style {
        background: Some(Background::Color(color)),
        border: Border {
            color: Color { a: 0.25, ..color },
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Button style for a preset-color swatch: flat color, no border, slight
/// highlight on hover so it reads as clickable.
fn preset_button_style(color: Color) -> button::Style {
    button::Style {
        background: Some(Background::Color(color)),
        border: Border {
            color: Color { a: 0.0, ..color },
            width: 0.0,
            radius: 3.0.into(),
        },
        ..Default::default()
    }
}
