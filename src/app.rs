//! Core application state, message types, and update/view logic.

use std::path::PathBuf;
use std::sync::Arc;

use iced::highlighter;
use iced::keyboard;
use iced::widget::{column, container, markdown, row, scrollable, space, text, text_editor};
use iced::{Element, Fill, Font, Task, Theme, window};

use crate::file_ops::{self, FileError};
use crate::theme as app_theme;
use crate::toolbar::{self, FormatAction};

/// Default content shown when the editor starts with no file.
const WELCOME_MD: &str = r#"# Welcome to Markdown Editor

Start typing Markdown in this pane — the **preview** updates in real time.

## Features

- **Live preview** with side-by-side layout
- **Syntax highlighting** in the editor
- **File operations**: Open (`Ctrl+O`) and Save (`Ctrl+S`)
- **Formatting toolbar** for common Markdown elements
- **Keyboard shortcuts** for bold, italic, and more

## Shortcuts

| Action     | Shortcut   |
|------------|------------|
| Save       | `Ctrl+S`   |
| Open       | `Ctrl+O`   |
| New        | `Ctrl+N`   |
| Bold       | `Ctrl+B`   |
| Italic     | `Ctrl+I`   |

> Try editing this text to see the preview update!

```rust
fn main() {
    println!("Hello from Markdown Editor!");
}
```
"#;

/// Main application state.
pub struct MarkdownEditor {
    /// The raw text content in the editor widget.
    editor_content: text_editor::Content,
    /// Parsed Markdown content for the preview pane.
    preview_content: markdown::Content,
    /// Path to the currently open file, if any.
    current_file: Option<PathBuf>,
    /// Whether the document has unsaved changes.
    is_dirty: bool,
    /// Whether a file operation is in progress.
    is_loading: bool,
    /// The editor highlight theme.
    highlight_theme: highlighter::Theme,
}

/// All messages the application can handle.
#[derive(Debug, Clone)]
pub enum Message {
    /// A text editor action (typing, cursor movement, etc.).
    EditorAction(text_editor::Action),
    /// A link in the preview was clicked.
    LinkClicked(markdown::Uri),
    /// Request to create a new file.
    NewFile,
    /// Request to open a file.
    OpenFile,
    /// A file was loaded from disk.
    FileOpened(Result<(PathBuf, Arc<String>), FileError>),
    /// Request to save the current file.
    SaveFile,
    /// The file was saved to disk.
    FileSaved(Result<PathBuf, FileError>),
    /// Apply a formatting action from the toolbar.
    Format(FormatAction),
}

impl MarkdownEditor {
    /// Create a new editor instance with default content.
    pub fn new() -> (Self, Task<Message>) {
        (
            Self {
                editor_content: text_editor::Content::with_text(WELCOME_MD),
                preview_content: markdown::Content::parse(WELCOME_MD),
                current_file: None,
                is_dirty: false,
                is_loading: false,
                highlight_theme: highlighter::Theme::SolarizedDark,
            },
            Task::none(),
        )
    }

    /// Window title, showing the file name and dirty indicator.
    pub fn title(&self) -> String {
        let file_name = self
            .current_file
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("Untitled");

        let dirty = if self.is_dirty { " *" } else { "" };

        format!("{file_name}{dirty} — Markdown Editor")
    }

    /// Handle all state transitions.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::EditorAction(action) => {
                let is_edit = action.is_edit();
                self.editor_content.perform(action);

                if is_edit {
                    self.is_dirty = true;
                    let raw = self.editor_content.text();
                    self.preview_content = markdown::Content::parse(&raw);
                }

                Task::none()
            }
            Message::LinkClicked(url) => {
                let _ = webbrowser::open(&url);
                Task::none()
            }
            Message::NewFile => {
                if !self.is_loading {
                    self.current_file = None;
                    self.editor_content = text_editor::Content::new();
                    self.preview_content = markdown::Content::parse("");
                    self.is_dirty = false;
                }
                Task::none()
            }
            Message::OpenFile => {
                if self.is_loading {
                    return Task::none();
                }
                self.is_loading = true;

                window::oldest()
                    .and_then(|id| window::run(id, file_ops::open_file_dialog))
                    .then(Task::future)
                    .map(Message::FileOpened)
            }
            Message::FileOpened(result) => {
                self.is_loading = false;
                self.is_dirty = false;

                if let Ok((path, contents)) = result {
                    self.current_file = Some(path);
                    self.editor_content = text_editor::Content::with_text(&contents);
                    self.preview_content = markdown::Content::parse(&contents);
                }

                Task::none()
            }
            Message::SaveFile => {
                if self.is_loading {
                    return Task::none();
                }
                self.is_loading = true;

                let current_path = Arc::new(self.current_file.clone());
                let contents = Arc::new(self.editor_content.text());

                window::oldest()
                    .and_then(move |id| {
                        let current_path = current_path.clone();
                        let contents = contents.clone();
                        window::run(id, move |w| {
                            let path = (*current_path).clone();
                            let text = (*contents).clone();
                            file_ops::save_file_dialog(w, path, text)
                        })
                    })
                    .then(Task::future)
                    .map(Message::FileSaved)
            }
            Message::FileSaved(result) => {
                self.is_loading = false;

                if let Ok(path) = result {
                    self.current_file = Some(path);
                    self.is_dirty = false;
                }

                Task::none()
            }
            Message::Format(action) => {
                // Get the currently selected text (or empty string).
                let selection = self.editor_content.selection().unwrap_or_default();
                let formatted = action.apply(&selection);

                // We insert the formatted text by performing edit actions.
                // First, if there's a selection it will be replaced by typing.
                for ch in formatted.chars() {
                    self.editor_content
                        .perform(text_editor::Action::Edit(text_editor::Edit::Insert(ch)));
                }

                self.is_dirty = true;
                let raw = self.editor_content.text();
                self.preview_content = markdown::Content::parse(&raw);

                Task::none()
            }
        }
    }

    /// Build the UI: toolbar on top, editor on the left, preview on the right.
    pub fn view(&self) -> Element<'_, Message> {
        // -- Toolbar --
        let file_controls = row![
            toolbar_text_button("New", Message::NewFile),
            toolbar_text_button("Open", Message::OpenFile),
            toolbar_text_button("Save", Message::SaveFile,),
        ]
        .spacing(4);

        let format_bar = toolbar::view();

        let toolbar = container(
            row![file_controls, format_bar]
                .spacing(16)
                .align_y(iced::Center),
        )
        .padding([8, 12])
        .width(Fill)
        .style(app_theme::toolbar_container);

        // -- Editor pane --
        let editor = container(
            text_editor(&self.editor_content)
                .placeholder("Type your Markdown here...")
                .on_action(Message::EditorAction)
                .height(Fill)
                .padding(12)
                .font(Font::MONOSPACE)
                .highlight("md", self.highlight_theme)
                .key_binding(|key_press| match key_press.key.as_ref() {
                    keyboard::Key::Character("s") if key_press.modifiers.command() => {
                        Some(text_editor::Binding::Custom(Message::SaveFile))
                    }
                    keyboard::Key::Character("o") if key_press.modifiers.command() => {
                        Some(text_editor::Binding::Custom(Message::OpenFile))
                    }
                    keyboard::Key::Character("n") if key_press.modifiers.command() => {
                        Some(text_editor::Binding::Custom(Message::NewFile))
                    }
                    keyboard::Key::Character("b") if key_press.modifiers.command() => Some(
                        text_editor::Binding::Custom(Message::Format(FormatAction::Bold)),
                    ),
                    keyboard::Key::Character("i") if key_press.modifiers.command() => Some(
                        text_editor::Binding::Custom(Message::Format(FormatAction::Italic)),
                    ),
                    _ => text_editor::Binding::from_key_press(key_press),
                }),
        )
        .width(Fill)
        .height(Fill)
        .style(app_theme::editor_pane);

        // -- Preview pane --
        let style = markdown::Style::from_palette(self.theme().palette());
        let preview_items = markdown::view(
            self.preview_content.items(),
            markdown::Settings::with_style(style),
        )
        .map(Message::LinkClicked);

        let preview = container(
            scrollable(container(preview_items).padding(16).width(Fill))
                .height(Fill)
                .width(Fill),
        )
        .width(Fill)
        .height(Fill)
        .style(app_theme::preview_pane);

        // -- Status bar --
        let file_display = self
            .current_file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| String::from("Untitled"));

        let cursor = self.editor_content.cursor();
        let position = format!(
            "Ln {}, Col {}",
            cursor.position.line + 1,
            cursor.position.column + 1
        );

        let status_bar = container(
            row![
                text(file_display).size(12).style(app_theme::status_text),
                space::horizontal(),
                text(if self.is_dirty { "Modified" } else { "Saved" })
                    .size(12)
                    .style(app_theme::status_text),
                space::horizontal().width(16),
                text(position).size(12).style(app_theme::status_text),
            ]
            .align_y(iced::Center),
        )
        .padding([4, 12])
        .width(Fill)
        .style(app_theme::toolbar_container);

        // -- Layout --
        let content = row![editor, preview].spacing(2);

        column![toolbar, content, status_bar].into()
    }

    /// Return the application theme.
    pub fn theme(&self) -> Theme {
        Theme::TokyoNight
    }
}

/// Helper to create a simple text button for the toolbar.
fn toolbar_text_button(label: &str, message: Message) -> Element<'_, Message> {
    iced::widget::button(text(label).size(13))
        .padding([4, 10])
        .style(app_theme::toolbar_button)
        .on_press(message)
        .into()
}
