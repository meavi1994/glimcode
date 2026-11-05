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
