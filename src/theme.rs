//! Theme configuration for the Markdown editor.
//!
//! Every style here is derived from the **selected** [`Theme`]'s palette
//! (via [`Theme::extended_palette`]) instead of hardcoded colors, so the whole
//! UI — panes, toolbar, buttons, code blocks — follows the theme the user picks
//! in the toolbar. This keeps light and dark themes readable alike.

use iced::widget::{button, container, svg, text};
use iced::{Background, Border, Color, Theme, highlighter};

/// Background and frame for the editor pane (the rendered document area).
pub fn editor_pane(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.base.color)),
        text_color: Some(palette.background.base.text),
        border: Border {
            color: palette.background.strong.color,
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Highlight style for the line currently being edited (raw source).
pub fn active_line(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.weak.color)),
        border: Border {
            // An accent-colored frame marks the active line per theme.
            color: palette.primary.base.color,
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

/// Style for the top toolbar and bottom status bar container.
pub fn toolbar_container(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.weak.color)),
        text_color: Some(palette.background.base.text),
        border: Border {
            color: palette.background.strong.color,
            width: 0.0,
            radius: 0.0.into(),
        },
        ..Default::default()
    }
}

/// Style for toolbar buttons.
pub fn toolbar_button(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let base = button::Style {
        background: Some(Background::Color(palette.background.strong.color)),
        text_color: palette.background.base.text,
        border: Border {
            color: palette.background.stronger.color,
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    };

    match status {
        button::Status::Hovered => button::Style {
            background: Some(Background::Color(palette.primary.weak.color)),
            text_color: palette.primary.weak.text,
            ..base
        },
        button::Status::Pressed => button::Style {
            background: Some(Background::Color(palette.primary.base.color)),
            text_color: palette.primary.base.text,
            ..base
        },
        _ => base,
    }
}

/// Tint for toolbar SVG icons, keeping them monochrome and theme-consistent.
pub fn toolbar_icon(theme: &Theme, _status: svg::Status) -> svg::Style {
    svg::Style {
        color: Some(theme.extended_palette().background.base.text),
    }
}

/// Style for the thin vertical separators between toolbar groups.
pub fn toolbar_separator(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(
            theme.extended_palette().background.strong.color,
        )),
        ..Default::default()
    }
}

/// Style for the status bar text (a dimmed version of the theme's text color).
pub fn status_text(theme: &Theme) -> text::Style {
    let text = theme.extended_palette().background.base.text;
    text::Style {
        color: Some(Color { a: 0.6, ..text }),
    }
}

/// Background and frame for rendered code blocks, slightly raised from the page
/// so the syntax-highlighted code stands out against the document.
pub fn code_block(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(palette.background.weak.color)),
        text_color: Some(palette.background.base.text),
        border: Border {
            color: palette.background.strong.color,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

/// Map the selected application [`Theme`] to the syntax-highlighting theme used
/// for code (both rendered code blocks and the line being edited).
///
/// There are only a handful of built-in highlighter themes, so light app themes
/// map to the single light highlighter (`InspiredGitHub`) and dark app themes to
/// a fitting dark one. Unknown/custom themes fall back based on the palette's
/// `is_dark` flag.
pub fn code_highlighter(theme: &Theme) -> highlighter::Theme {
    use highlighter::Theme as H;

    match theme {
        Theme::Light
        | Theme::SolarizedLight
        | Theme::GruvboxLight
        | Theme::CatppuccinLatte
        | Theme::TokyoNightLight
        | Theme::KanagawaLotus => H::InspiredGitHub,
        Theme::SolarizedDark => H::SolarizedDark,
        Theme::Dracula
        | Theme::CatppuccinFrappe
        | Theme::CatppuccinMacchiato
        | Theme::CatppuccinMocha
        | Theme::Moonfly
        | Theme::Nightfly => H::Base16Mocha,
        Theme::GruvboxDark
        | Theme::KanagawaWave
        | Theme::KanagawaDragon
        | Theme::Ferra
        | Theme::Oxocarbon => H::Base16Eighties,
        Theme::Dark | Theme::Nord | Theme::TokyoNight | Theme::TokyoNightStorm => H::Base16Ocean,
        _ => {
            if theme.extended_palette().is_dark {
                H::Base16Ocean
            } else {
                H::InspiredGitHub
            }
        }
    }
}
