# Repository Guidelines

## Project Structure & Module Organization

`nox` is a minimal Rust binary crate using the 2024 edition. `Cargo.toml` defines package metadata and dependencies; the dependency list is currently empty. `src/main.rs` contains the executable entry point and currently prints “Hello, world!”. Add application modules under `src/` as functionality grows. There are no dedicated test, asset, or documentation directories yet. Cargo build artifacts belong in the ignored `target/` directory.

## Build, Test, and Development Commands

Use a Rust toolchain supporting the 2024 edition and run commands from the repository root:

- `cargo run`: build and run the executable locally.
- `cargo build`: compile a debug build.
- `cargo build --release`: compile an optimized executable in `target/release/`.
- `cargo check`: check compilation without producing an executable.
- `cargo test`: run unit, integration, and applicable documentation tests.
- `cargo fmt --check`: verify formatting; use `cargo fmt` to apply it.
- `cargo clippy --all-targets -- -D warnings`: lint all targets and treat warnings as errors.

## Coding Style & Naming Conventions

Follow standard Rust formatting with `rustfmt`, including four-space indentation. Use `snake_case` for modules, files, and functions; `UpperCamelCase` for types and traits; and `SCREAMING_SNAKE_CASE` for constants. Keep `main` focused on startup and delegate substantial behavior to modules. No custom formatting or lint configuration is currently present.

## Testing Guidelines

Use Rust’s built-in test framework. Place unit tests in a `#[cfg(test)] mod tests` block alongside the implementation, and add integration tests under `tests/` when needed. Name tests after observable behavior, such as `rejects_empty_input`. Cover new behavior and relevant failure cases. No tests or coverage threshold currently exist; run `cargo test` before submitting changes.

## Commit & Pull Request Guidelines

The Git history contains only the initial `cargo` commit, so no established commit convention is evident. Use concise, imperative subjects, such as `Add argument parsing`, and keep commits focused. Pull requests should explain the purpose and behavior changes, link related issues when applicable, and list validation commands and results. Include sample terminal output when changing command-line behavior. Before requesting review, run formatting, lint, and test checks above.
