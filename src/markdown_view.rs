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

use crate::document::{TextHit, TextHits};
use crate::fences;
use crate::syntax;
use crate::theme::MarkdownColors;
use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::text::{self as core_text, Paragraph as _};
use iced::advanced::widget::{Operation, Tree, Widget, tree};
use iced::advanced::{Clipboard, Shell, overlay};
use iced::alignment;
use iced::widget::markdown::{self, Uri, Viewer as MarkdownViewer};
use iced::widget::table;
use iced::widget::text::Span;
use iced::widget::{column, container, rich_text, scrollable, span};
use iced::{
    Background, Border, Color, Element, Event, Font, Length, Pixels, Rectangle, Renderer, Size,
    Theme, Vector, font, mouse, padding, touch,
};

/// A single highlighted code line: its styled text spans.
type Line = Arc<[Span<'static, Uri>]>;

/// The highlighted lines of a whole code block, cheap to clone (`Arc`).
type Lines = Arc<[Line]>;

/// Highlighted code blocks, cached whole.
///
/// Owned by the application and cleared when the theme changes (the cached
/// colors are theme-specific) or a new document is loaded. The grammars, and
/// the lines they highlighted, live in [`syntax::with_memo`] — shared with the
/// raw editor, which finds a block's lines already highlighted when the caret
/// enters it.
#[derive(Default)]
pub struct CodeCache {
    /// Already highlighted code blocks, keyed by `language\0code`.
    blocks: HashMap<String, Lines>,
}

impl CodeCache {
    /// Drop every cached block (the theme changed, or a new document was
    /// loaded).
    pub fn clear(&mut self) {
        self.blocks.clear();
    }

    /// The highlighted lines of `code`, highlighting it if this is the first
    /// time it is seen.
    ///
    /// `info` is the fence's info string, which names the language first.
    fn highlighted(&mut self, code: &str, info: Option<&str>, theme: syntax::Theme) -> Lines {
        let language = info.and_then(fences::language).unwrap_or("txt");
        let key = format!("{language}\u{0}{code}");

        if let Some(lines) = self.blocks.get(&key) {
            return lines.clone();
        }

        let lines = syntax::with_memo(language, theme, |memo| highlight(memo, code));
        self.blocks.insert(key, lines.clone());
        lines
    }
}

/// Languages compiled ahead of time by [`warm_up`], most urgent first.
///
/// `md` comes first because the active line's editor highlights Markdown from
/// the very first frame; the rest are the languages a Markdown document is most
/// likely to fence.
const WARM_UP_LANGUAGES: &[&str] = &[
    "md",
    "rust",
    "python",
    "javascript",
    "typescript",
    "json",
    "bash",
    "html",
    "css",
    "yaml",
    "toml",
    "sql",
    "c",
    "cpp",
    "go",
    "java",
];

/// Compile the syntax definitions for common languages on a background thread.
///
/// Two separate costs are paid lazily by the syntax highlighter, and both used
/// to land on the UI thread at the worst possible moment:
///
/// * loading the process-wide syntax and theme sets (tens of milliseconds), on
///   the very first frame — the editor line is Markdown-highlighted; and
/// * compiling an individual language's regex contexts the first time a code
///   block in that language is highlighted. This is by far the worse of the
///   two: JavaScript alone takes ~50 ms and Python ~25 ms, so simply scrolling
///   a fenced block into view froze the window for several frames.
///
/// Both results are cached process-wide, so doing the work up front on another
/// thread means it is already done — and free — by the time a block needs it.
/// Unknown language tokens fall back to plain text and cost nothing.
///
/// `theme` should be the one the editor starts with, so the theme-side caches
/// are warmed too.
pub fn warm_up(theme: syntax::Theme) {
    syntax::warm_up(theme, WARM_UP_LANGUAGES);
}

/// Build [`markdown::Settings`] whose inline-code and link colors follow `theme`,
/// with per-tag overrides from `colors` taking precedence.
///
/// Link colors are already palette-derived by [`markdown::Style::from_palette`];
/// here we additionally theme inline code, which the default leaves hardcoded.
///
/// `text_size` is the body text size in pixels; heading, code and spacing sizes
/// are derived from it proportionally by [`markdown::Settings::with_text_size`],
/// so the whole document scales from a single number.
pub fn settings(theme: &Theme, colors: &MarkdownColors, text_size: f32) -> markdown::Settings {
    let mut style = markdown::Style::from_palette(theme.palette());
    style.inline_code_color = colors.inline_code_color(theme);
    style.inline_code_highlight.background =
        Background::Color(colors.inline_code_background(theme));
    style.link_color = colors.link_color(theme);
    markdown::Settings::with_text_size(text_size, style)
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
    code_theme: syntax::Theme,
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
    ///
    /// Borrowed rather than owned: the source lives in the application state
    /// for as long as the viewer does, so copying it per block per frame was
    /// pure waste.
    block_source: Cell<Option<&'a str>>,
    /// Where rendered text records a press on it.
    hits: &'a TextHits,
    /// The number the next piece of text rendered in the current block gets
    /// — see [`Located`].
    text_items: Cell<usize>,
    /// The rendered text of every piece numbered, in order, for tests to
    /// check the numbering against [`crate::source_map`].
    #[cfg(test)]
    numbered: RefCell<Vec<String>>,
}

impl<'a> Viewer<'a> {
    /// Create a viewer with colors derived from `theme`'s palette, overridden by
    /// the user's per-tag `colors`, whose text records presses in `hits`.
    pub fn new(
        theme: &Theme,
        colors: &MarkdownColors,
        cache: &'a RefCell<CodeCache>,
        hits: &'a TextHits,
    ) -> Self {
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
            block_source: Cell::new(None),
            hits,
            text_items: Cell::new(0),
            #[cfg(test)]
            numbered: RefCell::default(),
        }
    }

    /// The rendered text of `spans`, drawn at `size` in `font` and aligned by
    /// `align`, reporting presses on it as the block's next piece of text.
    ///
    /// The one place a piece of text's layout is decided: [`Located`] has to
    /// hit-test a press against the very same layout to land it on the
    /// character under the pointer, so it is handed the same four values the
    /// `rich_text` is built from rather than being told them a second time.
    fn located(
        &self,
        spans: Arc<[Span<'static, Uri>]>,
        size: Pixels,
        font: Option<Font>,
        align: core_text::Alignment,
    ) -> Element<'a, Uri> {
        let item = self.text_items.get();
        self.text_items.set(item + 1);

        #[cfg(test)]
        self.numbered
            .borrow_mut()
            .push(spans.iter().map(|span| span.text.as_ref()).collect());

        let text = rich_text(spans.clone())
            .on_link_click(Self::on_link_click)
            .size(size)
            .align_x(align);

        Located {
            content: match font {
                Some(font) => text.font(font).into(),
                None => text.into(),
            },
            item,
            spans,
            size,
            font,
            align,
            hits: self.hits,
        }
        .into()
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
    pub fn set_block_source(&self, source: Option<&'a str>) {
        self.block_source.set(source);
        // The text of every block is numbered from the start.
        self.text_items.set(0);
    }

    /// Build a custom `iced::widget::table` from the block source, with both
    /// vertical and horizontal separators and fill width — features the
    /// library's `markdown::table` does not provide (it sets
    /// `.separator_x(0)` and wraps in a shrink-width scrollable).
    ///
    /// Returns `None` when the source does not contain a parseable table, in
    /// which case the caller falls back to `markdown::table`.
    fn build_custom_table(&self, settings: markdown::Settings) -> Option<Element<'a, Uri>> {
        let parsed = parse_table_source(self.block_source.get()?)?;

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

        let spans: Arc<[_]> = Self::painted(&text.spans(settings.style), color).into();

        // Match the default heading's leading spacing between stacked blocks.
        let top = if index > 0 {
            settings.text_size.0 / 2.0
        } else {
            0.0
        };

        // A title is set, not justified: there is nothing to gain from
        // stretching the rows of one that happens to wrap.
        container(self.located(spans, size, None, core_text::Alignment::Default))
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
            let spans: Arc<[_]> = self.body(&text.spans(settings.style)).into();

            // Table cells are not numbered: `source_map` does not count the
            // text inside tables, and every piece after it must keep the
            // number it has there. They are not justified either — a cell is
            // laid out to its own content, so there is no column edge to
            // stretch its rows to.
            if self.in_table.get() {
                return rich_text(spans)
                    .on_link_click(Self::on_link_click)
                    .size(settings.text_size)
                    .into();
            }

            // Prose is justified: a paragraph long enough to wrap reads as a
            // block of text, flush on both sides, rather than trailing off
            // raggedly on the right.
            self.located(
                spans,
                settings.text_size,
                None,
                core_text::Alignment::Justified,
            )
        }
    }

    fn code_block(
        &self,
        settings: markdown::Settings,
        language: Option<&'a str>,
        code: &'a str,
        _lines: &'a [markdown::Text],
    ) -> Element<'a, Uri> {
        let background = self.code_block_background;

        // Reuse the cached highlight unless this exact block is new. The cache is
        // cleared on theme change, so an entry always matches the current theme.
        let lines = self
            .cache
            .borrow_mut()
            .highlighted(code, language, self.code_theme);

        // Each line is a piece of text of its own, in the order the source
        // map numbers them. Code is laid out as written — it does not wrap
        // (the block scrolls sideways instead), and its columns have to line
        // up character for character.
        let rows = lines.iter().cloned().map(|spans| {
            self.located(
                spans,
                settings.code_size,
                Some(Font::MONOSPACE),
                core_text::Alignment::Default,
            )
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

/// Syntax-highlight `code` line by line with the memoized grammar, from its
/// initial state, returning cheap-to-clone styled spans for each line.
fn highlight(memo: &mut syntax::Memo, code: &str) -> Lines {
    let mut lines: Vec<Line> = Vec::new();
    let mut state = memo.start();

    for line in code.lines() {
        let highlights = memo.highlight(line, &mut state);

        let mut spans: Vec<Span<'static, Uri>> = Vec::new();
        for (range, highlight) in highlights {
            if range.is_empty() {
                continue;
            }
            spans.push(
                span(line[range.clone()].to_owned())
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

// -- Locating presses in rendered text ----------------------------------------

/// Rendered text that knows where a press on it landed.
///
/// Wraps the `rich_text` of a heading, a paragraph or a line of code — built
/// by [`Viewer::located`], which hands it the same spans, size, font and
/// alignment. A press over it is hit-tested against a paragraph laid out from
/// those, at the same bounds the `rich_text` laid its own out at — so it lands
/// on exactly the character under the pointer, justified prose included — and
/// recorded, as a [`TextHit`], in the slot the block's `document::Hit` reads
/// once the press has been handled.
///
/// `item` numbers the text within its block in drawing order, the order
/// [`crate::source_map`] numbers it in to map it back to the source.
struct Located<'a> {
    content: Element<'a, Uri>,
    item: usize,
    spans: Arc<[Span<'static, Uri>]>,
    size: Pixels,
    font: Option<Font>,
    align: core_text::Alignment,
    hits: &'a TextHits,
}

/// The bounds a [`Located`] text was last laid out against.
struct LaidOut(Size);

impl Widget<Uri, Theme, Renderer> for Located<'_> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<LaidOut>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(LaidOut(Size::ZERO))
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // `rich_text` lays its paragraph out against the maximum it is given.
        tree.state.downcast_mut::<LaidOut>().0 = limits.max();

        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Uri>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        let pressed = matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                | Event::Touch(touch::Event::FingerPressed { .. })
        );
        // A press on a link is the link's.
        if !pressed || shell.is_event_captured() {
            return;
        }
        let Some(point) = cursor.position_in(layout.bounds()) else {
            return;
        };

        let paragraph = <Renderer as core_text::Renderer>::Paragraph::with_spans(core_text::Text {
            content: &self.spans[..],
            bounds: tree.state.downcast_ref::<LaidOut>().0,
            size: self.size,
            line_height: core_text::LineHeight::default(),
            font: self
                .font
                .unwrap_or_else(|| core_text::Renderer::default_font(renderer)),
            align_x: self.align,
            align_y: alignment::Vertical::Top,
            shaping: core_text::Shaping::Advanced,
            wrapping: core_text::Wrapping::default(),
        });

        let offset = rendered_offset(&paragraph, &self.spans, point)
            // Beside the text rather than on it: its end.
            .unwrap_or_else(|| self.spans.iter().map(|span| span.text.len()).sum());

        self.hits.set(Some(TextHit {
            item: self.item,
            offset,
        }));
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Uri, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a> From<Located<'a>> for Element<'a, Uri> {
    fn from(located: Located<'a>) -> Self {
        Element::new(located)
    }
}

/// The byte offset, in the text `spans` render, of the character at `point`
/// in `paragraph` — which laid those spans out.
///
/// The hit test only reports an offset within the paragraph's own line, and a
/// hard line break (a span of its own, `"\n"`) starts a new one. The span
/// under the point tells which line that is, so the offset is made relative
/// to the whole text again.
fn rendered_offset(
    paragraph: &impl core_text::Paragraph,
    spans: &[Span<'static, Uri>],
    point: iced::Point,
) -> Option<usize> {
    let within_line = paragraph.hit_test(point)?.cursor();

    let is_break = |span: &Span<'static, Uri>| span.text.as_ref() == "\n";
    if !spans.iter().any(is_break) {
        return Some(within_line);
    }

    let under = paragraph.hit_span(point)?;
    let line_start = spans[..under.min(spans.len())]
        .iter()
        .rposition(is_break)
        .map_or(0, |last_break| {
            spans[..=last_break]
                .iter()
                .map(|span| span.text.len())
                .sum()
        });

    Some(line_start + within_line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The viewer numbers the text it renders, and `source_map` numbers the
    /// text it maps back to the source; a press is only placed right if the
    /// two agree — on how many pieces there are, and on how long each is.
    #[test]
    fn the_viewer_numbers_text_as_the_source_map_does() {
        let blocks = [
            "plain paragraph",
            "# Heading\ntext under it",
            "a **bold** and *italic* and `code` and [link](https://x.y)",
            "hard  \nbreak and soft\nbreak",
            "lead in\n- one\n- two\n  1. nested\n  2. list\n- three",
            "- [ ] task\n- [x] done",
            "> quoted\n> - with a list\n>\n> and more",
            "```rust\nfn a() {}\n\nfn b() {}\n```",
            "text\n```\ncode\n```\nafter",
            "| A | B |\n|---|---|\n| 1 | 2 |",
            "> | A | B |\n> |---|---|\n> | 1 | 2 |\n\nafter the quoted table",
            "![alt text](image.png) and a caption",
            "---\ntitle: front matter\n---\nbody",
            "<div>html block</div>",
        ];

        let theme = Theme::TokyoNight;
        let colors = MarkdownColors::default();
        let cache = RefCell::default();
        let hits = TextHits::default();
        let settings = settings(&theme, &colors, 16.0);

        for source in blocks {
            let viewer = Viewer::new(&theme, &colors, &cache, &hits);
            let content = markdown::Content::parse(source);

            viewer.set_block_source(Some(source));
            let _element = markdown::view_with(content.items(), settings, &viewer);

            let numbered = viewer.numbered.take();
            let expected = crate::source_map::item_lengths(source);

            assert_eq!(
                numbered.len(),
                expected.len(),
                "{source:?}: the viewer numbered {numbered:?}, the source map {expected:?}"
            );
            for (text, length) in numbered.iter().zip(&expected) {
                // An empty line of code is drawn as a space, to keep its height.
                let drawn_empty = text == " " && *length == 0;
                assert!(
                    text.len() == *length || drawn_empty,
                    "{source:?}: {text:?} is {} bytes, the source map says {length}",
                    text.len()
                );
            }
        }
    }

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
