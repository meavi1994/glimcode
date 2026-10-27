# Terminal dependency patches

`vt100` is based on crates.io version 0.16.2 (MIT; see its LICENSE). Local changes add retained-row access with stable row numbers, small visible-screen snapshots, cell-associated OSC 8 hyperlink IDs, and strike/conceal attributes. Hyperlink targets are bounded to 4096 entries of at most 4096 bytes. Cell size remains bounded to 34 bytes; URLs are not duplicated per cell. Keep the dependency's original source and attribution when updating it.

`egui-winit` is based on crates.io version 0.33.3 (MIT OR Apache-2.0; distributed here under Apache-2.0). The keyboard paste branch emits an empty `egui::Event::Paste` when the clipboard contains no text, allowing the terminal to handle image-only clipboard contents. Other widgets continue to receive an ordinary empty paste. No clipboard image is read until the user requests a paste.

Regression tests for these patches live in `src/terminal.rs` and `src/terminal/view.rs` and run with the application's test suite. Recheck the keyboard-paste branch when upgrading egui-winit; if upstream adds image paste events, remove the local patch and use those events.
