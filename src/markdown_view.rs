//! Theme-aware Markdown rendering.
//!
//! The default [`markdown::view`] only themes link colors; headings, bold and
//! italic all fall back to the plain body text color, inline code is hardcoded
//! white-on-near-black, and code blocks are always highlighted with a single
//! built-in scheme. This module supplies:
//!
//! * [`settings`] — a [`markdown::Settings`] whose inline-code colors come from
//!   the selected theme's palette.
//! * [`Viewer`] — a custom [`markdown::Viewer`] that paints **headings**, **bold**
//!   and **italic** spans with distinct, palette-derived colors and re-highlights
//!   **code blocks** with the syntax theme that matches the selected app theme
//!   (see [`crate::theme::code_highlighter`]).
//!
//! Code-block highlighting is memoized in a caller-owned cache ([`CodeCache`])
//! keyed by the code text, so re-rendering on each keystroke does not re-run the
//! syntax highlighter for blocks that have not changed.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use iced::widget::markdown::Uri;
use iced::widget::text::Span;
use iced::widget::{column, container, markdown, rich_text, scrollable, span};
use iced::{Background, Color, Element, Font, Length, Theme, font, highlighter, padding};

/// A single highlighted code line: its styled text spans.
type Line = Arc<[Span<'static, Uri>]>;

/// The highlighted lines of a whole code block, cheap to clone (`Arc`).
type Lines = Arc<[Line]>;

/// Memoized syntax-highlighted code blocks, keyed by `language\0code`.
///
/// Owned by the application and cleared when the theme changes (the cached
/// colors are theme-specific) or a new document is loaded.
pub type CodeCache = HashMap<String, Lines>;

/// Build [`markdown::Settings`] whose inline-code colors follow `theme`.
///
/// Link colors are already palette-derived by [`markdown::Style::from_palette`];
/// here we additionally theme inline code, which the default leaves hardcoded.
pub fn settings(theme: &Theme) -> markdown::Settings {
    let palette = theme.extended_palette();
    let mut style = markdown::Style::from_palette(theme.palette());
    style.inline_code_color = palette.primary.base.color;
    style.inline_code_highlight.background = Background::Color(palette.background.strong.color);
    markdown::Settings::with_style(style)
}

/// A [`markdown::Viewer`] that colors text and code per the selected theme.
pub struct Viewer<'a> {
    /// Color for heading (title) text.
    heading: Color,
    /// Color for **bold** (strong) text.
    strong: Color,
    /// Color for *italic* (emphasis) text.
    emphasis: Color,
    /// Syntax-highlighting theme for code blocks.
    code_theme: highlighter::Theme,
    /// Shared cache of highlighted code blocks.
    cache: &'a RefCell<CodeCache>,
}

impl<'a> Viewer<'a> {
    /// Create a viewer with colors derived from `theme`'s palette.
    pub fn new(theme: &Theme, cache: &'a RefCell<CodeCache>) -> Self {
        let palette = theme.extended_palette();
        Self {
            heading: palette.primary.base.color,
            strong: palette.primary.strong.color,
            emphasis: palette.secondary.base.color,
            code_theme: crate::theme::code_highlighter(theme),
            cache,
        }
    }

    /// Paint every otherwise-uncolored span with `color` (used for headings, so
    /// the whole title — including any bold/italic runs — takes the accent).
    fn painted(spans: &[Span<'static, Uri>], color: Color) -> Vec<Span<'static, Uri>> {
        spans
            .iter()
            .map(|span| {
                let mut span = span.clone();
                if span.color.is_none() {
                    span.color = Some(color);
                }
                span
            })
            .collect()
    }

    /// Recolor body spans: bold takes [`Viewer::strong`], italic takes
    /// [`Viewer::emphasis`]; plain text keeps the theme's default text color and
    /// links / inline code keep the colors baked in by [`markdown::Style`].
    fn body(&self, spans: &[Span<'static, Uri>]) -> Vec<Span<'static, Uri>> {
        spans
            .iter()
            .map(|span| {
                let mut span = span.clone();
                if span.color.is_none()
                    && let Some(font) = span.font
                {
                    if font.weight == font::Weight::Bold {
                        span.color = Some(self.strong);
                    } else if font.style == font::Style::Italic {
                        span.color = Some(self.emphasis);
                    }
                }
                span
            })
            .collect()
    }
}

impl<'a> markdown::Viewer<'a, Uri> for Viewer<'a> {
    fn on_link_click(url: Uri) -> Uri {
        url
    }

    fn heading(
        &self,
        settings: markdown::Settings,
        level: &'a markdown::HeadingLevel,
        text: &'a markdown::Text,
        index: usize,
    ) -> Element<'a, Uri> {
        use markdown::HeadingLevel::{H1, H2, H3, H4, H5, H6};

        let size = match level {
            H1 => settings.h1_size,
            H2 => settings.h2_size,
            H3 => settings.h3_size,
            H4 => settings.h4_size,
            H5 => settings.h5_size,
            H6 => settings.h6_size,
        };

        let spans = Self::painted(&text.spans(settings.style), self.heading);

        // Match the default heading's leading spacing between stacked blocks.
        let top = if index > 0 {
            settings.text_size.0 / 2.0
        } else {
            0.0
        };

        container(
            rich_text(spans)
                .on_link_click(Self::on_link_click)
                .size(size),
        )
        .padding(padding::top(top))
        .into()
    }

    fn paragraph(&self, settings: markdown::Settings, text: &markdown::Text) -> Element<'a, Uri> {
        rich_text(self.body(&text.spans(settings.style)))
            .on_link_click(Self::on_link_click)
            .size(settings.text_size)
            .into()
    }

    fn code_block(
        &self,
        settings: markdown::Settings,
        language: Option<&'a str>,
        code: &'a str,
        _lines: &'a [markdown::Text],
    ) -> Element<'a, Uri> {
        let key = format!("{}\u{0}{}", language.unwrap_or(""), code);

        // Reuse the cached highlight unless this exact block is new. The cache is
        // cleared on theme change, so an entry always matches the current theme.
        let lines = self
            .cache
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| highlight(code, language, self.code_theme))
            .clone();

        let rows = lines.iter().cloned().map(|spans| {
            rich_text(spans)
                .on_link_click(Self::on_link_click)
                .font(Font::MONOSPACE)
                .size(settings.code_size)
                .into()
        });

        container(
            scrollable(container(column(rows)).padding(settings.code_size)).direction(
                scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::default()
                        .width(settings.code_size / 2)
                        .scroller_width(settings.code_size / 2),
                ),
            ),
        )
        .width(Length::Fill)
        .padding(settings.code_size / 4)
        .style(crate::theme::code_block)
        .into()
    }
}

/// Syntax-highlight `code` line by line with the given highlighter `theme`,
/// returning cheap-to-clone styled spans for each line.
fn highlight(code: &str, language: Option<&str>, theme: highlighter::Theme) -> Lines {
    let mut stream = highlighter::Stream::new(&highlighter::Settings {
        theme,
        token: language.unwrap_or("txt").to_owned(),
    });

    let mut lines: Vec<Line> = Vec::new();

    for line in code.lines() {
        // Collect first so the mutable borrow from `highlight_line` is released
        // before committing the line's state for the next one.
        let highlights: Vec<_> = stream.highlight_line(line).collect();
        stream.commit();

        let mut spans: Vec<Span<'static, Uri>> = Vec::new();
        for (range, highlight) in highlights {
            if range.is_empty() {
                continue;
            }
            spans.push(
                span(line[range].to_owned())
                    .color_maybe(highlight.color())
                    .font(highlight.font().unwrap_or(Font::MONOSPACE)),
            );
        }

        // Keep blank lines visible so vertical spacing is preserved.
        if spans.is_empty() {
            spans.push(span(" ".to_owned()).font(Font::MONOSPACE));
        }

        lines.push(Arc::from(spans));
    }

    Arc::from(lines)
}
