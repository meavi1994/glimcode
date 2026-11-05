# egui-winit

[![Latest version](https://img.shields.io/crates/v/egui-winit.svg)](https://crates.io/crates/egui-winit)
[![Documentation](https://docs.rs/egui-winit/badge.svg)](https://docs.rs/egui-winit)
![MIT](https://img.shields.io/badge/license-MIT-blue.svg)
![Apache](https://img.shields.io/badge/license-Apache-blue.svg)

This crates provides bindings between [`egui`](https://github.com/emilk/egui) and [`winit`](https://crates.io/crates/winit).

The library translates winit events to egui, handled copy/paste, updates the cursor, open links clicked in egui, etc.

Glimcode timing capture: when `GLIMCODE_PROFILE_PATH` is set, native key presses,
mouse presses/wheel events and IME commits store an arrival timestamp and count
in the egui context. The running application's profiler consumes them. Key
contents are never stored by this hook. The monitor's reported refresh rate is
also exposed when available. With capture disabled the hook only checks a cached
boolean. See `docs/PERFORMANCE.md` in the application repository for metric scope.
