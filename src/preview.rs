use crate::{chrome, highlighting};
#[cfg(test)]
use eframe::egui;
use eframe::egui::text::LayoutJob;
use std::{
    collections::VecDeque,
    ops::Range,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
};

/// Keep the full source and its syntax styles, while laying out only visible lines.
pub struct SourcePreview {
    pub path: PathBuf,
    source: String,
    lines: Vec<Range<usize>>,
    syntax: LayoutJob,
    pub dark: bool,
    selected: Option<(usize, usize)>,
    pub width: f32,
}

impl SourcePreview {
    pub fn build(
        path: PathBuf,
        source: String,
        dark: bool,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Self> {
        let mut offset = 0;
        let mut lines = vec![];
        let mut longest = 0;
        for line in source.split_inclusive('\n') {
            if cancelled() {
                return None;
            }
            let content = line.trim_end_matches(['\r', '\n']);
            lines.push(offset..offset + content.len());
            longest = longest.max(content.chars().count());
            offset += line.len();
        }
        if source.is_empty() || source.ends_with('\n') {
            lines.push(source.len()..source.len());
        }
        let syntax = highlighting::background(
            &source,
            path.extension().and_then(|e| e.to_str()).unwrap_or("txt"),
            dark,
            cancelled,
        )?;
        Some(Self {
            path,
            source,
            lines,
            syntax,
            dark,
            selected: None,
            width: longest as f32 * 8.5 + 70.0,
        })
    }

    pub fn line_for_ui(&self, ui: &eframe::egui::Ui, index: usize, query: &str) -> LayoutJob {
        let mut job = self.line(index, query);
        let size = crate::preferences::editor_font_size(ui.ctx());
        for section in &mut job.sections {
            section.format.font_id.size = size;
        }
        job
    }

    fn bytes(&self) -> usize {
        self.source.len()
            + self.syntax.text.len()
            + self.lines.len() * size_of::<Range<usize>>()
            + self.syntax.sections.len() * size_of::<eframe::egui::text::LayoutSection>()
    }

    pub fn line_text(&self, index: usize) -> &str {
        self.lines
            .get(index)
            .map_or("", |range| &self.source[range.clone()])
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn select(&mut self, line: usize, column: usize) -> Option<f32> {
        let target = (line, column);
        if self.selected == Some(target) {
            return None;
        }
        self.selected = Some(target);
        Some(line.saturating_sub(4) as f32 * 20.0)
    }

    pub fn line(&self, index: usize, query: &str) -> LayoutJob {
        let range = &self.lines[index];
        let mut job = LayoutJob {
            text: self.source[range.clone()].to_owned(),
            ..Default::default()
        };
        let first = self
            .syntax
            .sections
            .partition_point(|section| section.byte_range.end <= range.start);
        for section in self.syntax.sections[first..]
            .iter()
            .take_while(|section| section.byte_range.start < range.end)
        {
            let mut part = section.clone();
            part.byte_range = part.byte_range.start.max(range.start) - range.start
                ..part.byte_range.end.min(range.end) - range.start;
            job.sections.push(part);
        }
        chrome::mark_matches(&mut job, query);
        job
    }
}

struct Request {
    version: u64,
    path: PathBuf,
    source: Option<String>,
    dark: bool,
}

/// A single cancellable worker and a small LRU cache keep category changes off the UI thread.
pub struct PreviewService {
    tx: Sender<Request>,
    rx: Receiver<(u64, SourcePreview)>,
    version: Arc<AtomicU64>,
    pending: Option<(PathBuf, bool)>,
    pub current: Option<SourcePreview>,
    cache: VecDeque<SourcePreview>,
}

impl Default for PreviewService {
    fn default() -> Self {
        let (tx, requests) = mpsc::channel::<Request>();
        let (results, rx) = mpsc::channel();
        let version = Arc::new(AtomicU64::new(0));
        let latest = version.clone();
        std::thread::spawn(move || {
            while let Ok(mut request) = requests.recv() {
                for next in requests.try_iter() {
                    request = next;
                }
                let cancelled = || latest.load(Ordering::Relaxed) != request.version;
                if cancelled() {
                    continue;
                }
                let source = request.source.unwrap_or_else(|| {
                    crate::navigation::read_text(&request.path)
                        .unwrap_or_else(|error| format!("Preview unavailable: {error}"))
                });
                if let Some(preview) =
                    SourcePreview::build(request.path, source, request.dark, &cancelled)
                    && !cancelled()
                    && results.send((request.version, preview)).is_err()
                {
                    break;
                }
            }
        });
        Self {
            tx,
            rx,
            version,
            pending: None,
            current: None,
            cache: VecDeque::new(),
        }
    }
}

impl PreviewService {
    pub fn needs(&self, path: &std::path::Path, dark: bool) -> bool {
        !self
            .current
            .as_ref()
            .is_some_and(|p| p.path == path && p.dark == dark)
            && !self
                .pending
                .as_ref()
                .is_some_and(|(p, d)| p == path && *d == dark)
    }

    fn keep(&mut self, preview: SourcePreview) {
        self.cache.push_front(preview);
        while self.cache.len() > 4
            || self.cache.iter().map(SourcePreview::bytes).sum::<usize>() > 32 * 1024 * 1024
        {
            self.cache.pop_back();
        }
    }

    pub fn request(&mut self, path: PathBuf, source: Option<String>, dark: bool) {
        let version = self.version.fetch_add(1, Ordering::Relaxed) + 1;
        let cached = self
            .cache
            .iter()
            .position(|p| p.path == path && p.dark == dark)
            .and_then(|i| self.cache.remove(i));
        if let Some(current) = self.current.take() {
            self.keep(current);
        }
        if let Some(mut preview) = cached {
            preview.selected = None;
            self.current = Some(preview);
            self.pending = None;
        } else {
            self.pending = Some((path.clone(), dark));
            let _ = self.tx.send(Request {
                version,
                path,
                source,
                dark,
            });
        }
    }

    pub fn poll(&mut self) {
        for (version, preview) in self.rx.try_iter() {
            if version == self.version.load(Ordering::Relaxed) {
                self.current = Some(preview);
                self.pending = None;
            }
        }
    }

    pub fn loading(&self) -> bool {
        self.pending.is_some()
    }

    pub fn clear(&mut self) {
        self.version.fetch_add(1, Ordering::Relaxed);
        self.pending = None;
        // Release large syntax trees/layouts away from the UI thread.
        let old = (self.current.take(), std::mem::take(&mut self.cache));
        if old.0.is_some() || !old.1.is_empty() {
            std::thread::spawn(move || drop(old));
        }
    }
}

impl Drop for PreviewService {
    fn drop(&mut self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wait(service: &mut PreviewService) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while service.loading() {
            service.poll();
            assert!(
                std::time::Instant::now() < deadline,
                "preview worker did not complete"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn preview_cache_returns_previous_files_without_reloading() {
        let mut service = PreviewService::default();
        let a = PathBuf::from("a.rs");
        let b = PathBuf::from("b.rs");
        service.request(a.clone(), Some("fn a() {}".into()), true);
        wait(&mut service);
        service.request(b, Some("fn b() {}".into()), true);
        wait(&mut service);
        service.request(a.clone(), None, true);
        assert!(
            !service.loading(),
            "cached previews must be available synchronously"
        );
        assert_eq!(service.current.as_ref().unwrap().path, a);
        assert_eq!(
            service.current.as_ref().unwrap().line(0, "").text,
            "fn a() {}"
        );
    }

    #[test]
    fn superseded_preview_cannot_replace_latest_selection() {
        let mut service = PreviewService::default();
        service.request(
            PathBuf::from("slow.rs"),
            Some("let old = 1;\n".repeat(50_000)),
            true,
        );
        service.request(
            PathBuf::from("latest.rs"),
            Some("fn latest() {}".into()),
            true,
        );
        wait(&mut service);
        assert_eq!(
            service.current.as_ref().unwrap().path,
            PathBuf::from("latest.rs")
        );
        assert!(
            SourcePreview::build(
                PathBuf::from("cancelled.rs"),
                "fn a() {}".into(),
                true,
                &|| true
            )
            .is_none()
        );
    }
    #[test]
    fn keeps_every_line_and_only_jumps_when_selection_changes() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let source = (1..=1_000)
                    .map(|n| format!("let value_{n} = {n};\n"))
                    .collect::<String>();
                let mut preview = SourcePreview::build(
                    PathBuf::from("a.rs"),
                    source,
                    ui.visuals().dark_mode,
                    &|| false,
                )
                .unwrap();
                assert_eq!(preview.len(), 1_001);
                assert_eq!(
                    preview.line(999, "value_1000").text,
                    "let value_1000 = 1000;"
                );
                assert!(preview.select(500, 5).is_some());
                assert_eq!(
                    preview.select(500, 5),
                    None,
                    "manual scrolling must not be reset each frame"
                );
                assert!(preview.select(900, 5).is_some());
            });
        });
    }
}
