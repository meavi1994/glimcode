## Changes in v0.1.2

- Search results are syntax colored. Every file behind the visible rows is highlighted once on a background worker, and each row shows the colors of the line it came from with the match marked. Rows use the monospaced editor font, so a hit reads as code instead of as plain text.
- Click-to-navigate now ranks by the type you are calling through. A method of the receiver's type wins over a same-named method elsewhere, `self.` calls prefer the type around the click, and mentions inside comments and strings never win. The ranking is recomputed on every click and follows the code as it changes.
- Fixed a right aligned shell prompt, such as a clock in the corner, appearing behind the prompt instead of at the right edge of the terminal. The shell now starts with the size of the terminal, so it builds its prompt for the width it actually has.
- The editor keeps one copy of a document in memory instead of three, the undo history of a large file is capped near a megabyte and released when the tab closes, and the files read for a click stay inside a fixed byte budget.
- The terminal paints one text run per row instead of one per cell, the status bar no longer walks the whole document on every frame, and the document is fingerprinted once per frame instead of three times.
- Fixed the search dialog and the file picker keeping the keyboard after they closed: the editor now gets the focus back, so typing and the editing shortcuts work again.
- Fixed deleting a file leaving the wrong tab active.
- The status bar reports the real line ending of a file instead of always claiming LF.
- Removed the theme buttons from the View menu. The workspace chrome is built for a single dark theme, and the light option left the panels unreadable.

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
