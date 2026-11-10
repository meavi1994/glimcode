//! Structural changes share the original index and store only changed paths.
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};
#[derive(Clone, Default)]
pub struct Delta {
    pub removed: BTreeSet<PathBuf>,
    pub files: BTreeSet<PathBuf>,
    pub folders: BTreeSet<PathBuf>,
}
impl Delta {
    pub fn removed(&self, path: &Path) -> bool {
        path.ancestors().any(|p| self.removed.contains(p))
    }
    pub fn hidden(&self, path: &Path, folder: bool) -> bool {
        self.removed(path) && !(if folder { &self.folders } else { &self.files }).contains(path)
    }
    pub fn remove(&mut self, path: PathBuf) {
        self.files.retain(|p| !p.starts_with(&path));
        self.folders.retain(|p| !p.starts_with(&path));
        if !self.removed(&path) {
            self.removed.retain(|p| !p.starts_with(&path));
            self.removed.insert(path);
        }
    }
    pub fn count(&self, base: &[PathBuf], folder: bool) -> usize {
        let mut count = base.len();
        for path in &self.removed {
            let range = prefix_range(base, path);
            count -= range.len();
        }
        count
            + (if folder { &self.folders } else { &self.files })
                .iter()
                .filter(|p| self.removed(p) || base.binary_search(p).is_err())
                .count()
    }
}
pub fn prefix_range(base: &[PathBuf], path: &Path) -> std::ops::Range<usize> {
    base.partition_point(|p| p.as_path() < path)
        ..base.partition_point(|p| p.as_path() <= path || p.starts_with(path))
}
pub struct Patch {
    pub file_count: usize,
    pub folder_count: usize,
    pub tree: Option<crate::explorer::FileTree>,
    pub delta: Arc<Delta>,
    pub search: Option<Arc<crate::path_search::PathSearch>>,
}
/// Reconcile selected roots; None performs a streaming full reconciliation. The
/// unchanged index is never copied, sorted, or rebuilt into another tree.
pub fn reconcile(
    root: &Path,
    scopes: Option<Vec<PathBuf>>,
    files: Arc<Vec<PathBuf>>,
    folders: Arc<Vec<PathBuf>>,
    previous: Arc<Delta>,
    cancel: impl Fn() -> bool,
) -> anyhow::Result<Option<Patch>> {
    let mut delta = (*previous).clone();
    if let Some(scopes) = scopes {
        let mut roots = BTreeSet::new();
        for path in scopes {
            if path == root || !path.starts_with(root) {
                return reconcile(root, None, files, folders, previous, cancel);
            }
            if !path.ancestors().skip(1).any(|p| roots.contains(p)) {
                roots.retain(|p: &PathBuf| !p.starts_with(&path));
                roots.insert(path);
            }
        }
        let mut rules = crate::ignore_rules::WorkspaceRules::new(root);
        for scope in roots {
            if cancel() {
                return Ok(None);
            }
            delta.remove(scope.clone());
            let metadata = match std::fs::symlink_metadata(&scope) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
                Ok(metadata) => metadata,
            };
            if !rules.allows(&scope, metadata.is_dir()) {
                continue;
            }
            if metadata.is_file() {
                delta.files.insert(scope);
            } else if metadata.is_dir() {
                delta.folders.insert(scope.clone());
                for child in crate::navigation::scoped_workspace_walk(root, &scope)?.build() {
                    if cancel() {
                        return Ok(None);
                    }
                    let child = child?;
                    if child.file_type().is_some_and(|t| t.is_file()) {
                        delta.files.insert(child.into_path());
                    } else if child.path() != root && child.file_type().is_some_and(|t| t.is_dir())
                    {
                        delta.folders.insert(child.into_path());
                    }
                }
            }
        }
    } else {
        let mut seen_files = vec![false; files.len()];
        let mut seen_folders = vec![false; folders.len()];
        let mut seen_added = BTreeSet::new();
        for entry in crate::navigation::scoped_workspace_walk(root, root)?.build() {
            if cancel() {
                return Ok(None);
            }
            let entry = entry?;
            let path = entry.path();
            let folder = entry.file_type().is_some_and(|t| t.is_dir());
            if !folder && !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            if path == root {
                continue;
            }
            let (base, seen, added) = if folder {
                (&folders, &mut seen_folders, &mut delta.folders)
            } else {
                (&files, &mut seen_files, &mut delta.files)
            };
            if let Ok(index) = base.binary_search_by(|p| p.as_path().cmp(path)) {
                seen[index] = true;
            } else {
                added.insert(path.to_owned());
            }
            if previous.removed(path) {
                if folder {
                    delta.folders.insert(path.to_owned());
                } else {
                    delta.files.insert(path.to_owned());
                }
            }
            if (if folder { &delta.folders } else { &delta.files }).contains(path) {
                seen_added.insert(path.to_owned());
            }
        }
        // Missing ancestor directories collapse a whole removed subtree to one tombstone.
        for (i, path) in folders.iter().enumerate() {
            if !seen_folders[i] && !delta.removed(path) {
                delta.remove(path.clone());
            }
        }
        for (i, path) in files.iter().enumerate() {
            if !seen_files[i] && !delta.removed(path) {
                delta.remove(path.clone());
            }
        }
        delta.files.retain(|p| seen_added.contains(p));
        delta.folders.retain(|p| seen_added.contains(p));
    }
    // Parent folders of additions must remain traversable even when a removed
    // directory is recreated with different contents.
    let parents: Vec<_> = delta
        .files
        .iter()
        .chain(delta.folders.iter())
        .flat_map(|p| {
            p.ancestors()
                .skip(1)
                .take_while(|p| *p != root && p.starts_with(root))
                .map(Path::to_path_buf)
        })
        .collect();
    delta.folders.extend(parents);
    let search = crate::path_search::PathSearch::build(
        root,
        Arc::new(delta.files.iter().cloned().collect()),
        Arc::new(delta.folders.iter().cloned().collect()),
        &cancel,
    )
    .map(Arc::new);
    let file_count = delta.count(&files, false);
    let folder_count = delta.count(&folders, true);
    let delta = Arc::new(delta);
    let tree = if files.len() < crate::project_tree::THRESHOLD
        && file_count >= crate::project_tree::THRESHOLD
    {
        let mut tree =
            match crate::explorer::FileTree::build_large_shared(root, files, folders, &cancel) {
                Some(tree) => tree,
                None => return Ok(None),
            };
        tree.apply_delta(root, delta.clone());
        Some(tree)
    } else {
        None
    };
    Ok((!cancel()).then(|| Patch {
        file_count,
        folder_count,
        tree,
        delta,
        search,
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scoped_changes_preserve_ignore_rules_renames_and_recreated_directories() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        std::fs::create_dir(root.join(".git"))?;
        std::fs::create_dir(root.join("src"))?;
        std::fs::write(root.join(".gitignore"), "*.ignored\n")?;
        std::fs::write(root.join("src/a.rs"), "a")?;
        let base = crate::navigation::workspace_paths(&root)?;
        let files = Arc::new(base.files);
        let folders = Arc::new(base.folders);
        std::fs::rename(root.join("src/a.rs"), root.join("src/b.rs"))?;
        std::fs::write(root.join("src/x.ignored"), "x")?;
        let patch = reconcile(
            &root,
            Some(vec![
                root.join("src/a.rs"),
                root.join("src/b.rs"),
                root.join("src/x.ignored"),
            ]),
            files.clone(),
            folders.clone(),
            Arc::default(),
            || false,
        )?
        .unwrap();
        assert!(patch.delta.hidden(&root.join("src/a.rs"), false));
        assert!(patch.delta.files.contains(&root.join("src/b.rs")));
        assert!(!patch.delta.files.contains(&root.join("src/x.ignored")));
        assert_eq!(patch.delta.count(&files, false), files.len());
        std::fs::remove_file(root.join("src/b.rs"))?;
        let patch = reconcile(&root, None, files.clone(), folders, patch.delta, || false)?.unwrap();
        assert_eq!(patch.delta.count(&files, false), files.len() - 1);
        Ok(())
    }
}

#[cfg(test)]
mod filesystem_benchmark {
    use super::*;
    use std::time::Instant;
    #[test]
    #[ignore = "creates and removes two million real empty files; run release separately"]
    fn real_two_million_file_workspace_benchmark() -> anyhow::Result<()> {
        let fixture = tempfile::Builder::new()
            .prefix("glimcode-real-2m-")
            .tempdir()?;
        let root = fixture.path().canonicalize()?;
        std::fs::create_dir(root.join(".git"))?;
        let creation = Instant::now();
        std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for shard in 0..4 {
                let root = &root;
                workers.push(scope.spawn(move || -> std::io::Result<()> {
                    for directory in (shard..2000).step_by(4) {
                        let parent = root.join(format!("dir{directory:04}"));
                        std::fs::create_dir(&parent)?;
                        for file in 0..1000 {
                            std::fs::File::create(parent.join(format!("file{file:04}.rs")))?;
                        }
                    }
                    Ok(())
                }));
            }
            for worker in workers {
                worker.join().unwrap()?;
            }
            Ok::<_, std::io::Error>(())
        })?;
        let creation_seconds = creation.elapsed().as_secs_f64();
        eprintln!("Created 2,000,000 real files in {creation_seconds:.2}s");
        let walk = Instant::now();
        let paths = crate::navigation::workspace_paths(&root)?;
        let walk_seconds = walk.elapsed().as_secs_f64();
        assert_eq!(paths.files.len(), 2_000_000);
        let files = Arc::new(paths.files);
        let folders = Arc::new(paths.folders);
        let build = Instant::now();
        let tree = crate::explorer::FileTree::build_shared_cancellable(
            &root,
            files.clone(),
            folders.clone(),
            || false,
        )
        .unwrap();
        let search =
            crate::path_search::PathSearch::build(&root, files.clone(), folders.clone(), || false)
                .unwrap();
        let index_seconds = build.elapsed().as_secs_f64();
        let mut query_ms = Vec::new();
        let mut query_examined = Vec::new();
        for query in [
            "file0999.rs",
            "file0",
            "0999",
            "dir1999/file0999",
            "absent-needle",
        ] {
            let started = Instant::now();
            let result = search.query(query, || false);
            query_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            query_examined.push(search.examined());
            if query == "absent-needle" {
                assert!(result.is_empty());
            } else {
                assert!(!result.is_empty());
            }
        }
        let extra = root.join("dir1999/new-unique.rs");
        std::fs::write(&extra, "")?;
        let update = Instant::now();
        let patch = reconcile(
            &root,
            Some(vec![extra.clone()]),
            files.clone(),
            folders.clone(),
            Arc::default(),
            || false,
        )?
        .unwrap();
        let delta_ms = update.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(patch.delta.count(&files, false), 2_000_001);
        assert_eq!(
            patch.search.as_ref().unwrap().query("new-unique", || false),
            vec![extra]
        );
        let reconcile_start = Instant::now();
        let patch = reconcile(&root, None, files.clone(), folders, patch.delta, || false)?.unwrap();
        let reconcile_seconds = reconcile_start.elapsed().as_secs_f64();
        assert_eq!(patch.delta.count(&files, false), 2_000_001);
        let report = serde_json::json!({"real_files":2_000_000,"creation_seconds":creation_seconds,"walk_seconds":walk_seconds,"index_seconds":index_seconds,"query_ms":query_ms,"query_examined":query_examined,"single_file_update_ms":delta_ms,"streaming_reconcile_seconds":reconcile_seconds});
        std::fs::create_dir_all("target/performance")?;
        std::fs::write(
            "target/performance/real-workspace.json",
            serde_json::to_vec_pretty(&report)?,
        )?;
        eprintln!("Real filesystem benchmark: {report}");
        drop((tree, search, patch, files));
        eprintln!("Removing real-filesystem fixture");
        fixture.close()?;
        Ok(())
    }
}
