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

## Publishing macOS binaries

The [macOS release workflow](.github/workflows/release.yml) builds and tests native Apple Silicon and Intel executables on GitHub-hosted macOS runners. Both downloads target macOS 15 or later. It packages the binary, README, user guide, images, and license into separate archives, generates `SHA256SUMS`, and publishes a GitHub release only after both builds succeed. Binaries use ad-hoc signatures; Developer ID signing and notarization are not configured.

To publish a version, update the package version in `Cargo.toml` and its entry in `Cargo.lock` if needed, review `.github/RELEASE_NOTES.md`, and commit the changes. Tag that commit with the matching version, then push the commit and tag:

```sh
git tag v0.1.0
git push origin main v0.1.0
```

Use a new version for each release. The tag must include `.github/workflows/release.yml`. Watch **Actions → macOS release** for build failures; downloads appear under **Releases** after publication. Failed builds do not publish a partial release. If publication fails after creating a draft, rerun the failed job; it reuses the draft and uploads the assets again.

To check packaging locally on macOS after a release build:

```sh
bash scripts/package-macos.sh target/release/glimcode dist
tar -tzf dist/glimcode-macos-arm64.tar.gz
```

Replace `arm64` with `x86_64` on an Intel Mac. The script reads the architecture from the executable, and generated files in `dist/` are ignored by Git.

## Reporting a bug

Include your OS and architecture, the commit you built, reproduction steps, expected and actual behavior, and relevant logs or screenshots. Use a small synthetic example where possible; remove secrets and private project data before posting.

For performance reports, include debug versus release build, repository size, open files, terminal sessions, measurement method, and the workload. Memory numbers should be reproducible and comparisons should use equivalent workloads.

## Optional search timing comparison

With `rg` installed for the baseline only, compare release-mode search on a real repository:

```sh
GLIMCODE_BENCH_ROOT=/path/to/project GLIMCODE_BENCH_QUERY=main cargo test --release --locked search_benchmark -- --ignored --nocapture
```

The output reports CLI completion, embedded completion, first embedded result, hit count, and summed syntax-parser time across workers. The embedded path includes classification and the 2,000-result cap; the CLI baseline emits all matches without classification. These are diagnostic timings, not equivalent-workload performance claims. The benchmark is ignored in normal test runs.

## Large-file performance check

Run the synthetic 70,000-line TypeScript editing check (about 3 million characters):

```sh
cargo test --release --locked large_typescript_editor_benchmark -- --ignored --nocapture
```

It reports first-frame layout, idle frames, typing, completed syntax layout, and jumping to the end of the file. It also checks that editing preserves the full buffer and that navigation uses the correct Unicode character offsets. Timings vary by machine; this is a regression check, not a performance guarantee for every source file.

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
