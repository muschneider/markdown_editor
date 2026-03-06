//! File I/O operations for opening and saving Markdown files.
//!
//! On Linux (Wayland/X11), `rfd` dialogs require a parent window handle to
//! appear correctly.  The functions [`open_file_dialog`] and
//! [`save_file_dialog`] accept `&dyn iced::Window` so callers can provide the
//! handle obtained via `iced::window::run`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use iced::Window;

/// Errors that can occur during file operations.
#[derive(Debug, Clone)]
pub enum FileError {
    /// The user cancelled the file dialog.
    DialogClosed,
    /// An I/O error occurred.
    IoError(io::ErrorKind),
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileError::DialogClosed => write!(f, "Dialog closed"),
            FileError::IoError(kind) => write!(f, "I/O error: {kind}"),
        }
    }
}

/// Load a file from disk, returning its path and contents.
pub async fn load_file(path: impl Into<PathBuf>) -> Result<(PathBuf, Arc<String>), FileError> {
    let path = path.into();

    let contents = tokio::fs::read_to_string(&path)
        .await
        .map(Arc::new)
        .map_err(|error| FileError::IoError(error.kind()))?;

    Ok((path, contents))
}

/// Show a native **open** file dialog parented to the application window.
///
/// The dialog lets the user browse folders and pick any file, with
/// convenience filters for Markdown, Text and All Files.
///
/// This function is designed to be called via
/// `window::run(id, open_file_dialog)` so the runtime provides the window
/// handle.
pub fn open_file_dialog(
    window: &dyn Window,
) -> impl Future<Output = Result<(PathBuf, Arc<String>), FileError>> + use<> {
    let dialog = rfd::AsyncFileDialog::new()
        .set_title("Open file")
        .add_filter("Markdown", &["md", "markdown"])
        .add_filter("Text", &["txt"])
        .add_filter("All Files", &["*"])
        .set_parent(&window);

    async move {
        let handle = dialog.pick_file().await.ok_or(FileError::DialogClosed)?;
        load_file(handle.path()).await
    }
}

/// Show a native **save** file dialog parented to the application window.
///
/// Always presents the dialog so the user can type a file name and pick a
/// location. When `current_path` is `Some`, the dialog starts in that
/// file's directory with the name pre-filled.
pub fn save_file_dialog(
    window: &dyn Window,
    current_path: Option<PathBuf>,
    contents: String,
) -> impl Future<Output = Result<PathBuf, FileError>> + use<> {
    let mut dialog = rfd::AsyncFileDialog::new()
        .set_title("Save Markdown file")
        .add_filter("Markdown", &["md", "markdown"])
        .add_filter("Text", &["txt"])
        .add_filter("All Files", &["*"])
        .set_parent(&window);

    if let Some(ref path) = current_path {
        if let Some(parent) = path.parent() {
            dialog = dialog.set_directory(parent);
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            dialog = dialog.set_file_name(name);
        }
    }

    async move {
        let path = dialog
            .save_file()
            .await
            .as_ref()
            .map(rfd::FileHandle::path)
            .map(Path::to_owned)
            .ok_or(FileError::DialogClosed)?;

        tokio::fs::write(&path, &contents)
            .await
            .map_err(|error| FileError::IoError(error.kind()))?;

        Ok(path)
    }
}
