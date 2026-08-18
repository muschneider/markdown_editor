//! Undo/redo history for the document.
//!
//! Every edit this editor makes is a **line-range replacement**: the lines in
//! `start..start + before.len()` become some other run of lines. Recording that
//! shape — rather than a snapshot of the whole document — keeps an undo entry
//! proportional to the *edit*, not to the file. Typing a character in a 50,000
//! line document costs one short `String`, not 50,000.
//!
//! Consecutive keystrokes in the same place are **coalesced** into a single
//! entry, so one `Ctrl+Z` undoes a word or a line rather than a single
//! character (the granularity every other editor uses). A run is broken
//! explicitly by [`History::break_run`] — when the cursor moves to another
//! line, when the file is saved, and around undo/redo itself.

use std::collections::VecDeque;

/// Maximum number of undo entries retained. Entries are bounded by the size of
/// each edit, so this is a small, predictable amount of memory.
const MAX_ENTRIES: usize = 1000;

/// A reversible line-range replacement.
///
/// Applying it replaces `start..start + after_len` with [`Change::before`].
#[derive(Debug, Clone)]
pub struct Change {
    /// First line of the replaced range.
    pub start: usize,
    /// The lines that occupied the range *before* the change was made.
    pub before: Vec<String>,
    /// How many lines the range holds *after* the change.
    pub after_len: usize,
    /// Cursor position (line index, byte column) to restore.
    pub cursor: (usize, usize),
}

/// Undo and redo stacks for a document.
#[derive(Debug, Default)]
pub struct History {
    undo: VecDeque<Change>,
    redo: Vec<Change>,
    /// Whether the newest undo entry may still absorb further edits.
    open: bool,
}

impl History {
    /// Discard all history (a new document was loaded).
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
        self.open = false;
    }

    /// End the current run so the next edit starts a fresh undo entry.
    pub fn break_run(&mut self) {
        self.open = false;
    }

    /// Record that `start..start + before_len` became `start..start + after_len`.
    ///
    /// `before` is only invoked when a new entry is actually needed, so an
    /// absorbed keystroke never clones the replaced lines.
    ///
    /// An edit is absorbed into the newest entry when it replaces exactly the
    /// lines that entry produced — i.e. it continues the same run at the same
    /// place. Anything else starts a new entry.
    pub fn record<F>(&mut self, start: usize, before_len: usize, after_len: usize, before: F)
    where
        F: FnOnce() -> (Vec<String>, (usize, usize)),
    {
        self.redo.clear();

        if self.open
            && let Some(top) = self.undo.back_mut()
            && top.start == start
            && top.after_len == before_len
        {
            top.after_len = after_len;
            return;
        }

        let (before, cursor) = before();
        self.push(Change {
            start,
            before,
            after_len,
            cursor,
        });
        self.open = true;
    }

    /// Take the newest undo entry, if any. The caller applies it and hands the
    /// inverse back through [`History::push_redo`].
    pub fn pop_undo(&mut self) -> Option<Change> {
        self.open = false;
        self.undo.pop_back()
    }

    /// Take the newest redo entry, if any.
    pub fn pop_redo(&mut self) -> Option<Change> {
        self.open = false;
        self.redo.pop()
    }

    /// Push the inverse of an applied undo entry onto the redo stack.
    pub fn push_redo(&mut self, change: Change) {
        self.redo.push(change);
    }

    /// Push the inverse of an applied redo entry back onto the undo stack.
    pub fn push_undo(&mut self, change: Change) {
        self.push(change);
    }

    /// Whether an undo is available.
    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    /// Whether a redo is available.
    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Append an entry, dropping the oldest once the cap is reached.
    fn push(&mut self, change: Change) {
        if self.undo.len() == MAX_ENTRIES {
            self.undo.pop_front();
        }
        self.undo.push_back(change);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.split('\n').map(String::from).collect()
    }

    /// Consecutive edits at the same place collapse into one entry whose
    /// `before` is the state at the *start* of the run.
    #[test]
    fn consecutive_edits_coalesce_into_one_entry() {
        let mut history = History::default();

        history.record(3, 1, 1, || (lines("a"), (3, 1)));
        history.record(3, 1, 1, || (lines("ab"), (3, 2)));
        history.record(3, 1, 1, || (lines("abc"), (3, 3)));

        assert_eq!(history.undo.len(), 1);
        let change = history.pop_undo().unwrap();
        assert_eq!(change.before, lines("a"));
        assert_eq!(change.after_len, 1);
        assert_eq!(change.cursor, (3, 1));
    }

    /// A coalesced run tracks a growing range, so undo removes every line the
    /// run produced.
    #[test]
    fn coalescing_tracks_a_growing_range() {
        let mut history = History::default();

        history.record(0, 1, 2, || (lines("one line"), (0, 0)));
        history.record(0, 2, 3, || panic!("must be absorbed"));

        let change = history.pop_undo().unwrap();
        assert_eq!(change.before, lines("one line"));
        assert_eq!(change.after_len, 3);
    }

    /// Breaking the run starts a new entry even at the same position.
    #[test]
    fn break_run_starts_a_new_entry() {
        let mut history = History::default();

        history.record(2, 1, 1, || (lines("first"), (2, 0)));
        history.break_run();
        history.record(2, 1, 1, || (lines("second"), (2, 0)));

        assert_eq!(history.undo.len(), 2);
    }

    /// An edit somewhere else is never absorbed.
    #[test]
    fn edits_at_a_different_line_start_a_new_entry() {
        let mut history = History::default();

        history.record(1, 1, 1, || (lines("here"), (1, 0)));
        history.record(9, 1, 1, || (lines("there"), (9, 0)));

        assert_eq!(history.undo.len(), 2);
    }

    /// Recording after an undo drops the redo stack (the future is rewritten).
    #[test]
    fn recording_clears_the_redo_stack() {
        let mut history = History::default();

        history.record(0, 1, 1, || (lines("x"), (0, 0)));
        let change = history.pop_undo().unwrap();
        history.push_redo(change);
        assert!(history.can_redo());

        history.record(0, 1, 1, || (lines("y"), (0, 0)));
        assert!(!history.can_redo());
    }

    /// The stack is bounded; the oldest entries are dropped.
    #[test]
    fn history_is_bounded() {
        let mut history = History::default();

        for i in 0..(MAX_ENTRIES + 50) {
            history.break_run();
            history.record(i, 1, 1, || (lines("x"), (i, 0)));
        }

        assert_eq!(history.undo.len(), MAX_ENTRIES);
        // The surviving entries are the newest ones.
        assert_eq!(history.undo.back().unwrap().start, MAX_ENTRIES + 49);
    }
}
