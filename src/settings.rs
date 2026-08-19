//! User preferences: theme, chrome visibility, typography and session restore.
//!
//! [`Settings`] is the typed, in-memory form of everything the editor persists
//! besides the per-tag Markdown colors (which live in
//! [`crate::theme::MarkdownColors`]). Both halves are written to the same file
//! by [`crate::config`].
//!
//! Values are stored **typed**, not as strings: `theme` is an [`iced::Theme`]
//! rather than its name, and the numeric preferences are newtypes with a fixed
//! set of legal values. Parsing therefore happens exactly once, at load, and
//! `view()` never has to validate — an invalid config value degrades to the
//! default at the file boundary and is never representable in memory.

use std::fmt;
use std::path::PathBuf;

use iced::widget::{button, checkbox, column, container, pick_list, row, space, text, tooltip};
use iced::{Element, Fill, Length, Theme};

use crate::app::{DEFAULT_THEME, Message};
use crate::theme;

/// Base font size (px): what the editor uses unless configured otherwise, and
/// the size the layout estimates in [`crate::app`] are calibrated against.
pub const BASE_FONT_SIZE: u16 = 16;

/// Editor font size in pixels.
///
/// A newtype rather than a bare `u16` so the picker renders "16 px" and so an
/// out-of-range value from a hand-edited config can be rejected in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FontSize(pub u16);

impl FontSize {
    /// The default editor font size.
    pub const DEFAULT: Self = Self(BASE_FONT_SIZE);

    /// Every size offered in the settings panel.
    pub const ALL: [Self; 9] = [
        Self(11),
        Self(12),
        Self(13),
        Self(14),
        Self(16),
        Self(18),
        Self(20),
        Self(24),
        Self(28),
    ];

    /// The size in pixels.
    pub fn pixels(self) -> f32 {
        f32::from(self.0)
    }

    /// Parse a size from config text, falling back to [`FontSize::DEFAULT`] for
    /// anything unparseable or outside the offered range.
    pub fn from_config(value: &str) -> Self {
        value
            .parse::<u16>()
            .ok()
            .map(Self)
            .filter(|size| Self::ALL.contains(size))
            .unwrap_or(Self::DEFAULT)
    }
}

impl Default for FontSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl fmt::Display for FontSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} px", self.0)
    }
}

/// Maximum width (px) the rendered document is laid out at, or [`WrapWidth::FULL`]
/// to use the whole window.
///
/// Long measure hurts readability; capping it centres the text in a column the
/// way a typical Markdown preview does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WrapWidth(pub u16);

impl WrapWidth {
    /// Use the full window width (no cap).
    pub const FULL: Self = Self(0);

    /// Every width offered in the settings panel.
    pub const ALL: [Self; 6] = [
        Self::FULL,
        Self(640),
        Self(720),
        Self(800),
        Self(960),
        Self(1200),
    ];

    /// The cap in pixels, or `None` when the document should fill the window.
    pub fn pixels(self) -> Option<f32> {
        (self.0 > 0).then(|| f32::from(self.0))
    }

    /// Parse a width from config text, falling back to [`WrapWidth::FULL`] for
    /// anything unparseable or outside the offered range.
    pub fn from_config(value: &str) -> Self {
        value
            .parse::<u16>()
            .ok()
            .map(Self)
            .filter(|width| Self::ALL.contains(width))
            .unwrap_or(Self::FULL)
    }
}

impl Default for WrapWidth {
    fn default() -> Self {
        Self::FULL
    }
}

impl fmt::Display for WrapWidth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            0 => f.write_str("Full width"),
            width => write!(f, "{width} px"),
        }
    }
}

/// Every user preference except the per-tag Markdown colors.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The application theme used to render the Markdown preview.
    pub theme: Theme,
    /// Whether the top toolbar is shown.
    pub show_toolbar: bool,
    /// Whether the bottom status bar is shown.
    pub show_status_bar: bool,
    /// Font size of the editor and the rendered document.
    pub font_size: FontSize,
    /// Maximum layout width of the rendered document.
    pub wrap_width: WrapWidth,
    /// Whether [`Settings::last_file`] is reopened on the next launch.
    pub restore_session: bool,
    /// The most recently opened or saved file, remembered for `restore_session`.
    ///
    /// Tracked regardless of `restore_session` so turning the preference on
    /// takes effect immediately rather than after one more file is opened.
    pub last_file: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: DEFAULT_THEME,
            show_toolbar: true,
            show_status_bar: true,
            font_size: FontSize::DEFAULT,
            wrap_width: WrapWidth::FULL,
            // Opt-in: running `md_editor` with no arguments should give a blank
            // document, not silently reopen whatever was last touched.
            restore_session: false,
            last_file: None,
        }
    }
}

impl Settings {
    /// The file to open at startup when no path was given on the command line.
    pub fn file_to_restore(&self) -> Option<PathBuf> {
        self.restore_session
            .then(|| self.last_file.clone())
            .flatten()
    }

    /// Resolve a theme by name against [`Theme::ALL`], falling back to
    /// [`DEFAULT_THEME`] for an unknown or empty name.
    ///
    /// Only the built-in themes round-trip; a `Theme::Custom` cannot be
    /// reconstructed from its name alone and is not offered by the picker.
    pub fn theme_from_name(name: &str) -> Theme {
        Theme::ALL
            .iter()
            .find(|theme| theme.to_string() == name)
            .cloned()
            .unwrap_or(DEFAULT_THEME)
    }
}

/// Build the collapsible settings panel.
pub fn view(settings: &Settings) -> Element<'_, Message> {
    let appearance = row![
        labelled(
            "Theme",
            pick_list(
                Theme::ALL,
                Some(settings.theme.clone()),
                Message::ThemeChanged
            )
            .text_size(13)
            .padding([4, 8])
            .width(Length::Fixed(150.0))
            .into(),
        ),
        labelled(
            "Font size",
            pick_list(
                FontSize::ALL,
                Some(settings.font_size),
                Message::FontSizeChanged
            )
            .text_size(13)
            .padding([4, 8])
            .width(Length::Fixed(100.0))
            .into(),
        ),
        labelled(
            "Text width",
            pick_list(
                WrapWidth::ALL,
                Some(settings.wrap_width),
                Message::WrapWidthChanged
            )
            .text_size(13)
            .padding([4, 8])
            .width(Length::Fixed(120.0))
            .into(),
        ),
    ]
    .spacing(18)
    .align_y(iced::Center);

    let toggles = row![
        toggle("Toolbar", settings.show_toolbar, Message::ToggleToolbar),
        toggle(
            "Status bar",
            settings.show_status_bar,
            Message::ToggleStatusBar
        ),
        toggle(
            "Reopen last file on launch",
            settings.restore_session,
            Message::ToggleRestoreSession
        ),
    ]
    .spacing(18)
    .align_y(iced::Center);

    let actions = row![
        button(text("Colors\u{2026}").size(12))
            .padding([4, 10])
            .style(theme::toolbar_button)
            .on_press(Message::ToggleColorPanel),
        button(text("Reset to defaults").size(12))
            .padding([4, 10])
            .style(theme::toolbar_button)
            .on_press(Message::ResetSettings),
        space::horizontal(),
        tooltip(
            button(text("\u{2715}").size(12))
                .padding([4, 10])
                .style(theme::toolbar_button)
                .on_press(Message::ToggleSettingsPanel),
            "Close (Esc)",
            tooltip::Position::Bottom,
        )
        .style(container::rounded_box),
    ]
    .spacing(8)
    .align_y(iced::Center);

    container(column![appearance, toggles, actions].spacing(10))
        .padding([10, 16])
        .width(Fill)
        .style(theme::toolbar_container)
        .into()
}

/// A control preceded by a small caption.
fn labelled<'a>(label: &'a str, control: Element<'a, Message>) -> Element<'a, Message> {
    row![text(label).size(13).style(theme::status_text), control]
        .spacing(6)
        .align_y(iced::Center)
        .into()
}

/// A labelled checkbox that emits `message` when clicked.
fn toggle<'a>(label: &'a str, checked: bool, message: Message) -> Element<'a, Message> {
    checkbox(checked)
        .label(label)
        .size(15)
        .text_size(13)
        .spacing(6)
        // The messages are plain toggles, so the new value is redundant.
        .on_toggle(move |_| message.clone())
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_show_all_chrome_at_base_size() {
        let settings = Settings::default();

        assert!(settings.show_toolbar);
        assert!(settings.show_status_bar);
        assert_eq!(settings.font_size, FontSize::DEFAULT);
        assert_eq!(settings.font_size.pixels(), f32::from(BASE_FONT_SIZE));
        assert_eq!(settings.wrap_width, WrapWidth::FULL);
        assert_eq!(settings.wrap_width.pixels(), None);
    }

    #[test]
    fn session_restore_is_opt_in() {
        let mut settings = Settings {
            last_file: Some(PathBuf::from("/tmp/notes.md")),
            ..Settings::default()
        };

        // Remembered, but not restored until the preference is enabled.
        assert_eq!(settings.file_to_restore(), None);

        settings.restore_session = true;
        assert_eq!(
            settings.file_to_restore(),
            Some(PathBuf::from("/tmp/notes.md"))
        );
    }

    #[test]
    fn restore_without_a_remembered_file_is_none() {
        let settings = Settings {
            restore_session: true,
            ..Settings::default()
        };
        assert_eq!(settings.file_to_restore(), None);
    }

    #[test]
    fn every_built_in_theme_round_trips_through_its_name() {
        for theme in Theme::ALL {
            assert_eq!(
                &Settings::theme_from_name(&theme.to_string()),
                theme,
                "theme {theme} did not round-trip"
            );
        }
    }

    #[test]
    fn unknown_theme_names_fall_back_to_the_default() {
        assert_eq!(Settings::theme_from_name(""), DEFAULT_THEME);
        assert_eq!(Settings::theme_from_name("Not A Theme"), DEFAULT_THEME);
    }

    #[test]
    fn font_size_rejects_values_outside_the_offered_range() {
        assert_eq!(FontSize::from_config("18"), FontSize(18));
        // Parseable but not offered, so not representable in memory.
        assert_eq!(FontSize::from_config("17"), FontSize::DEFAULT);
        assert_eq!(FontSize::from_config("0"), FontSize::DEFAULT);
        assert_eq!(FontSize::from_config("garbage"), FontSize::DEFAULT);
        assert_eq!(FontSize::from_config(""), FontSize::DEFAULT);
        // Would overflow u16.
        assert_eq!(FontSize::from_config("99999999"), FontSize::DEFAULT);
    }

    #[test]
    fn wrap_width_rejects_values_outside_the_offered_range() {
        assert_eq!(WrapWidth::from_config("800"), WrapWidth(800));
        assert_eq!(WrapWidth::from_config("0"), WrapWidth::FULL);
        assert_eq!(WrapWidth::from_config("777"), WrapWidth::FULL);
        assert_eq!(WrapWidth::from_config("garbage"), WrapWidth::FULL);
    }

    #[test]
    fn pickers_label_sizes_and_widths_readably() {
        assert_eq!(FontSize(14).to_string(), "14 px");
        assert_eq!(WrapWidth::FULL.to_string(), "Full width");
        assert_eq!(WrapWidth(800).to_string(), "800 px");
    }
}
