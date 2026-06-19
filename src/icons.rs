//! SVG icons for the toolbar.
//!
//! Each icon is a small, monochrome [Lucide](https://lucide.dev)-style glyph
//! embedded as SVG markup and exposed as a cached [`Handle`]. The stroke colour
//! baked into the markup is irrelevant: icons are re-tinted at render time by
//! [`crate::theme::toolbar_icon`], so a single set of glyphs follows the theme.

use std::sync::LazyLock;

use iced::widget::svg::Handle;

/// Declare a lazily-built, cached icon handle behind a public accessor.
///
/// The inner markup is wrapped in a shared 24×24 stroked `<svg>` document so
/// every icon has a consistent weight and viewBox.
macro_rules! icon {
    ($(#[$meta:meta])* $name:ident = $inner:literal) => {
        $(#[$meta])*
        pub(crate) fn $name() -> Handle {
            static HANDLE: LazyLock<Handle> = LazyLock::new(|| {
                Handle::from_memory(
                    concat!(
                        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 24 24\" ",
                        "fill=\"none\" stroke=\"black\" stroke-width=\"2\" ",
                        "stroke-linecap=\"round\" stroke-linejoin=\"round\">",
                        $inner,
                        "</svg>",
                    )
                    .as_bytes(),
                )
            });
            HANDLE.clone()
        }
    };
}

icon!(
    /// Create a new, blank file.
    new_file = "<path d=\"M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z\"/>\
                <polyline points=\"14 2 14 8 20 8\"/><line x1=\"12\" y1=\"18\" x2=\"12\" y2=\"12\"/>\
                <line x1=\"9\" y1=\"15\" x2=\"15\" y2=\"15\"/>"
);

icon!(
    /// Open an existing file (folder).
    open_file = "<path d=\"M4 20h16a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.93a2 2 0 0 1-1.66-.9l-.82-1.2\
                 A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z\"/>"
);

icon!(
    /// Save the current file (floppy disk).
    save_file = "<path d=\"M19 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h11l5 5v11a2 2 0 0 1-2 2z\"/>\
                 <polyline points=\"17 21 17 13 7 13 7 21\"/><polyline points=\"7 3 7 8 15 8\"/>"
);

icon!(
    /// Bold (`**text**`).
    bold = "<path d=\"M6 4h8a4 4 0 0 1 0 8H6z\"/><path d=\"M6 12h9a4 4 0 0 1 0 8H6z\"/>"
);

icon!(
    /// Italic (`*text*`).
    italic = "<line x1=\"19\" y1=\"4\" x2=\"10\" y2=\"4\"/><line x1=\"14\" y1=\"20\" x2=\"5\" y2=\"20\"/>\
              <line x1=\"15\" y1=\"4\" x2=\"9\" y2=\"20\"/>"
);

icon!(
    /// Inline code (`` `text` ``).
    code = "<polyline points=\"16 18 22 12 16 6\"/><polyline points=\"8 6 2 12 8 18\"/>"
);

icon!(
    /// Heading (`# text`).
    heading = "<path d=\"M6 12h12\"/><path d=\"M6 20V4\"/><path d=\"M18 20V4\"/>"
);

icon!(
    /// Bullet list (`- item`).
    bullet_list = "<line x1=\"8\" y1=\"6\" x2=\"21\" y2=\"6\"/><line x1=\"8\" y1=\"12\" x2=\"21\" y2=\"12\"/>\
                   <line x1=\"8\" y1=\"18\" x2=\"21\" y2=\"18\"/><path d=\"M3 6h.01\"/>\
                   <path d=\"M3 12h.01\"/><path d=\"M3 18h.01\"/>"
);

icon!(
    /// Numbered list (`1. item`).
    numbered_list = "<line x1=\"10\" y1=\"6\" x2=\"21\" y2=\"6\"/><line x1=\"10\" y1=\"12\" x2=\"21\" y2=\"12\"/>\
                     <line x1=\"10\" y1=\"18\" x2=\"21\" y2=\"18\"/><path d=\"M4 6h1v4\"/>\
                     <path d=\"M4 10h2\"/><path d=\"M6 18H4c0-1 2-2 2-3s-1-1.5-2-1\"/>"
);

icon!(
    /// Blockquote (`> text`).
    quote = "<path d=\"M5 5v14\"/><path d=\"M10 7h9\"/><path d=\"M10 12h9\"/><path d=\"M10 17h6\"/>"
);

icon!(
    /// Horizontal rule (`---`).
    horizontal_rule = "<line x1=\"4\" y1=\"12\" x2=\"20\" y2=\"12\"/>"
);

icon!(
    /// Link (`[text](url)`).
    link = "<path d=\"M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71\"/>\
            <path d=\"M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71\"/>"
);
