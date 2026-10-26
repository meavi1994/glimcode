//! Apply Glimcode's extra ignore rules to paths supplied outside the directory walker.
use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::{
    cell::RefCell,
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

pub struct GlobalIgnores {
    root: PathBuf,
    canonical_root: PathBuf,
    global: Gitignore,
    local: HashMap<PathBuf, Gitignore>,
    in_git: bool,
}

impl GlobalIgnores {
    pub fn load(root: &Path, file: Option<&Path>) -> Result<Self> {
        let mut builder = GitignoreBuilder::new(root);
        if let Some(file) = file.filter(|file| file.is_file())
            && let Some(error) = builder.add(file)
        {
            return Err(error).context("Cannot load global ignore rules");
        }
        Ok(Self {
            root: root.to_owned(),
            canonical_root: root.canonicalize().unwrap_or_else(|_| root.to_owned()),
            global: builder.build()?,
            local: HashMap::new(),
            in_git: root.ancestors().any(|path| path.join(".git").exists()),
        })
    }

    pub fn is_ignored(&mut self, path: &Path) -> bool {
        if self.global.is_empty() {
            return false;
        }
        let Ok(relative) = path
            .strip_prefix(&self.root)
            .or_else(|_| path.strip_prefix(&self.canonical_root))
        else {
            return false;
        };
        let mut components = relative.components().peekable();
        let mut prefix = PathBuf::new();
        // Check each directory before the file, just like the walker. This
        // handles deleted files and avoids matching the host's /tmp directory.
        while let Some(component) = components.next() {
            prefix.push(component);
            let is_dir = components.peek().is_some();
            if self.global.matched(&prefix, is_dir).is_ignore()
                && !self.locally_whitelisted(&prefix, is_dir)
            {
                return true;
            }
        }
        false
    }

    fn locally_whitelisted(&mut self, relative: &Path, is_dir: bool) -> bool {
        let absolute = self.root.join(relative);
        let parent = relative.parent().unwrap_or(Path::new(""));
        // Match the walker's precedence: custom ignore, .ignore, then .gitignore;
        // within each source, deeper files override their ancestors.
        for name in [".rgignore", ".ignore", ".gitignore"] {
            if name == ".gitignore" && !self.in_git {
                continue;
            }
            for scope in parent.ancestors() {
                let directory = self.root.join(scope);
                let file = directory.join(name);
                let matcher = self.local.entry(file.clone()).or_insert_with(|| {
                    let mut builder = GitignoreBuilder::new(&directory);
                    if file.is_file() {
                        let _ = builder.add(file);
                    }
                    builder.build().unwrap_or_else(|_| Gitignore::empty())
                });
                let matched = matcher.matched(&absolute, is_dir);
                if !matched.is_none() {
                    return matched.is_whitelist();
                }
            }
        }
        false
    }
}

struct Cached {
    root: PathBuf,
    file: Option<PathBuf>,
    stamp: Option<(SystemTime, u64)>,
    loaded: Instant,
    rules: GlobalIgnores,
}

thread_local! {
    static CACHE: RefCell<Option<Cached>> = const { RefCell::new(None) };
}

/// The Changed tree is drawn every frame. Reuse compiled rules, and refresh
/// local exceptions at least as often as the project's background file scan.
pub fn with_global_ignores<T>(root: &Path, apply: impl FnOnce(&mut GlobalIgnores) -> T) -> T {
    CACHE.with(|cache| {
        let file = crate::preferences::global_ignore_path();
        let stamp = file
            .as_ref()
            .and_then(|file| std::fs::metadata(file).ok())
            .and_then(|meta| Some((meta.modified().ok()?, meta.len())));
        let mut cache = cache.borrow_mut();
        if cache.as_ref().is_none_or(|cached| {
            cached.root != root
                || cached.file != file
                || cached.stamp != stamp
                || cached.loaded.elapsed() >= Duration::from_secs(2)
        }) {
            let rules = GlobalIgnores::load(root, file.as_deref())
                .unwrap_or_else(|_| GlobalIgnores::load(root, None).expect("empty ignore matcher"));
            *cache = Some(Cached {
                root: root.to_owned(),
                file,
                stamp,
                loaded: Instant::now(),
                rules,
            });
        }
        apply(&mut cache.as_mut().unwrap().rules)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_global_patterns_apply_to_explicit_paths_without_matching_host_ancestors() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("tmp/project");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::create_dir_all(root.join("nested"))?;
        let file = dir.path().join("global.gitignore");
        std::fs::write(
            &file,
            "**/tmp/\n*.log\n/root-only.txt\nbuild/**/generated-?.rs\nsecret[0-9].txt\n!keep.log\n",
        )?;
        std::fs::write(root.join(".gitignore"), "!local.log\n")?;
        std::fs::write(root.join("nested/.gitignore"), "!nested.log\n")?;
        let mut rules = GlobalIgnores::load(&root, Some(&file))?;
        for path in [
            "tmp/tracked.rs",
            "nested/tmp/deleted.rs",
            "debug.log",
            "nested/debug.log",
            "root-only.txt",
            "build/deep/generated-a.rs",
            "secret3.txt",
        ] {
            assert!(rules.is_ignored(&root.join(path)), "must exclude {path}");
        }
        for path in [
            "src/main.rs",
            "keep.log",
            "local.log",
            "nested/nested.log",
            "nested/root-only.txt",
            "secretx.txt",
        ] {
            assert!(!rules.is_ignored(&root.join(path)), "must include {path}");
        }
        assert!(!rules.is_ignored(&dir.path().join("outside.log")));
        // Root aliases must not change matching, including for deleted paths.
        assert!(rules.is_ignored(&root.canonicalize()?.join("nested/tmp/deleted.rs")));
        Ok(())
    }
}
