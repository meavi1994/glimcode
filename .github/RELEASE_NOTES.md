## Changes in v0.1.1

- Improved responsiveness for large source files with background syntax and outline analysis, cached text layouts, and deferred Git annotations.
- Fixed stack overflows when collecting symbols from deeply nested TypeScript syntax.
- Unified Git status colors in Project and Changes: committed changes are green, uncommitted changes amber, and deletions red.
- Added independent Editor, Dialogs, Terminal, Search results, and Search filters font controls, all defaulting to 14 pt. Search rows and dialog controls adapt to larger fonts.
- Fixed the Linux CI clipboard test to use the terminal copy shortcut and verify Ctrl+C remains a shell interrupt.

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
