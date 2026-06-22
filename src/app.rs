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
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use iced::keyboard;
use iced::widget::{
    Column, button, column, container, markdown, mouse_area, pick_list, row, scrollable, space,
    stack, text, text_editor,
};
use iced::{Element, Fill, Font, Subscription, Task, Theme, window};

use crate::color_picker;
use crate::config;
use crate::file_ops::{self, FileError};
use crate::icons;
use crate::markdown_view;
use crate::theme as app_theme;
use crate::theme::{MarkdownColors, MarkdownTag};
use crate::toolbar::{self, FormatAction};

mod vim;

/// Default content shown when the editor starts with no file.
const WELCOME_MD: &str = r#"# Welcome to Markdown Editor

This editor renders Markdown **inline** and is driven by **Vim keys**. The
document is shown rendered, except the line your cursor is on — that one shows
the raw source so you can edit it.

The editor starts in **Normal mode**: press `i` to insert text and `Esc` to
return to Normal mode.

## Modes

- **Normal** — keys are commands (motions, operators, …)
- **Insert** — entered with `i a o I A O`; type normally; `Esc` leaves it
- **Visual** — `v` (characters) or `V` (whole lines) to select

## Motions & edits

| Keys           | Action                              |
|----------------|-------------------------------------|
| `h j k l`      | move left / down / up / right       |
| `w b e`        | word forward / back / end           |
| `0 ^ $`        | line start / first non-blank / end  |
| `gg G`         | document start / end (`5G` → line 5)|
| `x  dd  dw`    | delete char / line / word           |
| `yy  p`        | yank line / paste                   |
| `u`  `Ctrl-r`  | undo / redo                         |

## Command line & search

| Command          | Action                          |
|------------------|---------------------------------|
| `:w`             | save                            |
| `:q`  `:q!`      | quit / quit without saving      |
| `:wq`  `:x`      | save and quit                   |
| `:e`             | new document                    |
| `/text`  `?text` | search forward / back (`n` `N`) |

> Click a block to jump to it, then press `i` to edit its raw Markdown.
"#;

/// Widget id of the editable (active) line, used to move focus when the
/// active line changes programmatically.
const ACTIVE_LINE_ID: &str = "md-active-line";

/// Padding applied to both the active editor and the rendered blocks so they
/// stay horizontally aligned.
const LINE_PADDING: u16 = 4;

/// Width (px) of the cursor marker drawn in the left margin of the current line
/// in view mode. Kept within [`LINE_PADDING`] so it sits in the row's left
/// padding without overlapping the rendered text.
const CURSOR_MARKER_WIDTH: f32 = 3.0;

/// Number of lines moved by `PgUp`/`PgDown` (a "page").
const PAGE_LINES: usize = 15;

/// Rough pixel height of one rendered line, used to turn the scroll offset into
/// a line range for windowed rendering. Headings are a bit taller, code lines a
/// bit shorter, and wrapped paragraphs span several visual lines — but
/// [`estimate_segment_height`] accounts for wrapping and
/// [`SCROLL_OVERSCAN_LINES`] absorbs the rest, so a placeholder never shows on
/// screen.
const EST_LINE_HEIGHT: f32 = 24.0;

/// Approximate characters per rendered line, used to estimate how many visual
/// lines a source line wraps to. Byte length is used (no UTF-8 decode), which
/// only ever *over*-estimates for multi-byte text — safe for windowing.
const EST_CHARS_PER_LINE: u32 = 90;

/// Extra source lines rendered above and below the visible region as real
/// elements. At 1080 px tall and 24 px/line ≈ 45 visible lines, 130 lines of
/// overscan is ≈ 3 screens on each side: fast scrolling never reveals a
/// placeholder before the block is built, and the residual height error of the
/// few real blocks inside the overscan is far too small to reach the viewport.
const SCROLL_OVERSCAN_LINES: i64 = 130;

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
    /// The application theme used to render the Markdown preview.
    theme: Theme,
    /// Per-tag Markdown color overrides. Empty strings follow the theme; valid
    /// hex strings override a single tag's color.
    colors: MarkdownColors,
    /// Whether the per-tag color customization panel is shown.
    show_color_panel: bool,
    /// Memoized syntax-highlighted code blocks for the current theme. Cleared
    /// when the theme changes or a new document is loaded.
    code_cache: RefCell<markdown_view::CodeCache>,
    /// Vim modal-editing state (current mode, pending command, registers, …).
    vim: vim::VimState,
    /// Document snapshots for `u` (undo).
    undo_stack: Vec<vim::Snapshot>,
    /// Document snapshots for `Ctrl-r` (redo).
    redo_stack: Vec<vim::Snapshot>,
    /// Set by `:wq`/`:x` so the editor quits once the async save succeeds.
    pending_quit: bool,
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
    /// A key routed to the Vim engine (Normal/Visual/command-line modes).
    VimKey(vim::Key),
    /// The document scroll viewport moved (or was first reported by layout).
    /// Carries the vertical scroll offset (px from the top) and the visible
    /// height (px), used to window the rendered blocks so only those near the
    /// viewport are built into the widget tree.
    Scrolled { offset_y: f32, view_height: f32 },
}

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

/// Cursor and region context handed to the active editor's key bindings, so
/// they can decide whether a key edits within the region or leaves it.
#[derive(Clone, Copy)]
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
    /// Create a new editor instance, optionally loading `initial_file` from
    /// disk instead of showing the welcome document.
    ///
    /// When `initial_file` is `Some`, the editor boots with the welcome
    /// document and immediately kicks off an async load of the given path;
    /// [`Message::FileOpened`] swaps in the file's contents on success, or
    /// leaves the welcome document on error. The per-tag color configuration
    /// is loaded from `$XDG_CONFIG_HOME/md_editor/config.yaml` (falling back
    /// to `$HOME/.config/...`) so customizations persist across restarts.
    /// Missing or malformed config files silently fall back to defaults.
    pub fn new_with_file(initial_file: Option<PathBuf>) -> (Self, Task<Message>) {
        let mut editor = Self {
            lines: split_lines(WELCOME_MD),
            blocks: Vec::new(),
            block_sources: Vec::new(),
            active_start: 0,
            active_end: 1,
            active_content: text_editor::Content::with_text(""),
            current_file: None,
            is_dirty: false,
            is_loading: false,
            theme: Theme::TokyoNight,
            colors: config::load(),
            show_color_panel: false,
            code_cache: RefCell::new(HashMap::new()),
            vim: vim::VimState::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            pending_quit: false,
            viewport: None,
        };
        editor.activate_line(0, 0);
        editor.rebuild_blocks();

        if let Some(path) = initial_file {
            editor.is_loading = true;
            return (
                editor,
                Task::perform(file_ops::load_file(path), Message::FileOpened),
            );
        }

        (editor, iced::widget::operation::focus(ACTIVE_LINE_ID))
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
        let task = self.handle(message);
        if needs_rebuild {
            self.rebuild_blocks();
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

                self.lines[self.active_start] = left;
                let new_index = self.active_start + 1;
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
                    self.current_file = Some(path);
                    self.is_dirty = false;
                    // `:wq`/`:x` requested a quit once the save completed.
                    if self.pending_quit {
                        self.pending_quit = false;
                        return window::oldest().and_then(window::close);
                    }
                } else {
                    // The save was cancelled or failed: cancel any pending quit
                    // so the editor is not closed with unsaved work.
                    self.pending_quit = false;
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
                self.theme = theme;
                // Cached highlights are colored for the previous theme.
                self.code_cache.borrow_mut().clear();
                Task::none()
            }
            Message::ToggleColorPanel => {
                self.show_color_panel = !self.show_color_panel;
                Task::none()
            }
            Message::ColorHexChanged(tag, hex) => {
                *self.colors.get_mut(tag) = hex;
                config::save(&self.colors);
                Task::none()
            }
            Message::ColorPicked(tag, color) => {
                *self.colors.get_mut(tag) = app_theme::to_hex(color);
                config::save(&self.colors);
                Task::none()
            }
            Message::ColorReset(tag) => {
                self.colors.get_mut(tag).clear();
                config::save(&self.colors);
                Task::none()
            }
            Message::ResetAllColors => {
                self.colors = MarkdownColors::default();
                config::save(&self.colors);
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
            Message::VimKey(key) => self.handle_vim_key(key),
        }
    }

    /// Build the UI: toolbar on top, the inline-rendered document, status bar.
    pub fn view(&self) -> Element<'_, Message> {
        // -- Toolbar --
        let file_controls = row![
            toolbar::icon_button(icons::new_file(), "New (:e)", Message::NewFile),
            toolbar::icon_button(icons::open_file(), "Open", Message::OpenFile),
            toolbar::icon_button(icons::save_file(), "Save (:w)", Message::SaveFile),
        ]
        .spacing(4)
        .align_y(iced::Center);

        let format_bar = toolbar::view();

        // Theme selector and color customization toggle, pushed to the right
        // edge of the toolbar.
        let colors_label = if self.show_color_panel {
            "Close"
        } else {
            "Colors"
        };
        let colors_button = button(text(colors_label).size(13))
            .padding([4, 10])
            .style(app_theme::toolbar_button)
            .on_press(Message::ToggleColorPanel);

        let theme_picker = row![
            text("Theme").size(13).style(app_theme::status_text),
            pick_list(Theme::ALL, Some(self.theme.clone()), Message::ThemeChanged)
                .text_size(13)
                .padding([4, 8]),
            colors_button,
        ]
        .spacing(8)
        .align_y(iced::Center);

        let toolbar = container(
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
        .style(app_theme::toolbar_container);

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
                container(
                    column(rows)
                        .spacing(6)
                        .push(space::vertical().height(4))
                        .push(reset_all),
                )
                .padding([8, 16])
                .width(Fill)
                .style(app_theme::toolbar_container)
                .into(),
            )
        } else {
            None
        };

        // -- Inline document --
        let theme = self.theme();
        let settings = markdown_view::settings(&theme, &self.colors);
        let highlight_theme = app_theme::code_highlighter(&theme);
        let viewer = markdown_view::Viewer::new(&theme, &self.colors, &self.code_cache);

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

        // How the cursor line is shown depends on the mode:
        //   * Edit mode (Insert/Visual) shows it as a focused raw `text_editor`
        //     inside the highlighted box, with a real text caret.
        //   * View mode (Normal) renders it as Markdown exactly like every other
        //     row — no box, no background change — and marks it only with a thin
        //     cursor bar in the left margin, so the row's display never changes.
        let edit_mode = self.vim_is_edit_mode();
        let vim_insert = self.vim_is_insert();

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
        // else becomes a fixed-height placeholder so the scrollable's content
        // height (and thus the scroll position) stays stable. Until the
        // scrollable reports a viewport (first frame, small documents whose
        // window already covers every line, or tests with no real layout) the
        // window is `None` and the whole document renders — preserving the
        // existing behaviour exactly.
        let window = visible_line_window(self.viewport, self.lines.len());

        let mut block_index = 0;
        let mut before: Vec<Element<'_, Message>> = Vec::new();
        let mut after: Vec<Element<'_, Message>> = Vec::new();
        let mut editor_element: Option<Element<'_, Message>> = None;

        // In view mode the document is segmented without splitting out the cursor
        // line (see `render_split`), so the cursor's line stays inside its block
        // and the layout never changes as the cursor moves; the cursor's segment
        // is found by `active_start` and marked with the left-margin bar. In edit
        // mode the cursor line is the `Active` segment (the raw editor).
        let (split_start, split_end) = self.render_split();

        for segment in segments(&self.lines, split_start, split_end) {
            // `is_none_or` short-circuits without running the closure when there
            // is no viewport, so the per-segment line-range match costs nothing
            // on the full-render path (small documents, first frame, tests).
            let in_window = window.is_none_or(|(ws, we)| {
                let (line_start, line_end) = match &segment {
                    Segment::Active => (self.active_start, self.active_end),
                    Segment::Blank(index) => (*index, *index + 1),
                    Segment::Block { start, end } => (*start, *end),
                };
                (line_end as i64) > ws && (line_start as i64) < we
            });

            match segment {
                // Only emitted in edit mode (Insert/Visual): the cursor line (or
                // table) shown as a focused raw `text_editor` inside the box. It
                // always renders for real (never windowed): it holds the focused
                // editor and its highlighter cache, and is where the cursor is.
                Segment::Active => {
                    let editor = text_editor(&self.active_content)
                        .id(ACTIVE_LINE_ID)
                        .placeholder("Type Markdown…")
                        .on_action(Message::EditorAction)
                        .padding(LINE_PADDING)
                        .font(Font::MONOSPACE)
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
                        .key_binding(move |key_press| {
                            if vim_insert {
                                insert_line_binding(key_press, ctx)
                            } else {
                                // Visual mode: hand every key to the Vim engine.
                                vim::to_vim_key(&key_press)
                                    .map(|k| text_editor::Binding::Custom(Message::VimKey(k)))
                            }
                        });

                    editor_element = Some(
                        container(editor)
                            .width(Fill)
                            .style(app_theme::active_line)
                            .into(),
                    );
                }
                Segment::Blank(index) => {
                    let element: Element<'_, Message> = if in_window {
                        // A space keeps the empty line clickable and gives the
                        // document natural paragraph spacing.
                        let blank = mouse_area(
                            container(text(" ").font(Font::MONOSPACE))
                                .width(Fill)
                                .padding(LINE_PADDING),
                        )
                        .on_press(Message::Activate(index));

                        // View mode: mark the cursor's (blank) line.
                        if !edit_mode && index == self.active_start {
                            mark_cursor_line(blank.into())
                        } else {
                            blank.into()
                        }
                    } else {
                        // Off-screen: a height-only placeholder keeps the
                        // scrollable's content height stable.
                        space::horizontal().height(EST_LINE_HEIGHT).into()
                    };

                    if editor_element.is_some() {
                        after.push(element);
                    } else {
                        before.push(element);
                    }
                }
                Segment::Block { start, end } => {
                    // `block_index` tracks every Block segment (rendered or not)
                    // so it stays aligned with `self.blocks`.
                    let element: Element<'_, Message> = if in_window {
                        let body: Element<'_, Message> = match self.blocks.get(block_index) {
                            Some(content) => {
                                if let Some(source) = self.block_sources.get(block_index) {
                                    viewer.set_block_source(source.clone());
                                }
                                markdown::view_with(content.items(), settings, &viewer)
                                    .map(Message::LinkClicked)
                            }
                            // Defensive fallback; `blocks` is kept aligned with the
                            // segment walk, so this should not happen.
                            None => text(self.lines[start..end].join("\n")).into(),
                        };
                        let block = mouse_area(container(body).width(Fill).padding(LINE_PADDING))
                            .on_press(Message::Activate(start));

                        // View mode: the cursor line is rendered inside its block
                        // (never split out), so mark the block that contains it.
                        // The block is rendered identically whether or not the
                        // cursor is in it, so the layout never changes.
                        if !edit_mode && (start..end).contains(&self.active_start) {
                            mark_cursor_line(block.into())
                        } else {
                            block.into()
                        }
                    } else {
                        // Off-screen: a height-only placeholder keeps the
                        // scrollable's content height stable so the viewport
                        // offset keeps mapping to the same document position.
                        space::horizontal()
                            .height(estimate_segment_height(&self.lines, start, end))
                            .into()
                    };
                    block_index += 1;

                    if editor_element.is_some() {
                        after.push(element);
                    } else {
                        before.push(element);
                    }
                }
            }
        }

        // In edit mode `segments` yields exactly one `Active` segment (the raw
        // editor); in view mode there is none (the cursor line is rendered inside
        // its block), so the middle slot collapses to nothing.
        let editor_element = editor_element.unwrap_or_else(|| space::vertical().height(0).into());

        let document = container(
            scrollable(
                column![
                    Column::with_children(before).spacing(1).width(Fill),
                    editor_element,
                    Column::with_children(after).spacing(1).width(Fill),
                ]
                .spacing(1)
                .padding(12)
                .width(Fill),
            )
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

        // Vim mode badge plus the command line being typed (`:wq`, `/foo`) or a
        // transient message (e.g. a search-failure notice).
        let mode_label = self.vim_mode_label();
        let info_line = self
            .vim_command_line()
            .or_else(|| self.vim_message().map(str::to_string))
            .unwrap_or_default();

        let status_bar = container(
            row![
                text(format!("-- {mode_label} --"))
                    .size(12)
                    .font(Font::MONOSPACE)
                    .style(app_theme::status_text),
                space::horizontal().width(16),
                text(info_line)
                    .size(12)
                    .font(Font::MONOSPACE)
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
        .style(app_theme::toolbar_container);

        let mut main_column = column![toolbar].spacing(0);

        if let Some(panel) = color_panel {
            main_column = main_column.push(panel);
        }

        main_column = main_column.push(document).push(status_bar);

        main_column.into()
    }

    /// Return the application theme selected by the user.
    pub fn theme(&self) -> Theme {
        self.theme.clone()
    }

    /// Keyboard handling for view (Normal) mode.
    ///
    /// In view mode the cursor line is rendered as Markdown like the rest of the
    /// document, so there is no focused editor to receive keys: a global key
    /// listener feeds them to the Vim engine instead. While editing (Insert) or
    /// selecting (Visual) the focused `text_editor` handles keys, so the
    /// subscription is disabled to avoid double handling.
    pub fn subscription(&self) -> Subscription<Message> {
        if self.vim_is_edit_mode() {
            Subscription::none()
        } else {
            iced::event::listen_with(on_key_event)
        }
    }

    /// Replace the whole document with `text`, resetting the active region.
    fn load_document(&mut self, text: &str) {
        self.lines = split_lines(text);
        self.activate_line(0, 0);
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

    /// The line range split out as the editable [`Segment::Active`] when the
    /// document is segmented for rendering.
    ///
    /// In **edit mode** (Insert/Visual) the cursor line (or table) is split out
    /// of its block and shown as a raw `text_editor`, so the range is the real
    /// `active_start..active_end`.
    ///
    /// In **view mode** (Normal) nothing is split out: the cursor line is
    /// rendered as Markdown *inside* its block, exactly like every other line, so
    /// the document layout never changes as the cursor moves between lines (a
    /// soft-wrapped paragraph is not broken apart when the cursor enters it). A
    /// range at the end of the document makes [`segments`] emit no `Active`
    /// segment, leaving every block whole.
    fn render_split(&self) -> (usize, usize) {
        if self.vim_is_edit_mode() {
            (self.active_start, self.active_end)
        } else {
            (self.lines.len(), self.lines.len())
        }
    }

    /// Rebuild the rendered blocks from the current lines and active range,
    /// reusing any previously parsed block whose source text is unchanged.
    ///
    /// Navigation and editing only change the blocks next to the cursor, so
    /// re-parsing the whole document on every keystroke is wasteful — and at
    /// keyboard auto-repeat speed it pegged the CPU and made the editor stall.
    /// Parsed blocks are cached by their source text and moved into the new
    /// layout when unchanged; only genuinely new text is parsed.
    fn rebuild_blocks(&mut self) {
        // The new blocks, as line ranges, in document order. The split range
        // depends on the mode: view mode keeps every block whole (no Active
        // segment), so the cursor moving never changes the set of blocks.
        let (split_start, split_end) = self.render_split();
        let ranges: Vec<(usize, usize)> = segments(&self.lines, split_start, split_end)
            .into_iter()
            .filter_map(|segment| match segment {
                Segment::Block { start, end } => Some((start, end)),
                _ => None,
            })
            .collect();

        // Pair the previous parses with their source text so unchanged ones can
        // be moved across by position. `Option` lets us take ownership of an
        // individual entry (from either end) without disturbing the others.
        let old_sources = std::mem::take(&mut self.block_sources);
        let old_blocks = std::mem::take(&mut self.blocks);
        let mut old: Vec<Option<(String, markdown::Content)>> =
            old_sources.into_iter().zip(old_blocks).map(Some).collect();

        let new_len = ranges.len();
        let old_len = old.len();

        // Edits are local, so the new layout almost always shares a long run of
        // unchanged blocks with the old one at the front and/or the back.
        // Comparing by text (without allocating a joined string) finds those runs
        // so only the genuinely changed middle is re-parsed — turning a full
        // re-parse on navigation into O(changed) work, with no hashing and no
        // per-block allocation for the parts that stayed the same.
        let mut prefix = 0;
        while prefix < new_len
            && prefix < old_len
            && block_source_eq(&self.lines, ranges[prefix], source_at(&old, prefix))
        {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < new_len - prefix
            && suffix < old_len - prefix
            && block_source_eq(
                &self.lines,
                ranges[new_len - 1 - suffix],
                source_at(&old, old_len - 1 - suffix),
            )
        {
            suffix += 1;
        }

        let middle_end = new_len - suffix;
        let mut sources = Vec::with_capacity(new_len);
        let mut blocks = Vec::with_capacity(new_len);

        for (new_index, &(start, end)) in ranges.iter().enumerate() {
            let reused = if new_index < prefix {
                old[new_index].take()
            } else if new_index >= middle_end {
                // k-th block from the end maps to the same offset in `old`.
                let k = new_len - 1 - new_index;
                old[old_len - 1 - k].take()
            } else {
                None
            };

            match reused {
                Some((source, content)) => {
                    sources.push(source);
                    blocks.push(content);
                }
                None => {
                    let source = self.lines[start..end].join("\n");
                    let content = markdown::Content::parse(&source);
                    sources.push(source);
                    blocks.push(content);
                }
            }
        }

        self.block_sources = sources;
        self.blocks = blocks;
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
        let (start, end) = active_bounds(&self.lines, line);

        // If the range hasn't changed, check whether the text is also the same.
        // `commit_active` keeps `lines` in sync with `active_content` after
        // edits, and pure motions never touch `lines`, so an unchanged range
        // with matching text means the content is already correct.
        let reuse_content = start == self.active_start
            && end == self.active_end
            && self.active_content.text() == self.lines[start..end].join("\n");

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
    /// the text is split on newlines and spliced over the current range,
    /// updating `active_end` to match.
    fn commit_active(&mut self) {
        let text = self.active_content.text();
        let parts: Vec<String> = text.split('\n').map(String::from).collect();
        let new_end = self.active_start + parts.len();
        self.lines.splice(self.active_start..self.active_end, parts);
        self.active_end = new_end;
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
        | Message::ThemeChanged(_)
        | Message::Scrolled { .. }
        | Message::ToggleColorPanel
        | Message::ColorHexChanged(_, _)
        | Message::ColorPicked(_, _)
        | Message::ColorReset(_)
        | Message::ResetAllColors => false,
        // A Vim key can edit the document or move the active region (changing
        // which line is shown as raw source), so always rebuild.
        Message::VimKey(_) => true,
        // Everything else may change the document or the active region.
        _ => true,
    }
}

/// Overlay the left-margin cursor marker on a view-mode row.
///
/// The marker is a thin vertical bar drawn *on top* of the row (via a `stack`),
/// so it does not take part in the row's layout: the rendered text is not
/// shifted and stays aligned with every other row. It sits in the row's left
/// padding, just left of the text, and spans the row's height.
///
/// The stack must be told to fill the width — it defaults to `Shrink`, which
/// would re-lay-out the row in a narrower box and make it wrap differently from
/// the other rows. With `Fill` the row is laid out exactly as it would be
/// without the marker.
fn mark_cursor_line(row: Element<'_, Message>) -> Element<'_, Message> {
    let marker = container(space::vertical())
        .width(CURSOR_MARKER_WIDTH)
        .height(Fill)
        .style(app_theme::cursor_marker);
    stack![row, marker].width(Fill).into()
}

/// Walk the document, grouping it into renderable [`Segment`]s.
///
/// The active range and blank lines are their own segments; every other
/// contiguous run of lines becomes a single [`Segment::Block`] so multi-line
/// Markdown constructs render together.
fn segments(lines: &[String], active_start: usize, active_end: usize) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        if index == active_start {
            segments.push(Segment::Active);
            // `active_start < active_end` always holds, but force forward
            // progress regardless: this loop runs in `view()` on the UI thread,
            // so a zero-width active range would otherwise spin forever and
            // freeze the window (requiring a hard kill).
            index = active_end.max(index + 1);
        } else if lines[index].trim().is_empty() {
            segments.push(Segment::Blank(index));
            index += 1;
        } else {
            let start = index;
            while index < lines.len() && index != active_start && !lines[index].trim().is_empty() {
                index += 1;
            }
            segments.push(Segment::Block { start, end: index });
        }
    }

    segments
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
/// The scroll offset (in content pixels) is mapped to a line index through the
/// constant [`EST_LINE_HEIGHT`]. This is approximate — headings, code and
/// wrapped lines deviate — but only the few real blocks *inside* the overscan
/// contribute their real height to the offset, so the cumulative error at the
/// viewport is bounded by the overscan and far too small to reach the screen.
fn visible_line_window(viewport: Option<(f32, f32)>, total_lines: usize) -> Option<(i64, i64)> {
    let (offset_y, view_height) = viewport?;

    let first = (offset_y / EST_LINE_HEIGHT).floor() as i64;
    let last = ((offset_y + view_height) / EST_LINE_HEIGHT).ceil() as i64;

    let start = first - SCROLL_OVERSCAN_LINES;
    let end = last + SCROLL_OVERSCAN_LINES;

    // The window already spans the whole document: render everything and skip
    // placeholders (keeps small documents on the exact original code path).
    if start <= 0 && end >= total_lines as i64 {
        return None;
    }

    Some((start, end))
}

/// Estimated pixel height of the block spanning `lines[start..end]`, used for
/// off-screen placeholders so the scrollable's content height tracks the real
/// document size.
///
/// Each source line is treated as at least one visual line, with extra lines
/// for wrapping based on byte length (no UTF-8 decode, so multi-byte text only
/// ever over-estimates — safe for windowing). Headings and code blocks deviate
/// by a few pixels, absorbed by [`SCROLL_OVERSCAN_LINES`].
fn estimate_segment_height(lines: &[String], start: usize, end: usize) -> f32 {
    let mut visual_lines = 0u32;
    for line in &lines[start..end] {
        let bytes = line.len() as u32;
        let wrapped = bytes.div_ceil(EST_CHARS_PER_LINE).max(1);
        visual_lines += wrapped;
    }
    visual_lines as f32 * EST_LINE_HEIGHT
}

/// Borrow the still-present source text of the `index`-th previous block.
///
/// Only called by the prefix/suffix scans in [`MarkdownEditor::rebuild_blocks`],
/// which run before any entry is taken, so the slot is always populated.
fn source_at(old: &[Option<(String, markdown::Content)>], index: usize) -> &str {
    old[index]
        .as_ref()
        .map(|(source, _)| source.as_str())
        .unwrap_or_default()
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

/// Map a global keyboard event to a [`Message::VimKey`], used in Normal mode
/// where no `text_editor` is focused (see [`MarkdownEditor::subscription`]).
///
/// Only *ignored* events are translated, so keys consumed by a focused widget
/// (such as an open theme dropdown) are left alone instead of also moving the
/// cursor.
fn on_key_event(
    event: iced::Event,
    status: iced::event::Status,
    _window: window::Id,
) -> Option<Message> {
    if status != iced::event::Status::Ignored {
        return None;
    }

    match event {
        iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key,
            modified_key,
            modifiers,
            ..
        }) => vim::to_vim_key_event(&key, &modified_key, modifiers).map(Message::VimKey),
        _ => None,
    }
}

/// Key bindings for the active region's editor while in **Insert mode**.
///
/// This is the document-aware editing layer the editor has always had, kept for
/// Insert mode so typing behaves naturally:
/// - **Esc** leaves Insert mode and returns to Normal mode (Vim).
/// - **Enter** splits a standalone line (adds a table row inside a table).
/// - **Backspace** at column 0 merges into the previous line.
/// - **Delete** at end of line merges the next line up.
/// - **↑/↓** move between lines from the region's top/bottom edge.
/// - **PgUp/PgDn** move the active line by a page; **Ctrl+Home/End** (and
///   **Ctrl+PgUp/PgDn**) jump to the document start/end.
///
/// Everything else (Home/End, Ctrl+←/→ word motion, Ctrl+C/X/V/A, typing)
/// falls through to the default editor bindings, which operate within the
/// active line. Application shortcuts that used to live here (Ctrl+S/O/N/B/I)
/// are replaced by Vim commands (`:w`, `:e`, …) and the toolbar.
fn insert_line_binding(
    key_press: text_editor::KeyPress,
    ctx: BindingContext,
) -> Option<text_editor::Binding<Message>> {
    use keyboard::key::Named;

    let key = key_press.key.clone();

    match key.as_ref() {
        // Esc returns to Normal mode instead of unfocusing the editor.
        keyboard::Key::Named(Named::Escape) => {
            Some(text_editor::Binding::Custom(Message::VimKey(vim::Key::Esc)))
        }
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
        keyboard::Key::Named(Named::PageUp) => Some(text_editor::Binding::Custom(
            if key_press.modifiers.command() {
                Message::JumpToStart
            } else {
                Message::PageUp
            },
        )),
        keyboard::Key::Named(Named::PageDown) => Some(text_editor::Binding::Custom(
            if key_press.modifiers.command() {
                Message::JumpToEnd
            } else {
                Message::PageDown
            },
        )),
        // Ctrl+Home / Ctrl+End jump to the document start/end. Plain Home/End
        // fall through to the default binding (start/end of the current line).
        keyboard::Key::Named(Named::Home) if key_press.modifiers.command() => {
            Some(text_editor::Binding::Custom(Message::JumpToStart))
        }
        keyboard::Key::Named(Named::End) if key_press.modifiers.command() => {
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
        editor.rebuild_blocks();

        // `rebuild_blocks` uses the mode-dependent split range, so the test must
        // segment the same way to count the expected blocks.
        let (start, end) = editor.render_split();
        let block_count = segments(&editor.lines, start, end)
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

    #[test]
    fn visible_line_window_renders_all_without_a_viewport() {
        // No layout has happened yet (first frame, or tests): render everything
        // so small documents and the test suite keep the original behaviour.
        assert_eq!(visible_line_window(None, 10_000), None);
    }

    #[test]
    fn visible_line_window_renders_all_when_window_covers_document() {
        // A small document scrolled to the top: the overscan already reaches
        // past the end, so there is nothing to skip.
        assert_eq!(visible_line_window(Some((0.0, 600.0)), 20), None);
    }

    #[test]
    fn visible_line_window_crops_a_scrolled_large_document() {
        // 8 000 lines, scrolled ~halfway down a 1 000 px viewport. The window
        // must be a bounded line range (not `None`) covering the visible lines.
        let win = visible_line_window(Some((50_000.0, 1_000.0)), 8_000);
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

    #[test]
    fn estimate_segment_height_counts_wrapped_lines() {
        let long = "x".repeat(200); // > EST_CHARS_PER_LINE → ≥3 visual lines
        let lines = to_lines(&format!("short\n{long}"));

        // One short line (1 visual) + one 200-char line (ceil(200/90) = 3 visual).
        let h = estimate_segment_height(&lines, 0, 2);
        assert_eq!(
            h,
            (1 + 200u32.div_ceil(EST_CHARS_PER_LINE)) as f32 * EST_LINE_HEIGHT
        );

        // A single short line is at least one line tall.
        let h0 = estimate_segment_height(&lines, 0, 1);
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
        editor.rebuild_blocks();
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
                editor.rebuild_blocks();
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

    // -- Mode-dependent rendering of the cursor line ---------------------
    //
    // In view mode (Normal) the cursor line is rendered as Markdown *inside its
    // block*, exactly like every other line: the block is never split at the
    // cursor, so moving the cursor through a (soft-wrapped) paragraph never
    // reflows it. The cursor is shown only by the left-margin marker. In edit
    // mode (Insert/Visual) the cursor line is split out into the `Active`
    // segment (the raw editor), so it leaves its block.

    fn send_vim(editor: &mut MarkdownEditor, key: vim::Key) {
        let _ = editor.update(Message::VimKey(key));
    }

    #[test]
    fn view_mode_does_not_split_the_cursor_block() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        // A soft-wrapped paragraph: two contiguous source lines forming one
        // block that renders (joined) as a single paragraph.
        editor.load_document("alpha line\nbeta line");
        editor.rebuild_blocks();
        assert_eq!(editor.vim_mode_label(), "NORMAL");

        // The whole paragraph is one block regardless of which line the cursor is
        // on — so the cursor moving never reflows it.
        let whole = vec!["alpha line\nbeta line".to_owned()];
        assert_eq!(editor.block_sources, whole);

        // Move the cursor down onto the second source line: still one block, the
        // exact same rendering.
        send_vim(&mut editor, vim::Key::Char('j'));
        assert_eq!(editor.active_start, 1);
        assert_eq!(editor.block_sources, whole);
        let _ = editor.view();
    }

    #[test]
    fn entering_insert_splits_the_cursor_line_out() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("alpha line\nbeta line");
        editor.rebuild_blocks();
        assert_eq!(
            editor.block_sources,
            vec!["alpha line\nbeta line".to_owned()]
        );

        // `i` enters Insert on line 0: it is split out as the raw editor, leaving
        // the remaining line as its own block.
        send_vim(&mut editor, vim::Key::Char('i'));
        assert_eq!(editor.vim_mode_label(), "INSERT");
        assert!(editor.vim_is_edit_mode());
        editor.rebuild_blocks();
        assert_eq!(editor.block_sources, vec!["beta line".to_owned()]);
        let _ = editor.view();
    }

    #[test]
    fn leaving_insert_rejoins_the_block() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("alpha line\nbeta line");
        send_vim(&mut editor, vim::Key::Char('i'));
        editor.rebuild_blocks();
        assert_eq!(editor.block_sources, vec!["beta line".to_owned()]);

        // Esc leaves Insert; the cursor line rejoins its block (view mode).
        send_vim(&mut editor, vim::Key::Esc);
        assert_eq!(editor.vim_mode_label(), "NORMAL");
        assert!(!editor.vim_is_edit_mode());
        editor.rebuild_blocks();
        assert_eq!(
            editor.block_sources,
            vec!["alpha line\nbeta line".to_owned()]
        );
        let _ = editor.view();
    }

    #[test]
    fn visual_mode_splits_the_cursor_line_out() {
        let (mut editor, _t) = MarkdownEditor::new_with_file(None);
        editor.load_document("alpha line\nbeta line");
        editor.rebuild_blocks();

        send_vim(&mut editor, vim::Key::Char('v'));
        assert_eq!(editor.vim_mode_label(), "VISUAL");
        assert!(editor.vim_is_edit_mode());
        editor.rebuild_blocks();
        assert_eq!(editor.block_sources, vec!["beta line".to_owned()]);
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
        editor.rebuild_blocks();
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
        editor.rebuild_blocks();
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
        // Reset to a known state — `new()` loads from the config file, which
        // may have values from a previous session.
        editor.colors = MarkdownColors::default();
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
        // Reset to a known state — `new()` loads from the config file, which
        // may have values from a previous session.
        editor.colors = MarkdownColors::default();
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

    // -- Incremental block rebuild --------------------------------------

    /// The block sources a *full* rebuild would produce, used as the ground
    /// truth the incremental [`MarkdownEditor::rebuild_blocks`] must match.
    /// Uses the same mode-dependent split range as `rebuild_blocks`.
    fn expected_block_sources(editor: &MarkdownEditor) -> Vec<String> {
        let (start, end) = editor.render_split();
        segments(&editor.lines, start, end)
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
        editor.rebuild_blocks();
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
        editor.rebuild_blocks();

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
        editor.rebuild_blocks();

        // Enter Insert mode: the cursor line ("body") is split out as the raw
        // editor, so the surrounding blocks no longer include it.
        send_vim(&mut editor, vim::Key::Char('i'));
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
        editor.rebuild_blocks();
        let lines = editor.lines.len();

        // Navigation hot path: each step rebuilds the blocks via the diff.
        let nav_steps = 1000;
        let t = Instant::now();
        for _ in 0..nav_steps {
            let _ = editor.update(Message::MoveDown);
        }
        let nav = t.elapsed();

        // Typing hot path: each edit now skips the block rebuild entirely.
        send_vim(&mut editor, vim::Key::Char('i'));
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
        editor.rebuild_blocks();
        let lines = editor.lines.len();

        // view() while typing in Insert mode (rebuild skipped, but view runs
        // every frame): the realistic per-keystroke cost the user feels.
        send_vim(&mut editor, vim::Key::Char('i'));
        let view_steps = 200;
        let t = Instant::now();
        for _ in 0..view_steps {
            let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
                text_editor::Edit::Insert('a'),
            )));
            let _element = editor.view();
        }
        let typing_view = t.elapsed();

        // view() while navigating in Normal mode (rebuild + view each step).
        send_vim(&mut editor, vim::Key::Esc);
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
        editor.rebuild_blocks();
        let lines = editor.lines.len();
        send_vim(&mut editor, vim::Key::Char('i'));

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
        editor.rebuild_blocks();

        // Navigate DOWN into and through the table (Normal mode), rebuilding the
        // blocks and the view at each step.
        let steps = 30;
        let t = Instant::now();
        for _ in 0..steps {
            let _ = editor.update(Message::VimKey(vim::Key::Char('j')));
            let _element = editor.view();
        }
        let down = t.elapsed();

        // Navigate back UP through the table.
        let t = Instant::now();
        for _ in 0..steps {
            let _ = editor.update(Message::VimKey(vim::Key::Char('k')));
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
        editor.rebuild_blocks();
        let open = t.elapsed();
        let lines = editor.lines.len();
        let blocks = editor.blocks.len();

        // First frame as the app renders it: load_document seeded a top-of-
        // document viewport, so only the lines near the top are built and
        // highlighted (not every code block in the file).
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
        println!("  open (load+rebuild):           {open:?}");
        println!("  first view (windowed to top):  {first_view:?}");
        println!("  full view (all blocks, forced): {full_view:?}");
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
        editor.rebuild_blocks();
        let t = Instant::now();
        for &p in &positions {
            editor.active_start = p;
            editor.active_end = p + 1;
            editor.rebuild_blocks();
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
