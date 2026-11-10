//! Coalesce native filesystem events without queueing one allocation per event.
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

const FILES: u8 = 1;
const GIT: u8 = 2;
const FAILED: u8 = 4;
const METADATA: u8 = 8;

#[derive(Default)]
pub struct Changes {
    pub files: bool,
    pub git: bool,
    pub git_metadata: bool,
}

#[derive(Clone)]
struct WatchIndex {
    files: Arc<Vec<PathBuf>>,
    folders: Arc<Vec<PathBuf>>,
}

type DirectoryUpdate = (Arc<Vec<PathBuf>>, Option<WatchIndex>);

pub struct WorkspaceWatch {
    root: PathBuf,
    watching: bool,
    initializing: Option<Receiver<bool>>,
    updates: Option<std::sync::mpsc::Sender<DirectoryUpdate>>,
    index: Arc<std::sync::RwLock<Option<WatchIndex>>>,
    flags: Arc<AtomicU8>,
    pending: u8,
    since: Option<Instant>,
}

fn indexed(paths: &[PathBuf], path: &Path) -> bool {
    paths
        .binary_search_by(|candidate| candidate.as_path().cmp(path))
        .is_ok()
}

/// Use the exact index produced by the ignore-aware walker. This avoids a
/// second implementation of .gitignore precedence/negation in the event path.
fn filter_event(root: &Path, index: Option<&WatchIndex>, event: &mut Event) -> u8 {
    if matches!(event.kind, EventKind::Access(_)) {
        return 0;
    }
    if event.need_rescan() {
        return FILES | GIT;
    }
    let membership = matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Any
            | EventKind::Other
            | EventKind::Modify(notify::event::ModifyKind::Name(_))
    );
    if !event.paths.is_empty() {
        event.paths.retain(|path| {
            let Ok(relative) = path.strip_prefix(root) else {
                return true;
            }; // linked Git metadata
            let mut parts = relative.components();
            if parts.next().is_some_and(|part| part.as_os_str() == ".git") {
                return !parts
                    .next()
                    .is_some_and(|part| part.as_os_str() == "objects");
            }
            if relative
                .components()
                .any(|part| part.as_os_str() == "target")
            {
                return false;
            }
            let Some(index) = index else {
                return true;
            }; // initial index is still being built
            if path == root || indexed(&index.files, path) || indexed(&index.folders, path) {
                return true;
            }
            let configuration = matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some(".gitignore" | ".ignore" | ".rgignore")
            );
            // A new entry in a visible directory needs a scan to apply its
            // ignore rules. Events inside an excluded subtree never do.
            (membership || configuration)
                && path
                    .parent()
                    .is_some_and(|parent| parent == root || indexed(&index.folders, parent))
        });
        if event.paths.is_empty() {
            return 0;
        }
    }
    classify(event)
}

fn git_directories(root: &Path) -> Vec<PathBuf> {
    let dot_git = root.join(".git");
    let directory = if dot_git.is_dir() {
        dot_git
    } else if let Ok(contents) = std::fs::read_to_string(&dot_git)
        && let Some(directory) = contents.trim().strip_prefix("gitdir: ")
    {
        root.join(directory)
    } else {
        return Vec::new();
    };
    let mut paths = vec![directory.clone()];
    if let Ok(common) = std::fs::read_to_string(directory.join("commondir")) {
        paths.push(directory.join(common.trim()));
    }
    paths
}

fn classify(event: &Event) -> u8 {
    // Directory walks and file reads must not trigger another refresh.
    if matches!(event.kind, EventKind::Access(_)) {
        return 0;
    }
    let ignore_changed = event.paths.iter().any(|path| {
        matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some(".gitignore" | ".ignore" | ".rgignore" | "exclude" | ".git")
        )
    });
    if event.need_rescan() {
        return FILES | GIT;
    }
    if !ignore_changed
        && !event.paths.is_empty()
        && event
            .paths
            .iter()
            .all(|path| path.components().any(|part| part.as_os_str() == ".git"))
    {
        let metadata = event
            .paths
            .iter()
            .any(|path| path.file_name().is_none_or(|name| name != "index.lock"));
        return GIT | if metadata { METADATA } else { 0 };
    }
    let paths_changed = matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Any
            | EventKind::Other
            | EventKind::Modify(notify::event::ModifyKind::Name(_))
    );
    GIT | if paths_changed || ignore_changed || event.need_rescan() {
        FILES
    } else {
        0
    }
}

impl WorkspaceWatch {
    pub fn new(root: &Path, ctx: &eframe::egui::Context) -> Self {
        let flags = Arc::new(AtomicU8::new(0));
        let output = flags.clone();
        let worker_flags = flags.clone();
        let index = Arc::new(std::sync::RwLock::new(None::<WatchIndex>));
        let event_index = index.clone();
        let ctx = ctx.clone();
        let watch_root = root.to_path_buf();
        let (tx, rx) = mpsc::channel();
        let (updates, directories) = mpsc::channel::<DirectoryUpdate>();
        std::thread::spawn(move || {
            let callback_ctx = ctx.clone();
            let event_root = watch_root.clone();
            let mut watcher = notify::recommended_watcher(move |event: notify::Result<Event>| {
                let bits = event
                    .map(|mut event| {
                        let index = event_index.read().unwrap();
                        filter_event(&event_root, index.as_ref(), &mut event)
                    })
                    .unwrap_or(FILES | GIT | FAILED);
                if bits != 0 && output.fetch_or(bits, Ordering::Relaxed) & (FILES | GIT) == 0 {
                    callback_ctx.request_repaint();
                }
            })
            .ok();
            if let Some(watch) = watcher.as_mut() {
                // FSEvents subscribes to a root without walking its descendants.
                // Backends which walk recursively get only eligible directories,
                // registered nonrecursively from the existing project index.
                let mode = if cfg!(target_os = "macos") {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                };
                if watch.watch(&watch_root, mode).is_err() {
                    watcher = None;
                }
            }
            if let Some(watch) = watcher.as_mut() {
                for directory in git_directories(&watch_root) {
                    for path in [
                        directory.clone(),
                        directory.join("info"),
                        directory.join("refs"),
                    ] {
                        if !path.is_dir() {
                            continue;
                        }
                        let mode = if path.ends_with("refs") {
                            RecursiveMode::Recursive
                        } else {
                            RecursiveMode::NonRecursive
                        };
                        if watch.watch(&path, mode).is_err() {
                            worker_flags.fetch_or(FILES | GIT | FAILED, Ordering::Relaxed);
                        }
                    }
                }
            }
            let _ = tx.send(watcher.is_some());
            ctx.request_repaint();
            #[cfg(not(target_os = "macos"))]
            let mut watched = std::collections::BTreeSet::from([watch_root.clone()]);
            while let Ok(mut update) = directories.recv() {
                for next in directories.try_iter() {
                    update = next;
                }
                // Old immutable index snapshots are retired here, off the UI thread.
                let (folders, old_index) = update;
                drop(old_index);
                #[cfg(target_os = "macos")]
                let _ = folders;
                #[cfg(not(target_os = "macos"))]
                if let Some(watch) = watcher.as_mut() {
                    let wanted: std::collections::BTreeSet<_> = folders
                        .iter()
                        .cloned()
                        .chain(std::iter::once(watch_root.clone()))
                        .collect();
                    let added = wanted.difference(&watched).count();
                    let mut batch = watch.paths_mut();
                    for path in watched.difference(&wanted) {
                        let _ = batch.remove(path);
                    }
                    for path in wanted.difference(&watched) {
                        if batch.add(path, RecursiveMode::NonRecursive).is_err() {
                            worker_flags.fetch_or(FILES | GIT | FAILED, Ordering::Relaxed);
                        }
                    }
                    if batch.commit().is_err() {
                        worker_flags.fetch_or(FILES | GIT | FAILED, Ordering::Relaxed);
                    }
                    watched = wanted;
                    // Reconcile changes made before newly discovered directories
                    // acquired watches. The next unchanged scan does not repeat it.
                    if added > 0 {
                        worker_flags.fetch_or(FILES | GIT, Ordering::Relaxed);
                        ctx.request_repaint();
                    }
                }
            }
        });
        Self {
            root: root.to_path_buf(),
            watching: false,
            initializing: Some(rx),
            updates: Some(updates),
            index,
            flags,
            pending: 0,
            since: None,
        }
    }

    pub fn set_index(&mut self, files: Arc<Vec<PathBuf>>, folders: Arc<Vec<PathBuf>>) {
        let mut index = self.index.write().unwrap();
        if index.as_ref().is_some_and(|current| {
            Arc::ptr_eq(&current.files, &files) && Arc::ptr_eq(&current.folders, &folders)
        }) {
            return;
        }
        let old = std::mem::replace(
            &mut *index,
            Some(WatchIndex {
                files,
                folders: folders.clone(),
            }),
        );
        drop(index);
        if let Some(tx) = &self.updates
            && let Err(error) = tx.send((folders, old))
        {
            std::thread::spawn(move || drop(error.0));
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn reliable(&self) -> bool {
        self.watching && self.flags.load(Ordering::Relaxed) & FAILED == 0
    }

    pub fn next_poll(&self) -> Option<Duration> {
        self.since
            .map(|at| Duration::from_millis(200).saturating_sub(at.elapsed()))
    }

    pub fn poll(&mut self, now: Instant) -> Changes {
        if let Some(rx) = &self.initializing {
            match rx.try_recv() {
                Ok(watching) => {
                    self.watching = watching;
                    self.initializing = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => self.initializing = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let flags = self.flags.fetch_and(FAILED, Ordering::Relaxed);
        self.pending |= flags & (FILES | GIT | METADATA);
        if self.pending != 0 {
            let since = *self.since.get_or_insert(now);
            if now.saturating_duration_since(since) >= Duration::from_millis(200) {
                let pending = std::mem::take(&mut self.pending);
                self.since = None;
                return Changes {
                    files: pending & FILES != 0,
                    git: pending & GIT != 0,
                    git_metadata: pending & METADATA != 0,
                };
            }
        }
        Changes::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_filters_use_the_same_gitignore_results_as_the_project_index() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        std::fs::create_dir(root.join(".git"))?;
        for folder in [
            "node_modules/pkg",
            ".node_modules/pkg",
            "src",
            "cache/nested",
        ] {
            std::fs::create_dir_all(root.join(folder))?;
        }
        std::fs::write(
            root.join(".gitignore"),
            "node_modules/\n.node_modules/\ncache/\n*.generated.ts\n!keep.generated.ts\n",
        )?;
        for name in [
            "node_modules/pkg/index.ts",
            ".node_modules/pkg/index.ts",
            "cache/nested/generated.ts",
            "src/index.ts",
            "skip.generated.ts",
            "keep.generated.ts",
        ] {
            std::fs::write(root.join(name), "const needle = 1;")?;
        }
        let paths = crate::navigation::workspace_paths(&root)?;
        let index = WatchIndex {
            files: Arc::new(paths.files),
            folders: Arc::new(paths.folders),
        };
        for name in [
            "node_modules/pkg/new.ts",
            ".node_modules/pkg/new.ts",
            "cache/nested/new.ts",
        ] {
            let mut event = Event::new(EventKind::Create(notify::event::CreateKind::File))
                .add_path(root.join(name));
            assert_eq!(
                filter_event(&root, Some(&index), &mut event),
                0,
                "{name} must not trigger refreshes"
            );
        }
        for name in ["node_modules", ".node_modules", "skip.generated.ts"] {
            let mut event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                .add_path(root.join(name));
            assert_eq!(filter_event(&root, Some(&index), &mut event), 0);
        }
        for name in ["src/index.ts", "keep.generated.ts"] {
            let mut event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
                .add_path(root.join(name));
            assert_eq!(filter_event(&root, Some(&index), &mut event), GIT);
        }
        let mut event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(root.join(".gitignore"));
        assert_eq!(filter_event(&root, Some(&index), &mut event), FILES | GIT);
        let mut event = Event::new(EventKind::Create(notify::event::CreateKind::File))
            .add_path(root.join("src/new.ts"));
        assert_eq!(filter_event(&root, Some(&index), &mut event), FILES | GIT);
        // No hard-coded dependency-folder exclusions: the project's rules decide.
        std::fs::write(root.join(".gitignore"), "")?;
        assert!(
            crate::navigation::workspace_paths(&root)?
                .files
                .contains(&root.join("node_modules/pkg/index.ts"))
        );
        Ok(())
    }

    #[test]
    #[ignore = "requires native filesystem events outside the process sandbox"]
    fn native_watch_reports_new_files_or_keeps_the_fallback_available() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let mut watch = WorkspaceWatch::new(&root, &eframe::egui::Context::default());
        let deadline = Instant::now() + Duration::from_secs(10);
        while watch.initializing.is_some() {
            watch.poll(Instant::now());
            anyhow::ensure!(Instant::now() < deadline, "watch setup timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        if !watch.reliable() {
            assert!(!watch.watching);
            return Ok(());
        }
        std::fs::write(root.join("new.ts"), "const value = 1;")?;
        loop {
            let changes = watch.poll(Instant::now());
            if changes.files {
                assert!(changes.git);
                break;
            }
            anyhow::ensure!(Instant::now() < deadline, "new file event did not arrive");
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    #[test]
    fn git_ignore_edits_and_overflow_request_a_workspace_rescan() {
        let event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path("/project/.git/info/exclude".into());
        assert_eq!(classify(&event), FILES | GIT);
        let event = Event::new(EventKind::Other)
            .add_path("/project/.git/index".into())
            .set_flag(notify::event::Flag::Rescan);
        assert_eq!(classify(&event), FILES | GIT);
        assert_eq!(
            classify(
                &Event::new(EventKind::Create(notify::event::CreateKind::File))
                    .add_path("/project/.git/refs/heads/topic".into())
            ),
            GIT | METADATA
        );
    }

    #[test]
    fn events_coalesce_without_losing_changes_during_continuous_writes() {
        let mut watch = WorkspaceWatch {
            root: PathBuf::new(),
            watching: false,
            initializing: None,
            updates: None,
            index: Arc::new(std::sync::RwLock::new(None)),
            flags: Arc::new(AtomicU8::new(FILES | GIT)),
            pending: 0,
            since: None,
        };
        let now = Instant::now();
        assert!(!watch.poll(now).files);
        watch.flags.fetch_or(GIT, Ordering::Relaxed);
        let changes = watch.poll(now + Duration::from_millis(200));
        assert!(changes.files && changes.git);
        assert!(!watch.poll(now + Duration::from_millis(201)).git);
        watch.flags.store(FILES | FAILED, Ordering::Relaxed);
        watch.poll(now + Duration::from_secs(1));
        assert!(watch.poll(now + Duration::from_secs(2)).files);
        assert!(!watch.reliable());
    }

    #[test]
    fn reads_are_ignored_but_renames_and_ignore_edits_refresh_the_tree() {
        use notify::event::{AccessKind, ModifyKind, RenameMode};
        assert_eq!(classify(&Event::new(EventKind::Access(AccessKind::Any))), 0);
        assert_eq!(
            classify(&Event::new(EventKind::Modify(ModifyKind::Data(
                notify::event::DataChange::Any
            )))),
            GIT
        );
        assert_eq!(
            classify(&Event::new(EventKind::Modify(ModifyKind::Name(
                RenameMode::Any
            )))),
            FILES | GIT
        );
        assert_eq!(
            classify(&Event::new(EventKind::Modify(ModifyKind::Any)).add_path(".gitignore".into())),
            FILES | GIT
        );
    }
}
