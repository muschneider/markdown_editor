//! Syntax highlighting for the raw editor.
//!
//! Prose is highlighted with the Markdown grammar. Fenced code is not left to
//! that grammar's embedded languages: every time it enters one, it compiles
//! the pattern that closes the fence afresh (the pattern refers back to the
//! opening fence) — 3 ms and more for a three-line block, paid every time the
//! caret enters one. Here fences are recognised line by line instead, and the
//! code between them goes straight to its own language's grammar: the same
//! colours, for a fraction of the cost.
//!
//! Every grammar is [memoized](crate::syntax::Memo), and the parser states at
//! the start of every line are kept, so after an edit highlighting resumes at
//! exactly the changed line — and the unchanged lines after it, which the
//! editor re-highlights down to the end of its region, come from the memo.

use std::ops::Range;
use std::rc::Rc;

use iced::advanced::text::highlighter::{self, Format};
use iced::{Color, Font, Theme};

use crate::fences::Fence;
use crate::syntax::{self, State};

/// How the raw editor is highlighted.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The syntax theme, for Markdown and code alike.
    pub theme: syntax::Theme,
}

/// A fenced code block open at the start of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Open {
    /// The opening fence's character and length, which the closing fence
    /// must match.
    marker: u8,
    length: usize,
    /// The grammar to highlight the code with. Shared: every line of the
    /// block records it.
    language: Rc<str>,
}

impl Open {
    /// Whether `fence` closes this block.
    fn is_closed_by(&self, fence: &Fence<'_>) -> bool {
        fence.closes(&Fence {
            marker: self.marker,
            length: self.length,
            info: "",
        })
    }
}

/// Everything highlighting needs to resume at the start of a line.
#[derive(Debug, Clone)]
struct LineState {
    /// The Markdown parser's state. Code blocks leave it as it was.
    markdown: State,
    /// The code block open at the start of the line, and its parser's state.
    code: Option<(Open, State)>,
}

/// The raw editor's highlighter; see the [module documentation](self).
///
/// The grammars and what they highlighted live in [`syntax::with_memo`],
/// shared with the rendered code blocks; this keeps only where each line
/// starts.
pub struct Highlighter {
    theme: syntax::Theme,
    /// The state at the start of each line highlighted so far — always one
    /// more entry than lines highlighted.
    lines: Vec<LineState>,
}

/// What a range of a line was highlighted as.
#[derive(Debug, Clone, Copy)]
pub enum Highlight {
    /// By a grammar: Markdown, or a code block's language.
    Syntax(syntax::Highlight),
    /// A fence opening or closing a code block.
    Fence,
}

impl Highlight {
    /// The colour and font of a highlighted range.
    pub fn to_format(self, theme: &Theme) -> Format<Font> {
        match self {
            Highlight::Syntax(highlight) => highlight.to_format(),
            // Fences are punctuation: present, but out of the way.
            Highlight::Fence => Format {
                color: Some(Color {
                    a: 0.5,
                    ..theme.extended_palette().background.base.text
                }),
                font: None,
            },
        }
    }
}

/// Highlight `line` with the memoized grammar `token` names, from `state`.
fn highlight(
    token: &str,
    theme: syntax::Theme,
    line: &str,
    state: &mut State,
) -> Vec<(Range<usize>, Highlight)> {
    syntax::with_memo(token, theme, |memo| {
        memo.highlight(line, state)
            .iter()
            .map(|(range, highlight)| (range.clone(), Highlight::Syntax(*highlight)))
            .collect()
    })
}

impl highlighter::Highlighter for Highlighter {
    type Settings = Settings;
    type Highlight = Highlight;
    type Iterator<'a> = std::vec::IntoIter<(Range<usize>, Highlight)>;

    fn new(settings: &Settings) -> Self {
        let start = LineState {
            markdown: syntax::with_memo("md", settings.theme, |memo| memo.start()),
            code: None,
        };

        Self {
            theme: settings.theme,
            lines: vec![start],
        }
    }

    fn update(&mut self, new_settings: &Settings) {
        *self = Self::new(new_settings);
    }

    /// Resume highlighting at `line`, whose starting state is known exactly
    /// if the line before it was highlighted.
    fn change_line(&mut self, line: usize) {
        self.lines.truncate(line + 1);
    }

    fn highlight_line(&mut self, line: &str) -> Self::Iterator<'_> {
        let mut state = self.lines.last().expect("a state per line").clone();
        let fence = Fence::parse(line);
        let whole_line = || vec![(0..line.len(), Highlight::Fence)];

        let highlights = match (state.code.take(), fence) {
            // An opening fence: what follows is code, in its language.
            (None, Some(fence)) => {
                let language: Rc<str> = Rc::from(fence.language().unwrap_or("txt"));
                let start = syntax::with_memo(&language, self.theme, |memo| memo.start());

                state.code = Some((
                    Open {
                        marker: fence.marker,
                        length: fence.length,
                        language,
                    },
                    start,
                ));
                whole_line()
            }
            (None, None) => highlight("md", self.theme, line, &mut state.markdown),
            // The closing fence: back to Markdown, as it was before the block.
            (Some((open, _)), Some(fence)) if open.is_closed_by(&fence) => whole_line(),
            (Some((open, mut code)), _) => {
                let highlights = highlight(&open.language, self.theme, line, &mut code);
                state.code = Some((open, code));
                highlights
            }
        };

        self.lines.push(state);
        highlights.into_iter()
    }

    fn current_line(&self) -> usize {
        self.lines.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use highlighter::Highlighter as _;

    fn highlighter() -> Highlighter {
        Highlighter::new(&Settings {
            theme: syntax::Theme::Base16Ocean,
        })
    }

    /// What each line was highlighted as: `S` by a grammar, `F` fence, `-`
    /// nothing at all.
    fn kinds(highlighter: &mut Highlighter, lines: &[&str]) -> String {
        lines
            .iter()
            .map(|line| {
                highlighter
                    .highlight_line(line)
                    .map(|(_, highlight)| match highlight {
                        Highlight::Syntax(_) => 'S',
                        Highlight::Fence => 'F',
                    })
                    .next()
                    .unwrap_or('-')
            })
            .collect()
    }

    /// Everything each line was highlighted with.
    fn highlights(highlighter: &mut Highlighter, lines: &[&str]) -> Vec<Vec<String>> {
        lines
            .iter()
            .map(|line| {
                highlighter
                    .highlight_line(line)
                    .map(|(range, highlight)| format!("{range:?} {highlight:?}"))
                    .collect()
            })
            .collect()
    }

    const DOCUMENT: &[&str] = &[
        "# Title",
        "```rust",
        "fn main() {}",
        "",
        "let x = 1;",
        "```",
        "after",
    ];

    #[test]
    fn fences_are_marked_and_everything_else_highlighted() {
        let mut highlighter = highlighter();

        assert_eq!(kinds(&mut highlighter, DOCUMENT), "SFS-SFS");
        assert_eq!(highlighter.current_line(), DOCUMENT.len());
    }

    /// Code gets its language's colours: the same line highlights
    /// differently as Rust than as Markdown prose.
    #[test]
    fn code_is_highlighted_by_its_language() {
        let mut in_code = highlighter();
        let mut as_prose = highlighter();

        let code = highlights(&mut in_code, &["```rust", "let x = \"s\";", "```"]);
        let prose = highlights(&mut as_prose, &["let x = \"s\";"]);

        assert_ne!(code[1], prose[0]);
    }

    #[test]
    fn a_fence_left_open_runs_to_the_end() {
        let mut highlighter = highlighter();

        assert_eq!(
            kinds(&mut highlighter, &["```", "code", "~~~", "more"]),
            "FSSS"
        );
    }

    /// After a change, highlighting resumes at exactly the changed line, and
    /// comes out the same as highlighting everything afresh.
    #[test]
    fn highlighting_resumes_at_the_changed_line() {
        let mut edited = highlighter();
        let _ = highlights(&mut edited, DOCUMENT);

        let mut changed = DOCUMENT.to_vec();
        changed[4] = "let x = 2;";
        edited.change_line(4);
        assert_eq!(edited.current_line(), 4);
        let resumed = highlights(&mut edited, &changed[4..]);

        let mut fresh = highlighter();
        let expected = highlights(&mut fresh, &changed);
        assert_eq!(resumed, expected[4..]);
    }

    /// Opening a fence above code turns what follows into code; closing it
    /// again turns it back — whatever the memo remembers.
    #[test]
    fn a_new_fence_changes_the_lines_after_it() {
        let mut highlighter = highlighter();

        assert_eq!(kinds(&mut highlighter, &["intro", "let x = 1;"]), "SS");
        highlighter.change_line(0);
        let _ = kinds(&mut highlighter, &["```rust"]);
        let as_code = highlights(&mut highlighter, &["let x = 1;"]);
        highlighter.change_line(0);
        let _ = kinds(&mut highlighter, &["intro"]);
        let as_prose = highlights(&mut highlighter, &["let x = 1;"]);

        assert_ne!(as_code, as_prose);
    }
}
