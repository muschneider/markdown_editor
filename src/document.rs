//! The document view: a keyed column of segments inside a scrollable that
//! stays pinned to its content.
//!
//! The document is *windowed* — only the segments near the viewport become
//! widgets, and the rest are stood in for by spacers — and two things go wrong
//! when that is built from plain iced widgets. This module fixes both.
//!
//! * **Identity.** iced matches a column's children to last frame's widget
//!   state *by position*. When the window slides, or the caret crosses a blank
//!   line, every child after the change moves one index and inherits its
//!   neighbour's state — so every rendered paragraph in the window re-shapes
//!   its text, on nearly every scroll event and arrow key. [`Column`] matches
//!   children by [`Key`] instead: a block keeps its laid-out text for as long
//!   as it exists.
//!
//! * **Anchoring.** A spacer's height is only an estimate of the segments it
//!   stands for. Every time a segment moves between spacer and widget, the
//!   content below it shifts by the estimation error and the page jumps under
//!   the reader. [`Viewport`] pins the scroll position to the content — the
//!   line at the top of the view stays exactly where it was — and [`Column`]
//!   reports every segment's real height ([`Measurements`]) so the estimates
//!   converge on the truth.
//!
//! [`Viewport`] is also what keeps the caret in view: a [`Reveal`] is carried
//! out during layout, against the real geometry, in the frame it was asked for
//! — not a frame later from a guessed offset.
//!
//! [`Hit`] turns a press on a rendered segment into the *line* that was
//! pressed, rather than the segment as a whole.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::text::{self, Paragraph as _};
use iced::advanced::widget::{self, Operation, Tree, Widget, operation, tree};
use iced::advanced::{Clipboard, Shell, overlay};
use iced::alignment;
use iced::widget::scrollable::AbsoluteOffset;
use iced::{
    Alignment, Element, Event, Font, Length, Padding, Pixels, Rectangle, Renderer, Size, Theme,
    Vector, mouse, touch,
};

/// Anchor candidates kept per frame: the first few segments on screen, so
/// the view can still be pinned when the topmost one is re-rendered.
const MAX_ANCHORS: usize = 4;

/// How close (in screens) the view may come to a spacer above it before the
/// application is told where it is, so it can move the render window.
///
/// With the window reaching one screen above the view, it moves every half a
/// screen of scrolling up — and so lays out half a screen of new content at a
/// time, rather than all of it at once.
const NOTIFY_MARGIN_ABOVE: f32 = 0.5;

/// Like [`NOTIFY_MARGIN_ABOVE`], for a spacer below the view. The window
/// reaches further down (1.5 screens), so this is larger for it to move every
/// half a screen too.
const NOTIFY_MARGIN_BELOW: f32 = 1.0;

/// Viewport reports in a row, without the view moving, that the application
/// gets to fill a spacer still showing on screen. Each one lets it lay out the
/// lines it estimated wrongly; after a couple it has measured them all.
const MAX_SETTLING_REPORTS: u8 = 3;

/// Identity of one child of the document [`Column`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// A spacer standing in for off-screen segments; the index tells the
    /// (at most three) spacers of one frame apart.
    Gap(u8),
    /// The raw editor holding the active region.
    Editor,
    /// A rendered Markdown block, by the id it was parsed under.
    Block(u64),
    /// A blank line, by its line index.
    Blank(usize),
}

impl Key {
    /// Whether the child shows real content, rather than standing in for it.
    fn is_content(self) -> bool {
        !matches!(self, Key::Gap(_))
    }
}

/// One child of the [`Column`] as it was last laid out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placed {
    /// The child's identity.
    pub key: Key,
    /// Document lines the child stands for, `start..end`.
    pub lines: (usize, usize),
    /// Top edge, measured from the top of the document.
    pub top: f32,
    /// Height as laid out.
    pub height: f32,
}

/// What the [`Column`] measured the last time it was laid out.
///
/// Shared between the column that writes it, the [`Viewport`] that anchors
/// the view with it in the same frame, and the application, which folds the
/// real heights into its estimates on its next update.
#[derive(Debug, Default)]
pub struct Measurements {
    /// Incremented on every layout, so a reader can tell new measurements
    /// from ones it has already taken in.
    pub generation: u64,
    /// Width available to each child.
    pub width: f32,
    /// Advance of one monospace character at the editor's font size, as
    /// `(font size, advance)`: what the raw editor's text wraps by.
    pub monospace: Option<(f32, f32)>,
    /// Every child, in order.
    pub children: Vec<Placed>,
}

/// A vertical stack of document segments, reconciled by [`Key`].
///
/// Laid out like an iced `Column` of `Fill`-width children, optionally capped
/// at a maximum width and centred. Every layout is recorded in the shared
/// [`Measurements`].
pub struct Column<'a, Message> {
    children: Vec<Element<'a, Message>>,
    slots: Vec<(Key, (usize, usize))>,
    spacing: f32,
    padding: f32,
    max_width: Option<f32>,
    monospace_size: f32,
    measurements: &'a RefCell<Measurements>,
}

impl<'a, Message> Column<'a, Message> {
    /// An empty column that reports its layout into `measurements`.
    pub fn new(measurements: &'a RefCell<Measurements>) -> Self {
        Self {
            children: Vec::new(),
            slots: Vec::new(),
            spacing: 0.0,
            padding: 0.0,
            max_width: None,
            monospace_size: 16.0,
            measurements,
        }
    }

    /// Vertical gap between two children.
    pub fn spacing(mut self, spacing: f32) -> Self {
        self.spacing = spacing;
        self
    }

    /// Padding on every side of the column.
    pub fn padding(mut self, padding: f32) -> Self {
        self.padding = padding;
        self
    }

    /// Cap the children's width, centring them in whatever is left over.
    pub fn max_width(mut self, max_width: Option<f32>) -> Self {
        self.max_width = max_width;
        self
    }

    /// Font size of the raw editor, whose monospace advance is measured.
    pub fn monospace(mut self, size: f32) -> Self {
        self.monospace_size = size;
        self
    }

    /// Append a child standing for document `lines`.
    ///
    /// Keys must be unique within a column: a repeated key would hand two
    /// children the same widget state.
    pub fn push(
        mut self,
        key: Key,
        lines: (usize, usize),
        child: impl Into<Element<'a, Message>>,
    ) -> Self {
        self.slots.push((key, lines));
        self.children.push(child.into());
        self
    }

    fn keys(&self) -> impl Iterator<Item = Key> + '_ {
        self.slots.iter().map(|(key, _)| *key)
    }

    /// Record the children's positions (and the editor's character advance)
    /// for the [`Viewport`] and the application to read.
    fn record(&self, node: &layout::Node, width: f32) {
        let mut measurements = self.measurements.borrow_mut();

        measurements.generation += 1;
        measurements.width = width;
        measurements.children.clear();
        measurements
            .children
            .extend(
                self.slots
                    .iter()
                    .zip(node.children())
                    .map(|(&(key, lines), child)| {
                        let bounds = child.bounds();
                        Placed {
                            key,
                            lines,
                            top: bounds.y,
                            height: bounds.height,
                        }
                    }),
            );

        let size = self.monospace_size;
        if measurements
            .monospace
            .is_none_or(|(measured, _)| measured != size)
        {
            measurements.monospace = Some((size, monospace_advance(size)));
        }
    }
}

/// Width of one character of the monospace font at `size`.
fn monospace_advance(size: f32) -> f32 {
    const SAMPLE: &str = "00000000000000000000000000000000";

    let paragraph = <Renderer as text::Renderer>::Paragraph::with_text(text::Text {
        content: SAMPLE,
        bounds: Size::INFINITE,
        size: Pixels(size),
        line_height: text::LineHeight::default(),
        font: Font::MONOSPACE,
        align_x: text::Alignment::Default,
        align_y: alignment::Vertical::Top,
        shaping: text::Shaping::Basic,
        wrapping: text::Wrapping::None,
    });

    paragraph.min_width() / SAMPLE.len() as f32
}

/// The keys the column's children had last frame, in order.
struct Keys(Vec<Key>);

impl<Message> Widget<Message, Theme, Renderer> for Column<'_, Message> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Keys>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Keys(self.keys().collect()))
    }

    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    /// Hand each child the state of the child that had its key last frame.
    ///
    /// A plain positional diff would give every child after an insertion or
    /// removal its neighbour's state; for text, that means shaping it again.
    fn diff(&self, tree: &mut Tree) {
        let unchanged = tree
            .state
            .downcast_ref::<Keys>()
            .0
            .iter()
            .copied()
            .eq(self.keys());

        if unchanged {
            tree.diff_children(&self.children);
            return;
        }

        debug_assert_eq!(
            self.keys().collect::<std::collections::HashSet<_>>().len(),
            self.slots.len(),
            "every child of the document needs a key of its own"
        );

        let Keys(keys) = tree.state.downcast_mut::<Keys>();
        let mut previous: HashMap<Key, Tree> = keys
            .drain(..)
            .zip(std::mem::take(&mut tree.children))
            .collect();

        tree.children = self
            .children
            .iter()
            .zip(&self.slots)
            .map(|(child, (key, _))| match previous.remove(key) {
                Some(mut state) => {
                    state.diff(child);
                    state
                }
                None => Tree::new(child),
            })
            .collect();

        keys.extend(self.keys());
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let available = (limits.max().width - 2.0 * self.padding).max(0.0);
        let width = match self.max_width {
            Some(max_width) if available.is_finite() => max_width.min(available),
            _ => available,
        };
        let inset = if available.is_finite() {
            (available - width) / 2.0
        } else {
            0.0
        };

        let node = layout::flex::resolve(
            layout::flex::Axis::Vertical,
            renderer,
            limits,
            Length::Fill,
            Length::Shrink,
            Padding {
                top: self.padding,
                bottom: self.padding,
                left: self.padding + inset,
                right: self.padding + inset,
            },
            self.spacing,
            Alignment::Start,
            &mut self.children,
            &mut tree.children,
        );

        self.record(&node, width);
        node
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
                .for_each(|((child, state), layout)| {
                    child
                        .as_widget_mut()
                        .operate(state, layout, renderer, operation);
                });
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        for ((child, state), layout) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget_mut().update(
                state, event, layout, cursor, renderer, clipboard, shell, viewport,
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, state), layout)| {
                child
                    .as_widget()
                    .mouse_interaction(state, layout, cursor, viewport, renderer)
            })
            .max()
            .unwrap_or_default()
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        for ((child, state), layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .filter(|(_, layout)| layout.bounds().intersects(viewport))
        {
            child
                .as_widget()
                .draw(state, renderer, theme, style, layout, cursor, viewport);
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message: 'a> From<Column<'a, Message>> for Element<'a, Message> {
    fn from(column: Column<'a, Message>) -> Self {
        Element::new(column)
    }
}

/// A request to bring the caret into view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reveal {
    /// Tells a new request from one already carried out: each generation is
    /// honoured once, so the user can scroll away from the caret afterwards.
    pub generation: u64,
    /// The part of the editor to show, as distances from its top edge.
    pub span: (f32, f32),
    /// How far to scroll.
    pub mode: RevealMode,
}

/// How far a [`Reveal`] scrolls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealMode {
    /// As little as it takes, and not at all when the span is in view.
    Minimal,
    /// Not at all when the span is in view; otherwise place it a third of the
    /// way down, with context above it (search results).
    Center,
    /// Keep the editor at the height on screen it had before it moved, the
    /// way paging does; then as `Minimal`, should that leave it off screen.
    Keep,
}

impl Reveal {
    /// The scroll offset that carries out this reveal for `editor`, starting
    /// from offset `from` in a viewport `height` tall.
    ///
    /// `was_at` is where the editor's top edge sat relative to the viewport's
    /// the last time it was laid out.
    fn offset(self, editor: &Placed, from: f32, height: f32, was_at: Option<f32>) -> f32 {
        let top = editor.top + self.span.0;
        let bottom = editor.top + self.span.1;

        let minimal = |from: f32| {
            if top < from || bottom - top > height {
                top
            } else if bottom > from + height {
                bottom - height
            } else {
                from
            }
        };

        match self.mode {
            RevealMode::Minimal => minimal(from),
            RevealMode::Center if top >= from && bottom <= from + height => from,
            RevealMode::Center => top - height / 3.0,
            RevealMode::Keep => minimal(was_at.map_or(from, |was_at| editor.top - was_at)),
        }
    }
}

/// A scrollable that stays pinned to the document content it shows.
///
/// Wraps the document's `scrollable`, whose content must be a [`Column`]
/// reporting into the same [`Measurements`], at the top of the scrollable
/// content. After every layout, the view is moved so the line that was at
/// its top edge still is — then a pending [`Reveal`] is carried out.
///
/// It also decides when the application hears about scrolling. A plain
/// `scrollable` reports every change of position, and each report costs a
/// full update, view and layout. Only the render window depends on the
/// position, and it only needs moving when the view nears the end of what
/// is rendered — so that is the only time [`Viewport::on_scroll`] is called.
/// Every other scroll step is just a redraw.
pub struct Viewport<'a, Message> {
    scrollable: Element<'a, Message>,
    measurements: &'a RefCell<Measurements>,
    reveal: Option<Reveal>,
    on_scroll: Option<Box<dyn Fn(f32, f32) -> Message + 'a>>,
}

impl<'a, Message> Viewport<'a, Message> {
    /// Pin `scrollable` to the content its [`Column`] reports.
    pub fn new(
        scrollable: impl Into<Element<'a, Message>>,
        measurements: &'a RefCell<Measurements>,
    ) -> Self {
        Self {
            scrollable: scrollable.into(),
            measurements,
            reveal: None,
            on_scroll: None,
        }
    }

    /// Bring the caret into view, unless this request was already honoured.
    pub fn reveal(mut self, reveal: Option<Reveal>) -> Self {
        self.reveal = reveal;
        self
    }

    /// Report the scroll offset and viewport height whenever the view comes
    /// near the edge of the rendered content — and when it is first laid out,
    /// or resized.
    pub fn on_scroll(mut self, on_scroll: impl Fn(f32, f32) -> Message + 'a) -> Self {
        self.on_scroll = Some(Box::new(on_scroll));
        self
    }

    /// Take new anchors if something other than a layout — a scroll event or
    /// a scroll operation — moved the view.
    fn follow(
        &mut self,
        pin: &mut Pin,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
    ) -> Option<Scroll> {
        let scroll = scroll(&mut self.scrollable, tree, layout, renderer, None)?;

        if (scroll.offset - pin.offset).abs() > f32::EPSILON || scroll.height != pin.height {
            pin.capture(&self.measurements.borrow().children, scroll);
        }

        Some(scroll)
    }
}

/// Where a [`Viewport`] is pinned.
#[derive(Debug, Default)]
struct Pin {
    /// Candidate anchors, best first: a document line, and how far below the
    /// top of that line the viewport's top edge sits.
    anchors: Vec<(usize, f32)>,
    /// Scroll offset the anchors were taken at.
    offset: f32,
    /// Viewport height the anchors were taken at.
    height: f32,
    /// Generation of the last [`Reveal`] carried out.
    revealed: u64,
    /// Where the editor's top edge was relative to the viewport's top edge.
    editor: Option<f32>,
    /// The offset and height last reported to the application.
    reported: Option<(f32, f32)>,
    /// Generation of the measurements the last report was made against.
    reported_generation: u64,
    /// Reports made in a row without the view moving.
    settling: u8,
}

impl Pin {
    /// Whether the application must hear where the view is now, to keep
    /// rendered content under it.
    fn should_report(&mut self, scroll: Scroll, measurements: &Measurements) -> bool {
        let Some((offset, height)) = self.reported else {
            return true;
        };
        if height != scroll.height {
            return true;
        }

        let children = &measurements.children;
        let moved = (offset - scroll.offset).abs() > 0.5;

        if moved {
            self.settling = 0;
            return shows_spacer(
                children,
                scroll.offset - scroll.height * NOTIFY_MARGIN_ABOVE,
                scroll.offset + scroll.height * (1.0 + NOTIFY_MARGIN_BELOW),
            );
        }

        // The view is still, yet a spacer is on screen: the window was placed
        // by estimates the last layout has since corrected. Give the
        // application a few more layouts' worth of measurements to fix it.
        let fresh = measurements.generation != self.reported_generation;
        if fresh
            && self.settling < MAX_SETTLING_REPORTS
            && shows_spacer(children, scroll.offset, scroll.offset + scroll.height)
        {
            self.settling += 1;
            return true;
        }

        false
    }

    /// Remember what is on screen at `scroll`, for the next layout to keep
    /// in place.
    fn capture(&mut self, children: &[Placed], scroll: Scroll) {
        self.offset = scroll.offset;
        self.height = scroll.height;
        self.anchors.clear();
        self.anchors.extend(
            children
                .iter()
                .filter(|child| {
                    child.key.is_content()
                        && child.top + child.height > scroll.offset
                        && child.top < scroll.offset + scroll.height
                })
                .take(MAX_ANCHORS)
                .map(|child| (child.lines.0, scroll.offset - child.top)),
        );
        self.editor = children
            .iter()
            .find(|child| child.key == Key::Editor)
            .map(|editor| editor.top - scroll.offset);
    }

    /// The offset that puts the anchored content back where it was.
    fn anchored(&self, children: &[Placed]) -> Option<f32> {
        self.anchors
            .iter()
            .find_map(|&(line, delta)| line_top(children, line).map(|top| top + delta))
    }
}

/// Whether any spacer (with any height to it) reaches into `top..bottom`.
fn shows_spacer(children: &[Placed], top: f32, bottom: f32) -> bool {
    children.iter().any(|child| {
        !child.key.is_content()
            && child.height > 0.0
            && child.top < bottom
            && child.top + child.height > top
    })
}

/// Top edge of document `line`, located in the child that renders it — or
/// `None` when no child does.
///
/// A line in the middle of a multi-line child is placed in proportion to its
/// position within it: the child's own layout is not visible from here.
fn line_top(children: &[Placed], line: usize) -> Option<f32> {
    children
        .iter()
        .filter(|child| child.key.is_content())
        .find(|child| (child.lines.0..child.lines.1).contains(&line))
        .map(|child| {
            let span = (child.lines.1 - child.lines.0) as f32;
            child.top + child.height * (line - child.lines.0) as f32 / span
        })
}

impl<Message> Widget<Message, Theme, Renderer> for Viewport<'_, Message> {
    fn size(&self) -> Size<Length> {
        self.scrollable.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.scrollable.as_widget().size_hint()
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Pin>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Pin::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.scrollable)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.scrollable));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let Tree {
            state, children, ..
        } = tree;
        let pin = state.downcast_mut::<Pin>();
        let content = &mut children[0];

        let node = self
            .scrollable
            .as_widget_mut()
            .layout(content, renderer, limits);

        let Some(scroll) = scroll(
            &mut self.scrollable,
            content,
            Layout::new(&node),
            renderer,
            None,
        ) else {
            return node;
        };

        let measurements = self.measurements.borrow();
        let children = measurements.children.as_slice();

        // Put back whatever was at the top of the view...
        let mut target = pin.anchored(children);

        // ...unless the caret has asked to be shown. This is measured against
        // the editor's real position, in this very layout.
        if let Some(reveal) = self
            .reveal
            .filter(|reveal| reveal.generation != pin.revealed)
        {
            pin.revealed = reveal.generation;

            if let Some(editor) = children.iter().find(|child| child.key == Key::Editor) {
                let from = target.unwrap_or(scroll.offset);
                target = Some(reveal.offset(editor, from, scroll.height, pin.editor));
            }
        }

        let scroll = match target.map(|offset| scroll.clamp(offset)) {
            Some(offset) if (offset - scroll.offset).abs() > f32::EPSILON => {
                let _ = self::scroll(
                    &mut self.scrollable,
                    content,
                    Layout::new(&node),
                    renderer,
                    Some(offset),
                );
                Scroll { offset, ..scroll }
            }
            _ => scroll,
        };

        pin.capture(children, scroll);
        node
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        let Tree {
            state, children, ..
        } = tree;

        self.scrollable
            .as_widget_mut()
            .operate(&mut children[0], layout, renderer, operation);

        let _ = self.follow(
            state.downcast_mut::<Pin>(),
            &mut children[0],
            layout,
            renderer,
        );
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let Tree {
            state, children, ..
        } = tree;
        let pin = state.downcast_mut::<Pin>();

        self.scrollable.as_widget_mut().update(
            &mut children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        let Some(scroll) = self.follow(pin, &mut children[0], layout, renderer) else {
            return;
        };

        if let Some(on_scroll) = &self.on_scroll {
            let measurements = self.measurements.borrow();

            if pin.should_report(scroll, &measurements) {
                pin.reported = Some((scroll.offset, scroll.height));
                pin.reported_generation = measurements.generation;
                shell.publish(on_scroll(scroll.offset, scroll.height));
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.scrollable.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.scrollable.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.scrollable.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message: 'a> From<Viewport<'a, Message>> for Element<'a, Message> {
    fn from(viewport: Viewport<'a, Message>) -> Self {
        Element::new(viewport)
    }
}

/// Scroll position of a scrollable.
#[derive(Debug, Clone, Copy)]
struct Scroll {
    /// Distance scrolled from the top of the content.
    offset: f32,
    /// Height of the visible part.
    height: f32,
    /// Height of the whole content.
    content: f32,
}

impl Scroll {
    /// `offset`, limited to the offsets the content allows.
    fn clamp(self, offset: f32) -> f32 {
        offset.clamp(0.0, (self.content - self.height).max(0.0))
    }
}

/// Read the scroll position of `scrollable` — and first move it to `target`,
/// when given.
///
/// Operations are the only access iced gives to a scrollable's position, and
/// nothing stops a parent from running one on its child in the middle of its
/// own layout: that is what lets the [`Viewport`] correct the position before
/// the frame is drawn, rather than a frame later.
fn scroll<Message>(
    scrollable: &mut Element<'_, Message>,
    tree: &mut Tree,
    layout: Layout<'_>,
    renderer: &Renderer,
    target: Option<f32>,
) -> Option<Scroll> {
    let mut probe = ScrollProbe {
        target,
        found: None,
    };
    scrollable
        .as_widget_mut()
        .operate(tree, layout, renderer, &mut probe);
    probe.found
}

/// The operation behind [`scroll`].
struct ScrollProbe {
    target: Option<f32>,
    found: Option<Scroll>,
}

impl Operation for ScrollProbe {
    fn traverse(&mut self, _operate: &mut dyn FnMut(&mut dyn Operation)) {
        // Never descend: the document holds scrollables of its own (code
        // blocks, tables), and only the outermost one is the view.
    }

    fn scrollable(
        &mut self,
        _id: Option<&widget::Id>,
        bounds: Rectangle,
        content_bounds: Rectangle,
        translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if self.found.is_some() {
            return;
        }

        if let Some(offset) = self.target {
            state.scroll_to(AbsoluteOffset {
                x: None,
                y: Some(offset),
            });
        }

        self.found = Some(Scroll {
            offset: self.target.unwrap_or(translation.y),
            height: bounds.height,
            content: content_bounds.height,
        });
    }
}

/// Where a press landed on rendered text: which of its segment's pieces of
/// text — numbered in drawing order — and the byte offset into that piece's
/// rendered text.
///
/// Recorded by the text itself (see `markdown_view::Located`) and read by the
/// [`Hit`] around it, through a [`TextHits`] slot both are given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextHit {
    /// The piece of text, in drawing order within its segment.
    pub item: usize,
    /// The byte offset into its rendered text.
    pub offset: usize,
}

/// Where rendered text records a press on it, for the [`Hit`] around it to
/// pick up in the same event.
pub type TextHits = Cell<Option<TextHit>>;

/// Where a rendered segment was pressed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Press {
    /// The document lines the pressed segment covers, `start..end`.
    pub segment: (usize, usize),
    /// The document line under the pointer, judged by height alone.
    pub line: usize,
    /// Horizontal distance from the segment's left edge.
    pub x: f32,
    /// How far down that line's share of the segment the pointer was, from
    /// `0.0` (its top) up to `1.0` (its bottom).
    pub fraction: f32,
    /// The character under the pointer, when it was over rendered text.
    pub text: Option<TextHit>,
}

/// Makes a rendered segment respond to the pointer line by line: a press
/// reports the line (and position) under the pointer, and a selection drag
/// reports every line the pointer moves onto.
///
/// Rendered Markdown keeps no mapping from a point back to its source, so the
/// segment's height is divided among its lines in proportion to `weights` —
/// the lines' heights as the application measured or estimated them.
pub struct Hit<'a, Message> {
    content: Element<'a, Message>,
    first_line: usize,
    weights: &'a [f32],
    text_hits: &'a TextHits,
    on_press: Box<dyn Fn(Press) -> Message + 'a>,
    on_drag: Option<Box<dyn Fn(usize) -> Message + 'a>>,
}

impl<'a, Message> Hit<'a, Message> {
    /// Wrap `content`, which renders the lines starting at `first_line` whose
    /// relative heights are `weights`, and whose text records presses in
    /// `text_hits`.
    pub fn new(
        content: impl Into<Element<'a, Message>>,
        first_line: usize,
        weights: &'a [f32],
        text_hits: &'a TextHits,
        on_press: impl Fn(Press) -> Message + 'a,
    ) -> Self {
        Self {
            content: content.into(),
            first_line,
            weights,
            text_hits,
            on_press: Box::new(on_press),
            on_drag: None,
        }
    }

    /// Report the lines a selection drag moves onto, while one is under way.
    ///
    /// Only wired while dragging: pointer movement over the document is
    /// otherwise not worth a message.
    pub fn on_drag_maybe(mut self, on_drag: Option<impl Fn(usize) -> Message + 'a>) -> Self {
        self.on_drag = on_drag.map(|on_drag| Box::new(on_drag) as _);
        self
    }
}

/// The line of a [`Hit`] under the pointer during a drag, so moving within a
/// line does not report it again.
#[derive(Default)]
struct Hovered(Option<usize>);

impl<Message> Widget<Message, Theme, Renderer> for Hit<'_, Message> {
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Hovered>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Hovered::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        let pressed = matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
                | Event::Touch(touch::Event::FingerPressed { .. })
        );
        // Whatever text is under the pointer records the press here while
        // handling it, just below.
        if pressed {
            self.text_hits.set(None);
        }

        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        // A link inside the segment took the press.
        if shell.is_event_captured() {
            return;
        }

        let bounds = layout.bounds();
        let Hovered(hovered) = tree.state.downcast_mut::<Hovered>();

        match event {
            _ if pressed => {
                let Some(point) = cursor.position_in(bounds) else {
                    return;
                };

                let (index, fraction) = locate(self.weights, bounds.height, point.y);
                *hovered = Some(self.first_line + index);

                shell.publish((self.on_press)(Press {
                    segment: (self.first_line, self.first_line + self.weights.len().max(1)),
                    line: self.first_line + index,
                    x: point.x,
                    fraction,
                    text: self.text_hits.take(),
                }));
                shell.capture_event();
            }
            Event::Mouse(mouse::Event::CursorMoved { .. })
            | Event::Touch(touch::Event::FingerMoved { .. }) => {
                let Some(on_drag) = &self.on_drag else {
                    return;
                };

                // Forgetting the line on leaving is what makes coming back
                // onto it report it again.
                let Some(point) = cursor.position_in(bounds) else {
                    *hovered = None;
                    return;
                };

                let line = self.first_line + locate(self.weights, bounds.height, point.y).0;
                if *hovered != Some(line) {
                    *hovered = Some(line);
                    shell.publish(on_drag(line));
                }
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        match self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        ) {
            // Over text that is not a link: the I-beam of a text editor.
            mouse::Interaction::None if cursor.is_over(layout.bounds()) => mouse::Interaction::Text,
            interaction => interaction,
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message: 'a> From<Hit<'a, Message>> for Element<'a, Message> {
    fn from(hit: Hit<'a, Message>) -> Self {
        Element::new(hit)
    }
}

/// Which of the lines weighted `weights` a point `y` pixels down a segment
/// `height` tall falls on, and how far down that line's share it is.
fn locate(weights: &[f32], height: f32, y: f32) -> (usize, f32) {
    let total: f32 = weights.iter().sum();
    let fraction = |from: f32, to: f32| {
        if to > from {
            ((y - from) / (to - from)).clamp(0.0, 1.0)
        } else {
            0.0
        }
    };

    if weights.len() <= 1 || total <= 0.0 {
        return (0, fraction(0.0, height));
    }

    let scale = height / total;
    let mut top = 0.0;

    for (index, weight) in weights.iter().enumerate() {
        let bottom = top + weight * scale;
        if y < bottom || index + 1 == weights.len() {
            return (index, fraction(top, bottom));
        }
        top = bottom;
    }

    unreachable!("the last line always matches")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(key: Key, lines: (usize, usize), top: f32, height: f32) -> Placed {
        Placed {
            key,
            lines,
            top,
            height,
        }
    }

    #[test]
    fn a_point_falls_on_the_line_whose_share_holds_it() {
        // Three lines, the middle one twice as tall, in a 400 px segment.
        let weights = [1.0, 2.0, 1.0];

        assert_eq!(locate(&weights, 400.0, 0.0), (0, 0.0));
        assert_eq!(locate(&weights, 400.0, 50.0), (0, 0.5));
        assert_eq!(locate(&weights, 400.0, 100.0), (1, 0.0));
        assert_eq!(locate(&weights, 400.0, 250.0), (1, 0.75));
        assert_eq!(locate(&weights, 400.0, 399.0).0, 2);
        // Past the bottom edge still lands on the last line.
        assert_eq!(locate(&weights, 400.0, 500.0), (2, 1.0));
    }

    #[test]
    fn a_single_line_takes_the_whole_segment() {
        assert_eq!(locate(&[30.0], 60.0, 15.0), (0, 0.25));
        assert_eq!(locate(&[], 60.0, 30.0), (0, 0.5));
    }

    #[test]
    fn a_line_is_found_in_whatever_child_renders_it() {
        let children = [
            placed(Key::Gap(0), (0, 10), 0.0, 300.0),
            placed(Key::Block(7), (10, 14), 301.0, 100.0),
            placed(Key::Editor, (14, 15), 402.0, 30.0),
        ];

        assert_eq!(line_top(&children, 10), Some(301.0));
        // Proportionally within a multi-line child.
        assert_eq!(line_top(&children, 12), Some(351.0));
        assert_eq!(line_top(&children, 14), Some(402.0));
        // Spacers only stand in for their lines; nothing to anchor to.
        assert_eq!(line_top(&children, 3), None);
        assert_eq!(line_top(&children, 15), None);
    }

    #[test]
    fn anchors_put_the_top_line_back_where_it_was() {
        let before = [
            placed(Key::Gap(0), (0, 10), 0.0, 300.0),
            placed(Key::Block(1), (10, 12), 301.0, 60.0),
            placed(Key::Block(2), (12, 20), 362.0, 240.0),
        ];

        let mut pin = Pin::default();
        pin.capture(
            &before,
            Scroll {
                offset: 331.0,
                height: 200.0,
                content: 5_000.0,
            },
        );

        // The spacer above turned out 45 px taller than estimated.
        let after = [
            placed(Key::Gap(0), (0, 10), 0.0, 345.0),
            placed(Key::Block(1), (10, 12), 346.0, 60.0),
            placed(Key::Block(2), (12, 20), 407.0, 240.0),
        ];

        assert_eq!(pin.anchored(&after), Some(376.0));
    }

    #[test]
    fn a_re_rendered_anchor_falls_back_to_the_next_one() {
        let before = [
            placed(Key::Block(1), (0, 4), 12.0, 120.0),
            placed(Key::Block(2), (4, 8), 133.0, 120.0),
        ];

        let mut pin = Pin::default();
        pin.capture(
            &before,
            Scroll {
                offset: 40.0,
                height: 600.0,
                content: 5_000.0,
            },
        );

        // Block 1 is gone: its lines went elsewhere, and line 0 is no longer
        // rendered by anything; line 4 still starts block 2.
        let after = [placed(Key::Block(2), (4, 8), 150.0, 120.0)];
        assert_eq!(pin.anchored(&after), Some(150.0 - 93.0));
    }

    fn editor_at(top: f32) -> Placed {
        placed(Key::Editor, (50, 51), top, 30.0)
    }

    fn reveal(mode: RevealMode) -> Reveal {
        Reveal {
            generation: 1,
            span: (0.0, 30.0),
            mode,
        }
    }

    #[test]
    fn a_minimal_reveal_scrolls_only_as_far_as_needed() {
        let minimal = reveal(RevealMode::Minimal);

        // In view: stays put.
        assert_eq!(minimal.offset(&editor_at(500.0), 400.0, 300.0, None), 400.0);
        // Below: bottom edge to bottom edge.
        assert_eq!(minimal.offset(&editor_at(800.0), 400.0, 300.0, None), 530.0);
        // Above: top edge to top edge.
        assert_eq!(minimal.offset(&editor_at(100.0), 400.0, 300.0, None), 100.0);
    }

    #[test]
    fn a_centred_reveal_leaves_context_above_the_caret() {
        let center = reveal(RevealMode::Center);

        assert_eq!(center.offset(&editor_at(500.0), 400.0, 300.0, None), 400.0);
        assert_eq!(
            center.offset(&editor_at(2_000.0), 400.0, 300.0, None),
            1_900.0
        );
    }

    #[test]
    fn paging_keeps_the_editor_at_its_height_on_screen() {
        let keep = reveal(RevealMode::Keep);

        // The editor was 120 px below the top of the view before it moved.
        assert_eq!(
            keep.offset(&editor_at(2_000.0), 400.0, 300.0, Some(120.0)),
            1_880.0
        );
        // It was off screen: it is brought in rather than kept off screen.
        assert_eq!(
            keep.offset(&editor_at(2_000.0), 400.0, 300.0, Some(900.0)),
            1_730.0
        );
    }
}
