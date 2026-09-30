//! Syntax highlighting, memoized line by line.
//!
//! [syntect] highlights a line as a function of two things: the line's text,
//! and the state its parser is in when the line starts. When both match a
//! line highlighted before, so does the result — so it is not computed again.
//!
//! That matters because the raw editor re-highlights everything from an
//! edited line to the end of its region, on every keystroke. A code block is
//! edited as one region, and a line of code costs a couple of hundred
//! microseconds to highlight: typing in a hundred-line block cost over 20 ms
//! per key. With the memo, only the edited line is parsed again; the lines
//! after it are found unchanged, reached in the same state, and reused. The
//! memo is keyed by the text rather than the line number, so a line keeps its
//! highlighting when lines are added or removed above it, and a block the
//! caret comes back to is not highlighted again at all.
//!
//! There is one memo per language, shared by everything that highlights on
//! the UI thread: a code block is highlighted when it is first rendered, and
//! when the caret later enters it and the raw editor highlights the same
//! lines, from the same state, they are all found in the memo.
//!
//! This is the same highlighting `iced_highlighter` does — the same syntax
//! definitions and themes, the same walk over the parser's scope changes —
//! with the parser state exposed so it can be compared.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::LazyLock;

use iced::advanced::text::highlighter::Format;
use iced::{Color, Font, font};
use two_face::re_exports::syntect::{highlighting, parsing};

pub use iced_highlighter::Theme;

/// Every syntax definition, loaded on first use (or by [`warm_up`]).
static SYNTAXES: LazyLock<parsing::SyntaxSet> = LazyLock::new(two_face::syntax::extra_no_newlines);

/// Every theme, loaded on first use.
static THEMES: LazyLock<highlighting::ThemeSet> =
    LazyLock::new(highlighting::ThemeSet::load_defaults);

/// Lines a [`Memo`] remembers before it starts over: a few megabytes at most.
const MEMO_CAPACITY: usize = 8192;

/// The memo of every language highlighted so far on a thread.
#[derive(Default)]
struct Memos {
    /// The theme they were highlighted in.
    theme: Option<Theme>,
    /// One per language token.
    languages: HashMap<Box<str>, Memo>,
}

thread_local! {
    static MEMOS: RefCell<Memos> = RefCell::default();
}

/// Run `f` with the memoized grammar `token` names, in `theme`.
///
/// Every caller on a thread shares the same memo per language. Highlighting
/// in another theme starts every memo over: colours are part of what is
/// remembered.
pub fn with_memo<T>(token: &str, theme: Theme, f: impl FnOnce(&mut Memo) -> T) -> T {
    MEMOS.with(|memos| {
        let Memos {
            theme: current,
            languages,
        } = &mut *memos.borrow_mut();
        if *current != Some(theme) {
            languages.clear();
            *current = Some(theme);
        }

        if !languages.contains_key(token) {
            languages.insert(token.into(), Memo::new(Grammar::new(token, theme)));
        }
        f(languages.get_mut(token).expect("just inserted"))
    })
}

/// The name of `theme` in the theme set.
fn theme_key(theme: Theme) -> &'static str {
    match theme {
        Theme::SolarizedDark => "Solarized (dark)",
        Theme::Base16Mocha => "base16-mocha.dark",
        Theme::Base16Ocean => "base16-ocean.dark",
        Theme::Base16Eighties => "base16-eighties.dark",
        Theme::InspiredGitHub => "InspiredGitHub",
    }
}

/// How a range of text is highlighted: a colour, and maybe a font style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Highlight(highlighting::StyleModifier);

impl Highlight {
    /// The colour of the text, or `None` to leave it as it is.
    pub fn color(&self) -> Option<Color> {
        self.0
            .foreground
            .map(|color| Color::from_rgba8(color.r, color.g, color.b, f32::from(color.a) / 255.0))
    }

    /// The font of the text, or `None` to leave it as it is.
    pub fn font(&self) -> Option<Font> {
        let style = self.0.font_style?;
        let bold = style.contains(highlighting::FontStyle::BOLD);
        let italic = style.contains(highlighting::FontStyle::ITALIC);

        (bold || italic).then_some(Font {
            weight: if bold {
                font::Weight::Bold
            } else {
                font::Weight::Normal
            },
            style: if italic {
                font::Style::Italic
            } else {
                font::Style::Normal
            },
            ..Font::MONOSPACE
        })
    }

    /// Both, as the text editor takes them.
    pub fn to_format(self) -> Format<Font> {
        Format {
            color: self.color(),
            font: self.font(),
        }
    }
}

/// The parser's state between two lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    parse: parsing::ParseState,
    scopes: parsing::ScopeStack,
}

/// A language's grammar, and a theme to colour it with.
pub struct Grammar {
    syntax: &'static parsing::SyntaxReference,
    highlighter: highlighting::Highlighter<'static>,
}

impl Grammar {
    /// The grammar `token` — a language name or file extension — names, or
    /// plain text when it names none.
    ///
    /// Not cheap: it compiles every scope selector of the theme. Keep one
    /// per language rather than creating one per block.
    pub fn new(token: &str, theme: Theme) -> Self {
        let syntax = SYNTAXES
            .find_syntax_by_token(token)
            .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text());

        Self {
            syntax,
            highlighter: highlighting::Highlighter::new(&THEMES.themes[theme_key(theme)]),
        }
    }

    /// The state the parser starts a document in.
    pub fn start(&self) -> State {
        State {
            parse: parsing::ParseState::new(self.syntax),
            scopes: parsing::ScopeStack::new(),
        }
    }

    /// Highlight `line`, parsed from `state`, which is left as it is after it.
    pub fn highlight(&self, line: &str, state: &mut State) -> Vec<(Range<usize>, Highlight)> {
        let ops = state.parse.parse_line(line, &SYNTAXES).unwrap_or_default();

        let mut highlights = Vec::new();
        let mut from = 0;

        // Every operation changes the scopes from its offset on; the text
        // before it is styled by the scopes as they were.
        for (at, op) in ops
            .into_iter()
            .chain(std::iter::once((line.len(), parsing::ScopeStackOp::Noop)))
        {
            if at > from {
                highlights.push((
                    from..at,
                    Highlight(self.highlighter.style_mod_for_stack(&state.scopes.scopes)),
                ));
            }
            from = from.max(at);
            let _ = state.scopes.apply(&op);
        }

        highlights
    }
}

/// A line highlighted before: the state it started in and ended in, and what
/// it was highlighted as.
struct Entry {
    before: State,
    after: State,
    highlights: Vec<(Range<usize>, Highlight)>,
}

/// A [`Grammar`] that remembers the lines it has highlighted.
pub struct Memo {
    grammar: Grammar,
    lines: HashMap<Box<str>, Vec<Entry>>,
    entries: usize,
}

impl Memo {
    /// Remember what `grammar` highlights.
    pub fn new(grammar: Grammar) -> Self {
        Self {
            grammar,
            lines: HashMap::new(),
            entries: 0,
        }
    }

    /// The state the parser starts a document in.
    pub fn start(&self) -> State {
        self.grammar.start()
    }

    /// Highlight `line`, parsed from `state`, which is left as it is after it
    /// — reusing the result when this line was highlighted from this state
    /// before.
    pub fn highlight(&mut self, line: &str, state: &mut State) -> &[(Range<usize>, Highlight)] {
        let known = self
            .lines
            .get(line)
            .and_then(|entries| entries.iter().position(|entry| entry.before == *state));

        let index = match known {
            Some(index) => index,
            None => {
                if self.entries >= MEMO_CAPACITY {
                    self.lines.clear();
                    self.entries = 0;
                }

                let before = state.clone();
                let highlights = self.grammar.highlight(line, state);
                let entries = self.lines.entry(line.into()).or_default();
                entries.push(Entry {
                    before,
                    after: state.clone(),
                    highlights,
                });
                self.entries += 1;
                entries.len() - 1
            }
        };

        let entry = &self.lines[line][index];
        if known.is_some() {
            state.clone_from(&entry.after);
        }
        &entry.highlights
    }
}

/// Load the syntax definitions and compile the grammars of common languages,
/// on a background thread — see [`crate::markdown_view::warm_up`].
pub fn warm_up(theme: Theme, languages: &'static [&'static str]) {
    // A snippet with a keyword, an identifier, a string, a number and a
    // comment: enough to reach the contexts a real code block will hit, so
    // they are compiled here rather than on the UI thread later.
    const SNIPPET: &str = "let x = \"s\"; /* 1 */";

    std::thread::spawn(move || {
        for token in languages {
            let grammar = Grammar::new(token, theme);
            // Parsing a line is what forces the contexts to compile; loading
            // the grammar alone does not.
            let _ = grammar.highlight(SNIPPET, &mut grammar.start());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &[&str] = &[
        "fn main() {",
        "    let greeting = \"hello\"; // a comment",
        "    /* a comment that",
        "       spans two lines */",
        "}",
    ];

    fn highlight_all(memo: &mut Memo, lines: &[&str]) -> Vec<Vec<(Range<usize>, Highlight)>> {
        let mut state = memo.start();
        lines
            .iter()
            .map(|line| memo.highlight(line, &mut state).to_vec())
            .collect()
    }

    #[test]
    fn every_byte_of_a_line_is_highlighted_once_in_order() {
        let grammar = Grammar::new("rust", Theme::Base16Ocean);
        let mut state = grammar.start();

        for line in RUST {
            let highlights = grammar.highlight(line, &mut state);
            let mut end = 0;
            for (range, _) in &highlights {
                assert_eq!(range.start, end, "{line:?}: {highlights:?}");
                end = range.end;
            }
            assert_eq!(end, line.len(), "{line:?}");
        }
    }

    #[test]
    fn code_is_highlighted_in_more_than_one_colour() {
        let grammar = Grammar::new("rust", Theme::Base16Ocean);
        let highlights = grammar.highlight(RUST[1], &mut grammar.start());

        let colours: std::collections::HashSet<_> = highlights
            .iter()
            .filter_map(|(_, highlight)| highlight.color().map(|c| c.into_rgba8()))
            .collect();
        assert!(colours.len() > 2, "{highlights:?}");
    }

    #[test]
    fn remembered_lines_give_the_same_result() {
        let mut memo = Memo::new(Grammar::new("rust", Theme::Base16Ocean));

        let first = highlight_all(&mut memo, RUST);
        let entries = memo.entries;
        let second = highlight_all(&mut memo, RUST);

        assert_eq!(first, second);
        assert_eq!(memo.entries, entries, "nothing was highlighted again");
    }

    /// The same text in a different state is a different line: the second
    /// half of a block comment must not be coloured as code.
    #[test]
    fn a_line_reached_in_another_state_is_highlighted_again() {
        let mut memo = Memo::new(Grammar::new("rust", Theme::Base16Ocean));

        let in_comment = highlight_all(&mut memo, &["/* open", "let x = 1;"]);
        let as_code = highlight_all(&mut memo, &["// closed", "let x = 1;"]);

        assert_ne!(in_comment[1], as_code[1]);
    }

    #[test]
    fn an_unknown_language_is_plain_text() {
        let grammar = Grammar::new("no-such-language", Theme::Base16Ocean);
        let highlights = grammar.highlight("anything at all", &mut grammar.start());

        assert_eq!(highlights.len(), 1);
    }
}
