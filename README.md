# Nox

A native Rust log explorer modeled on the core workflow of [Klogg](https://github.com/variar/klogg), built with [GPUI Community Edition](https://gpui-ce.github.io/). This is a new implementation, not a copy of Klogg's Qt/C++ code.

```sh
cargo run -- fixtures/demo.log
cargo run -- /var/log/app.log /var/log/worker.log
```

Nox has a full-file pane and a separate filtered-results pane, multiple file tabs, regex and literal searches, case/whole-word/inverted matching, context lines, search presets, colored highlighting, bookmarks, line navigation, live following, recent/favorite files, session restore, clipboard copying, and result export. File indexing and searches run on worker threads. Both panes render only visible rows and scroll horizontally for long lines. A clickable overview marks the locations of matches and bookmarks.

Requires a Rust toolchain supporting edition 2024 and the native build prerequisites for [GPUI CE](https://github.com/gpui-ce/gpui-ce/blob/main/SETUP.md). macOS builds use Metal. This repository has been built and tested on macOS; Linux and Windows have not been verified. Linux builds may need the platform crate's `x11` or `wayland` feature and the corresponding system libraries.

## Using the viewer

Open files with **Open**, by dragging files onto the window, the command line, or **Files** (recent files and configured favorites). Click a tab to switch; **Close tab** closes the active file. Enter a search and press **Enter** or **Search**. `.*`, `Aa`, `Word`, and `Not` toggle regex, case sensitivity, whole-word matching, and inversion. Presets apply immediately. An empty search shows only bookmarks when `include_bookmarks` is enabled.

Click a result to jump to its source line. Click the gutter or use **Bookmark** to mark a line. Selecting a line stops following; **Follow** resumes scrolling to the end as changes arrive. Watching continues even when following is off. **Reload** retries failed opens and forces reindexing. Truncation and replacement clear stale bookmarks while ordinary appends preserve them.

**Export** writes all currently displayed result rows, including context and bookmarks, in source order. It exports complete original bytes, regardless of the display limit, and refuses to overwrite an existing file. Copying copies the complete selected line as text. Regex errors and file errors appear in the status bar.

Default shortcuts (`secondary` is Command on macOS and Control elsewhere):

| Context | Keys | Action |
| --- | --- | --- |
| Workspace | secondary-o / secondary-w | Open / close tab |
| Workspace | ctrl-tab | Next tab |
| Workspace | secondary-f / secondary-l | Focus search / go to line |
| Workspace | secondary-r / secondary-e | Reload / export results |
| Workspace | F3 / shift-F3 | Next / previous match |
| Workspace | secondary-q | Quit |
| Log | arrows, home/end, pageup/pagedown | Navigate lines |
| Log | m | Toggle bookmark |
| Log | b / shift-b | Next / previous bookmark |
| Log | f | Toggle following |
| Log | secondary-c | Copy selected line |
| Search | Enter / Escape | Apply search / focus log |
| GoTo | Enter / Escape | Jump / cancel |

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
| `appearance.json` | Font family/size and semantic colors (`#RRGGBB`) |
| `viewer.json` | Line numbers/height, match overview visibility/width, tab expansion, display character limit, following, bookmark inclusion, polling interval |
| `search.json` | Default search mode, case/word/inversion, context lines, match limit, named presets |
| `highlighting.json` | Enable highlighting; ordered regex rules with colors and case sensitivity (first match wins) |
| `files.json` | Startup paths, favorites, session restore/save, session path, recent-file limit |
| `keyboard.json` | Key sequences, typed action names, and `Workspace`, `Log`, `Search`, or `GoTo` contexts |

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

Session state is separate from configuration and has its own `schemas/session.schema.json`. On a normal close or quit, Nox atomically saves open files, active tab, per-tab query/options, selected line, bookmarks, follow state, and recent files. Command-line or configured startup files take priority over restoring the previous tabs. Corrupt/unsupported session files are reported and preserved rather than overwritten.

The default state location is:

- macOS: `~/Library/Application Support/nox/session.json`
- Linux: `$XDG_STATE_HOME/nox/session.json`, falling back to `~/.local/state/nox/session.json`
- Windows: `%LOCALAPPDATA%/nox/session.json`

Set `NOX_STATE_DIR` or `files.state_file` to change it (`state_file` takes priority). `save_session` and `restore_session` can be disabled independently. Paths in JSON are filesystem paths, relative to the working directory if not absolute; shell expansions such as `~` are not performed. Session saving happens on normal shutdown, not after a crash. Concurrent app instances share the same state path; the last clean exit wins.

## Architecture and limits

`src/config.rs` owns typed configuration, validation, and schema generation. `src/logfile.rs` indexes byte offsets and reads source data on demand using positioned reads. `src/search.rs` streams through the file and returns matching/context line indices. `src/session.rs` manages persisted state. `src/ui.rs` owns GPUI presentation and asynchronous job coordination. The executable entry point handles command-line startup.

The line index uses approximately eight bytes per line (plus allocation overhead). Search buffers one complete line at a time; pathological single-line files can still use substantial memory. Display text is bounded by `max_display_chars`, while matching, copying, and export use complete lines. Search uses Rust's Unicode regex engine, not Klogg's Hyperscan/PCRE engine: lookaround and backreferences are unsupported. `max_results` caps matching lines, not additional context/bookmark rows.

Files are treated as UTF-8 with replacement characters for invalid sequences. Live monitoring polls metadata and rebuilds the index in the background when a change is detected; it does not yet incrementally index appends. A scan reads up to the file length captured at its start. In-place rewrites during reading can temporarily show inconsistent content until the next refresh. Large scans finish before another automatic reload starts, avoiding search starvation on continuously growing files.

This version covers Klogg's core local-file workflow, not complete feature parity. Remaining differences include encoding detection/selection, archives and URL loading, boolean expression parsing, scratchpads, in-app settings editors, multi-line/substring selection in log panes, configurable highlighter sets, and Klogg's SIMD search optimizations.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

Core tests cover indexing, CRLF/UTF-8 handling, file replacement/truncation, search modes/context/limits/cancellation, safe export, configuration overlays/validation, and session persistence. UI regression tests exercise keyboard focus and binding validation.
