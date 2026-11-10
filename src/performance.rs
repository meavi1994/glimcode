//! Opt-in timing capture. No source text or key contents are recorded.
use eframe::{egui, glow::HasContext as _};
use std::{
    io::{BufWriter, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub struct Profiler {
    tx: mpsc::SyncSender<serde_json::Value>,
    dropped: Arc<AtomicU64>,
    started: Instant,
    previous: Option<Instant>,
    sequence: u64,
    continuous: bool,
    gpu_wait: bool,
    seconds: Option<f64>,
}

impl Profiler {
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("GLIMCODE_PROFILE_PATH")?;
        Some(Self::new(PathBuf::from(path)))
    }

    fn new(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::sync_channel::<serde_json::Value>(2048);
        std::thread::spawn(move || {
            let result = (|| -> std::io::Result<()> {
                let mut output = BufWriter::new(std::fs::File::create(&path)?);
                for record in rx {
                    serde_json::to_writer(&mut output, &record)?;
                    output.write_all(b"\n")?;
                    output.flush()?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                eprintln!(
                    "Could not write performance trace {}: {error}",
                    path.display()
                );
            }
        });
        Self {
            tx,
            dropped: Arc::default(),
            started: Instant::now(),
            previous: None,
            sequence: 0,
            continuous: std::env::var("GLIMCODE_PROFILE_CONTINUOUS").is_ok_and(|v| v == "1"),
            gpu_wait: std::env::var("GLIMCODE_PROFILE_GPU_WAIT").is_ok_and(|v| v == "1"),
            seconds: std::env::var("GLIMCODE_PROFILE_SECONDS")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v > 0.0),
        }
    }

    pub fn frame(
        &mut self,
        ctx: &egui::Context,
        start: Instant,
        files: usize,
        indexing: bool,
        searching: bool,
        terminal: bool,
    ) {
        let native: Option<(Instant, u64)> = ctx.data_mut(|data| {
            let id = egui::Id::new("glimcode-native-input-timing");
            let value = data.get_temp(id);
            data.remove::<(Instant, u64)>(id);
            value
        });
        let monitor_hz =
            ctx.data(|data| data.get_temp::<f32>(egui::Id::new("glimcode-monitor-hz")));
        let interval = self
            .previous
            .replace(start)
            .map(|previous| milliseconds(start.saturating_duration_since(previous)));
        self.sequence += 1;
        let record = serde_json::json!({
            "schema": 1, "frame": self.sequence,
            "elapsed_ms": milliseconds(start.saturating_duration_since(self.started)),
            "app_cpu_ms": milliseconds(start.elapsed()), "frame_interval_ms": interval,
            "input_events": native.map_or(0, |(_, count)| count),
            "input_to_ui_ms": native.map(|(at, _)| milliseconds(start.saturating_duration_since(at))),
            "monitor_hz": monitor_hz, "continuous": self.continuous, "gpu_wait": self.gpu_wait,
            "indexed_files": files, "indexing": indexing, "searching": searching, "terminal_visible": terminal,
        });
        let tx = self.tx.clone();
        let dropped = self.dropped.clone();
        let gpu_wait = self.gpu_wait;
        // A last-layer callback measures renderer submission. Optional glFinish
        // measures GPU completion, before window swap and physical scanout.
        let callback = eframe::egui_glow::CallbackFn::new(move |_info, painter| {
            let mut record = record.clone();
            record["input_to_render_callback_ms"] =
                serde_json::json!(native.map(|(at, _)| milliseconds(at.elapsed())));
            record["ui_to_render_callback_ms"] = serde_json::json!(milliseconds(start.elapsed()));
            if gpu_wait {
                unsafe {
                    painter.gl().finish();
                }
                record["input_to_gpu_complete_ms"] =
                    serde_json::json!(native.map(|(at, _)| milliseconds(at.elapsed())));
                record["ui_to_gpu_complete_ms"] = serde_json::json!(milliseconds(start.elapsed()));
            }
            record["dropped_records"] = serde_json::json!(dropped.load(Ordering::Relaxed));
            if tx.try_send(record).is_err() {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        });
        ctx.layer_painter(egui::LayerId::new(
            egui::Order::Debug,
            egui::Id::new("performance-frame-end"),
        ))
        .add(egui::PaintCallback {
            rect: ctx.content_rect(),
            callback: Arc::new(callback),
        });
        if self.continuous {
            ctx.request_repaint();
        }
        if self
            .seconds
            .is_some_and(|seconds| self.started.elapsed().as_secs_f64() >= seconds)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_capture_has_no_key_or_document_contents() {
        let dir = tempfile::tempdir().unwrap();
        let mut profiler = Profiler::new(dir.path().join("trace.jsonl"));
        let ctx = egui::Context::default();
        let output = ctx.run(Default::default(), |ctx| {
            ctx.data_mut(|data| {
                data.insert_temp(
                    egui::Id::new("glimcode-native-input-timing"),
                    (Instant::now(), 3_u64),
                )
            });
            profiler.frame(ctx, Instant::now(), 2_000_000, true, true, true);
        });
        assert!(
            ctx.data(|data| data
                .get_temp::<(Instant, u64)>(egui::Id::new("glimcode-native-input-timing")))
                .is_none()
        );
        assert!(
            output
                .shapes
                .iter()
                .any(|shape| matches!(shape.shape, egui::Shape::Callback(_)))
        );
    }
}

#[cfg(test)]
pub fn thread_cpu_time() -> Option<Duration> {
    #[cfg(unix)]
    {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } == 0 {
            return Some(Duration::new(time.tv_sec as u64, time.tv_nsec as u32));
        }
    }
    None
}
