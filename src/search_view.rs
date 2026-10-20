use crate::navigation::{Location, MatchCategory};
use std::path::Path;

#[derive(Default)]
pub struct SearchView {
    key: Option<(u64, usize, Option<MatchCategory>, Option<u64>)>,
    pub indices: Vec<usize>,
    pub counts: [usize; 6],
    pub total: usize,
    paths: Option<std::collections::HashSet<std::path::PathBuf>>,
    pub extensions: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    Project,
    Opened,
    Current,
}
impl Scope {
    pub fn toggle(&mut self, scope: Self) {
        *self = if *self == scope { Self::Project } else { scope };
    }
}

impl SearchView {
    pub fn set_paths(&mut self, paths: Option<Vec<std::path::PathBuf>>) {
        let paths = paths.map(|paths| paths.into_iter().collect());
        if self.paths != paths {
            self.paths = paths;
            self.key = None;
        }
    }

    pub fn refresh(
        &mut self,
        results: &[Location],
        revision: u64,
        category: Option<MatchCategory>,
        selected_extension: Option<&str>,
    ) {
        let key = (
            revision,
            results.len(),
            category,
            // A hash keeps the key free of allocations on every frame.
            selected_extension.map(eframe::egui::util::hash),
        );
        if self.key.as_ref() == Some(&key) {
            return;
        }
        self.key = Some(key);
        self.indices.clear();
        self.counts = [0; 6];
        self.total = 0;
        let mut extensions = std::collections::BTreeSet::new();
        for (index, location) in results.iter().enumerate() {
            if self
                .paths
                .as_ref()
                .is_some_and(|paths| !paths.contains(&location.path))
            {
                continue;
            }
            let extension = extension(&location.path);
            let include = selected_extension.is_none_or(|selected| extension == selected);
            extensions.insert(extension);
            if include {
                self.total += 1;
                self.counts[location.category as usize] += 1;
                if category.is_none_or(|category| category == location.category) {
                    self.indices.push(index);
                }
            }
        }
        self.extensions = extensions.into_iter().collect();
    }
}

pub fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{}", e.to_lowercase()))
        .unwrap_or_else(|| "(no extension)".into())
}

#[cfg(test)]
pub fn result_visible(
    location: &Location,
    category: Option<MatchCategory>,
    selected_extension: Option<&str>,
) -> bool {
    category.is_none_or(|category| location.category == category)
        && selected_extension.is_none_or(|selected| extension(&location.path) == selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_scope_combines_with_category_and_extension_and_invalidates_counts() {
        let results = [
            Location {
                path: "/p/open.go".into(),
                line: 1,
                column: 1,
                preview: String::new(),
                category: MatchCategory::Usage,
            },
            Location {
                path: "/p/open.go".into(),
                line: 2,
                column: 1,
                preview: String::new(),
                category: MatchCategory::Declaration,
            },
            Location {
                path: "/p/current.kt".into(),
                line: 1,
                column: 1,
                preview: String::new(),
                category: MatchCategory::Usage,
            },
            Location {
                path: "/p/closed.java".into(),
                line: 1,
                column: 1,
                preview: String::new(),
                category: MatchCategory::Usage,
            },
        ];
        let mut view = SearchView::default();
        view.set_paths(Some(vec!["/p/open.go".into(), "/p/current.kt".into()]));
        view.refresh(&results, 0, Some(MatchCategory::Usage), None);
        assert_eq!(view.indices, [0, 2]);
        assert_eq!(view.total, 3);
        assert_eq!(view.counts[MatchCategory::Declaration as usize], 1);
        view.set_paths(Some(vec!["/p/current.kt".into()]));
        view.refresh(&results, 0, Some(MatchCategory::Usage), Some(".kt"));
        assert_eq!(view.indices, [2]);
        assert_eq!(view.total, 1);
        assert_eq!(view.extensions, [".kt"]);
        view.set_paths(Some(vec![]));
        view.refresh(&results, 0, None, None);
        assert_eq!(view.total, 0);
        view.set_paths(None);
        view.refresh(&results, 0, None, None);
        assert_eq!(view.total, 4);
    }
}
