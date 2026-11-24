# Nox

A native Rust log explorer modeled on the core workflow of [Klogg](https://github.com/variar/klogg), built with [GPUI Community Edition](https://gpui-ce.github.io/). This is a new implementation, not a copy of Klogg's Qt/C++ code.

```sh
cargo run -- fixtures/demo.log
cargo run -- /var/log/app.log /var/log/worker.log
```

Nox has a full-file pane and a separate filtered-results pane, multiple file tabs, regex and literal searches, case/whole-word/inverted matching, context lines, search presets, colored highlighting, bookmarks, line navigation, live following, recent/favorite files, session restore, clipboard copying, and result export. File indexing and searches run on worker threads. Both panes render only visible rows and scroll horizontally for long lines. A clickable overview marks the locations of matches and bookmarks.

Requires a Rust toolchain supporting edition 2024 and the native build prerequisites for [GPUI CE](https://github.com/gpui-ce/gpui-ce/blob/main/SETUP.md). macOS builds use Metal. This repository has been built and tested on macOS; Linux and Windows have not been verified. Linux builds may need the platform crate's `x11` or `wayland` feature and the corresponding system libraries.

## Using the viewer

Open files with the folder icon (**⌘O** / **Ctrl+O**), by dragging files onto the window, the command line, or the history icon (**Recent & favorites**). Click a tab to switch and middle-click it or click its inline `×` icon to close it. Hover any icon button to see its action label and active keyboard shortcut. Enter a query and press **Enter** or click the apply icon; toggle **Regular expression**, **Match case**, **Whole words**, and **Exclude matches** directly from the inline icons inside the search bar, and toggle search match background highlighting from the highlighter icon on the left side of the search bar (or **⌘⌥H** / **Ctrl+Alt+H**). In regex mode, highlighting emphasizes only capturing groups when present (or the full match if there are none); in literal mode, it highlights the matched literal text across both panes. Open saved patterns from the **Patterns** dropdown (or **⌘P** / **Ctrl+P**), navigate with **↑**/**↓** (**Home**/**End**), and press **Enter** or click an item to apply it immediately. An empty search shows only bookmarks when `include_bookmarks` is enabled.

Click a result to focus the filter pane and jump the main pane to its source line; press **↑**/**↓** (or **Home**/**End**/**PageUp**/**PageDown**) to keep navigating inside the filter pane, or press **Tab** to switch focus between the main pane and the filter pane. Select text directly inside displayed rows in either pane by clicking and dragging across characters or multiple rows, double-clicking a word, triple-clicking a line, or **Shift+Clicking** to extend an active text selection (**⌘C** / **Ctrl+C** copies the selected text and **Escape** clears it). Multi-select lines in either pane with **Shift+Click**, **⌘+Click** / **Ctrl+Click**, click-and-drag, **Shift+Arrows** / **PageUp** / **PageDown** / **Home** / **End**, or **⌘A** / **Ctrl+A** (**Alt+O** / **⌥O** flips the selection anchor between the top and bottom ends, and **Escape** collapses back to the active line). Click the gutter to bookmark a single line, or press **m** (or the bookmark icon) to toggle bookmarks on all selected lines. Selecting a line stops following; the follow icon resumes scrolling to the end as changes arrive. Watching continues even when following is off. The reload icon retries failed opens and forces reindexing. Truncation and replacement clear stale bookmarks while ordinary appends preserve them.

The export icon in the results header writes all currently displayed result rows, including context and bookmarks, in source order. It exports complete original bytes, regardless of the display limit, and refuses to overwrite an existing file. Resize the filter pane vertically all the way from the bottom (hiding the filter pane) to the top (hiding the main log pane) by dragging the horizontal split handle above the search bar (double-click it to reset to `window.results_height`), clicking the resize buttons in the results header, or pressing **-** (taller) and **⇧-** / **Shift+-** (shorter) in the log/filter panes — holding either key smoothly glides with an automatic acceleration gradient. The copy icon (or **⌘C** / **Ctrl+C**) copies all selected lines in the active pane as text. Regex errors and file errors appear in a banner above the viewer, even when the status bar is hidden.

Default shortcuts (`secondary` is Command on macOS and Control elsewhere):

| Context | Keys | Action |
| --- | --- | --- |
| Workspace | secondary-o / secondary-w | Open / close tab |
| Workspace | ctrl-tab | Next tab |
| Workspace | secondary-f / secondary-l | Focus search / go to line |
| Workspace | secondary-p | Toggle saved patterns dropdown |
| Workspace | secondary-alt-h | Toggle search match highlighting |
| Workspace | secondary-r / secondary-e | Reload / export results |
| Workspace | F3 / shift-F3 | Next / previous match |
| Workspace | secondary-q | Quit |
| Log | arrows, home/end, pageup/pagedown | Navigate lines (in active pane) |
| Log | shift-arrows, shift-home/end, shift-pageup/pagedown | Extend selection (in active pane) |
| Log | secondary-a / escape | Select all / collapse selection |
| Log | tab | Switch focus between main and filter pane |
| Log | m | Toggle bookmark on selected lines |
| Log | b / shift-b | Next / previous bookmark |
| Log | f | Toggle following |
| Log | secondary-c | Copy selected lines |
| Log | - / shift-- | Make filter pane taller / shorter (smooth glide on long press) |
| Selection | alt-o | Toggle selection anchor between top and bottom |
| TextSelected | secondary-c | Copy selected text |
| TextSelected | f | Insert selected text into search box (regex-escaped in Regex mode) and select it |
| TextSelected && RegexMode | a | Append regex-escaped selected text to search pattern with `\|` (or copy if empty) and focus end |
| TextSelected | escape | Clear text selection |
| Search | Enter / Escape | Apply search / focus log |
| Search | secondary-alt-r | Toggle regular expression |
| Search | secondary-alt-c | Toggle match case |
| Search | secondary-alt-w | Toggle whole words |
| Search | secondary-alt-i | Toggle exclude matches (invert) |
| GoTo | Enter / Escape | Jump / cancel |
| Presets | up/down, home/end, Enter/Space, Escape | Navigate / apply / close saved patterns |

Text editing uses GPUI CE's native editable-text element, including clipboard operations, selection, undo, and IME support.

## Configuration

Configuration is grouped by responsibility. The checked-in files in `config/` are also embedded as portable defaults; no config files are needed to start the binary.

```sh
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
| `viewer.json` | Line numbers/height, match overview visibility/width, tab expansion, display character limit, following, bookmark inclusion, polling interval |
| `search.json` | Default search mode, case/word/inversion/match highlighting, context lines, match limit, named presets (pattern, mode, `highlight_match`) |
| `highlighting.json` | Enable highlighting; ordered regex rules with colors and case sensitivity (first match wins) |
| `files.json` | Startup paths, favorites, session restore/save, session path, recent-file limit |
| `keyboard.json` | Key sequences, typed action names, and `Workspace`, `Log`, `Selection`, `TextSelected`, `TextSelected && RegexMode`, `RegexMode`, `Search`, `GoTo`, or `Presets` contexts |

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

Session state is separate from configuration and has its own `schemas/session.schema.json`. On a normal close or quit, Nox atomically saves open files, active tab, per-tab query/options, selected line, bookmarks, follow state, recent files, and the resized filter pane height. Command-line or configured startup files take priority over restoring the previous tabs. Corrupt/unsupported session files are reported and preserved rather than overwritten.

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

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Core tests cover indexing, CRLF/UTF-8 handling, file replacement/truncation, search modes/context/limits/cancellation, safe export, configuration overlays/validation, and session persistence. UI regression tests exercise keyboard focus and binding validation.
