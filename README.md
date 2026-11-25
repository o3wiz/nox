# Nox

[![CI](https://github.com/o3wiz/nox/actions/workflows/ci.yml/badge.svg)](https://github.com/o3wiz/nox/actions/workflows/ci.yml)
[![Release](https://github.com/o3wiz/nox/actions/workflows/release.yml/badge.svg)](https://github.com/o3wiz/nox/actions/workflows/release.yml)

A native Rust log explorer modeled on the core workflow of [Klogg](https://github.com/variar/klogg), built with [GPUI Community Edition](https://gpui-ce.github.io/). This is a new implementation, not a copy of Klogg's Qt/C++ code.

```sh
nox fixtures/demo.log
nox /var/log/app.log /var/log/worker.log
```

Nox has a full-file pane and a separate filtered-results pane, multiple file tabs, regex and literal searches, case/whole-word/inverted matching, context lines, search presets, colored highlighting, bookmarks, line navigation, live following, recent/favorite files, session restore, clipboard copying, and result export. File indexing and searches run on worker threads. Both panes render only visible rows and scroll horizontally for long lines. A clickable overview marks the locations of matches and bookmarks.

## Installation

### Pre-built releases

Download the latest release package for your platform from [GitHub Releases](https://github.com/o3wiz/nox/releases) and verify integrity against `SHA256SUMS.txt`:

- **macOS (Apple Silicon / Intel)**: Open `Nox-<version>-<arch>.dmg` and drag **Nox.app** into **Applications** (or extract `Nox-<version>-<arch>.app.zip`), or use the standalone CLI archive `nox-v<version>-<target>.tar.gz`.
  - Release app bundles are ad-hoc signed. If macOS Gatekeeper warns on first launch, run `xattr -cr /Applications/Nox.app` or right-click **Nox.app** and choose **Open**.
- **Linux (`x86_64`)**: Extract `nox-v<version>-x86_64-unknown-linux-gnu.tar.gz`, copy `nox` onto your `PATH` (e.g. `~/.local/bin/nox`), and optionally install `nox.desktop` into `~/.local/share/applications/` and `nox.svg` into `~/.local/share/icons/hicolor/scalable/apps/`.
- **Windows (`x86_64`)**: Extract `nox-v<version>-x86_64-pc-windows-msvc.zip` and run `nox.exe` (or add its folder to `PATH`).

### Build from source

Requires Rust `1.95+` (edition 2024) and the native build prerequisites for [GPUI CE](https://github.com/gpui-ce/gpui-ce/blob/main/SETUP.md):

- **macOS**: Xcode Command Line Tools (`xcode-select --install`, renders with Metal).
- **Linux (Ubuntu/Debian)**:
  ```sh
  sudo apt-get install -y pkg-config libfontconfig1-dev libfreetype6-dev \
    libwayland-dev libx11-dev libx11-xcb-dev libxcb-randr0-dev \
    libxcb-shape0-dev libxcb-xfixes0-dev libxcb-xinput-dev \
    libxkbcommon-dev libxkbcommon-x11-dev
  ```
- **Windows**: Visual Studio C++ Build Tools and Windows 10/11 SDK (MSVC toolchain).

```sh
# Install CLI binary directly with Cargo
cargo install --git https://github.com/o3wiz/nox --locked

# Or build and run from a local clone
cargo run -- fixtures/demo.log
```

## Using the viewer

Open files with the folder icon (**⌘O** / **Ctrl+O**), by editing the file path bar above the log (click it or press **⌘⇧O** / **Ctrl+Shift+O** — use **Tab** / **↑** / **↓** for filesystem filename completion, **Enter** to replace the current tab's log, **⌘↵** / **Ctrl+Enter** to open in a new tab, and **Escape** to cancel), by dragging files onto the window, the command line, or the history icon (**Recent & favorites**). Click a tab to switch and middle-click it or click its inline `×` icon to close it. Hover any icon button to see its action label and active keyboard shortcut. Enter a query and press **Enter** or click the apply icon; toggle **Regular expression**, **Match case**, **Whole words**, and **Exclude matches** directly from the inline icons inside the search bar, and toggle search match background highlighting from the highlighter icon on the left side of the search bar (or **⌘⌥H** / **Ctrl+Alt+H**). In regex mode, highlighting emphasizes only capturing groups when present (or the full match if there are none); in literal mode, it highlights the matched literal text across both panes. Open saved patterns from the **Patterns** dropdown (or **⌘P** / **Ctrl+P**), type to live fuzzy-filter by pattern name, optionally refine with **↑**/**↓** (**Home**/**End**), and press **Enter** or click an item to apply it immediately. An empty search shows only bookmarks when `include_bookmarks` is enabled.

Click a result to focus the filter pane and jump the main pane to its source line; press **↑**/**↓** (or **Home**/**End**/**PageUp**/**PageDown**) to keep navigating inside the filter pane, **←**/**→** to scroll long lines horizontally in the active pane, or press **Tab** to switch focus between the main pane and the filter pane. Select text directly inside displayed rows in either pane by clicking and dragging across characters or multiple rows, double-clicking a word, triple-clicking a line, or **Shift+Clicking** to extend an active text selection (**⌘C** / **Ctrl+C** copies the selected text and **Escape** clears it). Multi-select lines in either pane with **Shift+Click**, **⌘+Click** / **Ctrl+Click**, click-and-drag, **Shift+Arrows** / **PageUp** / **PageDown** / **Home** / **End**, or **⌘A** / **Ctrl+A** (**Alt+O** / **⌥O** flips the selection anchor between the top and bottom ends, and **Escape** collapses back to the active line). Click the gutter to bookmark a single line, or press **m** (or the bookmark icon) to toggle bookmarks on all selected lines. Selecting a line stops following; the follow icon resumes scrolling to the end as changes arrive. Watching continues even when following is off. The reload icon retries failed opens and forces reindexing. Truncation and replacement clear stale bookmarks while ordinary appends preserve them.

The export icon in the results header writes all currently displayed result rows, including context and bookmarks, in source order. It exports complete original bytes, regardless of the display limit, and refuses to overwrite an existing file. Resize the filter pane vertically all the way from the bottom (hiding the filter pane) up to the file path bar (hiding the main log pane while keeping the file path bar visible) by dragging the horizontal split handle above the search bar (double-click it to reset to `window.results_height`), clicking the resize buttons in the results header, or pressing **-** (taller) and **⇧-** / **Shift+-** (shorter) in the log/filter panes — holding either key smoothly glides with an automatic acceleration gradient. Zoom the log and filter panes in and out with the zoom buttons in the top toolbar or **⌘=** / **Ctrl+=** (or **⌘+** / **Ctrl++**) and **⌘-** / **Ctrl+-**, and reset back to 100% with **⌘0** / **Ctrl+0** or by clicking the zoom indicator in the status bar. The copy icon (or **⌘C** / **Ctrl+C**) copies all selected lines in the active pane as text. Regex errors and file errors appear in a banner above the viewer, even when the status bar is hidden.

Default shortcuts (`secondary` is Command on macOS and Control elsewhere):

| Context | Keys | Action |
| --- | --- | --- |
| Workspace | secondary-o / secondary-w | Open / close tab |
| Workspace | secondary-shift-o | Focus editable file path bar |
| Workspace | ctrl-tab / ctrl-shift-tab | Next / previous tab |
| Workspace | secondary-f / secondary-l | Focus search / go to line |
| Workspace | secondary-p | Toggle saved patterns dropdown |
| Workspace | secondary-alt-h | Toggle search match highlighting |
| Workspace | secondary-r / secondary-e | Reload / export results |
| Workspace | secondary-= / secondary-- / secondary-0 | Zoom in / zoom out / reset zoom |
| Workspace | F3 / shift-F3 | Next / previous match |
| Workspace | secondary-q | Quit |
| Log | up/down, home/end, pageup/pagedown | Navigate lines (in active pane) |
| Log | left / right | Scroll horizontally (in active pane) |
| Log | shift-up/down, shift-home/end, shift-pageup/pagedown | Extend selection (in active pane) |
| Log | secondary-a / escape | Select all / collapse selection |
| Log | tab | Switch focus between main and filter pane |
| Log | m | Toggle bookmark on selected lines |
| Log | b / shift-b | Next / previous bookmark |
| Log | f | Toggle following |
| Log | secondary-c | Copy selected lines |
| Log | - / shift-- | Make filter pane taller / shorter (smooth glide on long press) |
| Selection | alt-o | Toggle selection anchor between top and bottom |
| TextSelected | secondary-c | Copy selected text |
| TextSelected | f | Insert selected text into search box (regex-escaped in Regex mode) and focus end |
| TextSelected && RegexMode | a | Append regex-escaped selected text to search pattern with `\|` (or copy if empty) and focus end |
| TextSelected | escape | Clear text selection |
| Search | Enter / Escape | Apply search / focus log |
| Search | secondary-alt-r | Toggle regular expression |
| Search | secondary-alt-c | Toggle match case |
| Search | secondary-alt-w | Toggle whole words |
| Search | secondary-alt-i | Toggle exclude matches (invert) |
| GoTo | Enter / Escape | Jump / cancel |
| FilePath | Enter / secondary-enter | Replace current tab's log / open in new tab |
| FilePath | Tab / shift-tab, up/down, Escape | Complete filesystem path / navigate suggestions / cancel |
| Presets | type to filter, up/down, home/end, Enter, Escape | Fuzzy-filter / navigate / apply / close saved patterns |

Text editing uses GPUI CE's native editable-text element, including clipboard operations, selection, undo, and IME support.

## Configuration

Configuration is grouped by responsibility. The checked-in files in `config/` are also embedded as portable defaults; no config files are needed to start the binary.

```sh
# Print version information.
cargo run -- --version
# Create editable defaults and local JSON schemas in a NEW directory.
cargo run -- --init-config ./my-config
# Use that configuration.
cargo run -- --config-dir ./my-config fixtures/demo.log
# Validate without opening a window.
cargo run -- --check-config --config-dir ./my-config
# Regenerate checked-in schemas after changing the Rust config types.
cargo run -- --write-schemas schemas
```

`--config-dir` takes precedence over `NOX_CONFIG_DIR`. Without either, Nox uses embedded defaults. To edit and run the repository defaults directly, use `--config-dir config`. Restart Nox after editing configuration.

Files and individual fields may be omitted. Objects overlay defaults; arrays replace the entire default array. Unknown fields, invalid types, out-of-range values, invalid colors/regexes, duplicate bindings in one context, and invalid key sequences produce an error. Explicitly selecting a nonexistent directory also produces an error. `--init-config` refuses to overwrite existing configuration files.

| File | Settings |
| --- | --- |
| `window.json` | Title, initial dimensions/maximized state, results-pane height, status bar |
| `appearance.json` | UI sans-serif & monospace font families/sizes, semantic colors, text selection color & opacity (`0.0`–`1.0`), and match foreground/background colors (`#RRGGBB`) |
| `viewer.json` | Line numbers/height, match overview visibility/width, cursor surrounding lines, tab expansion, display character limit, following, bookmark inclusion, polling interval |
| `search.json` | Default search mode, case/word/inversion/match highlighting, context lines, match limit, named presets (pattern, mode, `highlight_match`) |
| `highlighting.json` | Enable highlighting; ordered regex rules with colors and case sensitivity (first match wins) |
| `files.json` | Startup paths, favorites, session restore/save, session path, recent-file limit |
| `keyboard.json` | Key sequences, typed action names, and `Workspace`, `Log`, `Selection`, `TextSelected`, `TextSelected && RegexMode`, `RegexMode`, `Search`, `FilePath`, `GoTo`, or `Presets` contexts |

Each JSON file has a `$schema` link for editor completion and validation. Schemas are generated from the same Rust types used to deserialize configuration. Top-level properties are optional because files are overlays; objects inside replacement arrays require their fields. Additional semantic checks (for example regex syntax and shortcut conflicts) run in the application.

Example partial overrides:

```json
{
  "$schema": "schemas/window.schema.json",
  "width": 1600,
  "height": 1000,
  "results_height": 320
}
```

```json
{
  "$schema": "schemas/keyboard.schema.json",
  "bindings": [
    { "keys": "secondary-o", "context": "Workspace", "action": "open" },
    { "keys": "secondary-f", "context": "Workspace", "action": "focus_search" },
    { "keys": "enter", "context": "Search", "action": "search" },
    { "keys": "escape", "context": "Search", "action": "focus_log" },
    { "keys": "m", "context": "Log", "action": "toggle_bookmark" }
  ]
}
```

The second example replaces all application bindings. GPUI's standard text-editing bindings remain available inside text fields. Multi-stroke shortcuts use GPUI syntax, such as `secondary-k secondary-o`. Log-only shortcuts never run while typing in the search field.

## Session state

Session state is separate from configuration and has its own `schemas/session.schema.json`. On a normal close or quit, Nox atomically saves open files, active tab, per-tab query/options, selected line, bookmarks, follow state, recent files, the resized filter pane height, and zoom level. Command-line or configured startup files take priority over restoring the previous tabs. Corrupt/unsupported session files are reported and preserved rather than overwritten.

The default state location is:

- macOS: `~/Library/Application Support/nox/session.json`
- Linux: `$XDG_STATE_HOME/nox/session.json`, falling back to `~/.local/state/nox/session.json`
- Windows: `%LOCALAPPDATA%/nox/session.json`

Set `NOX_STATE_DIR` or `files.state_file` to change it (`state_file` takes priority). `save_session` and `restore_session` can be disabled independently. Paths in JSON are filesystem paths, relative to the working directory if not absolute; shell expansions such as `~` are not performed. Session saving happens on normal shutdown, not after a crash. Concurrent app instances share the same state path; the last clean exit wins.

## Architecture and limits

`src/config.rs` owns typed configuration, validation, and schema generation. `src/logfile.rs` indexes byte offsets and reads source data on demand using positioned reads. `src/search.rs` streams through the file and returns matching/context line indices. `src/session.rs` manages persisted state. `src/ui/` owns GPUI presentation (`mod.rs`), vector icons (`icons.rs`), design tokens and font resolution (`theme.rs`), filter-pane resize math and hold acceleration (`resize.rs`), custom scrollbars (`scrollbar.rs`), and reusable icon-first controls (`components.rs`). The executable entry point handles command-line startup.

The line index uses approximately eight bytes per line (plus allocation overhead). Search buffers one complete line at a time; pathological single-line files can still use substantial memory. Display text is bounded by `max_display_chars`, while matching, copying, and export use complete lines. Search uses Rust's Unicode regex engine, not Klogg's Hyperscan/PCRE engine: lookaround and backreferences are unsupported. `max_results` caps matching lines, not additional context/bookmark rows.

Files are treated as UTF-8 with replacement characters for invalid sequences. Live monitoring polls metadata and rebuilds the index in the background when a change is detected; it does not yet incrementally index appends. A scan reads up to the file length captured at its start. In-place rewrites during reading can temporarily show inconsistent content until the next refresh. Large scans finish before another automatic reload starts, avoiding search starvation on continuously growing files.

This version covers Klogg's core local-file workflow, not complete feature parity. Remaining differences include encoding detection/selection, archives and URL loading, boolean expression parsing, scratchpads, in-app settings editors, configurable highlighter sets, and Klogg's SIMD search optimizations.

## Development, packaging, and releases

Run the full local quality gate (formatting, Clippy, tests, docs, and config validation):

```sh
make check
```

Or run individual Cargo commands directly:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

Core tests cover indexing, CRLF/UTF-8 handling, file replacement/truncation, search modes/context/limits/cancellation, safe export, configuration overlays/validation, and session persistence. UI regression tests exercise keyboard focus, text/line selection, zooming, path completion, and binding validation. CLI integration tests verify `--help`, `--version`, `--init-config`, `--write-schemas`, and `--check-config`.

### Building local deliverables

```sh
# macOS: build Nox.app, drag-to-Applications .dmg, .app.zip, CLI .tar.gz, and SHA256SUMS.txt in dist/
make package-macos

# macOS: build and install directly to /Applications/Nox.app
make install-macos

# Linux: build release .tar.gz with desktop entry, icon, and SHA256SUMS.txt in dist/
make package-linux
```

### Publishing a GitHub Release (CI/CD)

1. Bump `version` in `Cargo.toml` and ensure `Cargo.lock` and `make check` are clean.
2. Push your changes to `master` (or `main`) and wait for the **CI** workflow (`macos-15`, `ubuntu-latest`, `windows-latest`) to pass.
3. Push a matching semantic version tag:
   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```
4. The **Release** workflow automatically verifies the tag matches `Cargo.toml`, runs formatting/lint/tests, builds optimized binaries and installers for macOS (Apple Silicon + Intel), Linux (`x86_64`), and Windows (`x86_64`), generates `SHA256SUMS.txt`, and publishes a GitHub Release with release notes. You can also trigger dry-run builds manually from **Actions → Release → Run workflow**.
