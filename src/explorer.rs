use eframe::egui;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq)]
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
    if folder
        && ui
            .add_enabled(path.is_dir(), egui::Button::new("New file…"))
            .clicked()
    {
        action = Some(FileAction::NewFile(path.into()));
        ui.close();
    }
    if path != root
        && ui
            .add_enabled(path.exists(), egui::Button::new("Rename…"))
            .clicked()
    {
        action = Some(FileAction::Rename(path.into()));
        ui.close();
    }
    if !folder
        && ui
            .add_enabled(path.is_file(), egui::Button::new("Delete file…"))
            .clicked()
    {
        action = Some(FileAction::Delete(path.into()));
        ui.close();
    }
    action
}

#[derive(Default)]
pub struct FileTree {
    pub action: Option<FileAction>,
    pub diff: Option<PathBuf>,
    children: BTreeMap<OsString, FileTree>,
    file: Option<PathBuf>,
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
        self.reveal_target = None;
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

    pub fn build_with_folders(root: &Path, files: &[PathBuf], folders: &[PathBuf]) -> Self {
        let mut tree = Self::build(root, files);
        for path in folders {
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
        }
        tree
    }

    /// Refresh entries while retaining reveal commands. Folder expansion is
    /// persisted by egui under path-based IDs, so it survives this rebuild.
    pub fn refresh(&mut self, root: &Path, files: &[PathBuf], folders: &[PathBuf]) {
        let mut fresh = Self::build_with_folders(root, files, folders);
        fresh.last_active = self.last_active.take();
        fresh.reveal_requested = self.reveal_requested;
        fresh.reveal_target = self.reveal_target.take();
        if let Some(target) = fresh.reveal_target.clone() {
            // Keep explicitly revealed empty/ignored folders in the rebuilt tree.
            let pending = fresh.reveal_requested;
            fresh.reveal_path(root, &target);
            fresh.reveal_requested = pending;
        }
        fresh.action = self.action.take();
        fresh.diff = self.diff.take();
        fn keep_commands(fresh: &mut FileTree, old: &mut FileTree) {
            fresh.open_override = old.open_override.take();
            for (name, child) in &mut fresh.children {
                if let Some(previous) = old.children.get_mut(name) {
                    keep_commands(child, previous);
                }
            }
        }
        keep_commands(&mut fresh, self);
        *self = fresh;
    }

    #[cfg(test)]
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

    fn matches_scope(
        &self,
        root: &Path,
        filter: &crate::chrome::CaseFilter,
        colors: &BTreeMap<PathBuf, egui::Color32>,
        changed_only: bool,
        hide_tests: bool,
    ) -> bool {
        if !changed_only && filter.is_empty() && !hide_tests {
            return true;
        }
        self.file.as_ref().is_some_and(|path| {
            (!changed_only || colors.contains_key(path))
                && (!hide_tests || !crate::test_files::is_test_file(path))
                && filter.matches(
                    path.strip_prefix(root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .as_ref(),
                )
        }) || self
            .children
            .values()
            .any(|child| child.matches_scope(root, filter, colors, changed_only, hide_tests))
    }

    #[cfg(test)]
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        root: &Path,
        active: Option<&Path>,
        filter: &str,
        colors: &BTreeMap<PathBuf, egui::Color32>,
    ) -> Option<PathBuf> {
        self.show_scoped(ui, root, active, filter, colors, false, false)
    }

    /// The same tree and row renderer serve both file scopes. Status entries may
    /// include deleted files which are absent from the on-disk project index.
    pub fn show_scoped(
        &mut self,
        ui: &mut egui::Ui,
        root: &Path,
        active: Option<&Path>,
        filter: &str,
        colors: &BTreeMap<PathBuf, egui::Color32>,
        changed_only: bool,
        hide_tests: bool,
    ) -> Option<PathBuf> {
        let requested = std::mem::take(&mut self.reveal_requested);
        let active_changed = self.last_active.as_deref() != active;
        if active_changed && !requested {
            self.reveal_target = None;
        }
        // Track editor navigation separately from an explicit tree selection.
        // Otherwise the next frame interprets the still-open file as a new
        // selection and immediately scrolls back to it.
        self.last_active = active.map(Path::to_path_buf);
        let target = self.reveal_target.clone();
        let selected = target.as_deref().or(active);
        let opened = self.show_children(
            ui,
            root,
            root,
            selected,
            &crate::chrome::CaseFilter::new(filter),
            requested || active_changed,
            colors,
            changed_only,
            hide_tests,
        );
        if opened.is_some() {
            self.reveal_target = None;
        }
        opened
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
        changed_only: bool,
        hide_tests: bool,
    ) -> Option<PathBuf> {
        let mut opened = None;
        for is_file in [false, true] {
            for (name, child) in &mut self.children {
                if child.file.is_some() != is_file
                    || !child.matches_scope(root, filter, colors, changed_only, hide_tests)
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
                        if colors.contains_key(file) && ui.button("View diff").clicked() {
                            self.diff = Some(file.clone());
                            ui.close();
                        }
                        if let Some(action) = context_menu(ui, root, file, false) {
                            self.action = Some(action);
                        }
                    });
                    if response.on_hover_text(file.display().to_string()).clicked()
                        && colors.get(file) != Some(&crate::git_view::deleted())
                    {
                        opened = Some(file.clone());
                    }
                } else {
                    let selected_background = (active == Some(path.as_path()))
                        .then(|| ui.painter().add(egui::Shape::Noop));
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
                            if let Some(file) = child.show_children(
                                ui,
                                root,
                                &path,
                                active,
                                filter,
                                reveal,
                                colors,
                                changed_only,
                                hide_tests,
                            ) {
                                opened = Some(file);
                            }
                        },
                    );
                    if let Some(background) = selected_background {
                        let rect = egui::Rect::from_min_max(
                            egui::pos2(ui.clip_rect().left() + 4.0, header_response.rect.top()),
                            header_response.rect.right_bottom(),
                        );
                        ui.painter().set(
                            background,
                            egui::Shape::rect_filled(rect, 4.0, crate::chrome::SELECTED),
                        );
                    }
                    if let Some(path) = child.diff.take() {
                        self.diff = Some(path);
                    }
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
    fn tree_uses_filtered_folder_index_instead_of_rediscovering_ignored_directories()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        let empty = root.join("empty");
        let ignored = root.join("ignored");
        std::fs::create_dir_all(&empty)?;
        std::fs::create_dir_all(&ignored)?;
        let mut tree = FileTree::build_with_folders(root, &[], &[empty]);
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                tree.show(ui, root, None, "", &BTreeMap::new());
            });
        });
        assert!(tree.children.contains_key(&OsString::from("empty")));
        assert!(!tree.children.contains_key(&OsString::from("ignored")));
        Ok(())
    }

    #[test]
    fn revealed_folder_stays_selected_across_frames_and_refresh_until_editor_navigation() {
        let root = Path::new("/project");
        let active = root.join("aaa/active.rs");
        let folder = root.join("zzz-target");
        let mut files = vec![active.clone()];
        files.extend((0..60).map(|i| root.join(format!("folder{i:02}/item.rs"))));
        let mut tree = FileTree::build(root, &files);
        let ctx = egui::Context::default();
        let colors = BTreeMap::new();
        let mut time = 0.0;
        let mut frame = |tree: &mut FileTree, active: &Path| {
            time += 0.1;
            let mut offset = 0.0;
            let _ = ctx.run(
                egui::RawInput {
                    time: Some(time),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(360.0, 260.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let scroll = egui::ScrollArea::vertical()
                            .id_salt("folder-reveal-test")
                            .show(ui, |ui| {
                                tree.show(ui, root, Some(active), "", &colors);
                            });
                        offset = scroll.state.offset.y;
                    });
                },
            );
            offset
        };
        for _ in 0..4 {
            frame(&mut tree, &active);
        }
        tree.reveal_path(root, &folder);
        for _ in 0..10 {
            frame(&mut tree, &active);
        }
        assert_eq!(tree.reveal_target.as_ref(), Some(&folder));
        assert_eq!(tree.last_active.as_ref(), Some(&active));
        assert!(
            frame(&mut tree, &active) > 500.0,
            "folder reveal must not scroll back to the active file"
        );
        tree.refresh(root, &files, &[]);
        assert!(tree.children.contains_key(&OsString::from("zzz-target")));
        assert!(frame(&mut tree, &active) > 500.0);
        tree.reveal_active();
        for _ in 0..10 {
            frame(&mut tree, &active);
        }
        assert!(tree.reveal_target.is_none());
        assert!(frame(&mut tree, &active) < 50.0);
        tree.reveal_path(root, &folder);
        frame(&mut tree, &active);
        frame(&mut tree, &root.join("folder10/item.rs"));
        assert!(
            tree.reveal_target.is_none(),
            "switching editor files must restore normal tree tracking"
        );
    }

    #[test]
    fn refresh_keeps_reveal_and_expansion_commands_but_removes_stale_entries() {
        let root = Path::new("/project");
        let active = root.join("src/main.rs");
        let removed = root.join("src/old.rs");
        let added = root.join("src/new.rs");
        let mut tree = FileTree::build(root, &[active.clone(), removed]);
        tree.last_active = Some(active.clone());
        tree.expand_all(false);
        tree.reveal_path(root, &active);
        tree.refresh(root, &[active.clone(), added], &[]);
        assert_eq!(tree.last_active, Some(active.clone()));
        assert_eq!(tree.reveal_target, Some(active));
        assert!(tree.reveal_pending());
        let src = &tree.children[&OsString::from("src")];
        assert_eq!(src.open_override, Some(false));
        assert!(src.children.contains_key(&OsString::from("new.rs")));
        assert!(!src.children.contains_key(&OsString::from("old.rs")));
    }

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
