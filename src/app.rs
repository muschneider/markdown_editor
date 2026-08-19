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

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Arc;

use iced::keyboard;
use iced::widget::{
    Column, button, column, container, markdown, mouse_area, pick_list, row, scrollable, space,
    text, text_editor,
};
use iced::{Element, Fill, Font, Length, Subscription, Task, Theme, window};

use crate::color_picker;
use crate::config::{self, Config};
use crate::file_ops::{self, FileError};
use crate::history::{self, History};
use crate::icons;
use crate::markdown_view;
use crate::search::{self, Search};
use crate::settings::{self, FontSize, Settings, WrapWidth};
use crate::theme as app_theme;
use crate::theme::{MarkdownColors, MarkdownTag};
use crate::toolbar::{self, FormatAction};

/// Default content shown when the editor starts with no file.
const WELCOME_MD: &str = r#"# Welcome to Markdown Editor

This editor renders Markdown **inline**: the document is shown rendered, except
the line your cursor is on — that one shows the raw source so you can edit it.

Just start typing. Click anywhere to move the cursor there.

## Shortcuts

| Keys                | Action                        |
|---------------------|-------------------------------|
| `Ctrl+N O S`        | new / open / save             |
| `Ctrl+Z`  `Ctrl+Y`  | undo / redo                   |
| `Ctrl+B`  `Ctrl+I`  | bold / italic                 |
| `Ctrl+←/→`          | move by word                  |
| `Home`  `End`       | start / end of line           |
| `PgUp`  `PgDn`      | move a page                   |
| `Ctrl+Home`  `Ctrl+End` | start / end of document   |

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

/// Number of lines moved by `PgUp`/`PgDown` (a "page").
const PAGE_LINES: usize = 15;

/// Rough pixel height of one rendered line at [`settings::BASE_FONT_SIZE`], used
/// to turn the scroll offset into a line range for windowed rendering. Headings
/// are a bit taller, code lines a bit shorter, and wrapped paragraphs span
/// several visual lines — but [`estimate_segment_height`] accounts for wrapping
/// and [`SCROLL_OVERSCAN_LINES`] absorbs the rest, so a placeholder never shows
/// on screen.
const EST_LINE_HEIGHT: f32 = 24.0;

/// Approximate characters per rendered line at [`settings::BASE_FONT_SIZE`],
/// used to estimate how many visual lines a source line wraps to. Byte length is
/// used (no UTF-8 decode), which only ever *over*-estimates for multi-byte text
/// — safe for windowing.
const EST_CHARS_PER_LINE: u32 = 90;

/// Approximate width (px) of one character at [`settings::BASE_FONT_SIZE`].
///
/// Only used to convert a configured [`WrapWidth`] into a character count;
/// [`EST_CHARS_PER_LINE`] already encodes the same ratio against an assumed
/// window width.
const EST_CHAR_WIDTH: f32 = 8.0;

/// Extra source lines rendered above and below the visible region as real
/// elements. At 1080 px tall and 24 px/line ≈ 45 visible lines, 130 lines of
/// overscan is ≈ 3 screens on each side: fast scrolling never reveals a
/// placeholder before the block is built, and the residual height error of the
/// few real blocks inside the overscan is far too small to reach the viewport.
const SCROLL_OVERSCAN_LINES: i64 = 130;

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
/// height; combined with [`SCROLL_OVERSCAN_LINES`] this windows the first frame
/// to the top of the document. The scrollable reports its true bounds on the
/// next layout, so any guess error self-corrects within one frame and is hidden
/// by the overscan regardless.
const INITIAL_VIEWPORT_HEIGHT: f32 = 1080.0;

/// Main application state.
pub struct MarkdownEditor {
    /// The document, split into logical lines (no trailing newline characters).
    lines: Vec<String>,
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
    /// The document split into renderable pieces, kept in sync with
    /// [`MarkdownEditor::lines`] and the active range.
    ///
    /// Cached rather than recomputed in [`MarkdownEditor::view`]: the walk is
    /// O(document) and `view` runs on every frame — including the frames a
    /// scroll, a mouse move or a repaint produce, where nothing changed at all.
    segments: Vec<Segment>,
    /// Estimated distance (px) from the top of the document to the top of each
    /// line, with a final entry holding the total height. Always
    /// `lines.len() + 1` long.
    ///
    /// This is the single source of truth for converting between a scroll
    /// offset and a line index, used both to window the rendered segments and
    /// to scroll a line into view. Deriving one of those from a flat
    /// pixels-per-line constant while the other used the wrapping-aware
    /// estimate made them disagree by hundreds of lines on documents with long
    /// paragraphs — the viewport then showed nothing but placeholders.
    line_offsets: Vec<f32>,
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

/// All messages the application can handle.
#[derive(Debug, Clone)]
pub enum Message {
    /// A text editor action on the active region (typing, cursor movement, etc.).
    EditorAction(text_editor::Action),
    /// Make the line at the given index the active (editable) one.
    Activate(usize),
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

/// Accumulator for a run of consecutive off-screen segments.
///
/// Emitting one placeholder widget per skipped segment makes the widget tree —
/// and with it every diff, layout, event and draw pass — proportional to the
/// size of the *document* instead of to what is actually on screen. A 10,000
/// line file then costs thousands of widgets per frame to show forty lines.
/// Folding each off-screen run into a single spacer of the same total height
/// keeps the scroll geometry identical while making the tree proportional to
/// the viewport.
#[derive(Default)]
struct Gap {
    /// Total estimated height of the skipped segments.
    height: f32,
    /// Number of segments skipped, needed to also reclaim the spacing the
    /// parent column would have inserted between them.
    count: u32,
}

impl Gap {
    /// Fold one skipped segment of `height` pixels into the run.
    fn add(&mut self, height: f32) {
        self.height += height;
        self.count += 1;
    }

    /// Emit the accumulated run as a single spacer, then reset.
    fn flush<'a>(&mut self, into: &mut Vec<Element<'a, Message>>) {
        if self.count == 0 {
            return;
        }

        // Collapsing `count` children into one also removes the `count - 1`
        // inter-child gaps the column would have added.
        let height = self.height + (self.count - 1) as f32 * SEGMENT_SPACING;
        into.push(space::horizontal().height(height).into());
        *self = Self::default();
    }
}

/// Cursor and region context handed to the active editor's key bindings, so
/// they can decide whether a key edits within the region or leaves it.
#[derive(Clone, Copy, Default)]
struct BindingContext {
    /// The raw region spans more than one line (i.e. an entire table).
    multiline: bool,
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
            blocks: Vec::new(),
            block_sources: Vec::new(),
            segments: Vec::new(),
            line_offsets: Vec::new(),
            active_start: 0,
            active_end: 1,
            active_content: text_editor::Content::with_text(""),
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
        editor.activate_line(0, 0);
        editor.rebuild();

        if let Some(path) = restored {
            editor.is_loading = true;
            return (
                editor,
                Task::perform(file_ops::load_file(path), Message::FileOpened),
            );
        }

        (editor, iced::widget::operation::focus(ACTIVE_LINE_ID))
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

    /// Handle a message, rebuilding the rendered blocks only when the document
    /// or the active region may have changed.
    ///
    /// Re-parsing every block on *every* message — including the pure cursor
    /// movements and selection changes that fire on each key press — makes the
    /// editor progressively unresponsive on larger documents. Skipping the
    /// rebuild for messages that cannot affect the rendered blocks keeps typing
    /// and navigation cheap.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        let needs_rebuild = message_affects_blocks(&message);
        // Search matches are byte ranges into `lines`, so they go stale on any
        // document change — including plain typing, which deliberately skips
        // the block rebuild. The search messages maintain their own matches and
        // report `false` above, so this neither double-works nor misses an
        // edit. It is a no-op, and costs nothing, while the bar is closed.
        let touches_document =
            needs_rebuild || matches!(&message, Message::EditorAction(action) if action.is_edit());

        let task = self.handle(message);

        if needs_rebuild {
            self.rebuild();
        }
        if touches_document {
            self.refresh_search();
        }
        task
    }

    /// Handle all state transitions.
    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::EditorAction(action) => {
                let is_edit = action.is_edit();
                self.active_content.perform(action);

                if is_edit {
                    self.is_dirty = true;
                    // Write the (possibly multi-line) editor text back into the
                    // document, keeping `active_end` in sync.
                    self.commit_active();
                }

                Task::none()
            }
            Message::Activate(index) => {
                // A click always targets a line outside the current raw region
                // (rendered blocks/blanks only); guard against redundant resets.
                if index < self.lines.len()
                    && (index < self.active_start || index >= self.active_end)
                {
                    self.activate_line(index, 0);
                }
                self.focus_active()
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
                self.lines[start] = left;
                let new_index = start + 1;
                self.lines.insert(new_index, right);
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
                let current = self.lines.remove(self.active_start);
                self.lines[previous_index].push_str(&current);
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
                let next = self.lines.remove(self.active_end);
                self.lines[last_line].push_str(&next);
                self.is_dirty = true;

                self.activate_line(last_line, join_column);
                Task::none()
            }
            // The navigation messages below move the active line while the
            // editor is already focused; its stable tree position keeps that
            // focus, so they must NOT issue a re-focus task (doing so forced a
            // full layout pass on every key press and pegged the CPU).
            Message::MoveUp => {
                if self.active_start == 0 {
                    return Task::none();
                }
                let column = self.active_content.cursor().position.column;
                self.activate_line(self.active_start - 1, column);
                Task::none()
            }
            Message::MoveDown => {
                if self.active_end >= self.lines.len() {
                    return Task::none();
                }
                let column = self.active_content.cursor().position.column;
                self.activate_line(self.active_end, column);
                Task::none()
            }
            Message::PageUp => {
                if self.active_start == 0 {
                    return Task::none();
                }
                let column = self.active_content.cursor().position.column;
                let target = self.active_start.saturating_sub(PAGE_LINES);
                self.activate_line(target, column);
                Task::none()
            }
            Message::PageDown => {
                let last = self.lines.len() - 1;
                if self.active_end > last {
                    return Task::none();
                }
                let column = self.active_content.cursor().position.column;
                let target = (self.active_start + PAGE_LINES).min(last);
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
        let file_controls = row![
            toolbar::icon_button(icons::new_file(), "New (Ctrl+N)", Message::NewFile),
            toolbar::icon_button(icons::open_file(), "Open (Ctrl+O)", Message::OpenFile),
            toolbar::icon_button(icons::save_file(), "Save (Ctrl+S)", Message::SaveFile),
            toolbar::separator(),
            toolbar::icon_button_maybe(
                icons::undo(),
                "Undo (Ctrl+Z)",
                self.history.can_undo().then_some(Message::Undo),
            ),
            toolbar::icon_button_maybe(
                icons::redo(),
                "Redo (Ctrl+Y)",
                self.history.can_redo().then_some(Message::Redo),
            ),
        ]
        .spacing(4)
        .align_y(iced::Center);

        let format_bar = toolbar::view();

        // Theme selector and settings toggle, pushed to the right edge of the
        // toolbar.
        let theme_picker = row![
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
        let viewer = markdown_view::Viewer::new(&theme, &self.colors, &self.code_cache);
        let metrics = self.metrics();

        // Cursor context for the active region's key bindings (computed once).
        let cursor = self.active_content.cursor();
        let cursor_line_len = self
            .lines
            .get(self.active_start + cursor.position.line)
            .map(String::len)
            .unwrap_or(0);
        let ctx = BindingContext {
            multiline: self.active_end - self.active_start > 1,
            at_editor_top: cursor.position.line == 0,
            at_editor_bottom: cursor.position.line + 1 >= self.active_content.line_count(),
            at_line_start: cursor.position.column == 0,
            at_line_end: cursor.position.column >= cursor_line_len,
            at_doc_first: self.active_start == 0,
            at_doc_last: self.active_end >= self.lines.len(),
        };

        // Split the rendered segments around the active editor. The editor is
        // always placed at the *same* position in the widget tree (between the
        // "before" and "after" blocks), so iced preserves its widget state —
        // crucially keyboard focus and the highlighter cache — as the active
        // line moves. In a flat list the editor's child index changes whenever
        // you navigate, so iced discards its state and we'd have to re-focus on
        // every key press (an extra full layout pass that pegged the CPU).
        //
        // Windowed rendering: for large documents only the segments near the
        // current scroll viewport are built into the widget tree; everything
        // else is replaced by a single spacer per off-screen *run*, so the
        // scrollable's content height (and thus the scroll position) stays
        // stable while the widget tree stays proportional to what is actually
        // on screen rather than to the size of the document. Until the
        // scrollable reports a viewport (first frame, small documents whose
        // window already covers every line, or tests with no real layout) the
        // window is `None` and the whole document renders.
        let window = visible_line_window(self.viewport, &self.line_offsets, self.lines.len());

        let mut block_index = 0;
        let mut before: Vec<Element<'_, Message>> = Vec::new();
        let mut after: Vec<Element<'_, Message>> = Vec::new();
        let mut seen_active = false;
        let mut editor_element: Option<Element<'_, Message>> = None;
        // Height and segment count of the off-screen run being accumulated.
        let mut gap = Gap::default();

        for segment in &self.segments {
            // `is_none_or` short-circuits without running the closure when there
            // is no viewport, so the per-segment line-range match costs nothing
            // on the full-render path (small documents, first frame, tests).
            let in_window = window.is_none_or(|(ws, we)| {
                let (line_start, line_end) = match segment {
                    Segment::Active => (self.active_start, self.active_end),
                    Segment::Blank(index) => (*index, *index + 1),
                    Segment::Block { start, end } => (*start, *end),
                };
                (line_end as i64) > ws && (line_start as i64) < we
            });

            // Off-screen segments never build a widget: their height is folded
            // into the current run and emitted as one spacer when the run ends.
            if !in_window && !matches!(segment, Segment::Active) {
                match segment {
                    Segment::Blank(_) => gap.add(metrics.line_height),
                    Segment::Block { start, end } => {
                        gap.add(estimate_segment_height(&self.lines, *start, *end, metrics));
                        block_index += 1;
                    }
                    Segment::Active => unreachable!("filtered above"),
                }
                continue;
            }

            let target = if seen_active { &mut after } else { &mut before };
            gap.flush(target);

            match segment {
                // The cursor line (or table) shown as a focused raw
                // `text_editor`. It always renders for real (never windowed):
                // it holds the focused editor and its highlighter cache, and is
                // where the cursor is.
                Segment::Active => {
                    let editor = text_editor(&self.active_content)
                        .id(ACTIVE_LINE_ID)
                        .placeholder("Type Markdown…")
                        .on_action(Message::EditorAction)
                        .padding(LINE_PADDING)
                        .font(Font::MONOSPACE)
                        .size(font_size)
                        // Equivalent to the convenience `.highlight("md", …)`,
                        // spelled out so it works without iced's `highlighter`
                        // feature (which we drop to stop the Markdown parser
                        // from eagerly highlighting every code block).
                        .highlight_with::<iced_highlighter::Highlighter>(
                            iced_highlighter::Settings {
                                theme: highlight_theme,
                                token: "md".to_owned(),
                            },
                            |highlight, _theme| highlight.to_format(),
                        )
                        .key_binding(move |key_press| editor_binding(key_press, ctx))
                        // No box around the cursor's line: it is drawn flush
                        // with the rendered rows, marked only by the caret.
                        .style(app_theme::active_line);

                    editor_element = Some(container(editor).width(Fill).into());
                    seen_active = true;
                }
                Segment::Blank(index) => {
                    // A space keeps the empty line clickable and gives the
                    // document natural paragraph spacing.
                    target.push(
                        mouse_area(
                            container(text(" ").font(Font::MONOSPACE).size(font_size))
                                .width(Fill)
                                .padding(LINE_PADDING),
                        )
                        .on_press(Message::Activate(*index))
                        .into(),
                    );
                }
                Segment::Block { start, end } => {
                    // `block_index` tracks every Block segment (rendered or not)
                    // so it stays aligned with `self.blocks`.
                    let body: Element<'_, Message> = match self.blocks.get(block_index) {
                        Some(content) => {
                            viewer.set_block_source(
                                self.block_sources.get(block_index).map(String::as_str),
                            );
                            markdown::view_with(content.items(), settings, &viewer)
                                .map(Message::LinkClicked)
                        }
                        // Defensive fallback; `blocks` is kept aligned with the
                        // segment walk, so this should not happen.
                        None => text(self.lines[*start..*end].join("\n")).into(),
                    };
                    block_index += 1;

                    target.push(
                        mouse_area(container(body).width(Fill).padding(LINE_PADDING))
                            .on_press(Message::Activate(*start))
                            .into(),
                    );
                }
            }
        }

        gap.flush(if seen_active { &mut after } else { &mut before });

        // `segments` always yields exactly one `Active` segment, but fall back
        // to an empty slot rather than panicking if that ever changes.
        let editor_element = editor_element.unwrap_or_else(|| space::vertical().height(0).into());

        let body = column![
            Column::with_children(before)
                .spacing(SEGMENT_SPACING)
                .width(Fill),
            editor_element,
            Column::with_children(after)
                .spacing(SEGMENT_SPACING)
                .width(Fill),
        ]
        .spacing(SEGMENT_SPACING)
        .padding(12)
        .width(Fill);

        // A configured text width caps the measure and centres the column; the
        // scrollable still fills the window, so only the content is narrowed.
        let body: Element<'_, Message> = match self.settings.wrap_width.pixels() {
            Some(max_width) => container(body).max_width(max_width).center_x(Fill).into(),
            None => body.into(),
        };

        let document = container(
            scrollable(body)
                .id(DOCUMENT_ID)
                .width(Fill)
                .height(Fill)
                // Track the viewport so view() can window the rendered blocks.
                // The callback fires on first layout and whenever the offset
                // changes (scrolling), but is skipped for redundant viewports, so
                // typing without scrolling produces no extra messages.
                .on_scroll(|viewport: scrollable::Viewport| {
                    let offset = viewport.absolute_offset();
                    let bounds = viewport.bounds();
                    Message::Scrolled {
                        offset_y: offset.y,
                        view_height: bounds.height,
                    }
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

        let active_row = self.active_start + cursor.position.line;
        let char_column = self
            .lines
            .get(active_row)
            .and_then(|line| line.get(..cursor.position.column))
            .map(|prefix| prefix.chars().count())
            .unwrap_or(0);
        let position = format!("Ln {}, Col {}", active_row + 1, char_column + 1);

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
        let mut main_column = Column::new().spacing(0);

        for chrome in [
            toolbar,
            self.show_settings_panel
                .then(|| settings::view(&self.settings)),
            color_panel,
            self.search.as_ref().map(search::view),
        ]
        .into_iter()
        .flatten()
        {
            main_column = main_column.push(chrome);
        }

        main_column = main_column.push(document);

        if let Some(status_bar) = status_bar {
            main_column = main_column.push(status_bar);
        }

        main_column.into()
    }

    /// Return the application theme selected by the user.
    pub fn theme(&self) -> Theme {
        self.settings.theme.clone()
    }

    /// Layout estimates for the current font size and text width.
    fn metrics(&self) -> Metrics {
        Metrics::new(self.settings.font_size, self.settings.wrap_width)
    }

    /// Whether some open chrome needs the global key listener installed.
    ///
    /// Named rather than inlined into [`MarkdownEditor::subscription`] so the
    /// "no listener during ordinary editing" invariant is directly testable —
    /// `Subscription` exposes no way to ask whether it is empty.
    fn needs_key_listener(&self) -> bool {
        self.search.is_some() || self.show_settings_panel || self.show_color_panel
    }

    /// Keyboard handling that cannot go through the focused widget.
    ///
    /// Only active while a panel that `Esc` should close is open. `text_input`
    /// swallows `Esc` (to unfocus itself) and fires `on_submit` for `Enter`
    /// regardless of modifiers, so neither can be bound from the widget side;
    /// `F3` is left alone by every widget involved. With everything closed
    /// there is no listener at all, so ordinary editing never pays for one.
    pub fn subscription(&self) -> Subscription<Message> {
        if !self.needs_key_listener() {
            return Subscription::none();
        }

        // `listen_with` takes a plain `fn`, so this closure must not capture:
        // which panel `Esc` closes is decided by [`Message::Dismiss`] in
        // `handle`, not here. `F3` is safe to emit unconditionally — stepping
        // matches with no search open is a no-op.
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

    /// Replace the whole document with `text`, resetting the active region.
    fn load_document(&mut self, text: &str) {
        self.lines = split_lines(text);
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

    /// Reconstruct the full document text from its lines.
    fn document_text(&self) -> String {
        self.lines.join("\n")
    }

    /// Re-segment the document and rebuild the rendered blocks.
    fn rebuild(&mut self) {
        self.resegment();
        self.rebuild_blocks();
    }

    /// Recompute the cached [`Segment`] list from the lines and active range.
    ///
    /// The old buffer's allocation is reused, so a re-segmentation of an
    /// unchanged-size document allocates nothing.
    fn resegment(&mut self) {
        let mut segments = std::mem::take(&mut self.segments);
        collect_segments(
            &self.lines,
            self.active_start,
            self.active_end,
            &mut segments,
        );
        self.segments = segments;
        self.remeasure();
    }

    /// Rebuild [`MarkdownEditor::line_offsets`] from the current lines.
    ///
    /// Must be re-run whenever the font size or text width changes: both scale
    /// every line's height, and this table is the sole offset↔line mapping.
    fn remeasure(&mut self) {
        let metrics = self.metrics();

        self.line_offsets.clear();
        self.line_offsets.reserve(self.lines.len() + 1);

        let mut top = 0.0;
        for line in &self.lines {
            self.line_offsets.push(top);
            top += estimate_line_height(line, metrics);
        }
        self.line_offsets.push(top);
    }

    /// Scroll the document so `line` is comfortably inside the viewport.
    ///
    /// The line is placed a third of the way down rather than at the very top,
    /// so there is context above it.
    fn scroll_to_line(&self, line: usize) -> Task<Message> {
        let top = self.line_offsets.get(line).copied().unwrap_or_default();
        let view_height = self
            .viewport
            .map_or(INITIAL_VIEWPORT_HEIGHT, |(_, height)| height);

        iced::widget::operation::scroll_to(
            DOCUMENT_ID,
            scrollable::AbsoluteOffset {
                x: 0.0,
                y: (top - view_height / 3.0).max(0.0),
            },
        )
    }

    /// Shift every cached segment after the active one by `delta` lines.
    ///
    /// An edit inside the active region can only add or remove lines *within*
    /// it: the segments before it are untouched and the ones after it keep
    /// their shape and merely slide. Sliding them is a handful of integer adds,
    /// where a full [`MarkdownEditor::resegment`] would rescan every line of
    /// the document on every keystroke.
    fn shift_segments_after_active(&mut self, delta: isize) {
        if delta == 0 {
            return;
        }

        let shift = |index: &mut usize| *index = index.wrapping_add_signed(delta);

        let mut past_active = false;
        for segment in &mut self.segments {
            match segment {
                Segment::Active => past_active = true,
                Segment::Blank(index) if past_active => shift(index),
                Segment::Block { start, end } if past_active => {
                    shift(start);
                    shift(end);
                }
                _ => {}
            }
        }
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

        let replaced = prefix..old_len - suffix;
        self.block_sources.splice(replaced.clone(), sources);
        self.blocks.splice(replaced, blocks);

        self.segments = segments;
    }

    /// Make `line` part of the active (raw) region and place the cursor on it.
    ///
    /// The region expands to a whole table when `line` is inside one (see
    /// [`active_bounds`]); otherwise it covers just that line. The cursor is
    /// placed at `column` (a byte offset, clamped to a char boundary) on `line`.
    ///
    /// When the range and text are unchanged (e.g. moving the cursor within a
    /// table with `j`/`k`), the existing `active_content` is reused and only the
    /// cursor is moved. Recreating `Content::with_text` on every step resets the
    /// `text_editor` widget's internal state — including its syntax-highlighter
    /// cache — forcing a full re-highlight of the entire table on the next
    /// render. Skipping the recreation keeps table navigation at cursor-speed.
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

    fn set_active(&mut self, line: usize, column: usize, may_reuse: bool) {
        let (start, end) = active_bounds(&self.lines, line);

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

        let editor_line = line - start;
        let target_column = clamp_column(&self.lines[line], column);

        if reuse_content {
            // The content already holds the correct text; just move the cursor.
            // Always call `move_to` because the cursor is at its old position.
            self.active_content.move_to(text_editor::Cursor {
                position: text_editor::Position {
                    line: editor_line,
                    column: target_column,
                },
                selection: None,
            });
        } else {
            self.active_content =
                text_editor::Content::with_text(&self.lines[start..end].join("\n"));
            if editor_line > 0 || target_column > 0 {
                self.active_content.move_to(text_editor::Cursor {
                    position: text_editor::Position {
                        line: editor_line,
                        column: target_column,
                    },
                    selection: None,
                });
            }
        }
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
            for offset in 0..new_len {
                if let Some(line) = self.active_content.line(offset) {
                    let target = &mut self.lines[start + offset];
                    target.clear();
                    target.push_str(&line.text);
                }
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

            self.lines.splice(start..self.active_end, replacement);
            self.active_end = start + new_len;
            // Only the segments *after* the active one move, and only by the
            // number of lines gained or lost.
            self.shift_segments_after_active(new_len as isize - old_len as isize);
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

        self.lines.splice(start..end, change.before);
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

    /// Make the highlighted match's line active, select the match, and scroll
    /// it into view.
    fn reveal_current_match(&mut self) -> Task<Message> {
        let Some(hit) = self.search.as_ref().and_then(Search::current_match) else {
            return Task::none();
        };
        if hit.line >= self.lines.len() {
            return Task::none();
        }

        // Rebuild the region: the match's line becomes the raw editor, so the
        // text being replaced is shown as source rather than rendered.
        self.reactivate_line(hit.line, hit.start);
        self.rebuild();
        self.select_in_active(hit);
        self.scroll_to_line(hit.line)
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
        self.lines[hit.line].replace_range(hit.start..hit.end, &replacement);
        self.is_dirty = true;
        self.history.break_run();

        // The line changed, so every match after it on that line moved.
        self.refresh_search();
        if let Some(search) = &mut self.search {
            search.anchor_to(hit.line);
        }
        self.rebuild();
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

        // Applied back to front so each replacement's byte offsets are still
        // valid when it is made.
        for hit in hits.iter().rev() {
            if hit.line < self.lines.len() && hit.end <= self.lines[hit.line].len() {
                self.lines[hit.line].replace_range(hit.start..hit.end, &replacement);
            }
        }

        self.is_dirty = true;
        self.history.break_run();

        // Replacing can blank a line out, which changes the segmentation.
        self.refresh_search();
        self.rebuild();
        self.reveal_current_match()
    }

    /// The cursor as a `(document line, byte column)` pair.
    fn cursor_position(&self) -> (usize, usize) {
        let cursor = self.active_content.cursor().position;
        (self.active_start + cursor.line, cursor.column)
    }

    /// Move keyboard focus to the active line's editor.
    fn focus_active(&self) -> Task<Message> {
        iced::widget::operation::focus(ACTIVE_LINE_ID)
    }
}

/// Whether handling `message` can change the set of rendered blocks (the
/// document text or the active region).
///
/// Used by [`MarkdownEditor::update`] to skip the full block re-parse for
/// messages that only move the cursor, follow a link, or report file-I/O
/// status.
fn message_affects_blocks(message: &Message) -> bool {
    match message {
        // A raw-editor action never changes the *rendered* blocks, so it never
        // needs a rebuild — not even when it is an edit.
        //
        // The active region is painted straight from `active_content`, never from
        // `blocks`. An edit only ever rewrites lines *inside* that region, so:
        //   * every non-active block keeps its exact source text, and
        //   * the number and order of `Segment::Block`s is unchanged (adding or
        //     removing lines inside the active region only shifts the indices of
        //     the blocks that follow it, never their contents),
        // which means a rebuild would reproduce `blocks` byte-for-byte. Skipping
        // it keeps typing at editor-native speed regardless of document size.
        // Structural edits that *do* move the active region (Enter on a stand-
        // alone line, Backspace/Delete at a region edge) arrive as their own
        // messages (`SplitLine`, `MergeWith*`, …) and still trigger a rebuild.
        Message::EditorAction(_) => false,
        // These never touch `lines` or the active range. A theme change only
        // affects rendering style at view time, not the parsed blocks.
        Message::LinkClicked(_)
        | Message::OpenFile
        | Message::SaveFile
        | Message::FileSaved(_)
        | Message::Scrolled { .. }
        // Preferences restyle and re-measure the document, but never re-parse
        // it: the blocks are the same Markdown either way. The handlers that
        // change a size call `remeasure` themselves.
        | Message::ThemeChanged(_)
        | Message::ToggleSettingsPanel
        | Message::Dismiss
        | Message::ToggleToolbar
        | Message::ToggleStatusBar
        | Message::FontSizeChanged(_)
        | Message::WrapWidthChanged(_)
        | Message::ToggleRestoreSession
        | Message::ResetSettings
        | Message::ToggleColorPanel
        | Message::ColorHexChanged(_, _)
        | Message::ColorPicked(_, _)
        | Message::ColorReset(_)
        | Message::ResetAllColors
        // The search bar drives its own rebuilds when it moves the active
        // region; these variants only touch the bar's own state.
        | Message::CloseSearch
        | Message::ReplacementChanged(_)
        | Message::ToggleSearchCase
        | Message::SearchQueryChanged(_)
        | Message::FindNext
        | Message::FindPrevious
        | Message::ReplaceCurrent
        | Message::ReplaceAll
        | Message::OpenSearch { .. } => false,
        // Everything else may change the document or the active region.
        _ => true,
    }
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
/// Markdown constructs render together.
///
/// `out` is cleared and refilled so its allocation is reused across rebuilds.
fn collect_segments(
    lines: &[String],
    active_start: usize,
    active_end: usize,
    out: &mut Vec<Segment>,
) {
    out.clear();
    let mut index = 0;

    while index < lines.len() {
        if index == active_start {
            out.push(Segment::Active);
            // `active_start < active_end` always holds, but force forward
            // progress regardless: a zero-width active range would otherwise
            // spin forever and freeze the window (requiring a hard kill).
            index = active_end.max(index + 1);
        } else if lines[index].trim().is_empty() {
            out.push(Segment::Blank(index));
            index += 1;
        } else {
            let start = index;
            while index < lines.len() && index != active_start && !lines[index].trim().is_empty() {
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

/// Line range to render as real elements, or `None` when the whole document
/// should be rendered (no viewport yet, or the window already covers every
/// line). Off-screen segments outside this range become height placeholders in
/// [`MarkdownEditor::view`] so the scrollable's content height stays stable.
///
/// The scroll offset (in content pixels) is mapped to a line index through
/// `line_offsets`, the *same* geometry the placeholders are sized with. It is
/// still approximate — headings and code deviate from the estimate — but only
/// the few real blocks *inside* the overscan contribute their real height to
/// the offset, so the cumulative error at the viewport is bounded by the
/// overscan and far too small to reach the screen.
fn visible_line_window(
    viewport: Option<(f32, f32)>,
    line_offsets: &[f32],
    total_lines: usize,
) -> Option<(i64, i64)> {
    let (offset_y, view_height) = viewport?;

    let first = line_at_offset(line_offsets, offset_y) as i64;
    let last = line_at_offset(line_offsets, offset_y + view_height) as i64 + 1;

    let start = first - SCROLL_OVERSCAN_LINES;
    let end = last + SCROLL_OVERSCAN_LINES;

    // The window already spans the whole document: render everything and skip
    // placeholders (keeps small documents on the exact original code path).
    if start <= 0 && end >= total_lines as i64 {
        return None;
    }

    Some((start, end))
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

/// Layout estimates for one combination of font size and text width.
///
/// [`EST_LINE_HEIGHT`] and [`EST_CHARS_PER_LINE`] are calibrated for
/// [`settings::BASE_FONT_SIZE`]; both scale with the configured size, in
/// opposite directions. Bundling them keeps the render window
/// ([`visible_line_window`]) and the `line_offsets` table derived from the same
/// numbers — deriving them separately is what desynchronises the viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Metrics {
    /// Estimated height (px) of one visual line.
    line_height: f32,
    /// Estimated characters that fit on one visual line.
    chars_per_line: u32,
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
        }
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new(FontSize::DEFAULT, WrapWidth::FULL)
    }
}

/// Estimated rendered height (px) of a single source line.
///
/// A line is at least one visual line, plus extra for wrapping based on byte
/// length (no UTF-8 decode, so multi-byte text only ever over-estimates —
/// safe for windowing).
fn estimate_line_height(line: &str, metrics: Metrics) -> f32 {
    let wrapped = (line.len() as u32).div_ceil(metrics.chars_per_line).max(1);
    wrapped as f32 * metrics.line_height
}

/// Estimated pixel height of the block spanning `lines[start..end]`, used for
/// off-screen placeholders so the scrollable's content height tracks the real
/// document size.
///
/// Each source line is treated as at least one visual line, with extra lines
/// for wrapping based on byte length (no UTF-8 decode, so multi-byte text only
/// ever over-estimates — safe for windowing). Headings and code blocks deviate
/// by a few pixels, absorbed by [`SCROLL_OVERSCAN_LINES`].
fn estimate_segment_height(lines: &[String], start: usize, end: usize, metrics: Metrics) -> f32 {
    lines[start..end]
        .iter()
        .map(|line| estimate_line_height(line, metrics))
        .sum()
}

/// Determine the contiguous line range to edit as raw source when the cursor is
/// on `line`.
///
/// A line inside a Markdown table expands to cover the whole table so it can be
/// edited without breaking the table's rendering; any other line stands alone.
fn active_bounds(lines: &[String], line: usize) -> (usize, usize) {
    table_bounds(lines, line).unwrap_or((line, line + 1))
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
fn split_lines(text: &str) -> Vec<String> {
    text.split('\n').map(String::from).collect()
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
/// - **↑/↓** move between lines from the region's top/bottom edge.
/// - **PgUp/PgDn** move the active line by a page; **Ctrl+Home/End** (and
///   **Ctrl+PgUp/PgDn**) jump to the document start/end.
/// - **Ctrl+N/O/S** file actions, **Ctrl+Z / Ctrl+Y** undo/redo,
///   **Ctrl+B / Ctrl+I** bold/italic.
///
/// Everything else (Home/End, Ctrl+←/→ word motion, Ctrl+C/X/V/A, typing)
/// falls through to the default editor bindings, which operate within the
/// active region.
fn editor_binding(
    key_press: text_editor::KeyPress,
    ctx: BindingContext,
) -> Option<text_editor::Binding<Message>> {
    use keyboard::key::Named;

    let key = key_press.key.clone();
    let command = key_press.modifiers.command();
    let shift = key_press.modifiers.shift();

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
            // The only way back once the toolbar is hidden.
            "," => Some(Message::ToggleSettingsPanel),
            _ => None,
        };

        if let Some(message) = message {
            return Some(text_editor::Binding::Custom(message));
        }
    }

    match key.as_ref() {
        // Enter on a standalone line splits it into a new document line; inside a
        // table it falls through to insert a newline (i.e. add a row).
        keyboard::Key::Named(Named::Enter) if !ctx.multiline => {
            Some(text_editor::Binding::Custom(Message::SplitLine))
        }
        // Backspace at the very start of the region merges into the line above;
        // elsewhere it edits within the editor (including joining table rows).
        keyboard::Key::Named(Named::Backspace)
            if ctx.at_editor_top && ctx.at_line_start && !ctx.at_doc_first =>
        {
            Some(text_editor::Binding::Custom(Message::MergeWithPrevious))
        }
        // Delete at the very end of the region pulls the next line up; elsewhere
        // it deletes forward within the editor.
        keyboard::Key::Named(Named::Delete)
            if ctx.at_editor_bottom && ctx.at_line_end && !ctx.at_doc_last =>
        {
            Some(text_editor::Binding::Custom(Message::MergeWithNext))
        }
        // Arrows only leave the region from its top/bottom edge; within a
        // multi-line region they move between rows by default.
        keyboard::Key::Named(Named::ArrowUp) if ctx.at_editor_top && !ctx.at_doc_first => {
            Some(text_editor::Binding::Custom(Message::MoveUp))
        }
        keyboard::Key::Named(Named::ArrowDown) if ctx.at_editor_bottom && !ctx.at_doc_last => {
            Some(text_editor::Binding::Custom(Message::MoveDown))
        }
        // Page navigation moves the active line through the document (a tiny
        // single-line editor has nowhere to page within). `Ctrl` jumps to the
        // very start/end. Handled regardless of `multiline` so you can also page
        // straight out of a table.
        keyboard::Key::Named(Named::PageUp) => Some(text_editor::Binding::Custom(if command {
            Message::JumpToStart
        } else {
            Message::PageUp
        })),
        keyboard::Key::Named(Named::PageDown) => Some(text_editor::Binding::Custom(if command {
            Message::JumpToEnd
        } else {
            Message::PageDown
        })),
        // Ctrl+Home / Ctrl+End jump to the document start/end. Plain Home/End
        // fall through to the default binding (start/end of the current line).
        keyboard::Key::Named(Named::Home) if command => {
            Some(text_editor::Binding::Custom(Message::JumpToStart))
        }
        keyboard::Key::Named(Named::End) if command => {
            Some(text_editor::Binding::Custom(Message::JumpToEnd))
        }
        _ => text_editor::Binding::from_key_press(key_press),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_lines(text: &str) -> Vec<String> {
        split_lines(text)
    }

    /// Convenience wrapper: the production walker fills a caller-owned buffer
    /// so it can reuse the allocation, which is noise in a test.
    fn segments(lines: &[String], active_start: usize, active_end: usize) -> Vec<Segment> {
        let mut out = Vec::new();
        collect_segments(lines, active_start, active_end, &mut out);
        out
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
    // `visible_line_window` and `estimate_segment_height` drive the
    // virtualization in `view`; they are pure functions, so they get their own
    // unit tests independent of the widget tree.

    /// The cumulative line-offset table for a document of uniform, non-wrapping
    /// lines — the geometry `visible_line_window` maps offsets through.
    fn offsets_for(lines: &[String]) -> Vec<f32> {
        let metrics = Metrics::default();
        let mut offsets = Vec::with_capacity(lines.len() + 1);
        let mut top = 0.0;
        for line in lines {
            offsets.push(top);
            top += estimate_line_height(line, metrics);
        }
        offsets.push(top);
        offsets
    }

    fn flat_offsets(count: usize) -> Vec<f32> {
        (0..=count).map(|i| i as f32 * EST_LINE_HEIGHT).collect()
    }

    #[test]
    fn visible_line_window_renders_all_without_a_viewport() {
        // No layout has happened yet (first frame, or tests): render everything
        // so small documents and the test suite keep the original behaviour.
        assert_eq!(
            visible_line_window(None, &flat_offsets(10_000), 10_000),
            None
        );
    }

    #[test]
    fn visible_line_window_renders_all_when_window_covers_document() {
        // A small document scrolled to the top: the overscan already reaches
        // past the end, so there is nothing to skip.
        assert_eq!(
            visible_line_window(Some((0.0, 600.0)), &flat_offsets(20), 20),
            None
        );
    }

    #[test]
    fn visible_line_window_crops_a_scrolled_large_document() {
        // 8 000 lines, scrolled ~halfway down a 1 000 px viewport. The window
        // must be a bounded line range (not `None`) covering the visible lines.
        let offsets = flat_offsets(8_000);
        let win = visible_line_window(Some((50_000.0, 1_000.0)), &offsets, 8_000);
        let (start, end) = win.expect("a large scrolled document is windowed");

        // offset_y / EST_LINE_HEIGHT ≈ 2 083 → first visible line ~2 083.
        let first_visible = (50_000.0 / EST_LINE_HEIGHT).floor() as i64;
        assert!(
            start <= first_visible && end >= first_visible,
            "window {start}..{end} must cover the first visible line {first_visible}"
        );
        // Overscan reaches past both ends of the visible region.
        assert!(start < first_visible, "overscan before the viewport");
        assert!(
            end > first_visible + (1_000.0 / EST_LINE_HEIGHT) as i64,
            "overscan after the viewport"
        );
    }

    /// The window must be derived from the *same* geometry the off-screen
    /// placeholders are sized with. Mapping the offset through a flat
    /// pixels-per-line constant instead put the window hundreds of lines away
    /// from what was on screen for any document with wrapping lines, so the
    /// viewport showed nothing but blank placeholders.
    #[test]
    fn visible_line_window_agrees_with_placeholder_geometry_when_lines_wrap() {
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

        let (start, end) =
            visible_line_window(Some((offset_y, view_height)), &offsets, lines.len())
                .expect("a large scrolled document is windowed");

        // The line genuinely at that offset, by the placeholders' own geometry.
        let on_screen = line_at_offset(&offsets, offset_y) as i64;
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
        assert_eq!(editor.line_offsets[0], 0.0);
        assert_eq!(
            *editor.line_offsets.last().unwrap(),
            estimate_segment_height(&editor.lines, 0, editor.lines.len(), editor.metrics())
        );
    }

    #[test]
    fn estimate_segment_height_counts_wrapped_lines() {
        let metrics = Metrics::default();
        let long = "x".repeat(200); // > EST_CHARS_PER_LINE → ≥3 visual lines
        let lines = to_lines(&format!("short\n{long}"));

        // One short line (1 visual) + one 200-char line (ceil(200/90) = 3 visual).
        let h = estimate_segment_height(&lines, 0, 2, metrics);
        assert_eq!(
            h,
            (1 + 200u32.div_ceil(EST_CHARS_PER_LINE)) as f32 * EST_LINE_HEIGHT
        );

        // A single short line is at least one line tall.
        let h0 = estimate_segment_height(&lines, 0, 1, metrics);
        assert_eq!(h0, EST_LINE_HEIGHT);
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
        let _ = editor.update(Message::Activate(2));

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
        let doc = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document(&doc);
        assert_eq!((editor.active_start, editor.active_end), (0, 1));

        // PgDown moves down one page (PAGE_LINES) from the current line.
        let _ = editor.update(Message::PageDown);
        assert_eq!(editor.active_start, PAGE_LINES);

        // PgDown again clamps to the last line.
        let _ = editor.update(Message::PageDown);
        assert_eq!(editor.active_start, 29);

        // At the very bottom PgDown is a no-op.
        let _ = editor.update(Message::PageDown);
        assert_eq!(editor.active_start, 29);

        // PgUp moves up one page.
        let _ = editor.update(Message::PageUp);
        assert_eq!(editor.active_start, 29 - PAGE_LINES);

        // Ctrl+Home / Ctrl+End (and Ctrl+PgUp / Ctrl+PgDown) jump to the ends.
        let _ = editor.update(Message::JumpToStart);
        assert_eq!((editor.active_start, editor.active_end), (0, 1));
        let _ = editor.update(Message::JumpToEnd);
        assert_eq!((editor.active_start, editor.active_end), (29, 30));
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
        let mut fresh = Vec::new();
        collect_segments(
            &editor.lines,
            editor.active_start,
            editor.active_end,
            &mut fresh,
        );
        assert_eq!(
            editor.segments, fresh,
            "seed {seed} step {step}: cached segments diverged from a full walk"
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

        match r % 18 {
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
            11 => Message::EditorAction(Action::SelectAll),
            12 => {
                let index = (r >> 8) as usize % (editor.lines.len() + 2);
                Message::Activate(index)
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
        ];

        for (s, doc) in seeds.iter().enumerate() {
            for seed in 0..120u64 {
                let (mut editor, _t) = MarkdownEditor::new_with_file(None);
                editor.load_document(doc);
                editor.rebuild();
                let mut state = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);

                for step in 0..50usize {
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

        let _ = editor.update(Message::Activate(1));
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
        let _ = editor.update(Message::Activate(1));
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

        let _ = editor.update(Message::Activate(2));
        let before = editor.lines.clone();
        let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
            text_editor::Edit::Enter,
        )));
        assert_eq!(editor.lines.len(), before.len() + 1);

        let _ = editor.update(Message::Undo);
        assert_eq!(editor.lines, before);
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
        let _ = editor.update(Message::Activate(2));
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
        let before = editor.block_sources.clone();

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

        assert_eq!(editor.block_sources, before);
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
    #[test]
    fn settings_changes_do_not_trigger_block_rebuild() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("# Title\n\nbody");
        editor.rebuild();
        let before = editor.block_sources.clone();

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
            assert!(
                !message_affects_blocks(&message),
                "{message:?} must not force a block rebuild"
            );
            let _ = editor.update(message);
        }

        assert_eq!(editor.block_sources, before);
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
        segments(&editor.lines, editor.active_start, editor.active_end)
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
