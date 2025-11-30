# Repository Guidelines

## Project Structure & Module Organization
This Rust workspace is organized into multiple crates, typically located in directories like `core/`, `tui/`, and `common/`.
- **Crate Naming**: Crates are prefixed with `codex-` (e.g., `core/` maps to `codex-core`).
- **Core Modules**:
  - `core/`: Main logic and orchestration.
  - `tui/`: Terminal User Interface implementation.
  - `protocol/`: Shared protocol definitions.
  - `docs/`: Documentation files (update when APIs change).

## Build, Test, and Development Commands
Ensure `just`, `rg`, and `cargo-insta` are installed.

- **Formatting**: Run `just fmt` automatically after code changes (no approval needed).
- **Linting**: Run `just fix -p <project>` to fix linter issues before finalizing. Ask for approval if running interactively.
- **Testing**:
  - Run specific tests: `cargo test -p <project>`.
  - Run all tests: `cargo test --all-features` (ask for approval).
  - Snapshot tests: Use `cargo insta pending-snapshots -p codex-tui` and `cargo insta accept -p codex-tui` for UI changes.

## Coding Style & Naming Conventions
- **Rust Idioms**:
  - Use `format!("Val: {val}")` (inline args) over `format!("Val: {}", val)`.
  - Collapse nested `if` statements.
  - Use method references over closures where possible.
- **TUI Styling (ratatui)**:
  - Prefer `Stylize` traits: `"text".red().bold()` over `Span::styled`.
  - Use `textwrap::wrap` for string wrapping.
  - Construct lines with `vec!["...".into()].into()` for compactness.

## Testing Guidelines
- **Coverage**: Maintain unit test coverage. Add integration tests for workflows spanning modules.
- **Assertions**: Use `pretty_assertions::assert_eq` for readable diffs.
- **Integration**: Use `core_test_support::responses` for `codex-core` end-to-end tests.
  - Mock responses using `ResponseMock` and helper constructors like `ev_response_created`.
- **Snapshots**: Review `*.snap.new` files before accepting changes with `insta`.

## Commit & Documentation Guidelines
- **Changelog**: For major changes, update `CHANGELOG.md` following a best-practice template.
  - Categorize entries under headers: `### Features`, `### Refactor`, or `### Fixes`.
  - Include file changes or affected components in the description.
  - Update `README.md` if the change affects public APIs or architectural behavior.
- **Docs**: Keep concise doc comments on functions.
- **Git**: Do not revert unrelated changes in dirty worktrees.
