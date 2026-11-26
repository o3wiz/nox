# Repository Guidelines

## Project Structure & Module Organization

`nox` is a native Rust (edition 2024, MSRV 1.95) GPU-accelerated log explorer built with GPUI Community Edition.

- `src/lib.rs`: core library modules (`config`, `logfile`, `path_complete`, `search`, `selection`, `session`, `zoom`).
- `src/main.rs`: CLI argument parsing (`--help`, `--version`, `--config-dir`, `--init-config`, `--migrate-config`, `--write-schemas`, `--check-config`) and application startup.
- `src/ui/`: GPUI presentation layer (`mod.rs`, `components.rs`, `icons.rs`, `resize.rs`, `scrollbar.rs`, `selectable_text.rs`, `theme.rs`).
- `config/`: default JSON configuration files embedded into the binary at compile time.
- `schemas/`: JSON Schemas generated from the Rust config/session types (verified by tests).
- `fixtures/demo.log`: sample log file for local testing and demos.
- `packaging/`: native desktop delivery assets (`assets/nox.svg`, `macos/Info.plist`, `macos/Nox.icns`, `linux/nox.desktop`).
- `tests/`: integration tests (`tests/core.rs`, `tests/cli.rs`).
- `.github/`: GitHub Actions CI, multi-platform release CD, security audit, and Dependabot configuration.
- `Makefile`: local quality gate and packaging targets (`make check`, `make package-macos`, `make install-macos`, `make package-linux`).

Build artifacts belong in the ignored `target/` and `dist/` directories.

## Build, Test, and Development Commands

Use Rust `1.95+` (edition 2024) from the repository root:

- `make check`: run the full CI check suite (format, Clippy, tests, docs, CLI validation).
- `cargo run -- fixtures/demo.log`: build and launch the viewer locally.
- `cargo build --release --locked`: compile the optimized release executable in `target/release/nox`.
- `cargo test --locked`: run unit, UI, and integration tests.
- `cargo fmt --all -- --check`: verify formatting; use `cargo fmt --all` to apply it.
- `cargo clippy --all-targets --locked -- -D warnings`: lint all targets and treat warnings as errors.
- `make package-macos` / `make install-macos`: assemble signed `Nox.app`, `.dmg`, `.app.zip`, CLI `.tar.gz`, and checksums in `dist/`.

## Coding Style & Naming Conventions

Follow standard Rust formatting with `rustfmt` (four-space indentation). Use `snake_case` for modules, files, and functions; `UpperCamelCase` for types and traits; and `SCREAMING_SNAKE_CASE` for constants. Keep `main` focused on CLI/startup and delegate behavior to library and UI modules. Whenever Rust config or session structs change, regenerate `schemas/` via `cargo run -- --write-schemas schemas`.

## Testing Guidelines

Place unit tests in `#[cfg(test)] mod tests` blocks alongside implementations, UI regression tests in `src/ui/mod.rs` using `#[gpui::test]`, and integration tests under `tests/`. Name tests after observable behavior (e.g., `ignores_macos_launch_services_psn_argument`) and ensure tests work cross-platform across macOS, Linux, and Windows.

## Commit & Pull Request Guidelines

Use concise, imperative commit subjects (e.g., `Add --version CLI flag`) and keep commits focused. Before opening a PR or tagging a release, verify `make check` passes cleanly. To cut a release, bump `version` in `Cargo.toml`, update `Cargo.lock`, push to the default branch, and push a matching `vX.Y.Z` tag.
