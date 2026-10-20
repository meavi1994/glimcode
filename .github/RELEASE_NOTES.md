## Changes in v0.1.3

- Fixed the case-insensitive file and value filter. A term with overlapping prefixes, such as `aab`, was not found in `aaab`, so the explorer filter and the CSV value filter hid rows that do match.
- Fixed background terminal tabs being resized on every frame. The command queue filled up, the hidden shell redrew in a loop, and the user's own input was pushed to the back of the line.
- Fixed copying a terminal selection that reaches past the last column of the screen, which crashed the editor.
- Fixed a search that started while a click was still being ranked: the status bar reported the old token and the selection jumped to an unrelated row.
- Fixed the results of the previous search staying on screen after the search term was cleared.
- Fixed a crash when a tab was closed and a queued action still referred to it.
- Fixed the Markdown preview aborting the editor for a document whose path could not be resolved to an absolute path.
- Fixed grid edits in a CSV being reported as a change on disk, and the whole file being copied on every frame.
- Click-to-navigate now ranks vendored, generated and test files last only when the click does not happen inside such a file. The declaration above the click is weighted by its distance, and a file next to the clicked one is preferred.
- The syntax colors of a document are computed once per change and shared by the editor, the Structure panel and the folding, instead of being copied and hashed several times per frame.
- A search hands over only the buffers that changed, and the worker reads the rest, so a search no longer freezes the interface on large open files. A click does not copy the file it happens in either.
- The Go highlighter walks the tree with a cursor instead of recursion, which could abort the editor on a deeply nested generated file, and looks up shadowed names by name.
- Files without a match are no longer parsed for the search classification.
- The file picker marks the part of the search term that is in the row, so a path such as `src/app` marks the file name it found, and the highlight now matches the case-insensitive filter.
- Find in Files fits into a small viewport instead of hanging off the window, Enter no longer reaches the editor behind an empty result list, and the duplicated Alt+F12 shortcut is resolved.

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
