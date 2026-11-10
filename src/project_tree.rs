//! Immutable large-workspace index, background projections and visible-row rendering.
use crate::{
    chrome,
    explorer::{FileAction, context_menu},
};
use eframe::egui::{self, Color32};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

pub const THRESHOLD: usize = 100_000;

struct Entry {
    path: EntryPath,
    folder: bool,
}
enum EntryPath {
    Indexed(Arc<Vec<PathBuf>>, usize),
    Owned(PathBuf),
}
impl Entry {
    fn path(&self) -> &PathBuf {
        match &self.path {
            EntryPath::Indexed(paths, index) => &paths[*index],
            EntryPath::Owned(path) => path,
        }
    }
}
type Index = BTreeMap<PathBuf, Vec<Arc<Entry>>>;
struct Row {
    entry: Arc<Entry>,
    depth: usize,
}
struct Projection {
    rows: Vec<Row>,
    reveal: Option<usize>,
}

pub struct LargeTree {
    root: PathBuf,
    index: Arc<Index>,
    rows: Projection,
    expanded: BTreeSet<PathBuf>,
    all_open: bool,
    extras: Arc<BTreeSet<PathBuf>>,
    delta: Arc<crate::workspace_delta::Delta>,
    scope: Option<(String, bool, bool, u64)>,
    status_revision: u64,
    dirty: Option<Instant>,
    pending: Option<(mpsc::Receiver<Projection>, Arc<AtomicBool>)>,
    reveal: Option<PathBuf>,
}

impl Drop for LargeTree {
    fn drop(&mut self) {
        if let Some((_, cancel)) = &self.pending {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl LargeTree {
    /// Called by the indexing worker, never by a drawing frame.
    pub fn build(
        root: &Path,
        files: &[PathBuf],
        folders: &[PathBuf],
        cancelled: &impl Fn() -> bool,
    ) -> Option<Self> {
        Self::build_shared(
            root,
            Arc::new(files.to_vec()),
            Arc::new(folders.to_vec()),
            cancelled,
        )
    }

    pub fn build_shared(
        root: &Path,
        files: Arc<Vec<PathBuf>>,
        folders: Arc<Vec<PathBuf>>,
        cancelled: &impl Fn() -> bool,
    ) -> Option<Self> {
        let mut index = Index::new();
        let mut directories = BTreeSet::new();
        for (paths, folder) in [(files, false), (folders, true)] {
            for (path_index, path) in paths.iter().enumerate() {
                if cancelled() {
                    return None;
                }
                if !path.starts_with(root) || path == root {
                    continue;
                }
                if folder {
                    directories.insert(path.clone());
                } else if let Some(parent) = path.parent() {
                    index
                        .entry(parent.to_owned())
                        .or_default()
                        .push(Arc::new(Entry {
                            path: EntryPath::Indexed(paths.clone(), path_index),
                            folder: false,
                        }));
                }
                let mut parent = path.parent();
                while let Some(path) = parent.filter(|p| *p != root && p.starts_with(root)) {
                    // Previously visited ancestors already have their own parents.
                    if !directories.insert(path.to_owned()) {
                        break;
                    }
                    parent = path.parent();
                }
            }
        }
        for path in directories {
            if cancelled() {
                return None;
            }
            if let Some(parent) = path.parent() {
                index
                    .entry(parent.to_owned())
                    .or_default()
                    .push(Arc::new(Entry {
                        path: EntryPath::Owned(path.clone()),
                        folder: true,
                    }));
            }
        }
        for entries in index.values_mut() {
            if cancelled() {
                return None;
            }
            entries.sort_unstable_by(|a, b| {
                b.folder.cmp(&a.folder).then_with(|| a.path().cmp(b.path()))
            });
        }
        let rows = index
            .get(root)
            .into_iter()
            .flatten()
            .map(|entry| Row {
                entry: entry.clone(),
                depth: 0,
            })
            .collect();
        Some(Self {
            root: root.to_owned(),
            index: Arc::new(index),
            rows: Projection { rows, reveal: None },
            expanded: BTreeSet::new(),
            all_open: false,
            extras: Arc::default(),
            delta: Arc::default(),
            scope: None,
            status_revision: 0,
            dirty: None,
            pending: None,
            reveal: None,
        })
    }

    fn invalidate(&mut self) {
        if let Some((rx, cancel)) = self.pending.take() {
            cancel.store(true, Ordering::Relaxed);
            // A completed projection may already be queued. Retire its rows
            // on a worker instead of dropping millions of references here.
            std::thread::spawn(move || drop(rx));
        }
        self.dirty = Some(Instant::now());
    }

    pub fn set_delta(&mut self, delta: Arc<crate::workspace_delta::Delta>) {
        self.delta = delta;
        self.invalidate();
    }

    pub fn expand_all(&mut self, open: bool) {
        self.all_open = open;
        self.expanded.clear();
        self.invalidate();
    }

    pub fn reveal(&mut self, path: &Path) {
        for parent in path
            .ancestors()
            .skip(1)
            .take_while(|p| *p != self.root && p.starts_with(&self.root))
        {
            if self.all_open {
                self.expanded.remove(parent);
            } else {
                self.expanded.insert(parent.to_owned());
            }
        }
        self.reveal = Some(path.to_owned());
        self.invalidate();
    }

    pub fn set_shared_extras(&mut self, extras: Arc<BTreeSet<PathBuf>>) {
        if Arc::ptr_eq(&self.extras, &extras) {
            return;
        }
        let old = std::mem::replace(&mut self.extras, extras);
        std::thread::spawn(move || drop(old));
        self.invalidate();
    }

    pub fn set_extras(&mut self, extras: BTreeSet<PathBuf>) {
        if *self.extras != extras {
            self.extras = Arc::new(extras);
            self.invalidate();
        }
    }

    pub fn set_status_revision(&mut self, revision: u64) {
        self.status_revision = revision;
    }

    pub fn adopt(&mut self, old: &mut Self) {
        self.expanded = std::mem::take(&mut old.expanded);
        self.all_open = old.all_open;
        // Retire the fresh default projection with the old tree, off the UI thread.
        std::mem::swap(&mut self.rows, &mut old.rows);
        self.invalidate();
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        active: Option<&Path>,
        filter: &str,
        colors: &BTreeMap<PathBuf, Color32>,
        changed_only: bool,
        hide_tests: bool,
        action: &mut Option<FileAction>,
        diff: &mut Option<PathBuf>,
    ) -> Option<PathBuf> {
        // Ordinary color changes do not require projecting the unfiltered tree again.
        let scope = (
            filter.to_owned(),
            changed_only,
            hide_tests,
            if changed_only {
                self.status_revision
            } else {
                0
            },
        );
        if self.scope.as_ref() != Some(&scope) {
            let initial = self.scope.is_none() && filter.is_empty() && !changed_only && !hide_tests;
            self.scope = Some(scope);
            if !initial {
                self.invalidate();
            }
        }
        if let Some((rx, _)) = &self.pending {
            match rx.try_recv() {
                Ok(rows) => {
                    let old = std::mem::replace(&mut self.rows, rows);
                    std::thread::spawn(move || drop(old));
                    self.pending = None;
                    self.reveal = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.pending = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if self.pending.is_none()
            && self
                .dirty
                .is_some_and(|at| at.elapsed() >= Duration::from_millis(100))
        {
            self.dirty = None;
            let index = self.index.clone();
            let root = self.root.clone();
            let expanded = self.expanded.clone();
            let all_open = self.all_open;
            let extras = self.extras.clone();
            let delta = self.delta.clone();
            let filter = filter.to_owned();
            let changed = changed_only.then(|| colors.keys().cloned().collect::<BTreeSet<_>>());
            let reveal = self.reveal.clone();
            let (tx, rx) = mpsc::channel();
            let cancel = Arc::new(AtomicBool::new(false));
            self.pending = Some((rx, cancel.clone()));
            let ctx = ui.ctx().clone();
            std::thread::spawn(move || {
                if let Some(rows) = project_delta(
                    &index,
                    &root,
                    &expanded,
                    all_open,
                    &extras,
                    &delta,
                    &filter,
                    changed.as_ref(),
                    hide_tests,
                    reveal.as_deref(),
                    &cancel,
                ) {
                    let _ = tx.send(rows);
                    ctx.request_repaint();
                }
            });
        }
        if let Some(at) = self.dirty {
            ui.ctx()
                .request_repaint_after(Duration::from_millis(100).saturating_sub(at.elapsed()));
        }
        let height = 25.0;
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), height * self.rows.rows.len() as f32),
            egui::Sense::hover(),
        );
        if let Some(index) = self.rows.reveal.take() {
            ui.scroll_to_rect(
                egui::Rect::from_min_size(
                    rect.min + egui::vec2(0.0, height * index as f32),
                    egui::vec2(rect.width(), height),
                ),
                Some(egui::Align::Center),
            );
        }
        let visible = visible_rows(rect.top(), ui.clip_rect(), self.rows.rows.len(), height);
        let mut opened = None;
        let mut toggle = None;
        for i in visible {
            let row = &self.rows.rows[i];
            let path = row.entry.path();
            let bounds = egui::Rect::from_min_size(
                rect.min + egui::vec2(0.0, i as f32 * height),
                egui::vec2(rect.width(), height),
            );
            let response = ui.interact(bounds, ui.make_persistent_id(path), egui::Sense::click());
            let selected = active == Some(path.as_path());
            if selected || response.hovered() {
                ui.painter().rect_filled(
                    bounds,
                    4.0,
                    if selected {
                        chrome::SELECTED
                    } else {
                        chrome::HEADER
                    },
                );
            }
            let origin = bounds.min + egui::vec2(18.0 * row.depth as f32, 1.0);
            let icon =
                egui::Rect::from_min_size(origin + egui::vec2(18.0, 0.0), egui::vec2(23.0, 23.0));
            if row.entry.folder {
                let open = self.all_open ^ self.expanded.contains(path) || !filter.is_empty();
                let points = if open {
                    vec![
                        origin + egui::vec2(3.0, 9.0),
                        origin + egui::vec2(13.0, 9.0),
                        origin + egui::vec2(8.0, 15.0),
                    ]
                } else {
                    vec![
                        origin + egui::vec2(5.0, 6.0),
                        origin + egui::vec2(11.0, 12.0),
                        origin + egui::vec2(5.0, 18.0),
                    ]
                };
                ui.painter().add(egui::Shape::convex_polygon(
                    points,
                    chrome::MUTED,
                    egui::Stroke::NONE,
                ));
                chrome::folder(ui.painter(), icon);
            } else {
                ui.put(icon, egui::Label::new(chrome::file_icon(path)));
            }
            ui.painter().text(
                origin + egui::vec2(47.0, 11.5),
                egui::Align2::LEFT_CENTER,
                path.file_name().unwrap_or_default().to_string_lossy(),
                egui::FontId::proportional(13.0),
                colors.get(path).copied().unwrap_or(chrome::TEXT),
            );
            response.context_menu(|ui| {
                if !row.entry.folder
                    && colors.contains_key(path)
                    && ui.button("View diff").clicked()
                {
                    *diff = Some(path.clone());
                    ui.close();
                }
                if let Some(next) = context_menu(ui, &self.root, path, row.entry.folder) {
                    *action = Some(next);
                }
            });
            if response.on_hover_text(path.display().to_string()).clicked() {
                if row.entry.folder {
                    toggle = Some(path.clone());
                } else if colors.get(path) != Some(&crate::git_view::deleted()) {
                    opened = Some(path.clone());
                }
            }
        }
        if let Some(path) = toggle {
            if !self.expanded.remove(&path) {
                self.expanded.insert(path);
            }
            self.invalidate();
        }
        opened
    }
}

fn visible_rows(top: f32, clip: egui::Rect, count: usize, height: f32) -> std::ops::Range<usize> {
    let start = ((clip.top() - top) / height).floor().max(0.0) as usize;
    let end = ((clip.bottom() - top) / height).ceil().max(0.0) as usize;
    start.min(count)..end.min(count)
}

#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn project(
    index: &Index,
    root: &Path,
    expanded: &BTreeSet<PathBuf>,
    all_open: bool,
    extras: &BTreeSet<PathBuf>,
    query: &str,
    changed: Option<&BTreeSet<PathBuf>>,
    hide_tests: bool,
    reveal: Option<&Path>,
    cancel: &AtomicBool,
) -> Option<Projection> {
    project_delta(
        index,
        root,
        expanded,
        all_open,
        extras,
        &crate::workspace_delta::Delta::default(),
        query,
        changed,
        hide_tests,
        reveal,
        cancel,
    )
}

fn project_delta(
    index: &Index,
    root: &Path,
    expanded: &BTreeSet<PathBuf>,
    all_open: bool,
    extras: &BTreeSet<PathBuf>,
    delta: &crate::workspace_delta::Delta,
    query: &str,
    changed: Option<&BTreeSet<PathBuf>>,
    hide_tests: bool,
    reveal: Option<&Path>,
    cancel: &AtomicBool,
) -> Option<Projection> {
    let filter = chrome::CaseFilter::new(query);
    let scoped = !filter.is_empty() || changed.is_some() || hide_tests;
    let mut overlay = Index::new();
    for path in extras {
        if let Some(parent) = path.parent() {
            overlay
                .entry(parent.to_owned())
                .or_default()
                .push(Arc::new(Entry {
                    path: EntryPath::Owned(path.clone()),
                    folder: false,
                }));
        }
        for parent in path
            .ancestors()
            .skip(1)
            .take_while(|p| *p != root && p.starts_with(root))
        {
            let grand = parent.parent()?;
            if !index
                .get(grand)
                .is_some_and(|entries| entries.iter().any(|e| e.path().as_path() == parent))
            {
                overlay
                    .entry(grand.to_owned())
                    .or_default()
                    .push(Arc::new(Entry {
                        path: EntryPath::Owned(parent.to_owned()),
                        folder: true,
                    }));
            }
        }
    }
    for (paths, folder) in [(&delta.files, false), (&delta.folders, true)] {
        for path in paths {
            if let Some(parent) = path.parent() {
                overlay
                    .entry(parent.to_owned())
                    .or_default()
                    .push(Arc::new(Entry {
                        path: EntryPath::Owned(path.clone()),
                        folder,
                    }));
            }
        }
    }
    let mut allowed = BTreeSet::new();
    if scoped {
        for entries in index.values().chain(overlay.values()) {
            for entry in entries {
                if cancel.load(Ordering::Relaxed) {
                    return None;
                }
                if !entry.folder
                    && changed.is_none_or(|paths| paths.contains(entry.path()))
                    && (!hide_tests || !crate::test_files::is_test_file(entry.path()))
                    && filter.matches(
                        &entry
                            .path()
                            .strip_prefix(root)
                            .unwrap_or(entry.path())
                            .to_string_lossy(),
                    )
                {
                    for path in entry.path().ancestors().take_while(|p| *p != root) {
                        allowed.insert(path.to_owned());
                    }
                }
            }
        }
    }
    for entries in overlay.values_mut() {
        entries
            .sort_unstable_by(|a, b| b.folder.cmp(&a.folder).then_with(|| a.path().cmp(b.path())));
        entries.dedup_by(|a, b| a.path() == b.path());
    }
    struct Walker<'a> {
        index: &'a Index,
        delta: &'a crate::workspace_delta::Delta,
        extras: &'a BTreeSet<PathBuf>,
        overlay: &'a Index,
        allowed: Option<&'a BTreeSet<PathBuf>>,
        expanded: &'a BTreeSet<PathBuf>,
        all_open: bool,
        filtered: bool,
        cancel: &'a AtomicBool,
        rows: Vec<Row>,
    }
    impl Walker<'_> {
        fn visit(&mut self, path: &Path, depth: usize) -> Option<()> {
            let mut main = self.index.get(path).into_iter().flatten().peekable();
            let mut extra = self.overlay.get(path).into_iter().flatten().peekable();
            loop {
                if self.cancel.load(Ordering::Relaxed) {
                    return None;
                }
                let entry = match (main.peek(), extra.peek()) {
                    (Some(a), Some(b)) => {
                        match b.folder.cmp(&a.folder).then_with(|| a.path().cmp(b.path())) {
                            std::cmp::Ordering::Less => main.next(),
                            std::cmp::Ordering::Equal => {
                                extra.next();
                                main.next()
                            }
                            std::cmp::Ordering::Greater => extra.next(),
                        }
                    }
                    (Some(_), None) => main.next(),
                    (None, Some(_)) => extra.next(),
                    (None, None) => break,
                }?;
                if self.delta.hidden(entry.path(), entry.folder)
                    && !self.extras.contains(entry.path())
                {
                    continue;
                }
                if self
                    .allowed
                    .is_some_and(|paths| !paths.contains(entry.path()))
                {
                    continue;
                }
                self.rows.push(Row {
                    entry: entry.clone(),
                    depth,
                });
                if entry.folder
                    && (self.all_open ^ self.expanded.contains(entry.path()) || self.filtered)
                {
                    self.visit(entry.path(), depth + 1)?;
                }
            }
            Some(())
        }
    }
    let mut walker = Walker {
        index,
        delta,
        extras,
        overlay: &overlay,
        allowed: scoped.then_some(&allowed),
        expanded,
        all_open,
        filtered: !filter.is_empty(),
        cancel,
        rows: Vec::new(),
    };
    walker.visit(root, 0)?;
    let rows = walker.rows;
    let reveal = reveal.and_then(|path| {
        rows.iter()
            .position(|row| row.entry.path().as_path() == path)
    });
    Some(Projection { rows, reveal })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> LargeTree {
        let root = Path::new("/project");
        LargeTree::build(
            root,
            &[
                root.join("src/main.rs"),
                root.join("src/main_test.rs"),
                root.join("README.md"),
            ],
            &[root.join("empty")],
            &|| false,
        )
        .unwrap()
    }

    #[test]
    fn projections_preserve_order_reveal_filters_and_deleted_entries() {
        let mut tree = fixture();
        assert_eq!(
            tree.rows
                .rows
                .iter()
                .map(|r| r
                    .entry
                    .path()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned())
                .collect::<Vec<_>>(),
            ["empty", "src", "README.md"]
        );
        let target = tree.root.join("src/main.rs");
        tree.reveal(&target);
        let cancel = AtomicBool::new(false);
        let rows = project(
            &tree.index,
            &tree.root,
            &tree.expanded,
            false,
            &BTreeSet::new(),
            "",
            None,
            false,
            Some(&target),
            &cancel,
        )
        .unwrap();
        assert_eq!(rows.rows[rows.reveal.unwrap()].entry.path(), &target);
        assert_eq!(rows.rows[2].depth, 1);
        let rows = project(
            &tree.index,
            &tree.root,
            &BTreeSet::new(),
            false,
            &BTreeSet::new(),
            "main.rs",
            None,
            false,
            None,
            &cancel,
        )
        .unwrap();
        assert_eq!(rows.rows.len(), 2);
        assert_eq!(rows.rows[1].entry.path(), &target);
        let deleted = tree.root.join("removed/old.rs");
        let extras = BTreeSet::from([deleted.clone()]);
        let changed = BTreeSet::from([deleted.clone(), tree.root.join("src/main_test.rs")]);
        let rows = project(
            &tree.index,
            &tree.root,
            &BTreeSet::new(),
            true,
            &extras,
            "",
            Some(&changed),
            true,
            None,
            &cancel,
        )
        .unwrap();
        assert_eq!(rows.rows.len(), 2);
        assert_eq!(rows.rows[1].entry.path(), &deleted);
        cancel.store(true, Ordering::Relaxed);
        assert!(
            project(
                &tree.index,
                &tree.root,
                &BTreeSet::new(),
                true,
                &extras,
                "",
                None,
                false,
                None,
                &cancel
            )
            .is_none()
        );
    }

    #[test]
    fn viewport_work_is_bounded_at_two_million_rows() {
        let clip = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(300.0, 900.0));
        assert_eq!(visible_rows(0.0, clip, 2_000_000, 25.0), 0..36);
        assert_eq!(
            visible_rows(-49_999_100.0, clip, 2_000_000, 25.0),
            1_999_964..2_000_000
        );
        assert_eq!(visible_rows(0.0, clip, 0, 25.0), 0..0);
    }

    #[test]
    fn replacing_the_index_retains_expansion_and_cancels_old_projection() {
        let mut old = fixture();
        old.reveal(&old.root.join("src/main.rs"));
        let mut fresh = fixture();
        fresh.adopt(&mut old);
        assert!(fresh.expanded.contains(&fresh.root.join("src")));
        assert!(fresh.dirty.is_some());
        let (_, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        fresh.pending = Some((rx, cancel.clone()));
        fresh.expand_all(false);
        assert!(cancel.load(Ordering::Relaxed));
    }

    #[test]
    #[ignore = "two-million-file scale benchmark; run with --release --ignored --nocapture"]
    fn two_million_file_tree_benchmark() {
        let root = Path::new("/project");
        let start = Instant::now();
        let files: Vec<_> = (0..2_000_000)
            .map(|i| root.join(format!("dir{:04}/file{:04}.rs", i / 1000, i % 1000)))
            .collect();
        let mut tree = LargeTree::build(root, &files, &[], &|| false).unwrap();
        eprintln!("2,000,000 files: index built in {:?}", start.elapsed());
        assert_eq!(tree.index.values().map(Vec::len).sum::<usize>(), 2_002_000);
        assert_eq!(tree.rows.rows.len(), 2000);
        let start = Instant::now();
        tree.rows = project(
            &tree.index,
            root,
            &BTreeSet::new(),
            true,
            &BTreeSet::new(),
            "",
            None,
            false,
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        tree.all_open = true;
        assert_eq!(tree.rows.rows.len(), 2_002_000);
        eprintln!(
            "Expanded projection built in {:?} (worker operation)",
            start.elapsed()
        );
        let ctx = egui::Context::default();
        let mut durations = Vec::new();
        for offset in [0.0, 0.0, 25_000_000.0, 50_049_100.0, 50_049_100.0] {
            let start = Instant::now();
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(400.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        egui::ScrollArea::vertical()
                            .vertical_scroll_offset(offset)
                            .show(ui, |ui| {
                                tree.show(
                                    ui,
                                    None,
                                    "",
                                    &BTreeMap::new(),
                                    false,
                                    false,
                                    &mut None,
                                    &mut None,
                                );
                            });
                    });
                },
            );
            assert!(
                output.shapes.len() < 500,
                "hidden rows must not emit shapes"
            );
            durations.push(start.elapsed());
        }
        eprintln!("Fully expanded tree frame times: {durations:?}");
        assert!(
            durations[1..]
                .iter()
                .all(|d| *d < Duration::from_micros(8_333)),
            "steady frames must fit a 120 Hz budget"
        );
    }
}
