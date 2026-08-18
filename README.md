# md_editor

A simple Markdown editor with live preview, built with [Iced](https://iced.rs/) and Rust.

## Development Approach

This project was implemented using **vibe coding** — an AI-assisted development approach where the focus is on high-level design and direction while AI handles the implementation details. The result is clean, production-ready Rust code with Iced's modern functional API.

## Features

- **Inline rendering**: the document is shown rendered; only the line under the
  cursor falls back to raw Markdown source, so there is no separate preview pane
- **Whole-table editing**: entering a table makes the entire table editable at
  once, so rows never fall apart mid-edit
- **Windowed rendering**: only the segments near the viewport are built into the
  widget tree, so large documents stay responsive
- **File Operations**: Open and save Markdown files via native file dialogs
- **Formatting Toolbar**: Quick access to common Markdown formatting options
- **Themes**: every built-in Iced theme, plus per-tag Markdown color overrides
  persisted to `$XDG_CONFIG_HOME/md_editor/config.yaml`

## Keyboard Shortcuts

| Keys                      | Action                     |
| ------------------------- | -------------------------- |
| `Ctrl+N` / `Ctrl+O` / `Ctrl+S` | New / open / save     |
| `Ctrl+Z` / `Ctrl+Y`       | Undo / redo                |
| `Ctrl+B` / `Ctrl+I`       | Bold / italic              |
| `Ctrl+←` / `Ctrl+→`       | Move by word               |
| `Home` / `End`            | Start / end of line        |
| `PgUp` / `PgDn`           | Move a page                |
| `Ctrl+Home` / `Ctrl+End`  | Start / end of document    |

Clicking a rendered block moves the cursor into it.

## Screenshots

![Markdown Editor](https://via.placeholder.com/800x450?text=Markdown+Editor+Screenshot)

## Prerequisites

- **Rust 1.88+** (managed via [mise](https://mise.jdx.dev/))
- **GTK3** (Linux only, for file dialogs)

### System Dependencies

**Debian/Ubuntu:**
```bash
sudo apt install libgtk-3-dev
```

**Fedora:**
```bash
sudo dnf install gtk3-devel
```

**Arch:**
```bash
sudo pacman -S gtk3
```

## Building

```bash
# Install Rust toolchain (via mise)
mise install

# Build debug version
cargo build

# Build release version
cargo build --release
```

## Running

```bash
cargo run
```

## macOS Release (Apple Silicon)

Tagged releases (`v*`) are built on a GitHub-hosted Apple Silicon runner and
publish a `.dmg` containing `md_editor.app` for the `aarch64-apple-darwin`
target (M1–M4). To build manually on an M-series Mac:

```bash
cargo install cargo-bundle
cargo bundle --release --target aarch64-apple-darwin
```

> **Gatekeeper note:** the app is **not** codesigned or notarized, so macOS will
> block it on first launch. Either right-click the app and choose **Open**, or
> clear the quarantine flag:
>
> ```bash
> xattr -dr com.apple.quarantine md_editor.app
> ```

## Development

### Code Style

```bash
# Format code
cargo fmt

# Run linter
cargo clippy

# Check formatting
cargo fmt -- --check
```

### Testing

```bash
# Run all tests
cargo test

# Run tests with output
cargo test -- --nocapture
```

## Project Structure

```
src/
  main.rs          # Application entry point
  app.rs           # Core state, messages, update/view logic
  color_picker.rs  # Per-tag color customization widget
  config.rs        # Persist color config to $XDG_CONFIG_HOME
  file_ops.rs      # Async file I/O operations
  history.rs       # Undo/redo, recorded as line-range replacements
  icons.rs         # SVG icons for the toolbar
  markdown_view.rs # Theme-aware Markdown rendering + code highlighting
  theme.rs         # Theme styles and per-tag Markdown colors
  toolbar.rs       # Formatting toolbar
```

## Technologies

- [Iced 0.14](https://github.com/iced-rs/iced) - Cross-platform GUI framework
- [pulldown-cmark](https://github.com/pulldown-cmark/pulldown-cmark) - Markdown parser (via Iced)
- [rfd](https://github.com/PolyMeilex/rfd) - Native file dialogs
- [Tokio](https://tokio.rs/) - Async runtime

## License

MIT License - See [LICENSE](LICENSE) for details.

## Contributing

Contributions are welcome! Please run `cargo fmt && cargo clippy -- -D warnings && cargo test` before submitting PRs.
