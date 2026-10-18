use std::{
    ops::Range,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub struct FileFind {
    path: PathBuf,
    query: String,
    pub matches: Vec<Range<usize>>,
    pub current: Option<usize>,
}

impl FileFind {
    /// Match offsets are bytes; the editor converts to character offsets at its boundary.
    pub fn refresh(&mut self, path: &Path, text: &str, query: &str, caret: usize) -> bool {
        let matches: Vec<_> = if query.is_empty() {
            vec![]
        } else {
            text.match_indices(query)
                .map(|(start, _)| start..start + query.len())
                .collect()
        };
        let reset = self.path != path || self.query != query;
        let previous = self.active().cloned();
        if reset || self.matches != matches {
            self.path = path.to_path_buf();
            self.query = query.to_owned();
            let anchor = if reset {
                caret
            } else {
                previous.as_ref().map_or(caret, |r| r.start)
            };
            self.current = (!matches.is_empty())
                .then(|| matches.iter().position(|r| r.start >= anchor).unwrap_or(0));
            self.matches = matches;
        }
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
    fn navigation_wraps_and_empty_results_are_safe() {
        let mut find = FileFind::default();
        find.refresh(Path::new("a.rs"), "猫 apps apps", "apps", 0);
        assert_eq!(find.counter(), "1/2");
        assert_eq!(find.active(), Some(&(4..8)));
        find.step(false);
        assert_eq!(find.counter(), "2/2");
        find.step(false);
        assert_eq!(find.counter(), "1/2");
        find.step(true);
        assert_eq!(find.counter(), "2/2");
        find.refresh(Path::new("a.rs"), "猫 apps apps", "absent", 0);
        find.step(true);
        assert_eq!(find.counter(), "0/0");
        assert!(find.active().is_none());
    }
    #[test]
    fn edits_and_file_switches_refresh_offsets_and_start_near_caret() {
        let mut find = FileFind::default();
        find.refresh(Path::new("a.rs"), "apps apps", "apps", 5);
        assert_eq!(find.counter(), "2/2");
        assert!(!find.refresh(Path::new("a.rs"), "apps apps", "apps", 5));
        find.refresh(Path::new("b.rs"), "apps", "apps", 0);
        assert_eq!(find.counter(), "1/1");
        find.refresh(Path::new("b.rs"), "", "apps", 0);
        assert_eq!(find.counter(), "0/0");
        find.refresh(Path::new("b.rs"), "apps", "", 0);
        assert!(find.matches.is_empty());
    }
}
