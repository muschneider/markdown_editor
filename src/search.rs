//! Find and find-and-replace over the document.
//!
//! Matches are recorded as **byte ranges within a single line** rather than
//! offsets into a joined document string: the editor stores the document as
//! `Vec<String>`, so a per-line range maps straight onto the line that has to
//! be activated and selected, and joining the whole document to search it
//! would allocate a copy of the file on every keystroke in the search box.
//!
//! Case-insensitive matching is **ASCII**-only on purpose. Unicode case folding
//! can change a string's byte length (`İ` lowercases to two chars), which would
//! invalidate the byte offsets the matches are made of; `eq_ignore_ascii_case`
//! compares in place and always preserves them.

use iced::widget::{button, container, row, space, text, text_input, tooltip};
use iced::{Element, Fill, Font, Length};

use crate::app::Message;
use crate::theme;

/// Widget id of the search query input, used to focus it when the bar opens.
pub const QUERY_ID: &str = "search-query";

/// One occurrence of the query: a byte range inside a single document line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// Index of the line the match is on.
    pub line: usize,
    /// Byte offset of the first byte of the match within the line.
    pub start: usize,
    /// Byte offset one past the last byte of the match.
    pub end: usize,
}

/// State of the find / find-and-replace bar.
#[derive(Debug, Default)]
pub struct Search {
    /// The text being searched for.
    pub query: String,
    /// The text matches are replaced with.
    pub replacement: String,
    /// Whether the replace row is shown (`Ctrl+H` rather than `Ctrl+F`).
    pub replacing: bool,
    /// Whether matching distinguishes upper from lower case.
    pub case_sensitive: bool,
    /// Every match, in document order.
    matches: Vec<Match>,
    /// Index into [`Search::matches`] of the currently highlighted one.
    current: usize,
}

impl Search {
    /// Open a fresh search, in replace mode when `replacing`.
    pub fn new(replacing: bool) -> Self {
        Self {
            replacing,
            ..Self::default()
        }
    }

    /// Recompute every match from `lines`, keeping the highlighted one as close
    /// as possible to where it was.
    pub fn refresh(&mut self, lines: &[String]) {
        let anchor = self.current_match().map(|m| m.line);

        self.matches.clear();
        if !self.query.is_empty() {
            for (index, line) in lines.iter().enumerate() {
                collect_line_matches(line, &self.query, self.case_sensitive, index, &mut self.matches);
            }
        }

        // Stay on (or just after) the line the highlight was on, so editing
        // does not throw the user back to the top of the document.
        self.current = match anchor {
            Some(line) => self
                .matches
                .iter()
                .position(|m| m.line >= line)
                .unwrap_or(0),
            None => 0,
        };
    }

    /// Point the highlight at the first match at or after `line`, so opening
    /// the bar starts from the cursor rather than the top of the file.
    pub fn anchor_to(&mut self, line: usize) {
        self.current = self
            .matches
            .iter()
            .position(|m| m.line >= line)
            .unwrap_or(0);
    }

    /// The currently highlighted match, if the query matches anything.
    pub fn current_match(&self) -> Option<Match> {
        self.matches.get(self.current).copied()
    }

    /// Move the highlight one match forward (or backward), wrapping around.
    pub fn step(&mut self, forward: bool) -> Option<Match> {
        let len = self.matches.len();
        if len == 0 {
            return None;
        }

        self.current = if forward {
            (self.current + 1) % len
        } else {
            (self.current + len - 1) % len
        };

        self.current_match()
    }

    /// Drop the highlighted match from the list without re-searching.
    ///
    /// Used after a single replacement: the replaced text is gone, and the
    /// following matches on the same line have shifted, so the list is
    /// recomputed by the caller — this only keeps the index sane in between.
    pub fn forget_current(&mut self) {
        if self.current < self.matches.len() {
            self.matches.remove(self.current);
        }
    }

    /// Total number of matches.
    pub fn count(&self) -> usize {
        self.matches.len()
    }

    /// All matches, in document order.
    pub fn matches(&self) -> &[Match] {
        &self.matches
    }

    /// Human-readable position, e.g. `3/12`, or `No results` when nothing
    /// matches and something was typed.
    fn status(&self) -> String {
        if self.query.is_empty() {
            String::new()
        } else if self.matches.is_empty() {
            String::from("No results")
        } else {
            format!("{}/{}", self.current + 1, self.matches.len())
        }
    }
}

/// Push every non-overlapping match of `query` in `line` onto `out`.
fn collect_line_matches(
    line: &str,
    query: &str,
    case_sensitive: bool,
    line_index: usize,
    out: &mut Vec<Match>,
) {
    if query.is_empty() || query.len() > line.len() {
        return;
    }

    if case_sensitive {
        out.extend(line.match_indices(query).map(|(start, hit)| Match {
            line: line_index,
            start,
            end: start + hit.len(),
        }));
        return;
    }

    let (haystack, needle) = (line.as_bytes(), query.as_bytes());
    let mut at = 0;

    while at + needle.len() <= haystack.len() {
        // `eq_ignore_ascii_case` only equates ASCII letters, so any byte that
        // matches is either ASCII or byte-identical to the query — either way
        // the range stays on UTF-8 boundaries.
        if line.is_char_boundary(at) && haystack[at..at + needle.len()].eq_ignore_ascii_case(needle)
        {
            out.push(Match {
                line: line_index,
                start: at,
                end: at + needle.len(),
            });
            at += needle.len();
        } else {
            at += 1;
        }
    }
}

/// Build the find (and, in replace mode, replace) bar.
pub fn view(search: &Search) -> Element<'_, Message> {
    let has_matches = search.count() > 0;

    let find_row = row![
        text_input("Find", &search.query)
            .id(QUERY_ID)
            .on_input(Message::SearchQueryChanged)
            .on_submit(Message::FindNext)
            .size(13)
            .padding([4, 8])
            .width(Length::FillPortion(2)),
        text(search.status())
            .size(12)
            .font(Font::MONOSPACE)
            .style(theme::status_text)
            .width(90),
        small_button("Aa", "Match case", Some(Message::ToggleSearchCase)),
        small_button(
            "\u{2191}",
            "Previous (Shift+Enter)",
            has_matches.then_some(Message::FindPrevious)
        ),
        small_button(
            "\u{2193}",
            "Next (Enter)",
            has_matches.then_some(Message::FindNext)
        ),
        space::horizontal(),
        small_button("\u{2715}", "Close (Esc)", Some(Message::CloseSearch)),
    ]
    .spacing(6)
    .align_y(iced::Center);

    let content: Element<'_, Message> = if search.replacing {
        let replace_row = row![
            text_input("Replace with", &search.replacement)
                .on_input(Message::ReplacementChanged)
                .on_submit(Message::ReplaceCurrent)
                .size(13)
                .padding([4, 8])
                .width(Length::FillPortion(2)),
            small_button(
                "Replace",
                "Replace this match",
                has_matches.then_some(Message::ReplaceCurrent)
            ),
            small_button(
                "Replace all",
                "Replace every match",
                has_matches.then_some(Message::ReplaceAll)
            ),
            space::horizontal(),
        ]
        .spacing(6)
        .align_y(iced::Center);

        iced::widget::column![find_row, replace_row].spacing(6).into()
    } else {
        find_row.into()
    };

    container(content)
        .padding([6, 12])
        .width(Fill)
        .style(theme::toolbar_container)
        .into()
}

/// A compact labelled button, disabled when `message` is `None`.
fn small_button<'a>(
    label: &'a str,
    tip: &'a str,
    message: Option<Message>,
) -> Element<'a, Message> {
    tooltip(
        button(text(label).size(12))
            .padding([4, 8])
            .style(theme::toolbar_button)
            .on_press_maybe(message),
        tip,
        tooltip::Position::Bottom,
    )
    .style(container::rounded_box)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.split('\n').map(String::from).collect()
    }

    fn found(doc: &str, query: &str, case_sensitive: bool) -> Vec<Match> {
        let mut search = Search {
            query: query.to_owned(),
            case_sensitive,
            ..Search::default()
        };
        search.refresh(&lines(doc));
        search.matches().to_vec()
    }

    #[test]
    fn finds_every_occurrence_in_document_order() {
        let hits = found("one two\nthree two\n\ntwo", "two", true);

        assert_eq!(
            hits,
            vec![
                Match { line: 0, start: 4, end: 7 },
                Match { line: 1, start: 6, end: 9 },
                Match { line: 3, start: 0, end: 3 },
            ]
        );
    }

    #[test]
    fn case_sensitivity_is_honoured() {
        assert_eq!(found("Foo foo FOO", "foo", true).len(), 1);
        assert_eq!(found("Foo foo FOO", "foo", false).len(), 3);
    }

    #[test]
    fn matches_do_not_overlap() {
        // "aa" in "aaaa" yields two matches, not three.
        assert_eq!(
            found("aaaa", "aa", true),
            vec![
                Match { line: 0, start: 0, end: 2 },
                Match { line: 0, start: 2, end: 4 },
            ]
        );
    }

    /// Byte offsets must stay on char boundaries so slicing the line is safe.
    #[test]
    fn multibyte_text_yields_valid_byte_ranges() {
        let doc = "héllo wörld héllo";
        let hits = found(doc, "héllo", false);

        assert_eq!(hits.len(), 2);
        for hit in hits {
            assert!(doc.is_char_boundary(hit.start));
            assert!(doc.is_char_boundary(hit.end));
            assert_eq!(&doc[hit.start..hit.end], "héllo");
        }
    }

    /// A query that only differs by ASCII case must not match bytes that
    /// happen to sit inside a multi-byte character.
    #[test]
    fn ascii_folding_never_splits_a_character() {
        let doc = "ÀÁÂÃ";
        assert!(found(doc, "a", false).is_empty());
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        assert!(found("anything at all", "", false).is_empty());
    }

    #[test]
    fn stepping_wraps_around_in_both_directions() {
        let mut search = Search {
            query: String::from("x"),
            ..Search::default()
        };
        search.refresh(&lines("x\nx\nx"));

        assert_eq!(search.current_match().unwrap().line, 0);
        assert_eq!(search.step(true).unwrap().line, 1);
        assert_eq!(search.step(true).unwrap().line, 2);
        // Past the end wraps to the start.
        assert_eq!(search.step(true).unwrap().line, 0);
        // And backwards from the start wraps to the end.
        assert_eq!(search.step(false).unwrap().line, 2);
    }

    #[test]
    fn stepping_an_empty_result_set_is_a_no_op() {
        let mut search = Search {
            query: String::from("nope"),
            ..Search::default()
        };
        search.refresh(&lines("nothing here"));

        assert!(search.step(true).is_none());
        assert!(search.current_match().is_none());
    }

    /// Editing must not throw the highlight back to the top of the document.
    #[test]
    fn refreshing_keeps_the_highlight_near_where_it_was() {
        let mut search = Search {
            query: String::from("hit"),
            ..Search::default()
        };
        let doc = lines("hit\nhit\nhit\nhit");
        search.refresh(&doc);
        search.step(true);
        search.step(true);
        assert_eq!(search.current_match().unwrap().line, 2);

        search.refresh(&doc);
        assert_eq!(search.current_match().unwrap().line, 2);
    }

    #[test]
    fn status_reports_position_and_emptiness() {
        let mut search = Search::default();
        assert_eq!(search.status(), "");

        search.query = String::from("zzz");
        search.refresh(&lines("nothing"));
        assert_eq!(search.status(), "No results");

        search.query = String::from("a");
        search.refresh(&lines("a\na"));
        assert_eq!(search.status(), "1/2");
        search.step(true);
        assert_eq!(search.status(), "2/2");
    }
}
