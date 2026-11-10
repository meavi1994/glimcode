# Performance checks

The acceptance workload is terminal-first interaction at 120 Hz, with 1,000–5,000-line source files and a repository containing two million files. The complete frame budget is 8.33 ms. Report p50, p95, p99 and maximum times; an average alone can hide visible stalls.

## Reproducible component and combined checks

Run benchmarks separately in a release build, so unrelated benchmark jobs do not compete for CPU:

```sh
cargo test --release source_workload_benchmark -- --ignored --nocapture
cargo test --release terminal_input_workload_benchmark -- --ignored --nocapture
cargo test --release terminal_pty_latency_benchmark -- --ignored --nocapture
cargo test --release busy_terminal_input_benchmark -- --ignored --nocapture
cargo test --release two_million_file_tree_benchmark -- --ignored --nocapture
cargo test --release combined_ide_stress_benchmark -- --ignored --nocapture
```

The source check measures the first visible viewport, frames during background geometry preparation, and typing after preparation. It warms application fonts before opening each file. The first viewport appears while full cursor geometry is prepared with independent fonts on a worker. Typed input received during preparation is replayed once when the editor is ready. Visible rows receive glyph meshes from the UI's own atlas; geometry from the worker's separate atlas is never painted. This preserves Unicode cursor positions and avoids constructing offscreen meshes on the UI thread.

The terminal checks measure enqueue/render time and real PTY replies, including continuous output and a blocked metadata query. The output generator requires Python 3. These are direct PTY checks; shell prompts, multiplexer transport and applications can contribute their own latency.

The combined check draws the complete IDE with a 5,000-line document, searches 2,000 real temporary files repeatedly, builds a synthetic 2,000,001-file directory index, and types into a real PTY producing continuous output at a target cadence of 120 inputs per second. It verifies that the jobs overlap, all input is acknowledged, and the editor document remains unchanged. It saves p50/p95/p99/max results to `target/performance/combined-stress.json`. The two-million-file index is synthetic: this does not claim to measure traversing two million real filesystem entries.

## Capture a running IDE

Timing capture is opt-in and writes JSON Lines on a background writer. It records counts, durations and workload flags; it does not record typed characters, shell commands, filenames or source contents.

```sh
cargo build --release --locked
GLIMCODE_PROFILE_PATH=/tmp/glimcode-live.jsonl \
  ./target/release/glimcode /path/to/repository
```

While the IDE runs, type in the terminal, scroll, switch tabs, open a large source file, and start a project search. Summarize the capture after exiting:

```sh
python3 scripts/summarize-profile.py /tmp/glimcode-live.jsonl
python3 scripts/summarize-profile.py /tmp/glimcode-live.jsonl --skip-frames 20
```

The first report includes startup; the second explicitly excludes the first twenty frames. A report with no native input samples cannot establish input latency.

Available environment options:

| Option | Effect |
| --- | --- |
| `GLIMCODE_PROFILE_PATH` | Enable capture and choose the output JSONL file. |
| `GLIMCODE_PROFILE_GPU_WAIT=1` | Add `glFinish` in the last paint callback to record GPU completion before swap. This introduces a GPU wait; also capture without it to assess normal pacing. |
| `GLIMCODE_PROFILE_CONTINUOUS=1` | Request continuous repainting for a separate frame-pacing check. Ordinary interactive capture leaves this unset. |
| `GLIMCODE_PROFILE_SECONDS=15` | Close the profiled window automatically after the chosen number of seconds. |
| `GLIMCODE_PROFILE_ISOLATED=1` | Skip restoring and writing workspace session metadata for this run. Existing preferences are still used. |
| `GLIMCODE_PROFILE_FILE=/absolute/path/to/file` | Open a source file while keeping the repository argument as the project root. |

`app_cpu_ms` measures the application update. `ui_to_render_callback_ms` also includes framework work through the final paint callback. Native input timings start when the OS event reaches winit; GPU timings end before window swap and display scanout. Terminal echo can arrive in a later frame, so these event-to-frame measurements are **not physical key-to-visible-character latency**. Actual key-to-photon validation on the 120 Hz display requires an external high-speed camera or equivalent hardware measurement.

The capture includes the monitor refresh rate when winit reports it, actual intervals during continuous repainting, and dropped trace-record counts. A 120 Hz-capable monitor does not by itself prove that the OS is currently presenting the window at 120 Hz.

## Recorded validation (5 November 2026)

On this Apple M3 Pro, the focused 5,000-line source check showed the first viewport in 0.42 ms, UI frames during geometry preparation up to 2.34 ms, and the typing frame in 2.72 ms. Background preparation still takes wall-clock time; the UI does not wait for all document meshes.

The combined run sampled 511 complete IDE frames, with indexing and search overlapping terminal input in 481 frames. Its p99 frame time was 0.63 ms, maximum 6.23 ms; p99 input-to-parsed-reply time was 0.63 ms. The directory index contained 2,000,001 synthetic paths; the search fixture contained 2,000 real search files plus the open source document.

A separate native run opened the real Kubernetes workspace and its 3,701-line kubelet file. The window reported 120 Hz, and the ignore-aware index contained 31,376 eligible files. After explicitly excluding twenty startup frames, the final capture contained 1,767 frames and 671 native-input frames, with no dropped trace records:

| Native metric | p99 | Maximum |
| --- | --- | --- |
| Application update | 1.36 ms | 3.41 ms |
| UI start to GPU completion | 3.21 ms | 4.97 ms |
| Native input dispatch to GPU completion | 3.36 ms | 4.36 ms |

Continuous redraw intervals had a median of 8.41 ms and p99 of 14.03 ms. These intervals include work outside the measured application/GPU stages and are not presentation timestamps. The capture therefore does not establish that every physical display frame met 8.33 ms, and does not establish physical key-to-photon latency. Input counts include scrolling and mouse input as well as keys; no key contents were recorded.

Local artifacts are saved under `target/performance/`: `combined-stress.json`, `kubernetes-native-final.jsonl`, and the `kubernetes-native-final-all.txt` and `kubernetes-native-final-steady.txt` summaries. These generated captures are not committed. Results depend on the workload and machine; repeat the commands and live capture for subsequent changes.

## Initial screenshot-audit pass (10 November 2026, edc10ec)

The screenshot findings referred to commit `b9525d9`. The current implementation was checked against each item. The changes below reduce repeated work while retaining full Git reconciliation, external-file detection, Unicode editing, terminal history, and folding.

| Finding | Current behavior |
| --- | --- |
| 1–2: full Git refresh after content edits, including all untracked files | Automatic content refreshes use one tracked-only status command and reuse recent branch metadata and untracked results. Refreshes wait at least three seconds after the preceding completion. Membership changes, Git metadata changes, manual refreshes and the thirty-second reconciliation use the complete status path. Deleted-file detection uses one symlink-aware metadata lookup. |
| 3: worktree status every three seconds | Cards refresh at thirty-second intervals measured from completion. Each status command has a five-second timeout and bounded output; unknown status remains unknown. |
| 4: back-to-back workspace walks | Walks never overlap. Event-triggered follow-up walks wait at least two seconds, or the previous walk's duration if longer. Periodic reconciliation runs five minutes after completion when native watching works. Manual refresh remains immediate. |
| 5, 8, 12: repaint loops | Index, search, ranking, preview, geometry, analysis and projection workers wake the UI when results arrive. Debounce timers schedule their actual deadlines. Loading indicators are static; terminal output repaint requests are coalesced to an eight-millisecond deadline. |
| 6–7: terminal parser lock and repeated grid construction | Painting uses a visible snapshot after releasing the parser lock. Cached rows retain text galleys, backgrounds and decorations. Selection, links, find results, geometry and font changes invalidate the relevant cache keys. |
| 9, 13: scrolling and folding | Visible row positions are indexed and binary-searched, including the positions after folds. Only newly visible rows receive meshes. Unchanged viewports reuse composed galleys; folded galleys are cached as well. |
| 10: selection rebuilds layout | Bounded, single-line selection queries use revision/range caches and sparse character checkpoints. Occurrence decorations scan visible rows and do not enter the text-layout key. Select-all does not become an occurrence query. |
| 11: repeated typing work | Unchanged line jobs and their style/geometry keys are reused. Mutation generations and document revisions replace UI-thread full-buffer hashes. Dirty-state comparisons run on a revision- and baseline-checked worker; undo can still restore the clean state. |
| 14: synchronous parsing of small files | All source sizes use cancellable background analysis after input settles. Syntax, folding and structure share its results. Plain text remains editable while analysis runs. |
| 15: changed-file lists and ignore-rule work | The Git tool shares a generation/dirty-path cache across consumers. The sidebar count reuses its color map. Dirty-path merging uses a hash index, and ignore-file metadata is checked at most every two seconds. |
| 16: terminal-link suffix lookup | Indexing builds a sorted filename index. Suffix resolution narrows to matching basenames before checking ambiguity and performing the final filesystem lookup. |
| 17: file picker scans | All project queries run on cancellable workers. Existing results remain visible during a new query, and Enter waits for the current query instead of opening stale results. |
| 18: other terminal costs | Visible and synchronized snapshots share cell allocations through row-level copy-on-write. Metadata queries are limited to at most two per second per tab. The writer sleeps up to 250 ms when idle and receives explicit wakeups for protocol replies. History search runs on a shared-row snapshot on a worker. Pointer reporting does not clone text/paste events, and keyboard events are moved out of the input queue. |
| 19: Markdown preview | Parsed events are keyed by document revision. Measured top-level blocks reserve offscreen space without cloning their events or laying out their widgets again. Width, scale, theme and content changes invalidate measurements. Nested lists, tables and code blocks stay together. |
| 20: duplicate file-index storage | Large-tree file entries reference the same immutable path vector as the workspace index. Old indices and projections are retired on workers. |
| Lower-priority findings | Watched workspaces use event wakeups and periodic reconciliation; disk metadata checks run on workers. Missing idle files are retried after another idle interval. Hover tokens, filtered outline rows and declaration colors are cached. The synchronous Git calls in the cited CLI preview modes are inside the optional `ui-capture` fixture code, not the ordinary startup path. |

**Historical limits of the initial pass, addressed by the second pass below:** These were bounded-work and caching changes, not a replacement of every underlying data structure. Full filesystem reconciliation still walks and sorts the workspace; an old index remains available while its replacement is built. Egui TextEdit still requires full cursor geometry, and composing changed document geometry still visits rows. Markdown's first measurement, content edits and width changes still require block layout; a single very large top-level block is not subdivided. Git metadata and untracked results can remain stale until the next reconciliation if native events are lost. No physical key-to-photon or real two-million-file filesystem-traversal claim is made by the synthetic checks.

### Validation for this follow-up

`cargo test --offline --no-fail-fast` passed 334 tests, with 22 opt-in tests excluded. The UI-capture feature also compiles. The release source, terminal redraw, busy-PTY, two-million-path picker/tree, and combined IDE checks passed separately.

| Release check | Observed result |
| --- | --- |
| 5,000-line source | First viewport 0.92 ms; typing frame 1.35 ms; next frame 1.09 ms. |
| Warm terminal redraws | Approximately 0.046–0.082 ms at the two tested panel sizes, after initial frames. |
| Busy terminal with input and a blocked metadata query | Maximum sampled UI frame 0.321 ms; maximum input-to-parsed-reply 0.321 ms. |
| Two-million-file tree | Worker build 2.81 s; expanded projection 6.89 ms; sampled fully expanded UI frames 0.039–0.356 ms. |
| Combined IDE | 511 frames, 481 with indexing/search/input overlap; frame p99 0.625 ms, maximum 7.203 ms; input-to-parsed-reply p99 0.628 ms, maximum 7.229 ms. |

The combined index contained 2,000,001 synthetic paths and the search fixture contained 2,000 real search files. These are component/combined CPU and PTY measurements, not display presentation timings. Results vary between runs and machines. The combined report is written to `target/performance/combined-stress.json`; generated test logs and captures are not committed.

## Incremental indexing and rendering pass (10 November 2026)

The remaining hot paths from the initial pass now have incremental implementations:

- **Workspace membership:** native events retain affected paths. Exact file events evaluate inherited ignore rules without enumerating siblings. New directories scan their own subtree; deletions use prefix tombstones. The explorer and picker share the original immutable index plus a delta containing changed paths. Routine creates, renames and deletes do not copy/sort the workspace path vector or rebuild its directory index. Healthy native watchers no longer schedule periodic whole-workspace walks.
- **Reconciliation and memory:** manual refresh, ignore-policy changes, overflow and unavailable watchers still reconcile the filesystem. Reconciliation streams entries against the existing index using compact seen arrays, retaining only changes rather than constructing a second complete index/tree. Large index, search, status and projection allocations are retired on workers. Large changes can promote an initially small explorer to the virtualized tree on a worker.
- **Picker and terminal paths:** paths are normalized once during index construction. Sorted exact/prefix lookups use subtree minima to obtain the highest-priority candidates without enumerating a large prefix range; substring queries intersect n-gram block postings. Short queries use block signatures. The same index resolves terminal-link suffixes, including paths added after startup. Queries retain recent/open-file priority and cancellation.
- **Editor edits:** a balanced row tree stores byte/character offsets, intrinsic bounds and mesh summaries. Edits split/join only affected rows and reuse unchanged syntax jobs, hashes, font geometry and meshes. Newline insertions/deletions update the summaries without reconstructing all line jobs. Egui's flat placed-row coordinates shift when line heights or counts change; this does not reshape those rows. Source revisions with identical text reuse the current galley. Syntax-color updates retain row geometry and update meshes only for visible rows, including when previously offscreen rows enter the viewport. Command-hover uses indexed row offsets and inspects the pointed row, including Unicode characters.
- **Editor scrolling and loading:** scrolling patches newly visible rows in the existing galley instead of recomposing all rows. Redundant previous-frame references are released before mutation; explicitly retained snapshots remain protected by copy-on-write. The initial loading preview receives line positions from the geometry worker instead of rescanning the document every frame.
- **Markdown:** Comrak parsing and large-block geometry run on a worker. A height index skips offscreen blocks before their first UI measurement. Long lists page at balanced item boundaries with correct numbering; long code blocks, prose and tables mesh only visible rows. Prose formatting and links retain their style and hit targets. Native image rendering remains available. Width, scale, theme and source changes invalidate the appropriate work.
- **Git:** ordinary content notifications retain changed paths and use literal, bounded pathspec batches. Unrelated cached changes remain intact. Unchanged scoped results reuse the shared repository file vector without rebuilding its full changed-file map. Native Git metadata notifications trigger full metadata/status refreshes, including linked worktrees; reliable watchers replace periodic full-status polling. Fallback/manual reconciliation remains available. Large status-color maps and missing-file overlays are prepared off the UI thread.
- **External files and ignore policy:** native file notifications invalidate metadata promptly. Active files outside the watched workspace and the global ignore file are checked on a background observer; the UI wakes only for changes. Stale metadata results cannot replace a newer save baseline. Global policy edits invalidate changed-file projections and active search results.

Initialization still has to read the filesystem and build search/geometry indexes. Changes to ignore rules can affect every path, so their reconciliation remains a background traversal. These necessary operations are distinct from repeatedly rebuilding everything for an ordinary edit or scroll. The tests do not establish physical key-to-photon latency or guarantee a presentation deadline on every machine.

### Real filesystem validation

The opt-in `real_two_million_file_workspace_benchmark` created **2,000,000 real empty files**, checked the index and indexed queries, added a file through the incremental path, performed streaming reconciliation, and removed the entire fixture afterward. This is separate from the combined benchmark's synthetic path index.

| Real-filesystem operation | Observed result |
| --- | --- |
| Fixture creation | 58.18 s |
| Initial ignore-aware walk | 6.74 s |
| Shared tree and search-index construction | 7.15 s |
| Single-file incremental update | 0.267 ms |
| Five indexed queries | 0.002–0.209 ms; 0–6,384 candidates examined |
| Full streaming reconciliation | 8.66 s |

The report is saved to `target/performance/real-workspace.json`. Reproduce separately from other benchmarks:

```sh
cargo test --release --offline real_two_million_file_workspace_benchmark -- --ignored --nocapture
```

The correctness suite passes **346 tests**, with 23 opt-in tests excluded. Additional checks cover native event delivery outside the sandbox, randomized Unicode edits and row offsets, line splits/joins, unchanged syntax-row sharing, in-place scrolling, large prose/table rendering, ordered-list pagination, scoped Git status, linked Git metadata, ignore precedence, and worker-based large status projections.

Release checks for this pass recorded a 5,000-line typing frame of **0.269 ms**, its next frame at **0.062 ms**, and warm terminal frames around **0.045–0.077 ms**. The final combined run sampled 512 frames, with search/indexing/input overlapping in 482: frame p99 **0.620 ms**, maximum **1.328 ms**; input-to-parsed-reply p99 **0.623 ms**, maximum **1.330 ms**. These are CPU/PTY measurements; the combined index is synthetic and its search fixture contains 2,000 real files.

An earlier repeat recorded a 10.208 ms maximum despite passing its p99 target. Thread-CPU and layout counters then identified a whole-file syntax-color layout as the slow operation. The final implementation applies syntax colors to visible rows while preserving the existing geometry; the final combined run had no frames above 4 ms. The report includes thread-CPU percentiles and slow-frame layout counters so future regressions can be distinguished from scheduler delays. These samples do not guarantee every frame on every run meets the budget.
