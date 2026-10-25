//! Small, versioned workspace metadata. Never stores file contents or shell commands.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub struct Store {
    directory: PathBuf,
}
impl Store {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
    fn path(&self, root: &Path) -> PathBuf {
        let hash = root
            .to_string_lossy()
            .bytes()
            .fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        self.directory.join(format!("{hash:016x}.json"))
    }
    pub fn load(&self, root: &Path) -> Option<Value> {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let root = canonical.as_path();
        let file = std::fs::File::open(self.path(root)).ok()?;
        if file.metadata().ok()?.len() > 1024 * 1024 {
            return None;
        }
        let value: Value = serde_json::from_reader(file).ok()?;
        (value["version"] == 1 && value["root"].as_str() == root.to_str()).then_some(value)
    }
    pub fn save(&self, root: &Path, mut value: Value) -> Result<()> {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let root = canonical.as_path();
        value["version"] = 1.into();
        value["root"] = root.to_string_lossy().as_ref().into();
        std::fs::create_dir_all(&self.directory)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer(&mut file, &value)?;
        file.flush()?;
        file.persist(self.path(root))
            .context("Save workspace session")?;
        if let Some(parent) = self.directory.parent() {
            let mut state = tempfile::NamedTempFile::new_in(parent)?;
            serde_json::to_writer(
                &mut state,
                &serde_json::json!({"last_project": root.to_string_lossy()}),
            )?;
            state.flush()?;
            state.persist(parent.join("state.json"))?;
        }
        Ok(())
    }
}

pub fn last_project() -> Option<PathBuf> {
    let state = crate::preferences::path()?.parent()?.join("state.json");
    let file = std::fs::File::open(state).ok()?;
    if file.metadata().ok()?.len() > 65536 {
        return None;
    }
    let value: Value = serde_json::from_reader(file).ok()?;
    value["last_project"]
        .as_str()
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sessions_are_isolated_atomic_and_tolerate_invalid_data() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let store = Store::new(dir.path().join("projects"));
        let first = Path::new("/repo/main");
        let second = Path::new("/repo/feature");
        store.save(first, serde_json::json!({"tabs": ["one.rs"]}))?;
        store.save(second, serde_json::json!({"tabs": ["two.rs"]}))?;
        assert_eq!(store.load(first).unwrap()["tabs"][0], "one.rs");
        assert_eq!(store.load(second).unwrap()["tabs"][0], "two.rs");
        let state: Value = serde_json::from_slice(&std::fs::read(dir.path().join("state.json"))?)?;
        assert_eq!(state["last_project"], "/repo/feature");
        store.save(first, serde_json::json!({"tabs": []}))?;
        assert_eq!(store.load(first).unwrap()["tabs"], serde_json::json!([]));
        std::fs::write(store.path(first), "broken")?;
        assert!(store.load(first).is_none());
        std::fs::write(store.path(second), r#"{"version":1,"root":"/wrong"}"#)?;
        assert!(store.load(second).is_none());
        Ok(())
    }
}
