# md_editor

A simple Markdown editor with live preview, built with [Iced](https://iced.rs/) and Rust.

## Development Approach

This project was implemented using **vibe coding** — an AI-assisted development approach where the focus is on high-level design and direction while AI handles the implementation details. The result is clean, production-ready Rust code with Iced's modern functional API.

## Features

- **Live Preview**: Side-by-side editing with real-time Markdown rendering
- **File Operations**: Open and save Markdown files via native file dialogs
- **Formatting Toolbar**: Quick access to common Markdown formatting options
- **Dark Theme**: Easy on the eyes for long writing sessions
- **Keyboard Shortcuts**: Efficient editing with custom key bindings

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
  main.rs       # Application entry point
  app.rs        # Core state and UI logic
  file_ops.rs   # Async file I/O operations
  theme.rs      # Dark theme styles
  toolbar.rs    # Formatting toolbar
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
