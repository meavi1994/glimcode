use eframe::egui;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub enum FileAction {
    NewFile(PathBuf),
    Rename(PathBuf),
    Delete(PathBuf),
}

pub fn context_menu(
    ui: &mut egui::Ui,
    root: &Path,
    path: &Path,
    folder: bool,
) -> Option<FileAction> {
    let mut action = None;
    for (label, value) in [
        (
            "Copy file name",
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
        ),
        ("Copy absolute path", path.to_string_lossy().into_owned()),
        (
            "Copy path from content root",
            path.strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned(),
        ),
    ] {
        if ui.button(label).clicked() {
            ui.ctx().copy_text(value);
            ui.close();
        }
    }
    ui.separator();
    if folder && ui.button("New file…").clicked() {
        action = Some(FileAction::NewFile(path.into()));
        ui.close();
    }
    if path != root && ui.button("Rename…").clicked() {
        action = Some(FileAction::Rename(path.into()));
        ui.close();
    }
    if !folder && ui.button("Delete file…").clicked() {
        action = Some(FileAction::Delete(path.into()));
        ui.close();
    }
    action
}

#[derive(Default)]
pub struct FileTree {
    pub action: Option<FileAction>,
    children: BTreeMap<OsString, FileTree>,
    file: Option<PathBuf>,
    directories_loaded: bool,
    last_active: Option<PathBuf>,
    open_override: Option<bool>,
    reveal_requested: bool,
    reveal_target: Option<PathBuf>,
}

impl FileTree {
    pub fn expand_all(&mut self, open: bool) {
        for child in self.children.values_mut() {
            child.open_override = Some(open);
            child.expand_all(open);
        }
    }
    pub fn reveal_active(&mut self) {
        self.reveal_requested = true;
    }

    pub fn reveal_pending(&self) -> bool {
        self.reveal_requested
    }

    pub fn reveal_path(&mut self, root: &Path, path: &Path) {
        let Ok(relative) = path.strip_prefix(root) else {
            return;
        };
        let mut node = &mut *self;
        for component in relative.components() {
            node = node
                .children
                .entry(component.as_os_str().to_owned())
                .or_default();
        }
        self.reveal_target = Some(path.to_path_buf());
        self.reveal_requested = true;
    }

    pub fn build(root: &Path, files: &[PathBuf]) -> Self {
        let mut tree = Self::default();
        for path in files {
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let mut node = &mut tree;
            for component in relative.components() {
                node = node
                    .children
                    .entry(component.as_os_str().to_owned())
                    .or_default();
            }
            node.file = Some(path.clone());
        }
        tree
    }

    fn matches(&self, root: &Path, filter: &crate::chrome::CaseFilter) -> bool {
        self.file.as_ref().is_some_and(|path| {
            filter.matches(
                path.strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .as_ref(),
            )
        }) || self
            .children
            .values()
            .any(|child| child.matches(root, filter))
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        root: &Path,
        active: Option<&Path>,
        filter: &str,
        colors: &BTreeMap<PathBuf, egui::Color32>,
    ) -> Option<PathBuf> {
        let target = self.reveal_target.take();
        let active = target.as_deref().or(active);
        let reveal =
            std::mem::take(&mut self.reveal_requested) || self.last_active.as_deref() != active;
        self.last_active = active.map(Path::to_path_buf);
        self.show_children(
            ui,
            root,
            root,
            active,
            &crate::chrome::CaseFilter::new(filter),
            reveal,
            colors,
        )
    }

    fn show_children(
        &mut self,
        ui: &mut egui::Ui,
        root: &Path,
        directory: &Path,
        active: Option<&Path>,
        filter: &crate::chrome::CaseFilter,
        reveal: bool,
        colors: &BTreeMap<PathBuf, egui::Color32>,
    ) -> Option<PathBuf> {
        let mut opened = None;
        // Include empty folders so they can receive a new file.
        if !self.directories_loaded {
            self.directories_loaded = true;
            for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_dir())
                    && !entry.file_name().to_string_lossy().starts_with('.')
                {
                    self.children.entry(entry.file_name()).or_default();
                }
            }
        }
        for is_file in [false, true] {
            for (name, child) in &mut self.children {
                if child.file.is_some() != is_file
                    || (!filter.is_empty() && !child.matches(root, filter))
                {
                    continue;
                }
                let path = directory.join(name);
                if let Some(file) = &child.file {
                    let (id, rect) = ui.allocate_space(egui::vec2(ui.available_width(), 23.0));
                    // Include the indentation area in the row highlight and click target.
                    let row_rect = egui::Rect::from_min_max(
                        egui::pos2(ui.clip_rect().left() + 4.0, rect.top()),
                        rect.right_bottom(),
                    );
                    let response = ui.interact(row_rect, id, egui::Sense::click());
                    let selected = active == Some(file.as_path());
                    if selected || response.hovered() {
                        ui.painter().rect_filled(
                            row_rect,
                            4.0,
                            if selected {
                                crate::chrome::SELECTED
                            } else {
                                crate::chrome::HEADER
                            },
                        );
                    }
                    ui.put(
                        egui::Rect::from_min_size(rect.min, egui::vec2(24.0, 23.0)),
                        egui::Label::new(crate::chrome::file_icon(file)),
                    );
                    ui.painter().text(
                        rect.left_center() + egui::vec2(29.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        name.to_string_lossy(),
                        egui::FontId::proportional(13.0),
                        colors.get(file).copied().unwrap_or(if selected {
                            egui::Color32::from_rgb(223, 225, 229)
                        } else {
                            crate::chrome::TEXT
                        }),
                    );
                    if reveal && active == Some(file.as_path()) {
                        ui.scroll_to_rect(rect, Some(egui::Align::Center));
                    }
                    response.context_menu(|ui| {
                        if let Some(action) = context_menu(ui, root, file, false) {
                            self.action = Some(action);
                        }
                    });
                    if response.on_hover_text(file.display().to_string()).clicked() {
                        opened = Some(file.clone());
                    }
                } else {
                    let open_override = child.open_override.take();
                    let header_response = crate::chrome::folder_header(
                        ui,
                        ui.make_persistent_id(&path),
                        &name.to_string_lossy(),
                        active.is_some_and(|active| active.starts_with(&path)),
                        if !filter.is_empty()
                            || (reveal && active.is_some_and(|file| file.starts_with(&path)))
                        {
                            Some(true)
                        } else {
                            open_override
                        },
                        |ui| {
                            if let Some(file) =
                                child.show_children(ui, root, &path, active, filter, reveal, colors)
                            {
                                opened = Some(file);
                            }
                        },
                    );
                    if let Some(action) = child.action.take() {
                        self.action = Some(action);
                    }
                    if reveal && active == Some(path.as_path()) {
                        ui.scroll_to_rect(header_response.rect, Some(egui::Align::Center));
                    }
                    header_response.context_menu(|ui| {
                        if let Some(action) = context_menu(ui, root, &path, true) {
                            self.action = Some(action);
                        }
                    });
                }
            }
        }
        opened
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expand_and_collapse_commands_reach_hidden_descendants() {
        let root = Path::new("/project");
        let mut tree = FileTree::build(root, &[root.join("src/nested/main.rs")]);
        tree.expand_all(false);
        let src = &tree.children[&OsString::from("src")];
        assert_eq!(src.open_override, Some(false));
        assert_eq!(
            src.children[&OsString::from("nested")].open_override,
            Some(false)
        );
        tree.expand_all(true);
        assert_eq!(
            tree.children[&OsString::from("src")].open_override,
            Some(true)
        );
        tree.reveal_active();
        assert!(tree.reveal_requested);
    }
    #[test]
    fn groups_nested_files_and_keeps_matching_ancestors() {
        fn filter(needle: &str) -> crate::chrome::CaseFilter {
            crate::chrome::CaseFilter::new(needle)
        }
        let root = Path::new("/project");
        let tree = FileTree::build(
            root,
            &[root.join("src/nested/main.rs"), root.join("README.md")],
        );
        assert_eq!(tree.children.len(), 2);
        assert!(tree.children[&OsString::from("src")].matches(root, &filter("main.rs")));
        assert!(!tree.children[&OsString::from("src")].matches(root, &filter("readme")));
        assert!(tree.matches(root, &filter("src/nested")));
    }
}
