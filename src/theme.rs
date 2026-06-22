//! Theme configuration for the Markdown editor.
//!
//! Every style here is derived from the **selected** [`Theme`]'s palette
//! (via [`Theme::extended_palette`]) instead of hardcoded colors, so the whole
//! UI — panes, toolbar, buttons, code blocks — follows the theme the user picks
//! in the toolbar. This keeps light and dark themes readable alike.
//!
//! On top of the base theme, [`MarkdownColors`] lets the user override the
//! color of individual Markdown tags (headings, bold, italic, links, …) without
//! switching themes. An empty value means "follow the theme", so overrides
//! compose cleanly on top of whichever [`Theme`] is selected.

use iced::widget::{button, container, svg, text};
use iced::{Background, Border, Color, Theme};
use iced_highlighter as highlighter;

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

/// Style for the cursor marker drawn in the left margin of the current line in
/// view mode (Normal).
///
/// A thin accent-colored vertical bar that marks the current line without
/// changing anything about the row's own display. It is overlaid in the row's
/// left padding, so it never shifts the rendered text.
pub fn cursor_marker(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(
            theme.extended_palette().primary.base.color,
        )),
        border: Border {
            radius: 1.0.into(),
            ..Default::default()
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
///
/// `color` is the background to apply; pass the theme's `background.weak.color`
/// for the default look, or a user override from [`MarkdownColors`].
pub fn code_block_with_background(theme: &Theme, color: Color) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(color)),
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

// -- Per-tag Markdown color customization -----------------------------------

/// Identifies a Markdown tag whose color the user can customize.
///
/// Each variant maps to one field of [`MarkdownColors`] and one row in the
/// color-customization panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MarkdownTag {
    /// Level-1 heading (`#`).
    H1,
    /// Level-2 heading (`##`).
    H2,
    /// Level-3 heading (`###`).
    H3,
    /// Level-4 heading (`####`).
    H4,
    /// Level-5 heading (`#####`).
    H5,
    /// Level-6 heading (`######`).
    H6,
    /// **Bold** (strong) text.
    Strong,
    /// *Italic* (emphasis) text.
    Emphasis,
    /// `Inline code` text color.
    InlineCode,
    /// Background of `inline code`.
    InlineCodeBackground,
    /// [Link] text color.
    Link,
    /// Background of fenced code blocks.
    CodeBlockBackground,
    /// Outer border / frame color of Markdown tables.
    TableBorder,
    /// Background color of a table's header row.
    TableHeaderBackground,
    /// Text color of a table's header row.
    TableHeaderText,
}

impl MarkdownTag {
    /// Human-readable label shown in the customization panel.
    pub fn label(self) -> &'static str {
        match self {
            MarkdownTag::H1 => "Heading 1",
            MarkdownTag::H2 => "Heading 2",
            MarkdownTag::H3 => "Heading 3",
            MarkdownTag::H4 => "Heading 4",
            MarkdownTag::H5 => "Heading 5",
            MarkdownTag::H6 => "Heading 6",
            MarkdownTag::Strong => "Bold",
            MarkdownTag::Emphasis => "Italic",
            MarkdownTag::InlineCode => "Inline code",
            MarkdownTag::InlineCodeBackground => "Inline code bg",
            MarkdownTag::Link => "Link",
            MarkdownTag::CodeBlockBackground => "Code block bg",
            MarkdownTag::TableBorder => "Table border",
            MarkdownTag::TableHeaderBackground => "Table header bg",
            MarkdownTag::TableHeaderText => "Table header text",
        }
    }

    /// Every customizable tag, in the order shown in the panel.
    pub const ALL: [MarkdownTag; 15] = [
        MarkdownTag::H1,
        MarkdownTag::H2,
        MarkdownTag::H3,
        MarkdownTag::H4,
        MarkdownTag::H5,
        MarkdownTag::H6,
        MarkdownTag::Strong,
        MarkdownTag::Emphasis,
        MarkdownTag::InlineCode,
        MarkdownTag::InlineCodeBackground,
        MarkdownTag::Link,
        MarkdownTag::CodeBlockBackground,
        MarkdownTag::TableBorder,
        MarkdownTag::TableHeaderBackground,
        MarkdownTag::TableHeaderText,
    ];
}

/// Per-tag Markdown color overrides, stored as hex strings (`"#RRGGBB"`,
/// `"#RGB"`, or `"RRGGBB"`).
///
/// An empty string means **follow the theme** — the color is derived from the
/// selected [`Theme`]'s palette at render time, so un-customized tags stay
/// consistent with the theme and adapt automatically when it changes. A valid
/// hex string overrides just that one tag. This composes cleanly: the user can
/// tweak a single color (say, make H1 red) while everything else follows the
/// theme.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MarkdownColors {
    /// Hex override for level-1 heading text, or empty to follow the theme.
    pub h1: String,
    /// Hex override for level-2 heading text, or empty to follow the theme.
    pub h2: String,
    /// Hex override for level-3 heading text, or empty to follow the theme.
    pub h3: String,
    /// Hex override for level-4 heading text, or empty to follow the theme.
    pub h4: String,
    /// Hex override for level-5 heading text, or empty to follow the theme.
    pub h5: String,
    /// Hex override for level-6 heading text, or empty to follow the theme.
    pub h6: String,
    /// Hex override for **bold** text, or empty to follow the theme.
    pub strong: String,
    /// Hex override for *italic* text, or empty to follow the theme.
    pub emphasis: String,
    /// Hex override for `inline code` text, or empty to follow the theme.
    pub inline_code: String,
    /// Hex override for the `inline code` background, or empty to follow the theme.
    pub inline_code_background: String,
    /// Hex override for [link] text, or empty to follow the theme.
    pub link: String,
    /// Hex override for fenced code-block backgrounds, or empty to follow the theme.
    pub code_block_background: String,
    /// Hex override for the outer border of Markdown tables, or empty to follow the theme.
    pub table_border: String,
    /// Hex override for the background of a table's header row, or empty to follow the theme.
    pub table_header_background: String,
    /// Hex override for the text color of a table's header row, or empty to follow the theme.
    pub table_header_text: String,
}

impl MarkdownColors {
    /// Get the hex string for `tag`.
    pub fn get(&self, tag: MarkdownTag) -> &str {
        match tag {
            MarkdownTag::H1 => &self.h1,
            MarkdownTag::H2 => &self.h2,
            MarkdownTag::H3 => &self.h3,
            MarkdownTag::H4 => &self.h4,
            MarkdownTag::H5 => &self.h5,
            MarkdownTag::H6 => &self.h6,
            MarkdownTag::Strong => &self.strong,
            MarkdownTag::Emphasis => &self.emphasis,
            MarkdownTag::InlineCode => &self.inline_code,
            MarkdownTag::InlineCodeBackground => &self.inline_code_background,
            MarkdownTag::Link => &self.link,
            MarkdownTag::CodeBlockBackground => &self.code_block_background,
            MarkdownTag::TableBorder => &self.table_border,
            MarkdownTag::TableHeaderBackground => &self.table_header_background,
            MarkdownTag::TableHeaderText => &self.table_header_text,
        }
    }

    /// Mutably get the hex string for `tag`.
    pub fn get_mut(&mut self, tag: MarkdownTag) -> &mut String {
        match tag {
            MarkdownTag::H1 => &mut self.h1,
            MarkdownTag::H2 => &mut self.h2,
            MarkdownTag::H3 => &mut self.h3,
            MarkdownTag::H4 => &mut self.h4,
            MarkdownTag::H5 => &mut self.h5,
            MarkdownTag::H6 => &mut self.h6,
            MarkdownTag::Strong => &mut self.strong,
            MarkdownTag::Emphasis => &mut self.emphasis,
            MarkdownTag::InlineCode => &mut self.inline_code,
            MarkdownTag::InlineCodeBackground => &mut self.inline_code_background,
            MarkdownTag::Link => &mut self.link,
            MarkdownTag::CodeBlockBackground => &mut self.code_block_background,
            MarkdownTag::TableBorder => &mut self.table_border,
            MarkdownTag::TableHeaderBackground => &mut self.table_header_background,
            MarkdownTag::TableHeaderText => &mut self.table_header_text,
        }
    }

    /// Effective color for heading `level` (1–6): the override if set, else the
    /// theme's primary color. Levels without a specific override fall back to
    /// the theme, so you can color just H1 and leave H2–H6 thematic.
    pub fn heading_color(&self, level: u8, theme: &Theme) -> Color {
        let hex = match level {
            1 => &self.h1,
            2 => &self.h2,
            3 => &self.h3,
            4 => &self.h4,
            5 => &self.h5,
            _ => &self.h6,
        };
        parse_hex(hex.as_str()).unwrap_or_else(|| theme.extended_palette().primary.base.color)
    }

    /// Effective bold (strong) color.
    pub fn strong_color(&self, theme: &Theme) -> Color {
        parse_hex(self.strong.as_str())
            .unwrap_or_else(|| theme.extended_palette().primary.strong.color)
    }

    /// Effective italic (emphasis) color.
    pub fn emphasis_color(&self, theme: &Theme) -> Color {
        parse_hex(self.emphasis.as_str())
            .unwrap_or_else(|| theme.extended_palette().secondary.base.color)
    }

    /// Effective inline-code text color.
    pub fn inline_code_color(&self, theme: &Theme) -> Color {
        parse_hex(self.inline_code.as_str())
            .unwrap_or_else(|| theme.extended_palette().primary.base.color)
    }

    /// Effective inline-code background color.
    pub fn inline_code_background(&self, theme: &Theme) -> Color {
        parse_hex(self.inline_code_background.as_str())
            .unwrap_or_else(|| theme.extended_palette().background.strong.color)
    }

    /// Effective link color.
    pub fn link_color(&self, theme: &Theme) -> Color {
        parse_hex(self.link.as_str()).unwrap_or_else(|| theme.palette().primary)
    }

    /// Effective code-block background color.
    pub fn code_block_background(&self, theme: &Theme) -> Color {
        parse_hex(self.code_block_background.as_str())
            .unwrap_or_else(|| theme.extended_palette().background.weak.color)
    }

    /// Effective table outer-border color.
    pub fn table_border_color(&self, theme: &Theme) -> Color {
        parse_hex(self.table_border.as_str())
            .unwrap_or_else(|| theme.extended_palette().background.strong.color)
    }

    /// Effective table header-row background color.
    pub fn table_header_background_color(&self, theme: &Theme) -> Color {
        parse_hex(self.table_header_background.as_str())
            .unwrap_or_else(|| theme.extended_palette().background.weak.color)
    }

    /// Effective table header-row text color.
    pub fn table_header_text_color(&self, theme: &Theme) -> Color {
        parse_hex(self.table_header_text.as_str())
            .unwrap_or_else(|| theme.extended_palette().primary.base.color)
    }
}

/// Parse a hex color string (`"#RRGGBB"`, `"#RGB"`, `"RRGGBB"`, or `"RGB"`) into
/// a [`Color`]. Returns `None` for empty or malformed input.
pub fn parse_hex(input: &str) -> Option<Color> {
    let s = input.trim().trim_start_matches('#');
    match s.len() {
        6 => {
            let r = u8::from_str_radix(&s[0..2], 16).ok()?;
            let g = u8::from_str_radix(&s[2..4], 16).ok()?;
            let b = u8::from_str_radix(&s[4..6], 16).ok()?;
            Some(Color::from_rgb(
                r as f32 / 255.0,
                g as f32 / 255.0,
                b as f32 / 255.0,
            ))
        }
        3 => {
            let r = u8::from_str_radix(&s[0..1].repeat(2), 16).ok()?;
            let g = u8::from_str_radix(&s[1..2].repeat(2), 16).ok()?;
            let b = u8::from_str_radix(&s[2..3].repeat(2), 16).ok()?;
            Some(Color::from_rgb(
                r as f32 / 255.0,
                g as f32 / 255.0,
                b as f32 / 255.0,
            ))
        }
        _ => None,
    }
}

/// Format a [`Color`] as a lowercase `#rrggbb` hex string.
pub fn to_hex(color: Color) -> String {
    let r = (color.r.clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (color.g.clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (color.b.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// A small palette of preset color swatches offered in the color picker.
///
/// Chosen to cover the common Markdown-accent colors across both light and dark
/// themes; each is distinguishable on either background.
pub const PRESET_COLORS: [Color; 12] = [
    Color::from_rgb(0.95, 0.61, 0.39), // orange
    Color::from_rgb(0.90, 0.40, 0.40), // red
    Color::from_rgb(0.92, 0.76, 0.32), // gold
    Color::from_rgb(0.55, 0.80, 0.41), // green
    Color::from_rgb(0.30, 0.69, 0.43), // forest
    Color::from_rgb(0.27, 0.71, 0.78), // teal
    Color::from_rgb(0.36, 0.57, 0.90), // blue
    Color::from_rgb(0.55, 0.45, 0.93), // indigo
    Color::from_rgb(0.72, 0.49, 0.86), // purple
    Color::from_rgb(0.87, 0.52, 0.78), // pink
    Color::from_rgb(0.78, 0.78, 0.82), // gray
    Color::from_rgb(0.95, 0.95, 0.95), // near-white
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_full_form() {
        let c = parse_hex("#ff5733").unwrap();
        assert!((c.r - 1.0).abs() < 1e-3);
        assert!((c.g - 0x57 as f32 / 255.0).abs() < 1e-3);
        assert!((c.b - 0x33 as f32 / 255.0).abs() < 1e-3);
    }

    #[test]
    fn parse_hex_short_form() {
        let c = parse_hex("#f53").unwrap();
        // #f53 expands to #ff5533
        assert!((c.r - 1.0).abs() < 1e-3);
        assert!((c.g - 0x55 as f32 / 255.0).abs() < 1e-3);
        assert!((c.b - 0x33 as f32 / 255.0).abs() < 1e-3);
    }

    #[test]
    fn parse_hex_without_hash() {
        assert!(parse_hex("ff5733").is_some());
        assert!(parse_hex("f53").is_some());
    }

    #[test]
    fn parse_hex_rejects_invalid() {
        assert!(parse_hex("").is_none());
        assert!(parse_hex("xyz").is_none());
        assert!(parse_hex("12345").is_none());
        assert!(parse_hex("gggggg").is_none());
    }

    #[test]
    fn to_hex_round_trips() {
        let c = Color::from_rgb(0.5, 0.25, 0.75);
        let hex = to_hex(c);
        let parsed = parse_hex(&hex).unwrap();
        assert!((parsed.r - c.r).abs() < 2.0 / 255.0);
        assert!((parsed.g - c.g).abs() < 2.0 / 255.0);
        assert!((parsed.b - c.b).abs() < 2.0 / 255.0);
    }

    #[test]
    fn empty_colors_follow_theme() {
        let colors = MarkdownColors::default();
        let theme = Theme::TokyoNight;
        // No overrides: each resolution falls back to the theme palette.
        assert_eq!(
            colors.heading_color(1, &theme),
            theme.extended_palette().primary.base.color
        );
        assert_eq!(
            colors.heading_color(3, &theme),
            theme.extended_palette().primary.base.color
        );
        assert_eq!(
            colors.strong_color(&theme),
            theme.extended_palette().primary.strong.color
        );
        assert_eq!(
            colors.emphasis_color(&theme),
            theme.extended_palette().secondary.base.color
        );
    }

    #[test]
    fn override_takes_precedence_over_theme() {
        let mut colors = MarkdownColors::default();
        colors.h1 = "#ff0000".to_owned();
        let theme = Theme::TokyoNight;
        let resolved = colors.heading_color(1, &theme);
        assert!((resolved.r - 1.0).abs() < 1e-3);
        assert!((resolved.g).abs() < 1e-3);
        assert!((resolved.b).abs() < 1e-3);
        // H2 is not overridden — it follows the theme.
        assert_eq!(
            colors.heading_color(2, &theme),
            theme.extended_palette().primary.base.color
        );
        // Strong is not overridden either.
        assert_eq!(
            colors.strong_color(&theme),
            theme.extended_palette().primary.strong.color
        );
    }

    #[test]
    fn invalid_override_falls_back_to_theme() {
        let mut colors = MarkdownColors::default();
        colors.link = "not-a-color".to_owned();
        let theme = Theme::TokyoNight;
        // An unparseable string is treated like no override.
        assert_eq!(colors.link_color(&theme), theme.palette().primary);
    }

    #[test]
    fn get_and_get_mut_round_trip() {
        let mut colors = MarkdownColors::default();
        for tag in MarkdownTag::ALL {
            assert!(colors.get(tag).is_empty());
            *colors.get_mut(tag) = "#abcdef".to_owned();
            assert_eq!(colors.get(tag), "#abcdef");
        }
    }

    #[test]
    fn heading_color_per_level() {
        let mut colors = MarkdownColors::default();
        colors.h1 = "#ff0000".to_owned();
        colors.h2 = "#00ff00".to_owned();
        colors.h3 = "#0000ff".to_owned();
        let theme = Theme::TokyoNight;

        assert!((colors.heading_color(1, &theme).r - 1.0).abs() < 1e-3);
        assert!((colors.heading_color(2, &theme).g - 1.0).abs() < 1e-3);
        assert!((colors.heading_color(3, &theme).b - 1.0).abs() < 1e-3);
        // H4-H6 have no override → theme default.
        assert_eq!(
            colors.heading_color(4, &theme),
            theme.extended_palette().primary.base.color
        );
        assert_eq!(
            colors.heading_color(6, &theme),
            theme.extended_palette().primary.base.color
        );
    }

    #[test]
    fn table_colors_default_to_theme() {
        let colors = MarkdownColors::default();
        let theme = Theme::TokyoNight;
        assert_eq!(
            colors.table_border_color(&theme),
            theme.extended_palette().background.strong.color
        );
        assert_eq!(
            colors.table_header_background_color(&theme),
            theme.extended_palette().background.weak.color
        );
        assert_eq!(
            colors.table_header_text_color(&theme),
            theme.extended_palette().primary.base.color
        );
    }

    #[test]
    fn table_colors_can_be_overridden() {
        let mut colors = MarkdownColors::default();
        colors.table_border = "#aabbcc".to_owned();
        colors.table_header_background = "#112233".to_owned();
        colors.table_header_text = "#ffeedd".to_owned();
        let theme = Theme::TokyoNight;

        let border = colors.table_border_color(&theme);
        assert!((border.r - 0xaa as f32 / 255.0).abs() < 1e-3);

        let bg = colors.table_header_background_color(&theme);
        assert!((bg.r - 0x11 as f32 / 255.0).abs() < 1e-3);

        let text = colors.table_header_text_color(&theme);
        assert!((text.r - 0xff as f32 / 255.0).abs() < 1e-3);
    }
}
