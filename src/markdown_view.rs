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

use iced::alignment;
use iced::widget::markdown::{self, Uri, Viewer as MarkdownViewer};
use iced::widget::table;
use iced::widget::text::Span;
use iced::widget::{column, container, rich_text, scrollable, span};
use iced::{Background, Border, Color, Element, Font, Length, Theme, font, padding};
use iced_highlighter as highlighter;

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
    ///
    /// Only used by the fallback path in [`Viewer::table`]; the primary path
    /// builds a custom table from [`Viewer::block_source`] and never touches
    /// these counters.
    in_table: Cell<bool>,
    table_column_count: Cell<usize>,
    content_call_index: Cell<usize>,
    /// Raw source text of the Markdown block currently being rendered, set by
    /// the app before calling `markdown::view_with`. [`Viewer::table`] uses it
    /// to parse table cells directly — `markdown::Row::cells` is private, so
    /// the library's `markdown::table` is the only other way to access body
    /// cell content, and it removes vertical column separators and wraps the
    /// table in a shrink-width scrollable. Parsing from the source lets us
    /// build a custom `iced::widget::table` with both separators and fill width.
    block_source: RefCell<Option<String>>,
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
            block_source: RefCell::new(None),
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

    /// Set the raw source text of the block currently being rendered.
    ///
    /// Must be called before `markdown::view_with` so [`Viewer::table`] can
    /// parse table cells from the source. The library's `markdown::Row::cells`
    /// field is private, so without the source we cannot build a custom table
    /// with column separators and fill width.
    pub fn set_block_source(&self, source: String) {
        *self.block_source.borrow_mut() = Some(source);
    }

    /// Build a custom `iced::widget::table` from the block source, with both
    /// vertical and horizontal separators and fill width — features the
    /// library's `markdown::table` does not provide (it sets
    /// `.separator_x(0)` and wraps in a shrink-width scrollable).
    ///
    /// Returns `None` when the source does not contain a parseable table, in
    /// which case the caller falls back to `markdown::table`.
    fn build_custom_table(&self, settings: markdown::Settings) -> Option<Element<'a, Uri>> {
        let source = self.block_source.borrow();
        let parsed = parse_table_source(source.as_deref()?)?;

        let colors = CellColors {
            header_text: self.table_header_text,
            header_bg: self.table_header_background,
            strong: self.strong,
            emphasis: self.emphasis,
        };

        let border_color = self.table_border;

        let columns: Vec<table::Column<'static, 'static, Vec<String>, Uri, Theme, iced::Renderer>> =
            parsed
                .alignments
                .iter()
                .enumerate()
                .map(|(i, &align)| {
                    let header_text = parsed.header_cells.get(i).cloned().unwrap_or_default();
                    let header = build_cell_element(&header_text, settings, colors, true);

                    table::column(header, move |row: Vec<String>| {
                        let cell_text = row.get(i).cloned().unwrap_or_default();
                        build_cell_element(&cell_text, settings, colors, false)
                    })
                    .align_x(align)
                })
                .collect();

        let table_widget = table::table(columns, parsed.body_rows.clone())
            .padding_x(settings.spacing.0)
            .padding_y(settings.spacing.0 / 2.0)
            .separator_x(1.0)
            .separator_y(1.0);

        Some(
            container(
                scrollable(table_widget)
                    .direction(scrollable::Direction::Horizontal(
                        scrollable::Scrollbar::default(),
                    ))
                    .width(Length::Fill),
            )
            .width(Length::Fill)
            .style(move |_theme| container::Style {
                border: Border {
                    color: border_color,
                    width: 1.0,
                    radius: 4.0.into(),
                },
                ..Default::default()
            })
            .into(),
        )
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
            // in a container with the header background. Keep the cell
            // content-sized (no `Length::Fill`): `markdown::table` wraps the
            // table in a horizontal `scrollable` (infinite width) and forces the
            // first column to Fill, so a Fill header there would blow up to the
            // scrollable's infinite width and hide every other column.
            let color = self.table_header_text;
            let bg = self.table_header_background;
            let spans = Self::painted(&text.spans(settings.style), color);

            container(
                rich_text(spans)
                    .on_link_click(Self::on_link_click)
                    .size(settings.text_size),
            )
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
        // Primary path: build a custom table from the block source with both
        // vertical and horizontal separators and fill width. The library's
        // `markdown::table` removes column separators (`.separator_x(0)`) and
        // wraps in a shrink-width scrollable, which makes tables look
        // unstructured.
        if let Some(custom) = self.build_custom_table(settings) {
            return custom;
        }

        // Fallback: use the library's table rendering when the block source is
        // not available. Set up header detection so header cells get distinct
        // styling.
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

// -- Custom table rendering ---------------------------------------------------

/// Colors needed to style table cells, extracted from the [`Viewer`] as `Copy`
/// values so they can be captured by `'static` closures.
#[derive(Clone, Copy)]
struct CellColors {
    header_text: Color,
    header_bg: Color,
    strong: Color,
    emphasis: Color,
}

/// A table parsed from raw Markdown source text.
struct ParsedTable {
    /// Text content of each header cell.
    header_cells: Vec<String>,
    /// Horizontal alignment per column.
    alignments: Vec<alignment::Horizontal>,
    /// Body rows, each a vector of cell texts.
    body_rows: Vec<Vec<String>>,
}

/// Parse a GFM table from raw Markdown `source`.
///
/// Scans for a header row followed by a delimiter row (`|---|:--:|---|`), then
/// collects subsequent table rows as the body. This mirrors the detection in
/// [`crate::app::table_bounds`] but returns the cell contents so we can build a
/// custom [`iced::widget::table`] with column separators and fill width —
/// features the library's `markdown::table` does not provide.
fn parse_table_source(source: &str) -> Option<ParsedTable> {
    let lines: Vec<&str> = source.lines().collect();

    // Find the header/delimiter pair: the first line that looks like a table
    // row immediately followed by a delimiter row.
    let mut start = None;
    for i in 0..lines.len().saturating_sub(1) {
        if is_table_line(lines[i]) && is_delimiter_line(lines[i + 1]) {
            start = Some(i);
            break;
        }
    }
    let start = start?;

    let header_cells = split_cells(lines[start]);
    let alignments = parse_alignments(lines[start + 1]);

    let mut body_rows = Vec::new();
    for line in lines.iter().skip(start + 2) {
        if is_table_line(line) {
            body_rows.push(split_cells(line));
        } else {
            break;
        }
    }

    Some(ParsedTable {
        header_cells,
        alignments,
        body_rows,
    })
}

/// Whether `line` could be a Markdown table row (non-blank, contains a pipe).
fn is_table_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.contains('|')
}

/// Whether `line` is a GFM table delimiter row (e.g. `|---|:--:|---|`).
fn is_delimiter_line(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.contains('|') || !trimmed.contains('-') {
        return false;
    }
    trimmed
        .chars()
        .all(|c| matches!(c, '|' | '-' | ':' | ' ' | '\t'))
}

/// Split a table row into individual cell texts by trimming leading/trailing
/// pipes and splitting on `|`.
fn split_cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed.trim_start_matches('|').trim_end_matches('|');
    inner.split('|').map(|c| c.trim().to_owned()).collect()
}

/// Parse column alignments from a delimiter row's cells.
fn parse_alignments(delimiter: &str) -> Vec<alignment::Horizontal> {
    split_cells(delimiter)
        .iter()
        .map(|cell| {
            let cell = cell.trim();
            let left = cell.starts_with(':');
            let right = cell.ends_with(':');
            match (left, right) {
                (true, true) => alignment::Horizontal::Center,
                (false, true) => alignment::Horizontal::Right,
                _ => alignment::Horizontal::Left,
            }
        })
        .collect()
}

/// Build a cell [`Element`] from raw cell text, parsing it as Markdown so
/// inline formatting (bold, italic, code, links) is preserved.
///
/// Header cells get a filled background and the header text color; body cells
/// get bold/italic recoloring like regular paragraphs.
fn build_cell_element(
    cell_text: &str,
    settings: markdown::Settings,
    colors: CellColors,
    is_header: bool,
) -> Element<'static, Uri> {
    // Parse the cell text as a standalone Markdown document so inline
    // formatting (bold, italic, code, links) is rendered. The spans returned
    // by `text.spans` are `'static` (owned), so the `Content` can be dropped
    // after extraction.
    let content = markdown::Content::parse(cell_text);

    let spans: Arc<[Span<'static, Uri>]> = content
        .items()
        .iter()
        .find_map(|item| {
            if let markdown::Item::Paragraph(text) = item {
                Some(text.spans(settings.style))
            } else {
                None
            }
        })
        .unwrap_or_else(|| Arc::from([span(cell_text.to_owned())]));

    if is_header {
        // Paint all uncolored spans with the header text color.
        let painted: Vec<Span<'static, Uri>> = spans
            .iter()
            .map(|s| {
                let mut s = s.clone();
                if s.color.is_none() {
                    s.color = Some(colors.header_text);
                }
                s
            })
            .collect();

        // Keep the header cell content-sized (the container defaults to
        // `Length::Shrink`). A `Length::Fill` cell here breaks the layout: the
        // table is wrapped in a horizontal `scrollable`, which lays its content
        // out against an *infinite* max width, and `table::Table::new` forces
        // the first column to `Length::Fill` when every column is `Shrink`
        // (which they are). A Fill cell in that first column then expands to the
        // scrollable's infinite width, pushing all other columns off-screen.
        // The library's own `markdown::table` avoids this by leaving header
        // cells `Shrink`, so the forced-Fill column still resolves to its
        // natural content width — mirror that here.
        container(
            rich_text(painted)
                .on_link_click(Viewer::on_link_click)
                .size(settings.text_size),
        )
        .style(move |_theme| container::Style {
            background: Some(Background::Color(colors.header_bg)),
            ..Default::default()
        })
        .into()
    } else {
        // Recolor bold/italic spans like regular body text.
        let body: Vec<Span<'static, Uri>> = spans
            .iter()
            .map(|s| {
                let mut s = s.clone();
                if s.color.is_none()
                    && let Some(font) = s.font
                {
                    if font.weight == font::Weight::Bold {
                        s.color = Some(colors.strong);
                    } else if font.style == font::Style::Italic {
                        s.color = Some(colors.emphasis);
                    }
                }
                s
            })
            .collect();

        rich_text(body)
            .on_link_click(Viewer::on_link_click)
            .size(settings.text_size)
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two-column table that previously rendered as a single column must
    /// parse into *both* columns — header and every body row — so the renderer
    /// builds a two-column `table::table`. If parsing collapsed to one column,
    /// the `Descricao` column would be lost before layout ever ran.
    #[test]
    fn parse_table_source_recovers_all_columns() {
        let source = "\
| Objetivo                  | Descricao                                     |
|---------------------------|-----------------------------------------------|
| Encontrar bugs            | Identificar erros logicos antes da producao   |
| Garantir manutencao       | Codigo legivel e facil de modificar no futuro |
| Compartilhar conhecimento | Toda a equipe aprende com cada revisao        |";

        let parsed = parse_table_source(source).expect("source contains a table");

        assert_eq!(parsed.header_cells, vec!["Objetivo", "Descricao"]);
        assert_eq!(
            parsed.alignments,
            vec![alignment::Horizontal::Left, alignment::Horizontal::Left]
        );
        assert_eq!(parsed.body_rows.len(), 3);
        // Every row keeps both columns, not just the first.
        assert!(parsed.body_rows.iter().all(|row| row.len() == 2));
        assert_eq!(
            parsed.body_rows[0],
            vec![
                "Encontrar bugs".to_owned(),
                "Identificar erros logicos antes da producao".to_owned(),
            ]
        );
    }

    /// Column alignments come from the delimiter row markers (`:--`, `:-:`, `--:`).
    #[test]
    fn parse_alignments_reads_colon_markers() {
        let parsed = parse_table_source(
            "\
| L    | C    | R    |
| :--- | :--: | ---: |
| a    | b    | c    |",
        )
        .expect("source contains a table");

        assert_eq!(
            parsed.alignments,
            vec![
                alignment::Horizontal::Left,
                alignment::Horizontal::Center,
                alignment::Horizontal::Right,
            ]
        );
    }

    /// Text without a header/delimiter pair is not a table.
    #[test]
    fn parse_table_source_rejects_non_table() {
        assert!(parse_table_source("just a paragraph\nwith two lines").is_none());
    }
}
