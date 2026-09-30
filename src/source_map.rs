//! Which source bytes each piece of rendered Markdown came from.
//!
//! Rendered Markdown keeps no link back to its source: `**bold**` shows as
//! four letters, a link as its text alone, three hard-wrapped source lines as
//! one reflowed paragraph. To put the caret where the user clicked, a point in
//! the rendered text has to be traced back to a line and column of the source.
//!
//! That is done by parsing the block again with the same parser and options
//! iced renders it with, and mirroring how iced turns the parser's events into
//! rendered items. Every event comes with the source range it was parsed
//! from, so the text of every rendered item can be mapped back byte by byte.
//!
//! The items are numbered in the order the renderer draws their text — which
//! is the order [`crate::markdown_view::Viewer`] numbers the text it renders,
//! so the two meet in the middle: the viewer reports "item 3, byte 17", and
//! [`locate`] answers "line 2, column 25".

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// A piece of rendered text and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    /// Where the piece starts in its item's rendered text.
    rendered: usize,
    /// How long it is, rendered.
    len: usize,
    /// Where it starts in the source.
    source: usize,
    /// How long it was in the source. Equal to `len` unless the source had
    /// escapes or entities, in which case offsets within it are approximate.
    source_len: usize,
}

/// One piece of rendered text — a heading, a paragraph, or a line of code —
/// as a sequence of mapped runs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Item {
    runs: Vec<Run>,
    len: usize,
}

impl Item {
    fn push(&mut self, rendered: &str, source: Range<usize>) {
        self.runs.push(Run {
            rendered: self.len,
            len: rendered.len(),
            source: source.start,
            source_len: source.len(),
        });
        self.len += rendered.len();
    }

    /// The source byte that byte `offset` of the rendered text came from.
    ///
    /// An offset at the very end maps to the end of the last run, where the
    /// caret belongs after a click past the end of the text.
    fn source(&self, offset: usize) -> Option<usize> {
        let run = self
            .runs
            .iter()
            .rev()
            .find(|run| run.rendered <= offset)
            .or(self.runs.first())?;

        let within = offset.saturating_sub(run.rendered).min(run.len);
        let within = if run.len == run.source_len {
            within
        } else {
            (within * run.source_len).checked_div(run.len).unwrap_or(0)
        };

        Some(run.source + within)
    }
}

/// The options iced parses Markdown with; anything else could produce
/// different items.
fn options() -> Options {
    Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS
        | Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
}

/// Every piece of text the renderer draws for `source`, in drawing order.
///
/// Mirrors iced's `markdown::parse`: spans gather until a paragraph, list
/// item or heading ends — or until a list, quote or fenced code block starts
/// and flushes them — and only non-empty paragraphs are produced. Text inside
/// tables is not counted (tables are drawn from their source directly), nor
/// is an image's alt text.
fn items(source: &str) -> Vec<Item> {
    let mut items = Vec::new();
    let mut pending = Item::default();
    let mut code: Option<Vec<CodeLine>> = None;
    let mut metadata = false;
    let mut tables = 0usize;

    let flush = |pending: &mut Item, items: &mut Vec<Item>, tables: usize| {
        let item = std::mem::take(pending);
        if !item.runs.is_empty() && tables == 0 {
            items.push(item);
        }
    };

    for (event, range) in Parser::new_ext(source, options()).into_offset_iter() {
        match event {
            Event::Start(Tag::MetadataBlock(_)) => metadata = true,
            Event::End(TagEnd::MetadataBlock(_)) => metadata = false,
            _ if metadata => {}

            Event::Start(Tag::List(_) | Tag::BlockQuote(_)) => {
                flush(&mut pending, &mut items, tables);
            }
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(_))) => {
                flush(&mut pending, &mut items, tables);
                code = Some(Vec::new());
            }
            Event::End(TagEnd::CodeBlock) => {
                // One item per line of code, as the viewer draws them.
                for (start, line, _) in code.take().unwrap_or_default() {
                    let mut item = Item::default();
                    item.push(&line, start..start + line.len());
                    items.push(item);
                }
            }
            Event::Start(Tag::Table(_)) => tables += 1,
            Event::End(TagEnd::Table) => tables = tables.saturating_sub(1),

            Event::End(TagEnd::Heading(_)) => {
                // A heading is drawn even when empty.
                let item = std::mem::take(&mut pending);
                if tables == 0 {
                    items.push(item);
                }
            }
            Event::End(TagEnd::Paragraph | TagEnd::Item | TagEnd::TableCell) => {
                flush(&mut pending, &mut items, tables);
            }
            // The text gathered so far becomes the image's alt text.
            Event::End(TagEnd::Image) => pending = Item::default(),

            Event::Text(text) => match &mut code {
                Some(lines) => code_lines(source, &text, range, lines),
                None => pending.push(&text, range),
            },
            Event::Code(text) => {
                // The range covers the backticks too; find the code in it.
                let within = source[range.clone()].find(text.as_ref()).unwrap_or(0);
                let start = range.start + within;
                pending.push(&text, start..start + text.len());
            }
            // Rendered as a space and a line break: both stand for the end of
            // the source line.
            Event::SoftBreak => pending.push(" ", range.start..range.start + 1),
            Event::HardBreak => pending.push("\n", range.start..range.start + 1),
            _ => {}
        }
    }

    items
}

/// A line of a code block: where it starts in the source, its text, and
/// whether its newline has been seen yet.
type CodeLine = (usize, String, bool);

/// Split a code block's `text`, parsed from `source[range]`, into lines with
/// the source offset of each.
///
/// An indented fence has that indentation stripped from its code, so each
/// line is searched for in the source rather than assumed to start the range.
fn code_lines(source: &str, text: &str, range: Range<usize>, lines: &mut Vec<CodeLine>) {
    let parsed = &source[range.clone()];
    let mut cursor = 0;

    for piece in text.split_inclusive('\n') {
        let complete = piece.ends_with('\n');
        let content = piece.strip_suffix('\n').unwrap_or(piece);
        let found = parsed[cursor..]
            .find(content)
            .map_or(cursor, |offset| cursor + offset);

        match lines.last_mut() {
            // The rest of a line an earlier event began.
            Some((_, line, done)) if !*done => {
                line.push_str(content);
                *done = complete;
            }
            _ => lines.push((range.start + found, content.to_owned(), complete)),
        }

        cursor = (found + piece.len()).min(parsed.len());
    }
}

/// The `(line, byte column)` within `lines` — a block's source lines — that
/// byte `offset` of its rendered text item number `item` came from.
///
/// `None` when the block has no such item: the renderer and the parse here
/// disagree, and the caller should place the caret some other way.
pub fn locate(lines: &[String], item: usize, offset: usize) -> Option<(usize, usize)> {
    let source = lines.join("\n");
    let byte = items(&source).get(item)?.source(offset)?;

    let line = source[..byte].matches('\n').count();
    let line_start = source[..byte].rfind('\n').map_or(0, |newline| newline + 1);
    let column = byte - line_start;

    // Never inside a character, never past the end of the line.
    let text = lines.get(line)?;
    let mut column = column.min(text.len());
    while !text.is_char_boundary(column) {
        column -= 1;
    }

    Some((line, column))
}

/// How long the rendered text of each item is, in drawing order — for tests
/// to check against what the renderer actually numbers.
#[cfg(test)]
pub fn item_lengths(source: &str) -> Vec<usize> {
    items(source).iter().map(|item| item.len).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.split('\n').map(String::from).collect()
    }

    /// The rendered text of every item, as the renderer would draw it.
    fn rendered(source: &str) -> Vec<String> {
        let items = items(source);
        items
            .iter()
            .map(|item| {
                item.runs
                    .iter()
                    .map(|run| &source[run.source..run.source + run.source_len])
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect()
    }

    #[test]
    fn plain_text_maps_byte_for_byte() {
        let block = lines("hello world");

        assert_eq!(locate(&block, 0, 0), Some((0, 0)));
        assert_eq!(locate(&block, 0, 6), Some((0, 6)));
        assert_eq!(locate(&block, 0, 11), Some((0, 11)));
    }

    #[test]
    fn markup_that_is_not_drawn_is_stepped_over() {
        // Rendered: "a bold word" — the asterisks are not drawn.
        let block = lines("a **bold** word");

        // Start of "bold": source column 4, past the opening asterisks.
        assert_eq!(locate(&block, 0, 2), Some((0, 4)));
        // Start of " word": after the closing asterisks.
        assert_eq!(locate(&block, 0, 6), Some((0, 10)));
    }

    #[test]
    fn a_link_maps_to_its_text_not_its_address() {
        // Rendered: "see docs here".
        let block = lines("see [docs](https://example.com) here");

        assert_eq!(locate(&block, 0, 4), Some((0, 5)));
        // The space after the link, then "here".
        assert_eq!(locate(&block, 0, 8), Some((0, 31)));
        assert_eq!(locate(&block, 0, 9), Some((0, 32)));
    }

    #[test]
    fn inline_code_maps_inside_its_backticks() {
        // Rendered: "run cargo now".
        let block = lines("run `cargo` now");

        assert_eq!(locate(&block, 0, 4), Some((0, 5)));
        assert_eq!(locate(&block, 0, 6), Some((0, 7)));
    }

    #[test]
    fn a_heading_maps_past_its_hashes() {
        let block = lines("## Getting started");

        assert_eq!(locate(&block, 0, 0), Some((0, 3)));
        assert_eq!(locate(&block, 0, 8), Some((0, 11)));
    }

    #[test]
    fn a_reflowed_paragraph_maps_back_to_its_source_lines() {
        // Three source lines render as one paragraph, joined by spaces.
        let block = lines("first line\nsecond line\nthird");

        assert_eq!(locate(&block, 0, 0), Some((0, 0)));
        // "second" begins after "first line ".
        assert_eq!(locate(&block, 0, 11), Some((1, 0)));
        assert_eq!(locate(&block, 0, 18), Some((1, 7)));
        assert_eq!(locate(&block, 0, 23), Some((2, 0)));
        // The space standing for a line break is the end of its line.
        assert_eq!(locate(&block, 0, 10), Some((0, 10)));
    }

    #[test]
    fn items_are_numbered_in_drawing_order() {
        let source = "# Title\nintro text\n- one\n- two\n\n  nested para\n> quoted";

        assert_eq!(
            rendered(source),
            vec!["Title", "intro text", "one", "two", "nested para", "quoted"]
        );
    }

    #[test]
    fn a_paragraph_before_a_list_is_its_own_item() {
        assert_eq!(rendered("lead in\n- item"), vec!["lead in", "item"]);
    }

    #[test]
    fn code_lines_are_items_of_their_own() {
        let block = lines("```rust\nfn main() {}\n\nlet x = 1;\n```");

        assert_eq!(
            rendered(&block.join("\n")),
            vec!["fn main() {}", "", "let x = 1;"]
        );
        assert_eq!(locate(&block, 0, 3), Some((1, 3)));
        assert_eq!(locate(&block, 2, 4), Some((3, 4)));
    }

    #[test]
    fn an_indented_fence_maps_to_the_indented_source() {
        let block = lines("  ```\n  code\n  ```");

        assert_eq!(locate(&block, 0, 0), Some((1, 2)));
    }

    #[test]
    fn table_text_is_not_counted() {
        let source = "| A | B |\n|---|---|\n| 1 | 2 |";

        assert!(rendered(source).is_empty());
    }

    #[test]
    fn an_image_is_not_text() {
        assert_eq!(rendered("![alt text](img.png)\n\nafter"), vec!["after"]);
    }

    #[test]
    fn multibyte_text_lands_on_character_boundaries() {
        let block = lines("olá **mundo**");

        // Rendered "olá mundo": "mundo" starts at rendered byte 5.
        assert_eq!(locate(&block, 0, 5), Some((0, 7)));
    }

    #[test]
    fn an_item_that_does_not_exist_is_reported() {
        assert_eq!(locate(&lines("one paragraph"), 3, 0), None);
    }
}
