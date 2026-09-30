//! Fenced code blocks: recognising their fences, pairing them up, and keeping
//! track of them as the document changes.
//!
//! Fences matter well beyond the code they hold. A blank line inside a code
//! block does not end it, so the fences decide how the whole document splits
//! into rendered blocks; a code block is edited as one raw region; and the raw
//! editor highlights the code between fences with the code's own language.

use std::ops::Range;

/// A line that opens or closes a fenced code block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fence<'a> {
    /// The fence character: a backtick or a tilde.
    pub marker: u8,
    /// How many of it the fence has — at least three.
    pub length: usize,
    /// Everything after the fence, trimmed: the info string of an opening
    /// fence, which starts with the code's language.
    pub info: &'a str,
}

impl<'a> Fence<'a> {
    /// The fence on `line`, if it is one.
    ///
    /// A fence is three or more backticks or tildes, indented by at most three
    /// spaces; a backtick fence's info string cannot contain a backtick.
    pub fn parse(line: &'a str) -> Option<Self> {
        let rest = line.trim_start_matches(' ');
        if line.len() - rest.len() > 3 {
            return None;
        }

        let marker = *rest.as_bytes().first()?;
        if marker != b'`' && marker != b'~' {
            return None;
        }

        let length = rest.bytes().take_while(|&byte| byte == marker).count();
        let info = rest[length..].trim();
        if length < 3 || (marker == b'`' && info.contains('`')) {
            return None;
        }

        Some(Self {
            marker,
            length,
            info,
        })
    }

    /// Whether this fence closes a block opened by `opening`: the same
    /// character, at least as many of it, and nothing after it.
    pub fn closes(&self, opening: &Fence<'_>) -> bool {
        self.marker == opening.marker && self.length >= opening.length && self.info.is_empty()
    }

    /// The language the info string names — its first word — or `None`.
    pub fn language(&self) -> Option<&'a str> {
        language(self.info)
    }
}

/// The language an info string names: its first word, or `None`.
///
/// `rust,ignore` and `python {.numbered}` name Rust and Python; the rest is
/// for other tools.
pub fn language(info: &str) -> Option<&str> {
    info.trim()
        .split([',', ' ', '\t', '{'])
        .next()
        .filter(|language| !language.is_empty())
}

/// Whether `line` is a fence at all.
pub fn is_fence(line: &str) -> bool {
    Fence::parse(line).is_some()
}

/// Line ranges (`start..end`, opening fence through closing fence) of the
/// fenced code blocks in `lines`, in order.
///
/// A fence left open is not listed. By the letter of CommonMark it runs to the
/// end of the document, but that is almost always a block still being typed:
/// swallowing everything below it — and making all of it one raw region —
/// until the closing fence arrives would be far worse than rendering it
/// line by line in the meantime.
pub fn blocks(lines: &[String]) -> Vec<(usize, usize)> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, Fence<'_>)> = None;

    for (index, line) in lines.iter().enumerate() {
        let Some(fence) = Fence::parse(line) else {
            continue;
        };

        match open {
            None => open = Some((index, fence)),
            Some((start, opening)) if fence.closes(&opening) => {
                blocks.push((start, index + 1));
                open = None;
            }
            Some(_) => {}
        }
    }

    blocks
}

/// The block (from [`blocks`]) holding `line`, fences included, if any.
pub fn block_at(blocks: &[(usize, usize)], line: usize) -> Option<(usize, usize)> {
    let next = blocks.partition_point(|&(start, _)| start <= line);
    blocks[..next]
        .last()
        .copied()
        .filter(|&(_, end)| line < end)
}

/// Whether `line` is code *inside* a block, between its fences.
pub fn in_code(blocks: &[(usize, usize)], line: usize) -> bool {
    block_at(blocks, line).is_some_and(|(start, end)| start < line && line + 1 < end)
}

/// Every fence among `lines`: its position, character, length and whether it
/// could close a block — everything that decides how fences pair up.
///
/// Compared before and after an edit, it tells whether the edit changed any
/// fence: the one kind of change that can reshape the blocks far from it.
pub fn signature(lines: &[String]) -> Vec<(usize, u8, usize, bool)> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let fence = Fence::parse(line)?;
            Some((index, fence.marker, fence.length, fence.info.is_empty()))
        })
        .collect()
}

/// Move every block in `blocks` for the lines in `range` having been replaced
/// by `range.len() + delta` lines, none of them a fence.
///
/// Blocks after the edit move; a block the edit falls inside grows or
/// shrinks. No fence line can be inside `range`: an edit that adds or removes
/// one pairs the fences up differently, and recomputes them instead.
pub fn shift(blocks: &mut [(usize, usize)], range: Range<usize>, delta: isize) {
    if delta == 0 {
        return;
    }

    for (start, end) in blocks.iter_mut() {
        if *start >= range.end {
            *start = start.wrapping_add_signed(delta);
            *end = end.wrapping_add_signed(delta);
        } else if *end > range.start {
            *end = end.wrapping_add_signed(delta);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.split('\n').map(String::from).collect()
    }

    #[test]
    fn fences_are_three_or_more_backticks_or_tildes() {
        let fence = Fence::parse("```rust").unwrap();
        assert_eq!((fence.marker, fence.length, fence.info), (b'`', 3, "rust"));
        assert_eq!(Fence::parse("  ~~~~").unwrap().length, 4);

        assert!(Fence::parse("``").is_none());
        assert!(
            Fence::parse("    ```").is_none(),
            "indented code, not a fence"
        );
        assert!(
            Fence::parse("```a`b").is_none(),
            "backtick in the info string"
        );
        assert!(Fence::parse("~~~a`b").is_some(), "tildes allow backticks");
        assert!(Fence::parse("text ```").is_none());
    }

    #[test]
    fn the_language_is_the_first_word_of_the_info_string() {
        assert_eq!(Fence::parse("```rust").unwrap().language(), Some("rust"));
        assert_eq!(
            Fence::parse("``` rust,ignore").unwrap().language(),
            Some("rust")
        );
        assert_eq!(
            Fence::parse("```python {.numbered}").unwrap().language(),
            Some("python")
        );
        assert_eq!(Fence::parse("```").unwrap().language(), None);
    }

    #[test]
    fn only_a_matching_bare_fence_closes_a_block() {
        let opening = Fence::parse("````js").unwrap();

        assert!(Fence::parse("````").unwrap().closes(&opening));
        assert!(Fence::parse("`````").unwrap().closes(&opening));
        assert!(!Fence::parse("```").unwrap().closes(&opening), "too short");
        assert!(
            !Fence::parse("~~~~").unwrap().closes(&opening),
            "wrong character"
        );
        assert!(
            !Fence::parse("````js").unwrap().closes(&opening),
            "has an info string"
        );
    }

    #[test]
    fn blocks_pair_fences_and_ignore_one_left_open() {
        let document = lines("a\n```\ncode\n\nmore\n```\nb\n~~~\nx\n~~~\n```\nopen");

        assert_eq!(blocks(&document), vec![(1, 6), (7, 10)]);
    }

    #[test]
    fn a_fence_of_the_other_kind_is_code_inside_a_block() {
        let document = lines("```\n~~~\n```");

        assert_eq!(blocks(&document), vec![(0, 3)]);
        assert!(in_code(&blocks(&document), 1));
    }

    #[test]
    fn lines_are_located_in_their_block() {
        let found = [(2, 5), (8, 12)];

        assert_eq!(block_at(&found, 1), None);
        assert_eq!(block_at(&found, 2), Some((2, 5)));
        assert_eq!(block_at(&found, 4), Some((2, 5)));
        assert_eq!(block_at(&found, 5), None);
        assert_eq!(block_at(&found, 11), Some((8, 12)));

        // Only the lines between the fences are code.
        assert!(!in_code(&found, 2));
        assert!(in_code(&found, 3));
        assert!(!in_code(&found, 4));
    }

    #[test]
    fn shifting_follows_an_edit_that_adds_or_removes_code_lines() {
        let mut found = vec![(2, 5), (8, 12)];

        // Two lines inserted inside the first block.
        shift(&mut found, 3..3, 2);
        assert_eq!(found, vec![(2, 7), (10, 14)]);

        // A line removed between the blocks.
        shift(&mut found, 8..9, -1);
        assert_eq!(found, vec![(2, 7), (9, 13)]);

        // Lines inserted right before a block move it; right after, they
        // leave it alone.
        shift(&mut found, 9..9, 1);
        shift(&mut found, 7..7, 1);
        assert_eq!(found, vec![(2, 7), (11, 15)]);
    }

    #[test]
    fn the_signature_sees_every_fence_change() {
        let before = lines("```rust\ncode\n```");

        assert_eq!(
            signature(&before),
            signature(&lines("```rust\nedited\n```"))
        );
        assert_ne!(signature(&before), signature(&lines("```rust\ncode\n````")));
        assert_ne!(signature(&before), signature(&lines("```rust\ncode\n```x")));
        assert_ne!(signature(&before), signature(&lines("```rust\ncode\nx```")));
    }
}
