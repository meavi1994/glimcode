//! Full-file parsing runs off the UI thread. Rendered text remains editable while it runs.
use crate::{editor_structure::Structure, folding, highlighting};
use eframe::egui::{self, text::LayoutJob};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

pub fn is_large(text: &str) -> bool {
    text.len() >= 512 * 1024 || text.bytes().filter(|b| *b == b'\n').take(10_000).count() == 10_000
}

pub struct Analysis {
    pub key: u64,
    pub syntax: LayoutJob,
    pub structure: Structure,
    pub folds: Vec<folding::Region>,
}

#[derive(Default)]
struct State {
    desired: Option<(u64, Instant)>,
    pending: Option<(u64, Receiver<Option<Analysis>>, Arc<AtomicBool>)>,
    ready: Option<Arc<Analysis>>,
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some((_, _, cancel)) = &self.pending {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

pub fn key(text: &str, extension: &str, dark: bool) -> u64 {
    egui::util::hash((text, extension, dark))
}

#[derive(Default)]
struct AnalysisComputer;
impl egui::cache::ComputerMut<egui::Id, Arc<Mutex<State>>> for AnalysisComputer {
    fn compute(&mut self, _: egui::Id) -> Arc<Mutex<State>> {
        Arc::default()
    }
}

fn state_for_ui(ui: &egui::Ui, path: &Path) -> Arc<Mutex<State>> {
    let id = ui.make_persistent_id(("background-editor-analysis", path));
    ui.ctx().memory_mut(|memory| {
        memory
            .caches
            .cache::<egui::cache::FrameCache<Arc<Mutex<State>>, AnalysisComputer>>()
            .get(id)
    })
}

pub fn get(ui: &egui::Ui, text: &str, path: &Path) -> Option<Arc<Analysis>> {
    let state = state_for_ui(ui, path);
    let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("txt");
    let dark = ui.visuals().dark_mode;
    let key = key(text, extension, dark);
    let mut state = state.lock().unwrap();
    if state.desired.is_none_or(|(old, _)| old != key) {
        state.desired = Some((key, Instant::now()));
        if let Some((old, _, cancel)) = &state.pending
            && *old != key
        {
            cancel.store(true, Ordering::Relaxed);
        }
    }
    if let Some((_, rx, _)) = &state.pending {
        match rx.try_recv() {
            Ok(result) => {
                state.pending = None;
                if let Some(result) = result
                    && result.key == key
                {
                    state.ready = Some(Arc::new(result));
                }
            }
            Err(mpsc::TryRecvError::Disconnected) => state.pending = None,
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }
    if let Some(ready) = &state.ready
        && ready.key == key
    {
        return Some(ready.clone());
    }
    // The first frame shows plain text; typing postpones analysis until input settles.
    if state.pending.is_none()
        && state
            .desired
            .is_some_and(|(_, at)| at.elapsed() >= Duration::from_millis(200))
    {
        let text = text.to_owned();
        let extension = extension.to_owned();
        let ctx = ui.ctx().clone();
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        state.pending = Some((key, rx, cancel.clone()));
        std::thread::spawn(move || {
            let cancelled = || cancel.load(Ordering::Relaxed);
            let result =
                highlighting::background(&text, &extension, dark, &cancelled).and_then(|syntax| {
                    if cancelled() {
                        return None;
                    }
                    let structure = Structure::build(&syntax);
                    if cancelled() {
                        return None;
                    }
                    let folds = folding::regions(&text, &structure, &extension);
                    (!cancelled()).then_some(Analysis {
                        key,
                        syntax,
                        structure,
                        folds,
                    })
                });
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    ui.ctx().request_repaint_after(Duration::from_millis(100));
    None
}

#[derive(Clone)]
pub struct CachedLayout {
    pub key: u64,
    pub galley: Arc<egui::Galley>,
}

#[derive(Default)]
struct LayoutComputer;
impl egui::cache::ComputerMut<egui::Id, Arc<Mutex<Option<CachedLayout>>>> for LayoutComputer {
    fn compute(&mut self, _: egui::Id) -> Arc<Mutex<Option<CachedLayout>>> {
        Arc::default()
    }
}

/// Cache only actively rendered files; closing or leaving a tab releases large galleys.
pub fn layout_cache(ui: &egui::Ui, id: egui::Id) -> Arc<Mutex<Option<CachedLayout>>> {
    ui.ctx().memory_mut(|memory| {
        memory
            .caches
            .cache::<egui::cache::FrameCache<Arc<Mutex<Option<CachedLayout>>>, LayoutComputer>>()
            .get(id)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaving_a_file_evicts_analysis_and_cancels_pending_work() {
        let ctx = egui::Context::default();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut weak = std::sync::Weak::new();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let state = state_for_ui(ui, Path::new("large.ts"));
                let (_tx, rx) = mpsc::channel();
                state.lock().unwrap().pending = Some((0, rx, cancel.clone()));
                weak = Arc::downgrade(&state);
            });
        });
        for _ in 0..2 {
            let _ = ctx.run(Default::default(), |_| {});
        }
        assert!(weak.upgrade().is_none());
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn first_frame_defers_work_and_stale_results_cannot_replace_current_text() {
        let ctx = egui::Context::default();
        let path = Path::new("large.ts");
        let text = "export const value = 1;\n";
        let mut state = None;
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                assert!(get(ui, text, path).is_none());
                state = Some(state_for_ui(ui, path));
            });
        });
        let state = state.unwrap();
        assert!(
            state.lock().unwrap().pending.is_none(),
            "no parsing on the first frame"
        );
        let old_key = key(text, "ts", ctx.style().visuals.dark_mode);
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        tx.send(Some(Analysis {
            key: old_key,
            syntax: LayoutJob::default(),
            structure: Structure::default(),
            folds: vec![],
        }))
        .unwrap_or_else(|_| panic!("receiver available"));
        state.lock().unwrap().pending = Some((old_key, rx, cancel.clone()));
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                assert!(get(ui, "export const changed = 2;\n", path).is_none());
            });
        });
        assert!(cancel.load(Ordering::Relaxed));
        assert!(state.lock().unwrap().ready.is_none());
    }

    #[test]
    fn large_file_detection_covers_long_lines_and_many_short_lines() {
        assert!(is_large(&"x".repeat(512 * 1024)));
        assert!(is_large(&"x\n".repeat(10_000)));
        assert!(!is_large("const value = 1;\n"));
    }
}
