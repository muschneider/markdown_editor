//! Core application state, message types, and update/view logic.
//!
//! The editor uses an **inline (hybrid) rendering** model instead of a
//! separate live-preview pane: the document is rendered as Markdown _in place_,
//! except the line that currently holds the cursor, which is shown as raw,
//! editable Markdown source.
//!
//! Rendering is **block-based**: contiguous runs of non-active, non-blank lines
//! are parsed and rendered together as a single Markdown block. This lets
//! multi-line constructs — tables, fenced code blocks, multi-line lists — render
//! correctly. The active region splits the surrounding block, so only the block
//! you are actively editing shows raw source.
//!
//! Some constructs cannot be edited one line at a time without falling apart: a
//! Markdown table is only valid when its header, delimiter (`|---|`), and body
//! rows stay together. Editing a single row in isolation leaves the rest as an
//! invalid, un-renderable fragment. So the raw region is a **range of lines**
//! (`active_start..active_end`), not a single line: normally it covers one line,
//! but when the cursor enters a table it expands to cover the whole table.
//!
//! All of the above is [`settings::Mode::Editor`]. In
//! [`settings::Mode::Visual`] there is **no raw region at all**: the cursor's
//! line is grouped with its neighbours like any other, the whole document
//! renders, and no `text_editor` is put in the tree. Everything that follows
//! from having a region — placing a caret, drawing a selection, editing —
//! therefore belongs to editor mode alone, and [`MarkdownEditor::active_region`]
//! is the one place that says which mode we are in.

use std::cell::RefCell;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use iced::widget::{
    Column, button, column, container, markdown, pick_list, row, scrollable, space, text,
    text_editor,
};
use iced::{Element, Fill, Font, Length, Subscription, Task, Theme, keyboard, mouse, window};

use crate::color_picker;
use crate::config::{self, Config};
use crate::document::{self, Key, Press, Reveal, RevealMode};
use crate::fences;
use crate::file_ops::{self, FileError};
use crate::highlighter;
use crate::history::{self, History};
use crate::html;
use crate::icons;
use crate::markdown_view;
use crate::search::{self, Search};
use crate::settings::{self, FontSize, Settings, WrapWidth};
use crate::source_map;
use crate::theme as app_theme;
use crate::theme::{MarkdownColors, MarkdownTag};
use crate::toolbar::{self, FormatAction};
use crate::wrap;

/// Default content shown when the editor starts with no file.
const WELCOME_MD: &str = r#"# Welcome to Markdown Editor

This editor renders Markdown **inline**: the document is shown rendered, except
the line your cursor is on — that one shows the raw source so you can edit it.

Just start typing. Click anywhere to move the cursor there.

## Shortcuts

| Keys                | Action                        |
|---------------------|-------------------------------|
| `Ctrl+N O S`        | new / open / save             |
| `Ctrl+E`            | visual / editor mode          |
| `Ctrl+Z`  `Ctrl+Y`  | undo / redo                   |
| `Ctrl+B`  `Ctrl+I`  | bold / italic                 |
| `Ctrl+←/→`          | move by word                  |
| `Home`  `End`       | start / end of line           |
| `PgUp`  `PgDn`      | move a page                   |
| `Ctrl+Home`  `Ctrl+End` | start / end of document   |
| `Ctrl+A`            | select everything             |
| `Shift+←/→/↑/↓`     | extend the selection          |
| `Ctrl+Shift+Home/End` | select to the document edge |
| `Ctrl+C X V`        | copy / cut / paste            |
| `Ctrl+Backspace`  `Ctrl+Delete` | delete a word     |

Select across lines by dragging too — then copy, cut or type over it.

## Markdown

Everything renders live — **bold**, *italic*, `inline code`, [links](https://commonmark.org),
lists, quotes, tables and fenced code blocks:

```rust
fn main() {
    println!("Hello, world!");
}
```

> Tables stay whole while you edit them, so the rows never fall apart.
"#;

/// Widget id of the editable (active) line, used to move focus when the
/// active line changes programmatically.
const ACTIVE_LINE_ID: &str = "md-active-line";

/// Widget id of the document scrollable, used to scroll a line into view.
const DOCUMENT_ID: &str = "md-document";

/// Padding applied to both the active editor and the rendered blocks so they
/// stay horizontally aligned.
const LINE_PADDING: u16 = 4;

/// Padding around the whole document, inside the scrollable.
const DOCUMENT_PADDING: f32 = 12.0;

/// Height of one row of text, relative to its font size: iced's default.
const LINE_HEIGHT: f32 = 1.3;

/// Lines moved by `PgUp`/`PgDown` before the scrollable has reported a
/// viewport. Once it has, [`MarkdownEditor::page_lines`] measures a real page
/// instead.
const PAGE_LINES: usize = 15;

/// Estimated pixel height of one rendered body line at
/// [`settings::BASE_FONT_SIZE`] — the starting point for a line's height until
/// it has been laid out and measured.
///
/// The value is what a body line actually measures: iced lays text out at its
/// default relative line height (1.3 × 16 px = 20.8 px), and every segment adds
/// [`LINE_PADDING`] top and bottom plus [`SEGMENT_SPACING`] — 29.8 px in total.
/// A good estimate matters less than it used to — measured heights replace it
/// as soon as a line has been on screen, and the view is anchored to its
/// content either way — but it still decides where a jump into never-seen
/// territory (a scrollbar drag) lands, and how long the scrollbar says the
/// document is.
const EST_LINE_HEIGHT: f32 = 30.0;

/// Approximate characters per rendered line at [`settings::BASE_FONT_SIZE`],
/// used to estimate how many visual lines a source line wraps to before the
/// document's real width is known. Byte length is used (no UTF-8 decode),
/// which only ever *over*-estimates for multi-byte text.
const EST_CHARS_PER_LINE: u32 = 90;

/// Approximate width (px) of one character of rendered (proportional) text at
/// [`settings::BASE_FONT_SIZE`].
///
/// Converts a width — the configured [`WrapWidth`], or the measured width of
/// the document — into a character count.
const EST_CHAR_WIDTH: f32 = 8.0;

/// Screens of document built as real widgets above the visible one.
///
/// The render window only has to absorb the error of the height estimates
/// for lines that have not been measured yet: it is recomputed from the new
/// scroll position before every frame is drawn, and measured heights replace
/// the estimates as soon as lines have been laid out.
const OVERSCAN_ABOVE: f32 = 1.0;

/// Screens of document built as real widgets below the visible one. More than
/// above, because the estimates err towards overstating heights (short code
/// lines, wrapped text counted by bytes), and a window that ends early leaves
/// blank space at the bottom of the view.
const OVERSCAN_BELOW: f32 = 1.5;

/// Maximum height (px) of the color customization panel before it scrolls.
///
/// It has one row per [`MarkdownTag`] and opens underneath the settings panel;
/// unbounded, the two together push the document off a short window.
const COLOR_PANEL_MAX_HEIGHT: f32 = 320.0;

/// Assumed viewport height (px) for the very first frame after a document loads,
/// before the scrollable has reported its real bounds.
///
/// Opening a document used to render *every* line on the first frame (the
/// viewport was unknown), so all of a large file's code blocks were syntax-
/// highlighted up front — a multi-hundred-millisecond stall. Instead we assume
/// the document starts scrolled to the top with roughly a screen's worth of
/// height, which windows the first frame to the top of the document. The
/// scrollable reports its true bounds on the next layout, so any guess error
/// self-corrects within one frame.
const INITIAL_VIEWPORT_HEIGHT: f32 = 1080.0;

/// Main application state.
pub struct MarkdownEditor {
    /// The document, split into logical lines (no line ending characters).
    lines: Vec<String>,
    /// The line ending the document was loaded with, written back on save.
    line_ending: &'static str,
    /// Parsed Markdown for each rendered block, in document order.
    ///
    /// A block is a contiguous run of non-active, non-blank lines. Kept in sync
    /// with [`MarkdownEditor::lines`] and the active range via
    /// [`MarkdownEditor::rebuild_blocks`].
    blocks: Vec<markdown::Content>,
    /// Source text each entry of [`MarkdownEditor::blocks`] was parsed from,
    /// in the same order. Lets [`MarkdownEditor::rebuild_blocks`] reuse already
    /// parsed blocks whose text is unchanged instead of re-parsing everything.
    block_sources: Vec<String>,
    /// Identity of each entry of [`MarkdownEditor::blocks`], in the same order:
    /// the key its widget is reconciled by. A block keeps its id for as long as
    /// its parse is reused, so its laid-out text survives navigation, scrolling
    /// and edits elsewhere in the document.
    block_ids: Vec<u64>,
    /// The id the next parsed block gets.
    next_block_id: u64,
    /// The document split into renderable pieces, kept in sync with
    /// [`MarkdownEditor::lines`] and the active range.
    ///
    /// Cached rather than recomputed in [`MarkdownEditor::view`]: the walk is
    /// O(document) and `view` runs on every frame — including the frames a
    /// scroll, a mouse move or a repaint produce, where nothing changed at all.
    segments: Vec<Segment>,
    /// Line ranges (`start..end`, opening fence through closing fence) of the
    /// document's fenced code blocks, in order. A code block renders — and is
    /// edited — as one piece, blank lines and all.
    ///
    /// Recomputed lazily, by [`MarkdownEditor::refresh_fences`], only after an
    /// edit that could have moved a fence: ordinary typing never pays for it.
    fences: Vec<(usize, usize)>,
    /// Whether [`MarkdownEditor::fences`] must be recomputed: a fence line was
    /// added, removed or changed. Edits elsewhere only shift the fences.
    fences_dirty: bool,
    /// An edit added, removed or changed a code fence, which can change how
    /// the whole rest of the document splits into blocks: the one kind of
    /// edit that is allowed to cost a full re-segmentation.
    structure_changed: bool,
    /// Lines whose segmentation may have changed since the cached segments
    /// were last brought up to date, or `None` when they are current.
    dirty: Option<Dirty>,
    /// The active range the cached segments were built with: what their
    /// [`Segment::Active`] stands for, or `None` when they were built in
    /// visual mode and hold no [`Segment::Active`] at all.
    synced_region: Option<(usize, usize)>,
    /// Incremented whenever the text of the document changes.
    revision: u64,
    /// Height (px) each line contributes to the laid-out document: its share
    /// of its segment's height as last measured, or an estimate for a line
    /// that has not been on screen yet. Always `lines.len()` long.
    line_heights: Vec<f32>,
    /// Distance (px) from the top of the document to the top of each line —
    /// the running sum of [`MarkdownEditor::line_heights`] — with a final entry
    /// holding the total height. Always `lines.len() + 1` long.
    ///
    /// This is the single source of truth for converting between a scroll
    /// offset and a line index: it windows the rendered segments, sizes the
    /// spacers that stand in for the rest, and measures a page. Deriving any
    /// of those from a flat pixels-per-line constant makes them disagree, and
    /// the viewport fills with blank spacers.
    line_offsets: Vec<f32>,
    /// First line whose offset is stale, if any. Offsets are brought up to date
    /// at the end of every update, from this line down only.
    offsets_stale_from: Option<usize>,
    /// Where the document column put its children the last time it was laid
    /// out. Written during layout; folded into
    /// [`MarkdownEditor::line_heights`] at the start of the next update.
    measurements: RefCell<document::Measurements>,
    /// Where rendered text records the character a press landed on, for the
    /// segment around it to report — see [`document::TextHit`].
    text_hits: document::TextHits,
    /// Generation of the last measurements folded in.
    measured_generation: u64,
    /// Width (px) of the document when its lines were last measured. Text
    /// rewraps at a new width, so a change discards every measurement.
    measured_width: f32,
    /// The latest request to bring the caret into view, carried out by the
    /// document viewport at its next layout.
    reveal: Option<Reveal>,
    /// The visual column the caret aims for across vertical moves, in
    /// characters: kept through short lines, so moving on to a long one
    /// returns to it.
    goal_column: Option<usize>,
    /// First line (inclusive) of the contiguous range edited as raw source.
    active_start: usize,
    /// One past the last line of the raw range (exclusive).
    ///
    /// Invariant: `active_start < active_end <= lines.len()`. Usually
    /// `active_start + 1` (a single line), but it spans an entire table when the
    /// cursor sits inside one.
    active_end: usize,
    /// Editable content for the active range (one or more lines joined by `\n`).
    active_content: text_editor::Content,
    /// Whether the raw range was widened to hold a **selection** rather than
    /// because the cursor sits in a table.
    ///
    /// A selection is only ever drawn — and only ever copied, cut or replaced —
    /// by the `text_editor`, which knows nothing of the document outside its
    /// own range. So a selection that reaches past the cursor's line drags the
    /// raw range along with it (see [`MarkdownEditor::reshape_region`]). This
    /// flag remembers *why* the range is wide, so it can shrink back to a
    /// single line the moment the selection is gone — a table's range must not.
    selection_region: bool,
    /// The in-progress mouse selection drag, if any.
    drag: Option<Drag>,
    /// Path to the currently open file, if any.
    current_file: Option<PathBuf>,
    /// Whether the document has unsaved changes.
    is_dirty: bool,
    /// Whether a file operation is in progress.
    is_loading: bool,
    /// User preferences: theme, chrome visibility, typography, session restore.
    ///
    /// The single source of truth for the selected theme — there is no separate
    /// `theme` field to keep in sync.
    settings: Settings,
    /// Per-tag Markdown color overrides. Empty strings follow the theme; valid
    /// hex strings override a single tag's color.
    colors: MarkdownColors,
    /// Whether the settings panel is shown.
    show_settings_panel: bool,
    /// Whether the per-tag color customization panel is shown.
    show_color_panel: bool,
    /// Memoized syntax-highlighted code blocks for the current theme. Cleared
    /// when the theme changes or a new document is loaded.
    code_cache: RefCell<markdown_view::CodeCache>,
    /// Undo/redo history, recorded as line-range replacements.
    history: History,
    /// Find / find-and-replace state, `None` while the bar is closed.
    ///
    /// Matches are byte ranges into [`MarkdownEditor::lines`], so any edit
    /// invalidates them — see [`MarkdownEditor::refresh_search`].
    search: Option<Search>,
    /// Last reported scroll viewport: `(offset_y, view_height)` in pixels.
    ///
    /// `None` until the scrollable's first layout reports it via
    /// [`Message::Scrolled`], so the first frame (and tests, which never lay
    /// out a real window) render the whole document — keeping the existing
    /// behaviour exact for small documents and for the test suite.
    viewport: Option<(f32, f32)>,
}

/// Messages that end a run of vertical moves, and so forget the column the
/// caret was aiming for — unless the caret stayed put.
///
/// Vertical moves keep it by definition; so do the messages that never touch
/// the caret (scrolling, preferences, panels).
fn keeps_goal_column(message: &Message) -> bool {
    use text_editor::{Action, Motion};

    matches!(
        message,
        Message::MoveUp
            | Message::MoveDown
            | Message::PageUp
            | Message::PageDown
            | Message::EditorAction(Action::Move(Motion::Up | Motion::Down))
    )
}

/// How far to scroll to show the caret after `message`, or `None` to leave
/// the view alone.
///
/// Everything that moves the caret or edits at it reveals it; scrolling,
/// panels and preferences never do, so the user can read elsewhere in the
/// document without being pulled back. `Minimal` is a no-op while the caret
/// is on screen, so asking after every key press costs nothing.
fn reveal_mode(message: &Message) -> Option<RevealMode> {
    use text_editor::Action;

    match message {
        // The page keys move the view with the caret, not just after it.
        Message::PageUp | Message::PageDown => Some(RevealMode::Keep),
        // Search reveals the match it selects, if it found one — see
        // `reveal_current_match` — and otherwise leaves the view alone.
        // The editor's own wheel handling moves nothing but the view.
        Message::EditorAction(Action::Scroll { .. }) => None,
        Message::EditorAction(_)
        | Message::ClickAt(_)
        | Message::SplitLine
        | Message::MergeWithPrevious
        | Message::MergeWithNext
        | Message::MoveUp
        | Message::MoveDown
        | Message::JumpToStart
        | Message::JumpToEnd
        | Message::NewFile
        | Message::FileOpened(Ok(_))
        | Message::Format(_)
        | Message::Undo
        | Message::Redo
        | Message::ExtendSelection(_)
        | Message::DragTo(_) => Some(RevealMode::Minimal),
        // Selecting everything leaves the view where it is, like every
        // other editor; so does everything that never touches the caret.
        _ => None,
    }
}

/// All messages the application can handle.
#[derive(Debug, Clone)]
pub enum Message {
    /// A text editor action on the active region (typing, cursor movement, etc.).
    EditorAction(text_editor::Action),
    /// A rendered line was pressed: make it the active (editable) one, with
    /// the caret where the pointer was.
    ClickAt(Press),
    /// Split the active line at the cursor (Enter on a standalone line).
    SplitLine,
    /// Merge the active region into the previous line (Backspace at column 0).
    MergeWithPrevious,
    /// Merge the next line into the active region (Delete at end of line).
    MergeWithNext,
    /// Move the cursor up out of the active region to the previous line.
    MoveUp,
    /// Move the cursor down out of the active region to the next line.
    MoveDown,
    /// Move the active line up by one page (`PgUp`).
    PageUp,
    /// Move the active line down by one page (`PgDown`).
    PageDown,
    /// Jump to the first line of the document (`Ctrl+PgUp`).
    JumpToStart,
    /// Jump to the last line of the document (`Ctrl+PgDown`).
    JumpToEnd,
    /// A link in a rendered block was clicked.
    LinkClicked(markdown::Uri),
    /// Request to create a new file.
    NewFile,
    /// Request to open a file.
    OpenFile,
    /// A file was loaded from disk.
    FileOpened(Result<(PathBuf, Arc<String>), FileError>),
    /// Request to save the current file.
    SaveFile,
    /// The file was saved to disk.
    FileSaved(Result<PathBuf, FileError>),
    /// Apply a formatting action from the toolbar.
    Format(FormatAction),
    /// Change the theme used to render the Markdown preview.
    ThemeChanged(Theme),
    /// Switch between editor mode (the cursor's line is raw source) and visual
    /// mode (the whole document renders).
    ToggleMode,
    /// Scroll the document in visual mode, where no widget holds the keyboard.
    ScrollDocument(ScrollStep),
    /// Toggle the settings panel open/closed.
    ToggleSettingsPanel,
    /// `Esc`: close the innermost open panel, whichever that is.
    Dismiss,
    /// Show or hide the top toolbar.
    ToggleToolbar,
    /// Show or hide the bottom status bar.
    ToggleStatusBar,
    /// Change the font size of the editor and the rendered document.
    FontSizeChanged(FontSize),
    /// Change the maximum layout width of the rendered document.
    WrapWidthChanged(WrapWidth),
    /// Toggle whether the last opened file is reopened on the next launch.
    ToggleRestoreSession,
    /// Restore every preference (but not the colors) to its default.
    ResetSettings,
    /// Toggle the per-tag color customization panel open/closed.
    ToggleColorPanel,
    /// The user typed in the hex input for `tag` (may be partial/invalid).
    ColorHexChanged(MarkdownTag, String),
    /// The user picked a preset color for `tag`.
    ColorPicked(MarkdownTag, iced::Color),
    /// Reset `tag`'s color to the theme default (clear the override).
    ColorReset(MarkdownTag),
    /// Reset every tag's color to the theme default.
    ResetAllColors,
    /// Revert the most recent edit.
    Undo,
    /// Re-apply the most recently undone edit.
    Redo,
    /// Open the search bar, in replace mode when `replace` is set.
    OpenSearch { replace: bool },
    /// Close the search bar and return focus to the document.
    CloseSearch,
    /// The user typed in the search query input.
    SearchQueryChanged(String),
    /// The user typed in the replacement input.
    ReplacementChanged(String),
    /// Highlight the next match, wrapping at the end of the document.
    FindNext,
    /// Highlight the previous match, wrapping at the start of the document.
    FindPrevious,
    /// Toggle case-sensitive matching.
    ToggleSearchCase,
    /// Replace the highlighted match.
    ReplaceCurrent,
    /// Replace every match in the document.
    ReplaceAll,
    /// The document scroll viewport moved (or was first reported by layout).
    /// Carries the vertical scroll offset (px from the top) and the visible
    /// height (px), used to window the rendered blocks so only those near the
    /// viewport are built into the widget tree.
    Scrolled { offset_y: f32, view_height: f32 },
    /// Select the whole document (`Ctrl/Cmd+A`).
    SelectAll,
    /// Extend the selection by a motion that runs past the raw region's edge,
    /// widening the region so the selection can continue into the document.
    ExtendSelection(text_editor::Motion),
    /// The mouse was dragged onto the rendered line at this index with the
    /// button held: extend the selection to it.
    DragTo(usize),
    /// The mouse button was released, ending any selection drag.
    DragEnd,
}

/// How far [`Message::ScrollDocument`] moves the visual-mode view.
///
/// Visual mode has no caret to navigate by and no focused widget to receive
/// keys, so the arrow and page keys move the *viewport* instead — the only
/// thing there is to move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollStep {
    /// One line towards the start of the document.
    LineUp,
    /// One line towards the end of the document.
    LineDown,
    /// One screen towards the start of the document.
    PageUp,
    /// One screen towards the end of the document.
    PageDown,
    /// The top of the document.
    Start,
    /// The bottom of the document.
    End,
}

/// Theme the editor starts with.
pub const DEFAULT_THEME: Theme = Theme::TokyoNight;

/// Vertical gap (px) the document column puts between two rendered segments.
const SEGMENT_SPACING: f32 = 1.0;

/// A renderable piece of the document, derived from the lines and active range.
#[derive(Debug, PartialEq, Eq)]
enum Segment {
    /// The active range — rendered as the raw, editable source.
    Active,
    /// A blank line at the given index — rendered as a clickable spacer.
    Blank(usize),
    /// A contiguous run of lines `start..end` rendered together as Markdown.
    Block { start: usize, end: usize },
}

impl Segment {
    /// The document lines the segment covers, given the active range.
    fn lines(&self, active: (usize, usize)) -> (usize, usize) {
        match *self {
            Segment::Active => active,
            Segment::Blank(index) => (index, index + 1),
            Segment::Block { start, end } => (start, end),
        }
    }

    /// Move the segment `delta` lines down the document.
    fn shift(&mut self, delta: isize) {
        let shift = |index: &mut usize| *index = index.wrapping_add_signed(delta);

        match self {
            Segment::Active => {}
            Segment::Blank(index) => shift(index),
            Segment::Block { start, end } => {
                shift(start);
                shift(end);
            }
        }
    }
}

/// The part of the document whose segmentation may have changed since the
/// cached segments were last brought up to date.
///
/// Lines before `start` are exactly as they were. So are the lines from `end`
/// on — only moved: line `n` there was line `n - delta` before. Everything in
/// between may have changed in any way, which is all a re-segmentation has to
/// look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dirty {
    /// First line that may have changed, in the current numbering.
    start: usize,
    /// One past the last line that may have changed, in the current numbering.
    end: usize,
    /// Lines added (or, when negative, removed) since.
    delta: isize,
}

impl Dirty {
    /// Mark `range`, in the current numbering, as possibly changed.
    fn touch(this: &mut Option<Self>, range: Range<usize>) {
        *this = Some(match *this {
            None => Self {
                start: range.start,
                end: range.end,
                delta: 0,
            },
            Some(dirty) => Self {
                start: dirty.start.min(range.start),
                end: dirty.end.max(range.end),
                ..dirty
            },
        });
    }

    /// Record that the lines in `range` were replaced by `inserted` lines.
    fn splice(this: &mut Option<Self>, range: Range<usize>, inserted: usize) {
        let delta = inserted as isize - range.len() as isize;
        let replaced_end = range.start + inserted;

        *this = Some(match *this {
            None => Self {
                start: range.start,
                end: replaced_end,
                delta,
            },
            Some(dirty) => Self {
                start: dirty.start.min(range.start),
                // An end past the splice moves with the lines after it; one
                // before or inside it is covered by the replacement.
                end: if dirty.end >= range.end {
                    dirty.end.wrapping_add_signed(delta)
                } else {
                    replaced_end
                },
                delta: dirty.delta + delta,
            },
        });
    }

    /// `end` in the numbering the cached segments use.
    fn old_end(&self) -> usize {
        self.end.wrapping_add_signed(-self.delta)
    }
}

/// A mouse selection drag in progress.
///
/// Held from the press that started it until the button is released, which is
/// what turns a mouse *move* over a rendered line into a selection instead of
/// plain hovering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Drag {
    /// Where the drag began, as a `(document line, byte column)` pair. It is
    /// the fixed end of the selection; the pointer moves the other one.
    anchor: (usize, usize),
}

/// Cursor and region context handed to the active editor's key bindings, so
/// they can decide whether a key edits within the region or leaves it.
#[derive(Clone, Copy, Default)]
struct BindingContext {
    /// The raw region spans more than one line (i.e. an entire table).
    multiline: bool,
    /// Some text is selected.
    ///
    /// Selection changes what several keys mean: `Backspace`/`Delete` remove
    /// the selection instead of a character or a line break, `Enter` replaces
    /// it, and `Copy`/`Cut` only have something to do when it is set.
    has_selection: bool,
    /// The cursor is on the first visual line of the editor.
    at_editor_top: bool,
    /// The cursor is on the last visual line of the editor.
    at_editor_bottom: bool,
    /// The cursor is at column 0.
    at_line_start: bool,
    /// The cursor is at the end of its current visual line.
    at_line_end: bool,
    /// The region starts at the first line of the document.
    at_doc_first: bool,
    /// The region ends at the last line of the document.
    at_doc_last: bool,
}

impl MarkdownEditor {
    /// Test fixture: a new editor with the **default** configuration.
    ///
    /// Deliberately does not read the user's config file — the real entry point
    /// is [`MarkdownEditor::new`], which takes the configuration from its
    /// caller. Loading it here would make every test depend on whatever the
    /// developer happens to have in `~/.config/md_editor/config.yaml`.
    #[cfg(test)]
    fn new_with_file(initial_file: Option<PathBuf>) -> (Self, Task<Message>) {
        Self::new(Config::default(), initial_file)
    }

    /// Create a new editor instance from `config`.
    ///
    /// When `initial_file` is `Some`, the editor boots with the welcome
    /// document and immediately kicks off an async load of the given path;
    /// [`Message::FileOpened`] swaps in the file's contents on success, or
    /// leaves the welcome document on error. With no path given, the file
    /// remembered by [`Settings::file_to_restore`] is opened instead — a
    /// command-line argument always wins over the restored session.
    pub fn new(config: Config, initial_file: Option<PathBuf>) -> (Self, Task<Message>) {
        let restored = initial_file.or_else(|| config.settings.file_to_restore());

        let mut editor = Self {
            lines: split_lines(WELCOME_MD),
            line_ending: "\n",
            blocks: Vec::new(),
            block_sources: Vec::new(),
            block_ids: Vec::new(),
            next_block_id: 0,
            segments: Vec::new(),
            fences: Vec::new(),
            fences_dirty: true,
            structure_changed: false,
            dirty: None,
            // Set for real by the `rebuild` below, once `settings` is in place.
            synced_region: None,
            revision: 0,
            line_heights: Vec::new(),
            line_offsets: Vec::new(),
            offsets_stale_from: None,
            measurements: RefCell::default(),
            text_hits: document::TextHits::default(),
            measured_generation: 0,
            measured_width: 0.0,
            reveal: None,
            goal_column: None,
            active_start: 0,
            active_end: 1,
            active_content: text_editor::Content::with_text(""),
            selection_region: false,
            drag: None,
            current_file: None,
            is_dirty: false,
            is_loading: false,
            settings: config.settings,
            colors: config.colors,
            show_settings_panel: false,
            show_color_panel: false,
            code_cache: RefCell::default(),
            history: History::default(),
            search: None,
            viewport: None,
        };
        editor.remeasure();
        editor.activate_line(0, 0);
        editor.rebuild();

        if let Some(path) = restored {
            editor.is_loading = true;
            return (
                editor,
                Task::perform(file_ops::load_file(path), Message::FileOpened),
            );
        }

        let focus = editor.focus_active();
        (editor, focus)
    }

    /// Persist the current configuration (preferences and colors).
    ///
    /// Fire-and-forget: [`config::save`] writes off the UI thread and swallows
    /// every error, so this is safe to call from any message handler.
    fn save_config(&self) {
        config::save(&Config {
            settings: self.settings.clone(),
            colors: self.colors.clone(),
        });
    }

    /// Remember `path` as the most recently used file and persist it.
    ///
    /// Recorded whether or not session restore is enabled, so turning the
    /// preference on takes effect immediately.
    fn remember_file(&mut self, path: &std::path::Path) {
        if self.settings.last_file.as_deref() == Some(path) {
            return;
        }

        self.settings.last_file = Some(path.to_path_buf());
        self.save_config();
    }

    /// Window title, showing the file name and dirty indicator.
    pub fn title(&self) -> String {
        let file_name = self
            .current_file
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("Untitled");

        let dirty = if self.is_dirty { " *" } else { "" };

        format!("{file_name}{dirty} — Markdown Editor")
    }

    /// Handle a message, then bring the rendered blocks up to date with what
    /// it changed — and only that.
    ///
    /// Every edit and every move of the active region records which lines it
    /// touched (see [`Dirty`]); afterwards only the segments around those
    /// lines are walked again, and only blocks whose text is new are parsed.
    /// A message that changes neither the text nor the region — a scroll
    /// report, a preference, typing inside the region — costs nothing here.
    /// Navigation, which used to walk and compare the whole document on every
    /// key press, costs the same on a ten-line file as on a ten-thousand-line
    /// one.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        // What the last layout measured is only valid for the document as it
        // was then, so it is taken in before anything changes.
        self.absorb_measurements();

        let keeps_goal = keeps_goal_column(&message);
        let reveal = reveal_mode(&message);
        let caret = self.cursor_position();
        let revision = self.revision;

        let task = self.handle(message);

        // An edit that moved a code fence may have paired fences around the
        // region, which must then cover the whole new code block.
        if self.structure_changed {
            self.snap_region_to_fences();
        }

        // A region widened to hold a selection shrinks back as soon as the
        // selection is gone, so no more of the document stays raw than has to.
        self.collapse_selection_region();

        if std::mem::take(&mut self.structure_changed) {
            // A fence changed: everything after it may split differently.
            self.rebuild();
        } else {
            // Re-walks just what changed; free when nothing did.
            self.sync();
        }

        let edited = self.revision != revision;

        // Search matches are byte ranges into `lines`, so they go stale on any
        // document change. It is a no-op, and costs nothing, while the bar is
        // closed.
        if edited {
            self.refresh_search();
        }

        if !keeps_goal && (edited || self.cursor_position() != caret) {
            self.goal_column = None;
        }
        if let Some(mode) = reveal {
            self.request_reveal(mode);
        }

        self.refresh_offsets();
        task
    }

    /// Handle all state transitions.
    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::EditorAction(action) => {
                // Rich text copied from a browser or a word processor reaches
                // the clipboard's plain-text flavour as raw markup; dropping
                // `<p>` and `<strong>` into a Markdown document is never what
                // the user meant, so it is converted on the way in.
                let action = match action {
                    text_editor::Action::Edit(text_editor::Edit::Paste(text))
                        if html::looks_like_html(&text) =>
                    {
                        text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
                            html::to_markdown(&text),
                        )))
                    }
                    action => action,
                };

                let is_edit = action.is_edit();
                // A press inside the editor also anchors a selection drag, so
                // it can carry on over the rendered lines around it.
                let is_click = matches!(action, text_editor::Action::Click(_));
                // A run of vertical moves that starts inside a multi-line
                // region aims for the column it started from, including once
                // it leaves the region.
                if matches!(
                    action,
                    text_editor::Action::Move(text_editor::Motion::Up | text_editor::Motion::Down)
                ) {
                    let _ = self.goal_column();
                }
                self.active_content.perform(action);

                if is_click {
                    self.drag = Some(Drag {
                        anchor: self.cursor_position(),
                    });
                }

                if is_edit {
                    self.is_dirty = true;
                    // Write the (possibly multi-line) editor text back into the
                    // document, keeping `active_end` in sync.
                    self.commit_active();
                }

                Task::none()
            }
            Message::ClickAt(press) => {
                // Visual mode wraps no segment in a `document::Hit`, so this
                // cannot arrive while it is on — but a caret placed from a
                // reading view would be a caret nobody asked for.
                if !self.is_editing() || press.line >= self.lines.len() {
                    return self.focus_active();
                }

                let (line, column) = self.press_target(press);

                // A click always targets a line outside the current raw region
                // (rendered blocks/blanks only); guard against redundant resets.
                if line < self.active_start || line >= self.active_end {
                    self.activate_line(line, column);
                }
                // Pressing is also how a drag-selection starts. Releasing
                // without moving leaves the cursor placed and nothing selected,
                // which is exactly what a plain click should do.
                self.drag = Some(Drag {
                    anchor: self.cursor_position(),
                });
                self.focus_active()
            }
            Message::SelectAll => self.select_all(),
            Message::ExtendSelection(motion) => self.extend_selection(motion),
            Message::DragTo(line) => self.drag_to(line),
            Message::DragEnd => {
                self.drag = None;
                Task::none()
            }
            Message::SplitLine => {
                // Only emitted for a single-line region, so the editor text is
                // exactly the active line.
                let text = self.active_content.text();
                let column = clamp_column(&text, self.active_content.cursor().position.column);
                let left = text[..column].to_owned();
                let right = text[column..].to_owned();

                let start = self.active_start;
                self.record_change(start, 1, 2, (start, column));
                self.splice_lines(start..start + 1, [left, right]);
                let new_index = start + 1;
                self.is_dirty = true;

                self.activate_line(new_index, 0);
                // The editor keeps focus across edits/navigation thanks to its
                // stable tree position, so no (costly) re-focus task is needed.
                Task::none()
            }
            Message::MergeWithPrevious => {
                if self.active_start == 0 {
                    return Task::none();
                }

                let previous_index = self.active_start - 1;
                let join_column = self.lines[previous_index].len();
                // The previous line and the region's first line become one.
                self.record_change(previous_index, 2, 1, (previous_index, join_column));
                let joined = [&self.lines[previous_index], &self.lines[self.active_start]]
                    .map(String::as_str)
                    .concat();
                self.splice_lines(previous_index..previous_index + 2, [joined]);
                self.is_dirty = true;

                self.activate_line(previous_index, join_column);
                Task::none()
            }
            Message::MergeWithNext => {
                // Forward-delete at the end of the region pulls the next line up
                // onto the region's last line (mirror of `MergeWithPrevious`).
                if self.active_end >= self.lines.len() {
                    return Task::none();
                }

                let last_line = self.active_end - 1;
                let join_column = self.lines[last_line].len();
                // The region's last line and the following one become one.
                self.record_change(last_line, 2, 1, (last_line, join_column));
                let joined = [&self.lines[last_line], &self.lines[self.active_end]]
                    .map(String::as_str)
                    .concat();
                self.splice_lines(last_line..last_line + 2, [joined]);
                self.is_dirty = true;

                self.activate_line(last_line, join_column);
                Task::none()
            }
            // The navigation messages below move the active line while the
            // editor is already focused; its stable key keeps that focus, so
            // they must NOT issue a re-focus task (doing so forced a full
            // layout pass on every key press and pegged the CPU). Nor do they
            // scroll: `update` asks the viewport to reveal the caret, which it
            // does against the real layout, in the same frame.
            Message::MoveUp => {
                let goal = self.goal_column();
                // A wrapped line, or a region of several, is walked row by
                // row before it is left.
                if self.step_within_editor(text_editor::Motion::Up) || self.active_start == 0 {
                    return Task::none();
                }
                let target = self.active_start - 1;
                let column = self.landing_column(target, goal, false);
                self.activate_line(target, column);
                Task::none()
            }
            Message::MoveDown => {
                let goal = self.goal_column();
                if self.step_within_editor(text_editor::Motion::Down)
                    || self.active_end >= self.lines.len()
                {
                    return Task::none();
                }
                let target = self.active_end;
                let column = self.landing_column(target, goal, true);
                self.activate_line(target, column);
                Task::none()
            }
            // The page keys move the view along with the caret: `update` asks
            // for a reveal that keeps the caret at the same height on screen.
            // At the ends of the document they go to the very start or end of
            // it, as in every other editor, which still brings it into view.
            Message::PageUp => {
                if self.active_start == 0 {
                    self.activate_line(0, 0);
                    return Task::none();
                }
                let goal = self.goal_column();
                let target = self
                    .active_start
                    .saturating_sub(self.page_lines(self.active_start, false));
                let column = self.landing_column(target, goal, true);
                self.activate_line(target, column);
                Task::none()
            }
            Message::PageDown => {
                let last = self.lines.len() - 1;
                if self.active_end > last {
                    self.activate_line(last, self.lines[last].len());
                    return Task::none();
                }
                let goal = self.goal_column();
                let target =
                    (self.active_start + self.page_lines(self.active_start, true)).min(last);
                let column = self.landing_column(target, goal, true);
                self.activate_line(target, column);
                Task::none()
            }
            Message::JumpToStart => {
                // Ctrl+Home / Ctrl+PgUp: the very start of the document.
                self.activate_line(0, 0);
                Task::none()
            }
            Message::JumpToEnd => {
                // Ctrl+End / Ctrl+PgDown: the very end of the document.
                let last = self.lines.len() - 1;
                let end_column = self.lines[last].len();
                self.activate_line(last, end_column);
                Task::none()
            }
            Message::LinkClicked(url) => {
                // Open the link off the UI thread. `webbrowser::open` spawns a
                // helper process and reads desktop-entry files synchronously
                // (and blocks entirely for text browsers); doing that on the
                // event loop would freeze the editor until it returned.
                std::thread::spawn(move || {
                    let _ = webbrowser::open(&url);
                });
                Task::none()
            }
            Message::NewFile => {
                if !self.is_loading {
                    self.current_file = None;
                    self.load_document("");
                    self.is_dirty = false;
                    return self.focus_active();
                }
                Task::none()
            }
            Message::OpenFile => {
                if self.is_loading {
                    return Task::none();
                }
                self.is_loading = true;

                window::oldest()
                    .and_then(|id| window::run(id, file_ops::open_file_dialog))
                    .then(Task::future)
                    .map(Message::FileOpened)
            }
            Message::FileOpened(result) => {
                self.is_loading = false;
                self.is_dirty = false;

                match result {
                    Ok((path, contents)) => {
                        self.remember_file(&path);
                        self.current_file = Some(path);
                        self.load_document(&contents);
                        self.focus_active()
                    }
                    // A failed read from the command-line argument is worth
                    // reporting; a cancelled open/save dialog is not.
                    Err(FileError::IoError(kind)) => {
                        eprintln!("Error: could not read file: {kind}");
                        Task::none()
                    }
                    Err(FileError::DialogClosed) => Task::none(),
                }
            }
            Message::SaveFile => {
                if self.is_loading {
                    return Task::none();
                }
                self.is_loading = true;

                let current_path = Arc::new(self.current_file.clone());
                let contents = Arc::new(self.document_text());

                window::oldest()
                    .and_then(move |id| {
                        let current_path = current_path.clone();
                        let contents = contents.clone();
                        window::run(id, move |w| {
                            let path = (*current_path).clone();
                            let text = (*contents).clone();
                            file_ops::save_file_dialog(w, path, text)
                        })
                    })
                    .then(Task::future)
                    .map(Message::FileSaved)
            }
            Message::FileSaved(result) => {
                self.is_loading = false;

                if let Ok(path) = result {
                    self.remember_file(&path);
                    self.current_file = Some(path);
                    self.is_dirty = false;
                    // A save is a natural undo boundary.
                    self.history.break_run();
                }

                Task::none()
            }
            Message::Format(action) => {
                // The toolbar greys these out in visual mode; there is no raw
                // line to insert the markup into.
                if !self.is_editing() {
                    return Task::none();
                }

                // Insert the formatted text into the active line by performing
                // edit actions; any existing selection is replaced by typing.
                let selection = self.active_content.selection().unwrap_or_default();
                let formatted = action.apply(&selection);

                for ch in formatted.chars() {
                    self.active_content
                        .perform(text_editor::Action::Edit(text_editor::Edit::Insert(ch)));
                }

                self.is_dirty = true;
                self.commit_active();

                Task::none()
            }
            Message::ThemeChanged(theme) => {
                self.settings.theme = theme;
                // Cached highlights are colored for the previous theme.
                self.code_cache.borrow_mut().clear();
                self.save_config();
                Task::none()
            }
            Message::ToggleMode => {
                self.settings.mode = self.settings.mode.toggled();

                // The search bar replaces text through the raw region, which
                // visual mode does not have.
                if !self.is_editing() {
                    self.search = None;
                }

                // Adding or removing the raw region re-splits the block around
                // the cursor, so the cached segments cannot be slid into
                // agreement — they are walked again from the start. A mode
                // switch is a deliberate, occasional act; `sync` then has
                // nothing left to do.
                self.rebuild();

                // Coming back to editing, the caret has not moved but has been
                // off screen all the while.
                self.request_reveal(RevealMode::Center);
                self.save_config();
                self.focus_active()
            }
            Message::ScrollDocument(step) => self.scroll_document(step),
            Message::ToggleSettingsPanel => {
                self.show_settings_panel = !self.show_settings_panel;
                // The color panel is opened *from* the settings panel, so it
                // must not outlive it.
                if !self.show_settings_panel {
                    self.show_color_panel = false;
                }
                Task::none()
            }
            // Innermost first: the search bar holds focus, then the color panel
            // opened from the settings panel, then the settings panel itself.
            Message::Dismiss => {
                if self.search.is_some() {
                    self.search = None;
                    return self.focus_active();
                }
                if self.show_color_panel {
                    self.show_color_panel = false;
                    return Task::none();
                }
                self.show_settings_panel = false;
                self.focus_active()
            }
            Message::ToggleToolbar => {
                self.settings.show_toolbar = !self.settings.show_toolbar;
                self.save_config();
                Task::none()
            }
            Message::ToggleStatusBar => {
                self.settings.show_status_bar = !self.settings.show_status_bar;
                self.save_config();
                Task::none()
            }
            Message::FontSizeChanged(size) => {
                self.settings.font_size = size;
                // Line heights changed, so every cached offset is stale and the
                // render window would otherwise be computed against the old
                // scale.
                self.remeasure();
                self.save_config();
                Task::none()
            }
            Message::WrapWidthChanged(width) => {
                self.settings.wrap_width = width;
                // A narrower column wraps more, so the height estimates move.
                self.remeasure();
                self.save_config();
                Task::none()
            }
            Message::ToggleRestoreSession => {
                self.settings.restore_session = !self.settings.restore_session;
                self.save_config();
                Task::none()
            }
            Message::ResetSettings => {
                // Preferences only: the colors have their own "Reset all".
                // `last_file` is preserved so resetting preferences does not
                // also forget the session.
                let last_file = self.settings.last_file.take();
                self.settings = Settings {
                    last_file,
                    ..Settings::default()
                };
                self.code_cache.borrow_mut().clear();
                self.remeasure();
                self.save_config();
                Task::none()
            }
            Message::ToggleColorPanel => {
                self.show_color_panel = !self.show_color_panel;
                Task::none()
            }
            Message::ColorHexChanged(tag, hex) => {
                *self.colors.get_mut(tag) = hex;
                self.save_config();
                Task::none()
            }
            Message::ColorPicked(tag, color) => {
                *self.colors.get_mut(tag) = app_theme::to_hex(color);
                self.save_config();
                Task::none()
            }
            Message::ColorReset(tag) => {
                self.colors.get_mut(tag).clear();
                self.save_config();
                Task::none()
            }
            Message::ResetAllColors => {
                self.colors = MarkdownColors::default();
                self.save_config();
                Task::none()
            }
            Message::Scrolled {
                offset_y,
                view_height,
            } => {
                // Pure viewport tracking: no document state changes, no rebuild.
                self.viewport = Some((offset_y, view_height));
                Task::none()
            }
            Message::Undo => {
                if let Some(change) = self.history.pop_undo() {
                    let inverse = self.apply_change(change);
                    self.history.push_redo(inverse);
                    self.is_dirty = true;
                }
                Task::none()
            }
            Message::Redo => {
                if let Some(change) = self.history.pop_redo() {
                    let inverse = self.apply_change(change);
                    self.history.push_undo(inverse);
                    self.is_dirty = true;
                }
                Task::none()
            }
            Message::OpenSearch { replace } => {
                match &mut self.search {
                    // Already open: `Ctrl+H` upgrades a find bar to a replace
                    // bar, and either shortcut re-focuses the query input so
                    // the user can immediately type a new search.
                    Some(search) => search.replacing |= replace,
                    None => {
                        let mut search = Search::new(replace);
                        // Seed from the selection, the way every editor does.
                        if let Some(selected) = self.active_content.selection()
                            && !selected.contains('\n')
                        {
                            search.query = selected;
                        }
                        search.refresh(&self.lines);
                        search.anchor_to(self.active_start);
                        self.search = Some(search);
                    }
                }

                iced::widget::operation::focus(search::QUERY_ID)
            }
            Message::CloseSearch => {
                self.search = None;
                self.focus_active()
            }
            Message::SearchQueryChanged(query) => {
                let Some(search) = &mut self.search else {
                    return Task::none();
                };
                search.query = query;
                self.refresh_search();
                // Jump to the first match at or after the cursor, so the
                // results follow along as the query is typed.
                if let Some(search) = &mut self.search {
                    search.anchor_to(self.active_start);
                }
                self.reveal_current_match()
            }
            Message::ReplacementChanged(replacement) => {
                if let Some(search) = &mut self.search {
                    search.replacement = replacement;
                }
                Task::none()
            }
            Message::FindNext => self.step_match(true),
            Message::FindPrevious => self.step_match(false),
            Message::ToggleSearchCase => {
                let Some(search) = &mut self.search else {
                    return Task::none();
                };
                search.case_sensitive = !search.case_sensitive;
                self.refresh_search();
                self.reveal_current_match()
            }
            Message::ReplaceCurrent => self.replace_current(),
            Message::ReplaceAll => self.replace_all(),
        }
    }

    /// Build the UI: toolbar on top, the inline-rendered document, status bar.
    pub fn view(&self) -> Element<'_, Message> {
        // -- Toolbar --
        //
        // Visual mode is a reading view: everything that would edit the
        // document acts on a caret that is not drawn and a line that is not
        // raw, so those controls are greyed out until editing resumes.
        let editing = self.is_editing();

        let file_controls = row![
            toolbar::icon_button(icons::new_file(), "New (Ctrl+N)", Message::NewFile),
            toolbar::icon_button(icons::open_file(), "Open (Ctrl+O)", Message::OpenFile),
            toolbar::icon_button(icons::save_file(), "Save (Ctrl+S)", Message::SaveFile),
            toolbar::separator(),
            toolbar::icon_button_maybe(
                icons::undo(),
                "Undo (Ctrl+Z)",
                (editing && self.history.can_undo()).then_some(Message::Undo),
            ),
            toolbar::icon_button_maybe(
                icons::redo(),
                "Redo (Ctrl+Y)",
                (editing && self.history.can_redo()).then_some(Message::Redo),
            ),
        ]
        .spacing(4)
        .align_y(iced::Center);

        let format_bar = toolbar::view(editing);

        // Shows the mode it switches *to*, like every other toggle button.
        let mode_toggle = if editing {
            toolbar::icon_button(
                icons::eye(),
                "Visual mode — render every line (Ctrl+E)",
                Message::ToggleMode,
            )
        } else {
            toolbar::icon_button(
                icons::pencil(),
                "Editor mode — edit the cursor's line (Ctrl+E)",
                Message::ToggleMode,
            )
        };

        // Theme selector and settings toggle, pushed to the right edge of the
        // toolbar.
        let theme_picker = row![
            mode_toggle,
            toolbar::separator(),
            text("Theme").size(13).style(app_theme::status_text),
            pick_list(
                Theme::ALL,
                Some(self.settings.theme.clone()),
                Message::ThemeChanged
            )
            .text_size(13)
            .padding([4, 8]),
            toolbar::icon_button(
                icons::settings(),
                "Settings (Ctrl+,)",
                Message::ToggleSettingsPanel,
            ),
        ]
        .spacing(8)
        .align_y(iced::Center);

        // Hidden entirely when the preference is off. The panel below stays
        // reachable through `Ctrl+,`, so the toolbar can always be brought back.
        let toolbar: Option<Element<'_, Message>> = self.settings.show_toolbar.then(|| {
            container(
                row![
                    file_controls,
                    toolbar::separator(),
                    format_bar,
                    space::horizontal(),
                    theme_picker,
                ]
                .spacing(12)
                .align_y(iced::Center),
            )
            .padding([8, 12])
            .width(Fill)
            .style(app_theme::toolbar_container)
            .into()
        });

        // -- Per-tag color customization panel (collapsible) --
        let color_panel: Option<Element<'_, Message>> = if self.show_color_panel {
            let theme = self.theme();
            let rows: Vec<Element<'_, Message>> = MarkdownTag::ALL
                .iter()
                .map(|&tag| {
                    let hex = self.colors.get(tag).to_owned();
                    let effective = match tag {
                        MarkdownTag::H1 => self.colors.heading_color(1, &theme),
                        MarkdownTag::H2 => self.colors.heading_color(2, &theme),
                        MarkdownTag::H3 => self.colors.heading_color(3, &theme),
                        MarkdownTag::H4 => self.colors.heading_color(4, &theme),
                        MarkdownTag::H5 => self.colors.heading_color(5, &theme),
                        MarkdownTag::H6 => self.colors.heading_color(6, &theme),
                        MarkdownTag::Strong => self.colors.strong_color(&theme),
                        MarkdownTag::Emphasis => self.colors.emphasis_color(&theme),
                        MarkdownTag::InlineCode => self.colors.inline_code_color(&theme),
                        MarkdownTag::InlineCodeBackground => {
                            self.colors.inline_code_background(&theme)
                        }
                        MarkdownTag::Link => self.colors.link_color(&theme),
                        MarkdownTag::CodeBlockBackground => {
                            self.colors.code_block_background(&theme)
                        }
                        MarkdownTag::TableBorder => self.colors.table_border_color(&theme),
                        MarkdownTag::TableHeaderBackground => {
                            self.colors.table_header_background_color(&theme)
                        }
                        MarkdownTag::TableHeaderText => self.colors.table_header_text_color(&theme),
                    };
                    color_picker::color_row(tag, &hex, effective)
                })
                .collect();

            let reset_all = button(text("Reset all to theme").size(12))
                .padding([4, 10])
                .style(app_theme::toolbar_button)
                .on_press(Message::ResetAllColors);

            Some(
                // Fifteen rows, stacked *below* the settings panel it is opened
                // from: capped and scrolled so the two together cannot squeeze
                // the document off screen on a short window.
                container(
                    scrollable(
                        column(rows)
                            .spacing(6)
                            .push(space::vertical().height(4))
                            .push(reset_all),
                    )
                    .height(Length::Shrink),
                )
                .padding([8, 16])
                .max_height(COLOR_PANEL_MAX_HEIGHT)
                .width(Fill)
                .style(app_theme::toolbar_container)
                .into(),
            )
        } else {
            None
        };

        // -- Inline document --
        let theme = self.theme();
        let font_size = self.settings.font_size.pixels();
        let settings = markdown_view::settings(&theme, &self.colors, font_size);
        let highlight_theme = app_theme::code_highlighter(&theme);
        let viewer =
            markdown_view::Viewer::new(&theme, &self.colors, &self.code_cache, &self.text_hits);

        // Cursor context for the active region's key bindings (computed once).
        let cursor = self.active_content.cursor();
        let cursor_line_len = self
            .lines
            .get(self.active_start + cursor.position.line)
            .map(String::len)
            .unwrap_or(0);
        let ctx = BindingContext {
            multiline: self.active_end - self.active_start > 1,
            has_selection: self.has_selection(),
            at_editor_top: cursor.position.line == 0,
            at_editor_bottom: cursor.position.line + 1 >= self.active_content.line_count(),
            at_line_start: cursor.position.column == 0,
            at_line_end: cursor.position.column >= cursor_line_len,
            at_doc_first: self.active_start == 0,
            at_doc_last: self.active_end >= self.lines.len(),
        };

        // Every segment is a child of one keyed column, so each keeps its
        // widget state — the editor its keyboard focus and highlighter cache,
        // a rendered block its shaped text — for as long as it exists, however
        // the children around it come and go.
        //
        // Windowed rendering: for large documents only the segments near the
        // current scroll viewport are built into the widget tree; each run of
        // the rest is replaced by a single spacer as tall as the lines it
        // stands for, so the widget tree stays proportional to what is on
        // screen rather than to the size of the document. Until the scrollable
        // reports a viewport (first frame, small documents whose window already
        // covers every line, or tests with no real layout) the window is `None`
        // and the whole document renders.
        let window = self.render_window();

        // The rendered lines only follow the pointer while a drag is actually
        // under way. Following it unconditionally would post a message on
        // every mouse move across the document — a full `update` per hovered
        // line, for nothing.
        let dragging = self.drag.is_some();

        let mut body = document::Column::new(&self.measurements)
            .spacing(SEGMENT_SPACING)
            .padding(DOCUMENT_PADDING)
            // A configured text width caps the measure and centres the text;
            // the scrollable still fills the window.
            .max_width(self.settings.wrap_width.pixels())
            .monospace(font_size);

        // Only the segments in the window are visited — found by binary
        // search, so building the view costs the same wherever in however
        // long a document it is — plus, in editor mode, the editor, which
        // always renders for real: it holds the keyboard focus and the caret.
        let region = self.active_region();
        let active = region.unwrap_or_default();
        let bounds = |segment: &Segment| segment.lines(active);
        let (first, last) = match window {
            Some((top, bottom)) => (
                self.segments.partition_point(|s| bounds(s).1 <= top),
                self.segments.partition_point(|s| bounds(s).0 < bottom),
            ),
            None => (0, self.segments.len()),
        };
        // Visual mode has no editor to keep in the tree, so the window is all
        // there is to visit.
        let editor =
            region.map(|(start, _)| self.segments.partition_point(|s| bounds(s).1 <= start));
        let visited = match editor {
            Some(editor) if editor < first => [editor..editor + 1, first..last],
            Some(editor) if editor >= last => [first..last, editor..editor + 1],
            _ => [first..last, 0..0],
        };

        // `blocks` holds one entry per Block segment, rendered or not.
        let mut block_index = self.segments[..first]
            .iter()
            .filter(|s| matches!(s, Segment::Block { .. }))
            .count();
        // The first line no child stands for yet; lines skipped over become
        // spacers.
        let mut covered = 0;
        let mut gaps = 0;

        for segment in visited
            .into_iter()
            .flatten()
            .map(|index| &self.segments[index])
        {
            let (start, end) = bounds(segment);

            if start > covered {
                body = body.push(
                    Key::Gap(gaps),
                    (covered, start),
                    self.spacer((covered, start)),
                );
                gaps += 1;
            }
            covered = end;

            match segment {
                // The cursor line (or table, or code block) shown as a focused
                // raw `text_editor`.
                Segment::Active => {
                    let editor = text_editor(&self.active_content)
                        .id(ACTIVE_LINE_ID)
                        .placeholder("Type Markdown…")
                        .on_action(Message::EditorAction)
                        .padding(LINE_PADDING)
                        .font(Font::MONOSPACE)
                        .size(font_size)
                        // Markdown, with fenced code highlighted by its own
                        // language directly (see `highlighter`).
                        .highlight_with::<highlighter::Highlighter>(
                            highlighter::Settings {
                                theme: highlight_theme,
                            },
                            |highlight, theme| highlight.to_format(theme),
                        )
                        .key_binding(move |key_press| editor_binding(key_press, ctx))
                        // No box around the cursor's line: it is drawn flush
                        // with the rendered rows, marked only by the caret.
                        .style(app_theme::active_line);

                    body = body.push(Key::Editor, active, container(editor).width(Fill));
                }
                Segment::Blank(index) => {
                    // A space keeps the empty line clickable and gives the
                    // document natural paragraph spacing.
                    let blank = container(text(" ").font(Font::MONOSPACE).size(font_size))
                        .width(Fill)
                        .padding(LINE_PADDING);

                    body = body.push(
                        Key::Blank(*index),
                        (start, end),
                        self.hit(blank, start, end, dragging),
                    );
                }
                Segment::Block { .. } => {
                    let rendered: Element<'_, Message> = match self.blocks.get(block_index) {
                        Some(content) => {
                            viewer.set_block_source(
                                self.block_sources.get(block_index).map(String::as_str),
                            );
                            markdown::view_with(content.items(), settings, &viewer)
                                .map(Message::LinkClicked)
                        }
                        // Defensive fallback; `blocks` is kept aligned with the
                        // segment walk, so this should not happen.
                        None => text(self.lines[start..end].join("\n")).into(),
                    };
                    let key = self
                        .block_ids
                        .get(block_index)
                        .map_or(Key::Blank(start), |&id| Key::Block(id));

                    body = body.push(
                        key,
                        (start, end),
                        self.hit(
                            container(rendered).width(Fill).padding(LINE_PADDING),
                            start,
                            end,
                            dragging,
                        ),
                    );
                    block_index += 1;
                }
            }
        }

        if covered < self.lines.len() {
            let rest = (covered, self.lines.len());
            body = body.push(Key::Gap(gaps), rest, self.spacer(rest));
        }

        let scrollable = scrollable(body).id(DOCUMENT_ID).width(Fill).height(Fill);

        // Pinned to its content, and carrying out the latest request to show
        // the caret against the real layout. It reports the viewport — so
        // view() can move the render window — only when the view nears the
        // edge of what is rendered: every other scroll step is a plain redraw,
        // with no update, view or layout at all.
        let document = container(
            document::Viewport::new(scrollable, &self.measurements)
                .reveal(self.reveal)
                .on_scroll(|offset_y, view_height| Message::Scrolled {
                    offset_y,
                    view_height,
                }),
        )
        .width(Fill)
        .height(Fill)
        .style(app_theme::editor_pane);

        // -- Status bar --
        let file_display = self
            .current_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| String::from("Untitled"));

        // Visual mode draws no caret, so a line and column would point at
        // something invisible; it reports the mode instead.
        let position = if editing {
            let active_row = self.active_start + cursor.position.line;
            let char_column = self
                .lines
                .get(active_row)
                .and_then(|line| line.get(..cursor.position.column))
                .map(|prefix| prefix.chars().count())
                .unwrap_or(0);

            format!("Ln {}, Col {}", active_row + 1, char_column + 1)
        } else {
            String::from("Visual")
        };

        let status_bar: Option<Element<'_, Message>> = self.settings.show_status_bar.then(|| {
            container(
                row![
                    text(format!("{} lines", self.lines.len()))
                        .size(12)
                        .style(app_theme::status_text),
                    space::horizontal(),
                    text(file_display).size(12).style(app_theme::status_text),
                    space::horizontal().width(16),
                    text(if self.is_dirty { "Modified" } else { "Saved" })
                        .size(12)
                        .style(app_theme::status_text),
                    space::horizontal().width(16),
                    text(position).size(12).style(app_theme::status_text),
                ]
                .align_y(iced::Center),
            )
            .padding([4, 12])
            .width(Fill)
            .style(app_theme::toolbar_container)
            .into()
        });

        // Chrome is stacked above the document in the order it can be opened:
        // toolbar, settings, colors (opened from settings), search.
        //
        // Every piece has a slot of its own, filled with nothing while it is
        // closed. A column matches its children to their widget state by
        // position: were a closed panel simply left out, opening it would
        // move the document down a slot and hand the document's state —
        // scroll position, focused editor and all — to the panel.
        //
        // The filler must be `Shrink`, not a fixed zero height: a column
        // drops children of fixed zero size altogether.
        fn slot(chrome: Option<Element<'_, Message>>) -> Element<'_, Message> {
            chrome.unwrap_or_else(|| space::Space::new().into())
        }

        Column::new()
            .push(slot(toolbar))
            .push(slot(
                self.show_settings_panel
                    .then(|| settings::view(&self.settings)),
            ))
            .push(slot(color_panel))
            .push(slot(self.search.as_ref().map(search::view)))
            .push(document)
            .push(slot(status_bar))
            .into()
    }

    /// Return the application theme selected by the user.
    pub fn theme(&self) -> Theme {
        self.settings.theme.clone()
    }

    /// The line range shown as raw, editable source — `None` in visual mode,
    /// where the whole document renders.
    ///
    /// The single source of truth for "is any line raw": the segment walk, the
    /// view and [`MarkdownEditor::sync`] all ask here rather than reading
    /// [`Settings::mode`] themselves, so they cannot disagree about it.
    fn active_region(&self) -> Option<(usize, usize)> {
        self.settings
            .mode
            .is_editor()
            .then_some((self.active_start, self.active_end))
    }

    /// Whether the cursor's line is shown as raw source and can be edited.
    fn is_editing(&self) -> bool {
        self.settings.mode.is_editor()
    }

    /// Layout estimates for the current font size and text width — and the
    /// document's real width, once it has been laid out.
    fn metrics(&self) -> Metrics {
        Metrics::new(self.settings.font_size, self.settings.wrap_width)
            .measured((self.measured_width > 0.0).then_some(self.measured_width))
    }

    /// Whether some open chrome needs the global key listener installed.
    ///
    /// Named rather than inlined into [`MarkdownEditor::subscription`] so the
    /// "no listener during ordinary editing" invariant is directly testable —
    /// `Subscription` exposes no way to ask whether it is empty.
    fn needs_key_listener(&self) -> bool {
        self.search.is_some() || self.show_settings_panel || self.show_color_panel
    }

    /// Events that cannot go through the focused widget.
    ///
    /// Every listener below is installed only while something actually needs
    /// one, so ordinary editing never pays for any of them.
    pub fn subscription(&self) -> Subscription<Message> {
        let panels = self.needs_key_listener().then(panel_keys);
        // Visual mode puts no editor in the tree, so nothing holds the
        // keyboard: the keys it does answer to have to be listened for.
        let visual = (!self.is_editing()).then(visual_keys);
        // The button can be released anywhere — over another widget, or outside
        // the window entirely — and no widget would report that, so the end of
        // a drag has to be watched for globally.
        let drag = self.drag.is_some().then(mouse_release);

        Subscription::batch(panels.into_iter().chain(visual).chain(drag))
    }

    /// Replace the whole document with `text`, resetting the active region.
    fn load_document(&mut self, text: &str) {
        self.line_ending = line_ending_of(text);
        self.lines = split_lines(text);
        self.revision += 1;
        self.fences_dirty = true;
        // Nothing of the old document survives: re-segment all of it.
        self.structure_changed = true;
        // Nothing of the new document has been measured yet.
        self.remeasure();
        self.activate_line(0, 0);
        self.history.clear();
        // Highlighted blocks from the old document are no longer referenced.
        self.code_cache.borrow_mut().clear();
        // A freshly loaded document starts at the top. Seed the viewport with an
        // assumed top-of-document window (rather than `None`, which renders the
        // whole document) so the very first frame only builds — and only syntax-
        // highlights the code blocks of — the lines near the top. Rendering a
        // large file in full on the first frame highlighted every code block at
        // once and stalled the open. The scrollable overwrites this with its
        // real bounds on the next layout.
        self.viewport = Some((0.0, INITIAL_VIEWPORT_HEIGHT));
    }

    /// Reconstruct the full document text from its lines, with the line
    /// ending the file was loaded with.
    fn document_text(&self) -> String {
        self.lines.join(self.line_ending)
    }

    /// Re-segment the whole document and rebuild the rendered blocks, reusing
    /// the parse of every block whose text is unchanged.
    ///
    /// O(document): only for when the document was replaced or a code fence
    /// changed. Everything else goes through [`MarkdownEditor::sync`].
    fn rebuild(&mut self) {
        self.resegment();
        self.rebuild_blocks();
        self.dirty = None;
        self.structure_changed = false;
        self.synced_region = self.active_region();
        self.refresh_offsets();
    }

    /// Bring the cached segments and blocks up to date with the lines marked
    /// dirty, re-walking only the segments around them.
    ///
    /// A segment boundary is decided by the two lines on either side of it —
    /// whether each is blank, and whether either is an edge of the active
    /// region — and by the code fences. So every boundary with both of its
    /// lines outside the dirty range is exactly where it was (fences are only
    /// shifted here: a changed fence goes through [`MarkdownEditor::rebuild`]
    /// instead). The walk restarts at the last such boundary before the
    /// range, stops at the first one after it, and the segments in between
    /// are replaced; the ones after only move by the lines gained or lost.
    fn sync(&mut self) {
        let region = self.active_region();
        if self.dirty.is_none() && region == self.synced_region {
            return;
        }
        // Switching between editor and visual mode adds or removes the raw
        // region, and with it the block boundaries the region split: there is
        // no incremental path from one mode's segments to the other's.
        if self.fences_dirty
            || self.segments.is_empty()
            || region.is_some() != self.synced_region.is_some()
        {
            return self.rebuild();
        }

        // In visual mode no segment carries the region, so any range does.
        let old_region = self.synced_region.unwrap_or_default();
        let bounds = |segment: &Segment| segment.lines(old_region);

        let mut dirty = self.dirty.take().unwrap_or(Dirty {
            start: old_region.0,
            end: old_region.1,
            delta: 0,
        });

        // Both the region the segments were built for and the current one
        // render differently from the lines around them, so both are walked
        // again. The old one is in the old numbering: lines before the dirty
        // range kept their index, lines after it moved by `delta`. Visual mode
        // has neither region, and so nothing to widen by.
        if let Some(region) = region {
            let old_end = dirty.old_end();
            // A position inside the dirty range has no single new number; the
            // range's own edge is always a safe (wider) choice.
            let renumbered_start = if old_region.0 < dirty.start {
                old_region.0
            } else if old_region.0 >= old_end {
                old_region.0.wrapping_add_signed(dirty.delta)
            } else {
                dirty.start
            };
            let renumbered_end = if old_region.1 <= dirty.start {
                old_region.1
            } else if old_region.1 >= old_end {
                old_region.1.wrapping_add_signed(dirty.delta)
            } else {
                dirty.end
            };

            dirty.start = dirty.start.min(renumbered_start).min(region.0);
            dirty.end = dirty.end.max(renumbered_end).max(region.1);
        }

        dirty.end = dirty.end.min(self.lines.len());
        let old_end = dirty.old_end();

        // Segments ending before the range are untouched; so are the ones
        // starting after it. Everything in between is walked again.
        let first = self.segments.partition_point(|s| bounds(s).1 < dirty.start);
        let last = self.segments.partition_point(|s| bounds(s).0 <= old_end);

        let from = self
            .segments
            .get(first)
            .map_or(dirty.start, |s| bounds(s).0.min(dirty.start));
        let to = self.segments.get(last).map_or(self.lines.len(), |s| {
            bounds(s).0.wrapping_add_signed(dirty.delta)
        });

        let mut fresh = Vec::new();
        walk_segments(&self.lines, &self.fences, region, from..to, &mut fresh);

        // The blocks of the replaced segments, in the dense block vectors.
        let block_start = self.segments[..first]
            .iter()
            .filter(|s| matches!(s, Segment::Block { .. }))
            .count();
        let replaced_blocks = self.segments[first..last]
            .iter()
            .filter(|s| matches!(s, Segment::Block { .. }))
            .count();

        self.replace_blocks(block_start..block_start + replaced_blocks, &fresh);

        let after = first + fresh.len();
        self.segments.splice(first..last, fresh);
        if dirty.delta != 0 {
            for segment in &mut self.segments[after..] {
                segment.shift(dirty.delta);
            }
        }

        self.synced_region = region;
    }

    /// Replace the blocks at `range` of the block vectors with the blocks of
    /// `segments`, keeping the parse (and the widget key) of any old one whose
    /// text reappears.
    fn replace_blocks(&mut self, range: Range<usize>, segments: &[Segment]) {
        let mut reusable: Vec<Option<(String, markdown::Content, u64)>> = self
            .block_sources
            .drain(range.clone())
            .zip(self.blocks.drain(range.clone()))
            .zip(self.block_ids.drain(range.clone()))
            .map(|((source, content), id)| Some((source, content, id)))
            .collect();

        let mut sources = Vec::new();
        let mut blocks = Vec::new();
        let mut ids = Vec::new();

        for segment in segments {
            let Segment::Block { start, end } = *segment else {
                continue;
            };

            let old = reusable.iter_mut().find(|old| {
                old.as_ref().is_some_and(|(source, _, _)| {
                    block_source_eq(&self.lines, (start, end), source)
                })
            });

            let (source, content, id) = match old.and_then(Option::take) {
                Some(reused) => reused,
                None => {
                    let source = self.lines[start..end].join("\n");
                    let content = markdown::Content::parse(&source);
                    let id = self.next_block_id;
                    self.next_block_id += 1;
                    (source, content, id)
                }
            };

            sources.push(source);
            blocks.push(content);
            ids.push(id);
        }

        let at = range.start;
        self.block_sources.splice(at..at, sources);
        self.blocks.splice(at..at, blocks);
        self.block_ids.splice(at..at, ids);
    }

    /// Recompute the cached [`Segment`] list from the lines and active range.
    ///
    /// The old buffer's allocation is reused, so a re-segmentation of an
    /// unchanged-size document allocates nothing.
    fn resegment(&mut self) {
        self.refresh_fences();

        let mut segments = std::mem::take(&mut self.segments);
        collect_segments(
            &self.lines,
            &self.fences,
            self.active_region(),
            &mut segments,
        );
        self.segments = segments;
    }

    /// Recompute [`MarkdownEditor::fences`] if an edit may have moved one.
    fn refresh_fences(&mut self) {
        if std::mem::take(&mut self.fences_dirty) {
            self.fences = fences::blocks(&self.lines);
        }
    }

    /// Estimate every line's height afresh, discarding what was measured.
    ///
    /// Must be re-run whenever the font size or text width changes: both scale
    /// every line's height, and [`MarkdownEditor::line_offsets`] is the sole
    /// offset↔line mapping. Measurements taken at the old size would be wrong,
    /// so they go too; the lines on screen are measured again on the next
    /// layout.
    fn remeasure(&mut self) {
        self.refresh_fences();
        let metrics = self.metrics();

        self.line_heights.clear();
        self.line_heights
            .extend(self.lines.iter().enumerate().map(|(index, line)| {
                estimate_line_height(line, fences::in_code(&self.fences, index), metrics)
            }));

        self.line_offsets.clear();
        self.offsets_stale_from = Some(0);
        self.refresh_offsets();
    }

    /// Replace `lines[range]` with `replacement`, keeping the per-line height
    /// table in step: new lines start out estimated.
    ///
    /// Every edit that changes the number of lines goes through here, so
    /// measured heights survive edits elsewhere in the document.
    fn splice_lines(&mut self, range: Range<usize>, replacement: impl IntoIterator<Item = String>) {
        let start = range.start;
        let before = self.lines.len();
        let removed_fence = self.lines[range.clone()]
            .iter()
            .any(|line| fences::is_fence(line));

        self.lines.splice(range.clone(), replacement);
        let inserted = self.lines.len() + range.len() - before;
        let delta = inserted as isize - range.len() as isize;
        let added_fence = self.lines[start..start + inserted]
            .iter()
            .any(|line| fences::is_fence(line));

        // Fences pair up by their order alone, so unless a fence line itself
        // came or went, every code block is the same block, moved.
        if removed_fence || added_fence {
            self.fences_dirty = true;
            self.structure_changed = true;
        } else {
            fences::shift(&mut self.fences, range.clone(), delta);
        }

        let metrics = self.metrics();
        let heights: Vec<f32> = self.lines[start..start + inserted]
            .iter()
            .map(|line| estimate_line_height(line, false, metrics))
            .collect();
        self.line_heights.splice(range.clone(), heights);

        Dirty::splice(&mut self.dirty, range, inserted);
        self.revision += 1;
        self.mark_offsets_stale(start);
    }

    /// Record that the text of `range` changed in place, without adding or
    /// removing lines.
    fn touch_lines(&mut self, range: Range<usize>, fences_before: &[(usize, u8, usize, bool)]) {
        if fences::signature(&self.lines[range.clone()]) != fences_before {
            self.fences_dirty = true;
            self.structure_changed = true;
        }

        Dirty::touch(&mut self.dirty, range);
        self.revision += 1;
    }

    /// Note that the offsets of `line` and everything after it are stale.
    fn mark_offsets_stale(&mut self, line: usize) {
        self.offsets_stale_from = Some(self.offsets_stale_from.map_or(line, |from| from.min(line)));
    }

    /// Bring [`MarkdownEditor::line_offsets`] up to date with the heights,
    /// from the first stale line down.
    fn refresh_offsets(&mut self) {
        let Some(from) = self.offsets_stale_from.take() else {
            return;
        };

        let from = from.min(self.line_heights.len());
        self.line_offsets.truncate(from + 1);
        if self.line_offsets.len() < from + 1 {
            // The table was cleared (or never built): start from the top.
            self.line_offsets.clear();
            self.line_offsets.push(0.0);
        }

        let first = self.line_offsets.len() - 1;
        let mut top = self.line_offsets[first];
        for height in &self.line_heights[first..] {
            top += height;
            self.line_offsets.push(top);
        }
    }

    /// Fold what the document column measured at its last layout into the
    /// per-line heights, so spacers and the render window follow the real
    /// layout rather than estimates.
    ///
    /// Each laid-out segment's height (plus the spacing after it) is shared
    /// among its lines in proportion to their estimates.
    fn absorb_measurements(&mut self) {
        let (generation, width) = {
            let measurements = self.measurements.borrow();
            (measurements.generation, measurements.width)
        };
        if generation == self.measured_generation {
            return;
        }
        self.measured_generation = generation;

        // Text rewraps at a new width: every height measured at the old one is
        // stale. Start again from estimates made for the new width.
        if (width - self.measured_width).abs() > 0.5 {
            self.measured_width = width;
            self.remeasure();
        }

        self.refresh_fences();
        let metrics = self.metrics();
        let measurements = self.measurements.borrow();
        let mut stale: Option<usize> = None;

        for placed in &measurements.children {
            let (start, end) = placed.lines;
            if matches!(placed.key, Key::Gap(_)) || start >= end || end > self.lines.len() {
                continue;
            }

            let estimates: Vec<f32> = (start..end)
                .map(|line| {
                    let in_code = fences::in_code(&self.fences, line);
                    estimate_line_height(&self.lines[line], in_code, metrics)
                })
                .collect();
            let total: f32 = estimates.iter().sum();
            let measured = placed.height + SEGMENT_SPACING;

            for (height, estimate) in self.line_heights[start..end].iter_mut().zip(estimates) {
                let share = measured * estimate / total;
                if (*height - share).abs() > 0.01 {
                    *height = share;
                    stale = Some(stale.map_or(start, |first| first.min(start)));
                }
            }
        }

        drop(measurements);
        if let Some(line) = stale {
            self.mark_offsets_stale(line);
        }
    }

    /// Lines to build as real widgets, or `None` for all of them.
    fn render_window(&self) -> Option<(usize, usize)> {
        render_window(self.viewport, &self.line_offsets, self.lines.len())
    }

    /// A spacer standing in for the off-screen document `lines`.
    fn spacer(&self, (start, end): (usize, usize)) -> Element<'_, Message> {
        let top = self.line_top(start);
        let bottom = self.line_top(end);
        // The run is one child where it would have been several: it keeps the
        // spacing after itself, and loses the ones between its segments.
        let height = (bottom - top - SEGMENT_SPACING).max(0.0);

        space::horizontal().height(height).into()
    }

    /// Make a rendered segment covering `start..end` respond to the pointer
    /// line by line.
    ///
    /// Visual mode has nowhere to put a caret, so the segment is left bare:
    /// no press is hit-tested and no drag is followed. Links inside it are
    /// handled by the rendered Markdown itself and keep working.
    fn hit<'a>(
        &'a self,
        content: impl Into<Element<'a, Message>>,
        start: usize,
        end: usize,
        dragging: bool,
    ) -> Element<'a, Message> {
        if !self.is_editing() {
            return content.into();
        }

        let weights = self.line_heights.get(start..end).unwrap_or_default();

        document::Hit::new(content, start, weights, &self.text_hits, Message::ClickAt)
            .on_drag_maybe(dragging.then_some(Message::DragTo))
            .into()
    }

    /// Ask the viewport to show the caret, the next time it is laid out.
    ///
    /// Visual mode draws no caret and puts no editor in the tree for the
    /// viewport to measure against, so there is nothing to reveal.
    fn request_reveal(&mut self, mode: RevealMode) {
        if !self.is_editing() {
            return;
        }

        let generation = self.reveal.map_or(1, |reveal| reveal.generation + 1);

        self.reveal = Some(Reveal {
            generation,
            span: self.caret_span(),
            mode,
        });
    }

    /// The caret's row of the editor, with a row of context above and below,
    /// as distances from the editor's top edge.
    fn caret_span(&self) -> (f32, f32) {
        let (top, bottom) = self.caret_row_span();
        let row = bottom - top;

        (top - row, bottom + row)
    }

    /// The caret's row of the editor, as distances from its top edge.
    fn caret_row_span(&self) -> (f32, f32) {
        let row = self.settings.font_size.pixels() * LINE_HEIGHT;
        let top = f32::from(LINE_PADDING) + self.caret_row() as f32 * row;

        (top, top + row)
    }

    /// Visual rows of the editor above the caret's.
    fn caret_row(&self) -> usize {
        let capacity = self.wrap_capacity();
        let (line, column) = self.cursor_position();
        let line = line.min(self.lines.len() - 1);

        let above: usize = self.lines[self.active_start.min(line)..line]
            .iter()
            .map(|text| wrap::rows(text, capacity).len())
            .sum();

        above + wrap::row_of(&wrap::rows(&self.lines[line], capacity), column)
    }

    /// Characters per row of the raw editor, as last laid out. Unbounded until
    /// the document has been laid out once: nothing wraps before then.
    fn wrap_capacity(&self) -> usize {
        let text_width = self.measured_width - 2.0 * f32::from(LINE_PADDING);
        if text_width <= 0.0 {
            return usize::MAX;
        }

        ((text_width / self.monospace_advance()).floor() as usize).max(1)
    }

    /// Width of one character of the raw editor's monospace text: measured
    /// once the editor has been laid out at the current size, the typical
    /// 0.6 em until then.
    fn monospace_advance(&self) -> f32 {
        let size = self.settings.font_size.pixels();

        match self.measurements.borrow().monospace {
            Some((measured, advance)) if measured == size && advance > 0.0 => advance,
            _ => size * 0.6,
        }
    }

    /// The visual column the caret aims for across vertical moves, taken from
    /// its current position when a run of them starts.
    fn goal_column(&mut self) -> usize {
        if let Some(goal) = self.goal_column {
            return goal;
        }

        let (line, column) = self.cursor_position();
        let text = &self.lines[line.min(self.lines.len() - 1)];
        let goal = wrap::column_in_row(text, &wrap::rows(text, self.wrap_capacity()), column);

        self.goal_column = Some(goal);
        goal
    }

    /// Where the caret lands on `line` when a vertical move carries it there:
    /// `goal` characters into the line's first row when moving down, or into
    /// its last row when moving up — so a wrapped line is entered at the row
    /// nearest to where the caret came from.
    fn landing_column(&self, line: usize, goal: usize, downward: bool) -> usize {
        let text = &self.lines[line];
        let rows = wrap::rows(text, self.wrap_capacity());
        let row = if downward { 0 } else { rows.len() - 1 };

        wrap::offset_in_row(text, &rows, row, goal)
    }

    /// Move the caret one visual row inside the editor, reporting whether it
    /// moved: it cannot when already on the editor's first or last row.
    ///
    /// Done by the editor itself, which knows exactly where its lines wrap.
    fn step_within_editor(&mut self, motion: text_editor::Motion) -> bool {
        // A selection is dropped where its moving end is, and the caret moves
        // on from there — rather than the first key press only collapsing it.
        // Without one the caret is left exactly as it is: placing it again
        // could move it to the other side of a wrap point.
        if self.active_content.cursor().selection.is_some() {
            let head = self.active_content.cursor().position;
            self.place_caret(head);
        }

        let before = self.active_content.cursor().position;
        self.active_content
            .perform(text_editor::Action::Move(motion));

        self.active_content.cursor().position != before
    }

    /// The `(line, byte column)` of the source a press on a rendered segment
    /// landed on.
    ///
    /// A press on rendered text knows the exact character it hit, which the
    /// source map traces back through the Markdown that produced it. A press
    /// beside the text — in a margin, on a bullet or a rule — falls back to
    /// an estimate from the pointer's position.
    fn press_target(&mut self, press: Press) -> (usize, usize) {
        let (start, end) = press.segment;

        if let Some(hit) = press.text
            && end <= self.lines.len()
            && let Some((line, column)) =
                source_map::locate(&self.lines[start..end], hit.item, hit.offset)
        {
            return (start + line, column);
        }

        (press.line, self.click_column(press))
    }

    /// The byte column a press beside a rendered line's text corresponds to,
    /// estimated.
    ///
    /// Without the text under the pointer to go by, the press is placed by
    /// proportion: the row it fell on, then the characters from the row's
    /// start to the pointer at the size the line is rendered in.
    fn click_column(&mut self, press: Press) -> usize {
        self.refresh_fences();

        let text = &self.lines[press.line];
        let font_size = self.settings.font_size.pixels();
        let x = (press.x - f32::from(LINE_PADDING)).max(0.0);

        if fences::in_code(&self.fences, press.line) {
            // Code renders monospace at the code size, inside the block's
            // padding, and scrolls sideways rather than wrapping.
            let code = font_size * CODE_SCALE;
            let chars = (x - code * 1.25) / (self.monospace_advance() * CODE_SCALE);
            return char_offset(text, chars.round().max(0.0) as usize);
        }

        let (prefix, scale) = rendered_prefix(text);
        let width = EST_CHAR_WIDTH * font_size / f32::from(settings::BASE_FONT_SIZE) * scale;
        let text_width = self.measured_width - 2.0 * f32::from(LINE_PADDING);
        let per_row = (text_width / width).floor().max(1.0);
        let rows = ((text.len() - prefix) as f32 / per_row).ceil().max(1.0);
        let row = (press.fraction * rows).floor().min(rows - 1.0);
        let chars = row * per_row + (x / width).round();

        prefix + char_offset(&text[prefix..], chars as usize)
    }

    /// Document lines one screen spans from `from`, towards the end of the
    /// document when `forward` and towards its start otherwise — how far
    /// `PgUp`/`PgDown` move the active line.
    ///
    /// Measured through `line_offsets` rather than counted as a fixed number of
    /// lines, so a page is a real page: wrapped paragraphs and larger fonts
    /// make it shorter, exactly as they make fewer lines fit on screen. The
    /// viewport then scrolls by exactly as far as the caret moved, so the
    /// caret keeps its place on screen.
    ///
    /// Measured from the caret in the direction it is about to travel — over
    /// the lines the page key scrolls into view. Near the end of the document
    /// there is not a screenful left *below* the caret; measured that way,
    /// `PgUp` would shrink to a line exactly where a full page is wanted.
    ///
    /// At least one line, so a page key always moves.
    fn page_lines(&self, from: usize, forward: bool) -> usize {
        let Some((_, view_height)) = self.viewport else {
            return PAGE_LINES;
        };

        let top = self.line_top(from);
        let to = if forward {
            line_at_offset(&self.line_offsets, top + view_height)
        } else {
            line_at_offset(&self.line_offsets, (top - view_height).max(0.0))
        };

        to.abs_diff(from).max(1)
    }

    /// Scroll the document by `step`.
    ///
    /// Visual mode's answer to the arrow and page keys: there is no caret to
    /// move them, and no focused widget for them to reach, so they move the
    /// view directly. The scroll goes through an operation rather than a
    /// computed offset so [`document::Viewport`] re-anchors to where it lands
    /// — a target derived from `line_offsets` would be off by however much the
    /// unmeasured lines in between were mis-estimated.
    fn scroll_document(&self, step: ScrollStep) -> Task<Message> {
        use iced::widget::operation::{AbsoluteOffset, RelativeOffset, scroll_by, snap_to};

        let view_height = self
            .viewport
            .map_or(INITIAL_VIEWPORT_HEIGHT, |(_, height)| height);
        let line = self.metrics().line_height;

        let delta = match step {
            ScrollStep::LineUp => -line,
            ScrollStep::LineDown => line,
            // A page keeps one line of overlap, so there is something to
            // carry the eye across the jump.
            ScrollStep::PageUp => line - view_height,
            ScrollStep::PageDown => view_height - line,
            ScrollStep::Start => return snap_to(DOCUMENT_ID, RelativeOffset::START),
            ScrollStep::End => return snap_to(DOCUMENT_ID, RelativeOffset::END),
        };

        scroll_by(DOCUMENT_ID, AbsoluteOffset { x: 0.0, y: delta })
    }

    /// Top of `line` in content pixels, as measured or estimated.
    fn line_top(&self, line: usize) -> f32 {
        self.line_offsets
            .get(line)
            .or(self.line_offsets.last())
            .copied()
            .unwrap_or_default()
    }

    /// Rebuild the rendered blocks from the cached segments, reusing any
    /// previously parsed block whose source text is unchanged.
    ///
    /// Navigation and editing only change the blocks next to the cursor, so
    /// re-parsing the whole document on every keystroke is wasteful — and at
    /// keyboard auto-repeat speed it pegged the CPU and made the editor stall.
    /// Parsed blocks are cached by their source text and moved into the new
    /// layout when unchanged; only genuinely new text is parsed.
    /// Blocks that did not change are left **in place**: earlier versions
    /// rebuilt both vectors from scratch every time, which moved every parsed
    /// block in the document on every arrow key even though at most a couple of
    /// them differed. Only the changed middle is spliced.
    fn rebuild_blocks(&mut self) {
        // Taken out so the segment list can be walked while the block vectors
        // are mutated; put back at the end.
        let segments = std::mem::take(&mut self.segments);
        let ranges = || {
            segments.iter().filter_map(|segment| match segment {
                Segment::Block { start, end } => Some((*start, *end)),
                _ => None,
            })
        };

        let new_len = ranges().count();
        let old_len = self.block_sources.len();

        // Edits are local, so the new layout almost always shares a long run of
        // unchanged blocks with the old one at the front and/or the back.
        // Comparing by text (without allocating a joined string) finds those
        // runs so only the genuinely changed middle is re-parsed — turning a
        // full re-parse on navigation into O(changed) work, with no hashing and
        // no per-block allocation for the parts that stayed the same.
        let mut prefix = 0;
        for (index, range) in ranges().enumerate() {
            if index >= old_len || !block_source_eq(&self.lines, range, &self.block_sources[index])
            {
                break;
            }
            prefix = index + 1;
        }

        // The two runs may not overlap, so the suffix can only claim what the
        // prefix left over.
        let max_suffix = new_len.min(old_len) - prefix;
        let mut suffix = 0;
        for (offset, range) in ranges().rev().enumerate() {
            if offset >= max_suffix
                || !block_source_eq(
                    &self.lines,
                    range,
                    &self.block_sources[old_len - 1 - offset],
                )
            {
                break;
            }
            suffix = offset + 1;
        }

        // Parse only what actually changed.
        let (sources, blocks): (Vec<String>, Vec<markdown::Content>) = ranges()
            .skip(prefix)
            .take(new_len - suffix - prefix)
            .map(|(start, end)| {
                let source = self.lines[start..end].join("\n");
                let content = markdown::Content::parse(&source);
                (source, content)
            })
            .unzip();

        // Every newly parsed block is a new widget, under a new key; the ones
        // reused above keep theirs, and with it their laid-out text.
        let first_id = self.next_block_id;
        self.next_block_id += blocks.len() as u64;

        let replaced = prefix..old_len - suffix;
        self.block_ids
            .splice(replaced.clone(), first_id..self.next_block_id);
        self.block_sources.splice(replaced.clone(), sources);
        self.blocks.splice(replaced, blocks);

        self.segments = segments;
    }

    /// Make `line` part of the active (raw) region and place the cursor on it.
    ///
    /// The region expands to a whole table or fenced code block when `line` is
    /// inside one (see [`MarkdownEditor::region_bounds`]); otherwise it covers
    /// just that line. The cursor is placed at `column` (a byte offset, clamped
    /// to a char boundary) on `line`.
    ///
    /// When the range and text are unchanged (e.g. moving the cursor within a
    /// table with `j`/`k`), the text the editor holds is left alone and only
    /// the cursor is moved. Replacing the text restarts the editor's
    /// highlighting and shaping for the whole region; skipping it keeps table
    /// navigation at cursor-speed.
    fn activate_line(&mut self, line: usize, column: usize) {
        self.set_active(line, column, true);
    }

    /// Like [`MarkdownEditor::activate_line`], but always rebuilds the editor's
    /// content.
    ///
    /// Used after undo/redo, where the region can keep the exact same bounds
    /// while its text changes underneath — the one case where the content
    /// reuse check below would be wrong to trust.
    fn reactivate_line(&mut self, line: usize, column: usize) {
        self.set_active(line, column, false);
    }

    /// Widen the region over any code block it cuts through, keeping the
    /// caret and selection where they are.
    ///
    /// Only an edit can cause that — typing a fence that pairs up with one
    /// further down — and a region that splits a code block renders what is
    /// left of it as broken fragments.
    fn snap_region_to_fences(&mut self) {
        let start = self
            .region_bounds(self.active_start)
            .0
            .min(self.active_start);
        let end = self
            .region_bounds(self.active_end - 1)
            .1
            .max(self.active_end);

        if (start, end) == (self.active_start, self.active_end) {
            return;
        }

        let (anchor, head) = self.selection_span();
        self.reshape_region(start, end, anchor, head);
        // Widened for a code block, not for a selection: nothing to shrink
        // back from once the selection is gone.
        self.selection_region = anchor != head;
    }

    /// The line range edited as raw source when the cursor is on `line`.
    ///
    /// Some constructs cannot be edited a line at a time without falling
    /// apart, so the whole of one becomes the raw region:
    ///
    /// * a fenced code block — split at the cursor, its fences no longer
    ///   pair up and the code on either side renders as stray paragraphs and
    ///   empty code boxes, changing shape on every step through it;
    /// * a table — a row on its own is not a table, and the rest of it would
    ///   render as an invalid fragment.
    ///
    /// Any other line stands alone.
    fn region_bounds(&mut self, line: usize) -> (usize, usize) {
        self.refresh_fences();

        fences::block_at(&self.fences, line)
            .or_else(|| table_bounds(&self.lines, line))
            .unwrap_or((line, line + 1))
    }

    fn set_active(&mut self, line: usize, column: usize, may_reuse: bool) {
        // Placing the cursor somewhere ends any selection, and with it the
        // only reason the region might have been widened past the cursor line.
        self.selection_region = false;

        let (start, end) = self.region_bounds(line);

        // Moving somewhere else ends the current run of edits, so the next one
        // becomes its own undo step.
        if start != self.active_start || end != self.active_end {
            self.history.break_run();
        }

        // If the range hasn't changed, check whether the text is also the same.
        // `commit_active` keeps `lines` in sync with `active_content` after
        // edits, and pure motions never touch `lines`, so an unchanged range
        // with matching text means the content is already correct.
        //
        // The comparison is line by line: materializing both sides (the
        // editor's full text and a joined slice of the document) allocated two
        // copies of the region on *every* cursor move just to throw them away.
        let reuse_content = may_reuse
            && start == self.active_start
            && end == self.active_end
            && content_matches(&self.active_content, &self.lines[start..end]);

        self.active_start = start;
        self.active_end = end;

        let position = text_editor::Position {
            line: line - start,
            column: clamp_column(&self.lines[line], column),
        };

        if !reuse_content {
            self.replace_content(start..end);
        }
        // With the content reused, the cursor is still wherever it was.
        self.place_caret(position);
    }

    /// Make the raw editor hold the document lines in `range`.
    ///
    /// The text of the existing `Content` is replaced by an edit rather than
    /// the `Content` itself by a new one. A new one is shaped in full as soon
    /// as it is created, in the default font, and again at its first layout
    /// in the editor's font; an edited one keeps the font it was laid out
    /// with, and its new lines are shaped once. On a hundred-line code block
    /// that halves the cost of the caret entering it.
    fn replace_content(&mut self, range: Range<usize>) {
        let text = self.lines[range.clone()].join("\n");

        self.active_content.perform(text_editor::Action::SelectAll);
        self.active_content
            .perform(text_editor::Action::Edit(text_editor::Edit::Paste(
                Arc::new(text),
            )));

        debug_assert_eq!(self.active_content.line_count(), range.len());
    }

    /// Put the caret at `position` in the raw editor, with nothing selected.
    ///
    /// Where a wrapped line breaks, one offset is two places on screen: the
    /// end of one row and the start of the next. `Content::move_to` always
    /// picks the end of the row — so a caret put at the start of a row would
    /// show on the row above, and the next `↑` would skip a row. Stepping
    /// back onto the offset from the character after it picks the start
    /// instead, which is where a caret is put in every other case: nothing
    /// else lands exactly on a wrap point.
    fn place_caret(&mut self, position: text_editor::Position) {
        // `move_to` cannot *clear* a selection (`Cursor::selection: None`
        // means "leave it alone", not "drop it"), so an old one is collapsed
        // first: placing the caret must never leave one behind.
        self.clear_selection();

        // Where the character after the caret ends, if there is one.
        let next = self.active_content.line(position.line).and_then(|line| {
            let column = position.column.min(line.text.len());
            line.text[column..]
                .chars()
                .next()
                .map(|ch| column + ch.len_utf8())
        });

        match next {
            Some(next) if position.column > 0 => {
                self.active_content.move_to(text_editor::Cursor {
                    position: text_editor::Position {
                        column: next,
                        ..position
                    },
                    selection: None,
                });
                self.active_content
                    .perform(text_editor::Action::Move(text_editor::Motion::Left));
            }
            _ => self.active_content.move_to(text_editor::Cursor {
                position,
                selection: None,
            }),
        }
    }

    /// Whether any text is selected.
    ///
    /// Asked from `view` on every frame and from `update` on every message, so
    /// it deliberately goes through the cursor rather than through
    /// [`text_editor::Content::selection`] — the latter *materializes* the
    /// selected text, which after `Ctrl+A` means copying the entire document
    /// once per frame.
    ///
    /// A *degenerate* selection (both ends at the same position) is rejected:
    /// it reads back as an empty string rather than as "nothing selected", and
    /// `Ctrl+C` must never overwrite the clipboard with `""`.
    fn has_selection(&self) -> bool {
        let cursor = self.active_content.cursor();

        match cursor.selection {
            None => false,
            // The ordinary case is decided from the two ends alone, without
            // touching the text.
            Some(anchor) if anchor != cursor.position => true,
            // A word- or line-selection (a double or triple click) reports the
            // click position as *both* of its ends, so only the text can say
            // whether anything is actually selected. Neither spans more than a
            // line, so this stays cheap.
            Some(_) => self
                .active_content
                .selection()
                .is_some_and(|text| !text.is_empty()),
        }
    }

    /// Drop any selection, leaving the cursor at the end it started from.
    ///
    /// [`text_editor::Content::move_to`] can only ever *set* a selection, so
    /// clearing one goes through a motion instead: performing a `Move` with a
    /// selection present discards it and keeps one of its ends, without moving
    /// the cursor any further.
    ///
    /// Degenerate selections count here (unlike in
    /// [`MarkdownEditor::has_selection`]): leaving one behind would make the
    /// editor report an empty string as its selection.
    fn clear_selection(&mut self) {
        if self.active_content.cursor().selection.is_some() {
            self.active_content
                .perform(text_editor::Action::Move(text_editor::Motion::Left));
        }
    }

    /// The selection as `(anchor, cursor)` in `(document line, byte column)`
    /// pairs. With nothing selected both ends are the cursor.
    ///
    /// The anchor is the end that stays put while the selection is extended;
    /// the cursor is the end the keyboard and the mouse move.
    fn selection_span(&self) -> ((usize, usize), (usize, usize)) {
        let cursor = self.active_content.cursor();
        let head = (
            self.active_start + cursor.position.line,
            cursor.position.column,
        );

        let Some(anchor) = cursor.selection else {
            return (head, head);
        };
        let anchor = (self.active_start + anchor.line, anchor.column);

        if anchor != head {
            return (anchor, head);
        }

        // Both ends at once means a word- or line-selection: a double or
        // triple click stores the *click* position as its anchor and leaves
        // the real extent implicit. Neither can span more than one line, so
        // the range is recovered by locating the selected text on it — and,
        // as in every other editor, extending it afterwards keeps its start
        // and moves its end.
        match self.active_content.selection() {
            Some(text) if !text.is_empty() && head.0 < self.lines.len() => {
                let start = selection_start_column(&self.lines[head.0], head.1, &text);
                ((head.0, start), (head.0, start + text.len()))
            }
            _ => (head, head),
        }
    }

    /// Translate a document position into one inside the active editor,
    /// clamped to the region and to a character boundary.
    fn editor_position(&self, (line, column): (usize, usize)) -> text_editor::Position {
        let line = line.clamp(self.active_start, self.active_end - 1);

        text_editor::Position {
            line: line - self.active_start,
            column: clamp_column(&self.lines[line], column),
        }
    }

    /// Widen the raw region to cover at least `start..end` and select from
    /// `anchor` to `head` (both in document coordinates) inside it.
    ///
    /// This is what makes a selection a *document* selection: the
    /// `text_editor` can only draw, copy or replace text it actually holds, so
    /// every line the selection touches is pulled into the raw region. Nothing
    /// else has to understand selection — copy, cut, paste and delete all keep
    /// working exactly as they do on a single line.
    ///
    /// The range is snapped outwards to whole tables and code blocks, so a
    /// selection that ends halfway down one does not leave the rest of it
    /// rendering as an invalid fragment.
    fn reshape_region(
        &mut self,
        start: usize,
        end: usize,
        anchor: (usize, usize),
        head: (usize, usize),
    ) {
        let last = self.lines.len() - 1;
        let start = self.region_bounds(start.min(last)).0;
        let end = self.region_bounds(end.clamp(start + 1, last + 1) - 1).1;

        // Changing the selection is a deliberate move to somewhere else, so
        // the next edit starts its own undo step rather than being folded into
        // whatever was being typed before.
        self.history.break_run();

        if start != self.active_start || end != self.active_end {
            // Replacing the text restarts the highlighter, so it is only done
            // when the text the editor holds actually changes.
            if !content_matches(&self.active_content, &self.lines[start..end]) {
                self.replace_content(start..end);
            }

            self.active_start = start;
            self.active_end = end;
        }

        self.selection_region = true;

        let position = self.editor_position(head);
        let selection = self.editor_position(anchor);

        // Collapsing the two ends onto each other has to go through
        // `clear_selection`: a `Cursor` whose selection equals its position is
        // an *empty* selection, not the absence of one.
        self.clear_selection();
        self.active_content.move_to(text_editor::Cursor {
            position,
            selection: (selection != position).then_some(selection),
        });
    }

    /// Select the entire document (`Ctrl/Cmd+A`).
    ///
    /// The whole document becomes the raw region, which is what makes the
    /// selection visible and lets one `Ctrl+C`, `Ctrl+X` or keystroke act on
    /// all of it.
    fn select_all(&mut self) -> Task<Message> {
        let last = self.lines.len() - 1;
        self.reshape_region(0, self.lines.len(), (0, 0), (last, self.lines[last].len()));

        // The viewport deliberately stays where it is: no editor scrolls the
        // document away on "select all".
        Task::none()
    }

    /// Extend the selection by `motion`, widening the raw region far enough
    /// for the motion to land inside it.
    ///
    /// Only the motions that would run *past* the region's edge come here; a
    /// selection that stays within it is handled by the editor itself. The
    /// motion is still performed by the editor once the region has grown, so
    /// wrapped lines, word boundaries and the caret's preferred column all
    /// behave exactly as they do on a single line.
    fn extend_selection(&mut self, motion: text_editor::Motion) -> Task<Message> {
        use text_editor::Motion;

        let (anchor, head) = self.selection_span();

        // A page is a property of the *document's* viewport, not of the small
        // editor holding the raw region — whose own idea of a page is however
        // many lines it currently contains, and which has not even been laid
        // out at its new size yet. So the page motions are resolved here, in
        // document coordinates, and applied as a position.
        if let Some(target) = self.page_target(motion, head) {
            self.reshape_region(
                self.active_start.min(target.0),
                self.active_end.max(target.0 + 1),
                anchor,
                target,
            );
            return Task::none();
        }

        let (start, end) = match motion {
            Motion::DocumentStart => (0, self.active_end),
            Motion::DocumentEnd => (self.active_start, self.lines.len()),
            // One line is enough for the character, word and vertical motions:
            // none can cross more than a single line break at a time.
            Motion::Up | Motion::Left | Motion::WordLeft | Motion::Home => {
                (self.active_start.saturating_sub(1), self.active_end)
            }
            Motion::Down | Motion::Right | Motion::WordRight | Motion::End => (
                self.active_start,
                (self.active_end + 1).min(self.lines.len()),
            ),
            Motion::PageUp | Motion::PageDown => unreachable!("resolved above"),
        };

        self.reshape_region(start, end, anchor, head);
        // The editor performs the motion itself, so wrapped lines, word
        // boundaries and the caret's preferred column all behave exactly as
        // they do inside a single line. `update` then keeps the moving end of
        // the selection on screen.
        self.active_content
            .perform(text_editor::Action::Select(motion));

        Task::none()
    }

    /// Where a page motion lands, in document coordinates, or `None` when
    /// `motion` is not one.
    fn page_target(
        &self,
        motion: text_editor::Motion,
        head: (usize, usize),
    ) -> Option<(usize, usize)> {
        let line = match motion {
            text_editor::Motion::PageUp => head.0.saturating_sub(self.page_lines(head.0, false)),
            text_editor::Motion::PageDown => {
                (head.0 + self.page_lines(head.0, true)).min(self.lines.len() - 1)
            }
            _ => return None,
        };

        Some((line, head.1))
    }

    /// Extend a mouse drag-selection to the rendered line at `line`.
    fn drag_to(&mut self, line: usize) -> Task<Message> {
        let Some(drag) = self.drag else {
            return Task::none();
        };
        if line >= self.lines.len() {
            return Task::none();
        }

        // Rendered Markdown carries no mapping from a pixel back to a column —
        // the glyphs on screen are not the source characters — so a line the
        // drag has reached is taken whole: down to its end when the pointer is
        // below the anchor, back to its start when above.
        let head = if line >= drag.anchor.0 {
            (line, self.lines[line].len())
        } else {
            (line, 0)
        };

        self.reshape_region(
            drag.anchor.0.min(line),
            drag.anchor.0.max(line) + 1,
            drag.anchor,
            head,
        );

        // `update` keeps the moving end of the selection on screen, so a drag
        // towards the edge of the window carries on scrolling the document.
        Task::none()
    }

    /// Shrink a selection-widened region back onto the cursor's line once the
    /// selection is gone, reporting whether the region changed.
    ///
    /// Without this the region would only ever grow: one `Shift+Down` too many
    /// would leave half the document rendering as raw source for the rest of
    /// the session.
    fn collapse_selection_region(&mut self) -> bool {
        // The overwhelmingly common case — ordinary typing and navigation —
        // costs one boolean test.
        if !self.selection_region || self.drag.is_some() || self.has_selection() {
            return false;
        }

        let (line, column) = self.cursor_position();
        let line = line.min(self.lines.len() - 1);
        let (start, end) = self.region_bounds(line);

        self.selection_region = false;
        if start == self.active_start && end == self.active_end {
            return false;
        }

        self.set_active(line, column, false);
        true
    }

    /// Write the active editor's text back into `lines`.
    ///
    /// The editor may hold several lines (a table, or a multi-line paste), so
    /// every editor line is written over the current range, updating
    /// `active_end` to match.
    ///
    /// The common case — a keystroke that leaves the line count alone — writes
    /// the lines in place, reusing their existing allocations. Only a change in
    /// line count needs to move the tail of the document.
    fn commit_active(&mut self) {
        let start = self.active_start;
        let old_len = self.active_end - self.active_start;
        let new_len = self.active_content.line_count();

        self.record_change(start, old_len, new_len, self.cursor_position());

        if new_len == old_len {
            // Typing only ever rewrites the region, so the segments and
            // blocks around it stay exactly as they were — unless the edit
            // made or unmade a code fence, which changes how everything after
            // it splits into blocks.
            let fences_before = fences::signature(&self.lines[start..self.active_end]);

            for offset in 0..new_len {
                if let Some(line) = self.active_content.line(offset) {
                    let target = &mut self.lines[start + offset];
                    target.clear();
                    target.push_str(&line.text);
                }
            }

            if self.is_editing() {
                self.revision += 1;
                if fences::signature(&self.lines[start..self.active_end]) != fences_before {
                    self.fences_dirty = true;
                    self.structure_changed = true;
                }
            } else {
                // Visual mode renders these lines from a *block*, not from
                // `active_content`, so the block holding them has to be parsed
                // again. The toolbar and the key bindings keep edits out of
                // visual mode, but nothing about an edit makes that true.
                self.touch_lines(start..self.active_end, &fences_before);
            }
        } else {
            let replacement: Vec<String> = (0..new_len)
                .map(|offset| {
                    self.active_content
                        .line(offset)
                        .map(|line| line.text.into_owned())
                        .unwrap_or_default()
                })
                .collect();

            // Only the segments *after* the region move, by the number of
            // lines gained or lost; `sync` slides them along.
            self.splice_lines(start..self.active_end, replacement);
            self.active_end = start + new_len;
        }
    }

    /// Record a line-range replacement of `start..start + before_len` with
    /// `after_len` lines in the undo history.
    ///
    /// Must be called *before* the lines are mutated: the replaced lines are
    /// read from the current document, and only when the edit cannot be folded
    /// into the run already in progress.
    fn record_change(
        &mut self,
        start: usize,
        before_len: usize,
        after_len: usize,
        cursor: (usize, usize),
    ) {
        let Self { history, lines, .. } = self;
        history.record(start, before_len, after_len, || {
            (lines[start..start + before_len].to_vec(), cursor)
        });
    }

    /// Apply an undo/redo [`Change`](history::Change), returning its inverse.
    fn apply_change(&mut self, change: history::Change) -> history::Change {
        let start = change.start.min(self.lines.len());
        let end = (start + change.after_len).min(self.lines.len());

        let inverse = history::Change {
            start,
            before: self.lines[start..end].to_vec(),
            after_len: change.before.len(),
            cursor: self.cursor_position(),
        };

        self.splice_lines(start..end, change.before);
        // `lines` must never be empty; a change always restores at least one.
        debug_assert!(!self.lines.is_empty());

        let (line, column) = change.cursor;
        let line = line.min(self.lines.len().saturating_sub(1));
        self.reactivate_line(line, column);

        inverse
    }

    /// Recompute the search matches against the current document.
    ///
    /// Matches are byte ranges into `lines`, so every edit invalidates them.
    /// This is O(document), which is why it only ever runs while the bar is
    /// open — with it closed, editing pays nothing.
    fn refresh_search(&mut self) {
        let Self { search, lines, .. } = self;
        if let Some(search) = search {
            search.refresh(lines);
        }
    }

    /// Move the highlight one match forward or backward and reveal it.
    fn step_match(&mut self, forward: bool) -> Task<Message> {
        match &mut self.search {
            Some(search) => {
                search.step(forward);
                self.reveal_current_match()
            }
            None => Task::none(),
        }
    }

    /// Make the highlighted match's line active, select the match, and bring
    /// it into view with the lines above it for context.
    ///
    /// Nothing moves when nothing matched: typing a query that finds nothing
    /// must not pull the view back to the caret.
    fn reveal_current_match(&mut self) -> Task<Message> {
        let Some(hit) = self.search.as_ref().and_then(Search::current_match) else {
            return Task::none();
        };
        if hit.line >= self.lines.len() {
            return Task::none();
        }

        // The match's line becomes the raw editor, so the text being replaced
        // is shown as source rather than rendered.
        self.reactivate_line(hit.line, hit.start);
        self.select_in_active(hit);
        self.request_reveal(RevealMode::Center);
        Task::none()
    }

    /// Select `hit` inside the active editor.
    ///
    /// The region can span several lines (a table), so the match's document
    /// line is translated into an offset within the editor's own content.
    fn select_in_active(&mut self, hit: search::Match) {
        let Some(editor_line) = hit.line.checked_sub(self.active_start) else {
            return;
        };
        if hit.line >= self.active_end {
            return;
        }

        self.active_content.move_to(text_editor::Cursor {
            position: text_editor::Position {
                line: editor_line,
                column: hit.end,
            },
            selection: Some(text_editor::Position {
                line: editor_line,
                column: hit.start,
            }),
        });
    }

    /// Replace the highlighted match with the replacement text.
    fn replace_current(&mut self) -> Task<Message> {
        let Some(search) = &self.search else {
            return Task::none();
        };
        let Some(hit) = search.current_match() else {
            return Task::none();
        };
        if hit.line >= self.lines.len() || hit.end > self.lines[hit.line].len() {
            return Task::none();
        }
        let replacement = search.replacement.clone();

        // A replacement is its own undo step, never folded into typing.
        self.history.break_run();
        self.record_change(hit.line, 1, 1, (hit.line, hit.start));
        let fences_before = fences::signature(&self.lines[hit.line..=hit.line]);
        self.lines[hit.line].replace_range(hit.start..hit.end, &replacement);
        self.touch_lines(hit.line..hit.line + 1, &fences_before);
        self.is_dirty = true;
        self.history.break_run();
        self.refresh_active_content();

        // The line changed, so every match after it on that line moved.
        self.refresh_search();
        if let Some(search) = &mut self.search {
            search.anchor_to(hit.line);
        }
        self.reveal_current_match()
    }

    /// Replace every match in the document.
    fn replace_all(&mut self) -> Task<Message> {
        let Some(search) = &self.search else {
            return Task::none();
        };
        if search.count() == 0 {
            return Task::none();
        }
        let replacement = search.replacement.clone();

        // The edit spans from the first matched line to the last, so it is
        // recorded as one undo entry covering exactly that range — not a
        // snapshot of the whole document.
        let first = search.matches()[0].line;
        let last = search.matches()[search.count() - 1].line;
        let hits: Vec<search::Match> = search.matches().to_vec();

        self.history.break_run();
        self.record_change(first, last - first + 1, last - first + 1, (first, 0));
        let fences_before = fences::signature(&self.lines[first..=last]);

        // Applied back to front so each replacement's byte offsets are still
        // valid when it is made.
        for hit in hits.iter().rev() {
            if hit.line < self.lines.len() && hit.end <= self.lines[hit.line].len() {
                self.lines[hit.line].replace_range(hit.start..hit.end, &replacement);
            }
        }

        // Replacing can blank a line out, which changes the segmentation.
        self.touch_lines(first..last + 1, &fences_before);
        self.is_dirty = true;
        self.history.break_run();
        self.refresh_active_content();

        self.refresh_search();
        self.reveal_current_match()
    }

    /// Make the raw editor hold the region's lines again after they were
    /// changed from outside it — by a replacement — with the caret as close
    /// to where it was as the new text allows.
    ///
    /// The editor writes every edit back line for line, so if it went on
    /// showing the old text, the next key press would put the old text back.
    fn refresh_active_content(&mut self) {
        let region = &self.lines[self.active_start..self.active_end];
        if content_matches(&self.active_content, region) {
            return;
        }

        let (line, column) = self.cursor_position();
        self.reactivate_line(line.min(self.lines.len() - 1), column);
    }

    /// The cursor as a `(document line, byte column)` pair.
    fn cursor_position(&self) -> (usize, usize) {
        let cursor = self.active_content.cursor().position;
        (self.active_start + cursor.line, cursor.column)
    }

    /// Move keyboard focus to the active line's editor.
    fn focus_active(&self) -> Task<Message> {
        if !self.is_editing() {
            // Visual mode has no editor in the tree to focus.
            return Task::none();
        }

        iced::widget::operation::focus(ACTIVE_LINE_ID)
    }
}

/// `Esc` and `F3`, handled outside the focused widget.
///
/// `text_input` swallows `Esc` (to unfocus itself) and fires `on_submit` for
/// `Enter` regardless of modifiers, so neither can be bound from the widget
/// side; `F3` is left alone by every widget involved.
fn panel_keys() -> Subscription<Message> {
    // `listen_with` takes a plain `fn`, so this closure must not capture: which
    // panel `Esc` closes is decided by [`Message::Dismiss`] in `handle`, not
    // here. `F3` is safe to emit unconditionally — stepping matches with no
    // search open is a no-op.
    iced::event::listen_with(|event, _status, _window| {
        use keyboard::key::Named;

        let iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event
        else {
            return None;
        };

        match key.as_ref() {
            // Deliberately handled even when a widget consumed the event:
            // while a panel is open, `Esc` can only mean "close it".
            keyboard::Key::Named(Named::Escape) => Some(Message::Dismiss),
            keyboard::Key::Named(Named::F3) if modifiers.shift() => Some(Message::FindPrevious),
            keyboard::Key::Named(Named::F3) => Some(Message::FindNext),
            _ => None,
        }
    })
}

/// The keys visual mode answers to, where no widget holds the keyboard.
fn visual_keys() -> Subscription<Message> {
    iced::event::listen_with(|event, status, _window| {
        let iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event
        else {
            return None;
        };

        // A panel may well be open over the document, with one of its own
        // controls focused; keys it consumed are its own.
        (status == iced::event::Status::Ignored)
            .then(|| visual_key(key.as_ref(), modifiers))
            .flatten()
    })
}

/// What a key press means in visual mode.
///
/// Only `Ctrl+E` — the way back to editing — and the keys that move the view,
/// since there is no caret for them to move instead. Split out from
/// [`visual_keys`] so the mapping is testable: `listen_with` takes a plain
/// `fn` and a `Subscription` cannot be run from a test.
fn visual_key(key: keyboard::Key<&str>, modifiers: keyboard::Modifiers) -> Option<Message> {
    use keyboard::key::Named;

    let step = if modifiers.command() {
        match key {
            keyboard::Key::Character("e") => return Some(Message::ToggleMode),
            keyboard::Key::Named(Named::Home | Named::ArrowUp) => ScrollStep::Start,
            keyboard::Key::Named(Named::End | Named::ArrowDown) => ScrollStep::End,
            _ => return None,
        }
    } else {
        match key {
            keyboard::Key::Named(Named::ArrowUp) => ScrollStep::LineUp,
            keyboard::Key::Named(Named::ArrowDown) => ScrollStep::LineDown,
            keyboard::Key::Named(Named::PageUp) => ScrollStep::PageUp,
            // `Space` pages down the way every reader does.
            keyboard::Key::Named(Named::PageDown | Named::Space) => ScrollStep::PageDown,
            keyboard::Key::Named(Named::Home) => ScrollStep::Start,
            keyboard::Key::Named(Named::End) => ScrollStep::End,
            _ => return None,
        }
    };

    Some(Message::ScrollDocument(step))
}

/// The left mouse button being released, which ends a selection drag.
fn mouse_release() -> Subscription<Message> {
    iced::event::listen_with(|event, _status, _window| match event {
        iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::DragEnd)
        }
        _ => None,
    })
}

/// Whether the editor's content holds exactly `lines`, one editor line per
/// entry.
///
/// Compared line by line so neither side has to be materialized: the previous
/// `content.text() == lines.join("\n")` allocated two full copies of the region
/// on every cursor move only to drop them again.
fn content_matches(content: &text_editor::Content, lines: &[String]) -> bool {
    if content.line_count() != lines.len() {
        return false;
    }

    lines
        .iter()
        .enumerate()
        .all(|(index, line)| content.line(index).is_some_and(|held| held.text == *line))
}

/// Walk the document, grouping it into renderable [`Segment`]s in `out`.
///
/// The active range and blank lines are their own segments; every other
/// contiguous run of lines becomes a single [`Segment::Block`] so multi-line
/// Markdown constructs render together. A blank line inside a fenced code
/// block is part of the code, not a break between blocks: splitting there
/// rendered one code block as two broken halves.
///
/// `active` is `None` in visual mode, where no line is raw: the document then
/// splits into blocks and blanks alone, and no [`Segment::Active`] is produced.
///
/// `out` is cleared and refilled so its allocation is reused across rebuilds.
fn collect_segments(
    lines: &[String],
    code_blocks: &[(usize, usize)],
    active: Option<(usize, usize)>,
    out: &mut Vec<Segment>,
) {
    out.clear();
    walk_segments(lines, code_blocks, active, 0..lines.len(), out);
}

/// Append the [`Segment`]s of `lines[range]` to `out`, as a walk of the whole
/// document would produce them — provided `range` starts and ends at a
/// segment boundary.
fn walk_segments(
    lines: &[String],
    code_blocks: &[(usize, usize)],
    active: Option<(usize, usize)>,
    range: Range<usize>,
    out: &mut Vec<Segment>,
) {
    let continues_block =
        |index: usize| !lines[index].trim().is_empty() || fences::in_code(code_blocks, index);
    let stop = range.end.min(lines.len());
    let mut index = range.start;
    // In visual mode there is no raw region, so nothing interrupts a block.
    let breaks_block = |index: usize| active.is_some_and(|(start, _)| index == start);

    while index < stop {
        if let Some((active_start, active_end)) = active
            && index == active_start
        {
            out.push(Segment::Active);
            // `active_start < active_end` always holds, but force forward
            // progress regardless: a zero-width active range would otherwise
            // spin forever and freeze the window (requiring a hard kill).
            index = active_end.max(index + 1);
        } else if !continues_block(index) {
            out.push(Segment::Blank(index));
            index += 1;
        } else {
            let start = index;
            while index < stop && !breaks_block(index) && continues_block(index) {
                index += 1;
            }
            out.push(Segment::Block { start, end: index });
        }
    }
}

/// Whether the block spanning `lines[range]` has exactly `source` as its text,
/// where `source` is a previously joined `lines[..].join("\n")`.
///
/// Compares line by line against the `\n`-separated `source` without allocating
/// a joined string, so [`MarkdownEditor::rebuild_blocks`] can detect unchanged
/// blocks cheaply. Lines never contain `\n`, so splitting `source` on `\n`
/// recovers the exact original lines.
fn block_source_eq(lines: &[String], range: (usize, usize), source: &str) -> bool {
    let mut parts = source.split('\n');
    for line in &lines[range.0..range.1] {
        match parts.next() {
            Some(part) if part == line => {}
            _ => return false,
        }
    }
    parts.next().is_none()
}

/// Lines `first..last` to render as real widgets, or `None` when the whole
/// document should be rendered (no viewport yet, or the window already covers
/// every line). Segments outside it are stood in for by spacers.
///
/// The window spans the visible screen plus [`OVERSCAN_ABOVE`] and
/// [`OVERSCAN_BELOW`] screens, mapped to lines through `line_offsets` — the
/// *same* geometry the spacers are sized with, so what the window expects to
/// be on screen is what the layout puts there.
fn render_window(
    viewport: Option<(f32, f32)>,
    line_offsets: &[f32],
    total_lines: usize,
) -> Option<(usize, usize)> {
    let (offset_y, view_height) = viewport?;

    let top = offset_y - view_height * OVERSCAN_ABOVE;
    let bottom = offset_y + view_height * (1.0 + OVERSCAN_BELOW);

    let first = if top <= 0.0 {
        0
    } else {
        line_at_offset(line_offsets, top)
    };
    let last = (line_at_offset(line_offsets, bottom) + 1).min(total_lines);

    // The window already spans the whole document: render everything and skip
    // the spacers (keeps small documents on the plain code path).
    (first > 0 || last < total_lines).then_some((first, last))
}

/// Index of the line occupying content pixel `y`, per the cumulative
/// `line_offsets` table.
fn line_at_offset(line_offsets: &[f32], y: f32) -> usize {
    // `line_offsets[i]` is the *top* of line `i`, so the line containing `y` is
    // the last one whose top is at or above it.
    line_offsets
        .partition_point(|&top| top <= y)
        .saturating_sub(1)
}

/// Size of rendered code relative to body text, as iced's Markdown settings
/// derive it.
const CODE_SCALE: f32 = 0.75;

/// Height of a line of rendered code relative to an estimated body line: set
/// at [`CODE_SCALE`], and without the padding every segment has of its own.
const CODE_ROW: f32 = 0.52;

/// Layout estimates for one combination of font size and text width.
///
/// [`EST_LINE_HEIGHT`] and [`EST_CHARS_PER_LINE`] are calibrated for
/// [`settings::BASE_FONT_SIZE`]; both scale with the configured size, in
/// opposite directions. Bundling them keeps the render window
/// ([`render_window`]) and the `line_offsets` table derived from the same
/// numbers — deriving them separately is what desynchronises the viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Metrics {
    /// Estimated height (px) of one visual line.
    line_height: f32,
    /// Estimated characters that fit on one visual line.
    chars_per_line: u32,
    /// Font scale relative to [`settings::BASE_FONT_SIZE`].
    scale: f32,
}

impl Metrics {
    fn new(font_size: FontSize, wrap_width: WrapWidth) -> Self {
        // `FontSize::ALL` has no zero, but a hand-written value could in
        // principle reach here; guard the division either way.
        let scale = (font_size.pixels() / f32::from(settings::BASE_FONT_SIZE)).max(0.1);

        // Two independent caps on the measure: the assumed window width, and
        // the configured text width if narrower. The tighter one wins.
        let by_window = EST_CHARS_PER_LINE as f32 / scale;
        let by_wrap = wrap_width
            .pixels()
            .map_or(f32::INFINITY, |width| width / (EST_CHAR_WIDTH * scale));

        Self {
            line_height: EST_LINE_HEIGHT * scale,
            chars_per_line: by_window.min(by_wrap).max(1.0) as u32,
            scale,
        }
    }

    /// Wrap by the document's real `width`, once it has been laid out,
    /// rather than by an assumed window width.
    fn measured(self, width: Option<f32>) -> Self {
        let Some(width) = width else {
            return self;
        };

        let text = width - 2.0 * f32::from(LINE_PADDING);
        let fits = (text / (EST_CHAR_WIDTH * self.scale)).max(1.0) as u32;

        Self {
            chars_per_line: fits,
            ..self
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new(FontSize::DEFAULT, WrapWidth::FULL)
    }
}

/// Estimated rendered height (px) of a single source line, `in_code` when it
/// sits between the fences of a code block.
///
/// A line is at least one visual line, plus extra for wrapping based on byte
/// length (no UTF-8 decode, so multi-byte text only ever over-estimates).
/// Headings are scaled up by their level. Code is set smaller and never wraps
/// (it scrolls sideways); a fence line renders as the code box's padding,
/// which is about as tall as a line of code.
fn estimate_line_height(line: &str, in_code: bool, metrics: Metrics) -> f32 {
    if in_code || fences::is_fence(line) {
        return metrics.line_height * CODE_ROW;
    }

    let (prefix, scale) = rendered_prefix(line);
    let wrapped = ((line.len() - prefix) as f32 * scale / metrics.chars_per_line as f32)
        .ceil()
        .max(1.0);

    wrapped * scale * metrics.line_height
}

/// How a line's rendering differs from its source at its start: the bytes
/// of Markdown syntax that do not show (a heading's `#`s), and the size the
/// rest is rendered at relative to body text.
fn rendered_prefix(line: &str) -> (usize, f32) {
    let hashes = line.bytes().take_while(|&byte| byte == b'#').count();
    let is_heading = (1..=6).contains(&hashes)
        && line[hashes..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace);

    if !is_heading {
        return (0, 1.0);
    }

    // The sizes `markdown::Settings::with_text_size` gives each level.
    let scale = match hashes {
        1 => 2.0,
        2 => 1.75,
        3 => 1.5,
        4 => 1.25,
        _ => 1.0,
    };
    let prefix = hashes + (line.len() - hashes - line[hashes..].trim_start().len());

    (prefix, scale)
}

/// Byte offset of the character `chars` characters into `text`, or its end.
fn char_offset(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(offset, _)| offset)
}

/// If `lines[line]` belongs to a GFM table, return the table's line range
/// `start..end` (header through last body row). Otherwise return `None`.
///
/// A table is a header row, a delimiter row (e.g. `|---|:--:|`) directly below
/// it, and zero or more body rows, all within a single blank-line-delimited run.
fn table_bounds(lines: &[String], line: usize) -> Option<(usize, usize)> {
    if lines[line].trim().is_empty() {
        return None;
    }

    // The non-blank run that contains `line`.
    let mut run_start = line;
    while run_start > 0 && !lines[run_start - 1].trim().is_empty() {
        run_start -= 1;
    }
    let mut run_end = line + 1;
    while run_end < lines.len() && !lines[run_end].trim().is_empty() {
        run_end += 1;
    }

    // A table is anchored by a delimiter row whose preceding line is its header.
    for delimiter in (run_start + 1)..run_end {
        if is_delimiter_row(&lines[delimiter]) && is_table_row(&lines[delimiter - 1]) {
            let header = delimiter - 1;
            let mut end = delimiter + 1;
            while end < run_end && is_table_row(&lines[end]) {
                end += 1;
            }
            if (header..end).contains(&line) {
                return Some((header, end));
            }
        }
    }

    None
}

/// Whether `line` is a GFM table delimiter row, e.g. `|---|---|` or `:--:|--`.
///
/// Requires at least one pipe so a bare `---` (a thematic break) is not mistaken
/// for a single-column delimiter.
fn is_delimiter_row(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let mut has_pipe = false;
    let mut has_dash = false;
    for ch in trimmed.chars() {
        match ch {
            '|' => has_pipe = true,
            '-' => has_dash = true,
            ':' | ' ' | '\t' => {}
            _ => return false,
        }
    }

    has_pipe && has_dash
}

/// Whether `line` could be a table row (a non-blank line containing a pipe).
fn is_table_row(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.contains('|')
}

/// Split text into logical lines. Always returns at least one element.
///
/// Lines break exactly where the raw editor's text engine breaks them — at
/// `\n`, `\r\n`, `\n\r` and a lone `\r` — so the lines of the active region
/// and the lines the editor holds always correspond one to one. Splitting at
/// `\n` alone left a `\r` at the end of every line of a Windows file, which
/// the editor took for a line break of its own: every edit then wrote an extra
/// empty line back into the document.
///
/// A final line ending leaves a final empty line, as it does in the editor.
fn split_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut rest = text;

    while let Some(at) = rest.find(['\r', '\n']) {
        lines.push(rest[..at].to_owned());

        let ending = &rest[at..];
        let length = if ending.starts_with("\r\n") || ending.starts_with("\n\r") {
            2
        } else {
            1
        };
        rest = &ending[length..];
    }

    lines.push(rest.to_owned());
    lines
}

/// The line ending `text` uses, judged by its first line break: `\r\n` for a
/// Windows file, `\n` for everything else.
fn line_ending_of(text: &str) -> &'static str {
    match text.find('\n') {
        Some(at) if text[..at].ends_with('\r') => "\r\n",
        _ => "\n",
    }
}

/// Column at which a word- or line-selection of `text` begins on `line`.
///
/// Such a selection only records the position that was clicked, which sits
/// *somewhere inside* it — so the start is found by looking for the occurrence
/// of the selected text that covers that position. At most `text.len()`
/// candidates are tried, and only the ones that are character boundaries.
fn selection_start_column(line: &str, column: usize, text: &str) -> usize {
    let column = column.min(line.len());

    (column.saturating_sub(text.len())..=column)
        .find(|&start| line.is_char_boundary(start) && line[start..].starts_with(text))
        .unwrap_or(column)
}

/// Clamp a byte `column` to the length of `line`, snapping to the nearest
/// char boundary at or below it.
fn clamp_column(line: &str, column: usize) -> usize {
    let mut column = column.min(line.len());
    while column > 0 && !line.is_char_boundary(column) {
        column -= 1;
    }
    column
}

/// Key bindings for the active region's editor.
///
/// The document-aware editing layer: the `text_editor` widget only knows about
/// the one line (or table) it holds, so the keys that would run off its edges
/// are turned into document-level messages instead.
/// - **Enter** splits a standalone line (adds a table row inside a table).
/// - **Backspace** at column 0 merges into the previous line.
/// - **Delete** at end of line merges the next line up.
/// - **Ctrl/Opt+Backspace / Ctrl/Opt+Delete** delete the word beside the cursor.
/// - **↑/↓** move between lines from the region's top/bottom edge.
/// - **Shift+↑/↓/←/→**, **Shift+PgUp/PgDn** and **Ctrl/Cmd+Shift+Home/End/↑/↓**
///   extend the selection past the region, growing it to match.
/// - **PgUp/PgDn** move the active line by a page; **Ctrl+Home/End** (and
///   **Ctrl+PgUp/PgDn**) jump to the document start/end.
/// - **Ctrl+A** selects the whole document.
/// - **Ctrl+N/O/S** file actions, **Ctrl+Z / Ctrl+Y** undo/redo,
///   **Ctrl+B / Ctrl+I** bold/italic.
///
/// Everything else falls through to the default editor bindings, which operate
/// *within* the active region — and since the region always covers the whole
/// selection, that is enough for Home/End, word motion, typing, and
/// `Ctrl+C`/`X`/`V` to behave exactly as they would in a plain text editor.
fn editor_binding(
    key_press: text_editor::KeyPress,
    ctx: BindingContext,
) -> Option<text_editor::Binding<Message>> {
    use keyboard::key::Named;
    use text_editor::{Binding, Motion};

    let key = key_press.key.clone();
    let modifiers = key_press.modifiers;
    let command = modifiers.command();
    let shift = modifiers.shift();
    // `Ctrl` on Windows/Linux, `Option` on macOS: the "by word" modifier.
    let by_word = modifiers.jump();
    // On macOS `Cmd+←/→` means start/end of line, not a word motion, so those
    // combinations never leave the region and must not be intercepted here.
    let line_motion = modifiers.macos_command();

    let extend = |motion| Some(Binding::Custom(Message::ExtendSelection(motion)));

    // Application shortcuts. Matched on the *unmodified* key so they keep
    // working on keyboard layouts where a modifier changes the produced
    // character.
    if command && let keyboard::Key::Character(c) = key.as_ref() {
        let message = match c {
            "n" => Some(Message::NewFile),
            "o" => Some(Message::OpenFile),
            "s" => Some(Message::SaveFile),
            "b" => Some(Message::Format(FormatAction::Bold)),
            "i" => Some(Message::Format(FormatAction::Italic)),
            "z" if shift => Some(Message::Redo),
            "z" => Some(Message::Undo),
            "y" => Some(Message::Redo),
            "f" => Some(Message::OpenSearch { replace: false }),
            "h" => Some(Message::OpenSearch { replace: true }),
            // Select the *document*, not just the raw region the editor holds.
            "a" => Some(Message::SelectAll),
            // The only way back once the toolbar is hidden.
            "," => Some(Message::ToggleSettingsPanel),
            // Leaving for visual mode; `visual_keys` brings us back.
            "e" => Some(Message::ToggleMode),
            _ => None,
        };

        if let Some(message) = message {
            return Some(Binding::Custom(message));
        }

        // Copy and cut with nothing selected must leave both the clipboard and
        // the document alone. The default binding would hand the widget an
        // empty selection to write, wiping whatever was on the clipboard.
        if matches!(c, "c" | "x") && !ctx.has_selection {
            return Some(Binding::Sequence(Vec::new()));
        }
    }

    match key.as_ref() {
        // -- Selection that runs past the raw region ---------------------
        //
        // Only the motions that would leave the region are intercepted; the
        // rest are the editor's own and already do the right thing, because
        // the region always spans the entire selection.
        //
        // `Ctrl/Cmd+Shift` with a vertical key or Home/End selects all the way
        // to the document's edge, matching the platform convention on macOS
        // and the `Ctrl+Shift+Home/End` every other desktop shares.
        keyboard::Key::Named(Named::Home | Named::ArrowUp) if shift && command => {
            extend(Motion::DocumentStart)
        }
        keyboard::Key::Named(Named::End | Named::ArrowDown) if shift && command => {
            extend(Motion::DocumentEnd)
        }
        keyboard::Key::Named(Named::ArrowUp) if shift && ctx.at_editor_top && !ctx.at_doc_first => {
            extend(Motion::Up)
        }
        keyboard::Key::Named(Named::ArrowDown)
            if shift && ctx.at_editor_bottom && !ctx.at_doc_last =>
        {
            extend(Motion::Down)
        }
        keyboard::Key::Named(Named::ArrowLeft)
            if shift
                && !line_motion
                && ctx.at_editor_top
                && ctx.at_line_start
                && !ctx.at_doc_first =>
        {
            extend(if by_word {
                Motion::WordLeft
            } else {
                Motion::Left
            })
        }
        keyboard::Key::Named(Named::ArrowRight)
            if shift
                && !line_motion
                && ctx.at_editor_bottom
                && ctx.at_line_end
                && !ctx.at_doc_last =>
        {
            extend(if by_word {
                Motion::WordRight
            } else {
                Motion::Right
            })
        }
        keyboard::Key::Named(Named::PageUp) if shift => extend(Motion::PageUp),
        keyboard::Key::Named(Named::PageDown) if shift => extend(Motion::PageDown),

        // -- Word deletion ----------------------------------------------
        //
        // Not something the widget offers: its `Backspace` is always one
        // character. Selecting the word first and deleting the selection is
        // exactly how every editor implements it, and makes it a single,
        // reversible edit. With text already selected the word motion would
        // throw that selection away, so the plain deletion is used instead.
        keyboard::Key::Named(Named::Backspace) if by_word && !ctx.has_selection => {
            if ctx.at_editor_top && ctx.at_line_start {
                // No word to the left inside the region: the line break before
                // it is what gets deleted, joining the two lines.
                return (!ctx.at_doc_first).then_some(Binding::Custom(Message::MergeWithPrevious));
            }
            Some(Binding::Sequence(vec![
                Binding::Select(Motion::WordLeft),
                Binding::Backspace,
            ]))
        }
        keyboard::Key::Named(Named::Delete) if by_word && !ctx.has_selection => {
            if ctx.at_editor_bottom && ctx.at_line_end {
                return (!ctx.at_doc_last).then_some(Binding::Custom(Message::MergeWithNext));
            }
            Some(Binding::Sequence(vec![
                Binding::Select(Motion::WordRight),
                Binding::Delete,
            ]))
        }

        // -- Editing at the region's edges -------------------------------
        //
        // Each of these only applies with *nothing* selected: a selection is
        // what the key acts on, and the editor already holds all of it.
        //
        // Enter on a standalone line splits it into a new document line; inside
        // a table it falls through to insert a newline (i.e. add a row).
        keyboard::Key::Named(Named::Enter) if !ctx.multiline && !ctx.has_selection => {
            Some(Binding::Custom(Message::SplitLine))
        }
        // Backspace at the very start of the region merges into the line above;
        // elsewhere it edits within the editor (including joining table rows).
        keyboard::Key::Named(Named::Backspace)
            if ctx.at_editor_top
                && ctx.at_line_start
                && !ctx.at_doc_first
                && !ctx.has_selection =>
        {
            Some(Binding::Custom(Message::MergeWithPrevious))
        }
        // Delete at the very end of the region pulls the next line up; elsewhere
        // it deletes forward within the editor.
        keyboard::Key::Named(Named::Delete)
            if ctx.at_editor_bottom
                && ctx.at_line_end
                && !ctx.at_doc_last
                && !ctx.has_selection =>
        {
            Some(Binding::Custom(Message::MergeWithNext))
        }

        // -- Navigation --------------------------------------------------
        //
        // Arrows only leave the region from its top/bottom edge; within a
        // multi-line region they move between rows by default.
        keyboard::Key::Named(Named::ArrowUp) if ctx.at_editor_top && !ctx.at_doc_first => {
            Some(Binding::Custom(Message::MoveUp))
        }
        keyboard::Key::Named(Named::ArrowDown) if ctx.at_editor_bottom && !ctx.at_doc_last => {
            Some(Binding::Custom(Message::MoveDown))
        }
        // Page navigation moves the active line through the document (a tiny
        // single-line editor has nowhere to page within). `Ctrl` jumps to the
        // very start/end. Handled regardless of `multiline` so you can also page
        // straight out of a table.
        keyboard::Key::Named(Named::PageUp) => Some(Binding::Custom(if command {
            Message::JumpToStart
        } else {
            Message::PageUp
        })),
        keyboard::Key::Named(Named::PageDown) => Some(Binding::Custom(if command {
            Message::JumpToEnd
        } else {
            Message::PageDown
        })),
        // Ctrl+Home / Ctrl+End jump to the document start/end. Plain Home/End
        // fall through to the default binding (start/end of the current line).
        keyboard::Key::Named(Named::Home) if command => Some(Binding::Custom(Message::JumpToStart)),
        keyboard::Key::Named(Named::End) if command => Some(Binding::Custom(Message::JumpToEnd)),
        _ => Binding::from_key_press(key_press),
    }
}

#[cfg(test)]
mod headless;

#[cfg(test)]
mod tests {
    use super::*;

    fn to_lines(text: &str) -> Vec<String> {
        split_lines(text)
    }

    /// Convenience wrapper: the production walker fills a caller-owned buffer
    /// so it can reuse the allocation, and takes the fences precomputed, which
    /// is noise in a test.
    fn segments(lines: &[String], active_start: usize, active_end: usize) -> Vec<Segment> {
        let mut out = Vec::new();
        collect_segments(
            lines,
            &fences::blocks(lines),
            Some((active_start, active_end)),
            &mut out,
        );
        out
    }

    /// The segments of a document with no raw region at all: visual mode.
    fn rendered_segments(lines: &[String]) -> Vec<Segment> {
        let mut out = Vec::new();
        collect_segments(lines, &fences::blocks(lines), None, &mut out);
        out
    }

    /// A from-scratch walk of `editor`'s document, in whichever mode it is in
    /// — what its incrementally maintained segments must always equal.
    fn walked_segments(editor: &MarkdownEditor) -> Vec<Segment> {
        let mut out = Vec::new();
        collect_segments(
            &editor.lines,
            &fences::blocks(&editor.lines),
            editor.active_region(),
            &mut out,
        );
        out
    }

    /// A press at the very start of rendered `line`, beside its text.
    fn click(line: usize) -> Message {
        Message::ClickAt(Press {
            segment: (line, line + 1),
            line,
            x: 0.0,
            fraction: 0.0,
            text: None,
        })
    }

    #[test]
    fn table_rows_group_into_one_block_when_not_active() {
        // Active line is the heading; the whole table sits in one block so it
        // can render as a real table instead of line-by-line raw text.
        let lines = to_lines("# Title\n\n| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |");
        let segments = segments(&lines, 0, 1);

        assert_eq!(
            segments,
            vec![
                Segment::Active,                     // line 0: # Title
                Segment::Blank(1),                   // line 1: empty
                Segment::Block { start: 2, end: 6 }, // lines 2..6: the table
            ]
        );
    }

    #[test]
    fn active_line_splits_a_block() {
        // Cursor inside the block breaks it into the part before, the editable
        // active line, and the part after.
        let lines = to_lines("para one\npara two\npara three");
        let segments = segments(&lines, 1, 2);

        assert_eq!(
            segments,
            vec![
                Segment::Block { start: 0, end: 1 },
                Segment::Active,
                Segment::Block { start: 2, end: 3 },
            ]
        );
    }

    #[test]
    fn blank_lines_separate_blocks() {
        let lines = to_lines("a\n\nb");
        // Active line is the trailing "b" so the first paragraph is a block.
        let segments = segments(&lines, 2, 3);

        assert_eq!(
            segments,
            vec![
                Segment::Block { start: 0, end: 1 },
                Segment::Blank(1),
                Segment::Active,
            ]
        );
    }

    /// Visual mode's whole point: the cursor's line is grouped with the lines
    /// around it like any other, so it renders exactly as they do.
    #[test]
    fn without_a_region_the_cursors_line_joins_its_block() {
        let lines = to_lines("a\nb\nc");

        assert_eq!(
            segments(&lines, 1, 2),
            vec![
                Segment::Block { start: 0, end: 1 },
                Segment::Active,
                Segment::Block { start: 2, end: 3 },
            ]
        );
        assert_eq!(
            rendered_segments(&lines),
            vec![Segment::Block { start: 0, end: 3 }]
        );
    }

    /// Blank lines still break blocks in visual mode — they are what paragraph
    /// spacing is made of — and a fenced block still holds its own blanks.
    #[test]
    fn without_a_region_blanks_and_fences_still_decide_the_blocks() {
        let lines = to_lines("a\n\n```\n\nx\n```\n\nb");

        assert_eq!(
            rendered_segments(&lines),
            vec![
                Segment::Block { start: 0, end: 1 },
                Segment::Blank(1),
                Segment::Block { start: 2, end: 6 },
                Segment::Blank(6),
                Segment::Block { start: 7, end: 8 },
            ]
        );
    }

    #[test]
    fn rebuild_blocks_matches_block_segments() {
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document("# Title\n\n| A | B |\n|---|---|\n| 1 | 2 |");
        editor.rebuild();

        let block_count = segments(&editor.lines, editor.active_start, editor.active_end)
            .iter()
            .filter(|s| matches!(s, Segment::Block { .. }))
            .count();

        assert_eq!(editor.blocks.len(), block_count);
    }

    // -- Windowed rendering ---------------------------------------------
    //
    // `render_window` and `estimate_line_height` drive the virtualization in
    // `view`; they are pure functions, so they get their own unit tests
    // independent of the widget tree.

    /// The cumulative line-offset table for a document of uniform, non-wrapping
    /// lines — the geometry `render_window` maps offsets through.
    fn offsets_for(lines: &[String]) -> Vec<f32> {
        let metrics = Metrics::default();
        let mut offsets = Vec::with_capacity(lines.len() + 1);
        let mut top = 0.0;
        for line in lines {
            offsets.push(top);
            top += estimate_line_height(line, false, metrics);
        }
        offsets.push(top);
        offsets
    }

    fn flat_offsets(count: usize) -> Vec<f32> {
        (0..=count).map(|i| i as f32 * EST_LINE_HEIGHT).collect()
    }

    #[test]
    fn render_window_renders_all_without_a_viewport() {
        // No layout has happened yet (first frame, or tests): render everything
        // so small documents and the test suite keep the original behaviour.
        assert_eq!(render_window(None, &flat_offsets(10_000), 10_000), None);
    }

    #[test]
    fn render_window_renders_all_when_window_covers_document() {
        // A small document scrolled to the top: the overscan already reaches
        // past the end, so there is nothing to skip.
        assert_eq!(
            render_window(Some((0.0, 600.0)), &flat_offsets(20), 20),
            None
        );
    }

    #[test]
    fn render_window_crops_a_scrolled_large_document() {
        // 8 000 lines, scrolled ~halfway down a 1 000 px viewport. The window
        // must be a bounded line range (not `None`) covering the visible lines.
        let offsets = flat_offsets(8_000);
        let win = render_window(Some((50_000.0, 1_000.0)), &offsets, 8_000);
        let (start, end) = win.expect("a large scrolled document is windowed");

        // offset_y / EST_LINE_HEIGHT ≈ 1 666 → first visible line ~1 666.
        let first_visible = (50_000.0 / EST_LINE_HEIGHT).floor() as usize;
        let last_visible = (51_000.0 / EST_LINE_HEIGHT).ceil() as usize;

        // Overscan reaches a screen past the top of the viewport, and further
        // past its bottom.
        let screen = (1_000.0 / EST_LINE_HEIGHT) as usize;
        assert!(
            start + screen <= first_visible + 1,
            "overscan before the viewport"
        );
        assert!(end >= last_visible + screen, "overscan after the viewport");

        // ...but no further: the widget tree must stay a few screens large.
        assert!(
            end - start < 5 * screen,
            "window {start}..{end} is too wide"
        );
    }

    /// The window must be derived from the *same* geometry the off-screen
    /// placeholders are sized with. Mapping the offset through a flat
    /// pixels-per-line constant instead put the window hundreds of lines away
    /// from what was on screen for any document with wrapping lines, so the
    /// viewport showed nothing but blank placeholders.
    #[test]
    fn render_window_agrees_with_placeholder_geometry_when_lines_wrap() {
        // Paragraphs of ~270 bytes: each wraps to three visual lines.
        let long = "x".repeat(270);
        let lines = to_lines(
            &(0..600)
                .map(|i| format!("{i} {long}\n\n"))
                .collect::<String>(),
        );
        let offsets = offsets_for(&lines);

        let view_height = 1_000.0;
        let total = *offsets.last().unwrap();
        let offset_y = total / 2.0 - view_height / 2.0;

        let (start, end) = render_window(Some((offset_y, view_height)), &offsets, lines.len())
            .expect("a large scrolled document is windowed");

        // The line genuinely at that offset, by the placeholders' own geometry.
        let on_screen = line_at_offset(&offsets, offset_y);
        assert!(
            start <= on_screen && on_screen < end,
            "window {start}..{end} must contain the line actually on screen ({on_screen})"
        );
    }

    #[test]
    fn line_at_offset_maps_pixels_back_to_lines() {
        let lines = to_lines("short\nshort\nshort");
        let offsets = offsets_for(&lines);

        assert_eq!(line_at_offset(&offsets, 0.0), 0);
        assert_eq!(line_at_offset(&offsets, EST_LINE_HEIGHT - 1.0), 0);
        assert_eq!(line_at_offset(&offsets, EST_LINE_HEIGHT), 1);
        assert_eq!(line_at_offset(&offsets, EST_LINE_HEIGHT * 2.5), 2);
    }

    /// The table must stay exactly `lines.len() + 1` long and end at the total
    /// height, or the offset lookups run off the end.
    #[test]
    fn line_offsets_track_the_document() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("a\nbb\nccc");
        editor.rebuild();

        assert_eq!(editor.line_offsets.len(), editor.lines.len() + 1);
        assert_eq!(editor.line_heights.len(), editor.lines.len());
        assert_eq!(editor.line_offsets[0], 0.0);

        let metrics = editor.metrics();
        let total: f32 = editor
            .lines
            .iter()
            .map(|line| estimate_line_height(line, false, metrics))
            .sum();
        assert_eq!(*editor.line_offsets.last().unwrap(), total);
    }

    #[test]
    fn estimate_line_height_counts_wrapped_lines() {
        let metrics = Metrics::default();
        let long = "x".repeat(200); // > EST_CHARS_PER_LINE → ≥3 visual lines

        // A 200-char line wraps to ceil(200/90) = 3 visual lines.
        assert_eq!(
            estimate_line_height(&long, false, metrics),
            200u32.div_ceil(EST_CHARS_PER_LINE) as f32 * EST_LINE_HEIGHT
        );

        // A single short line is at least one line tall.
        assert_eq!(
            estimate_line_height("short", false, metrics),
            EST_LINE_HEIGHT
        );
    }

    #[test]
    fn headings_and_code_are_estimated_at_their_rendered_size() {
        let metrics = Metrics::default();
        let body = estimate_line_height("Title", false, metrics);

        assert_eq!(estimate_line_height("# Title", false, metrics), body * 2.0);
        assert_eq!(
            estimate_line_height("### Title", false, metrics),
            body * 1.5
        );
        // A hash that does not open a heading is ordinary text.
        assert_eq!(estimate_line_height("#Title", false, metrics), body);

        // Code is set small, and a fence is little more than padding.
        let code = estimate_line_height("let x = 1;", true, metrics);
        assert!(code < body);
        assert_eq!(estimate_line_height("```rust", false, metrics), code);
    }

    #[test]
    fn windowed_view_renders_without_panicking_on_a_large_document() {
        // Simulate a scrolled viewport (as the scrollable would report) and
        // build the view: the windowed path must produce a valid element tree
        // without touching the off-screen blocks.
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document(
            &(0..2_000)
                .map(|i| format!("## H{i}\n\nbody\n\n"))
                .collect::<String>(),
        );
        editor.rebuild();
        editor.viewport = Some((8_000.0, 800.0));

        let _element = editor.view();
    }

    #[test]
    fn delimiter_row_detection() {
        assert!(is_delimiter_row("|---|---|"));
        assert!(is_delimiter_row("| :--- | ---: |"));
        assert!(is_delimiter_row("--- | ---"));
        // A bare thematic break is not a table delimiter.
        assert!(!is_delimiter_row("---"));
        assert!(!is_delimiter_row("| A | B |"));
        assert!(!is_delimiter_row(""));
    }

    #[test]
    fn table_bounds_spans_header_delimiter_and_body() {
        let lines = to_lines("intro\n\n| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |");

        // Every row of the table maps to the same full range.
        for row in 2..=5 {
            assert_eq!(table_bounds(&lines, row), Some((2, 6)));
        }

        // Lines outside the table are not part of it.
        assert_eq!(table_bounds(&lines, 0), None); // intro paragraph
        assert_eq!(table_bounds(&lines, 1), None); // blank line
    }

    #[test]
    fn activating_a_table_line_makes_the_whole_table_raw() {
        // This is the bug: clicking the first row of a table used to leave the
        // remaining rows as an invalid, un-renderable table fragment.
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document("intro\n\n| A | B |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |");

        // Click the table's header row.
        let _ = editor.update(click(2));

        // The whole table (lines 2..6) is now the editable region.
        assert_eq!((editor.active_start, editor.active_end), (2, 6));
        assert_eq!(editor.active_content.line_count(), 4);

        // No rendered block starts inside the table, so nothing renders as a
        // broken half-a-table fragment.
        let segments = segments(&editor.lines, editor.active_start, editor.active_end);
        assert!(
            !segments
                .iter()
                .any(|s| matches!(s, Segment::Block { start, .. } if *start >= 2)),
            "the table must not be split into rendered blocks while editing"
        );
    }

    #[test]
    fn editing_inside_a_table_keeps_the_region_whole() {
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document("| A | B |\n|---|---|\n| 1 | 2 |");

        // The table is active from the start (line 0 is its header).
        assert_eq!((editor.active_start, editor.active_end), (0, 3));

        // Typing a character keeps the full table as the active region and
        // commits the edit back into the document.
        let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
            text_editor::Edit::Insert('X'),
        )));

        assert_eq!((editor.active_start, editor.active_end), (0, 3));
        assert!(editor.lines[0].starts_with('X'));
    }

    #[test]
    fn moving_down_into_a_table_selects_the_whole_table() {
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document("intro\n| A | B |\n|---|---|\n| 1 | 2 |");

        // Start on the intro line, then move down into the table.
        assert_eq!((editor.active_start, editor.active_end), (0, 1));
        let _ = editor.update(Message::MoveDown);

        // The cursor lands in the table, which becomes the whole raw region.
        assert_eq!((editor.active_start, editor.active_end), (1, 4));
    }

    #[test]
    fn page_and_jump_navigation_moves_the_active_line() {
        let last = 199;
        let doc = (0..=last)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document(&doc);
        editor.rebuild();
        assert_eq!((editor.active_start, editor.active_end), (0, 1));

        // A page is measured from the viewport, so it is a screenful of lines
        // — not a fixed count, and more than one line.
        let page = editor.page_lines(0, true);
        assert!(page > 1, "a page must span more than one line");
        assert!(page < last, "the document must be more than one page long");

        // PgDown moves down one page at a time, PgUp back up by the same.
        let _ = editor.update(Message::PageDown);
        assert_eq!(editor.active_start, page);
        let _ = editor.update(Message::PageDown);
        assert_eq!(editor.active_start, 2 * page);
        let _ = editor.update(Message::PageUp);
        assert_eq!(editor.active_start, page);

        // Both clamp at the document's ends instead of running off them, and
        // are a no-op once there.
        for _ in 0..=(last / page + 1) {
            let _ = editor.update(Message::PageDown);
        }
        assert_eq!((editor.active_start, editor.active_end), (last, last + 1));

        for _ in 0..=(last / page + 1) {
            let _ = editor.update(Message::PageUp);
        }
        assert_eq!((editor.active_start, editor.active_end), (0, 1));

        // Ctrl+End / Ctrl+Home (and Ctrl+PgDown / Ctrl+PgUp) jump to the ends.
        let _ = editor.update(Message::JumpToEnd);
        assert_eq!((editor.active_start, editor.active_end), (last, last + 1));
        let _ = editor.update(Message::JumpToStart);
        assert_eq!((editor.active_start, editor.active_end), (0, 1));
    }

    /// The page keys must move the *viewport*, not just the active line: the
    /// cursor moving off screen while the document stays put is exactly the
    /// "nothing happened" that returning no scroll task produced.
    #[test]
    fn a_page_down_scrolls_as_far_as_the_cursor_moves() {
        let doc = (0..400)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document(&doc);
        editor.rebuild();

        // The view is scrolled by the viewport, during layout; assert on the
        // geometry it scrolls by: the height of the lines crossed.
        let page = editor.page_lines(0, true);
        let scrolled = editor.line_top(page) - editor.line_top(0);
        assert!(
            scrolled >= editor.viewport.expect("a seeded viewport").1 * 0.9,
            "a page down must scroll about a screenful, not {scrolled} px"
        );

        // The distance travelled is the same one the cursor moves, so the
        // cursor keeps its place on screen instead of drifting off it.
        let _ = editor.update(Message::PageDown);
        assert_eq!(editor.active_start, page);
        assert_eq!(editor.line_top(editor.active_start), scrolled);
    }

    /// Without a laid-out viewport there is nothing to measure, so a page falls
    /// back to a fixed count rather than collapsing to a single line.
    #[test]
    fn page_lines_falls_back_without_a_viewport() {
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document("a\nb\nc");
        editor.rebuild();
        editor.viewport = None;

        assert_eq!(editor.page_lines(1, true), PAGE_LINES);
        assert_eq!(editor.page_lines(1, false), PAGE_LINES);
    }

    /// A page must stay a page at the bottom of the document, where there are
    /// no longer a screenful of lines below the cursor to measure against.
    #[test]
    fn a_page_stays_a_page_at_the_end_of_the_document() {
        let doc = (0..400)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document(&doc);
        editor.rebuild();
        let page = editor.page_lines(0, true);

        // Scrolled to the bottom, as the scrollable would report it.
        let total = *editor.line_offsets.last().unwrap();
        let view_height = editor.viewport.expect("a seeded viewport").1;
        editor.viewport = Some((total - view_height, view_height));

        // Upwards from the last line: a full page, not the scrap below it.
        assert_eq!(editor.page_lines(editor.lines.len() - 1, false), page);

        let _ = editor.update(Message::JumpToEnd);
        let from = editor.active_start;
        let _ = editor.update(Message::PageUp);
        assert_eq!(editor.active_start, from - page);
    }

    #[test]
    fn merge_with_next_joins_the_following_line() {
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document("foo\nbar\nbaz");
        assert_eq!((editor.active_start, editor.active_end), (0, 1));

        // Forward-delete at the end of "foo" pulls "bar" up onto it.
        let _ = editor.update(Message::MergeWithNext);
        assert_eq!(editor.lines, vec!["foobar", "baz"]);
        assert_eq!((editor.active_start, editor.active_end), (0, 1));

        // At the last line it is a no-op (nothing to pull up).
        let _ = editor.update(Message::JumpToEnd);
        let before = editor.lines.len();
        let _ = editor.update(Message::MergeWithNext);
        assert_eq!(editor.lines.len(), before);
    }

    // -- Line endings -------------------------------------------------------

    #[test]
    fn lines_break_where_the_editor_breaks_them() {
        assert_eq!(split_lines("a\nb"), ["a", "b"]);
        assert_eq!(split_lines("a\r\nb\r\n"), ["a", "b", ""]);
        assert_eq!(split_lines("a\rb"), ["a", "b"]);
        assert_eq!(split_lines("a\n\rb"), ["a", "b"]);
        assert_eq!(split_lines("a\n\r\nb"), ["a", "", "b"]);
        assert_eq!(split_lines(""), [""]);

        // The raw editor holds exactly the lines the document has.
        for text in ["a\r\nb\r\n", "x\ry", "a\n\n\r\nb", "\r\n"] {
            let content = text_editor::Content::<iced::Renderer>::with_text(text);
            assert_eq!(content.line_count(), split_lines(text).len(), "{text:?}");
        }
    }

    /// A Windows file is edited like any other — no stray lines appear — and
    /// is saved with the line endings it came with.
    #[test]
    fn a_windows_file_keeps_its_lines_and_its_line_endings() {
        let mut editor = editor_with("# Title\r\n\r\nfirst\r\nsecond\r\n", 2, 5);

        type_text(&mut editor, "!");
        let _ = editor.update(Message::MoveDown);
        type_text(&mut editor, "?");

        assert_eq!(editor.lines, ["# Title", "", "first!", "second?", ""]);
        assert_eq!(
            editor.document_text(),
            "# Title\r\n\r\nfirst!\r\nsecond?\r\n"
        );

        // A new document is written with plain line feeds.
        let _ = editor.update(Message::NewFile);
        type_text(&mut editor, "x");
        let _ = editor.update(Message::SplitLine);
        assert_eq!(editor.document_text(), "x\n");
    }

    // -- Code blocks --------------------------------------------------------

    #[test]
    fn a_line_inside_a_code_block_makes_the_whole_block_raw() {
        let mut editor = editor_with(
            "intro\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\nafter",
            0,
            0,
        );

        // The blank line inside the code, of all places.
        let _ = editor.update(click(4));

        assert_eq!(region(&editor), (2, 7));
        assert_eq!(editor.cursor_position(), (4, 0));
    }

    #[test]
    fn a_blank_line_inside_code_does_not_split_its_block() {
        let lines = to_lines("```\na\n\nb\n```\n\nafter");

        assert_eq!(
            segments(&lines, 6, 7),
            vec![
                Segment::Block { start: 0, end: 5 },
                Segment::Blank(5),
                Segment::Active,
            ]
        );
    }

    /// Arrowing up into a code block lands on its last line, and carries on
    /// through it a line at a time.
    #[test]
    fn arrowing_up_walks_into_a_code_block_from_below() {
        let mut editor = editor_with("```\none\ntwo\n```\nafter", 4, 0);

        let _ = editor.update(Message::MoveUp);
        assert_eq!(region(&editor), (0, 4));
        assert_eq!(editor.cursor_position(), (3, 0));

        let _ = editor.update(Message::MoveUp);
        assert_eq!(editor.cursor_position(), (2, 0));
    }

    /// Typing a fence that pairs up with one further down turns everything in
    /// between into code — and the raw region grows to take all of it, so
    /// none of it renders as a broken half of a block.
    #[test]
    fn typing_a_fence_turns_what_follows_into_one_code_block() {
        let mut editor = editor_with("intro\n\nx\nlet a = 1;\n\nlet b = 2;\n```\n\nafter", 2, 0);

        type_text(&mut editor, "```");

        assert_eq!(editor.lines[2], "```x");
        assert_eq!(
            region(&editor),
            (2, 7),
            "the new code block is raw as a whole"
        );
        assert_eq!(editor.cursor_position(), (2, 3));
        assert_eq!(editor.block_sources, expected_block_sources(&editor));
    }

    // -- Vertical moves -----------------------------------------------------

    /// A short line clamps the caret, but the column it was aiming for is
    /// kept: on a long line again, it returns there.
    #[test]
    fn the_caret_keeps_its_column_across_a_short_line() {
        let mut editor = editor_with("a long first line\nab\nanother long line", 0, 10);

        let _ = editor.update(Message::MoveDown);
        assert_eq!(editor.cursor_position(), (1, 2));
        let _ = editor.update(Message::MoveDown);
        assert_eq!(editor.cursor_position(), (2, 10));

        // Moving sideways sets a new column to aim for.
        act(
            &mut editor,
            text_editor::Action::Move(text_editor::Motion::Left),
        );
        let _ = editor.update(Message::MoveUp);
        let _ = editor.update(Message::MoveUp);
        assert_eq!(editor.cursor_position(), (0, 9));
    }

    #[test]
    fn typing_forgets_the_column_aimed_for() {
        let mut editor = editor_with("a long first line\nab\nanother long line", 0, 10);

        let _ = editor.update(Message::MoveDown);
        type_text(&mut editor, "c");
        let _ = editor.update(Message::MoveDown);

        assert_eq!(editor.cursor_position(), (2, 3));
    }

    // -- Incremental re-segmentation -----------------------------------------

    /// Moving the caret only re-parses the blocks it leaves and enters; the
    /// rest keep their parse — and with it their key, and their laid-out text.
    #[test]
    fn navigation_leaves_distant_blocks_parsed_once() {
        let document: String = (0..200).map(|i| format!("para {i}\n\n")).collect();
        let mut editor = editor_with(&document, 0, 0);
        let distant = *editor.block_ids.last().unwrap();
        let before = editor.next_block_id;

        for _ in 0..20 {
            let _ = editor.update(Message::MoveDown);
        }

        assert_eq!(*editor.block_ids.last().unwrap(), distant);
        assert!(
            editor.next_block_id - before <= 20,
            "{} blocks parsed for 20 moves",
            editor.next_block_id - before
        );
        assert_eq!(editor.block_sources, expected_block_sources(&editor));
    }

    /// Lines before a dirty range keep their numbers; lines after it moved by
    /// the lines gained or lost, however many splices happened.
    #[test]
    fn dirty_ranges_follow_splices() {
        let mut dirty = None;
        Dirty::touch(&mut dirty, 10..12);

        // Two lines gained before the range: it widens back to the splice,
        // and its end moves down with the lines after it.
        Dirty::splice(&mut dirty, 2..3, 3);
        assert_eq!(
            dirty,
            Some(Dirty {
                start: 2,
                end: 14,
                delta: 2
            })
        );
        assert_eq!(dirty.unwrap().old_end(), 12);

        // Two lines lost after it: the range reaches the splice.
        Dirty::splice(&mut dirty, 20..22, 0);
        assert_eq!(
            dirty,
            Some(Dirty {
                start: 2,
                end: 20,
                delta: 0
            })
        );
    }

    // ---- randomized regression coverage ----
    //
    // A freeze that needs `kill -9` is almost always an infinite loop or a
    // panic on the UI thread. These helpers drive the editor through long
    // randomized sequences that mirror the real key-binding routing, asserting
    // the active-range invariants and building the view after every step (the
    // view path is what a frozen window would be stuck in).

    fn check_invariants(editor: &MarkdownEditor, seed: u64, step: usize) {
        assert!(
            !editor.lines.is_empty(),
            "seed {seed} step {step}: no lines"
        );
        assert!(
            editor.active_start < editor.lines.len(),
            "seed {seed} step {step}: active_start {} >= len {}",
            editor.active_start,
            editor.lines.len()
        );
        assert!(
            editor.active_start < editor.active_end,
            "seed {seed} step {step}: active_start {} >= active_end {}",
            editor.active_start,
            editor.active_end
        );
        assert!(
            editor.active_end <= editor.lines.len(),
            "seed {seed} step {step}: active_end {} > len {}",
            editor.active_end,
            editor.lines.len()
        );

        // The cached segment list is maintained incrementally (edits inside the
        // active region only slide the segments after it). It must stay
        // byte-identical to a full walk, or `view` renders the wrong lines and
        // `blocks` falls out of alignment with them.
        // The raw editor holds exactly the region's lines: every edit is
        // written back line for line, so any drift corrupts the document.
        assert!(
            content_matches(
                &editor.active_content,
                &editor.lines[editor.active_start..editor.active_end]
            ),
            "seed {seed} step {step}: the editor holds {:?}, the region is {:?}",
            editor.active_content.text(),
            &editor.lines[editor.active_start..editor.active_end]
        );

        let fresh = walked_segments(editor);
        assert_eq!(
            editor.segments, fresh,
            "seed {seed} step {step}: cached segments diverged from a full walk"
        );
        assert_eq!(
            editor.synced_region,
            editor.active_region(),
            "seed {seed} step {step}: segments were built for another region"
        );

        // The height tables are the sole offset↔line mapping: a length that
        // drifts from the document's sends the window and every spacer
        // somewhere else.
        assert_eq!(
            editor.line_heights.len(),
            editor.lines.len(),
            "seed {seed} step {step}: line heights out of step with the lines"
        );
        assert_eq!(
            editor.line_offsets.len(),
            editor.lines.len() + 1,
            "seed {seed} step {step}: line offsets out of step with the lines"
        );
        assert_eq!(
            editor.block_ids.len(),
            editor.blocks.len(),
            "seed {seed} step {step}: block ids out of step with the blocks"
        );

        // A code block is never split into rendered pieces: the region either
        // covers all of one or none of it.
        for (start, end) in fences::blocks(&editor.lines) {
            let overlaps = editor.active_start < end && start < editor.active_end;
            let covers = editor.active_start <= start && end <= editor.active_end;
            assert!(
                !overlaps || covers,
                "seed {seed} step {step}: region {}..{} splits code block {start}..{end}",
                editor.active_start,
                editor.active_end
            );
        }

        // `rebuild_blocks` only splices the changed middle, so a region that
        // moves by more than a line — which every selection message can do —
        // is exactly where it would leave a stale block behind, rendering the
        // wrong Markdown for a line that is still on screen.
        assert_eq!(
            editor.block_sources,
            expected_block_sources(editor),
            "seed {seed} step {step}: parsed blocks diverged from the segments"
        );
        assert_eq!(
            editor.blocks.len(),
            editor.block_sources.len(),
            "seed {seed} step {step}: block count diverged from its sources"
        );
    }

    fn next_rand(state: &mut u64) -> u64 {
        // xorshift64
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    fn simulated_message(editor: &MarkdownEditor, r: u64) -> Message {
        use text_editor::{Action, Edit, Motion};

        let cursor = editor.active_content.cursor();
        let multiline = editor.active_end - editor.active_start > 1;
        let at_editor_top = cursor.position.line == 0;
        let at_editor_bottom = cursor.position.line + 1 >= editor.active_content.line_count();
        let at_line_start = cursor.position.column == 0;
        let cursor_line_len = editor
            .lines
            .get(editor.active_start + cursor.position.line)
            .map(String::len)
            .unwrap_or(0);
        let at_line_end = cursor.position.column >= cursor_line_len;
        let at_doc_first = editor.active_start == 0;
        let at_doc_last = editor.active_end >= editor.lines.len();

        match r % 28 {
            0 | 1 => {
                let chars = ['a', ' ', '|', '-', '#', '*', '`', '>', '[', ']'];
                let ch = chars[(r >> 8) as usize % chars.len()];
                Message::EditorAction(Action::Edit(Edit::Insert(ch)))
            }
            2 => {
                // Enter: routed exactly like the key binding.
                if !multiline {
                    Message::SplitLine
                } else {
                    Message::EditorAction(Action::Edit(Edit::Enter))
                }
            }
            3 => {
                // Backspace routing.
                if at_editor_top && at_line_start && !at_doc_first {
                    Message::MergeWithPrevious
                } else {
                    Message::EditorAction(Action::Edit(Edit::Backspace))
                }
            }
            4 => {
                // Delete routing.
                if at_editor_bottom && at_line_end && !at_doc_last {
                    Message::MergeWithNext
                } else {
                    Message::EditorAction(Action::Edit(Edit::Delete))
                }
            }
            5 => {
                // Arrow up routing.
                if at_editor_top && !at_doc_first {
                    Message::MoveUp
                } else {
                    Message::EditorAction(Action::Move(Motion::Up))
                }
            }
            6 => {
                // Arrow down routing.
                if at_editor_bottom && !at_doc_last {
                    Message::MoveDown
                } else {
                    Message::EditorAction(Action::Move(Motion::Down))
                }
            }
            7 => Message::EditorAction(Action::Move(Motion::Left)),
            8 => Message::EditorAction(Action::Move(Motion::Right)),
            // PgUp/PgDown are intercepted by the binding into document
            // navigation, so the app sees these custom messages, not raw moves.
            9 => Message::PageUp,
            10 => Message::PageDown,
            11 => {
                // Word and line selections keep only the clicked position, so
                // they exercise the recovery in `selection_span` too.
                let actions = [Action::SelectAll, Action::SelectWord, Action::SelectLine];
                Message::EditorAction(actions[(r >> 8) as usize % actions.len()].clone())
            }
            12 => {
                let index = (r >> 8) as usize % (editor.lines.len() + 2);
                click(index)
            }
            13 => {
                let actions = [
                    FormatAction::Bold,
                    FormatAction::Italic,
                    FormatAction::InlineCode,
                    FormatAction::Heading,
                    FormatAction::BulletList,
                    FormatAction::Quote,
                    FormatAction::HorizontalRule,
                    FormatAction::Link,
                ];
                Message::Format(actions[(r >> 8) as usize % actions.len()].clone())
            }
            14 => Message::NewFile,
            15 => Message::JumpToStart,
            16 => Message::JumpToEnd,
            // Selection widens and shrinks the raw region on its own, so it
            // gets the same randomized scrutiny as the editing paths: every
            // one of these can leave `segments` and `blocks` describing a
            // different set of lines than the region actually covers.
            17 => Message::SelectAll,
            18 => {
                let motions = [
                    Motion::Up,
                    Motion::Down,
                    Motion::Left,
                    Motion::Right,
                    Motion::WordLeft,
                    Motion::WordRight,
                    Motion::PageUp,
                    Motion::PageDown,
                    Motion::DocumentStart,
                    Motion::DocumentEnd,
                ];
                Message::ExtendSelection(motions[(r >> 8) as usize % motions.len()])
            }
            19 => Message::DragTo((r >> 8) as usize % (editor.lines.len() + 2)),
            20 => Message::DragEnd,
            21 => Message::EditorAction(Action::Edit(Edit::Paste(Arc::new(
                [
                    "x",
                    "a\nb",
                    "  spaced  ",
                    "<p>html</p>",
                    "",
                    "```\ncode\n\nmore\n```",
                    "\n\n",
                    "~~~",
                    "p\r\nq\r\n",
                ][(r >> 8) as usize % 9]
                    .to_owned(),
            )))),
            // Switching modes adds or removes the raw region, re-splitting the
            // block the cursor is in; the steps that follow then run against
            // the other mode's segmentation.
            22 => Message::ToggleMode,
            // Undo and redo splice lines wherever the edit being undone was,
            // however far from the region — the hardest case for keeping the
            // segments up to date incrementally.
            23 | 24 => Message::Undo,
            25 => Message::Redo,
            // Replacing edits lines in place, anywhere in the document.
            26 => Message::ReplaceAll,
            _ => Message::EditorAction(Action::Edit(Edit::Insert('x'))),
        }
    }

    #[test]
    fn fuzz_editing_never_breaks_invariants() {
        let seeds = [
            "# Hello\n\nsome text\n\n| A | B |\n|---|---|\n| 1 | 2 |\n",
            "",
            "single line",
            "a\nb\nc\nd\ne",
            "| H |\n|---|\n| x |",
            // Code blocks with blank lines inside, next to prose and each
            // other: the fences decide how everything after them splits.
            "intro\n\n```rust\nfn a() {}\n\nfn b() {}\n```\n\nmiddle\n\n~~~\n\nx\n~~~\n\nend",
            "```\n\n```\n```\nopen fence left dangling\n\n- a\n- b\n\n> q",
            // A Windows file.
            "# Title\r\n\r\ntext\r\n\r\n```\r\ncode\r\n```\r\n",
        ];

        // `MD_EDITOR_FUZZ=<runs>,<steps>` for a longer stress run.
        let (runs, steps) = std::env::var("MD_EDITOR_FUZZ")
            .ok()
            .and_then(|value| {
                let (runs, steps) = value.split_once(',')?;
                Some((runs.parse().ok()?, steps.parse().ok()?))
            })
            .unwrap_or((120u64, 50usize));

        for (s, doc) in seeds.iter().enumerate() {
            for seed in 0..runs {
                let (mut editor, _t) = MarkdownEditor::new_with_file(None);
                editor.load_document(doc);
                editor.rebuild();
                let mut state = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);

                // A third of the runs replace as they go: deleting every "a"
                // can blank a line out anywhere in the document.
                if seed % 3 == 0 {
                    let _ = editor.update(Message::OpenSearch { replace: true });
                    let _ = editor.update(Message::SearchQueryChanged("a".to_owned()));
                    check_invariants(&editor, seed + (s as u64) * 1000, 0);
                }

                for step in 0..steps {
                    let r = next_rand(&mut state);
                    let msg = simulated_message(&editor, r);
                    let _ = editor.update(msg);
                    check_invariants(&editor, seed + (s as u64) * 1000, step);
                    // Build the view too: this exercises the render-construction
                    // path (segment/block alignment, the active-range math, and
                    // the status-bar indexing) — i.e. the code a frozen window
                    // would be stuck in.
                    let _element = editor.view();
                }
            }
        }
    }

    // -- The cursor line is always raw, editable source ------------------

    #[test]
    fn the_cursor_line_is_split_out_of_its_block() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        // Two contiguous source lines that would otherwise form one block.
        editor.load_document("alpha line\nbeta line");
        editor.rebuild();

        // The cursor starts on line 0, so only line 1 is rendered as Markdown.
        assert_eq!(editor.block_sources, vec!["beta line".to_owned()]);

        // Moving the cursor down swaps which line is raw.
        let _ = editor.update(Message::MoveDown);
        assert_eq!(editor.active_start, 1);
        assert_eq!(editor.block_sources, vec!["alpha line".to_owned()]);
        let _ = editor.view();
    }

    // -- Undo / redo -----------------------------------------------------

    fn type_text(editor: &mut MarkdownEditor, text: &str) {
        for ch in text.chars() {
            let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
                text_editor::Edit::Insert(ch),
            )));
        }
    }

    #[test]
    fn undo_reverts_a_run_of_typing_in_one_step() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("hello");
        editor.rebuild();

        let _ = editor.update(Message::EditorAction(text_editor::Action::Move(
            text_editor::Motion::End,
        )));
        type_text(&mut editor, " world");
        assert_eq!(editor.lines[0], "hello world");

        // A whole run of keystrokes collapses into a single undo entry.
        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines[0], "hello");
        assert!(!editor.history.can_undo());
    }

    #[test]
    fn redo_reapplies_an_undone_edit() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("hello");
        editor.rebuild();

        let _ = editor.update(Message::EditorAction(text_editor::Action::Move(
            text_editor::Motion::End,
        )));
        type_text(&mut editor, "!");

        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines[0], "hello");
        assert!(editor.history.can_redo());

        let _ = editor.update(Message::Redo);
        assert_eq!(editor.lines[0], "hello!");
        assert_eq!(editor.document_text(), "hello!");
    }

    #[test]
    fn undo_reverts_a_line_split() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("one two");
        editor.rebuild();

        let _ = editor.update(Message::EditorAction(text_editor::Action::Move(
            text_editor::Motion::End,
        )));
        let _ = editor.update(Message::SplitLine);
        assert_eq!(editor.lines, vec!["one two".to_owned(), String::new()]);

        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines, vec!["one two".to_owned()]);
    }

    #[test]
    fn undo_reverts_a_merge_with_the_previous_line() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("first\nsecond");
        editor.rebuild();

        let _ = editor.update(click(1));
        let _ = editor.update(Message::MergeWithPrevious);
        assert_eq!(editor.lines, vec!["firstsecond".to_owned()]);

        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines, vec!["first".to_owned(), "second".to_owned()]);
    }

    #[test]
    fn moving_the_cursor_starts_a_new_undo_step() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("aa\nbb");
        editor.rebuild();

        type_text(&mut editor, "x");
        // A click on the next line: a separate place, so a separate undo step.
        let _ = editor.update(click(1));
        type_text(&mut editor, "y");
        assert_eq!(editor.lines, vec!["xaa".to_owned(), "ybb".to_owned()]);

        // Each line's edits undo separately.
        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines, vec!["xaa".to_owned(), "bb".to_owned()]);
        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines, vec!["aa".to_owned(), "bb".to_owned()]);
    }

    #[test]
    fn undoing_restores_the_editor_content_not_just_the_lines() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("value");
        editor.rebuild();

        let _ = editor.update(Message::EditorAction(text_editor::Action::Move(
            text_editor::Motion::End,
        )));
        type_text(&mut editor, "-changed");

        let _ = editor.update(Message::Undo);
        // The raw editor must show the restored text, otherwise the next
        // keystroke would commit the stale content straight back.
        assert_eq!(editor.active_content.text(), "value");
    }

    #[test]
    fn loading_a_document_clears_the_history() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("something");
        editor.rebuild();
        type_text(&mut editor, "x");
        assert!(editor.history.can_undo());

        editor.load_document("fresh");
        assert!(!editor.history.can_undo());
        assert!(!editor.history.can_redo());
    }

    #[test]
    fn undo_survives_an_edit_that_changes_the_line_count() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        // A table: the whole thing is one active region, so an Enter inside it
        // grows the region rather than splitting the document.
        editor.load_document("| A |\n|---|\n| 1 |\n\ntail");
        editor.rebuild();

        let _ = editor.update(click(2));
        let before = editor.lines.clone();
        let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
            text_editor::Edit::Enter,
        )));
        assert_eq!(editor.lines.len(), before.len() + 1);

        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines, before);
    }

    // -- Selection, clipboard and deletion --------------------------------
    //
    // The editor only ever holds *one* `text_editor`, covering the raw region.
    // A selection is therefore only real — drawable, copyable, replaceable —
    // for the lines that region spans, which is why nearly every assertion
    // below checks the region alongside the selected text.

    /// The text `Ctrl+C` would put on the clipboard.
    fn selected(editor: &MarkdownEditor) -> Option<String> {
        editor.active_content.selection()
    }

    /// The raw region as a `start..end` pair.
    fn region(editor: &MarkdownEditor) -> (usize, usize) {
        (editor.active_start, editor.active_end)
    }

    /// An editor holding `document`, with the cursor at `(line, column)`.
    fn editor_with(document: &str, line: usize, column: usize) -> MarkdownEditor {
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document(document);
        editor.activate_line(line, column);
        editor.rebuild();
        editor
    }

    /// A key press as the *focused* active editor sees it, so the default
    /// bindings (which ignore an unfocused editor) are exercised too.
    fn key_press(key: keyboard::Key, modifiers: keyboard::Modifiers) -> text_editor::KeyPress {
        text_editor::KeyPress {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            modifiers,
            text: None,
            status: text_editor::Status::Focused { is_hovered: false },
        }
    }

    /// A named key press with no modifiers.
    fn named_key(named: keyboard::key::Named) -> text_editor::KeyPress {
        key_press(keyboard::Key::Named(named), keyboard::Modifiers::empty())
    }

    /// Perform an editor action, the way the widget would report it.
    fn act(editor: &mut MarkdownEditor, action: text_editor::Action) {
        let _ = editor.update(Message::EditorAction(action));
    }

    /// `Shift+→` from the end of a line pulls the next line into the region,
    /// so the selection can actually cross the line break.
    #[test]
    fn shift_right_selects_across_a_line_break() {
        let mut editor = editor_with("alpha\nbravo", 0, 5);

        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Right));

        assert_eq!(region(&editor), (0, 2));
        assert_eq!(selected(&editor).as_deref(), Some("\n"));
    }

    /// `Shift+←` at column 0 reaches back into the previous line.
    #[test]
    fn shift_left_selects_back_across_a_line_break() {
        let mut editor = editor_with("alpha\nbravo", 1, 0);

        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Left));

        assert_eq!(region(&editor), (0, 2));
        assert_eq!(selected(&editor).as_deref(), Some("\n"));
    }

    /// `Shift+↓` at the bottom of the region grows it downwards.
    #[test]
    fn shift_down_selects_into_the_following_line() {
        let mut editor = editor_with("alpha\nbravo\ncharlie", 0, 0);

        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Down));

        assert_eq!(region(&editor), (0, 2));
        assert_eq!(selected(&editor).as_deref(), Some("alpha\n"));
    }

    /// `Shift+↑` at the top of the region grows it upwards, and the anchor
    /// stays where the selection started.
    #[test]
    fn shift_up_selects_into_the_previous_line() {
        let mut editor = editor_with("alpha\nbravo\ncharlie", 2, 0);

        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Up));

        assert_eq!(region(&editor), (1, 3));
        assert_eq!(selected(&editor).as_deref(), Some("bravo\n"));
    }

    /// Repeating the motion keeps widening the region one line at a time.
    #[test]
    fn selection_grows_one_line_per_keystroke() {
        let mut editor = editor_with("one\ntwo\nthree\nfour", 0, 0);

        for expected in [(0, 2), (0, 3), (0, 4)] {
            let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Down));
            assert_eq!(region(&editor), expected);
        }

        assert_eq!(selected(&editor).as_deref(), Some("one\ntwo\nthree\n"));
    }

    /// `Ctrl/Cmd+Shift+End` selects from the cursor to the end of the
    /// document, however far away that is.
    #[test]
    fn selecting_to_the_document_end_covers_everything_below() {
        let mut editor = editor_with("one\ntwo\nthree", 0, 1);

        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::DocumentEnd));

        assert_eq!(region(&editor), (0, 3));
        assert_eq!(selected(&editor).as_deref(), Some("ne\ntwo\nthree"));
    }

    /// `Ctrl/Cmd+Shift+Home` is its mirror image.
    #[test]
    fn selecting_to_the_document_start_covers_everything_above() {
        let mut editor = editor_with("one\ntwo\nthree", 2, 2);

        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::DocumentStart));

        assert_eq!(region(&editor), (0, 3));
        assert_eq!(selected(&editor).as_deref(), Some("one\ntwo\nth"));
    }

    /// `Shift+PgDn` takes a whole page of lines with it.
    #[test]
    fn selecting_by_page_covers_a_page_of_lines() {
        let document: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let mut editor = editor_with(&document, 0, 0);
        // No real layout in a test, so a page is the fallback line count.
        editor.viewport = None;

        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::PageDown));

        assert_eq!(region(&editor), (0, PAGE_LINES + 1));
        assert_eq!(editor.cursor_position(), (PAGE_LINES, 0));
        assert_eq!(
            selected(&editor).as_deref(),
            Some(&document[..document.find("line 15").unwrap()])
        );

        // And back up again, to exactly where it started.
        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::PageUp));
        assert!(!editor.has_selection());
        assert_eq!(editor.cursor_position(), (0, 0));
    }

    /// Selection is reversible: shrinking it back to nothing drops it.
    #[test]
    fn a_selection_collapsed_back_onto_itself_disappears() {
        let mut editor = editor_with("alpha\nbravo", 0, 5);

        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Right));
        assert!(editor.has_selection());

        act(
            &mut editor,
            text_editor::Action::Select(text_editor::Motion::Left),
        );
        assert!(!editor.has_selection());
    }

    /// A double click selects a word and a triple click a line — both through
    /// the widget, which records only the *clicked* position and leaves the
    /// extent implicit. The editor must still see them as real selections,
    /// or `Ctrl+C` after a double click would quietly do nothing.
    #[test]
    fn a_word_or_line_selection_is_recognised_and_measured() {
        let mut editor = editor_with("Hello beautiful world", 0, 10);

        act(&mut editor, text_editor::Action::SelectWord);
        assert!(editor.has_selection());
        assert_eq!(selected(&editor).as_deref(), Some("beautiful"));
        // Recovered from the text: the click position alone says nothing.
        assert_eq!(editor.selection_span(), ((0, 6), (0, 15)));

        act(&mut editor, text_editor::Action::SelectLine);
        assert!(editor.has_selection());
        assert_eq!(editor.selection_span(), ((0, 0), (0, 21)));
    }

    /// Extending a double-click selection past the line keeps the whole word,
    /// anchored at its start.
    #[test]
    fn extending_a_word_selection_keeps_the_whole_word() {
        let mut editor = editor_with("Hello beautiful world\nnext line", 0, 10);

        act(&mut editor, text_editor::Action::SelectWord);
        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Down));

        assert_eq!(region(&editor), (0, 2));
        assert!(
            selected(&editor).is_some_and(|text| text.starts_with("beautiful")),
            "the double-clicked word should still be selected: {:?}",
            selected(&editor)
        );
    }

    /// The recovery has to pick the occurrence that actually covers the
    /// cursor, not merely the first one on the line.
    #[test]
    fn a_repeated_word_is_located_at_the_cursor() {
        assert_eq!(selection_start_column("one one one", 8, "one"), 8);
        assert_eq!(selection_start_column("one one one", 4, "one"), 4);
        // A caret sitting on the boundary belongs to the word before it.
        assert_eq!(selection_start_column("one one one", 7, "one"), 4);
        // Multi-byte text never lands mid-character.
        assert_eq!(selection_start_column("héllo wörld", 8, "wörld"), 7);
    }

    // -- Select all -------------------------------------------------------

    #[test]
    fn select_all_selects_the_whole_document() {
        let mut editor = editor_with("# Title\n\nbody text\n\n| A |\n|---|\n| 1 |", 2, 0);

        let _ = editor.update(Message::SelectAll);

        assert_eq!(region(&editor), (0, editor.lines.len()));
        assert_eq!(
            selected(&editor).as_deref(),
            Some("# Title\n\nbody text\n\n| A |\n|---|\n| 1 |")
        );
    }

    /// `Ctrl+A` is routed to the *document*, not to the raw region the editor
    /// happens to hold.
    #[test]
    fn ctrl_a_selects_the_document() {
        let binding = editor_binding(ctrl_key_press("a"), BindingContext::default());

        assert!(matches!(
            binding,
            Some(text_editor::Binding::Custom(Message::SelectAll))
        ));
    }

    /// An empty document has nothing to select, and must not end up with a
    /// zero-width "selection" that reads back as an empty string.
    #[test]
    fn select_all_on_an_empty_document_selects_nothing() {
        let mut editor = editor_with("", 0, 0);

        let _ = editor.update(Message::SelectAll);

        assert!(!editor.has_selection());
        assert_eq!(region(&editor), (0, 1));
    }

    /// Typing replaces the entire selection, which after `Ctrl+A` is the
    /// entire document — in one undoable step.
    #[test]
    fn typing_over_a_select_all_replaces_the_document() {
        let mut editor = editor_with("one\ntwo\nthree", 0, 0);

        let _ = editor.update(Message::SelectAll);
        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        );

        assert_eq!(editor.lines, vec!["x".to_owned()]);

        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines, to_lines("one\ntwo\nthree"));
    }

    // -- Copy and cut -----------------------------------------------------

    /// Copy is the widget's own: it writes exactly [`Content::selection`] to
    /// the clipboard. What matters here is that the selection really is the
    /// text the user marked, line breaks and all, and that copying leaves the
    /// document completely alone.
    #[test]
    fn a_copied_selection_is_the_exact_marked_text() {
        let mut editor = editor_with("  spaced\ttext\nsecond line", 0, 0);
        let before = editor.lines.clone();

        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::DocumentEnd));

        assert_eq!(
            selected(&editor).as_deref(),
            Some("  spaced\ttext\nsecond line")
        );
        assert_eq!(editor.lines, before);
        assert!(!editor.is_dirty);
    }

    /// With nothing selected, `Ctrl+C` and `Ctrl+X` do nothing at all — in
    /// particular they must not hand the widget an empty string to write over
    /// whatever is already on the clipboard.
    #[test]
    fn copy_and_cut_without_a_selection_do_nothing() {
        for key in ["c", "x"] {
            let binding = editor_binding(ctrl_key_press(key), BindingContext::default());

            assert!(
                matches!(&binding, Some(text_editor::Binding::Sequence(steps)) if steps.is_empty()),
                "{key} with no selection should be a no-op"
            );
        }
    }

    /// With a selection they fall through to the widget's own clipboard
    /// handling.
    #[test]
    fn copy_and_cut_with_a_selection_reach_the_clipboard() {
        let ctx = BindingContext {
            has_selection: true,
            ..BindingContext::default()
        };

        for (key, expected) in [
            ("c", text_editor::Binding::Copy),
            ("x", text_editor::Binding::Cut),
        ] {
            let press = key_press(
                keyboard::Key::Character(key.into()),
                keyboard::Modifiers::CTRL,
            );

            assert_eq!(
                std::mem::discriminant(&editor_binding(press, ctx).unwrap()),
                std::mem::discriminant(&expected),
                "{key} with a selection should reach the clipboard",
            );
        }
    }

    /// Cut removes exactly the selection, leaves the cursor where it was, and
    /// is a single undoable edit. (The widget copies first, then sends the
    /// deletion through as an ordinary edit — which is what is replayed here.)
    #[test]
    fn cutting_a_multi_line_selection_is_one_undoable_edit() {
        let mut editor = editor_with("alpha\nbravo\ncharlie", 0, 2);
        let before = editor.lines.clone();

        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Down));
        assert_eq!(selected(&editor).as_deref(), Some("pha\nbr"));

        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Delete),
        );

        assert_eq!(editor.lines, to_lines("alavo\ncharlie"));
        // The cursor sits where the removed text began, with nothing selected.
        assert_eq!(editor.cursor_position(), (0, 2));
        assert!(!editor.has_selection());

        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines, before);
    }

    // -- Paste ------------------------------------------------------------

    /// Pasting text at the cursor keeps every line break, space and tab, and
    /// leaves the cursor immediately after what was inserted.
    #[test]
    fn pasting_multi_line_text_preserves_it_exactly() {
        let mut editor = editor_with("start\nend", 0, 5);

        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
                "\n\tone  two\nthree".to_owned(),
            ))),
        );

        assert_eq!(editor.lines, to_lines("start\n\tone  two\nthree\nend"));
        assert_eq!(editor.cursor_position(), (2, 5));
        assert!(editor.is_dirty);
    }

    /// A paste with a selection replaces it, rather than inserting alongside.
    #[test]
    fn pasting_over_a_selection_replaces_it() {
        let mut editor = editor_with("one\ntwo\nthree", 0, 0);

        let _ = editor.update(Message::SelectAll);
        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new("replaced".to_owned()))),
        );

        assert_eq!(editor.lines, vec!["replaced".to_owned()]);
        assert!(!editor.has_selection());
    }

    /// Rich text arrives as markup; pasting the tags verbatim into a Markdown
    /// document is never what was meant.
    #[test]
    fn pasting_html_converts_it_to_markdown() {
        let mut editor = editor_with("", 0, 0);

        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
                "<h1>Title</h1><p>Some <strong>bold</strong> text.</p>".to_owned(),
            ))),
        );

        assert_eq!(editor.lines, to_lines("# Title\n\nSome **bold** text."));
    }

    /// Plain text that merely *contains* an angle bracket is pasted untouched.
    #[test]
    fn pasting_plain_text_never_runs_it_through_the_converter() {
        let mut editor = editor_with("", 0, 0);

        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(
                "if a < b && c > d { }".to_owned(),
            ))),
        );

        assert_eq!(editor.lines, vec!["if a < b && c > d { }".to_owned()]);
    }

    // -- Backspace and Delete ---------------------------------------------

    /// Backspace with a selection removes the selection, not one character —
    /// so it must not be rerouted into a line merge at the region's edge.
    #[test]
    fn backspace_with_a_selection_deletes_the_selection() {
        let ctx = BindingContext {
            has_selection: true,
            at_editor_top: true,
            at_line_start: true,
            ..BindingContext::default()
        };

        assert!(matches!(
            editor_binding(named_key(keyboard::key::Named::Backspace), ctx),
            Some(text_editor::Binding::Backspace)
        ));

        let mut editor = editor_with("alpha\nbravo\ncharlie", 1, 0);
        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Down));
        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Backspace),
        );

        assert_eq!(editor.lines, to_lines("alpha\ncharlie"));
    }

    /// Delete behaves identically — the same selection, the same result.
    #[test]
    fn delete_with_a_selection_deletes_the_same_selection() {
        let mut editor = editor_with("alpha\nbravo\ncharlie", 1, 0);
        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Down));
        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Delete),
        );

        assert_eq!(editor.lines, to_lines("alpha\ncharlie"));
    }

    /// Backspace at the start of a line joins it onto the one above.
    #[test]
    fn backspace_at_the_start_of_a_line_merges_it_upwards() {
        let mut editor = editor_with("alpha\nbravo", 1, 0);

        let binding = editor_binding(
            named_key(keyboard::key::Named::Backspace),
            BindingContext {
                at_editor_top: true,
                at_line_start: true,
                ..BindingContext::default()
            },
        );
        assert!(matches!(
            binding,
            Some(text_editor::Binding::Custom(Message::MergeWithPrevious))
        ));

        let _ = editor.update(Message::MergeWithPrevious);
        assert_eq!(editor.lines, vec!["alphabravo".to_owned()]);
        assert_eq!(editor.cursor_position(), (0, 5));
    }

    /// Delete at the end of a line pulls the next one up onto it.
    #[test]
    fn delete_at_the_end_of_a_line_merges_the_next_one_up() {
        let mut editor = editor_with("alpha\nbravo", 0, 5);

        let binding = editor_binding(
            named_key(keyboard::key::Named::Delete),
            BindingContext {
                at_editor_bottom: true,
                at_line_end: true,
                ..BindingContext::default()
            },
        );
        assert!(matches!(
            binding,
            Some(text_editor::Binding::Custom(Message::MergeWithNext))
        ));

        let _ = editor.update(Message::MergeWithNext);
        assert_eq!(editor.lines, vec!["alphabravo".to_owned()]);
    }

    /// There is nothing before the first line or after the last, so neither
    /// key may run off the document.
    #[test]
    fn deleting_past_the_edges_of_the_document_does_nothing() {
        let mut editor = editor_with("only line", 0, 0);

        let _ = editor.update(Message::MergeWithPrevious);
        let _ = editor.update(Message::MergeWithNext);

        assert_eq!(editor.lines, vec!["only line".to_owned()]);
    }

    /// Enter with a selection replaces it with the line break, instead of
    /// splitting the line around a cursor that is not where the user thinks.
    #[test]
    fn enter_with_a_selection_replaces_it() {
        let ctx = BindingContext {
            has_selection: true,
            ..BindingContext::default()
        };

        assert!(matches!(
            editor_binding(named_key(keyboard::key::Named::Enter), ctx),
            Some(text_editor::Binding::Enter)
        ));
    }

    // -- Word deletion ----------------------------------------------------

    /// `Ctrl+Backspace` (`Opt+Backspace` on macOS) selects the word to the
    /// left and deletes it — one word, not one character.
    #[test]
    fn ctrl_backspace_removes_the_previous_word() {
        let press = key_press(
            keyboard::Key::Named(keyboard::key::Named::Backspace),
            keyboard::Modifiers::CTRL,
        );
        let binding = editor_binding(
            press,
            BindingContext {
                at_line_start: false,
                ..BindingContext::default()
            },
        );

        let Some(text_editor::Binding::Sequence(steps)) = binding else {
            panic!("word deletion should be a select-then-delete sequence");
        };
        assert!(matches!(
            steps.as_slice(),
            [
                text_editor::Binding::Select(text_editor::Motion::WordLeft),
                text_editor::Binding::Backspace
            ]
        ));

        // And the sequence really removes a word.
        let mut editor = editor_with("Hello beautiful world", 0, 15);
        act(
            &mut editor,
            text_editor::Action::Select(text_editor::Motion::WordLeft),
        );
        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Backspace),
        );

        assert_eq!(editor.lines, vec!["Hello  world".to_owned()]);
    }

    /// `Ctrl+Delete` is its forward mirror.
    #[test]
    fn ctrl_delete_removes_the_next_word() {
        let press = key_press(
            keyboard::Key::Named(keyboard::key::Named::Delete),
            keyboard::Modifiers::CTRL,
        );
        let binding = editor_binding(press, BindingContext::default());

        let Some(text_editor::Binding::Sequence(steps)) = binding else {
            panic!("word deletion should be a select-then-delete sequence");
        };
        assert!(matches!(
            steps.as_slice(),
            [
                text_editor::Binding::Select(text_editor::Motion::WordRight),
                text_editor::Binding::Delete
            ]
        ));

        let mut editor = editor_with("Hello beautiful world", 0, 6);
        act(
            &mut editor,
            text_editor::Action::Select(text_editor::Motion::WordRight),
        );
        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Delete),
        );

        assert_eq!(editor.lines, vec!["Hello  world".to_owned()]);
    }

    /// At the very start of a line there is no word to the left: the line
    /// break is what goes, joining the line onto the one above.
    #[test]
    fn ctrl_backspace_at_the_start_of_a_line_merges_it_upwards() {
        let press = key_press(
            keyboard::Key::Named(keyboard::key::Named::Backspace),
            keyboard::Modifiers::CTRL,
        );
        let binding = editor_binding(
            press,
            BindingContext {
                at_editor_top: true,
                at_line_start: true,
                ..BindingContext::default()
            },
        );

        assert!(matches!(
            binding,
            Some(text_editor::Binding::Custom(Message::MergeWithPrevious))
        ));
    }

    /// With text already selected, word deletion must not throw that
    /// selection away and delete something else instead.
    #[test]
    fn word_deletion_with_a_selection_deletes_the_selection() {
        let ctx = BindingContext {
            has_selection: true,
            ..BindingContext::default()
        };

        for (named, expected) in [
            (keyboard::key::Named::Backspace, "backspace"),
            (keyboard::key::Named::Delete, "delete"),
        ] {
            let press = key_press(keyboard::Key::Named(named), keyboard::Modifiers::CTRL);
            let binding = editor_binding(press, ctx);

            assert!(
                matches!(
                    binding,
                    Some(text_editor::Binding::Backspace | text_editor::Binding::Delete)
                ),
                "ctrl+{expected} with a selection should delete just the selection"
            );
        }
    }

    // -- Mouse drag selection ---------------------------------------------

    /// Pressing a rendered line anchors a drag; entering another line while
    /// the button is held extends the selection over everything between.
    #[test]
    fn dragging_down_across_lines_selects_them() {
        let mut editor = editor_with("one\ntwo\nthree\nfour", 0, 0);

        let _ = editor.update(click(1));
        assert_eq!(editor.drag, Some(Drag { anchor: (1, 0) }));

        let _ = editor.update(Message::DragTo(3));

        assert_eq!(region(&editor), (1, 4));
        assert_eq!(selected(&editor).as_deref(), Some("two\nthree\nfour"));

        // Releasing keeps the selection; only the anchor is forgotten.
        let _ = editor.update(Message::DragEnd);
        assert_eq!(editor.drag, None);
        assert_eq!(selected(&editor).as_deref(), Some("two\nthree\nfour"));
    }

    /// Dragging the other way selects backwards from the anchor.
    #[test]
    fn dragging_up_across_lines_selects_backwards() {
        let mut editor = editor_with("one\ntwo\nthree\nfour", 0, 0);

        let _ = editor.update(click(3));
        let _ = editor.update(Message::DragTo(1));

        assert_eq!(region(&editor), (1, 4));
        assert_eq!(selected(&editor).as_deref(), Some("two\nthree\n"));
    }

    /// Reversing direction mid-drag follows the pointer rather than
    /// accumulating everything it has ever touched.
    #[test]
    fn a_drag_that_turns_around_follows_the_pointer() {
        let mut editor = editor_with("one\ntwo\nthree\nfour", 0, 0);

        let _ = editor.update(click(0));
        let _ = editor.update(Message::DragTo(3));
        let _ = editor.update(Message::DragTo(1));

        assert_eq!(selected(&editor).as_deref(), Some("one\ntwo"));
    }

    /// Pressing and releasing without moving places the cursor and selects
    /// nothing — an ordinary click.
    #[test]
    fn a_click_without_a_drag_selects_nothing() {
        let mut editor = editor_with("one\ntwo\nthree", 0, 0);

        let _ = editor.update(click(2));
        let _ = editor.update(Message::DragEnd);

        assert!(!editor.has_selection());
        assert_eq!(region(&editor), (2, 3));
        assert_eq!(editor.cursor_position(), (2, 0));
    }

    /// Clicking inside an existing selection clears it and puts the cursor
    /// there, the way every text editor behaves.
    #[test]
    fn clicking_clears_an_existing_selection() {
        let mut editor = editor_with("one\ntwo\nthree", 0, 0);

        let _ = editor.update(Message::SelectAll);
        assert!(editor.has_selection());

        // A press inside the raw region is reported by the widget itself.
        act(&mut editor, text_editor::Action::Click(iced::Point::ORIGIN));

        assert!(!editor.has_selection());
        assert!(editor.drag.is_some());
    }

    /// Dragging over a line that no longer exists (the document shrank under
    /// a stale message) must not panic or move the cursor out of bounds.
    #[test]
    fn dragging_past_the_end_of_the_document_is_ignored() {
        let mut editor = editor_with("one\ntwo", 0, 0);

        let _ = editor.update(click(0));
        let _ = editor.update(Message::DragTo(99));

        assert_eq!(region(&editor), (0, 1));
        assert!(!editor.has_selection());
    }

    /// A drag only listens for the pointer while the button is down.
    #[test]
    fn hovering_without_a_drag_changes_nothing() {
        let mut editor = editor_with("one\ntwo\nthree", 0, 0);

        let _ = editor.update(Message::DragTo(2));

        assert_eq!(region(&editor), (0, 1));
        assert!(!editor.has_selection());
    }

    // -- The region tracks the selection ----------------------------------

    /// The region is widened *because* of the selection, so it shrinks back
    /// the moment the selection is gone. Otherwise one `Shift+↓` too many
    /// would leave half the document rendering as raw source forever.
    #[test]
    fn the_region_shrinks_back_when_the_selection_is_dropped() {
        let mut editor = editor_with("one\ntwo\nthree", 0, 0);

        let _ = editor.update(Message::SelectAll);
        assert_eq!(region(&editor), (0, 3));

        // A plain motion collapses the selection onto one of its ends.
        act(
            &mut editor,
            text_editor::Action::Move(text_editor::Motion::Right),
        );

        assert!(!editor.has_selection());
        assert_eq!(region(&editor), (2, 3));
        // The two lines the region gave up are rendered Markdown again.
        assert_eq!(editor.block_sources, vec!["one\ntwo".to_owned()]);
    }

    /// A region held open by a *table* is not a selection region and must
    /// survive a selection being dropped inside it.
    #[test]
    fn a_table_region_is_not_collapsed_by_losing_a_selection() {
        let mut editor = editor_with("| A | B |\n|---|---|\n| 1 | 2 |", 0, 0);
        assert_eq!(region(&editor), (0, 3));

        let _ = editor.update(Message::SelectAll);
        act(
            &mut editor,
            text_editor::Action::Move(text_editor::Motion::Left),
        );

        assert_eq!(region(&editor), (0, 3));
    }

    /// A selection that ends halfway down a table takes the whole table with
    /// it, so the rows left over never render as an invalid fragment.
    #[test]
    fn a_selection_reaching_into_a_table_takes_all_of_it() {
        let mut editor = editor_with("intro\n\n| A |\n|---|\n| 1 |\n| 2 |", 0, 0);

        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Down));
        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Down));

        // Reaching the header pulls in the delimiter and both body rows.
        assert_eq!(region(&editor), (0, 6));
    }

    /// The segment cache and the parsed blocks must stay consistent with a
    /// region that is being widened and shrunk by the selection.
    #[test]
    fn selection_keeps_the_segments_and_blocks_consistent() {
        let mut editor = editor_with("# Head\n\nalpha\nbravo\n\ntail", 2, 0);

        for message in [
            Message::ExtendSelection(text_editor::Motion::Down),
            Message::ExtendSelection(text_editor::Motion::Down),
            Message::ExtendSelection(text_editor::Motion::Up),
            Message::SelectAll,
            Message::EditorAction(text_editor::Action::Move(text_editor::Motion::Left)),
        ] {
            let _ = editor.update(message);
            check_invariants(&editor, 0, 0);
            assert_eq!(editor.block_sources, expected_block_sources(&editor));
            let _element = editor.view();
        }
    }

    /// A drag that starts inside the editor carries on over the rendered
    /// lines around it: the widget reports the press, and the pointer leaving
    /// its bounds is picked up by the line it lands on.
    #[test]
    fn a_drag_started_in_the_editor_continues_over_rendered_lines() {
        let mut editor = editor_with("alpha\nbravo\ncharlie", 0, 0);

        act(&mut editor, text_editor::Action::Click(iced::Point::ORIGIN));
        assert_eq!(editor.drag, Some(Drag { anchor: (0, 0) }));

        let _ = editor.update(Message::DragTo(2));

        assert_eq!(region(&editor), (0, 3));
        assert_eq!(selected(&editor).as_deref(), Some("alpha\nbravo\ncharlie"));
    }

    /// The region must not be shrunk out from under a drag that has, for the
    /// moment, no selection — the pointer is still down.
    #[test]
    fn a_drag_back_to_its_anchor_keeps_the_region_open() {
        let mut editor = editor_with("one\ntwo\nthree", 0, 0);

        let _ = editor.update(click(0));
        let _ = editor.update(Message::DragTo(2));
        assert_eq!(region(&editor), (0, 3));

        // Back to the anchor line: the selection shrinks but the drag goes on.
        let _ = editor.update(Message::DragTo(0));
        assert!(editor.drag.is_some());
        assert_eq!(region(&editor), (0, 1));
    }

    /// Selecting is a move, so it closes the current run of edits: undo must
    /// not roll the typing before a selection into what replaced it.
    #[test]
    fn selecting_ends_the_current_undo_run() {
        let mut editor = editor_with("word", 0, 4);

        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Insert('s')),
        );
        assert_eq!(editor.lines, vec!["words".to_owned()]);

        let _ = editor.update(Message::SelectAll);
        act(
            &mut editor,
            text_editor::Action::Edit(text_editor::Edit::Insert('x')),
        );
        assert_eq!(editor.lines, vec!["x".to_owned()]);

        // First undo takes back only the replacement…
        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines, vec!["words".to_owned()]);
        // …and the second takes back the typing before it.
        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines, vec!["word".to_owned()]);
    }

    /// Copying must not touch the undo history either.
    #[test]
    fn a_selection_alone_is_not_an_edit() {
        let mut editor = editor_with("one\ntwo", 0, 0);

        let _ = editor.update(Message::SelectAll);
        let _ = editor.update(Message::ExtendSelection(text_editor::Motion::Up));

        assert!(!editor.is_dirty);
        assert!(!editor.history.can_undo());
    }

    /// A drag installs the mouse listener that reports the release, and
    /// nothing else does.
    #[test]
    fn the_mouse_listener_is_only_installed_while_dragging() {
        let mut editor = editor_with("one\ntwo", 0, 0);
        assert!(editor.drag.is_none());

        let _ = editor.update(click(1));
        assert!(editor.drag.is_some());

        let _ = editor.update(Message::DragEnd);
        assert!(editor.drag.is_none());
    }

    // -- Find and replace ------------------------------------------------

    fn open_search(editor: &mut MarkdownEditor, query: &str, replace: bool) {
        let _ = editor.update(Message::OpenSearch { replace });
        let _ = editor.update(Message::SearchQueryChanged(query.to_owned()));
    }

    #[test]
    fn ctrl_f_opens_the_find_bar_and_ctrl_h_upgrades_it_to_replace() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("nothing special");
        editor.rebuild();
        assert!(editor.search.is_none());

        let _ = editor.update(Message::OpenSearch { replace: false });
        assert!(!editor.search.as_ref().unwrap().replacing);

        let _ = editor.update(Message::OpenSearch { replace: true });
        assert!(editor.search.as_ref().unwrap().replacing);

        let _ = editor.update(Message::CloseSearch);
        assert!(editor.search.is_none());
    }

    #[test]
    fn searching_activates_and_selects_the_match() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("alpha\n\nbeta target gamma\n\ndelta");
        editor.rebuild();

        open_search(&mut editor, "target", false);

        // The matching line became the raw, editable region...
        assert_eq!(editor.active_start, 2);
        // ...and the match itself is selected, ready to be typed over.
        assert_eq!(editor.active_content.selection().as_deref(), Some("target"));
    }

    #[test]
    fn find_next_and_previous_wrap_through_every_match() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("hit\n\nmiss\n\nhit\n\nhit");
        editor.rebuild();

        open_search(&mut editor, "hit", false);
        assert_eq!(editor.search.as_ref().unwrap().count(), 3);
        assert_eq!(editor.active_start, 0);

        let _ = editor.update(Message::FindNext);
        assert_eq!(editor.active_start, 4);
        let _ = editor.update(Message::FindNext);
        assert_eq!(editor.active_start, 6);
        // Past the last match wraps back to the first.
        let _ = editor.update(Message::FindNext);
        assert_eq!(editor.active_start, 0);
        // And backwards from the first wraps to the last.
        let _ = editor.update(Message::FindPrevious);
        assert_eq!(editor.active_start, 6);
    }

    #[test]
    fn toggling_case_sensitivity_re_searches() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("Foo\n\nfoo\n\nFOO");
        editor.rebuild();

        open_search(&mut editor, "foo", false);
        assert_eq!(editor.search.as_ref().unwrap().count(), 3);

        let _ = editor.update(Message::ToggleSearchCase);
        assert_eq!(editor.search.as_ref().unwrap().count(), 1);
    }

    #[test]
    fn replace_current_changes_only_the_highlighted_match() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("cat\n\ncat\n\ncat");
        editor.rebuild();

        open_search(&mut editor, "cat", true);
        let _ = editor.update(Message::ReplacementChanged(String::from("dog")));
        let _ = editor.update(Message::ReplaceCurrent);

        assert_eq!(editor.lines[0], "dog");
        assert_eq!(editor.lines[2], "cat");
        assert_eq!(editor.lines[4], "cat");
        assert!(editor.is_dirty);
    }

    #[test]
    fn replace_all_changes_every_match_including_repeats_on_one_line() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("cat cat\n\nno match\n\ncat");
        editor.rebuild();

        open_search(&mut editor, "cat", true);
        let _ = editor.update(Message::ReplacementChanged(String::from("dog")));
        let _ = editor.update(Message::ReplaceAll);

        assert_eq!(editor.lines[0], "dog dog");
        assert_eq!(editor.lines[2], "no match");
        assert_eq!(editor.lines[4], "dog");
        assert_eq!(editor.search.as_ref().unwrap().count(), 0);
    }

    /// Replacements of a different length must not corrupt the following
    /// matches on the same line — they are applied back to front for exactly
    /// this reason.
    #[test]
    fn replace_all_handles_replacements_of_a_different_length() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("a-a-a");
        editor.rebuild();

        open_search(&mut editor, "a", true);
        let _ = editor.update(Message::ReplacementChanged(String::from("LONG")));
        let _ = editor.update(Message::ReplaceAll);

        assert_eq!(editor.lines[0], "LONG-LONG-LONG");
    }

    #[test]
    fn replace_all_is_a_single_undo_step() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("x\n\nx\n\nx");
        editor.rebuild();
        let before = editor.lines.clone();

        open_search(&mut editor, "x", true);
        let _ = editor.update(Message::ReplacementChanged(String::from("y")));
        let _ = editor.update(Message::ReplaceAll);
        assert_eq!(editor.lines, vec!["y", "", "y", "", "y"]);

        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines, before);
    }

    /// The raw editor shows a replacement made on its own line, so the next
    /// key press edits the new text rather than writing the old one back —
    /// also once the last match is gone, and nothing is left to reveal.
    #[test]
    fn typing_after_a_replacement_keeps_it() {
        for replace_all in [false, true] {
            let mut editor = editor_with("one cat\nother", 1, 0);
            open_search(&mut editor, "cat", true);
            let _ = editor.update(Message::ReplacementChanged("dog".to_owned()));

            let _ = editor.update(if replace_all {
                Message::ReplaceAll
            } else {
                Message::ReplaceCurrent
            });
            let _ = editor.update(Message::EditorAction(text_editor::Action::Move(
                text_editor::Motion::End,
            )));
            type_text(&mut editor, "!");

            assert_eq!(
                editor.lines,
                ["one dog!", "other"],
                "replace all: {replace_all}"
            );
        }
    }

    #[test]
    fn replace_can_blank_a_line_and_the_blocks_stay_consistent() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("keep\n\ngone\n\nkeep");
        editor.rebuild();

        open_search(&mut editor, "gone", true);
        let _ = editor.update(Message::ReplacementChanged(String::new()));
        let _ = editor.update(Message::ReplaceAll);

        assert_eq!(editor.lines[2], "");
        // Emptying the line turns it into a Blank segment; the cached segments
        // and parsed blocks must both reflect that.
        assert_eq!(editor.block_sources, expected_block_sources(&editor));
    }

    /// Matches are byte offsets into `lines`, so editing must not leave stale
    /// ones behind for a later jump to index with.
    #[test]
    fn editing_the_document_refreshes_the_matches() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("find me\n\nfind me");
        editor.rebuild();

        open_search(&mut editor, "find", false);
        assert_eq!(editor.search.as_ref().unwrap().count(), 2);

        // Delete the whole second line through the normal editing path.
        let _ = editor.update(click(2));
        let _ = editor.update(Message::EditorAction(text_editor::Action::SelectAll));
        let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
            text_editor::Edit::Backspace,
        )));

        assert_eq!(editor.search.as_ref().unwrap().count(), 1);
    }

    #[test]
    fn opening_search_seeds_the_query_from_the_selection() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("seed word");
        editor.rebuild();

        let _ = editor.update(Message::EditorAction(text_editor::Action::SelectAll));
        let _ = editor.update(Message::OpenSearch { replace: false });

        assert_eq!(editor.search.as_ref().unwrap().query, "seed word");
    }

    #[test]
    fn a_query_matching_nothing_leaves_the_document_alone() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("alpha\n\nbeta");
        editor.rebuild();
        let before = editor.lines.clone();

        open_search(&mut editor, "zzz", true);
        assert_eq!(editor.search.as_ref().unwrap().count(), 0);

        let _ = editor.update(Message::ReplacementChanged(String::from("x")));
        let _ = editor.update(Message::ReplaceAll);
        let _ = editor.update(Message::ReplaceCurrent);
        let _ = editor.update(Message::FindNext);

        assert_eq!(editor.lines, before);
        assert!(!editor.is_dirty);
    }

    #[test]
    fn the_search_bar_renders() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("render me");
        editor.rebuild();

        open_search(&mut editor, "render", true);
        let _ = editor.view();
    }

    // -- Per-tag color customization -----------------------------------

    #[test]
    fn color_panel_is_hidden_by_default() {
        let (editor, _t) = MarkdownEditor::new_with_file(None);
        assert!(!editor.show_color_panel);
    }

    #[test]
    fn toggling_color_panel_opens_and_closes() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        let _ = editor.update(Message::ToggleColorPanel);
        assert!(editor.show_color_panel);
        let _ = editor.update(Message::ToggleColorPanel);
        assert!(!editor.show_color_panel);
    }

    #[test]
    fn color_picked_sets_hex_override() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        let red = iced::Color::from_rgb(1.0, 0.0, 0.0);
        let _ = editor.update(Message::ColorPicked(MarkdownTag::H1, red));
        assert_eq!(editor.colors.h1, "#ff0000");
    }

    #[test]
    fn color_hex_changed_updates_override() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        let _ = editor.update(Message::ColorHexChanged(
            MarkdownTag::Strong,
            "#00ff00".into(),
        ));
        assert_eq!(editor.colors.strong, "#00ff00");
    }

    #[test]
    fn color_reset_clears_override() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        let _ = editor.update(Message::ColorPicked(
            MarkdownTag::Link,
            iced::Color::from_rgb(0.0, 0.0, 1.0),
        ));
        assert!(!editor.colors.link.is_empty());
        let _ = editor.update(Message::ColorReset(MarkdownTag::Link));
        assert!(editor.colors.link.is_empty());
    }

    #[test]
    fn reset_all_colors_clears_every_override() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        for tag in MarkdownTag::ALL {
            let _ = editor.update(Message::ColorPicked(
                tag,
                iced::Color::from_rgb(0.5, 0.5, 0.5),
            ));
        }
        let _ = editor.update(Message::ResetAllColors);
        for tag in MarkdownTag::ALL {
            assert!(editor.colors.get(tag).is_empty(), "{tag:?} not cleared");
        }
    }

    #[test]
    fn view_renders_with_color_panel_open() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document(
            "# Title\n\n**bold** and *italic* with `code` and [link](u)\n\n| A | B |\n|---|---|\n| 1 | 2 |",
        );
        editor.rebuild();
        let _ = editor.update(Message::ToggleColorPanel);
        // Setting a few overrides and building the view must not panic.
        let _ = editor.update(Message::ColorPicked(
            MarkdownTag::H1,
            iced::Color::from_rgb(1.0, 0.3, 0.3),
        ));
        let _ = editor.update(Message::ColorHexChanged(
            MarkdownTag::InlineCode,
            "#abcdef".into(),
        ));
        let _ = editor.update(Message::ColorPicked(
            MarkdownTag::TableBorder,
            iced::Color::from_rgb(0.5, 0.5, 0.5),
        ));
        let _ = editor.update(Message::ColorPicked(
            MarkdownTag::TableHeaderBackground,
            iced::Color::from_rgb(0.2, 0.2, 0.3),
        ));
        let _element = editor.view();
    }

    #[test]
    fn color_changes_do_not_trigger_block_rebuild() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("# Title\n\nbody");
        editor.rebuild();
        let before = editor.block_ids.clone();

        // Color messages are pure render-time changes; they must not rebuild.
        let _ = editor.update(Message::ColorPicked(
            MarkdownTag::H1,
            iced::Color::from_rgb(1.0, 0.0, 0.0),
        ));
        let _ = editor.update(Message::ColorHexChanged(
            MarkdownTag::Strong,
            "#00ff00".into(),
        ));
        let _ = editor.update(Message::ColorReset(MarkdownTag::H1));
        let _ = editor.update(Message::ResetAllColors);
        let _ = editor.update(Message::ToggleColorPanel);

        assert_eq!(editor.block_ids, before);
    }

    #[test]
    fn per_level_heading_colors_are_independent() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        let _ = editor.update(Message::ColorPicked(
            MarkdownTag::H1,
            iced::Color::from_rgb(1.0, 0.0, 0.0),
        ));
        let _ = editor.update(Message::ColorPicked(
            MarkdownTag::H2,
            iced::Color::from_rgb(0.0, 1.0, 0.0),
        ));
        // H1 and H2 have independent overrides; H3 is still empty.
        assert_eq!(editor.colors.h1, "#ff0000");
        assert_eq!(editor.colors.h2, "#00ff00");
        assert!(editor.colors.h3.is_empty());
        // Resetting H1 does not affect H2.
        let _ = editor.update(Message::ColorReset(MarkdownTag::H1));
        assert!(editor.colors.h1.is_empty());
        assert_eq!(editor.colors.h2, "#00ff00");
    }

    #[test]
    fn table_color_overrides_round_trip() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        let _ = editor.update(Message::ColorPicked(
            MarkdownTag::TableBorder,
            iced::Color::from_rgb(0.1, 0.2, 0.3),
        ));
        let _ = editor.update(Message::ColorHexChanged(
            MarkdownTag::TableHeaderText,
            "#ffaa00".into(),
        ));
        assert_eq!(editor.colors.table_border, "#1a334d");
        assert_eq!(editor.colors.table_header_text, "#ffaa00");
        assert!(editor.colors.table_header_background.is_empty());
    }

    // -- Settings -------------------------------------------------------

    #[test]
    fn the_settings_panel_is_hidden_by_default() {
        let (editor, _t) = MarkdownEditor::new_with_file(None);
        assert!(!editor.show_settings_panel);
    }

    #[test]
    fn toggling_the_settings_panel_opens_and_closes_it() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);

        let _ = editor.update(Message::ToggleSettingsPanel);
        assert!(editor.show_settings_panel);

        let _ = editor.update(Message::ToggleSettingsPanel);
        assert!(!editor.show_settings_panel);
    }

    /// The color panel is reached *through* the settings panel, so closing the
    /// settings panel must not leave it orphaned on screen.
    #[test]
    fn closing_the_settings_panel_also_closes_the_color_panel() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);

        let _ = editor.update(Message::ToggleSettingsPanel);
        let _ = editor.update(Message::ToggleColorPanel);
        assert!(editor.show_color_panel);

        let _ = editor.update(Message::ToggleSettingsPanel);
        assert!(!editor.show_settings_panel);
        assert!(!editor.show_color_panel);
    }

    #[test]
    fn chrome_toggles_flip_the_matching_preference() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        assert!(editor.settings.show_toolbar);
        assert!(editor.settings.show_status_bar);

        let _ = editor.update(Message::ToggleToolbar);
        assert!(!editor.settings.show_toolbar);
        // …and only that one.
        assert!(editor.settings.show_status_bar);

        let _ = editor.update(Message::ToggleStatusBar);
        assert!(!editor.settings.show_status_bar);
    }

    /// Both are `Option<Element>` in `view`, so a hidden one must not panic or
    /// take the document with it.
    #[test]
    fn the_view_renders_with_every_combination_of_chrome() {
        for (toolbar, status_bar) in [(true, true), (true, false), (false, true), (false, false)] {
            let (mut editor, _t) = MarkdownEditor::new_with_file(None);
            editor.settings.show_toolbar = toolbar;
            editor.settings.show_status_bar = status_bar;
            let _element = editor.view();
        }
    }

    #[test]
    fn the_view_renders_with_the_settings_panel_open() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        let _ = editor.update(Message::ToggleSettingsPanel);
        // The element borrows the editor, so it must be dropped before the next
        // `update`.
        drop(editor.view());

        // …including with the color panel stacked below it.
        let _ = editor.update(Message::ToggleColorPanel);
        drop(editor.view());
    }

    #[test]
    fn the_theme_is_taken_from_the_settings() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        assert_eq!(editor.theme(), DEFAULT_THEME);

        let _ = editor.update(Message::ThemeChanged(Theme::Dracula));
        assert_eq!(editor.settings.theme, Theme::Dracula);
        assert_eq!(editor.theme(), Theme::Dracula);
    }

    /// The offsets table is the sole offset↔line mapping, so a size change that
    /// skipped `remeasure` would leave the render window computed against the
    /// old scale — the failure mode that fills the viewport with blank spacers.
    #[test]
    fn changing_the_font_size_remeasures_the_document() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("alpha\nbeta\ngamma");
        editor.rebuild();
        let base_height = *editor.line_offsets.last().unwrap();

        let _ = editor.update(Message::FontSizeChanged(FontSize(32)));
        let doubled = *editor.line_offsets.last().unwrap();
        assert_eq!(doubled, base_height * 2.0);

        let _ = editor.update(Message::FontSizeChanged(FontSize(8)));
        let halved = *editor.line_offsets.last().unwrap();
        assert_eq!(halved, base_height / 2.0);

        assert_eq!(editor.line_offsets.len(), editor.lines.len() + 1);
    }

    /// A narrower measure wraps long lines more often, so the document gets
    /// taller — the offsets must follow.
    #[test]
    fn narrowing_the_text_width_remeasures_the_document() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document(&"x".repeat(600));
        editor.rebuild();
        let full_width = *editor.line_offsets.last().unwrap();

        let _ = editor.update(Message::WrapWidthChanged(WrapWidth(640)));
        let narrowed = *editor.line_offsets.last().unwrap();

        assert!(
            narrowed > full_width,
            "narrowing to 640 px should wrap more, got {narrowed} vs {full_width}"
        );
    }

    #[test]
    fn resetting_settings_restores_defaults_but_keeps_the_remembered_file() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.settings.last_file = Some(PathBuf::from("/tmp/notes.md"));

        let _ = editor.update(Message::ToggleToolbar);
        let _ = editor.update(Message::ThemeChanged(Theme::Dracula));
        let _ = editor.update(Message::FontSizeChanged(FontSize(24)));
        let _ = editor.update(Message::WrapWidthChanged(WrapWidth(800)));
        let _ = editor.update(Message::ToggleRestoreSession);

        let _ = editor.update(Message::ResetSettings);

        assert_eq!(
            editor.settings,
            Settings {
                last_file: Some(PathBuf::from("/tmp/notes.md")),
                ..Settings::default()
            }
        );
    }

    /// Preferences restyle and re-measure, but the Markdown is unchanged: a
    /// rebuild here would re-parse the whole document on every click.
    ///
    /// Block ids are handed out afresh by every parse, so unchanged ids prove
    /// that nothing was re-parsed — not merely re-parsed into the same text.
    #[test]
    fn settings_changes_do_not_trigger_block_rebuild() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("# Title\n\nbody");
        editor.rebuild();
        let before = editor.block_ids.clone();

        for message in [
            Message::ToggleSettingsPanel,
            Message::ToggleToolbar,
            Message::ToggleStatusBar,
            Message::FontSizeChanged(FontSize(20)),
            Message::WrapWidthChanged(WrapWidth(800)),
            Message::ToggleRestoreSession,
            Message::ThemeChanged(Theme::Dracula),
            Message::ResetSettings,
            Message::Dismiss,
        ] {
            let _ = editor.update(message.clone());
            assert_eq!(
                editor.block_ids, before,
                "{message:?} must not re-parse the document"
            );
        }
    }

    #[test]
    fn escape_closes_the_innermost_open_panel() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        let _ = editor.update(Message::ToggleSettingsPanel);
        let _ = editor.update(Message::ToggleColorPanel);
        let _ = editor.update(Message::OpenSearch { replace: false });

        // Search first: it holds focus and is what the user is typing into.
        let _ = editor.update(Message::Dismiss);
        assert!(editor.search.is_none());
        assert!(editor.show_color_panel);
        assert!(editor.show_settings_panel);

        let _ = editor.update(Message::Dismiss);
        assert!(!editor.show_color_panel);
        assert!(editor.show_settings_panel);

        let _ = editor.update(Message::Dismiss);
        assert!(!editor.show_settings_panel);

        // Idempotent with nothing open.
        let _ = editor.update(Message::Dismiss);
        assert!(!editor.show_settings_panel);
    }

    /// The global key listener exists only while some panel needs `Esc`;
    /// ordinary editing must not install one.
    #[test]
    fn the_key_listener_is_only_installed_while_a_panel_is_open() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        assert!(!editor.needs_key_listener());

        let _ = editor.update(Message::ToggleSettingsPanel);
        assert!(editor.needs_key_listener());

        let _ = editor.update(Message::ToggleSettingsPanel);
        assert!(!editor.needs_key_listener());

        let _ = editor.update(Message::OpenSearch { replace: false });
        assert!(editor.needs_key_listener());
    }

    /// A `Ctrl`-modified character key press, as the active editor sees it.
    fn ctrl_key_press(character: &str) -> text_editor::KeyPress {
        text_editor::KeyPress {
            key: keyboard::Key::Character(character.into()),
            modified_key: keyboard::Key::Character(character.into()),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            modifiers: keyboard::Modifiers::CTRL,
            text: None,
            status: text_editor::Status::Active,
        }
    }

    /// The only way back to the settings once the toolbar is hidden.
    #[test]
    fn ctrl_comma_opens_the_settings_panel() {
        let binding = editor_binding(ctrl_key_press(","), BindingContext::default());

        assert!(matches!(
            binding,
            Some(text_editor::Binding::Custom(Message::ToggleSettingsPanel))
        ));
    }

    // -- Visual / editor mode -------------------------------------------

    /// Editor mode is what the editor opens in, so nothing about the existing
    /// behaviour changes until the user asks for it.
    #[test]
    fn the_editor_starts_in_editor_mode() {
        let (editor, _t) = MarkdownEditor::new_with_file(None);

        assert_eq!(editor.settings.mode, settings::Mode::Editor);
        assert_eq!(editor.active_region(), Some((0, 1)));
        assert!(editor.segments.contains(&Segment::Active));
    }

    /// The point of visual mode: no line is raw, so the cursor's line renders
    /// exactly like its neighbours — one block covers them all.
    #[test]
    fn visual_mode_renders_the_cursors_line_like_every_other() {
        let mut editor = editor_with("alpha\nbeta\ngamma", 1, 0);
        assert!(editor.segments.contains(&Segment::Active));

        let _ = editor.update(Message::ToggleMode);

        assert_eq!(editor.settings.mode, settings::Mode::Visual);
        assert_eq!(editor.active_region(), None);
        assert_eq!(editor.segments, vec![Segment::Block { start: 0, end: 3 }]);
        assert_eq!(editor.block_sources, vec!["alpha\nbeta\ngamma"]);
    }

    /// And back: editor mode is today's behaviour, down to the region the
    /// cursor was left in.
    #[test]
    fn returning_to_editor_mode_restores_the_raw_region() {
        let mut editor = editor_with("alpha\nbeta\ngamma", 1, 2);

        let _ = editor.update(Message::ToggleMode);
        let _ = editor.update(Message::ToggleMode);

        assert_eq!(editor.settings.mode, settings::Mode::Editor);
        assert_eq!(editor.active_region(), Some((1, 2)));
        assert_eq!(
            editor.segments,
            vec![
                Segment::Block { start: 0, end: 1 },
                Segment::Active,
                Segment::Block { start: 2, end: 3 },
            ]
        );
        assert_eq!(editor.cursor_position(), (1, 2));
        assert_eq!(editor.block_sources, vec!["alpha", "gamma"]);
    }

    /// A table is one region in editor mode and one block in visual mode;
    /// either way it must never be split into fragments.
    #[test]
    fn visual_mode_keeps_a_table_whole() {
        let mut editor = editor_with("| A | B |\n|---|---|\n| 1 | 2 |", 1, 0);
        assert_eq!(editor.active_region(), Some((0, 3)));

        let _ = editor.update(Message::ToggleMode);

        assert_eq!(editor.segments, vec![Segment::Block { start: 0, end: 3 }]);
    }

    /// Visual mode has no editor in the tree to hit-test into, so a stray
    /// press must not plant a caret nobody can see.
    #[test]
    fn visual_mode_ignores_clicks_and_drags() {
        let mut editor = editor_with("alpha\nbeta\ngamma", 0, 0);
        let _ = editor.update(Message::ToggleMode);

        let _ = editor.update(click(2));
        let _ = editor.update(Message::DragTo(2));

        assert_eq!(editor.cursor_position(), (0, 0));
        assert!(editor.drag.is_none());
        assert_eq!(editor.segments, vec![Segment::Block { start: 0, end: 3 }]);
    }

    /// The formatting toolbar is greyed out in visual mode; the message it
    /// would have sent must be inert if it arrives anyway.
    #[test]
    fn visual_mode_does_not_format() {
        let mut editor = editor_with("alpha", 0, 0);
        let _ = editor.update(Message::ToggleMode);

        let _ = editor.update(Message::Format(FormatAction::Bold));

        assert_eq!(editor.document_text(), "alpha");
        assert!(!editor.is_dirty);
    }

    /// Search replaces text through the raw region, which visual mode drops.
    #[test]
    fn entering_visual_mode_closes_the_search_bar() {
        let mut editor = editor_with("alpha\nbeta", 0, 0);
        let _ = editor.update(Message::OpenSearch { replace: true });
        assert!(editor.search.is_some());

        let _ = editor.update(Message::ToggleMode);

        assert!(editor.search.is_none());
    }

    /// Nothing holds the keyboard in visual mode, so the keys it answers to —
    /// `Ctrl+E` above all, the way back — have to be listened for globally.
    #[test]
    fn visual_mode_installs_a_key_listener() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        assert!(editor.is_editing());

        let _ = editor.update(Message::ToggleMode);
        assert!(!editor.is_editing());

        let _ = editor.update(Message::ToggleMode);
        assert!(editor.is_editing());
    }

    /// `Ctrl+E` leaves editor mode from the editor's own key binding; the
    /// listener above brings it back.
    #[test]
    fn ctrl_e_toggles_the_mode() {
        let binding = editor_binding(ctrl_key_press("e"), BindingContext::default());

        assert!(matches!(
            binding,
            Some(text_editor::Binding::Custom(Message::ToggleMode))
        ));
    }

    /// Visual mode has no caret to move, so the navigation keys move the view.
    /// `Ctrl+E` is the way back, and it has to come from here: there is no
    /// focused widget left to carry the binding.
    #[test]
    fn visual_mode_keys_scroll_the_document() {
        use keyboard::key::Named;

        let plain = keyboard::Modifiers::empty();
        let ctrl = keyboard::Modifiers::CTRL;
        let named = keyboard::Key::Named;

        assert!(matches!(
            visual_key(keyboard::Key::Character("e"), ctrl),
            Some(Message::ToggleMode)
        ));

        for (key, modifiers, expected) in [
            (named(Named::ArrowUp), plain, ScrollStep::LineUp),
            (named(Named::ArrowDown), plain, ScrollStep::LineDown),
            (named(Named::PageUp), plain, ScrollStep::PageUp),
            (named(Named::PageDown), plain, ScrollStep::PageDown),
            (named(Named::Space), plain, ScrollStep::PageDown),
            (named(Named::Home), plain, ScrollStep::Start),
            (named(Named::End), plain, ScrollStep::End),
            // `Ctrl` with a vertical key jumps to the document's edge, as it
            // does while editing.
            (named(Named::ArrowUp), ctrl, ScrollStep::Start),
            (named(Named::ArrowDown), ctrl, ScrollStep::End),
        ] {
            let actual = visual_key(key.clone(), modifiers);
            assert!(
                matches!(&actual, Some(Message::ScrollDocument(step)) if *step == expected),
                "{key:?} with {modifiers:?} gave {actual:?}"
            );
        }

        // Typing is not an edit in visual mode; it is not anything.
        assert!(visual_key(keyboard::Key::Character("a"), plain).is_none());
        assert!(visual_key(named(Named::Backspace), plain).is_none());
    }

    #[test]
    fn the_mode_is_remembered_across_restarts() {
        let config = Config {
            settings: Settings {
                mode: settings::Mode::Visual,
                ..Settings::default()
            },
            colors: MarkdownColors::default(),
        };

        let (editor, _t) = MarkdownEditor::new(config, None);

        assert!(!editor.is_editing());
        assert_eq!(editor.active_region(), None);
        assert!(!editor.segments.contains(&Segment::Active));
    }

    /// Opening a file must not drop back into editor mode: `load_document`
    /// replaces the document, never the preferences.
    #[test]
    fn a_file_opens_in_visual_mode_when_the_preference_says_so() {
        let config = Config {
            settings: Settings {
                mode: settings::Mode::Visual,
                ..Settings::default()
            },
            colors: MarkdownColors::default(),
        };

        let (mut editor, _t) = MarkdownEditor::new(config, Some(PathBuf::from("/tmp/arg.md")));
        let _ = editor.update(Message::FileOpened(Ok((
            PathBuf::from("/tmp/arg.md"),
            Arc::new(String::from("# Opened\n\nbody")),
        ))));

        assert_eq!(editor.current_file, Some(PathBuf::from("/tmp/arg.md")));
        assert!(!editor.is_editing());
        assert_eq!(editor.active_region(), None);
        assert!(!editor.segments.contains(&Segment::Active));
    }

    /// The view is built from the segments and the dense block vectors; in
    /// visual mode there is no editor child to force into the window either.
    #[test]
    fn the_view_renders_in_visual_mode() {
        let mut editor = editor_with("# Title\n\nbody\n\n```rust\nfn a() {}\n```", 2, 0);
        let _ = editor.update(Message::ToggleMode);

        let _element = editor.view();
    }

    #[test]
    fn the_status_bar_reports_the_mode_instead_of_a_caret() {
        let mut editor = editor_with("alpha\nbeta", 1, 2);
        let _ = editor.update(Message::ToggleMode);

        // The caret is still where it was; it is simply not drawn.
        assert_eq!(editor.cursor_position(), (1, 2));
        let _element = editor.view();
    }

    // -- Session restore ------------------------------------------------

    #[test]
    fn a_command_line_path_wins_over_the_restored_session() {
        let config = Config {
            settings: Settings {
                restore_session: true,
                last_file: Some(PathBuf::from("/tmp/remembered.md")),
                ..Settings::default()
            },
            colors: MarkdownColors::default(),
        };

        // Both paths produce a load task, so assert on the state that decided
        // it: the editor is loading either way, and `file_to_restore` is what
        // `new` falls back to only when no argument was given.
        let (editor, _t) = MarkdownEditor::new(config.clone(), Some(PathBuf::from("/tmp/cli.md")));
        assert!(editor.is_loading);
        assert_eq!(
            editor.settings.file_to_restore(),
            Some(PathBuf::from("/tmp/remembered.md"))
        );

        let (restored, _t) = MarkdownEditor::new(config, None);
        assert!(restored.is_loading);
    }

    #[test]
    fn nothing_is_restored_when_the_preference_is_off() {
        let config = Config {
            settings: Settings {
                restore_session: false,
                last_file: Some(PathBuf::from("/tmp/remembered.md")),
                ..Settings::default()
            },
            colors: MarkdownColors::default(),
        };

        let (editor, _t) = MarkdownEditor::new(config, None);

        assert!(!editor.is_loading);
        assert_eq!(editor.current_file, None);
    }

    /// The path is remembered whether or not restore is enabled, so turning the
    /// preference on takes effect immediately rather than one file later.
    #[test]
    fn opening_a_file_remembers_it_even_with_restore_disabled() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        assert!(!editor.settings.restore_session);

        let _ = editor.update(Message::FileOpened(Ok((
            PathBuf::from("/tmp/opened.md"),
            Arc::new(String::from("# Opened")),
        ))));

        assert_eq!(
            editor.settings.last_file,
            Some(PathBuf::from("/tmp/opened.md"))
        );
    }

    #[test]
    fn saving_a_file_remembers_it() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);

        let _ = editor.update(Message::FileSaved(Ok(PathBuf::from("/tmp/saved.md"))));

        assert_eq!(
            editor.settings.last_file,
            Some(PathBuf::from("/tmp/saved.md"))
        );
    }

    // -- Incremental block rebuild --------------------------------------

    /// The block sources a *full* rebuild would produce, used as the ground
    /// truth the incremental [`MarkdownEditor::rebuild_blocks`] must match.
    fn expected_block_sources(editor: &MarkdownEditor) -> Vec<String> {
        walked_segments(editor)
            .into_iter()
            .filter_map(|segment| match segment {
                Segment::Block { start, end } => Some(editor.lines[start..end].join("\n")),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn diff_rebuild_keeps_block_sources_correct_while_navigating() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("# A\n\npara one\n\npara two\n\n| H |\n|---|\n| x |\n\nlast");
        editor.rebuild();
        assert_eq!(editor.block_sources, expected_block_sources(&editor));

        // Walk the active line all the way down (crossing the table) and back up.
        // In view mode the blocks stay whole regardless of the cursor, and every
        // step rebuilds via the prefix/suffix reuse path, so this proves the reuse
        // never desyncs the cached parses from the real document.
        for _ in 0..15 {
            let _ = editor.update(Message::MoveDown);
            assert_eq!(editor.block_sources, expected_block_sources(&editor));
        }
        for _ in 0..15 {
            let _ = editor.update(Message::MoveUp);
            assert_eq!(editor.block_sources, expected_block_sources(&editor));
        }
    }

    #[test]
    fn diff_rebuild_correct_after_structural_edits() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("alpha\n\nbeta\n\ngamma\n\ndelta");
        editor.rebuild();

        // Splits, merges and line insertions all shift block indices; the reuse
        // logic must still reproduce a from-scratch parse each time.
        editor.activate_line(2, 2); // inside "beta"
        let _ = editor.update(Message::SplitLine);
        assert_eq!(editor.block_sources, expected_block_sources(&editor));

        let _ = editor.update(Message::MergeWithPrevious);
        assert_eq!(editor.block_sources, expected_block_sources(&editor));

        editor.activate_line(0, 0);
        let _ = editor.update(Message::MergeWithNext);
        assert_eq!(editor.block_sources, expected_block_sources(&editor));
    }

    #[test]
    fn typing_keeps_blocks_consistent_without_rebuilding() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("# Title\n\nbody\n\nmore");
        editor.activate_line(2, 0); // "body"
        editor.rebuild();

        // Enter Insert mode: the cursor line ("body") is split out as the raw
        // editor, so the surrounding blocks no longer include it.
        let blocks_at_insert = editor.block_sources.clone();
        assert_eq!(
            blocks_at_insert,
            vec!["# Title".to_owned(), "more".to_owned()]
        );

        // Typing must NOT trigger a block rebuild…
        for ch in "XYZ".chars() {
            let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
                text_editor::Edit::Insert(ch),
            )));
        }

        // …yet the cached parses stay exactly correct, because an edit only ever
        // rewrites the active region (rendered straight from the raw editor),
        // never the surrounding blocks.
        assert_eq!(editor.block_sources, blocks_at_insert);
        assert_eq!(editor.block_sources, expected_block_sources(&editor));
        assert_eq!(editor.lines[2], "XYZbody");
        // The view still builds correctly against the un-rebuilt blocks.
        let _ = editor.view();
    }

    #[test]
    #[ignore = "manual perf measurement: cargo test -- --ignored --nocapture"]
    fn measure_navigation_and_typing_cost_on_a_large_document() {
        use std::time::Instant;

        // ~6,000 lines: headings, paragraphs and fenced code blocks.
        let mut doc = String::new();
        for i in 0..1000 {
            doc.push_str(&format!("## Section {i}\n\n"));
            doc.push_str("Lorem ipsum dolor sit amet, consectetur adipiscing elit.\n\n");
            doc.push_str("```rust\nfn main() { let _ = 1 + 1; }\n```\n\n");
        }

        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document(&doc);
        editor.rebuild();
        let lines = editor.lines.len();

        // Navigation hot path: each step rebuilds the blocks via the diff.
        let nav_steps = 1000;
        let t = Instant::now();
        for _ in 0..nav_steps {
            let _ = editor.update(Message::MoveDown);
        }
        let nav = t.elapsed();

        // Typing hot path: each edit now skips the block rebuild entirely.
        let type_steps = 1000;
        let t = Instant::now();
        for _ in 0..type_steps {
            let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
                text_editor::Edit::Insert('a'),
            )));
        }
        let typing = t.elapsed();

        println!("[{lines} lines]");
        println!(
            "  navigation rebuild: {nav:?} total, {:?}/step",
            nav / nav_steps
        );
        println!(
            "  typing update:      {typing:?} total, {:?}/step",
            typing / type_steps
        );
    }

    #[test]
    #[ignore = "manual perf measurement: cargo test -- --ignored --nocapture"]
    fn measure_view_cost_on_a_large_document() {
        use std::time::Instant;

        // ~6,000 lines: headings, paragraphs and fenced code blocks.
        let mut doc = String::new();
        for i in 0..1000 {
            doc.push_str(&format!("## Section {i}\n\n"));
            doc.push_str("Lorem ipsum dolor sit amet, consectetur adipiscing elit.\n\n");
            doc.push_str("```rust\nfn main() {{ let _ = 1 + 1; }}\n```\n\n");
        }

        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document(&doc);
        editor.rebuild();
        let lines = editor.lines.len();

        // view() while typing (rebuild skipped, but view runs every frame):
        // the realistic per-keystroke cost the user feels.
        let view_steps = 200;
        let t = Instant::now();
        for _ in 0..view_steps {
            let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
                text_editor::Edit::Insert('a'),
            )));
            let _element = editor.view();
        }
        let typing_view = t.elapsed();

        // view() while navigating (rebuild + view each step).
        let nav_view_steps = 200;
        let t = Instant::now();
        for _ in 0..nav_view_steps {
            let _ = editor.update(Message::MoveDown);
            let _element = editor.view();
        }
        let nav_view = t.elapsed();

        // Pure view() cost (no state change) — what scrolling pays per frame.
        let pure_view_steps = 200;
        let t = Instant::now();
        for _ in 0..pure_view_steps {
            let _element = editor.view();
        }
        let pure_view = t.elapsed();

        println!("[{lines} lines]");
        println!(
            "  typing + view:  {typing_view:?} total, {:?}/step",
            typing_view / view_steps
        );
        println!(
            "  nav + view:     {nav_view:?} total, {:?}/step",
            nav_view / nav_view_steps
        );
        println!(
            "  pure view:      {pure_view:?} total, {:?}/step",
            pure_view / pure_view_steps
        );
    }

    #[test]
    #[ignore = "manual perf measurement: cargo test -- --ignored --nocapture"]
    fn measure_windowed_view_speedup_on_a_large_document() {
        use std::time::Instant;

        // ~6,000 lines: headings, paragraphs and fenced code blocks.
        let mut doc = String::new();
        for i in 0..1000 {
            doc.push_str(&format!("## Section {i}\n\n"));
            doc.push_str("Lorem ipsum dolor sit amet, consectetur adipiscing elit.\n\n");
            doc.push_str("```rust\nfn main() {{ let _ = 1 + 1; }}\n```\n\n");
        }

        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document(&doc);
        editor.rebuild();
        let lines = editor.lines.len();

        // FULL render: no viewport reported yet (first frame / tests).
        editor.viewport = None;
        let steps = 300;
        let t = Instant::now();
        for _ in 0..steps {
            let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
                text_editor::Edit::Insert('a'),
            )));
            let _element = editor.view();
        }
        let full = t.elapsed();

        // WINDOWED render: viewport reported around the middle of the document
        // (where the cursor is), as the scrollable would after first layout.
        editor.viewport = Some((40_000.0, 1_000.0));
        let t = Instant::now();
        for _ in 0..steps {
            let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
                text_editor::Edit::Insert('a'),
            )));
            let _element = editor.view();
        }
        let windowed = t.elapsed();

        println!("[{lines} lines]");
        println!("  full render:     {full:?} total, {:?}/step", full / steps);
        println!(
            "  windowed render: {windowed:?} total, {:?}/step",
            windowed / steps
        );
        println!(
            "  speedup: {:.1}x",
            full.as_secs_f64() / windowed.as_secs_f64()
        );
    }

    #[test]
    #[ignore = "manual perf measurement: cargo test -- --ignored --nocapture"]
    fn measure_table_navigation_cost() {
        use std::time::Instant;

        // A document with a 20-row table — exercises the worst case for
        // navigation: the active region spans the whole table, so every j/k
        // recreates the editor content and rebuilds the surrounding blocks.
        let mut doc = String::from("intro line\n\n");
        doc.push_str("| Col A | Col B | Col C |\n");
        doc.push_str("|-------|-------|-------|\n");
        for i in 0..20 {
            doc.push_str(&format!("| a{i} | b{i} | c{i} |\n"));
        }
        doc.push_str("\ntrailing paragraph\n");

        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document(&doc);
        editor.rebuild();

        // Navigate DOWN into and through the table, rebuilding the blocks and
        // the view at each step.
        let steps = 30;
        let t = Instant::now();
        for _ in 0..steps {
            let _ = editor.update(Message::MoveDown);
            let _element = editor.view();
        }
        let down = t.elapsed();

        // Navigate back UP through the table.
        let t = Instant::now();
        for _ in 0..steps {
            let _ = editor.update(Message::MoveUp);
            let _element = editor.view();
        }
        let up = t.elapsed();

        println!("[table: 22 rows]");
        println!("  j (down):  {down:?} total, {:?}/step", down / steps);
        println!("  k (up):    {up:?} total, {:?}/step", up / steps);
    }

    /// The original `HashMap`-keyed rebuild, kept here only to benchmark the new
    /// prefix/suffix rebuild against it on identical inputs.
    fn rebuild_blocks_hashmap(
        lines: &[String],
        active_start: usize,
        active_end: usize,
        old_sources: &mut Vec<String>,
        old_blocks: &mut Vec<markdown::Content>,
    ) {
        let mut cache: std::collections::HashMap<String, markdown::Content> =
            old_sources.drain(..).zip(old_blocks.drain(..)).collect();
        let mut sources = Vec::new();
        let mut blocks = Vec::new();
        for segment in segments(lines, active_start, active_end) {
            if let Segment::Block { start, end } = segment {
                let source = lines[start..end].join("\n");
                let content = cache
                    .remove(&source)
                    .unwrap_or_else(|| markdown::Content::parse(&source));
                sources.push(source);
                blocks.push(content);
            }
        }
        // Mirror the original active-region parse (with cache reuse).
        let source = lines[active_start..active_end].join("\n");
        let _ = cache
            .remove(&source)
            .unwrap_or_else(|| markdown::Content::parse(&source));
        *old_sources = sources;
        *old_blocks = blocks;
    }

    #[test]
    #[ignore = "manual perf measurement: cargo test -- --ignored --nocapture"]
    fn measure_open_cost_on_a_code_heavy_document() {
        use std::time::Instant;

        // A code-heavy document in several languages — the worst case for the
        // open path, which used to syntax-highlight every code block while
        // *parsing* (work the renderer throws away and redoes per theme). The
        // parser no longer highlights, so load+rebuild is just pulldown-cmark.
        let mut doc = String::new();
        for i in 0..120 {
            doc.push_str(&format!("## Section {i}\n\n"));
            doc.push_str("Lorem ipsum dolor sit amet, consectetur adipiscing elit.\n\n");
            doc.push_str("```rust\nfn main() { let _ = 1 + 1; }\n```\n\n");
            doc.push_str("```python\ndef f(x):\n    return x + 1\n```\n\n");
            doc.push_str("```javascript\nconst a = () => 42;\n```\n\n");
        }

        let (mut editor, _t) = MarkdownEditor::new_with_file(None);

        // "Open" cost: load + first rebuild — exactly what FileOpened does.
        let t = Instant::now();
        editor.load_document(&doc);
        editor.rebuild();
        let open = t.elapsed();
        let lines = editor.lines.len();
        let blocks = editor.blocks.len();

        // The real app warms the syntax highlighter on a background thread at
        // startup (see `markdown_view::warm_up`); without it this measures
        // syntect compiling Rust/Python/JavaScript from scratch on the UI
        // thread, which is exactly the stall the warm-up exists to remove.
        let t = Instant::now();
        {
            let _e = editor.view();
        }
        let cold_view = t.elapsed();

        // First frame as the app actually renders it: languages already
        // compiled, and `load_document` seeded a top-of-document viewport so
        // only the lines near the top are built and highlighted (not every code
        // block in the file).
        editor.code_cache.borrow_mut().clear();
        let t = Instant::now();
        {
            let _e = editor.view();
        }
        let first_view = t.elapsed();

        // For contrast: rendering the WHOLE document (the old first-frame
        // behaviour) still pays the cold syntax-highlight of every code block.
        editor.viewport = None;
        let t = Instant::now();
        {
            let _e = editor.view();
        }
        let full_view = t.elapsed();

        println!("[{lines} lines, {blocks} blocks]");
        println!("  open (load+rebuild):              {open:?}");
        println!("  first view (languages uncompiled): {cold_view:?}");
        println!("  first view (warmed, as shipped):   {first_view:?}");
        println!("  full view (every block, forced):   {full_view:?}");
    }

    #[test]
    #[ignore = "manual perf measurement: cargo test -- --ignored --nocapture"]
    fn bench_rebuild_old_vs_new() {
        use std::time::Instant;

        let mut doc = String::new();
        for i in 0..120 {
            doc.push_str(&format!("## Section {i}\n\n"));
            doc.push_str("Lorem ipsum dolor sit amet, consectetur adipiscing elit.\n\n");
            doc.push_str("```rust\nfn main() { let _ = 1 + 1; }\n```\n\n");
        }

        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document(&doc);
        let lines = editor.lines.len();
        let positions: Vec<usize> = (0..120).collect();

        // NEW: the prefix/suffix diff rebuild.
        editor.active_start = 0;
        editor.active_end = 1;
        editor.rebuild();
        let t = Instant::now();
        for &p in &positions {
            editor.active_start = p;
            editor.active_end = p + 1;
            editor.rebuild();
        }
        let new_time = t.elapsed();

        // OLD: the HashMap-keyed rebuild, on the same sequence of positions.
        let mut os = Vec::new();
        let mut ob = Vec::new();
        rebuild_blocks_hashmap(&editor.lines, 0, 1, &mut os, &mut ob);
        let t = Instant::now();
        for &p in &positions {
            rebuild_blocks_hashmap(&editor.lines, p, p + 1, &mut os, &mut ob);
        }
        let old_time = t.elapsed();

        let steps = positions.len() as u32;
        println!("[{lines} lines, {steps} navigation rebuilds]");
        println!(
            "  OLD (hashmap):       {old_time:?} total, {:?}/step",
            old_time / steps
        );
        println!(
            "  NEW (prefix/suffix): {new_time:?} total, {:?}/step",
            new_time / steps
        );
        println!(
            "  speedup: {:.1}x",
            old_time.as_secs_f64() / new_time.as_secs_f64()
        );
    }
}
