## Changes in v0.2.0

- Added Git worktree creation, safe removal, and switching. Each workspace keeps its unsaved buffers, editor tabs, navigation history, and live terminals while the app is running.
- Reworked workspace navigation with folder-first cards, branch labels, active and unsaved indicators, visible action menus, and a creation button above the scrolling list. New worktrees default to a sibling folder and a branch matching their name.
- Refreshed the editor with a slate palette, clearer selected tabs, consistent controls, a useful welcome screen, and an active indentation guide for the current code scope.
- Unified project files and changes into a right-side Files panel. Comparison controls start collapsed; expanding them reveals a full-width branch selector with bounded, searchable choices and long-name tooltips.
- Added persistent folder sessions under `~/.glimcode/projects/`, shared settings in `~/.glimcode/settings.json`, and last-folder state in `~/.glimcode/state.json`. Existing `~/.coder` settings migrate while retaining the old file and unknown settings keys.
- Reopening a folder restores file tabs, active tab, cursor locations, panel visibility, and terminal tabs. Restored terminals start fresh shells on demand in their reported or launch directories. Shell commands are never replayed. Unsaved buffers, running processes, and terminal scrollback are not restored after quitting.
- Improved Git review with selectable source text, hunk reversion into editor buffers, synchronized comparisons, and changed-file navigation.
- Reduced work during workspace rendering by removing worktree-list clones and moving path normalization off the render loop. Added regression coverage for session persistence, narrow comparison controls, workspace menus, scope guides, and panel resizing.

## Installation

Download Glimcode for macOS 15 (Sequoia) or later. Rust is not required to run these binaries.

| Mac | Download |
| --- | --- |
| Apple Silicon (M1 and later) | `glimcode-macos-arm64.tar.gz` |
| Intel | `glimcode-macos-x86_64.tar.gz` |

Extract the archive, then run `./glimcode /path/to/project` from the extracted directory. To install the command for use from any directory:

```sh
mkdir -p "$HOME/.local/bin"
install -m 755 ./glimcode "$HOME/.local/bin/glimcode"
```

Add `export PATH="$HOME/.local/bin:$PATH"` to `~/.zshrc` (zsh) or `~/.bashrc` (Bash), then reload that file. See the bundled README or the repository's installation guide for full steps and usage examples.

The binaries have an ad-hoc code signature, but are not Apple Developer ID signed or notarized. If macOS blocks the first launch, use System Settings → Privacy & Security → Open Anyway after attempting to run the binary, if you trust this download.

`SHA256SUMS` contains checksums for both archives. With the archives and checksum file in the same directory, run `shasum -a 256 -c SHA256SUMS` to verify them (an archive you did not download will be reported as missing).
