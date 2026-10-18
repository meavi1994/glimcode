# Contributing to Glimcode

Pull requests for extra features, bug fixes, usability, documentation, and performance improvements are welcome. You can start with an issue, improve an existing feature, or send a small focused fix.

## Get set up

Install stable Rust and Git, then fork and clone the repository. Search libraries are compiled into the app; no `rg` installation is needed.

```sh
cargo run --release -- /path/to/test/project
```

For a quick workspace with sample code, CSV, JSON, and Markdown:

```sh
cargo run --release -- examples
```

Linux builds may need development packages for X11, Wayland, OpenGL, and xkbcommon. Include your distribution and any missing packages in build reports. macOS Apple Silicon is the currently exercised desktop environment.

## Sending a pull request

1. Create a branch in your fork and make a focused change.
2. Explain the problem and how your change improves the behavior.
3. Add a regression test when fixing behavior that can be tested meaningfully. Include before/after screenshots for visual changes.
4. Run the checks below and report any platform checks you could not run.
5. Open a PR against `main`. Link related issues and describe how you verified the change.

```sh
cargo fmt --check
cargo test --locked
cargo build --release --locked
```

Discuss larger features or architecture changes in an issue first. Keep small fixes easy to review, preserve existing user data, and avoid unrelated formatting changes.

## Reporting a bug

Include your OS and architecture, the commit you built, reproduction steps, expected and actual behavior, and relevant logs or screenshots. Use a small synthetic example where possible; remove secrets and private project data before posting.

For performance reports, include debug versus release build, repository size, open files, terminal sessions, measurement method, and the workload. Memory numbers should be reproducible and comparisons should use equivalent workloads.

## Optional search timing comparison

With `rg` installed for the baseline only, compare release-mode search on a real repository:

```sh
GLIMCODE_BENCH_ROOT=/path/to/project GLIMCODE_BENCH_QUERY=main cargo test --release --locked search_benchmark -- --ignored --nocapture
```

The output reports CLI completion, embedded completion, first embedded result, hit count, and summed syntax-parser time across workers. The embedded path includes classification and the 2,000-result cap; the CLI baseline emits all matches without classification. These are diagnostic timings, not equivalent-workload performance claims. The benchmark is ignored in normal test runs.

## Source map

- `src/app.rs`: desktop workspace and editor integration
- `src/navigation.rs`, `src/file_find.rs`, `src/search_view.rs`: navigation and search
- `src/highlighting.rs`, `src/folding.rs`, `src/editor_structure.rs`: reading code
- `src/document.rs`, `src/sheet.rs`: buffers, saving, and CSV/TSV editing
- `src/git.rs`, `src/git_view.rs`, `src/diff.rs`: Git review
- `src/terminal.rs`, `src/preferences.rs`: terminal and preferences
- `src/chrome.rs`: shared visual components
- `vendor/egui-data-table/`: upstream table library with documented local patches

Keep upstream license notices intact when changing vendored code. Contributions are provided under the repository’s MIT license.
