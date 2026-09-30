//! A headless driver for the real UI pipeline, for tests and benchmarks.
//!
//! The unit tests in `app.rs` call `update` and `view`, but most of what a user
//! actually feels happens *after* `view` returns, inside iced's runtime:
//! diffing the new widget tree against the old one, laying it out (which is
//! where text is shaped), and drawing it. This module runs that loop without a
//! window — a `UserInterface` built from `view()` with the software renderer,
//! fed events, its messages and tasks run back through `update` — in the same
//! order `iced_winit` does every frame.
//!
//! It serves two purposes:
//!
//! * **measuring** — the `#[ignore]`d benchmarks time whole frames, so they
//!   see the layout cost of a change and not only the cost of building it;
//! * **asserting on real geometry** — whether the caret is on screen after a
//!   key press can only be answered with a real layout.

use std::time::{Duration, Instant};

use iced::advanced::clipboard;
use iced::advanced::renderer::Style;
use iced::advanced::widget::{Id, Operation, operation};
use iced::keyboard::{self, key};
use iced::{Event, Font, Pixels, Point, Rectangle, Size, Task, Vector, mouse, window};
use iced_runtime::user_interface::{Cache, UserInterface};
use iced_runtime::{Action, task};

use super::{ACTIVE_LINE_ID, DOCUMENT_ID, MarkdownEditor, Message, RevealMode};
use crate::fences;

/// Frames run by [`Harness::settle`] before it gives up on the UI going quiet.
///
/// A healthy UI settles in two or three: the one that handles the input, and
/// the ones that follow the scrollable's viewport notification. Hitting the
/// cap means messages are feeding back into themselves.
const MAX_FRAMES: usize = 16;

/// Window size the harness lays the UI out in.
pub(super) const WINDOW: Size = Size::new(1200.0, 900.0);

/// Time spent in each phase of the frames run so far.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Timings {
    /// `MarkdownEditor::update`, for every message the frames produced.
    pub update: Duration,
    /// `view()`, building the widget tree.
    pub view: Duration,
    /// Building the `UserInterface` from it: tree diff and layout.
    pub build: Duration,
    /// Delivering events to the widgets.
    pub events: Duration,
    /// Drawing (recording primitives; nothing is rasterized).
    pub draw: Duration,
    /// Frames run.
    pub frames: u32,
}

/// Where the document and the caret are, measured from a real layout.
#[derive(Debug, Clone, Copy)]
pub(super) struct Geometry {
    /// On-screen bounds of the document scrollable.
    pub viewport: Rectangle,
    /// Scroll offset of the document.
    pub offset: Vector,
    /// Layout bounds of the active editor, in content coordinates.
    pub editor: Rectangle,
}

impl Geometry {
    /// The editor's bounds as they appear on screen.
    pub fn editor_on_screen(&self) -> Rectangle {
        Rectangle {
            y: self.editor.y - self.offset.y,
            ..self.editor
        }
    }

    /// Whether the part of the editor `span` — distances from its top edge —
    /// is inside the visible part of the document.
    pub fn shows(&self, (top, bottom): (f32, f32)) -> bool {
        let editor = self.editor_on_screen();
        editor.y + top >= self.viewport.y - 0.5
            && editor.y + bottom <= self.viewport.y + self.viewport.height + 0.5
    }
}

/// An editor wired to a headless `UserInterface`.
pub(super) struct Harness {
    pub editor: MarkdownEditor,
    renderer: iced::Renderer,
    cache: Cache,
    cursor: mouse::Cursor,
    /// Widget operations produced by tasks, applied to the next UI built.
    operations: Vec<Box<dyn Operation>>,
    /// Messages produced by tasks, handled before the next UI is built.
    messages: Vec<Message>,
    pub timings: Timings,
}

impl Harness {
    /// A harness showing `document`, settled on its first frames.
    pub fn new(document: &str) -> Self {
        let (mut editor, task) = MarkdownEditor::new_with_file(None);
        editor.load_document(document);
        editor.rebuild();

        let mut harness = Self {
            editor,
            renderer: iced_renderer::fallback::Renderer::Secondary(iced_tiny_skia::Renderer::new(
                Font::DEFAULT,
                Pixels(16.0),
            )),
            cache: Cache::default(),
            // Over the middle of the document, where a wheel event scrolls it.
            cursor: mouse::Cursor::Available(Point::new(WINDOW.width / 2.0, WINDOW.height / 2.0)),
            operations: Vec::new(),
            messages: Vec::new(),
            timings: Timings::default(),
        };

        // The focus task `new` returns, exactly as the runtime would run it.
        harness.run_task(task);
        harness.run_task(harness.editor.focus_active());
        harness.settle(Vec::new());
        harness.timings = Timings::default();
        harness
    }

    /// Handle `message` the way the runtime would: update, then frames until
    /// the UI is quiet again.
    pub fn send(&mut self, message: Message) {
        self.messages.push(message);
        self.settle(Vec::new());
    }

    /// Press `key` (with `modifiers`) on the focused widget.
    pub fn press(&mut self, key: keyboard::Key, modifiers: keyboard::Modifiers) {
        let text = match &key {
            keyboard::Key::Character(c) if !modifiers.command() => Some(c.clone()),
            _ => None,
        };

        self.settle(vec![Event::Keyboard(keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
            location: keyboard::Location::Standard,
            modifiers,
            text,
            repeat: false,
        })]);
    }

    /// Press a named key with no modifiers.
    pub fn press_named(&mut self, named: key::Named) {
        self.press(keyboard::Key::Named(named), keyboard::Modifiers::empty());
    }

    /// Type `text`, one key press per character.
    pub fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.press(
                keyboard::Key::Character(ch.to_string().into()),
                keyboard::Modifiers::empty(),
            );
        }
    }

    /// Click the left button at `point`, on screen.
    pub fn click_at(&mut self, point: Point) {
        let resting = self.cursor;
        self.cursor = mouse::Cursor::Available(point);

        self.settle(vec![
            Event::Mouse(mouse::Event::CursorMoved { position: point }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        ]);
        self.settle(vec![Event::Mouse(mouse::Event::ButtonReleased(
            mouse::Button::Left,
        ))]);

        self.cursor = resting;
    }

    /// Where on screen the first glyph of the rendered segment covering
    /// `line` is drawn — past the column's and the segment's padding.
    pub fn text_origin(&mut self, line: usize) -> Point {
        let geometry = self.geometry();
        let measurements = self.editor.measurements.borrow();
        let segment = measurements
            .children
            .iter()
            .find(|child| {
                !matches!(child.key, crate::document::Key::Gap(_))
                    && (child.lines.0..child.lines.1).contains(&line)
            })
            .expect("a rendered segment covering the line");

        let padding = f32::from(super::LINE_PADDING);
        Point::new(
            geometry.viewport.x + super::DOCUMENT_PADDING + padding,
            geometry.viewport.y + segment.top - geometry.offset.y + padding,
        )
    }

    /// Turn the mouse wheel over the document by `pixels` (positive scrolls
    /// down, the way a touchpad reports it).
    pub fn scroll(&mut self, pixels: f32) {
        self.settle(vec![Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Pixels { x: 0.0, y: -pixels },
        })]);
    }

    /// Measure where the document and the caret are right now.
    pub fn geometry(&mut self) -> Geometry {
        let mut probe = Probe::default();
        let mut ui = build(&self.editor, &mut self.cache, &mut self.renderer);
        ui.operate(&self.renderer, &mut probe);
        self.cache = ui.into_cache();

        let (viewport, offset) = probe.scrollable.expect("the document scrollable");
        Geometry {
            viewport,
            offset,
            editor: probe.editor.expect("the active editor"),
        }
    }

    /// Run frames until no messages or operations are left, delivering
    /// `events` to the first one.
    fn settle(&mut self, mut events: Vec<Event>) {
        for _ in 0..MAX_FRAMES {
            let started = Instant::now();
            for message in std::mem::take(&mut self.messages) {
                let task = self.editor.update(message);
                self.run_task(task);
            }
            self.timings.update += started.elapsed();

            let Self {
                editor,
                renderer,
                cache,
                cursor,
                operations,
                messages,
                timings,
            } = self;

            let started = Instant::now();
            let element = editor.view();
            timings.view += started.elapsed();

            let started = Instant::now();
            let mut ui = UserInterface::build(element, WINDOW, std::mem::take(cache), renderer);
            timings.build += started.elapsed();

            for operation in operations.drain(..) {
                run_operation(&mut ui, renderer, operation);
            }

            // Every frame ends in a redraw, which is also when a scrollable
            // reports a viewport that changed underneath it.
            events.push(Event::Window(
                window::Event::RedrawRequested(Instant::now()),
            ));

            let started = Instant::now();
            let _ = ui.update(&events, *cursor, renderer, &mut clipboard::Null, messages);
            events.clear();
            timings.events += started.elapsed();

            let started = Instant::now();
            let theme = editor.theme();
            ui.draw(
                renderer,
                &theme,
                &Style {
                    text_color: theme.palette().text,
                },
                *cursor,
            );
            timings.draw += started.elapsed();
            timings.frames += 1;

            *cache = ui.into_cache();

            if self.messages.is_empty() && self.operations.is_empty() {
                return;
            }
        }

        panic!("the UI did not settle within {MAX_FRAMES} frames");
    }

    /// Queue what `task` produces: its messages and its widget operations.
    fn run_task(&mut self, task: Task<Message>) {
        use iced::futures::StreamExt;

        let Some(stream) = task::into_stream(task) else {
            return;
        };

        for action in iced::futures::executor::block_on(stream.collect::<Vec<_>>()) {
            match action {
                Action::Output(message) => self.messages.push(message),
                Action::Widget(operation) => self.operations.push(operation),
                // Windows, clipboard, fonts: nothing a headless test needs.
                _ => {}
            }
        }
    }
}

/// Build a `UserInterface` for `editor`'s current view, reusing (and taking)
/// the widget tree cached from the previous frame.
fn build<'a>(
    editor: &'a MarkdownEditor,
    cache: &mut Cache,
    renderer: &mut iced::Renderer,
) -> UserInterface<'a, Message, iced::Theme, iced::Renderer> {
    UserInterface::build(editor.view(), WINDOW, std::mem::take(cache), renderer)
}

/// Apply `operation` to `ui`, following any chained operations it returns.
fn run_operation(
    ui: &mut UserInterface<'_, Message, iced::Theme, iced::Renderer>,
    renderer: &iced::Renderer,
    mut operation: Box<dyn Operation>,
) {
    loop {
        ui.operate(renderer, operation.as_mut());

        match operation.finish() {
            operation::Outcome::Chain(next) => operation = next,
            _ => return,
        }
    }
}

/// A code-heavy document: `sections` × (heading, paragraph, fenced code).
pub(super) fn sectioned_document(sections: usize) -> String {
    (0..sections)
        .map(|i| {
            format!(
                "## Section {i}\n\n\
                 Lorem ipsum dolor sit amet, consectetur adipiscing elit.\n\n\
                 ```rust\nfn main() {{ let _ = {i} + 1; }}\n```\n\n"
            )
        })
        .collect()
}

/// A prose-heavy document: long wrapping paragraphs, lists, quotes and tables,
/// closer to real notes than [`sectioned_document`].
pub(super) fn prose_document(sections: usize) -> String {
    let paragraph = "Markdown editors live and die by how they feel under the \
                     fingers: every key press has to land on screen before the \
                     next one, whether the document is ten lines long or ten \
                     thousand, and the text must never jump around under the caret.";

    (0..sections)
        .map(|i| {
            format!(
                "# Chapter {i}\n\n\
                 {paragraph}\n\n\
                 - first item with **bold** text\n\
                 - second item with *emphasis* and `code`\n\
                 - third item with a [link](https://example.com/{i})\n\n\
                 > A quotation that wraps across a couple of lines once the \
                 window is narrow enough to force it.\n\n\
                 | Key | Value | Notes |\n\
                 |-----|-------|-------|\n\
                 | a{i} | 1 | first |\n\
                 | b{i} | 2 | second |\n\n\
                 {paragraph}\n\n"
            )
        })
        .collect()
}

/// Distribution of per-step costs.
pub(super) struct Samples(Vec<Duration>);

impl Samples {
    fn percentile(&self, p: f64) -> Duration {
        let mut sorted = self.0.clone();
        sorted.sort();
        let index = ((sorted.len() - 1) as f64 * p).round() as usize;
        sorted[index]
    }

    fn mean(&self) -> Duration {
        self.0.iter().sum::<Duration>() / self.0.len() as u32
    }

    fn report(&self, label: &str) {
        println!(
            "  {label:<24} mean {:>9.3?}   p50 {:>9.3?}   p95 {:>9.3?}   max {:>9.3?}",
            self.mean(),
            self.percentile(0.5),
            self.percentile(0.95),
            self.percentile(1.0),
        );
    }
}

/// Time `steps` repetitions of `step`, one sample each.
fn sample(harness: &mut Harness, steps: usize, mut step: impl FnMut(&mut Harness)) -> Samples {
    harness.timings = Timings::default();

    let samples = Samples(
        (0..steps)
            .map(|_| {
                let started = Instant::now();
                step(harness);
                started.elapsed()
            })
            .collect(),
    );

    if std::env::var_os("MD_EDITOR_PHASES").is_some() {
        let t = harness.timings;
        let per = |d: Duration| d / steps as u32;
        println!(
            "      frames/step {:.2}  update {:?}  view {:?}  build {:?}  events {:?}  draw {:?}",
            t.frames as f32 / steps as f32,
            per(t.update),
            per(t.view),
            per(t.build),
            per(t.events),
            per(t.draw),
        );
    }

    samples
}

/// Frame costs for the interactions the user feels most: moving the caret,
/// scrolling, and typing.
fn measure_interactions(name: &str, document: &str) {
    let mut harness = Harness::new(document);
    println!(
        "[{name}: {} lines, {} blocks]",
        harness.editor.lines.len(),
        harness.editor.blocks.len()
    );

    sample(&mut harness, 300, |h| h.press_named(key::Named::ArrowDown))
        .report("arrow down (frame)");
    sample(&mut harness, 300, |h| h.press_named(key::Named::ArrowUp)).report("arrow up (frame)");

    let mut harness = Harness::new(document);
    sample(&mut harness, 400, |h| h.scroll(40.0)).report("wheel 40px (frame)");
    sample(&mut harness, 400, |h| h.scroll(-40.0)).report("wheel -40px (frame)");

    let mut harness = Harness::new(document);
    for _ in 0..40 {
        harness.press_named(key::Named::ArrowDown);
    }
    harness.press_named(key::Named::End);
    let (line, _) = harness.editor.cursor_position();
    println!(
        "  (typing at the end of line {line}, {} bytes long, in a {}-line region)",
        harness.editor.lines[line].len(),
        harness.editor.active_end - harness.editor.active_start
    );
    sample(&mut harness, 200, |h| h.type_text("a")).report("typing (frame)");
    sample(&mut harness, 50, |h| h.press_named(key::Named::Enter)).report("enter (frame)");
    // A message that changes nothing: what re-laying out the window costs
    // on its own.
    sample(&mut harness, 200, |h| h.send(Message::DragEnd)).report("relayout (no change)");
}

/// Assert that the caret's row is on screen.
fn assert_caret_visible(harness: &mut Harness, context: &str) {
    let geometry = harness.geometry();
    let caret = harness.editor.caret_row_span();

    assert!(
        geometry.shows(caret),
        "{context}: caret on line {} (row {caret:?} of the editor) is off screen: {geometry:?}",
        harness.editor.cursor_position().0,
    );
}

/// The middle of the `index`th character of `rendered` — plain text at `size`
/// in `font`, laid out `width` wide — relative to where the text starts.
fn glyph_center(rendered: &str, index: usize, size: f32, font: Font, width: f32) -> Vector {
    use iced::advanced::text::{self, Paragraph as _};

    let paragraph = <iced::Renderer as text::Renderer>::Paragraph::with_text(text::Text {
        content: rendered,
        bounds: Size::new(width, f32::INFINITY),
        size: Pixels(size),
        line_height: text::LineHeight::default(),
        font,
        align_x: text::Alignment::Default,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::Advanced,
        wrapping: text::Wrapping::Word,
    });

    let at = paragraph
        .grapheme_position(0, index)
        .expect("the character");
    let next = paragraph
        .grapheme_position(0, index + 1)
        .expect("the next character");

    Vector::new(
        (at.x + next.x) / 2.0,
        at.y + size * super::LINE_HEIGHT / 2.0,
    )
}

/// Click the `index`th character of the rendered text of the segment that
/// starts at `line`, and return where the caret went.
fn click_rendered(
    harness: &mut Harness,
    line: usize,
    rendered: &str,
    index: usize,
) -> (usize, usize) {
    let origin = harness.text_origin(line);
    let width = harness.editor.measured_width - 2.0 * f32::from(super::LINE_PADDING);
    let center = glyph_center(rendered, index, 16.0, Font::DEFAULT, width);

    harness.click_at(origin + center);
    harness.editor.cursor_position()
}

/// Three hard-wrapped source lines render as one reflowed row. A click on
/// the word that came from the *third* source line must land there, not on
/// whichever line the click's height happens to divide the block into.
#[test]
fn a_click_lands_on_the_source_line_of_the_word_clicked() {
    let mut harness =
        Harness::new("intro\n\nfirst source line\nsecond source line\nthird source line");
    let rendered = "first source line second source line third source line";
    let third = rendered.find("third").unwrap();

    // The middle of "third"'s "i".
    let (line, column) = click_rendered(&mut harness, 2, rendered, third + 2);

    assert_eq!(line, 4, "the click was on the third source line's word");
    assert!(
        (2..=3).contains(&column),
        "column {column} is not at the clicked letter"
    );
}

/// Markup that is not drawn — the asterisks of bold, a link's address — is
/// stepped over: the caret lands on the letter clicked, in the source.
#[test]
fn a_click_after_markup_lands_on_the_letter_clicked() {
    let source = "Some **bold** words and a [link](https://example.com) here";
    let mut harness = Harness::new(&format!("intro\n\n{source}"));
    let rendered = "Some bold words and a link here";
    let here = rendered.find("here").unwrap();

    let (line, column) = click_rendered(&mut harness, 2, rendered, here + 1);

    let in_source = source.find("here").unwrap();
    assert_eq!(line, 2);
    assert!(
        (in_source..=in_source + 4).contains(&column),
        "column {column} is not within \"here\" at {in_source}"
    );
}

#[test]
fn a_click_on_a_heading_lands_past_its_hashes() {
    let mut harness = Harness::new("intro\n\n## Getting started");
    let rendered = "Getting started";

    // Headings are drawn at their level's size: 1.75× the body for `##`.
    let origin = harness.text_origin(2);
    let width = harness.editor.measured_width - 2.0 * f32::from(super::LINE_PADDING);
    let index = rendered.find("started").unwrap() + 3;
    let center = glyph_center(rendered, index, 16.0 * 1.75, Font::DEFAULT, width);
    harness.click_at(origin + center);

    let (line, column) = harness.editor.cursor_position();
    assert_eq!(line, 2);
    assert!(
        (14..=15).contains(&column),
        "column {column} is not at the clicked letter"
    );
}

/// A click in a code block makes the whole block raw, with the caret on the
/// line and at the letter clicked.
#[test]
fn a_click_in_code_lands_on_the_letter_clicked() {
    let mut harness = Harness::new("intro\n\n```rust\nlet answer = 42;\n```");
    let code = "let answer = 42;";

    // The code sits inside the block's padding: a quarter of the code size
    // around the box, and the code size inside it.
    let code_size = 16.0 * 0.75;
    let origin = harness.text_origin(2) + Vector::new(code_size * 1.25, code_size * 1.25);
    let center = glyph_center(code, 6, code_size, Font::MONOSPACE, f32::INFINITY);
    harness.click_at(origin + center);

    assert_eq!(
        (harness.editor.active_start, harness.editor.active_end),
        (2, 5),
        "the whole code block is raw"
    );
    let (line, column) = harness.editor.cursor_position();
    assert_eq!(line, 3);
    assert!(
        (6..=7).contains(&column),
        "column {column} is not at the clicked letter"
    );
}

/// A line too long for the window wraps; the arrows walk its rows before
/// leaving it — and entering it from below lands on its last row, not its
/// first.
#[test]
fn up_and_down_walk_the_rows_of_a_wrapped_line() {
    let long = "word ".repeat(60);
    let mut harness = Harness::new(&format!("{long}\nnext line"));

    harness.press_named(key::Named::ArrowDown);
    let (line, column) = harness.editor.cursor_position();
    assert_eq!(line, 0, "still on the wrapped line");
    assert!(column > 0, "on its second row");

    harness.press_named(key::Named::ArrowDown);
    harness.press_named(key::Named::ArrowDown);
    assert_eq!(harness.editor.cursor_position(), (1, 0), "off its last row");

    harness.press_named(key::Named::ArrowUp);
    let (line, column) = harness.editor.cursor_position();
    let rows = crate::wrap::rows(&long, harness.editor.wrap_capacity());
    assert!(
        rows.len() >= 3,
        "the line wraps to at least three rows: {rows:?}"
    );
    let last_row = rows[rows.len() - 1];
    assert_eq!(line, 0);
    assert!(
        column >= last_row,
        "entered on row starting at {last_row}, at {column}"
    );

    // The start of a row is also the end of the one above it; the caret must
    // be on the row it was put on, or the next step skips a row.
    harness.press_named(key::Named::ArrowUp);
    let (line, column) = harness.editor.cursor_position();
    let above = rows[rows.len() - 2];
    assert_eq!(line, 0);
    assert!(
        (above..last_row).contains(&column),
        "one row up is {above}..{last_row}, but the caret is at {column}"
    );
}

/// Opening and closing the panels above the document must not lose its place:
/// they are added to the window's column *before* the document, and a
/// positional column hands the document's widget state to the panel.
#[test]
fn opening_panels_keeps_the_document_where_it_was() {
    let mut harness = Harness::new(&sectioned_document(200));
    for _ in 0..40 {
        harness.scroll(40.0);
    }
    let scrolled = harness.geometry().offset.y;
    assert!(scrolled > 1_000.0, "scrolled down: {scrolled}");

    for (open, close) in [
        (Message::OpenSearch { replace: true }, Message::CloseSearch),
        (Message::ToggleSettingsPanel, Message::ToggleSettingsPanel),
        (Message::ToggleToolbar, Message::ToggleToolbar),
    ] {
        let label = format!("{open:?}");
        let before = harness.geometry().offset.y;
        harness.send(open);
        let opened = harness.geometry().offset.y;
        harness.send(close);
        let closed = harness.geometry().offset.y;

        // The content stays put on screen: the offset only changes by as much
        // as the view's top edge moved.
        assert!(
            (opened - before).abs() < 400.0,
            "{label}: opening moved the document from {before} to {opened}"
        );
        assert!(
            (closed - before).abs() < 1.0,
            "{label}: closing moved the document from {before} to {closed}"
        );
    }
}

/// A match found far down the document is scrolled to, with context above
/// it; a query that finds nothing leaves the view where it is.
#[test]
fn searching_scrolls_to_the_match_only_when_there_is_one() {
    let mut harness = Harness::new(&sectioned_document(200));
    harness.send(Message::OpenSearch { replace: false });

    harness.send(Message::SearchQueryChanged("Section 150".to_owned()));
    let geometry = harness.geometry();
    let (line, _) = harness.editor.cursor_position();
    assert_eq!(harness.editor.lines[line], "## Section 150");
    assert_caret_visible(&mut harness, "at the match");
    let caret = geometry.editor_on_screen().y - geometry.viewport.y;
    assert!(
        caret > geometry.viewport.height / 6.0,
        "the match has context above it: {caret}px down"
    );

    let before = harness.geometry().offset.y;
    harness.send(Message::SearchQueryChanged("no such text".to_owned()));
    assert_eq!(
        harness.geometry().offset.y,
        before,
        "nothing found, nothing moved"
    );
}

/// `PgDn` moves the view with the caret, so the caret stays where it was on
/// screen instead of jumping to the edge of the window.
#[test]
fn page_down_keeps_the_caret_at_its_height_on_screen() {
    let mut harness = Harness::new(&prose_document(40));
    for _ in 0..12 {
        harness.press_named(key::Named::ArrowDown);
    }
    let before = harness.geometry();

    harness.press_named(key::Named::PageDown);
    let after = harness.geometry();

    assert!(
        after.offset.y > before.offset.y + before.viewport.height / 2.0,
        "the view moved a page: {} → {}",
        before.offset.y,
        after.offset.y
    );
    let (was, is) = (before.editor_on_screen().y, after.editor_on_screen().y);
    assert!(
        (was - is).abs() < 1.0,
        "the caret moved on screen: {was} → {is}"
    );
    assert_caret_visible(&mut harness, "after PgDn");
}

/// Arrowing down past the bottom of the window must bring the caret along,
/// and arrowing back up past the top must too.
#[test]
fn arrowing_keeps_the_caret_on_screen() {
    let mut harness = Harness::new(&sectioned_document(40));

    for step in 0..120 {
        harness.press_named(key::Named::ArrowDown);
        assert_caret_visible(&mut harness, &format!("down, step {step}"));
    }
    for step in 0..120 {
        harness.press_named(key::Named::ArrowUp);
        assert_caret_visible(&mut harness, &format!("up, step {step}"));
    }
}

/// Scrolling by a constant step must move the content by exactly that step:
/// any other movement is the page jumping under the reader.
#[test]
fn scrolling_moves_the_content_by_exactly_the_scrolled_distance() {
    let document = sectioned_document(400);
    let mut harness = Harness::new(&document);

    // Park the caret deep in the document, where it is rendered for real
    // with windowed segments above it, and bring it on screen: it is the
    // probe whose on-screen position is followed.
    harness.editor.activate_line(1200, 0);
    harness.editor.rebuild();
    let top = harness.editor.line_top(1200);
    harness.send(Message::Scrolled {
        offset_y: top,
        view_height: 830.0,
    });
    harness.run_task(iced::widget::operation::scroll_to(
        DOCUMENT_ID,
        iced::widget::scrollable::AbsoluteOffset {
            x: 0.0,
            y: top - 700.0,
        },
    ));
    harness.settle(Vec::new());

    let mut previous = harness.geometry();
    let mut wrong = Vec::new();
    for step in 0..60 {
        harness.scroll(10.0);
        let geometry = harness.geometry();
        let moved = previous.editor_on_screen().y - geometry.editor_on_screen().y;
        if (moved - 10.0).abs() >= 0.5 {
            wrong.push((step, moved));
        }
        previous = geometry;
    }

    assert!(
        wrong.is_empty(),
        "{} of 60 steps moved the content by the wrong distance \
         (step, px moved for a 10px scroll): {wrong:?}",
        wrong.len()
    );
}

/// Which arrow-key steps are slow, and what was going on at them.
#[test]
#[ignore = "manual perf measurement: cargo test --release -- --ignored --nocapture"]
fn measure_slowest_navigation_steps() {
    warm_up();
    let mut harness = Harness::new(&sectioned_document(1000));

    let mut steps = Vec::new();
    for _ in 0..600 {
        harness.timings = Timings::default();
        let started = Instant::now();
        harness.press_named(key::Named::ArrowDown);
        let elapsed = started.elapsed();
        let region = (harness.editor.active_start, harness.editor.active_end);
        steps.push((elapsed, region, harness.timings));
    }

    let mut sorted: Vec<_> = steps.iter().map(|(elapsed, ..)| *elapsed).collect();
    sorted.sort();
    let p90 = sorted[sorted.len() * 9 / 10];

    for (elapsed, region, timings) in steps.iter().filter(|(elapsed, ..)| *elapsed > p90) {
        println!(
            "  {elapsed:>9.3?}  region {region:?}  frames {}  update {:?}  build {:?}  draw {:?}",
            timings.frames, timings.update, timings.build, timings.draw
        );
    }
}

/// What highlighting a fenced code block as Markdown costs the raw editor.
#[test]
#[ignore = "manual perf measurement: cargo test --release -- --ignored --nocapture"]
fn measure_raw_editor_highlighting() {
    use iced::advanced::text::Highlighter as _;

    let highlighter = |token: &str| {
        iced_highlighter::Highlighter::new(&iced_highlighter::Settings {
            theme: iced_highlighter::Theme::Base16Ocean,
            token: token.to_owned(),
        })
    };

    for (label, mut highlighter, lines) in [
        (
            "md: prose",
            highlighter("md"),
            &["Lorem ipsum dolor sit amet.", "", "## Heading"][..],
        ),
        (
            "md: ```rust",
            highlighter("md"),
            &["```rust", "fn main() { let _ = 1 + 1; }", "```"][..],
        ),
        (
            "md: ```",
            highlighter("md"),
            &["```", "fn main() { let _ = 1 + 1; }", "```"][..],
        ),
        (
            "rust: code",
            highlighter("rust"),
            &["fn main() { let _ = 1 + 1; }"][..],
        ),
    ] {
        let mut total = Duration::ZERO;
        for round in 0..51 {
            let started = Instant::now();
            highlighter.change_line(0);
            for line in lines {
                let _ = highlighter.highlight_line(line).count();
            }
            // The first round compiles whatever it meets for the first time.
            if round > 0 {
                total += started.elapsed();
            }
        }
        println!("  {label:<16} {:>9.3?} per block", total / 50);
    }

    // The raw editor's own highlighter, on the same fenced block: first with
    // code it has never seen (a different number each round), then with the
    // same code again, as when the caret comes back to a block.
    let mut ours = crate::highlighter::Highlighter::new(&crate::highlighter::Settings {
        theme: iced_highlighter::Theme::Base16Ocean,
    });
    for (label, fresh) in [("ours: new code", true), ("ours: seen code", false)] {
        let mut total = Duration::ZERO;
        for round in 0..51 {
            let code = if fresh {
                format!("fn main() {{ let _ = {round} + 1; }}")
            } else {
                "fn main() { let _ = 1 + 1; }".to_owned()
            };
            let started = Instant::now();
            ours.change_line(0);
            for line in ["```rust", &code, "```"] {
                let _ = ours.highlight_line(line).count();
            }
            if round > 0 {
                total += started.elapsed();
            }
        }
        println!("  {label:<16} {:>9.3?} per block", total / 50);
    }
}

/// What a keystroke costs in a long code block, which is edited — and so
/// re-highlighted from the edit down — as one region.
#[test]
#[ignore = "manual perf measurement: cargo test --release -- --ignored --nocapture"]
fn measure_typing_in_a_long_code_block() {
    warm_up();

    for lines in [20, 100, 300] {
        let code: String = (0..lines)
            .map(|i| {
                format!("    let value_{i} = compute(&input[{i}..], {i}u32).unwrap_or_default();\n")
            })
            .collect();
        let mut harness = Harness::new(&format!("intro\n\n```rust\n{code}```\n\nafter"));

        // Into the block, near its top: the worst place to edit it.
        harness.press_named(key::Named::ArrowDown);
        harness.press_named(key::Named::ArrowDown);
        harness.press_named(key::Named::ArrowDown);
        harness.press_named(key::Named::ArrowDown);
        assert_eq!(harness.editor.active_start, 2, "inside the code block");

        sample(&mut harness, 60, |h| h.type_text("x")).report(&format!("typing, {lines} lines"));
    }
}

/// Pay the syntax highlighter's one-off costs — loading its syntax sets,
/// compiling the languages the benchmark documents use — before measuring,
/// as the application does on a background thread at startup.
fn warm_up() {
    let mut harness = Harness::new(&sectioned_document(3));
    for _ in 0..24 {
        harness.press_named(key::Named::ArrowDown);
    }
}

/// Where a navigation `update` spends its time, piece by piece.
#[test]
#[ignore = "manual perf measurement: cargo test --release -- --ignored --nocapture"]
fn measure_navigation_update_pieces() {
    let mut harness = Harness::new(&sectioned_document(1000));
    // A prose line in the middle of the document: moving off it and back
    // moves the region across block boundaries.
    harness.editor.activate_line(4002, 0);
    harness.editor.rebuild();
    let editor = &mut harness.editor;

    let mut time = |label: &str, f: &mut dyn FnMut(&mut MarkdownEditor)| {
        let steps = 200;
        let started = Instant::now();
        for _ in 0..steps {
            f(editor);
        }
        println!("  {label:<24} {:>9.3?}", started.elapsed() / steps);
    };

    time("resegment", &mut |e| e.resegment());
    time("rebuild_blocks", &mut |e| e.rebuild_blocks());
    time("absorb_measurements", &mut |e| {
        e.measured_generation = 0;
        e.absorb_measurements();
    });
    time("refresh_offsets (full)", &mut |e| {
        e.mark_offsets_stale(0);
        e.refresh_offsets();
    });
    time("request_reveal", &mut |e| {
        e.request_reveal(RevealMode::Minimal)
    });
    time("fenced_blocks", &mut |e| {
        let _ = fences::blocks(&e.lines);
    });
    time("view", &mut |e| {
        let _ = e.view();
    });
    time("update(MoveDown/Up)", &mut |e| {
        let _ = e.update(Message::MoveDown);
        let _ = e.update(Message::MoveUp);
    });
}

#[test]
#[ignore = "manual perf measurement: cargo test --release -- --ignored --nocapture"]
fn measure_frame_costs() {
    warm_up();
    measure_interactions("sections", &sectioned_document(1000));
    measure_interactions("prose", &prose_document(300));
}

/// Collects the document scrollable's viewport and the active editor's bounds.
#[derive(Default)]
struct Probe {
    scrollable: Option<(Rectangle, Vector)>,
    editor: Option<Rectangle>,
}

impl Operation for Probe {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        _content_bounds: Rectangle,
        translation: Vector,
        _state: &mut dyn operation::Scrollable,
    ) {
        if id == Some(&Id::new(DOCUMENT_ID)) {
            self.scrollable = Some((bounds, translation));
        }
    }

    fn focusable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        _state: &mut dyn operation::Focusable,
    ) {
        if id == Some(&Id::new(ACTIVE_LINE_ID)) {
            self.editor = Some(bounds);
        }
    }
}
