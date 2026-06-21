//! Theme-aware Markdown rendering.
//!
//! The default [`markdown::view`] only themes link colors; headings, bold and
//! italic all fall back to the plain body text color, inline code is hardcoded
//! white-on-near-black, and code blocks are always highlighted with a single
//! built-in scheme. This module supplies:
//!
//! * [`settings`] — a [`markdown::Settings`] whose inline-code and link colors
//!   come from the selected theme's palette (or the user's per-tag overrides in
//!   [`MarkdownColors`]).
//! * [`Viewer`] — a custom [`markdown::Viewer`] that paints **headings**, **bold**
//!   and **italic** spans with distinct, palette-derived colors and re-highlights
//!   **code blocks** with the syntax theme that matches the selected app theme
//!   (see [`crate::theme::code_highlighter`]). Per-tag color overrides from
//!   [`MarkdownColors`] take precedence over the theme defaults.
//!
//! Code-block highlighting is memoized in a caller-owned cache ([`CodeCache`])
//! keyed by the code text, so re-rendering on each keystroke does not re-run the
//! syntax highlighter for blocks that have not changed.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Arc;

use iced::widget::markdown::Uri;
use iced::widget::text::Span;
use iced::widget::{column, container, markdown, rich_text, scrollable, span};
use iced::{Background, Border, Color, Element, Font, Length, Theme, font, highlighter, padding};

use crate::theme::MarkdownColors;

/// A single highlighted code line: its styled text spans.
type Line = Arc<[Span<'static, Uri>]>;

/// The highlighted lines of a whole code block, cheap to clone (`Arc`).
type Lines = Arc<[Line]>;

/// Memoized syntax-highlighted code blocks, keyed by `language\0code`.
///
/// Owned by the application and cleared when the theme changes (the cached
/// colors are theme-specific) or a new document is loaded.
pub type CodeCache = HashMap<String, Lines>;

/// Build [`markdown::Settings`] whose inline-code and link colors follow `theme`,
/// with per-tag overrides from `colors` taking precedence.
///
/// Link colors are already palette-derived by [`markdown::Style::from_palette`];
/// here we additionally theme inline code, which the default leaves hardcoded.
pub fn settings(theme: &Theme, colors: &MarkdownColors) -> markdown::Settings {
    let mut style = markdown::Style::from_palette(theme.palette());
    style.inline_code_color = colors.inline_code_color(theme);
    style.inline_code_highlight.background =
        Background::Color(colors.inline_code_background(theme));
    style.link_color = colors.link_color(theme);
    markdown::Settings::with_style(style)
}

/// A [`markdown::Viewer`] that colors text and code per the selected theme and
/// the user's per-tag [`MarkdownColors`] overrides.
pub struct Viewer<'a> {
    /// Per-level heading colors (indices 0–5 → H1–H6).
    heading_colors: [Color; 6],
    /// Color for **bold** (strong) text.
    strong: Color,
    /// Color for *italic* (emphasis) text.
    emphasis: Color,
    /// Background color for fenced code blocks.
    code_block_background: Color,
    /// Outer border color for Markdown tables.
    table_border: Color,
    /// Background color for the header row of Markdown tables.
    table_header_background: Color,
    /// Text color for the header row of Markdown tables.
    table_header_text: Color,
    /// Syntax-highlighting theme for code blocks.
    code_theme: highlighter::Theme,
    /// Shared cache of highlighted code blocks.
    cache: &'a RefCell<CodeCache>,
    /// — Table-header detection —
    ///
    /// `markdown::table` renders column headers first, then body cells, all
    /// through the same [`Viewer::paragraph`] / [`Viewer::heading`] methods. The
    /// `Table` widget's constructor calls the view closures in order: all N
    /// header cells, then all body cells. So the first `N` content calls (where
    /// `N` = column count) are headers. These cells track that count so
    /// [`Viewer::paragraph`] can style headers differently from body cells.
    in_table: Cell<bool>,
    table_column_count: Cell<usize>,
    content_call_index: Cell<usize>,
}

impl<'a> Viewer<'a> {
    /// Create a viewer with colors derived from `theme`'s palette, overridden by
    /// the user's per-tag `colors`.
    pub fn new(theme: &Theme, colors: &MarkdownColors, cache: &'a RefCell<CodeCache>) -> Self {
        Self {
            heading_colors: [
                colors.heading_color(1, theme),
                colors.heading_color(2, theme),
                colors.heading_color(3, theme),
                colors.heading_color(4, theme),
                colors.heading_color(5, theme),
                colors.heading_color(6, theme),
            ],
            strong: colors.strong_color(theme),
            emphasis: colors.emphasis_color(theme),
            code_block_background: colors.code_block_background(theme),
            table_border: colors.table_border_color(theme),
            table_header_background: colors.table_header_background_color(theme),
            table_header_text: colors.table_header_text_color(theme),
            code_theme: crate::theme::code_highlighter(theme),
            cache,
            in_table: Cell::new(false),
            table_column_count: Cell::new(0),
            content_call_index: Cell::new(0),
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

    /// Returns `true` if the current content call (from [`Viewer::paragraph`]
    /// or [`Viewer::heading`]) is rendering a **table header cell** rather than
    /// a body cell or a standalone block.
    ///
    /// The `markdown::table` function builds the `Table` widget by calling
    /// viewer methods for all header cells first, then all body cells. So the
    /// first `N` calls (where `N` = column count) are headers. This method
    /// increments the call index on each check and compares it against the
    /// column count to decide.
    ///
    /// Outside of [`Viewer::table`], `in_table` is `false` and this always
    /// returns `false`, so regular paragraphs and headings are unaffected.
    fn is_table_header_cell(&self) -> bool {
        if !self.in_table.get() {
            return false;
        }
        let idx = self.content_call_index.get();
        self.content_call_index.set(idx + 1);
        idx < self.table_column_count.get()
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

        let level_index = match level {
            H1 => 0,
            H2 => 1,
            H3 => 2,
            H4 => 3,
            H5 => 4,
            H6 => 5,
        };
        let color = self.heading_colors[level_index];

        let spans = Self::painted(&text.spans(settings.style), color);

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
        let is_header = self.is_table_header_cell();

        if is_header {
            // Table header cell: paint spans with the header text color and wrap
            // in a container with the header background.
            let color = self.table_header_text;
            let bg = self.table_header_background;
            let spans = Self::painted(&text.spans(settings.style), color);

            container(
                rich_text(spans)
                    .on_link_click(Self::on_link_click)
                    .size(settings.text_size),
            )
            .width(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(Background::Color(bg)),
                ..Default::default()
            })
            .into()
        } else {
            rich_text(self.body(&text.spans(settings.style)))
                .on_link_click(Self::on_link_click)
                .size(settings.text_size)
                .into()
        }
    }

    fn code_block(
        &self,
        settings: markdown::Settings,
        language: Option<&'a str>,
        code: &'a str,
        _lines: &'a [markdown::Text],
    ) -> Element<'a, Uri> {
        let key = format!("{}\u{0}{}", language.unwrap_or(""), code);
        let background = self.code_block_background;

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
        .style(move |theme| crate::theme::code_block_with_background(theme, background))
        .into()
    }

    fn table(
        &self,
        settings: markdown::Settings,
        columns: &'a [markdown::Column],
        rows: &'a [markdown::Row],
    ) -> Element<'a, Uri> {
        // Set up table-header detection: the first `columns.len()` content calls
        // through the viewer are header cells, subsequent calls are body cells.
        self.in_table.set(true);
        self.table_column_count.set(columns.len());
        self.content_call_index.set(0);

        let table_element = markdown::table(self, settings, columns, rows);

        self.in_table.set(false);

        // Wrap the table in a container with the user's border color so the
        // outer frame follows the customization.
        let border_color = self.table_border;
        container(table_element)
            .style(move |_theme| container::Style {
                border: Border {
                    color: border_color,
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            })
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
