# md_editor

A simple Markdown editor with live preview, built with [Iced](https://iced.rs/) and Rust.

## Development Approach

This project was implemented using **vibe coding** — an AI-assisted development approach where the focus is on high-level design and direction while AI handles the implementation details. The result is clean, production-ready Rust code with Iced's modern functional API.

## Features

- **Inline rendering**: the document is shown rendered; only the line under the
  cursor falls back to raw Markdown source, so there is no separate preview pane
- **Whole-table editing**: entering a table makes the entire table editable at
  once, so rows never fall apart mid-edit
- **Find and replace**: `Ctrl+F` / `Ctrl+H`, with a match counter, case
  toggle, and single or whole-document replacement
- **Windowed rendering**: only the segments near the viewport are built into the
  widget tree, so large documents stay responsive
- **File Operations**: Open and save Markdown files via native file dialogs
- **Formatting Toolbar**: Quick access to common Markdown formatting options
- **Settings panel** (`Ctrl+,`): theme, font size, text width, toolbar and
  status-bar visibility, and reopening the last file on launch
- **Themes**: every built-in Iced theme, plus per-tag Markdown color overrides

Every preference is persisted to `$XDG_CONFIG_HOME/md_editor/config.yaml`
(falling back to `$HOME/.config/...`) and reapplied on the next launch. The file
is plain YAML and safe to hand-edit — unknown keys and invalid values fall back
to their defaults rather than failing the load.

## Keyboard Shortcuts

| Keys                      | Action                     |
| ------------------------- | -------------------------- |
| `Ctrl+N` / `Ctrl+O` / `Ctrl+S` | New / open / save     |
| `Ctrl+Z` / `Ctrl+Y`       | Undo / redo                |
| `Ctrl+F` / `Ctrl+H`       | Find / find and replace    |
| `Enter` / `F3` / `Shift+F3` | Next / next / previous match |
| `Ctrl+,`                  | Settings panel             |
| `Esc`                     | Close the find bar or an open panel |
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

# Run without installing
cargo run -- README.md
```

## Installing (Linux)

### From source

```bash
make install
```

Builds in release mode and installs three files under `~/.local` — no `sudo`:

| Path                                                      | What            |
| --------------------------------------------------------- | --------------- |
| `~/.local/bin/md_editor`                                   | the binary      |
| `~/.local/share/applications/md_editor.desktop`            | launcher entry  |
| `~/.local/share/icons/hicolor/scalable/apps/md_editor.svg` | icon            |

The desktop entry registers `md_editor` as a handler for `text/markdown`, so
Markdown files can be opened with it from a file manager. Make sure
`~/.local/bin` is on your `PATH`.

Override the location with `PREFIX`, and undo it with `uninstall`:

```bash
sudo make install PREFIX=/usr/local
make uninstall                       # add the same PREFIX you installed with
make help                            # list every target
```

### From a release tarball

Tagged releases publish `md_editor-<version>-x86_64-unknown-linux-gnu.tar.gz`,
built on Ubuntu 22.04 (glibc 2.35). It needs no Rust toolchain — only GTK3.

```bash
tar xf md_editor-*-x86_64-unknown-linux-gnu.tar.gz
cd md_editor-*-x86_64-unknown-linux-gnu
./install.sh                    # ~/.local; --prefix /usr/local for system-wide
./install.sh --uninstall
```

Verify the download against the published `SHA256SUMS` first:

```bash
sha256sum -c SHA256SUMS --ignore-missing
```

### Arch Linux

```bash
makepkg -si -p packaging/PKGBUILD
```

`make dist` produces the same tarball locally that the release workflow
publishes.

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
  config.rs        # Persist settings + colors to $XDG_CONFIG_HOME
  file_ops.rs      # Async file I/O operations
  history.rs       # Undo/redo, recorded as line-range replacements
  icons.rs         # SVG icons for the toolbar
  markdown_view.rs # Theme-aware Markdown rendering + code highlighting
  search.rs        # Find / find-and-replace state and bar
  settings.rs      # User preferences and the settings panel
  theme.rs         # Theme styles and per-tag Markdown colors
  toolbar.rs       # Formatting toolbar

assets/
  md_editor.svg    # Application icon (scalable, installed to hicolor)

packaging/
  md_editor.desktop # Freedesktop launcher entry + text/markdown association
  install.sh        # Installer shipped inside the binary tarball
  PKGBUILD          # Arch Linux package

Makefile            # build / install / uninstall / dist
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
