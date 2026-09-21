//! Convert pasted rich text (HTML) into Markdown.
//!
//! The system clipboard exposes several *flavours* of the same copied
//! selection, but the toolkit only hands us the plain-text one
//! ([`iced::clipboard`] reads `text/plain`). Most sources therefore already
//! paste as plain text. What does arrive as markup is the case this module
//! exists for: text copied *as* HTML — from a "copy as HTML" action, a page's
//! source, or an application whose plain-text flavour is the markup itself.
//! Dropping raw `<p>`/`<strong>` tags into a Markdown document is never what
//! the user meant, so [`looks_like_html`] recognises it and [`to_markdown`]
//! rewrites it as Markdown before it is inserted.
//!
//! The conversion is deliberately small and dependency-free: it covers the
//! constructs Markdown itself can express (headings, emphasis, code, links,
//! images, lists, quotes, rules, tables) and *drops the tags* of anything else
//! while keeping its text. It is not a conformant HTML parser and does not try
//! to be — a paste is a best-effort convenience, and anything it cannot model
//! degrades to plain text rather than to garbage.
//!
//! Detection is intentionally conservative (see [`looks_like_html`]): pasting
//! a snippet of prose that merely *mentions* a tag must not be rewritten.

/// Elements whose presence marks a string as HTML worth converting.
///
/// Deliberately limited to what [`to_markdown`] can actually express, so text
/// that happens to contain an unrelated angle bracket is left alone.
const MARKUP_ELEMENTS: &[&str] = &[
    "html",
    "body",
    "div",
    "p",
    "br",
    "hr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "strong",
    "b",
    "em",
    "i",
    "u",
    "del",
    "s",
    "strike",
    "code",
    "pre",
    "a",
    "img",
    "ul",
    "ol",
    "li",
    "blockquote",
    "table",
    "thead",
    "tbody",
    "tr",
    "td",
    "th",
    "span",
    "font",
];

/// Whether `text` should be converted by [`to_markdown`] before being pasted.
///
/// Two conditions must hold, and both matter:
///
/// * the text must *start* with a tag, so a Markdown paragraph that merely
///   mentions `<div>` in passing is never rewritten, and
/// * it must contain at least one element [`to_markdown`] understands, so a
///   generic `<` (a comparison, a shell redirect) is not mistaken for markup.
pub fn looks_like_html(text: &str) -> bool {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('<') {
        return false;
    }

    tokens(trimmed).any(|token| match token {
        Token::Open { name, .. } | Token::Close { name } => {
            MARKUP_ELEMENTS.contains(&name.as_str())
        }
        Token::Text(_) => false,
    })
}

/// Convert an HTML fragment into Markdown.
///
/// Unknown elements are transparent: their tags vanish and their text is kept,
/// which is the least surprising outcome for a paste.
pub fn to_markdown(html: &str) -> String {
    let mut writer = Writer::default();

    for token in tokens(html) {
        match token {
            Token::Text(text) => writer.text(&decode_entities(text)),
            Token::Open {
                name,
                attrs,
                self_closing,
            } => {
                writer.open(&name, &attrs);
                if self_closing {
                    writer.close(&name);
                }
            }
            Token::Close { name } => writer.close(&name),
        }
    }

    writer.finish()
}

// -- Tokenizer ---------------------------------------------------------------

/// One piece of an HTML fragment.
#[derive(Debug, PartialEq, Eq)]
enum Token<'a> {
    /// Character data between tags, still entity-encoded.
    Text(&'a str),
    /// A start tag, with its attributes lower-cased by name.
    Open {
        name: String,
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    /// An end tag.
    Close { name: String },
}

/// Split `html` into [`Token`]s.
///
/// Comments, doctypes and processing instructions are skipped entirely; the
/// contents of `<script>` and `<style>` are dropped by the [`Writer`] rather
/// than here, so the tokenizer stays a pure lexer.
fn tokens(html: &str) -> impl Iterator<Item = Token<'_>> {
    let bytes = html.as_bytes();
    let mut index = 0;

    std::iter::from_fn(move || {
        while index < bytes.len() {
            if bytes[index] == b'<' {
                // `<` that does not start a tag is literal text, not markup.
                let Some(end) = tag_end(html, index) else {
                    let text = &html[index..];
                    index = bytes.len();
                    return Some(Token::Text(text));
                };

                let raw = &html[index + 1..end];
                index = end + 1;

                if raw.starts_with('!') || raw.starts_with('?') {
                    // Comment, doctype or processing instruction: skipped.
                    continue;
                }

                return Some(parse_tag(raw));
            }

            let start = index;
            while index < bytes.len() && bytes[index] != b'<' {
                index += 1;
            }
            return Some(Token::Text(&html[start..index]));
        }

        None
    })
}

/// Index of the `>` closing the tag that starts at `start`, honouring quoted
/// attribute values (`<a title="a > b">`) and the `<!-- -->` comment form.
fn tag_end(html: &str, start: usize) -> Option<usize> {
    let rest = &html[start + 1..];

    if let Some(body) = rest.strip_prefix("!--") {
        let close = body.find("-->")?;
        // `start + 1` skips `<`, `+ 3` skips `!--`, `+ 2` lands on the final `>`.
        return Some(start + 1 + 3 + close + 2);
    }

    let mut quote = None;
    for (offset, ch) in rest.char_indices() {
        match (quote, ch) {
            (None, '"') | (None, '\'') => quote = Some(ch),
            (Some(open), ch) if ch == open => quote = None,
            (None, '>') => return Some(start + 1 + offset),
            _ => {}
        }
    }

    None
}

/// Turn the inside of a tag (`raw`, without the angle brackets) into a token.
fn parse_tag(raw: &str) -> Token<'static> {
    if let Some(name) = raw.strip_prefix('/') {
        return Token::Close {
            name: name.trim().to_ascii_lowercase(),
        };
    }

    let self_closing = raw.trim_end().ends_with('/');
    let raw = raw.trim_end().trim_end_matches('/');

    let mut rest = raw.trim_start();
    let name_end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let name = rest[..name_end].to_ascii_lowercase();
    rest = &rest[name_end..];

    Token::Open {
        name,
        attrs: parse_attributes(rest),
        self_closing,
    }
}

/// Parse the attribute list of a start tag.
fn parse_attributes(mut rest: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();

    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return attrs;
        }

        let key_end = rest
            .find(|c: char| c.is_whitespace() || c == '=')
            .unwrap_or(rest.len());
        let key = rest[..key_end].to_ascii_lowercase();
        rest = rest[key_end..].trim_start();

        let value = match rest.strip_prefix('=') {
            Some(after) => {
                let after = after.trim_start();
                let (value, remainder) = match after.chars().next() {
                    Some(quote @ ('"' | '\'')) => {
                        let after = &after[quote.len_utf8()..];
                        match after.find(quote) {
                            Some(end) => (&after[..end], &after[end + quote.len_utf8()..]),
                            None => (after, ""),
                        }
                    }
                    _ => {
                        let end = after
                            .find(|c: char| c.is_whitespace())
                            .unwrap_or(after.len());
                        (&after[..end], &after[end..])
                    }
                };
                rest = remainder;
                decode_entities(value)
            }
            None => String::new(),
        };

        if !key.is_empty() {
            attrs.push((key, value));
        }
    }
}

/// Decode the HTML entities a clipboard payload realistically carries.
///
/// Unknown entities are left verbatim rather than dropped: showing `&foo;` is
/// far less destructive than silently deleting it.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];

        // Entities are short; a `;` far away is a stray ampersand.
        let Some(end) = after.find(';').filter(|end| *end <= 10) else {
            out.push('&');
            rest = after;
            continue;
        };

        let name = &after[..end];
        match named_entity(name) {
            Some(decoded) => out.push_str(decoded),
            None => match numeric_entity(name) {
                Some(decoded) => out.push(decoded),
                None => {
                    out.push('&');
                    out.push_str(name);
                    out.push(';');
                }
            },
        }

        rest = &after[end + 1..];
    }

    out.push_str(rest);
    out
}

/// Expand a named entity, e.g. `amp` → `&`.
fn named_entity(name: &str) -> Option<&'static str> {
    Some(match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" | "#39" => "'",
        // A non-breaking space pasted into Markdown source is invisible and
        // breaks list/heading detection, so it becomes an ordinary space.
        "nbsp" => " ",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "ldquo" => "\u{201c}",
        "rdquo" => "\u{201d}",
        "bull" => "•",
        "middot" => "·",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        _ => return None,
    })
}

/// Expand a numeric entity body, e.g. `#x2014` or `#8212`.
fn numeric_entity(name: &str) -> Option<char> {
    let digits = name.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse().ok()?,
    };
    char::from_u32(code)
}

// -- Markdown writer ---------------------------------------------------------

/// A block-level container currently open, used to build each line's prefix.
#[derive(Debug, Clone, Copy)]
enum Container {
    /// A `<blockquote>`, rendered as `> `.
    Quote,
    /// A list; `next` is the number the next `<li>` takes when ordered.
    List { ordered: bool, next: u32 },
}

/// Accumulates Markdown as tokens are walked.
#[derive(Default)]
struct Writer {
    out: String,
    /// Open block containers, outermost first.
    containers: Vec<Container>,
    /// Line breaks owed before the next character is written: `1` starts a new
    /// line, `2` leaves a blank line between blocks.
    ///
    /// Buffered rather than written eagerly so trailing whitespace never
    /// reaches the output and consecutive block ends collapse into one break.
    pending_breaks: usize,
    /// Whether the current line already has content (so a prefix is not
    /// re-emitted mid-line).
    line_started: bool,
    /// Depth of `<pre>` nesting: inside it, whitespace is significant.
    pre_depth: usize,
    /// Depth of elements whose text must be discarded (`<script>`, `<style>`).
    hidden_depth: usize,
    /// Start tags still open, so a close tag can be matched to its opener.
    open_tags: Vec<String>,
    /// `href` of the innermost open `<a>`, emitted when it closes.
    links: Vec<String>,
    /// Byte offset in `out` where the innermost open `<a>`'s text begins.
    link_starts: Vec<usize>,
    /// Whether the current table row is a header row, so a GFM delimiter row
    /// can be emitted underneath it.
    header_row: bool,
    /// Cells seen in the current header row, to size the delimiter row.
    header_cells: usize,
    /// Whether a cell separator is owed before the next cell's text.
    row_started: bool,
}

impl Writer {
    /// Emit character data.
    fn text(&mut self, text: &str) {
        if self.hidden_depth > 0 || text.is_empty() {
            return;
        }

        if self.pre_depth > 0 {
            // Inside `<pre>` every space and newline is content.
            let text = text.to_owned();
            for (index, line) in text.split('\n').enumerate() {
                if index > 0 {
                    self.break_line(1);
                }
                if !line.is_empty() {
                    self.raw(line);
                }
            }
            return;
        }

        // Outside `<pre>`, HTML collapses runs of whitespace to one space and
        // ignores it entirely at the edges of a block.
        let leading = text.starts_with(|c: char| c.is_whitespace());
        let trailing = text.ends_with(|c: char| c.is_whitespace());
        let mut words = text.split_whitespace().peekable();

        if words.peek().is_none() {
            // Whitespace only: it still separates two inline runs.
            if leading && self.line_started && self.pending_breaks == 0 {
                self.raw(" ");
            }
            return;
        }

        if leading && self.line_started && self.pending_breaks == 0 {
            self.raw(" ");
        }

        while let Some(word) = words.next() {
            let escaped = escape_markdown(word, !self.line_started);
            self.raw(&escaped);
            if words.peek().is_some() {
                self.raw(" ");
            }
        }

        if trailing {
            self.raw(" ");
        }
    }

    /// Handle a start tag.
    fn open(&mut self, name: &str, attrs: &[(String, String)]) {
        if matches!(name, "script" | "style" | "head" | "title") {
            self.hidden_depth += 1;
            self.open_tags.push(name.to_owned());
            return;
        }

        if self.hidden_depth > 0 {
            self.open_tags.push(name.to_owned());
            return;
        }

        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.break_line(2);
                let level = name[1..].parse::<usize>().unwrap_or(1);
                self.raw(&"#".repeat(level));
                self.raw(" ");
            }
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "figure"
            | "figcaption" | "dl" | "dt" | "dd" => self.break_line(2),
            "br" => self.break_line(1),
            "hr" => {
                self.break_line(2);
                self.raw("---");
                self.break_line(2);
            }
            "strong" | "b" => self.raw("**"),
            "em" | "i" => self.raw("*"),
            "del" | "s" | "strike" => self.raw("~~"),
            "code" if self.pre_depth == 0 => self.raw("`"),
            "pre" => {
                self.break_line(2);
                self.raw("```");
                self.break_line(1);
                self.pre_depth += 1;
            }
            "blockquote" => {
                self.break_line(2);
                self.containers.push(Container::Quote);
            }
            "ul" | "ol" => {
                // A nested list continues the current item's line, so only a
                // single break is owed; a top-level one starts a new block.
                self.break_line(if self.containers.is_empty() { 2 } else { 1 });
                self.containers.push(Container::List {
                    ordered: name == "ol",
                    next: attrs
                        .iter()
                        .find(|(key, _)| key == "start")
                        .and_then(|(_, value)| value.parse().ok())
                        .unwrap_or(1),
                });
            }
            "li" => {
                self.break_line(1);
                let marker = match self.containers.last_mut() {
                    Some(Container::List { ordered, next }) => {
                        let marker = if *ordered {
                            format!("{next}. ")
                        } else {
                            String::from("- ")
                        };
                        *next += 1;
                        marker
                    }
                    // A stray `<li>` outside any list still reads as an item.
                    _ => String::from("- "),
                };
                self.raw(&marker);
            }
            "a" => {
                self.links.push(
                    attrs
                        .iter()
                        .find(|(key, _)| key == "href")
                        .map(|(_, value)| value.clone())
                        .unwrap_or_default(),
                );
                self.start_line();
                self.link_starts.push(self.out.len());
            }
            "img" => {
                let attr = |wanted: &str| {
                    attrs
                        .iter()
                        .find(|(key, _)| key == wanted)
                        .map(|(_, value)| value.as_str())
                        .unwrap_or_default()
                };
                let src = attr("src");
                if !src.is_empty() {
                    let alt = attr("alt");
                    self.raw(&format!("![{alt}]({src})"));
                }
            }
            "table" => self.break_line(2),
            "tr" => {
                self.break_line(1);
                self.row_started = false;
                self.header_row = false;
                self.header_cells = 0;
            }
            "th" | "td" => {
                if !self.row_started {
                    self.raw("| ");
                    self.row_started = true;
                }
                if name == "th" {
                    self.header_row = true;
                    self.header_cells += 1;
                }
            }
            _ => {}
        }

        self.open_tags.push(name.to_owned());
    }

    /// Handle an end tag.
    fn close(&mut self, name: &str) {
        // Ignore a close tag with no matching opener: the fragment is
        // malformed, and honouring it would unbalance the container stack.
        let Some(index) = self.open_tags.iter().rposition(|open| open == name) else {
            return;
        };
        self.open_tags.truncate(index);

        if matches!(name, "script" | "style" | "head" | "title") {
            self.hidden_depth = self.hidden_depth.saturating_sub(1);
            return;
        }

        if self.hidden_depth > 0 {
            return;
        }

        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.break_line(2),
            "p" | "div" | "section" | "article" | "header" | "footer" | "main" | "figure"
            | "figcaption" | "dl" | "dt" | "dd" => self.break_line(2),
            "strong" | "b" => self.raw("**"),
            "em" | "i" => self.raw("*"),
            "del" | "s" | "strike" => self.raw("~~"),
            "code" if self.pre_depth == 0 => self.raw("`"),
            "pre" => {
                self.pre_depth = self.pre_depth.saturating_sub(1);
                self.break_line(1);
                self.raw("```");
                self.break_line(2);
            }
            "blockquote" => {
                self.pop_container();
                self.break_line(2);
            }
            "ul" | "ol" => {
                self.pop_container();
                self.break_line(if self.containers.is_empty() { 2 } else { 1 });
            }
            "li" => self.break_line(1),
            "a" => {
                let href = self.links.pop().unwrap_or_default();
                let start = self.link_starts.pop().unwrap_or(self.out.len());
                // An anchor with no target (or no text) is just its text.
                if !href.is_empty() && start < self.out.len() {
                    self.out.insert(start, '[');
                    self.raw(&format!("]({href})"));
                }
            }
            "th" | "td" => self.raw(" | "),
            "tr" => {
                if self.header_row && self.header_cells > 0 {
                    // GFM needs a delimiter row for the table to render.
                    self.break_line(1);
                    self.raw("|");
                    for _ in 0..self.header_cells {
                        self.raw(" --- |");
                    }
                }
                self.break_line(1);
                self.row_started = false;
            }
            "table" => self.break_line(2),
            _ => {}
        }
    }

    /// Finish the document, trimming the trailing whitespace the block ends
    /// would otherwise leave behind.
    fn finish(mut self) -> String {
        while self.out.ends_with([' ', '\t']) {
            self.out.pop();
        }
        self.out
    }

    /// Drop the innermost container, keeping the stack balanced.
    fn pop_container(&mut self) {
        self.containers.pop();
    }

    /// Request `count` line breaks before the next character.
    ///
    /// Requests are merged rather than added, so two adjacent block ends do not
    /// open a widening run of blank lines.
    fn break_line(&mut self, count: usize) {
        if self.out.is_empty() && !self.line_started {
            // Nothing written yet: a leading blank line helps no one.
            return;
        }
        self.pending_breaks = self.pending_breaks.max(count);
        self.line_started = false;
    }

    /// Write `text` verbatim, first settling any owed line breaks and prefix.
    fn raw(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.start_line();
        self.out.push_str(text);
    }

    /// Emit the owed line breaks and this line's container prefix.
    fn start_line(&mut self) {
        if self.pending_breaks > 0 {
            // Trailing spaces before a break are invisible noise at best and a
            // Markdown hard break at worst.
            while self.out.ends_with([' ', '\t']) {
                self.out.pop();
            }
            for _ in 0..self.pending_breaks {
                self.out.push('\n');
            }
            self.pending_breaks = 0;
        }

        if self.line_started {
            return;
        }
        self.line_started = true;

        // A fenced code block's contents are literal: no quote or list prefix.
        if self.pre_depth > 0 {
            return;
        }

        for container in &self.containers {
            match container {
                Container::Quote => self.out.push_str("> "),
                // Indentation for the continuation of a list item; the item's
                // own marker is written by `<li>` right after this.
                Container::List { .. } => self.out.push_str("  "),
            }
        }

        // The marker `<li>` is about to write replaces its own indent level.
        if matches!(self.containers.last(), Some(Container::List { .. })) {
            self.out.truncate(self.out.len() - 2);
        }
    }
}

/// Escape the Markdown syntax characters in a word taken from HTML text.
///
/// Only the characters that would silently re-format the paste are escaped:
/// emphasis, code and link markers anywhere, and the block markers that are
/// only meaningful at the start of a line. `_` is left alone — CommonMark
/// ignores it inside a word, and escaping it mangles `snake_case`.
fn escape_markdown(word: &str, at_line_start: bool) -> String {
    let mut out = String::with_capacity(word.len());

    if at_line_start {
        // `- x`, `> x`, `# x` and `1. x` only start a block at column 0.
        let block_marker = match word {
            "#" | "##" | "###" | "####" | "#####" | "######" | "-" | "+" | ">" => true,
            _ => word.strip_suffix('.').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            }),
        };
        if block_marker {
            out.push('\\');
        }
    }

    for ch in word.chars() {
        if matches!(ch, '\\' | '`' | '*' | '[' | ']' | '<' | '>') {
            out.push('\\');
        }
        out.push(ch);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_not_mistaken_for_html() {
        assert!(!looks_like_html("just some text"));
        assert!(!looks_like_html("a < b and c > d"));
        assert!(!looks_like_html("# A heading\n\nwith <angle> words"));
        assert!(!looks_like_html(""));
    }

    #[test]
    fn markup_is_recognised() {
        assert!(looks_like_html("<p>hello</p>"));
        assert!(looks_like_html("  \n<div><span>x</span></div>"));
        assert!(looks_like_html(
            "<!DOCTYPE html><html><body>hi</body></html>"
        ));
    }

    #[test]
    fn an_unknown_leading_tag_alone_is_not_markup() {
        // Starts with `<` but names nothing we can convert: left verbatim.
        assert!(!looks_like_html("<mycustomthing/>"));
    }

    #[test]
    fn headings_and_paragraphs_become_markdown() {
        assert_eq!(
            to_markdown("<h1>Title</h1><p>Body text.</p>"),
            "# Title\n\nBody text."
        );
    }

    #[test]
    fn inline_formatting_is_converted() {
        assert_eq!(
            to_markdown("<p>a <strong>bold</strong> and <em>italic</em> word</p>"),
            "a **bold** and *italic* word"
        );
    }

    #[test]
    fn links_and_images_keep_their_targets() {
        assert_eq!(
            to_markdown(r#"<a href="https://example.com">site</a>"#),
            "[site](https://example.com)"
        );
        assert_eq!(
            to_markdown(r#"<img src="/a.png" alt="A picture">"#),
            "![A picture](/a.png)"
        );
    }

    #[test]
    fn an_anchor_without_a_target_keeps_only_its_text() {
        assert_eq!(to_markdown("<a>plain</a>"), "plain");
    }

    #[test]
    fn lists_become_markdown_lists() {
        assert_eq!(
            to_markdown("<ul><li>one</li><li>two</li></ul>"),
            "- one\n- two"
        );
        assert_eq!(
            to_markdown("<ol><li>one</li><li>two</li></ol>"),
            "1. one\n2. two"
        );
    }

    #[test]
    fn an_ordered_list_honours_its_start_attribute() {
        assert_eq!(
            to_markdown(r#"<ol start="5"><li>five</li></ol>"#),
            "5. five"
        );
    }

    #[test]
    fn nested_lists_are_indented() {
        assert_eq!(
            to_markdown("<ul><li>outer<ul><li>inner</li></ul></li></ul>"),
            "- outer\n  - inner"
        );
    }

    #[test]
    fn blockquotes_are_prefixed() {
        assert_eq!(
            to_markdown("<blockquote><p>quoted</p></blockquote>"),
            "> quoted"
        );
    }

    #[test]
    fn preformatted_text_keeps_its_whitespace() {
        assert_eq!(
            to_markdown("<pre><code>fn main() {\n    let x = 1;\n}</code></pre>"),
            "```\nfn main() {\n    let x = 1;\n}\n```"
        );
    }

    #[test]
    fn inline_code_is_backquoted() {
        assert_eq!(to_markdown("<p>use <code>cargo</code></p>"), "use `cargo`");
    }

    #[test]
    fn breaks_and_rules_are_converted() {
        assert_eq!(to_markdown("a<br>b"), "a\nb");
        assert_eq!(to_markdown("<p>a</p><hr><p>b</p>"), "a\n\n---\n\nb");
    }

    #[test]
    fn a_table_gets_a_delimiter_row() {
        assert_eq!(
            to_markdown(
                "<table><tr><th>A</th><th>B</th></tr><tr><td>1</td><td>2</td></tr></table>"
            ),
            "| A | B |\n| --- | --- |\n| 1 | 2 |"
        );
    }

    #[test]
    fn scripts_and_styles_are_dropped() {
        assert_eq!(
            to_markdown("<p>keep</p><script>alert('x')</script><style>p{}</style>"),
            "keep"
        );
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(
            to_markdown("<p>a &amp; b &lt;c&gt; &nbsp;d &#8212; e &#x27;f&#x27;</p>"),
            "a & b \\<c\\> d — e 'f'"
        );
    }

    #[test]
    fn an_unknown_entity_survives_verbatim() {
        assert_eq!(to_markdown("<p>&notanentity;</p>"), "&notanentity;");
    }

    #[test]
    fn whitespace_between_inline_elements_is_collapsed() {
        assert_eq!(to_markdown("<p>one   two\n\tthree</p>"), "one two three");
        assert_eq!(to_markdown("<p><em>a</em> <em>b</em></p>"), "*a* *b*");
    }

    #[test]
    fn unknown_elements_are_transparent() {
        assert_eq!(to_markdown("<p>a <mark>b</mark> c</p>"), "a b c");
    }

    #[test]
    fn markdown_characters_in_text_are_escaped() {
        assert_eq!(
            to_markdown("<p>2 * 3 and [brackets] and `ticks`</p>"),
            "2 \\* 3 and \\[brackets\\] and \\`ticks\\`"
        );
    }

    #[test]
    fn a_line_leading_block_marker_is_escaped() {
        assert_eq!(to_markdown("<p>- not a list</p>"), "\\- not a list");
        assert_eq!(to_markdown("<p>1. not ordered</p>"), "\\1. not ordered");
    }

    #[test]
    fn comments_and_doctypes_are_skipped() {
        assert_eq!(
            to_markdown("<!DOCTYPE html><!-- note --><p>text</p>"),
            "text"
        );
        assert_eq!(to_markdown("<!-- a > b --><p>x</p>"), "x");
    }

    #[test]
    fn an_attribute_value_may_contain_a_closing_bracket() {
        assert_eq!(
            to_markdown(r#"<a href="https://e.com/?a=1&amp;b=2" title="a > b">x</a>"#),
            "[x](https://e.com/?a=1&b=2)"
        );
    }

    #[test]
    fn a_stray_unclosed_angle_bracket_is_text() {
        assert_eq!(to_markdown("<p>x</p> 3 < 4"), "x\n\n3 \\< 4");
    }

    #[test]
    fn an_unmatched_close_tag_does_not_unbalance_the_output() {
        assert_eq!(to_markdown("</ul><p>text</p>"), "text");
        assert_eq!(
            to_markdown("<blockquote>a</blockquote></blockquote>b"),
            "> a\n\nb"
        );
    }

    #[test]
    fn a_whole_document_converts() {
        let html = "<html><head><title>T</title></head>\
                    <body><h2>Sub</h2><p>Hello <b>world</b>.</p></body></html>";
        assert_eq!(to_markdown(html), "## Sub\n\nHello **world**.");
    }
}
