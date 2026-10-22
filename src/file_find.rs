use std::{
    ops::Range,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub struct FileFind {
    path: PathBuf,
    query: String,
    /// Document revision the matches were found in.
    revision: Option<u64>,
    pub matches: Vec<Range<usize>>,
    pub current: Option<usize>,
}

impl FileFind {
    /// Match offsets are bytes; the editor converts to character offsets at its boundary.
    pub fn refresh(
        &mut self,
        path: &Path,
        text: &str,
        query: &str,
        revision: u64,
        caret: impl FnOnce() -> usize,
    ) -> bool {
        let reset = self.path != path || self.query != query;
        if !reset && self.revision == Some(revision) {
            // The text and the query are unchanged, so the matches still hold.
            return false;
        }
        let matches: Vec<_> = if query.is_empty() {
            vec![]
        } else {
            text.match_indices(query)
                .map(|(start, _)| start..start + query.len())
                .collect()
        };
        let previous = self.active().cloned();
        if reset || self.matches != matches {
            self.path = path.to_path_buf();
            self.query = query.to_owned();
            let anchor = if reset {
                caret()
            } else {
                previous.as_ref().map_or_else(caret, |r| r.start)
            };
            self.current = (!matches.is_empty())
                .then(|| matches.iter().position(|r| r.start >= anchor).unwrap_or(0));
            self.matches = matches;
        }
        self.revision = Some(revision);
        reset || previous != self.active().cloned()
    }

    pub fn active(&self) -> Option<&Range<usize>> {
        self.current.and_then(|index| self.matches.get(index))
    }

    pub fn step(&mut self, backwards: bool) {
        let count = self.matches.len();
        if count == 0 {
            self.current = None;
            return;
        }
        let current = self.current.unwrap_or(0);
        self.current = Some(if backwards {
            (current + count - 1) % count
        } else {
            (current + 1) % count
        });
    }

    pub fn counter(&self) -> String {
        format!(
            "{}/{}",
            self.current.map_or(0, |index| index + 1),
            self.matches.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn idle_frames_skip_caret_scans_and_same_length_edits_refresh() {
        let mut find = FileFind::default();
        let path = Path::new("a.txt");
        find.refresh(path, "猫 apps", "apps", 1, || 0);
        assert_eq!(find.active(), Some(&(4..8)));
        assert!(!find.refresh(path, "猫 apps", "apps", 1, || panic!("idle caret scan")));
        find.refresh(path, "apps 猫", "apps", 2, || 0);
        assert_eq!(find.active(), Some(&(0..4)));
        find.refresh(path, "no match", "apps", 3, || 0);
        assert_eq!(find.counter(), "0/0");
    }

    #[test]
    fn navigation_wraps_and_empty_results_are_safe() {
        let mut find = FileFind::default();
        find.refresh(Path::new("a.rs"), "猫 apps apps", "apps", 0, || 0);
        assert_eq!(find.counter(), "1/2");
        assert_eq!(find.active(), Some(&(4..8)));
        find.step(false);
        assert_eq!(find.counter(), "2/2");
        find.step(false);
        assert_eq!(find.counter(), "1/2");
        find.step(true);
        assert_eq!(find.counter(), "2/2");
        find.refresh(Path::new("a.rs"), "猫 apps apps", "absent", 0, || 0);
        find.step(true);
        assert_eq!(find.counter(), "0/0");
        assert!(find.active().is_none());
    }
    #[test]
    fn edits_and_file_switches_refresh_offsets_and_start_near_caret() {
        let mut find = FileFind::default();
        find.refresh(Path::new("a.rs"), "apps apps", "apps", 0, || 5);
        assert_eq!(find.counter(), "2/2");
        assert!(!find.refresh(Path::new("a.rs"), "apps apps", "apps", 0, || 5));
        find.refresh(Path::new("b.rs"), "apps", "apps", 0, || 0);
        assert_eq!(find.counter(), "1/1");
        find.refresh(Path::new("b.rs"), "", "apps", 1, || 0);
        assert_eq!(find.counter(), "0/0");
        find.refresh(Path::new("b.rs"), "apps", "", 2, || 0);
        assert!(find.matches.is_empty());
    }
}
