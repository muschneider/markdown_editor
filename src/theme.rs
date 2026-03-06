//! Theme configuration for the Markdown editor.

use iced::widget::{button, container, text};
use iced::{Background, Border, Color, Theme};

/// Background color for the editor pane.
pub fn editor_pane(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.15, 0.15, 0.18))),
        border: Border {
            color: Color::from_rgb(0.25, 0.25, 0.30),
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Background color for the preview pane.
pub fn preview_pane(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.12, 0.12, 0.15))),
        border: Border {
            color: Color::from_rgb(0.25, 0.25, 0.30),
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Style for the top toolbar container.
pub fn toolbar_container(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(Color::from_rgb(0.10, 0.10, 0.13))),
        border: Border {
            color: Color::from_rgb(0.20, 0.20, 0.25),
            width: 0.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

/// Style for toolbar buttons.
pub fn toolbar_button(_theme: &Theme, status: button::Status) -> button::Style {
    let base = button::Style {
        background: Some(Background::Color(Color::from_rgb(0.20, 0.20, 0.25))),
        text_color: Color::from_rgb(0.85, 0.85, 0.90),
        border: Border {
            color: Color::from_rgb(0.30, 0.30, 0.35),
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    };

    match status {
        button::Status::Hovered => button::Style {
            background: Some(Background::Color(Color::from_rgb(0.30, 0.30, 0.38))),
            ..base
        },
        button::Status::Pressed => button::Style {
            background: Some(Background::Color(Color::from_rgb(0.15, 0.15, 0.20))),
            ..base
        },
        _ => base,
    }
}

/// Style for the status bar text.
pub fn status_text(_theme: &Theme) -> text::Style {
    text::Style {
        color: Some(Color::from_rgb(0.55, 0.55, 0.60)),
    }
}
