//! Worktree management. Git operations run off the UI thread; force removal is opt-in.
use crate::chrome;
use anyhow::{Result, bail};
use eframe::egui::{self, FontId, RichText, Stroke};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc::{self, Receiver},
};

#[derive(Clone, Debug)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub protected: bool,
}

pub fn list(root: &Path) -> Result<Vec<Worktree>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["worktree", "list", "--porcelain", "-z"])
        .output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let mut trees: Vec<Worktree> = Vec::new();
    for field in output.stdout.split(|b| *b == 0) {
        let field = String::from_utf8_lossy(field);
        if let Some(path) = field.strip_prefix("worktree ") {
            trees.push(Worktree {
                path: PathBuf::from(path)
                    .canonicalize()
                    .unwrap_or_else(|_| path.into()),
                branch: "detached HEAD".into(),
                protected: trees.is_empty(),
            });
        } else if let Some(tree) = trees.last_mut() {
            if let Some(branch) = field.strip_prefix("branch refs/heads/") {
                tree.branch = branch.into();
            }
            if field.starts_with("locked") || field == "bare" {
                tree.protected = true;
            }
        }
    }
    Ok(trees)
}

pub fn create(root: &Path, path: &Path, branch: &str) -> Result<()> {
    if branch.trim().is_empty() || branch.starts_with('-') {
        bail!("Enter a valid new branch name");
    }
    let output = Command::new("git")
        .current_dir(root)
        .args(["worktree", "add", "-b", branch, "--"])
        .arg(path)
        .output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

pub fn remove(root: &Path, path: &Path, force: bool) -> Result<()> {
    let trees = list(root)?;
    if !trees.iter().any(|t| t.path == path && !t.protected) {
        bail!("This worktree cannot be removed");
    }
    let mut command = Command::new("git");
    command.current_dir(root).args(["worktree", "remove"]);
    if force {
        command.arg("--force");
    }
    let output = command.arg("--").arg(path).output()?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

#[derive(Default)]
pub struct Manager {
    project: PathBuf,
    trees: Vec<Worktree>,
    rx: Option<Receiver<Result<Vec<Worktree>>>>,
    name: String,
    branch: String,
    custom_branch: bool,
    path: String,
    custom_path: bool,
    delete: Option<PathBuf>,
    deleting: bool,
    delete_force: bool,
    error: Option<String>,
    create_open: bool,
    create_focus: bool,
    creating: bool,
    create_root: PathBuf,
}
impl Manager {
    pub fn modal(&self) -> bool {
        self.create_open || self.delete.is_some()
    }

    fn open_create(&mut self, root: &Path) {
        self.create_root = root.to_path_buf();
        self.update_default_path(root);
        self.create_open = true;
        self.create_focus = true;
        self.error = None;
    }

    pub fn poll(&mut self) {
        if let Some(rx) = &self.rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.rx = None;
                    match result {
                        Ok(trees) => {
                            self.trees = trees;
                            if self.deleting {
                                self.delete = None;
                                self.delete_force = false;
                            }
                            if self.creating {
                                self.create_open = false;
                                self.name.clear();
                                self.branch.clear();
                                self.path.clear();
                                self.custom_path = false;
                                self.custom_branch = false;
                            }
                        }
                        Err(error) => self.error = Some(error.to_string()),
                    }
                    self.creating = false;
                    self.deleting = false;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.rx = None;
                    self.creating = false;
                    self.deleting = false;
                    self.error = Some("Worktree operation interrupted".into());
                }
                _ => {}
            }
        }
    }

    pub fn dialog(&mut self, ctx: &egui::Context) {
        self.poll();
        if self.delete.is_some() {
            self.delete_dialog(ctx);
            return;
        }
        if !self.create_open {
            return;
        }
        let root = self.create_root.clone();
        self.update_default_path(&root);
        let mut submit = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("new-worktree-dialog"))
            .frame(chrome::popup_frame())
            .show(ctx, |ui| {
                chrome::modal_style(ui, 480.0);
                cancel = chrome::dialog_header(
                    ui,
                    "+",
                    "New worktree",
                    "A separate workspace for your next change",
                    chrome::BLUE,
                );
                ui.add_enabled_ui(!self.creating, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height((ctx.content_rect().height() - 240.0).max(140.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new("Worktree name").size(11.0).strong());
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut self.name)
                                    .id(egui::Id::new("worktree-name"))
                                    .desired_width(f32::INFINITY)
                                    .hint_text("e.g. fix-login")
                                    .margin(egui::vec2(8.0, 7.0)),
                            );
                            if std::mem::take(&mut self.create_focus) {
                                response.request_focus();
                            }
                            if response.changed() {
                                self.update_default_path(&root);
                            }
                            ui.label(
                                RichText::new("The branch and folder follow this name by default.")
                                    .size(11.0)
                                    .color(chrome::MUTED),
                            );
                            ui.label(RichText::new("Branch name").size(11.0).strong());
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut self.branch)
                                        .desired_width(f32::INFINITY)
                                        .margin(egui::vec2(8.0, 7.0)),
                                )
                                .changed()
                            {
                                self.custom_branch = !self.branch.trim().is_empty();
                            }
                            if self.custom_branch && ui.small_button("Use worktree name").clicked()
                            {
                                self.custom_branch = false;
                                self.update_default_path(&root);
                            }
                            ui.add_space(2.0);
                            ui.label(RichText::new("Folder location").size(11.0).strong());
                            if ui
                                .add(
                                    egui::TextEdit::singleline(&mut self.path)
                                        .desired_width(f32::INFINITY)
                                        .margin(egui::vec2(8.0, 7.0)),
                                )
                                .on_hover_text(&self.path)
                                .changed()
                            {
                                self.custom_path = !self.path.trim().is_empty();
                            }
                            ui.horizontal(|ui| {
                                if ui.small_button("Browse…").clicked()
                                    && let Some(path) = rfd::FileDialog::new().pick_folder()
                                {
                                    self.path = path.to_string_lossy().into();
                                    self.custom_path = true;
                                }
                                if self.custom_path && ui.small_button("Use default").clicked() {
                                    self.custom_path = false;
                                    self.update_default_path(&root);
                                }
                            });
                        });
                });
                if let Some(error) = &self.error {
                    chrome::dialog_card(ui, |ui| {
                        ui.colored_label(chrome::DANGER, error);
                    });
                }
                chrome::dialog_divider(ui);
                let ready = self.rx.is_none()
                    && !self.name.trim().is_empty()
                    && !self.branch.trim().is_empty()
                    && !self.path.trim().is_empty();
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    submit = ui
                        .add_enabled_ui(ready, |ui| {
                            chrome::dialog_button(ui, "Create worktree", Some(chrome::BLUE))
                        })
                        .inner
                        .clicked();
                    if self.creating {
                        ui.spinner();
                        ui.label("Creating worktree…");
                    }
                    cancel |= ui
                        .add_enabled_ui(!self.creating, |ui| {
                            chrome::dialog_button(ui, "Cancel", None)
                        })
                        .inner
                        .clicked();
                });
                if !self.creating {
                    cancel |=
                        ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                    submit |= ready
                        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
                }
            });
        if cancel && !self.creating {
            self.create_open = false;
        } else if submit {
            let path = PathBuf::from(self.path.trim());
            let path = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            let branch = self.branch.trim().to_owned();
            self.creating = true;
            self.run(&root, move |root| create(root, &path, &branch), ctx);
        }
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(path) = self.delete.clone() else {
            return;
        };
        let mut submit = false;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("delete-worktree-dialog"))
            .frame(chrome::popup_frame())
            .show(ctx, |ui| {
                chrome::modal_style(ui, 480.0);
                cancel = ui
                    .add_enabled_ui(!self.deleting, |ui| {
                        chrome::dialog_header(
                            ui,
                            "!",
                            "Delete worktree?",
                            "The folder will be removed from disk. The branch is kept.",
                            chrome::DANGER,
                        )
                    })
                    .inner;
                egui::ScrollArea::vertical()
                    .max_height((ctx.content_rect().height() - 280.0).max(140.0))
                    .show(ui, |ui| {
                        ui.label(path.display().to_string());
                        ui.add_enabled_ui(!self.deleting, |ui| {
                            ui.checkbox(&mut self.delete_force, "Force delete (--force)");
                        });
                        if self.delete_force {
                            ui.colored_label(
                                chrome::WARNING,
                                "Uncommitted changes and untracked files in this worktree will be permanently deleted.",
                            );
                        }
                        if let Some(error) = &self.error {
                            chrome::dialog_card(ui, |ui| {
                                ui.colored_label(chrome::DANGER, error);
                            });
                        }
                    });
                chrome::dialog_divider(ui);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    submit = ui
                        .add_enabled_ui(self.rx.is_none(), |ui| {
                            chrome::dialog_button(ui, "Delete worktree", Some(chrome::DANGER))
                        })
                        .inner
                        .clicked();
                    if self.deleting {
                        ui.spinner();
                        ui.label("Deleting worktree…");
                    }
                    cancel |= ui
                        .add_enabled_ui(!self.deleting, |ui| {
                            chrome::dialog_button(ui, "Cancel", None)
                        })
                        .inner
                        .clicked();
                });
                if !self.deleting {
                    cancel |=
                        ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                }
            });
        if cancel && !self.deleting {
            self.delete = None;
            self.delete_force = false;
            self.error = None;
        } else if submit {
            self.deleting = true;
            let root = self.project.clone();
            let force = self.delete_force;
            self.run(&root, move |root| remove(root, &path, force), ctx);
        }
    }

    fn update_default_path(&mut self, root: &Path) {
        if !self.custom_branch {
            self.branch = self.name.trim().to_owned();
        }
        if !self.custom_path {
            // A branch may contain slashes; keep the worktree a single sibling directory.
            let name = self.name.trim().replace(['/', '\\'], "-");
            let name = if name == "." || name == ".." {
                ""
            } else {
                &name
            };
            self.path = root
                .parent()
                .unwrap_or(root)
                .join(name)
                .to_string_lossy()
                .into_owned();
        }
    }

    fn run(
        &mut self,
        root: &Path,
        operation: impl FnOnce(&Path) -> Result<()> + Send + 'static,
        ctx: &egui::Context,
    ) {
        let root = root.to_owned();
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.error = None;
        std::thread::spawn(move || {
            let result = operation(&root).and_then(|_| list(&root));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    pub fn content(
        &mut self,
        ui: &mut egui::Ui,
        root: &Path,
        dirty: &[PathBuf],
    ) -> Option<PathBuf> {
        self.poll();
        let mut switch = None;
        ui.vertical(|ui| {
            ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
            if self.project != root && self.rx.is_none() {
                self.project = root.to_path_buf();
                self.trees.clear();
                self.delete = None;
                self.delete_force = false;
                self.run(root, |_| Ok(()), ui.ctx());
            }
            self.update_default_path(root);
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} worktrees", self.trees.len()))
                        .size(11.0)
                        .color(chrome::MUTED),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.rx.is_some() {
                        ui.spinner();
                    } else if chrome::icon_button(ui, chrome::Icon::Refresh, "Refresh worktrees")
                        .clicked()
                    {
                        self.run(root, |_| Ok(()), ui.ctx());
                    }
                });
            });
            if ui
                .add_enabled(
                    self.rx.is_none(),
                    egui::Button::new(
                        RichText::new("+  New worktree")
                            .strong()
                            .color(chrome::TEXT),
                    )
                    .fill(chrome::HEADER)
                    .stroke(Stroke::new(1.0_f32, chrome::BORDER))
                    .corner_radius(5)
                    .min_size(egui::vec2(ui.available_width(), 30.0)),
                )
                .clicked()
            {
                self.open_create(root);
            }
            ui.add_space(4.0);
            egui::ScrollArea::vertical()
                .id_salt("worktree-list")
                .max_height((ui.available_height() - 8.0).max(80.0))
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for tree in &self.trees {
                        let current = root == tree.path;
                        let unsaved = dirty.contains(&tree.path);
                        let folder = tree
                            .path
                            .file_name()
                            .unwrap_or(tree.path.as_os_str())
                            .to_string_lossy();
                        let (_, rect) = ui.allocate_space(egui::vec2(ui.available_width(), 56.0));
                        let body =
                            egui::Rect::from_min_max(rect.min, rect.max - egui::vec2(42.0, 0.0));
                        let response = ui.interact(
                            body,
                            ui.make_persistent_id(("worktree", &tree.path)),
                            egui::Sense::click(),
                        );
                        response.widget_info(|| {
                            egui::WidgetInfo::selected(
                                egui::WidgetType::SelectableLabel,
                                ui.is_enabled(),
                                current,
                                format!(
                                    "{folder}, branch {}{}",
                                    tree.branch,
                                    if unsaved { ", unsaved changes" } else { "" }
                                ),
                            )
                        });
                        let fill = if current {
                            chrome::SELECTED
                        } else if response.hovered() || response.has_focus() {
                            chrome::HEADER
                        } else {
                            egui::Color32::TRANSPARENT
                        };
                        ui.painter().rect(
                            rect,
                            5,
                            fill,
                            Stroke::new(
                                1.0_f32,
                                if current || response.has_focus() {
                                    chrome::BLUE.gamma_multiply(0.4)
                                } else {
                                    egui::Color32::TRANSPARENT
                                },
                            ),
                            egui::StrokeKind::Inside,
                        );
                        if current {
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(
                                    rect.min + egui::vec2(0.0, 11.0),
                                    egui::vec2(3.0, 34.0),
                                ),
                                2,
                                chrome::BLUE,
                            );
                        }
                        let title =
                            egui::WidgetText::from(RichText::new(folder).strong().size(13.0))
                                .into_galley(
                                    ui,
                                    Some(egui::TextWrapMode::Truncate),
                                    (rect.width() - 58.0).max(1.0),
                                    FontId::proportional(13.0),
                                );
                        ui.painter()
                            .galley(rect.min + egui::vec2(13.0, 9.0), title, chrome::TEXT);
                        chrome::paint_icon(
                            ui.painter(),
                            rect.min + egui::vec2(20.0, 36.0),
                            chrome::Icon::Branch,
                            chrome::MUTED,
                        );
                        let branch = egui::WidgetText::from(RichText::new(&tree.branch).size(11.0))
                            .into_galley(
                                ui,
                                Some(egui::TextWrapMode::Truncate),
                                (rect.width() - 75.0).max(1.0),
                                FontId::proportional(11.0),
                            );
                        ui.painter().galley(
                            rect.min + egui::vec2(34.0, 30.0),
                            branch,
                            chrome::MUTED,
                        );
                        if current || unsaved {
                            ui.painter().circle_filled(
                                egui::pos2(rect.right() - 22.0, rect.top() + 38.0),
                                3.0,
                                if unsaved {
                                    chrome::WARNING
                                } else {
                                    chrome::GREEN
                                },
                            );
                        }
                        if response.clicked() && !current && self.rx.is_none() {
                            switch = Some(tree.path.clone());
                        }
                        let busy = self.rx.is_some();
                        let mut actions = |ui: &mut egui::Ui| {
                            ui.label(RichText::new(&tree.branch).size(11.0).color(chrome::MUTED));
                            ui.separator();
                            if ui
                                .add_enabled(
                                    !busy && !current,
                                    egui::Button::new("Switch to worktree"),
                                )
                                .clicked()
                            {
                                switch = Some(tree.path.clone());
                                ui.close();
                            }
                            if ui.button("Copy folder path").clicked() {
                                ui.ctx().copy_text(tree.path.display().to_string());
                                ui.close();
                            }
                            ui.separator();
                            if ui
                                .add_enabled(
                                    !busy && !current && !tree.protected && !unsaved,
                                    egui::Button::new(
                                        RichText::new("Delete worktree…").color(chrome::WARNING),
                                    ),
                                )
                                .on_disabled_hover_text(if current {
                                    "Switch to another worktree first"
                                } else if unsaved {
                                    "Save your changes first"
                                } else {
                                    "The main or locked worktree is protected"
                                })
                                .clicked()
                            {
                                self.delete = Some(tree.path.clone());
                                self.delete_force = false;
                                self.error = None;
                                ui.close();
                            }
                        };
                        response
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text(format!("{}\n{}", tree.path.display(), tree.branch))
                            .context_menu(&mut actions);
                        let menu_rect = egui::Rect::from_min_size(
                            egui::pos2(rect.right() - 36.0, rect.top() + 7.0),
                            egui::vec2(28.0, 28.0),
                        );
                        // An overlay must not advance (or rewind) the list's cursor.
                        let mut menu_ui = ui.new_child(
                            egui::UiBuilder::new()
                                .id_salt(("worktree-actions", &tree.path))
                                .max_rect(menu_rect)
                                .layout(egui::Layout::top_down(egui::Align::Center)),
                        );
                        menu_ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                        menu_ui.spacing_mut().button_padding = egui::vec2(4.0, 4.0);
                        menu_ui.visuals_mut().widgets.inactive.weak_bg_fill =
                            egui::Color32::TRANSPARENT;
                        menu_ui.visuals_mut().widgets.inactive.bg_stroke = Stroke::NONE;
                        menu_ui
                            .menu_button("···", &mut actions)
                            .response
                            .on_hover_text("Workspace actions");
                    }
                    if self.trees.is_empty() && self.rx.is_none() {
                        ui.label(RichText::new("No worktrees to display.").color(chrome::MUTED));
                    }
                });
            if !self.create_open
                && self.delete.is_none()
                && let Some(error) = &self.error
            {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
        });
        switch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn deletion_dialog_stays_open_on_failure_and_closes_on_success() {
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let path = PathBuf::from("/projects/feature");
        let mut manager = Manager {
            project: "/projects/main".into(),
            delete: Some(path.clone()),
            deleting: true,
            delete_force: true,
            ..Default::default()
        };
        let (tx, rx) = mpsc::channel();
        manager.rx = Some(rx);
        let frame = |manager: &mut Manager, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 800.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| manager.dialog(ctx),
            )
        };
        let escape = || {
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]
        };
        frame(&mut manager, vec![]);
        frame(&mut manager, escape());
        assert!(
            manager.modal(),
            "deletion cannot be dismissed while running"
        );
        tx.send(Err(anyhow::anyhow!("Worktree contains modified files")))
            .unwrap();
        frame(&mut manager, vec![]);
        let output = frame(&mut manager, vec![]);
        assert!(manager.modal());
        assert_eq!(manager.delete.as_ref(), Some(&path));
        assert!(!manager.deleting);
        assert!(
            manager.delete_force,
            "a failed attempt retains the force option"
        );
        assert!(
            output.shapes.iter().any(|shape| matches!(
                &shape.shape,
                egui::Shape::Text(text)
                    if text.galley.text() == "Worktree contains modified files"
            )),
            "the deletion error must appear inside the dialog"
        );

        // A retry clears the error and closes the dialog only after success.
        let (tx, rx) = mpsc::channel();
        manager.error = None;
        manager.deleting = true;
        manager.rx = Some(rx);
        frame(&mut manager, vec![]);
        assert!(manager.modal());
        tx.send(Ok(vec![])).unwrap();
        frame(&mut manager, vec![]);
        assert!(!manager.modal());
        assert!(!manager.deleting);
        assert!(!manager.delete_force);

        // Interrupted operations also retain the target and permit cancellation.
        manager.delete = Some(path.clone());
        manager.deleting = true;
        manager.delete_force = true;
        let (tx, rx) = mpsc::channel();
        manager.rx = Some(rx);
        drop(tx);
        frame(&mut manager, vec![]);
        assert!(manager.modal());
        assert_eq!(
            manager.error.as_deref(),
            Some("Worktree operation interrupted")
        );
        frame(&mut manager, escape());
        assert!(!manager.modal());
        assert!(manager.error.is_none());
        assert!(!manager.delete_force);
    }

    #[test]
    fn workspace_actions_do_not_switch_tabs_and_creation_stays_above_the_list() {
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let root = Path::new("/projects/main");
        let mut manager = Manager {
            project: root.into(),
            trees: (0..10)
                .map(|index| Worktree {
                    path: if index == 0 {
                        root.into()
                    } else {
                        PathBuf::from(format!("/projects/feature-{index}"))
                    },
                    branch: format!("branch-{index}"),
                    protected: index == 0,
                })
                .collect(),
            ..Default::default()
        };
        let frame = |manager: &mut Manager, events| {
            let mut action = None;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(240.0, 600.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        action = manager.content(ui, root, &[]);
                    });
                },
            );
            (output, action)
        };
        frame(&mut manager, vec![]);
        let (output, _) = frame(&mut manager, vec![]);
        let label = |output: &egui::FullOutput, name| {
            output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == name => {
                    Some(text.pos + text.galley.size() / 2.0)
                }
                _ => None,
            })
        };
        let create = label(&output, "+  New worktree").unwrap();
        let first = label(&output, "main").unwrap();
        assert!(create.y < first.y);
        let second = label(&output, "feature-1").unwrap();
        assert!(second.y - first.y >= 56.0, "worktree rows must not overlap");
        let menu = label(&output, "···").unwrap();
        let click = |manager: &mut Manager, point| {
            frame(
                manager,
                vec![
                    egui::Event::PointerMoved(point),
                    egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            frame(
                manager,
                vec![egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            )
        };
        let (_, action) = click(&mut manager, menu);
        assert!(
            action.is_none(),
            "opening the action menu must not activate another workspace"
        );
        let (output, _) = frame(&mut manager, vec![]);
        let copy =
            label(&output, "Copy folder path").expect("workspace actions must be discoverable");
        let (output, action) = click(&mut manager, copy);
        assert!(action.is_none());
        assert!(output.platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(path) if path == "/projects/main")));
    }

    #[test]
    fn typing_worktree_name_fills_in_branch_and_directory() {
        let root = Path::new("/projects/coder");
        let mut manager = Manager {
            project: root.into(),
            ..Default::default()
        };
        manager.open_create(root);
        let ctx = egui::Context::default();
        let frame = |manager: &mut Manager, events| {
            let _ = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        manager.content(ui, root, &[]);
                    });
                    manager.dialog(ctx);
                },
            );
        };
        frame(&mut manager, vec![]);
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("worktree-name")));
        frame(&mut manager, vec![egui::Event::Text("fix-login".into())]);
        assert_eq!(manager.name, "fix-login");
        assert_eq!(manager.branch, "fix-login");
        assert_eq!(manager.path, "/projects/fix-login");
    }

    #[test]
    fn name_drives_sibling_directory_and_branch_until_overridden() {
        let root = Path::new("/projects/coder");
        let mut manager = Manager {
            name: "fix-login".into(),
            ..Default::default()
        };
        manager.update_default_path(root);
        assert_eq!(manager.path, "/projects/fix-login");
        assert_eq!(manager.branch, "fix-login");
        manager.name = "feature/search".into();
        manager.update_default_path(root);
        assert_eq!(manager.path, "/projects/feature-search");
        assert_eq!(manager.branch, "feature/search");
        manager.custom_path = true;
        manager.path = "/custom/location".into();
        manager.custom_branch = true;
        manager.branch = "custom-branch".into();
        manager.name = "new-name".into();
        manager.update_default_path(root);
        assert_eq!(manager.path, "/custom/location");
        assert_eq!(manager.branch, "custom-branch");
        manager.custom_path = false;
        manager.custom_branch = false;
        manager.update_default_path(root);
        assert_eq!(manager.path, "/projects/new-name");
        assert_eq!(manager.branch, "new-name");
    }

    #[test]
    fn lifecycle_protects_main_and_dirty_worktrees() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("repo");
        std::fs::create_dir(&root)?;
        for args in [
            vec!["init"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "initial",
            ],
        ] {
            assert!(
                Command::new("git")
                    .current_dir(&root)
                    .args(args)
                    .output()?
                    .status
                    .success()
            );
        }
        let root = root.canonicalize()?;
        let path = root.parent().unwrap().join("feature space");
        create(&root, &path, "feature/test")?;
        let trees = list(&root)?;
        assert_eq!(trees.len(), 2);
        assert!(trees[0].protected);
        assert_eq!(trees[1].branch, "feature/test");
        assert_eq!(list(&path)?.len(), 2);
        assert!(remove(&path, &root, false).is_err());
        assert!(remove(&path, &root, true).is_err());
        std::fs::write(path.join("unsaved.txt"), "keep me")?;
        assert!(remove(&root, &path, false).is_err());
        assert!(path.join("unsaved.txt").exists());
        remove(&root, &path, true)?;
        assert!(!path.exists(), "force removal deletes a dirty worktree");
        create(&root, &path, "feature/clean")?;
        assert!(
            Command::new("git")
                .current_dir(&root)
                .args(["worktree", "lock", "--"])
                .arg(&path)
                .output()?
                .status
                .success()
        );
        assert!(remove(&root, &path, true).is_err());
        assert!(path.exists(), "locked worktrees remain protected");
        assert!(
            Command::new("git")
                .current_dir(&root)
                .args(["worktree", "unlock", "--"])
                .arg(&path)
                .output()?
                .status
                .success()
        );
        remove(&root, &path, false)?;
        assert_eq!(list(&root)?.len(), 1);
        assert!(create(&root, &path, "-invalid").is_err());
        // Exercise the modal with real Git: creation closes it, failure preserves it.
        let ctx = egui::Context::default();
        let mut manager = Manager {
            project: root.clone(),
            ..Default::default()
        };
        let frame = |manager: &mut Manager, events| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 800.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    manager.dialog(ctx);
                },
            );
        };
        let enter = || {
            vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]
        };
        manager.open_create(&root);
        frame(&mut manager, vec![]);
        frame(
            &mut manager,
            vec![egui::Event::Text("dialog-feature".into())],
        );
        frame(&mut manager, enter());
        assert!(manager.creating);
        let started = std::time::Instant::now();
        while manager.creating && started.elapsed() < std::time::Duration::from_secs(5) {
            manager.poll();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !manager.modal(),
            "successful creation closes the dialog: {:?}",
            manager.error
        );
        let created = root.parent().unwrap().join("dialog-feature");
        assert!(created.join(".git").is_file());
        assert!(
            list(&root)?
                .iter()
                .any(|tree| tree.branch == "dialog-feature")
        );
        manager.name = "dialog-feature".into();
        manager.open_create(&root);
        frame(&mut manager, vec![]);
        frame(&mut manager, enter());
        assert!(manager.creating);
        let started = std::time::Instant::now();
        while manager.creating && started.elapsed() < std::time::Duration::from_secs(5) {
            manager.poll();
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(manager.modal());
        assert!(manager.error.is_some());
        assert_eq!(manager.name, "dialog-feature");
        frame(
            &mut manager,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!manager.modal());

        Ok(())
    }
}
