//! Where the raw editor wraps a line, simulated.
//!
//! The raw editor lays its text out in a monospace font, word-wrapped to its
//! width, and iced does not expose the rows that produces. Several things need
//! them *before* the next layout: landing the caret on the right row when it
//! moves onto a wrapped line, keeping the column it aims for across lines,
//! placing a click, and knowing how far down a tall editor the caret sits so
//! it can be scrolled into view. So the wrap is simulated here — greedily,
//! word by word, as the editor's layout does it — which with a monospace font
//! is a matter of counting characters.
//!
//! It is an approximation: tabs and double-width characters are counted as one
//! column. Everything built on it degrades gracefully — at worst the caret
//! lands a row away from the ideal one.

/// Byte offsets at which each visual row of `line` starts when it is
/// word-wrapped at `capacity` characters per row. Always at least one row.
///
/// Spaces hang past the edge of a row rather than wrapping, and a word longer
/// than a whole row sits on a row of its own, overflowing it — both as the
/// editor's word wrapping lays them out.
pub fn rows(line: &str, capacity: usize) -> Vec<usize> {
    let mut rows = vec![0];

    // A line no longer in bytes than a row is in characters cannot wrap.
    if line.len() <= capacity {
        return rows;
    }

    let mut used = 0;
    let mut chars = line.char_indices().peekable();

    while let Some(&(start, ch)) = chars.peek() {
        if ch == ' ' {
            chars.next();
            used += 1;
            continue;
        }

        let mut word = 0;
        while chars.next_if(|&(_, ch)| ch != ' ').is_some() {
            word += 1;
        }

        if used > 0 && used + word > capacity {
            rows.push(start);
            used = word;
        } else {
            used += word;
        }
    }

    rows
}

/// Index of the row that holds byte `column`.
pub fn row_of(rows: &[usize], column: usize) -> usize {
    rows.partition_point(|&start| start <= column)
        .saturating_sub(1)
}

/// Characters from the start of `column`'s row up to `column`: its visual
/// column.
pub fn column_in_row(line: &str, rows: &[usize], column: usize) -> usize {
    let column = column.min(line.len());
    let start = rows[row_of(rows, column)];

    line.get(start..column)
        .map_or(0, |text| text.chars().count())
}

/// Byte offset `chars` characters into row `row` of `line`, kept on that row.
///
/// Every row but the last stops short of its final character: a caret right
/// at the point where a row wraps is drawn at the start of the next one.
pub fn offset_in_row(line: &str, rows: &[usize], row: usize, chars: usize) -> usize {
    let row = row.min(rows.len() - 1);
    let start = rows[row];
    let end = rows.get(row + 1).copied().unwrap_or(line.len());
    let last = row + 1 == rows.len();

    let text = &line[start..end];
    let width = text.chars().count();
    let chars = if last {
        chars.min(width)
    } else {
        chars.min(width.saturating_sub(1))
    };

    start
        + text
            .char_indices()
            .nth(chars)
            .map_or(text.len(), |(offset, _)| offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_line_is_one_row() {
        assert_eq!(rows("hello world", 20), vec![0]);
        assert_eq!(rows("", 20), vec![0]);
    }

    #[test]
    fn words_move_whole_to_the_next_row() {
        // "hello world again" at 11 columns: "hello world" fits exactly.
        assert_eq!(rows("hello world again", 11), vec![0, 12]);
        // At 10, "world" no longer fits after "hello ".
        assert_eq!(rows("hello world again", 10), vec![0, 6, 12]);
    }

    #[test]
    fn spaces_hang_past_the_edge() {
        // Trailing spaces never force a wrap of their own.
        assert_eq!(rows("abcde     fgh", 5), vec![0, 10]);
    }

    #[test]
    fn an_overlong_word_sits_on_its_own_row() {
        assert_eq!(rows("a abcdefghij b", 5), vec![0, 2, 13]);
    }

    #[test]
    fn multibyte_text_wraps_by_characters_not_bytes() {
        // Ten two-byte characters and a space per word: 20 bytes, 10 chars.
        let line = "éééééééééé éééééééééé";
        assert_eq!(rows(line, 10), vec![0, 21]);
        assert_eq!(rows(line, 21), vec![0]);
    }

    #[test]
    fn columns_are_counted_from_the_start_of_their_row() {
        let line = "hello world again";
        let rows = rows(line, 11);

        assert_eq!(row_of(&rows, 0), 0);
        assert_eq!(row_of(&rows, 11), 0);
        assert_eq!(row_of(&rows, 12), 1);
        assert_eq!(column_in_row(line, &rows, 14), 2);
        assert_eq!(column_in_row(line, &rows, 99), 5);
    }

    #[test]
    fn offsets_stay_on_their_row() {
        let line = "hello world again";
        let rows = rows(line, 11);

        assert_eq!(offset_in_row(line, &rows, 0, 3), 3);
        // Clamped short of the wrap point on every row but the last...
        assert_eq!(offset_in_row(line, &rows, 0, 40), 11);
        // ...and to the end of the line on the last.
        assert_eq!(offset_in_row(line, &rows, 1, 40), line.len());
        assert_eq!(offset_in_row(line, &rows, 1, 2), 14);
    }

    #[test]
    fn offsets_land_on_character_boundaries() {
        let line = "ééé ééé";
        let rows = rows(line, 3);

        assert_eq!(rows, vec![0, 7]);
        assert_eq!(offset_in_row(line, &rows, 1, 1), 9);
        assert!(line.is_char_boundary(offset_in_row(line, &rows, 0, 2)));
    }
}
