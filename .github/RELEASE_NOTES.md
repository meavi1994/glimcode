## Changes in v0.2.4

- Fixed unnecessary terminal resizes when opening Find or switching terminal tabs. Find now overlays the terminal without changing the running program’s grid.
- Restored window and panel dimensions across IDE restarts, including each project’s terminal panel height before reconnecting persistent zmx sessions.
- Fixed a scrollback reflow crash after resizing an application that uses a scroll region, and preserved history written at different widths.
- Unified desktop and browser resize requests, removed duplicate resizes, and made stopping sharing restore the latest desktop size, including when the terminal panel is hidden.
- Added regression tests for resize ownership, queue pressure, Find, reconnect geometry, scroll regions, and live zmx sharing.

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
