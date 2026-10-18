# Local changes to egui_commonmark 0.22.0

- Add a read-only `CommonMarkViewer::show_events` entry point for an externally
  parsed event stream. Existing source-string and mutable-source APIs keep their
  upstream behavior.
- Share the native rendering loop between parsed events and the original parser.

Glimcode supplies Comrak AST events through this entry point. Pulldown-cmark is
retained as a compatibility event vocabulary for the native widget backend, not
as the parser of Glimcode previews. Upstream MIT and Apache license files are
retained alongside the source.
