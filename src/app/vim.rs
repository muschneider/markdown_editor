//! Vim-style modal editing for the Markdown editor.
//!
//! This module turns the inline editor into a modal, Vim-like one. It is a
//! sibling-file submodule of [`crate::app`] (`src/app/vim.rs`), so it can reach
//! the private fields and helpers of [`MarkdownEditor`] directly while keeping
//! the bulk of the modal logic out of `app.rs`.
//!
//! ## Design
//!
//! Every key press in any mode **except Insert** is routed to
//! [`MarkdownEditor::handle_vim_key`] (see [`to_vim_key`]). Insert mode keeps the
//! original block-editing bindings so typing, Enter/Backspace line joining and
//! the arrow keys behave exactly as before; only <kbd>Esc</kbd> is intercepted
//! to return to Normal mode.
//!
//! The cursor is treated in **document coordinates** — a `(row, column)` pair of
//! char indices into [`MarkdownEditor::lines`] — and motions/operators are
//! computed there, then reflected by re-activating the target line via
//! [`MarkdownEditor::activate_line`]. This reuses the existing block/table
//! machinery, so a `j` that lands inside a table expands the raw region to the
//! whole table for free, exactly like an arrow-key move does.
//!
//! ## Supported keys
//!
//! - **Motions**: `h j k l`, `w b e`, `0 ^ $`, `gg G`, arrows, with counts.
//! - **Operators**: `d c y` + motion, plus `dd cc yy`, `D C Y`, `x s S`, `r`.
//! - **Insert entry**: `i I a A o O`.
//! - **Paste / undo**: `p P`, `u`, `Ctrl-r`.
//! - **Visual**: `v` (charwise), `V` (linewise) with `d y c x o` and motions.
//! - **Command line**: `:w :q :wq :x :q! :e/:new`, `:<n>` to jump to a line.
//! - **Search**: `/pat`, `?pat`, repeated with `n` / `N`.
//!
//! Word motions and char-wise operators act within the current line (they clamp
//! at line ends); this keeps ranges unambiguous in the block-rendered document.

use iced::Task;
use iced::widget::text_editor;

use super::{MarkdownEditor, Message};

/// The active editing mode. Command-line and search entry are represented by
/// [`VimState::command`] being `Some`, with the underlying `mode` staying
/// `Normal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Default mode: keys are commands, not text.
    Normal,
    /// Text typed into the buffer is inserted (original editor behavior).
    Insert,
    /// A selection is being extended (char-wise or line-wise).
    Visual,
}

/// A pending operator awaiting a motion (or a doubled key for line-wise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    /// `d` — delete the spanned text into the register.
    Delete,
    /// `c` — delete the spanned text and enter Insert mode.
    Change,
    /// `y` — copy the spanned text into the register.
    Yank,
}

/// A single cursor motion, in document space.
#[derive(Debug, Clone, Copy)]
enum Move {
    Left,
    Right,
    Up,
    Down,
    WordFwd,
    WordBack,
    WordEnd,
    LineStart,
    FirstNonBlank,
    LineEnd,
    DocStart,
    DocEnd,
}

impl Move {
    /// Whether the motion spans whole lines (affects operator semantics).
    fn linewise(self) -> bool {
        matches!(self, Move::Up | Move::Down | Move::DocStart | Move::DocEnd)
    }
}

/// The kind of command-line entry currently open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CmdKind {
    /// `:` — an Ex command.
    Ex,
    /// `/` — forward search.
    SearchForward,
    /// `?` — backward search.
    SearchBackward,
}

/// An open command-line, holding what the user has typed so far.
#[derive(Debug, Clone)]
pub(crate) struct CommandLine {
    pub(crate) kind: CmdKind,
    pub(crate) buffer: String,
}

/// The unnamed register: text yanked or deleted, ready to paste.
#[derive(Debug, Clone, Default)]
struct Register {
    /// The stored text (lines joined by `\n` when `linewise`).
    text: String,
    /// Whether the text represents whole lines (`p` opens new lines).
    linewise: bool,
    /// Set once anything has been yanked/deleted, so `p` on a fresh editor with
    /// an empty char-wise register is a no-op rather than inserting nothing.
    filled: bool,
}

/// State carried between key presses while resolving a multi-key command.
#[derive(Debug, Clone, Copy, Default)]
struct Pending {
    /// Numeric count typed before/within a command (e.g. the `3` in `3w`).
    count: Option<usize>,
    /// Operator awaiting a motion.
    operator: Option<Operator>,
    /// Count typed before the operator (the `2` in `2dw`).
    op_count: Option<usize>,
    /// `g` was pressed; the next `g` completes `gg`.
    g: bool,
    /// `r` was pressed; the next char replaces the one under the cursor.
    replace: bool,
}

/// An active Visual-mode selection.
#[derive(Debug, Clone, Copy)]
struct Visual {
    /// Fixed end of the selection, in document `(row, col)` char coords.
    anchor: (usize, usize),
    /// Moving end of the selection (the cursor).
    head: (usize, usize),
    /// Whether whole lines are selected (`V`) vs characters (`v`).
    linewise: bool,
}

/// A document snapshot for undo/redo.
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

/// All Vim-related state attached to the editor.
#[derive(Debug, Clone)]
pub(crate) struct VimState {
    mode: Mode,
    pending: Pending,
    register: Register,
    visual: Option<Visual>,
    command: Option<CommandLine>,
    /// Last search `(forward, pattern)`, for `n` / `N`.
    last_search: Option<(bool, String)>,
    /// Transient status/error line (e.g. `E486: Pattern not found`).
    status: Option<String>,
}

impl VimState {
    /// Create the initial state: Normal mode, nothing pending.
    pub(crate) fn new() -> Self {
        Self {
            mode: Mode::Normal,
            pending: Pending::default(),
            register: Register::default(),
            visual: None,
            command: None,
            last_search: None,
            status: None,
        }
    }
}

impl Default for VimState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Key translation
// ---------------------------------------------------------------------------

/// A keyboard event reduced to what the Vim engine cares about. Characters are
/// already shift-resolved (so `Shift+;` arrives as `Char(':')`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Key {
    /// A printable character (shift applied).
    Char(char),
    /// <kbd>Esc</kbd>.
    Esc,
    /// <kbd>Enter</kbd>.
    Enter,
    /// <kbd>Backspace</kbd>.
    Backspace,
    /// <kbd>Tab</kbd>.
    Tab,
    /// An arrow key.
    Left,
    Right,
    Up,
    Down,
    /// A `Ctrl`-modified character (e.g. `Ctrl-r`).
    Ctrl(char),
}

/// Translate an iced key (unmodified + shift-applied) into a [`Key`], returning
/// `None` for keys the engine ignores.
fn map_key(
    key: &iced::keyboard::Key,
    modified_key: &iced::keyboard::Key,
    modifiers: iced::keyboard::Modifiers,
) -> Option<Key> {
    use iced::keyboard::Key as IKey;
    use iced::keyboard::key::Named;

    // Ctrl combinations use the unmodified key so layouts don't interfere.
    if modifiers.control() {
        if let IKey::Character(s) = key {
            return s.chars().next().map(Key::Ctrl);
        }
        return None;
    }

    match modified_key {
        IKey::Named(Named::Escape) => Some(Key::Esc),
        IKey::Named(Named::Enter) => Some(Key::Enter),
        IKey::Named(Named::Backspace) => Some(Key::Backspace),
        IKey::Named(Named::Tab) => Some(Key::Tab),
        IKey::Named(Named::Space) => Some(Key::Char(' ')),
        IKey::Named(Named::ArrowLeft) => Some(Key::Left),
        IKey::Named(Named::ArrowRight) => Some(Key::Right),
        IKey::Named(Named::ArrowUp) => Some(Key::Up),
        IKey::Named(Named::ArrowDown) => Some(Key::Down),
        IKey::Character(s) => s.chars().next().map(Key::Char),
        _ => None,
    }
}

/// Translate a focused [`text_editor::KeyPress`] (used in Visual mode, where the
/// editor is rendered for its selection) into a [`Key`].
pub(super) fn to_vim_key(key_press: &text_editor::KeyPress) -> Option<Key> {
    if !matches!(key_press.status, text_editor::Status::Focused { .. }) {
        return None;
    }
    map_key(&key_press.key, &key_press.modified_key, key_press.modifiers)
}

/// Translate a global keyboard event (used in Normal mode, where no editor is
/// focused and keys arrive via a subscription) into a [`Key`].
pub(super) fn to_vim_key_event(
    key: &iced::keyboard::Key,
    modified_key: &iced::keyboard::Key,
    modifiers: iced::keyboard::Modifiers,
) -> Option<Key> {
    map_key(key, modified_key, modifiers)
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

impl MarkdownEditor {
    /// Whether the editor is currently in Insert mode (Insert keeps the original
    /// text-editing key bindings).
    pub(super) fn vim_is_insert(&self) -> bool {
        self.vim.mode == Mode::Insert
    }

    /// Whether an **edit mode** — Insert or Visual — is active.
    ///
    /// The cursor line is always shown as a focused raw `text_editor`; this marks
    /// the modes that decorate it with the highlighted box and that fully own
    /// keyboard input (so the global Normal-mode key subscription is disabled).
    /// Insert keeps the document-aware text-editing bindings; Visual routes keys
    /// to the Vim engine for its selection.
    pub(super) fn vim_is_edit_mode(&self) -> bool {
        matches!(self.vim.mode, Mode::Insert | Mode::Visual)
    }

    /// A short label for the current mode, shown in the status bar.
    pub(super) fn vim_mode_label(&self) -> &'static str {
        match self.vim.mode {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Visual => {
                if self.vim.visual.map(|v| v.linewise).unwrap_or(false) {
                    "V-LINE"
                } else {
                    "VISUAL"
                }
            }
        }
    }

    /// The command-line text being typed (e.g. `":wq"` or `"/foo"`), if any.
    pub(super) fn vim_command_line(&self) -> Option<String> {
        self.vim.command.as_ref().map(|c| {
            let prefix = match c.kind {
                CmdKind::Ex => ':',
                CmdKind::SearchForward => '/',
                CmdKind::SearchBackward => '?',
            };
            format!("{prefix}{}", c.buffer)
        })
    }

    /// The transient status/error message, if one is set.
    pub(super) fn vim_message(&self) -> Option<&str> {
        self.vim.status.as_deref()
    }

    /// Entry point: handle one [`Key`] according to the current mode.
    pub(super) fn handle_vim_key(&mut self, key: Key) -> Task<Message> {
        // A new key press clears any previous transient message.
        self.vim.status = None;

        let was_edit_mode = self.vim_is_edit_mode();

        let task = if self.vim.command.is_some() {
            self.command_key(key)
        } else {
            match self.vim.mode {
                Mode::Insert => {
                    if key == Key::Esc {
                        self.leave_insert();
                    }
                    Task::none()
                }
                Mode::Normal => self.normal_key(key),
                Mode::Visual => self.visual_key(key),
            }
        };

        // Entering an edit mode (Insert/Visual) disables the global key
        // subscription, so make sure the editor is focused to receive keys —
        // in case input had just arrived via that subscription (focus lost).
        if self.vim_is_edit_mode() && !was_edit_mode {
            Task::batch([task, self.focus_active()])
        } else {
            task
        }
    }

    // -- Normal mode ------------------------------------------------------

    /// Dispatch a key in Normal mode.
    fn normal_key(&mut self, key: Key) -> Task<Message> {
        // `r{char}`: replace the char under the cursor.
        if self.vim.pending.replace {
            self.vim.pending.replace = false;
            if let Key::Char(c) = key {
                self.vim_replace_char(c);
            }
            return Task::none();
        }

        // `g{char}`: only `gg` is supported.
        if self.vim.pending.g {
            self.vim.pending.g = false;
            if key == Key::Char('g') {
                return self.apply_move(Move::DocStart);
            }
            self.clear_pending();
            return Task::none();
        }

        match key {
            Key::Char(c) => self.normal_char(c),
            Key::Left | Key::Backspace => self.apply_move(Move::Left),
            Key::Right => self.apply_move(Move::Right),
            Key::Up => self.apply_move(Move::Up),
            Key::Down | Key::Enter => self.apply_move(Move::Down),
            Key::Ctrl('r') => {
                self.vim_redo();
                Task::none()
            }
            // Standard Vim scrolling: half/full page down/up.
            Key::Ctrl('d') => {
                self.clear_pending();
                self.move_cursor(Move::Down, super::PAGE_LINES / 2);
                Task::none()
            }
            Key::Ctrl('u') => {
                self.clear_pending();
                self.move_cursor(Move::Up, super::PAGE_LINES / 2);
                Task::none()
            }
            Key::Ctrl('f') => {
                self.clear_pending();
                self.move_cursor(Move::Down, super::PAGE_LINES);
                Task::none()
            }
            Key::Ctrl('b') => {
                self.clear_pending();
                self.move_cursor(Move::Up, super::PAGE_LINES);
                Task::none()
            }
            Key::Esc => {
                self.clear_pending();
                Task::none()
            }
            Key::Ctrl(_) | Key::Tab => Task::none(),
        }
    }

    /// Handle a single printable character in Normal mode.
    fn normal_char(&mut self, c: char) -> Task<Message> {
        // While an operator is pending, only counts, motions, `g` and a doubled
        // operator are meaningful; anything else cancels the operator.
        if self.vim.pending.operator.is_some() {
            let allowed = c.is_ascii_digit()
                || matches!(
                    c,
                    'h' | 'l'
                        | 'j'
                        | 'k'
                        | 'w'
                        | 'b'
                        | 'e'
                        | '0'
                        | '^'
                        | '$'
                        | 'G'
                        | 'g'
                        | 'd'
                        | 'c'
                        | 'y'
                );
            if !allowed {
                self.clear_pending();
                return Task::none();
            }
        }

        match c {
            '1'..='9' => {
                self.push_count(c);
                Task::none()
            }
            '0' if self.has_count() => {
                self.push_count('0');
                Task::none()
            }
            'd' => self.operator(Operator::Delete),
            'c' => self.operator(Operator::Change),
            'y' => self.operator(Operator::Yank),
            '0' => self.apply_move(Move::LineStart),
            '^' => self.apply_move(Move::FirstNonBlank),
            '$' => self.apply_move(Move::LineEnd),
            'h' => self.apply_move(Move::Left),
            'l' | ' ' => self.apply_move(Move::Right),
            'j' => self.apply_move(Move::Down),
            'k' => self.apply_move(Move::Up),
            'w' => self.apply_move(Move::WordFwd),
            'b' => self.apply_move(Move::WordBack),
            'e' => self.apply_move(Move::WordEnd),
            'G' => self.apply_move(Move::DocEnd),
            'g' => {
                self.vim.pending.g = true;
                Task::none()
            }
            'x' => {
                self.vim_delete_char();
                Task::none()
            }
            'r' => {
                self.vim.pending.count = None;
                self.vim.pending.replace = true;
                Task::none()
            }
            'p' => {
                self.vim_paste(true);
                Task::none()
            }
            'P' => {
                self.vim_paste(false);
                Task::none()
            }
            'i' => {
                let (r, col) = self.doc_cursor_chars();
                self.enter_insert_at(r, col)
            }
            'I' => {
                let (r, _) = self.doc_cursor_chars();
                let col = first_non_blank(&self.lines[r]);
                self.enter_insert_at(r, col)
            }
            'a' => {
                let (r, col) = self.doc_cursor_chars();
                let n = char_count(&self.lines[r]);
                self.enter_insert_at(r, (col + 1).min(n))
            }
            'A' => {
                let (r, _) = self.doc_cursor_chars();
                let n = char_count(&self.lines[r]);
                self.enter_insert_at(r, n)
            }
            'o' => self.open_line(true),
            'O' => self.open_line(false),
            'D' => {
                self.vim.pending.count = None;
                let (r, col) = self.doc_cursor_chars();
                let n = char_count(&self.lines[r]);
                self.apply_charwise(Operator::Delete, r, col, n)
            }
            'C' => {
                self.vim.pending.count = None;
                let (r, col) = self.doc_cursor_chars();
                let n = char_count(&self.lines[r]);
                self.apply_charwise(Operator::Change, r, col, n)
            }
            's' => {
                let (r, col) = self.doc_cursor_chars();
                let n = char_count(&self.lines[r]);
                if n == 0 {
                    self.enter_insert_at(r, 0)
                } else {
                    let count = self.count1();
                    let end = (col + count).min(n);
                    self.apply_charwise(Operator::Change, r, col, end)
                }
            }
            'S' => {
                let count = self.count1();
                let (r, _) = self.doc_cursor_chars();
                let r1 = (r + count - 1).min(self.lines.len() - 1);
                self.apply_linewise(Operator::Change, r, r1)
            }
            'Y' => {
                let count = self.count1();
                let (r, _) = self.doc_cursor_chars();
                let r1 = (r + count - 1).min(self.lines.len() - 1);
                self.apply_linewise(Operator::Yank, r, r1)
            }
            'u' => {
                self.vim.pending.count = None;
                self.vim_undo();
                Task::none()
            }
            'v' => {
                self.enter_visual(false);
                Task::none()
            }
            'V' => {
                self.enter_visual(true);
                Task::none()
            }
            ':' => {
                self.start_command(CmdKind::Ex);
                Task::none()
            }
            '/' => {
                self.start_command(CmdKind::SearchForward);
                Task::none()
            }
            '?' => {
                self.start_command(CmdKind::SearchBackward);
                Task::none()
            }
            'n' => {
                self.vim.pending.count = None;
                self.repeat_search(true);
                Task::none()
            }
            'N' => {
                self.vim.pending.count = None;
                self.repeat_search(false);
                Task::none()
            }
            _ => {
                self.clear_pending();
                Task::none()
            }
        }
    }

    /// Apply a motion, or — if an operator is pending — operate over its range.
    fn apply_move(&mut self, m: Move) -> Task<Message> {
        if let Some(op) = self.vim.pending.operator {
            let count = self.combined_op_count();
            let task = self.apply_operator_motion(op, m, count);
            self.clear_pending();
            return task;
        }
        // `{n}G` / `{n}gg` jump to an absolute (1-based) line number.
        if matches!(m, Move::DocStart | Move::DocEnd)
            && let Some(n) = self.vim.pending.count.take()
        {
            let last = self.lines.len() - 1;
            let row = n.saturating_sub(1).min(last);
            let col = first_non_blank(&self.lines[row]);
            let col = self.rest_clamp(row, col);
            self.activate_char(row, col);
            return Task::none();
        }
        let count = self.count1();
        self.move_cursor(m, count);
        Task::none()
    }

    /// Begin (or, when doubled, immediately apply) an operator.
    fn operator(&mut self, op: Operator) -> Task<Message> {
        if self.vim.pending.operator == Some(op) {
            // Doubled operator (`dd`, `yy`, `cc`): line-wise over `count` lines.
            let count = self.count1();
            let task = self.apply_linewise_operator(op, count);
            self.clear_pending();
            return task;
        }
        self.vim.pending.operator = Some(op);
        self.vim.pending.op_count = self.vim.pending.count.take();
        Task::none()
    }

    /// Apply a doubled operator over `count` whole lines from the cursor.
    fn apply_linewise_operator(&mut self, op: Operator, count: usize) -> Task<Message> {
        let (row, _) = self.doc_cursor_chars();
        let r1 = (row + count.saturating_sub(1)).min(self.lines.len() - 1);
        self.apply_linewise(op, row, r1)
    }

    /// Resolve an operator + motion into a concrete range and apply it.
    fn apply_operator_motion(&mut self, op: Operator, m: Move, count: usize) -> Task<Message> {
        let (row, col) = self.doc_cursor_chars();

        if m.linewise() {
            let last = self.lines.len() - 1;
            let (r0, r1) = match m {
                Move::Down => (row, (row + count).min(last)),
                Move::Up => (row.saturating_sub(count), row),
                Move::DocEnd => (row, last),
                Move::DocStart => (0, row),
                _ => (row, row),
            };
            return self.apply_linewise(op, r0, r1);
        }

        let line = self.lines[row].clone();
        let n = char_count(&line);
        let (c0, c1) = match m {
            Move::Right => (col, (col + count).min(n)),
            Move::Left => (col.saturating_sub(count), col),
            Move::LineEnd => (col, n),
            Move::LineStart => (0, col),
            Move::FirstNonBlank => {
                let f = first_non_blank(&line);
                (f.min(col), f.max(col))
            }
            Move::WordFwd => {
                // Vim special case: `cw` on a non-blank acts like `ce`, changing
                // to the end of the word without swallowing trailing whitespace.
                let on_word = line.chars().nth(col).map(|c| !c.is_whitespace());
                if op == Operator::Change && on_word.unwrap_or(false) {
                    let mut t = col;
                    for _ in 0..count {
                        t = word_end(&line, t);
                    }
                    (col, (t + 1).min(n))
                } else {
                    let mut t = col;
                    for _ in 0..count {
                        t = word_forward(&line, t);
                    }
                    (col, t.min(n))
                }
            }
            Move::WordBack => {
                let mut t = col;
                for _ in 0..count {
                    t = word_backward(&line, t);
                }
                (t.min(col), col)
            }
            Move::WordEnd => {
                let mut t = col;
                for _ in 0..count {
                    t = word_end(&line, t);
                }
                (col, (t + 1).min(n))
            }
            _ => (col, col),
        };

        if c1 > c0 {
            self.apply_charwise(op, row, c0, c1)
        } else if op == Operator::Change {
            // `cw` on an empty word still drops into Insert at the cursor.
            self.push_undo();
            self.vim.mode = Mode::Insert;
            self.activate_char(row, col);
            Task::none()
        } else {
            Task::none()
        }
    }

    /// Move the cursor by `count` repetitions of `m`, resting on a real char.
    fn move_cursor(&mut self, m: Move, count: usize) {
        let (mut row, mut col) = self.doc_cursor_chars();
        for _ in 0..count {
            let (r, c) = self.moved(m, row, col);
            row = r;
            col = c;
        }
        let col = self.rest_clamp(row, col);
        self.activate_char(row, col);
    }

    /// Compute the target of a single motion from `(row, col)` (char coords).
    fn moved(&self, m: Move, row: usize, col: usize) -> (usize, usize) {
        let last_row = self.lines.len() - 1;
        let line = &self.lines[row];
        let n = char_count(line);
        match m {
            Move::Left => (row, col.saturating_sub(1)),
            Move::Right => (row, if n == 0 { 0 } else { (col + 1).min(n - 1) }),
            Move::Up => (row.saturating_sub(1), col),
            Move::Down => ((row + 1).min(last_row), col),
            Move::WordFwd => (row, word_forward(line, col)),
            Move::WordBack => (row, word_backward(line, col)),
            Move::WordEnd => (row, word_end(line, col)),
            Move::LineStart => (row, 0),
            Move::FirstNonBlank => (row, first_non_blank(line)),
            Move::LineEnd => (row, if n == 0 { 0 } else { n - 1 }),
            Move::DocStart => (0, first_non_blank(&self.lines[0])),
            Move::DocEnd => (last_row, first_non_blank(&self.lines[last_row])),
        }
    }

    // -- Edits ------------------------------------------------------------

    /// `x`: delete `count` characters from under the cursor.
    fn vim_delete_char(&mut self) {
        let (row, col) = self.doc_cursor_chars();
        let count = self.count1();
        let n = char_count(&self.lines[row]);
        if n == 0 || col >= n {
            return;
        }
        let end = (col + count).min(n);
        let b0 = char_to_byte(&self.lines[row], col);
        let b1 = char_to_byte(&self.lines[row], end);
        self.push_undo();
        let text = self.lines[row][b0..b1].to_string();
        self.set_register(text, false);
        self.lines[row].replace_range(b0..b1, "");
        self.is_dirty = true;
        let col = self.rest_clamp(row, col);
        self.activate_char(row, col);
    }

    /// `r{char}`: replace the single character under the cursor.
    fn vim_replace_char(&mut self, ch: char) {
        let (row, col) = self.doc_cursor_chars();
        let n = char_count(&self.lines[row]);
        if col >= n {
            return;
        }
        let b0 = char_to_byte(&self.lines[row], col);
        let b1 = char_to_byte(&self.lines[row], col + 1);
        self.push_undo();
        self.lines[row].replace_range(b0..b1, &ch.to_string());
        self.is_dirty = true;
        self.activate_char(row, col);
    }

    /// Apply a char-wise operator over `[c0, c1)` on `row`.
    fn apply_charwise(&mut self, op: Operator, row: usize, c0: usize, c1: usize) -> Task<Message> {
        let b0 = char_to_byte(&self.lines[row], c0);
        let b1 = char_to_byte(&self.lines[row], c1);
        let text = self.lines[row][b0..b1].to_string();
        match op {
            Operator::Yank => {
                self.set_register(text, false);
                let col = self.rest_clamp(row, c0);
                self.activate_char(row, col);
            }
            Operator::Delete => {
                self.push_undo();
                self.set_register(text, false);
                self.lines[row].replace_range(b0..b1, "");
                self.is_dirty = true;
                let col = self.rest_clamp(row, c0);
                self.activate_char(row, col);
            }
            Operator::Change => {
                self.push_undo();
                self.set_register(text, false);
                self.lines[row].replace_range(b0..b1, "");
                self.is_dirty = true;
                self.vim.mode = Mode::Insert;
                self.activate_char(row, c0);
            }
        }
        Task::none()
    }

    /// Apply a line-wise operator over the inclusive line range `r0..=r1`.
    fn apply_linewise(&mut self, op: Operator, r0: usize, r1: usize) -> Task<Message> {
        let last = self.lines.len() - 1;
        let r1 = r1.min(last);
        let r0 = r0.min(r1);
        let text = self.lines[r0..=r1].join("\n");
        match op {
            Operator::Yank => {
                self.set_register(text, true);
                let col = first_non_blank(&self.lines[r0]);
                let col = self.rest_clamp(r0, col);
                self.activate_char(r0, col);
            }
            Operator::Delete => {
                self.push_undo();
                self.set_register(text, true);
                self.lines.drain(r0..=r1);
                if self.lines.is_empty() {
                    self.lines.push(String::new());
                }
                self.is_dirty = true;
                let row = r0.min(self.lines.len() - 1);
                let col = first_non_blank(&self.lines[row]);
                let col = self.rest_clamp(row, col);
                self.activate_char(row, col);
            }
            Operator::Change => {
                self.push_undo();
                self.set_register(text, true);
                self.lines.splice(r0..=r1, std::iter::once(String::new()));
                self.is_dirty = true;
                self.vim.mode = Mode::Insert;
                self.activate_char(r0, 0);
            }
        }
        Task::none()
    }

    /// `i`/`a`/`I`/`A`: snapshot for undo and enter Insert mode at `(row, col)`.
    fn enter_insert_at(&mut self, row: usize, col: usize) -> Task<Message> {
        self.push_undo();
        self.vim.pending.count = None;
        self.vim.mode = Mode::Insert;
        self.activate_char(row, col);
        Task::none()
    }

    /// `o`/`O`: open a blank line below/above and enter Insert mode.
    fn open_line(&mut self, below: bool) -> Task<Message> {
        self.push_undo();
        self.vim.pending.count = None;
        let (row, _) = self.doc_cursor_chars();
        let at = if below { row + 1 } else { row };
        self.lines.insert(at, String::new());
        self.is_dirty = true;
        self.vim.mode = Mode::Insert;
        self.activate_char(at, 0);
        Task::none()
    }

    /// Leave Insert mode, stepping the cursor left one column (Vim behavior).
    fn leave_insert(&mut self) {
        self.vim.mode = Mode::Normal;
        let (row, col) = self.doc_cursor_chars();
        let col = self.rest_clamp(row, col.saturating_sub(1));
        self.activate_char(row, col);
    }

    /// `p`/`P`: paste the register `count` times, after/before the cursor.
    fn vim_paste(&mut self, after: bool) {
        if !self.vim.register.filled {
            return;
        }
        let count = self.count1();
        self.push_undo();
        let (row, col) = self.doc_cursor_chars();

        if self.vim.register.linewise {
            let mut new_lines: Vec<String> = Vec::new();
            for _ in 0..count {
                for line in self.vim.register.text.split('\n') {
                    new_lines.push(line.to_string());
                }
            }
            let at = if after { row + 1 } else { row };
            self.lines.splice(at..at, new_lines);
            self.is_dirty = true;
            let col = first_non_blank(&self.lines[at]);
            let col = self.rest_clamp(at, col);
            self.activate_char(at, col);
        } else {
            let text = self.vim.register.text.repeat(count);
            let n = char_count(&self.lines[row]);
            let insert_col = if n == 0 {
                0
            } else if after {
                (col + 1).min(n)
            } else {
                col
            };
            let (er, ec) = self.insert_text_at(row, insert_col, &text);
            self.is_dirty = true;
            let ec = self.rest_clamp(er, ec.saturating_sub(1));
            self.activate_char(er, ec);
        }
    }

    /// Splice `text` (which may contain newlines) into `lines` at `(row, col)`,
    /// returning the `(row, col)` just past the inserted text (char coords).
    fn insert_text_at(&mut self, row: usize, col: usize, text: &str) -> (usize, usize) {
        let byte = char_to_byte(&self.lines[row], col);
        let suffix = self.lines[row][byte..].to_string();
        self.lines[row].truncate(byte);

        let segments: Vec<&str> = text.split('\n').collect();
        if segments.len() == 1 {
            self.lines[row].push_str(segments[0]);
            let end_col = col + char_count(segments[0]);
            self.lines[row].push_str(&suffix);
            (row, end_col)
        } else {
            self.lines[row].push_str(segments[0]);
            let mut at = row + 1;
            for seg in &segments[1..segments.len() - 1] {
                self.lines.insert(at, (*seg).to_string());
                at += 1;
            }
            let last = segments[segments.len() - 1];
            let end_col = char_count(last);
            let mut last_line = last.to_string();
            last_line.push_str(&suffix);
            self.lines.insert(at, last_line);
            (at, end_col)
        }
    }

    // -- Visual mode ------------------------------------------------------

    /// Enter Visual (`v`) or Visual-Line (`V`) mode anchored at the cursor.
    fn enter_visual(&mut self, linewise: bool) {
        self.vim.pending.count = None;
        let (row, col) = self.doc_cursor_chars();
        self.vim.visual = Some(Visual {
            anchor: (row, col),
            head: (row, col),
            linewise,
        });
        self.vim.mode = Mode::Visual;
        self.sync_visual();
    }

    /// Dispatch a key in Visual mode.
    fn visual_key(&mut self, key: Key) -> Task<Message> {
        if self.vim.pending.g {
            self.vim.pending.g = false;
            if key == Key::Char('g') {
                return self.visual_move(Move::DocStart);
            }
        }
        match key {
            Key::Esc => {
                self.exit_visual();
                Task::none()
            }
            Key::Char(c) => self.visual_char(c),
            Key::Left | Key::Backspace => self.visual_move(Move::Left),
            Key::Right => self.visual_move(Move::Right),
            Key::Up => self.visual_move(Move::Up),
            Key::Down | Key::Enter => self.visual_move(Move::Down),
            Key::Ctrl(_) | Key::Tab => Task::none(),
        }
    }

    /// Handle a printable character in Visual mode.
    fn visual_char(&mut self, c: char) -> Task<Message> {
        match c {
            '1'..='9' => {
                self.push_count(c);
                Task::none()
            }
            '0' if self.has_count() => {
                self.push_count('0');
                Task::none()
            }
            '0' => self.visual_move(Move::LineStart),
            '^' => self.visual_move(Move::FirstNonBlank),
            '$' => self.visual_move(Move::LineEnd),
            'h' => self.visual_move(Move::Left),
            'l' | ' ' => self.visual_move(Move::Right),
            'j' => self.visual_move(Move::Down),
            'k' => self.visual_move(Move::Up),
            'w' => self.visual_move(Move::WordFwd),
            'b' => self.visual_move(Move::WordBack),
            'e' => self.visual_move(Move::WordEnd),
            'G' => self.visual_move(Move::DocEnd),
            'g' => {
                self.vim.pending.g = true;
                Task::none()
            }
            'd' | 'x' => self.visual_apply(Operator::Delete),
            'y' => self.visual_apply(Operator::Yank),
            'c' | 's' => self.visual_apply(Operator::Change),
            'o' => {
                if let Some(v) = self.vim.visual.as_mut() {
                    std::mem::swap(&mut v.anchor, &mut v.head);
                }
                self.sync_visual();
                Task::none()
            }
            'v' => {
                match self.vim.visual {
                    Some(v) if v.linewise => {
                        self.vim.visual = Some(Visual {
                            linewise: false,
                            ..v
                        });
                        self.sync_visual();
                    }
                    _ => self.exit_visual(),
                }
                Task::none()
            }
            'V' => {
                match self.vim.visual {
                    Some(v) if !v.linewise => {
                        self.vim.visual = Some(Visual {
                            linewise: true,
                            ..v
                        });
                        self.sync_visual();
                    }
                    _ => self.exit_visual(),
                }
                Task::none()
            }
            ':' => Task::none(),
            _ => Task::none(),
        }
    }

    /// Move the Visual selection's head by `count` repetitions of `m`.
    fn visual_move(&mut self, m: Move) -> Task<Message> {
        let count = self.count1();
        let Some(mut v) = self.vim.visual else {
            return Task::none();
        };
        let (mut r, mut c) = v.head;
        for _ in 0..count {
            let (nr, nc) = self.moved(m, r, c);
            r = nr;
            c = nc;
        }
        v.head = (r, c);
        self.vim.visual = Some(v);
        self.sync_visual();
        Task::none()
    }

    /// Apply an operator to the Visual selection, then return to Normal mode
    /// (or Insert mode, for a change).
    fn visual_apply(&mut self, op: Operator) -> Task<Message> {
        let Some(v) = self.vim.visual.take() else {
            return Task::none();
        };
        self.vim.mode = Mode::Normal;
        let (a, b) = order_pos(v.anchor, v.head);

        if v.linewise {
            return self.apply_linewise(op, a.0, b.0);
        }

        let n = char_count(&self.lines[b.0]);
        let c1 = (b.1 + 1).min(n); // selection includes the head char
        let text = self.span_text(a.0, a.1, b.0, c1);
        match op {
            Operator::Yank => {
                self.set_register(text, false);
                let col = self.rest_clamp(a.0, a.1);
                self.activate_char(a.0, col);
            }
            Operator::Delete => {
                self.push_undo();
                self.set_register(text, false);
                self.delete_span(a.0, a.1, b.0, c1);
                self.is_dirty = true;
                let col = self.rest_clamp(a.0, a.1);
                self.activate_char(a.0, col);
            }
            Operator::Change => {
                self.push_undo();
                self.set_register(text, false);
                self.delete_span(a.0, a.1, b.0, c1);
                self.is_dirty = true;
                self.vim.mode = Mode::Insert;
                self.activate_char(a.0, a.1);
            }
        }
        Task::none()
    }

    /// Leave Visual mode, placing the cursor at the selection head.
    fn exit_visual(&mut self) {
        if let Some(v) = self.vim.visual.take() {
            let (r, c) = v.head;
            let c = self.rest_clamp(r, c);
            self.activate_char(r, c);
        }
        self.vim.mode = Mode::Normal;
    }

    /// Reflect the Visual selection in the editor: expand the raw region to span
    /// the selected lines and set the editor's native selection so it is shown
    /// highlighted as raw source.
    fn sync_visual(&mut self) {
        let Some(v) = self.vim.visual else {
            return;
        };
        let (a, b) = order_pos(v.anchor, v.head);
        let start = a.0;
        let end = b.0;

        self.active_start = start;
        self.active_end = end + 1;
        self.active_content = text_editor::Content::with_text(&self.lines[start..=end].join("\n"));

        let (position, anchor) = if v.linewise {
            let last_local = end - start;
            let end_len = char_count(&self.lines[end]);
            (
                text_editor::Position {
                    line: last_local,
                    column: char_to_byte(&self.lines[end], end_len),
                },
                text_editor::Position { line: 0, column: 0 },
            )
        } else {
            let head_end = (b.1 + 1).min(char_count(&self.lines[b.0]));
            (
                text_editor::Position {
                    line: b.0 - start,
                    column: char_to_byte(&self.lines[b.0], head_end),
                },
                text_editor::Position {
                    line: a.0 - start,
                    column: char_to_byte(&self.lines[a.0], a.1),
                },
            )
        };

        self.active_content.move_to(text_editor::Cursor {
            position,
            selection: Some(anchor),
        });
    }

    /// Text spanned by `(r0,c0)..(r1,c1)` (c1 exclusive), char coords.
    fn span_text(&self, r0: usize, c0: usize, r1: usize, c1: usize) -> String {
        if r0 == r1 {
            let line = &self.lines[r0];
            let b0 = char_to_byte(line, c0);
            let b1 = char_to_byte(line, c1);
            return line[b0..b1].to_string();
        }
        let mut out = String::new();
        let first = &self.lines[r0];
        out.push_str(&first[char_to_byte(first, c0)..]);
        out.push('\n');
        for line in &self.lines[r0 + 1..r1] {
            out.push_str(line);
            out.push('\n');
        }
        let last = &self.lines[r1];
        out.push_str(&last[..char_to_byte(last, c1)]);
        out
    }

    /// Delete the span `(r0,c0)..(r1,c1)` (c1 exclusive) from `lines`.
    fn delete_span(&mut self, r0: usize, c0: usize, r1: usize, c1: usize) {
        if r0 == r1 {
            let b0 = char_to_byte(&self.lines[r0], c0);
            let b1 = char_to_byte(&self.lines[r0], c1);
            self.lines[r0].replace_range(b0..b1, "");
            return;
        }
        let b0 = char_to_byte(&self.lines[r0], c0);
        let b1 = char_to_byte(&self.lines[r1], c1);
        let tail = self.lines[r1][b1..].to_string();
        self.lines[r0].truncate(b0);
        self.lines[r0].push_str(&tail);
        self.lines.drain(r0 + 1..=r1);
    }

    // -- Command line -----------------------------------------------------

    /// Open the command line for the given prompt kind.
    fn start_command(&mut self, kind: CmdKind) {
        self.vim.pending = Pending::default();
        self.vim.command = Some(CommandLine {
            kind,
            buffer: String::new(),
        });
    }

    /// Handle a key while the command line is open.
    fn command_key(&mut self, key: Key) -> Task<Message> {
        match key {
            Key::Char(c) => {
                if let Some(cmd) = self.vim.command.as_mut() {
                    cmd.buffer.push(c);
                }
                Task::none()
            }
            Key::Backspace => {
                // Backspacing past the prompt cancels the command line.
                let empty = self
                    .vim
                    .command
                    .as_ref()
                    .map(|c| c.buffer.is_empty())
                    .unwrap_or(true);
                if empty {
                    self.vim.command = None;
                } else if let Some(cmd) = self.vim.command.as_mut() {
                    cmd.buffer.pop();
                }
                Task::none()
            }
            Key::Esc => {
                self.vim.command = None;
                Task::none()
            }
            Key::Enter => self.execute_command(),
            _ => Task::none(),
        }
    }

    /// Run the typed command line on <kbd>Enter</kbd>.
    fn execute_command(&mut self) -> Task<Message> {
        let Some(cmd) = self.vim.command.take() else {
            return Task::none();
        };
        match cmd.kind {
            CmdKind::Ex => self.run_ex(cmd.buffer.trim()),
            CmdKind::SearchForward => self.start_search(cmd.buffer, true),
            CmdKind::SearchBackward => self.start_search(cmd.buffer, false),
        }
    }

    /// Execute an Ex command (`:w`, `:q`, `:wq`, `:e`, `:<n>`, …).
    fn run_ex(&mut self, cmd: &str) -> Task<Message> {
        match cmd {
            "" => Task::none(),
            "w" | "write" | "w!" => self.handle(Message::SaveFile),
            "q" | "quit" => {
                if self.is_dirty {
                    self.vim.status =
                        Some("E37: No write since last change (add ! to override)".to_string());
                    Task::none()
                } else {
                    self.quit_task()
                }
            }
            "q!" | "quit!" => self.quit_task(),
            "wq" | "x" | "wq!" | "x!" => {
                // Save (async); the FileSaved handler quits once it succeeds.
                self.pending_quit = true;
                self.handle(Message::SaveFile)
            }
            "e" | "e!" | "enew" | "new" => self.handle(Message::NewFile),
            other => {
                if let Ok(n) = other.parse::<usize>() {
                    let last = self.lines.len() - 1;
                    let row = n.saturating_sub(1).min(last);
                    let col = first_non_blank(&self.lines[row]);
                    let col = self.rest_clamp(row, col);
                    self.activate_char(row, col);
                    Task::none()
                } else {
                    self.vim.status = Some(format!("E492: Not an editor command: {other}"));
                    Task::none()
                }
            }
        }
    }

    /// A task that closes the window, ending the application.
    fn quit_task(&self) -> Task<Message> {
        iced::window::oldest().and_then(iced::window::close)
    }

    // -- Search -----------------------------------------------------------

    /// Begin a search; an empty pattern reuses the previous one.
    fn start_search(&mut self, pattern: String, forward: bool) -> Task<Message> {
        let pattern = if pattern.is_empty() {
            match &self.vim.last_search {
                Some((_, p)) => p.clone(),
                None => {
                    self.vim.status = Some("E35: No previous regular expression".to_string());
                    return Task::none();
                }
            }
        } else {
            pattern
        };
        self.vim.last_search = Some((forward, pattern.clone()));
        self.do_search(&pattern, forward);
        Task::none()
    }

    /// `n` / `N`: repeat the last search, same or opposite direction.
    fn repeat_search(&mut self, same_direction: bool) {
        let Some((forward, pattern)) = self.vim.last_search.clone() else {
            self.vim.status = Some("E35: No previous regular expression".to_string());
            return;
        };
        let forward = if same_direction { forward } else { !forward };
        self.do_search(&pattern, forward);
    }

    /// Move the cursor to the next match of `pattern` from the cursor, wrapping.
    fn do_search(&mut self, pattern: &str, forward: bool) {
        let (row, col) = self.doc_cursor_chars();
        match find_match(&self.lines, row, col, pattern, forward) {
            Some((r, byte_col)) => {
                let cc = byte_to_char(&self.lines[r], byte_col);
                self.activate_char(r, cc);
            }
            None => {
                self.vim.status = Some(format!("E486: Pattern not found: {pattern}"));
            }
        }
    }

    // -- Undo / redo ------------------------------------------------------

    /// Push the current document onto the undo stack (clearing redo).
    fn push_undo(&mut self) {
        let (row, col) = self.doc_cursor_chars();
        self.undo_stack.push(Snapshot {
            lines: self.lines.clone(),
            row,
            col,
        });
        // Bound memory: keep the most recent history only.
        if self.undo_stack.len() > 500 {
            self.undo_stack.remove(0);
        }
        self.redo_stack.clear();
    }

    /// `u`: restore the previous document snapshot.
    fn vim_undo(&mut self) {
        if let Some(snap) = self.undo_stack.pop() {
            let (row, col) = self.doc_cursor_chars();
            self.redo_stack.push(Snapshot {
                lines: self.lines.clone(),
                row,
                col,
            });
            self.lines = snap.lines;
            self.is_dirty = true;
            let row = snap.row.min(self.lines.len() - 1);
            let col = self.rest_clamp(row, snap.col);
            self.activate_char(row, col);
        } else {
            self.vim.status = Some("Already at oldest change".to_string());
        }
    }

    /// `Ctrl-r`: re-apply an undone change.
    fn vim_redo(&mut self) {
        if let Some(snap) = self.redo_stack.pop() {
            let (row, col) = self.doc_cursor_chars();
            self.undo_stack.push(Snapshot {
                lines: self.lines.clone(),
                row,
                col,
            });
            self.lines = snap.lines;
            self.is_dirty = true;
            let row = snap.row.min(self.lines.len() - 1);
            let col = self.rest_clamp(row, snap.col);
            self.activate_char(row, col);
        } else {
            self.vim.status = Some("Already at newest change".to_string());
        }
    }

    // -- Small helpers ----------------------------------------------------

    /// Store text in the unnamed register.
    fn set_register(&mut self, text: String, linewise: bool) {
        self.vim.register = Register {
            text,
            linewise,
            filled: true,
        };
    }

    /// The cursor in document `(row, col)` char coordinates.
    fn doc_cursor_chars(&self) -> (usize, usize) {
        let cursor = self.active_content.cursor();
        let row = (self.active_start + cursor.position.line).min(self.lines.len() - 1);
        let col = byte_to_char(&self.lines[row], cursor.position.column);
        (row, col)
    }

    /// Activate `row` and place the cursor at char index `col`.
    fn activate_char(&mut self, row: usize, col: usize) {
        let row = row.min(self.lines.len() - 1);
        let byte = char_to_byte(&self.lines[row], col);
        self.activate_line(row, byte);
    }

    /// Clamp `col` so a Normal-mode cursor rests on a real character.
    fn rest_clamp(&self, row: usize, col: usize) -> usize {
        let n = char_count(&self.lines[row]);
        if n == 0 { 0 } else { col.min(n - 1) }
    }

    /// Whether a numeric count is currently being accumulated.
    fn has_count(&self) -> bool {
        self.vim.pending.count.is_some()
    }

    /// Append a digit to the pending count.
    fn push_count(&mut self, c: char) {
        let digit = (c as usize) - ('0' as usize);
        let current = self.vim.pending.count.unwrap_or(0);
        let next = current
            .saturating_mul(10)
            .saturating_add(digit)
            .min(100_000);
        self.vim.pending.count = Some(next);
    }

    /// Take the pending count, defaulting to 1.
    fn count1(&mut self) -> usize {
        self.vim.pending.count.take().unwrap_or(1).max(1)
    }

    /// Take the combined operator × motion count (e.g. `2d3w` → 6).
    fn combined_op_count(&mut self) -> usize {
        let op = self.vim.pending.op_count.take().unwrap_or(1).max(1);
        let motion = self.vim.pending.count.take().unwrap_or(1).max(1);
        op.saturating_mul(motion)
    }

    /// Reset all pending command state.
    fn clear_pending(&mut self) {
        self.vim.pending = Pending::default();
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Character classes used for Vim word motions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punct,
}

/// Classify a character for word motions (keyword vs punctuation vs space).
fn class(c: char) -> Class {
    if c.is_whitespace() {
        Class::Space
    } else if c.is_alphanumeric() || c == '_' {
        Class::Word
    } else {
        Class::Punct
    }
}

/// Number of characters in `line`.
fn char_count(line: &str) -> usize {
    line.chars().count()
}

/// Byte offset of char index `ch` (clamped to the line length).
fn char_to_byte(line: &str, ch: usize) -> usize {
    line.char_indices()
        .nth(ch)
        .map(|(b, _)| b)
        .unwrap_or(line.len())
}

/// Char index of byte offset `byte` (snapped down to a char boundary).
fn byte_to_char(line: &str, byte: usize) -> usize {
    let mut b = byte.min(line.len());
    while b > 0 && !line.is_char_boundary(b) {
        b -= 1;
    }
    line[..b].chars().count()
}

/// Char index of the first non-blank character, or 0 if the line is all blank.
fn first_non_blank(line: &str) -> usize {
    for (i, c) in line.chars().enumerate() {
        if !c.is_whitespace() {
            return i;
        }
    }
    0
}

/// `w`: char index of the start of the next word (clamps to the line length).
fn word_forward(line: &str, col: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    if col >= n {
        return n;
    }
    let mut i = col;
    let start = class(chars[i]);
    if start != Class::Space {
        while i < n && class(chars[i]) == start {
            i += 1;
        }
    }
    while i < n && class(chars[i]) == Class::Space {
        i += 1;
    }
    i
}

/// `b`: char index of the start of the current/previous word.
fn word_backward(line: &str, col: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    if col == 0 {
        return 0;
    }
    let mut i = col - 1;
    while i > 0 && class(chars[i]) == Class::Space {
        i -= 1;
    }
    if class(chars[i]) == Class::Space {
        return 0;
    }
    let c = class(chars[i]);
    while i > 0 && class(chars[i - 1]) == c {
        i -= 1;
    }
    i
}

/// `e`: char index of the end of the next word.
fn word_end(line: &str, col: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    if n == 0 {
        return 0;
    }
    if col + 1 >= n {
        return n - 1;
    }
    let mut i = col + 1;
    while i < n && class(chars[i]) == Class::Space {
        i += 1;
    }
    if i >= n {
        return n - 1;
    }
    let c = class(chars[i]);
    while i + 1 < n && class(chars[i + 1]) == c {
        i += 1;
    }
    i
}

/// Order two `(row, col)` positions so the earlier one comes first.
fn order_pos(a: (usize, usize), b: (usize, usize)) -> ((usize, usize), (usize, usize)) {
    if a.0 < b.0 || (a.0 == b.0 && a.1 <= b.1) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Find the next occurrence of `pattern` from `(row, col)`, wrapping around the
/// document. Returns the match's `(row, byte_col)`.
fn find_match(
    lines: &[String],
    row: usize,
    col: usize,
    pattern: &str,
    forward: bool,
) -> Option<(usize, usize)> {
    if pattern.is_empty() {
        return None;
    }

    let cursor_byte = char_to_byte(&lines[row], col);

    let mut matches: Vec<(usize, usize)> = Vec::new();
    for (r, line) in lines.iter().enumerate() {
        let mut start = 0;
        while let Some(p) = line[start..].find(pattern) {
            let at = start + p;
            matches.push((r, at));
            start = at + 1;
            while start < line.len() && !line.is_char_boundary(start) {
                start += 1;
            }
            if start > line.len() {
                break;
            }
        }
    }

    if matches.is_empty() {
        return None;
    }

    if forward {
        matches
            .iter()
            .find(|&&(r, c)| r > row || (r == row && c > cursor_byte))
            .copied()
            .or_else(|| matches.first().copied())
    } else {
        matches
            .iter()
            .rev()
            .find(|&&(r, c)| r < row || (r == row && c < cursor_byte))
            .copied()
            .or_else(|| matches.last().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- Pure helpers -----------------------------------------------------

    #[test]
    fn word_forward_steps_over_words_and_punct() {
        // "foo, bar" — from 'f' to ',' to 'bar'.
        assert_eq!(word_forward("foo, bar", 0), 3); // 'f' -> ','
        assert_eq!(word_forward("foo, bar", 3), 5); // ',' -> 'b'
        assert_eq!(word_forward("foo, bar", 5), 8); // 'b' -> end
        // Trailing position clamps to the line length.
        assert_eq!(word_forward("foo", 1), 3);
    }

    #[test]
    fn word_backward_steps_to_word_starts() {
        assert_eq!(word_backward("foo bar", 6), 4); // within "bar" -> 'b'
        assert_eq!(word_backward("foo bar", 4), 0); // 'b' -> 'f'
        assert_eq!(word_backward("foo", 0), 0); // already at start
    }

    #[test]
    fn word_end_lands_on_last_word_char() {
        assert_eq!(word_end("foo bar", 0), 2); // 'f' -> 'o' (end of foo)
        assert_eq!(word_end("foo bar", 2), 6); // end of foo -> end of bar
    }

    #[test]
    fn first_non_blank_skips_indentation() {
        assert_eq!(first_non_blank("    code"), 4);
        assert_eq!(first_non_blank("code"), 0);
        assert_eq!(first_non_blank("     "), 0);
    }

    #[test]
    fn char_byte_roundtrip_handles_unicode() {
        let line = "héllo"; // 'é' is two bytes
        assert_eq!(char_count(line), 5);
        assert_eq!(char_to_byte(line, 2), 3); // after "hé"
        assert_eq!(byte_to_char(line, 3), 2);
        // A byte offset inside 'é' snaps down to the char boundary.
        assert_eq!(byte_to_char(line, 2), 1);
    }

    #[test]
    fn find_match_wraps_in_both_directions() {
        let lines: Vec<String> = ["alpha", "beta", "alpha"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // Forward from (0,0): next "alpha" is on row 2.
        assert_eq!(find_match(&lines, 0, 0, "alpha", true), Some((2, 0)));
        // Forward from (2,0) wraps to row 0.
        assert_eq!(find_match(&lines, 2, 0, "alpha", true), Some((0, 0)));
        // Backward from (2,0): previous "alpha" is row 0.
        assert_eq!(find_match(&lines, 2, 0, "alpha", false), Some((0, 0)));
        // Missing pattern yields nothing.
        assert_eq!(find_match(&lines, 0, 0, "zzz", true), None);
    }

    // -- Engine integration ----------------------------------------------

    fn editor_with(text: &str) -> MarkdownEditor {
        let (mut editor, _task) = MarkdownEditor::new_with_file(None);
        editor.load_document(text);
        editor.rebuild_blocks();
        editor
    }

    fn press(editor: &mut MarkdownEditor, keys: &str) {
        for c in keys.chars() {
            let _ = editor.update(Message::VimKey(Key::Char(c)));
        }
    }

    fn press_key(editor: &mut MarkdownEditor, key: Key) {
        let _ = editor.update(Message::VimKey(key));
    }

    #[test]
    fn starts_in_normal_mode() {
        let editor = editor_with("hello world");
        assert_eq!(editor.vim.mode, Mode::Normal);
    }

    #[test]
    fn dd_deletes_the_current_line() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "dd");
        assert_eq!(editor.lines, vec!["two", "three"]);
        assert_eq!(editor.vim.mode, Mode::Normal);
    }

    #[test]
    fn dd_then_paste_restores_line_below() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "dd"); // delete "one" (linewise register)
        press(&mut editor, "p"); // paste below current line ("two")
        assert_eq!(editor.lines, vec!["two", "one", "three"]);
    }

    #[test]
    fn x_deletes_character_under_cursor() {
        let mut editor = editor_with("hello");
        press(&mut editor, "x");
        assert_eq!(editor.lines, vec!["ello"]);
    }

    #[test]
    fn counted_x_deletes_multiple_chars() {
        let mut editor = editor_with("hello");
        press(&mut editor, "3x");
        assert_eq!(editor.lines, vec!["lo"]);
    }

    #[test]
    fn dw_deletes_a_word() {
        let mut editor = editor_with("foo bar baz");
        press(&mut editor, "dw");
        assert_eq!(editor.lines, vec!["bar baz"]);
    }

    #[test]
    fn insert_then_escape_inserts_text() {
        let mut editor = editor_with("bar");
        press(&mut editor, "i"); // Insert at column 0
        assert_eq!(editor.vim.mode, Mode::Insert);
        // Simulate typing via the editor action path, as the widget would.
        let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
            text_editor::Edit::Insert('X'),
        )));
        press_key(&mut editor, Key::Esc);
        assert_eq!(editor.vim.mode, Mode::Normal);
        assert_eq!(editor.lines, vec!["Xbar"]);
    }

    #[test]
    fn append_inserts_after_cursor() {
        let mut editor = editor_with("a");
        press(&mut editor, "a"); // append: cursor moves to column 1
        let _ = editor.update(Message::EditorAction(text_editor::Action::Edit(
            text_editor::Edit::Insert('b'),
        )));
        press_key(&mut editor, Key::Esc);
        assert_eq!(editor.lines, vec!["ab"]);
    }

    #[test]
    fn open_line_below_adds_a_line() {
        let mut editor = editor_with("one\ntwo");
        press(&mut editor, "o");
        assert_eq!(editor.vim.mode, Mode::Insert);
        assert_eq!(editor.lines, vec!["one", "", "two"]);
    }

    #[test]
    fn replace_char_swaps_one_character() {
        let mut editor = editor_with("cat");
        press(&mut editor, "r");
        press(&mut editor, "b"); // replacement char
        assert_eq!(editor.lines, vec!["bat"]);
        assert_eq!(editor.vim.mode, Mode::Normal);
    }

    #[test]
    fn undo_restores_previous_state() {
        let mut editor = editor_with("hello");
        press(&mut editor, "x"); // "ello"
        assert_eq!(editor.lines, vec!["ello"]);
        press(&mut editor, "u");
        assert_eq!(editor.lines, vec!["hello"]);
    }

    #[test]
    fn redo_reapplies_change() {
        let mut editor = editor_with("hello");
        press(&mut editor, "x");
        press(&mut editor, "u");
        press_key(&mut editor, Key::Ctrl('r'));
        assert_eq!(editor.lines, vec!["ello"]);
    }

    #[test]
    fn yank_line_and_paste() {
        let mut editor = editor_with("copy\nother");
        press(&mut editor, "yy");
        press(&mut editor, "p");
        assert_eq!(editor.lines, vec!["copy", "copy", "other"]);
    }

    #[test]
    fn visual_delete_removes_selection() {
        let mut editor = editor_with("hello");
        press(&mut editor, "v"); // visual, anchor on 'h'
        press(&mut editor, "ll"); // extend over 'e','l' (head on index 2)
        press(&mut editor, "d"); // delete inclusive selection "hel"
        assert_eq!(editor.lines, vec!["lo"]);
        assert_eq!(editor.vim.mode, Mode::Normal);
    }

    #[test]
    fn visual_line_delete_removes_whole_lines() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "V"); // visual line on "one"
        press(&mut editor, "j"); // extend to "two"
        press(&mut editor, "d"); // delete both
        assert_eq!(editor.lines, vec!["three"]);
    }

    #[test]
    fn motion_j_moves_active_line_down() {
        let mut editor = editor_with("one\ntwo\nthree");
        assert_eq!(editor.active_start, 0);
        press(&mut editor, "j");
        assert_eq!(editor.active_start, 1);
        press(&mut editor, "j");
        assert_eq!(editor.active_start, 2);
    }

    #[test]
    fn count_motion_jumps_multiple_lines() {
        let mut editor = editor_with("a\nb\nc\nd\ne");
        press(&mut editor, "3j");
        assert_eq!(editor.active_start, 3);
    }

    #[test]
    fn gg_and_shift_g_jump_to_ends() {
        let mut editor = editor_with("a\nb\nc\nd");
        press(&mut editor, "G");
        assert_eq!(editor.active_start, 3);
        press(&mut editor, "gg");
        assert_eq!(editor.active_start, 0);
    }

    #[test]
    fn count_prefixed_g_jumps_to_absolute_line() {
        let doc = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut editor = editor_with(&doc);
        press(&mut editor, "5G");
        assert_eq!(editor.active_start, 4); // 1-based line 5
        press(&mut editor, "2gg");
        assert_eq!(editor.active_start, 1); // 1-based line 2
    }

    #[test]
    fn ctrl_d_scrolls_down_half_page() {
        let doc = (0..40)
            .map(|i| format!("l{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut editor = editor_with(&doc);
        press_key(&mut editor, Key::Ctrl('d'));
        assert_eq!(editor.active_start, super::super::PAGE_LINES / 2);
    }

    #[test]
    fn ex_goto_line_activates_that_line() {
        let mut editor = editor_with("a\nb\nc\nd");
        press_key(&mut editor, Key::Char(':'));
        press(&mut editor, "3");
        press_key(&mut editor, Key::Enter);
        assert_eq!(editor.active_start, 2);
        assert!(editor.vim.command.is_none());
    }

    #[test]
    fn search_moves_to_match() {
        let mut editor = editor_with("alpha\nbeta\ngamma");
        press_key(&mut editor, Key::Char('/'));
        press(&mut editor, "gamma");
        press_key(&mut editor, Key::Enter);
        assert_eq!(editor.active_start, 2);
        // `n` wraps back to the same match.
        press(&mut editor, "n");
        assert_eq!(editor.active_start, 2);
    }

    #[test]
    fn change_word_enters_insert_and_clears_word() {
        let mut editor = editor_with("foo bar");
        press(&mut editor, "cw");
        assert_eq!(editor.vim.mode, Mode::Insert);
        assert_eq!(editor.lines, vec![" bar"]);
    }

    #[test]
    fn dollar_moves_to_line_end() {
        let mut editor = editor_with("hello");
        press(&mut editor, "$");
        let (_row, col) = editor.doc_cursor_chars();
        assert_eq!(col, 4); // last char index of "hello"
    }

    #[test]
    fn escape_clears_pending_operator() {
        let mut editor = editor_with("hello");
        press(&mut editor, "d"); // operator pending
        press_key(&mut editor, Key::Esc);
        press(&mut editor, "d"); // a lone 'd' now just re-arms; line intact
        assert_eq!(editor.lines, vec!["hello"]);
    }
}
