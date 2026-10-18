use crate::{
    chrome, diff,
    git::{self, BlameLine, CommitDetail, FileData, PullRequest, Repository},
    preview::SourcePreview,
};
use eframe::egui::{self, Color32, RichText};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

type Job<T> = Receiver<Result<T, String>>;
fn worker<T: Send + 'static>(
    ctx: &egui::Context,
    task: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Job<T> {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let result = task().map_err(|e| format!("{e:#}"));
        let _ = tx.send(result);
        ctx.request_repaint();
    });
    rx
}
fn received<T>(rx: &Option<Job<T>>) -> Option<Result<T, String>> {
    rx.as_ref().and_then(|rx| match rx.try_recv() {
        Ok(result) => Some(result),
        Err(mpsc::TryRecvError::Disconnected) => Some(Err("Background operation stopped".into())),
        Err(mpsc::TryRecvError::Empty) => None,
    })
}
#[derive(Debug, PartialEq)]
pub enum ChangeAction {
    Open(PathBuf),
    Diff(PathBuf),
}
impl ChangeAction {
    fn absolute(self, root: &Path) -> Self {
        match self {
            Self::Open(path) => Self::Open(root.join(path)),
            Self::Diff(path) => Self::Diff(root.join(path)),
        }
    }
}

#[derive(Default)]
pub struct GitTool {
    pub visible: bool,
    pub show_blame: bool,
    pub repo: Option<Repository>,
    pub note: Option<String>,
    pub file_note: Option<String>,
    project: PathBuf,
    selected_base: Option<String>,
    repo_rx: Option<Job<Repository>>,
    last_refresh: Option<Instant>,
    pr: Option<PullRequest>,
    pr_note: Option<String>,
    pr_rx: Option<Job<Option<PullRequest>>>,
    pr_key: Option<String>,
    file_rx: Option<Job<(u64, FileData)>>,
    file_key: Option<u64>,
    file: Option<(u64, Arc<FileData>)>,
    desired: Option<(u64, Instant)>,
    failed_file: Option<u64>,
    review: Option<Review>,
    review_mode: DiffMode,
    review_rx: Option<Job<Review>>,
    review_next: Option<(Repository, PathBuf, Option<String>, bool)>,
    review_open: bool,
    review_note: Option<String>,
    detail: Option<CommitDetail>,
    detail_rx: Option<Job<CommitDetail>>,
    detail_open: bool,
    detail_note: Option<String>,
    description_cache: egui_commonmark::CommonMarkCache,
    filter: String,
}
impl GitTool {
    pub fn resize_windows(&self) -> Vec<egui::Id> {
        let mut windows = Vec::new();
        if self.review_open {
            windows.push(egui::Id::new("branch-review"));
        }
        if self.detail_open {
            windows.push(egui::Id::new("blame-detail"));
        }
        windows
    }

    pub fn reset(&mut self, project: &Path, ctx: &egui::Context) {
        *self = Self::default();
        self.refresh(project, ctx);
    }
    pub fn refresh(&mut self, project: &Path, ctx: &egui::Context) {
        if self.repo_rx.is_some() {
            return;
        }
        self.project = project.to_path_buf();
        let root = self.project.clone();
        let base = self.selected_base.clone();
        self.repo_rx = Some(worker(ctx, move || git::repository(&root, base.as_deref())));
        self.last_refresh = Some(Instant::now());
    }
    pub fn poll(&mut self, project: &Path, ctx: &egui::Context) {
        if self.project != project {
            self.reset(project, ctx);
        }
        if let Some(result) = received(&self.repo_rx) {
            self.repo_rx = None;
            match result {
                Ok(repo) => {
                    let changed = self.repo.as_ref().is_none_or(|old| {
                        old.head != repo.head || old.merge_base != repo.merge_base
                    });
                    if changed {
                        self.file = None;
                        self.failed_file = None;
                    }
                    self.selected_base = Some(repo.base.clone());
                    self.repo = Some(repo);
                    self.note = None;
                }
                Err(error) => self.note = Some(error),
            }
        }
        if let Some(result) = received(&self.file_rx) {
            self.file_rx = None;
            match result {
                Ok((key, data)) => {
                    if self.desired.is_some_and(|(desired, _)| desired == key) {
                        self.file = Some((key, Arc::new(data)));
                        self.file_note = None;
                    }
                }
                Err(error) => {
                    self.failed_file = self.file_key;
                    if self.desired.map(|(key, _)| key) == self.file_key {
                        self.file_note = Some(error);
                    }
                }
            }
        }
        if self.file_rx.is_none() {
            self.file_key = None;
        }
        if let Some(result) = received(&self.pr_rx) {
            self.pr_rx = None;
            match result {
                Ok(pr) => {
                    self.pr = pr;
                    self.pr_note = None;
                }
                Err(note) => self.pr_note = Some(note),
            }
        }
        if let Some(result) = received(&self.review_rx) {
            self.review_rx = None;
            if let Some((repo, path, buffer, dark)) = self.review_next.take() {
                self.review_rx = Some(worker(ctx, move || Review::new(&repo, path, buffer, dark)));
            } else {
                match result {
                    Ok(review) => {
                        self.review = Some(review);
                        self.review_note = None;
                    }
                    Err(note) => self.review_note = Some(note),
                }
            }
        }
        if let Some(result) = received(&self.detail_rx) {
            self.detail_rx = None;
            match result {
                Ok(detail) => {
                    self.detail = Some(detail);
                    self.detail_note = None;
                }
                Err(note) => self.detail_note = Some(note),
            }
        }
        if self.repo.is_some()
            && self.repo_rx.is_none()
            && self
                .last_refresh
                .is_some_and(|t| t.elapsed() > Duration::from_secs(4))
        {
            let root = self.project.clone();
            self.refresh(&root, ctx);
        }
        if self.visible
            && let Some(repo) = &self.repo
        {
            let key = format!("{}:{}", repo.root.display(), repo.branch);
            if self.pr_key.as_ref() != Some(&key) && self.pr_rx.is_none() {
                let repo = repo.clone();
                self.pr_key = Some(key);
                self.pr = None;
                self.pr_note = None;
                self.pr_rx = Some(worker(ctx, move || git::branch_pr(&repo)));
            }
        }
        if self.repo_rx.is_some()
            || self.file_rx.is_some()
            || self.pr_rx.is_some()
            || self.review_rx.is_some()
            || self.detail_rx.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if self.repo.is_some() {
            ctx.request_repaint_after(Duration::from_secs(2));
        }
    }
    pub fn ensure_file(
        &mut self,
        path: &Path,
        source: &str,
        ctx: &egui::Context,
    ) -> Option<Arc<FileData>> {
        let repo = self.repo.as_ref()?;
        if self
            .selected_base
            .as_deref()
            .is_some_and(|base| base != repo.base)
        {
            return None;
        }
        if !path.starts_with(&repo.root) {
            return None;
        }
        let key = egui::util::hash((&repo.head, &repo.merge_base, path, source, self.show_blame));
        if self.desired.is_none_or(|(previous, _)| previous != key) {
            self.desired = Some((key, Instant::now()));
        }
        let matching = self
            .file
            .as_ref()
            .filter(|(stored, _)| *stored == key)
            .map(|(_, data)| data.clone());
        if matching.is_none() && self.file_rx.is_none() && self.failed_file != Some(key) {
            let wait = self.file.is_some()
                && self
                    .desired
                    .is_some_and(|(_, time)| time.elapsed() < Duration::from_millis(250));
            if wait {
                ctx.request_repaint_after(Duration::from_millis(250));
            } else {
                let repo = repo.clone();
                let path = path.to_path_buf();
                let source = source.to_owned();
                let blame = self.show_blame;
                self.file_key = Some(key);
                self.file_rx = Some(worker(ctx, move || {
                    Ok((key, git::file_data(&repo, &path, Some(source), blame)?))
                }));
            }
        }
        matching
    }
    pub fn open_review(&mut self, path: PathBuf, buffer: Option<String>, ctx: &egui::Context) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.review_open = true;
        self.review = None;
        self.review_note = None;
        let dark = ctx.style().visuals.dark_mode;
        if self.review_rx.is_some() {
            self.review_next = Some((repo, path, buffer, dark));
        } else {
            self.review_rx = Some(worker(ctx, move || Review::new(&repo, path, buffer, dark)));
        }
    }
    pub fn show_commit(&mut self, blame: BlameLine, ctx: &egui::Context) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.detail_open = true;
        self.detail = None;
        self.detail_note = None;
        self.detail_rx = Some(worker(ctx, move || git::commit_detail(&repo, &blame)));
    }
    /// Returns an explicit editor or diff action for a repository-absolute file.
    pub fn panel(
        &mut self,
        ctx: &egui::Context,
        project: &Path,
        dirty: &[PathBuf],
    ) -> Option<ChangeAction> {
        if !self.visible {
            return None;
        }
        if self.last_refresh.is_none() {
            self.refresh(project, ctx);
        }
        let mut selected = None;
        let mut refresh = false;
        egui::SidePanel::right("git-changes-panel")
            .default_width(320.0)
            .width_range(240.0..=650.0)
            .frame(egui::Frame::new().fill(chrome::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    chrome::heading_icon(ui, chrome::Icon::Branch, chrome::GREEN);
                    ui.strong("Changes");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if chrome::icon_button(ui, chrome::Icon::HideRight, "Hide Changes")
                            .clicked()
                        {
                            self.visible = false;
                        }
                        if chrome::icon_button(
                            ui,
                            chrome::Icon::Refresh,
                            "Refresh changes and pull request",
                        )
                        .clicked()
                        {
                            refresh = true;
                        }
                    });
                });
                if let Some(repo) = &self.repo {
                    ui.label(RichText::new(&repo.branch).color(chrome::BLUE));
                    let previous_base = self
                        .selected_base
                        .as_deref()
                        .unwrap_or(&repo.base)
                        .to_owned();
                    let mut base = previous_base.clone();
                    ui.add_enabled_ui(self.repo_rx.is_none(), |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Base");
                            egui::ComboBox::from_id_salt("git-base")
                                .selected_text(&base)
                                .width((ui.available_width() - 8.0).max(100.0))
                                .show_ui(ui, |ui| {
                                    for branch in &repo.branches {
                                        ui.selectable_value(&mut base, branch.clone(), branch);
                                    }
                                });
                        });
                    });
                    if base != previous_base {
                        self.selected_base = Some(base);
                        refresh = true;
                        self.file = None;
                        self.failed_file = None;
                    }
                    if let Some(base) = &repo.merge_base {
                        ui.weak(format!("Merge base {}", &base[..base.len().min(10)]));
                    }
                    if let Some(pr) = &self.pr {
                        ui.hyperlink_to(format!("PR #{} · {}", pr.number, pr.title), &pr.url);
                    } else if self.pr_rx.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak("Finding branch PR…");
                        });
                    } else if let Some(note) = &self.pr_note {
                        ui.weak("PR information unavailable").on_hover_text(note);
                    } else if self.pr_key.is_some() {
                        ui.weak("No open PR found for this branch");
                    }
                    ui.checkbox(&mut self.show_blame, "Git blame annotations");
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(committed(), "● Committed");
                        ui.colored_label(working(), "● Uncommitted");
                        ui.colored_label(deleted(), "● Deleted");
                    });
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .hint_text("Filter changed files…")
                            .desired_width(f32::INFINITY),
                    );
                    let mut files = repo.files.clone();
                    for path in dirty {
                        if let Ok(path) = path.strip_prefix(&repo.root) {
                            if let Some(file) = files.iter_mut().find(|file| file.path == path) {
                                file.uncommitted = true;
                            } else {
                                files.push(git::ChangedFile {
                                    path: path.to_path_buf(),
                                    uncommitted: true,
                                    ..Default::default()
                                });
                            }
                        }
                    }
                    ui.weak(format!("{} changed files · click to open", files.len()));
                    ui.separator();
                    let tree = ChangeTree::build(&files, &self.filter);
                    egui::ScrollArea::both()
                        .id_salt("changed-file-tree")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            selected = tree
                                .show(ui, &files, Path::new(""))
                                .map(|action| action.absolute(&repo.root));
                        });
                    if files.is_empty() {
                        ui.weak("No changes relative to the selected base.");
                    }
                } else if self.repo_rx.is_some() {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading repository…");
                    });
                }
                if let Some(note) = &self.note {
                    ui.label(RichText::new(note).color(working()));
                }
            });
        if refresh {
            self.pr_key = None;
            self.file = None;
            self.failed_file = None;
            self.refresh(project, ctx);
        }
        selected
    }
    pub fn windows(&mut self, ctx: &egui::Context) -> Option<PathBuf> {
        let mut opened = None;
        if self.review_open {
            #[cfg(feature = "ui-capture")]
            if let Ok(mode) = std::env::var("GLIMCODE_DIFF_MODE") {
                self.review_mode = if mode == "unified" {
                    DiffMode::Unified
                } else {
                    DiffMode::Split
                };
            }
            let mut close = false;
            let size = ctx.content_rect().size();
            egui::Window::new("Branch changes · PR diff")
                .id(egui::Id::new("branch-review"))
                .title_bar(false)
                .collapsible(false)
                .resizable(true)
                .min_size([640.0, 300.0])
                .default_size([(size.x - 80.0).min(1280.0), (size.y - 110.0).min(800.0)])
                .default_pos(egui::pos2(40.0, 65.0))
                .frame(
                    egui::Frame::new()
                        .fill(chrome::EDITOR)
                        .corner_radius(9)
                        .stroke(egui::Stroke::new(1.0_f32, chrome::BORDER))
                        .inner_margin(12),
                )
                .show(ctx, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
                    ui.spacing_mut().button_padding = egui::vec2(9.0, 4.0);
                    ui.spacing_mut().interact_size.y = 26.0;
                    ui.horizontal(|ui| {
                        chrome::heading_icon(ui, chrome::Icon::Branch, chrome::BLUE);
                        ui.label(RichText::new("Branch changes").size(15.0).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            close =
                                chrome::icon_button(ui, chrome::Icon::Close, "Close diff (Esc)")
                                    .clicked();
                            if let Some(review) = &self.review {
                                ui.weak(
                                    RichText::new(format!("{} → working tree", review.base))
                                        .size(11.0),
                                );
                            }
                        });
                    });
                    if let Some(review) = &mut self.review {
                        egui::Frame::new()
                            .fill(chrome::PANEL)
                            .corner_radius(5)
                            .inner_margin(8)
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(chrome::file_icon(&review.path));
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(review.relative_path.to_string_lossy())
                                                .strong(),
                                        )
                                        .truncate(),
                                    );
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .add_enabled(
                                                    review.path.exists(),
                                                    egui::Button::new("Open file").frame(false),
                                                )
                                                .clicked()
                                            {
                                                opened = Some(review.path.clone());
                                            }
                                        },
                                    );
                                });
                            });
                        review.show(ui, &mut self.review_mode);
                    } else if let Some(note) = &self.review_note {
                        ui.label(note);
                    } else {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Preparing diff…");
                        });
                    }
                    close |=
                        ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                });
            if close {
                self.review_open = false;
            }
        }
        if self.detail_open {
            let mut close = false;
            egui::Window::new("Commit and pull request")
                .id(egui::Id::new("blame-detail"))
                .title_bar(false)
                .collapsible(false)
                .frame(chrome::popup_frame())
                .default_size([780.0, 620.0])
                .show(ctx, |ui| {
                    chrome::dialog_style(ui, ui.available_width());
                    close = chrome::dialog_header(
                        ui,
                        "⑂",
                        "Commit & pull request",
                        "The story behind this change",
                        chrome::BLUE,
                    );
                    if let Some(detail) = &self.detail {
                        ui.label(RichText::new(&detail.subject).size(18.0).strong());
                        ui.label(format!("{}  {}", detail.author, detail.date));
                        if !detail.email.is_empty() {
                            ui.weak(&detail.email);
                        }
                        if let Some(url) = &detail.url {
                            ui.hyperlink_to(&detail.sha, url);
                        } else if !detail.sha.is_empty() {
                            ui.monospace(&detail.sha);
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("commit-pr-description")
                            .show(ui, |ui| {
                                if !detail.body.is_empty() {
                                    ui.label(&detail.body);
                                }
                                for pr in &detail.prs {
                                    ui.separator();
                                    ui.hyperlink_to(
                                        format!("PR #{} · {}", pr.number, pr.title),
                                        &pr.url,
                                    );
                                    ui.weak(format!(
                                        "{} · {} · {} → {} · updated {}",
                                        pr.author, pr.state, pr.head, pr.base, pr.updated
                                    ));
                                    if pr.body.trim().is_empty() {
                                        ui.weak("No PR description.");
                                    } else {
                                        egui_commonmark::CommonMarkViewer::new().show(
                                            ui,
                                            &mut self.description_cache,
                                            &pr.body,
                                        );
                                    }
                                }
                                if detail.prs.is_empty()
                                    && detail.pr_note.is_none()
                                    && !detail.sha.is_empty()
                                {
                                    ui.weak("No associated pull request was found.");
                                }
                                if let Some(note) = &detail.pr_note {
                                    ui.separator();
                                    ui.label(note);
                                }
                            });
                    } else if let Some(note) = &self.detail_note {
                        ui.label(note);
                    } else {
                        ui.spinner();
                        ui.label("Loading commit and PR information…");
                    }
                    close |=
                        ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                });
            if close {
                self.detail_open = false;
            }
        }
        opened
    }
}
pub fn committed() -> Color32 {
    Color32::from_rgb(100, 165, 245)
}
pub fn working() -> Color32 {
    Color32::from_rgb(224, 180, 95)
}
pub fn deleted() -> Color32 {
    Color32::from_rgb(230, 115, 125)
}
pub fn marker(mark: git::Mark) -> Color32 {
    match mark {
        git::Mark::Added => Color32::from_rgb(82, 168, 104),
        git::Mark::Modified => committed(),
        git::Mark::Deleted => deleted(),
    }
}

#[derive(Default)]
struct ChangeTree {
    children: BTreeMap<String, ChangeTree>,
    file: Option<usize>,
}
impl ChangeTree {
    fn build(files: &[git::ChangedFile], filter: &str) -> Self {
        let mut tree = Self::default();
        let filter = filter.to_lowercase();
        for (index, file) in files.iter().enumerate() {
            if !file.path.to_string_lossy().to_lowercase().contains(&filter) {
                continue;
            }
            let mut node = &mut tree;
            for part in file.path.components() {
                node = node
                    .children
                    .entry(part.as_os_str().to_string_lossy().into_owned())
                    .or_default();
            }
            node.file = Some(index);
        }
        tree
    }
    fn show(
        &self,
        ui: &mut egui::Ui,
        files: &[git::ChangedFile],
        directory: &Path,
    ) -> Option<ChangeAction> {
        let mut selected = None;
        for is_file in [false, true] {
            for (name, node) in &self.children {
                if node.file.is_some() != is_file {
                    continue;
                }
                let path = directory.join(name);
                if let Some(index) = node.file {
                    let file = &files[index];
                    let color = if file.deleted {
                        deleted()
                    } else if file.uncommitted {
                        working()
                    } else {
                        committed()
                    };
                    let badge = if file.conflict {
                        "!"
                    } else if file.deleted {
                        "D"
                    } else if file.committed && file.uncommitted {
                        "C+W"
                    } else if file.uncommitted {
                        "W"
                    } else {
                        "C"
                    };
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 23.0),
                        egui::Sense::click(),
                    );
                    if response.hovered() {
                        ui.painter().rect_filled(rect, 3.0, chrome::HEADER);
                    }
                    ui.put(
                        egui::Rect::from_min_size(rect.min, egui::vec2(24.0, 23.0)),
                        egui::Label::new(chrome::file_icon(&file.path)),
                    );
                    ui.painter().text(
                        rect.left_center() + egui::vec2(29.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        format!("{badge}  {name}"),
                        egui::FontId::proportional(13.0),
                        color,
                    );
                    if response
                        .clone()
                        .on_hover_text(format!(
                            "{}{}",
                            file.path.display(),
                            file.old_path
                                .as_ref()
                                .map(|old| format!("\nRenamed from {}", old.display()))
                                .unwrap_or_default()
                        ))
                        .clicked()
                        && !file.deleted
                    {
                        selected = Some(ChangeAction::Open(file.path.clone()));
                    }
                    response.context_menu(|ui| {
                        if ui
                            .add_enabled(!file.deleted, egui::Button::new("Open in editor"))
                            .clicked()
                        {
                            selected = Some(ChangeAction::Open(file.path.clone()));
                            ui.close();
                        }
                        if ui.button("View diff").clicked() {
                            selected = Some(ChangeAction::Diff(file.path.clone()));
                            ui.close();
                        }
                    });
                } else {
                    let header = egui::CollapsingHeader::new(format!("        {name}"))
                        .id_salt(("git-folder", &path))
                        .default_open(true)
                        .show(ui, |ui| {
                            if let Some(path) = node.show(ui, files, &path) {
                                selected = Some(path);
                            }
                        });
                    chrome::folder(
                        ui.painter(),
                        egui::Rect::from_min_size(
                            header.header_response.rect.min + egui::vec2(18.0, -1.0),
                            egui::vec2(23.0, 23.0),
                        ),
                    );
                }
            }
        }
        selected
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum DiffMode {
    Unified,
    #[default]
    Split,
}

struct Review {
    path: PathBuf,
    relative_path: PathBuf,
    base: String,
    before: SourcePreview,
    after: SourcePreview,
    rows: Vec<diff::Row>,
    visible: Vec<ReviewRow>,
    full: bool,
    split: Vec<SplitRow>,
    scroll_y: f32,
    hunk: usize,
    changes: Vec<usize>,
    jump_requested: bool,
    before_newline: bool,
    after_newline: bool,
}
#[derive(Clone, Debug)]
enum ReviewRow {
    Hunk(String),
    Line(usize),
}
#[derive(Debug, PartialEq)]
enum SplitRow {
    Hunk(String),
    Line {
        before: Option<usize>,
        after: Option<usize>,
        changed: bool,
    },
}

/// Pair whole replacement blocks, rather than putting every deletion above additions.
fn split_rows(rows: &[diff::Row], visible: &[ReviewRow]) -> Vec<SplitRow> {
    let mut result = Vec::new();
    let mut index = 0;
    while index < visible.len() {
        match &visible[index] {
            ReviewRow::Hunk(header) => {
                result.push(SplitRow::Hunk(header.clone()));
                index += 1;
            }
            ReviewRow::Line(line) if !rows[*line].changed => {
                result.push(SplitRow::Line {
                    before: rows[*line].disk,
                    after: rows[*line].editor,
                    changed: false,
                });
                index += 1;
            }
            ReviewRow::Line(_) => {
                let (mut before, mut after) = (Vec::new(), Vec::new());
                while let Some(ReviewRow::Line(line)) = visible.get(index) {
                    let row = &rows[*line];
                    if !row.changed {
                        break;
                    }
                    before.extend(row.disk);
                    after.extend(row.editor);
                    index += 1;
                }
                for offset in 0..before.len().max(after.len()) {
                    result.push(SplitRow::Line {
                        before: before.get(offset).copied(),
                        after: after.get(offset).copied(),
                        changed: true,
                    });
                }
            }
        }
    }
    result
}

fn change_targets(rows: &[diff::Row]) -> Vec<usize> {
    let mut targets = Vec::new();
    let mut pending = false;
    for row in review_rows(rows, false) {
        match row {
            ReviewRow::Hunk(_) => pending = true,
            ReviewRow::Line(index) if pending && rows[index].changed => {
                targets.push(index);
                pending = false;
            }
            _ => {}
        }
    }
    targets
}

// Navigate from the viewport, so scrolling away never strands the selected hunk.
fn navigation_target(targets: &[usize], scroll_y: f32, height: f32, next: bool) -> usize {
    let offset = |row: &usize| (*row as f32 * height - 2.0 * height).max(0.0);
    if next {
        targets
            .partition_point(|row| offset(row) <= scroll_y + 1.0)
            .min(targets.len().saturating_sub(1))
    } else {
        targets
            .partition_point(|row| offset(row) < scroll_y - 1.0)
            .saturating_sub(1)
    }
}

impl Review {
    /// Map stable changes to each layout, including expanded full context.
    fn target_rows(&self, mode: DiffMode) -> Vec<usize> {
        let mut positions = vec![None; self.changes.len()];
        match mode {
            DiffMode::Unified => {
                let wanted: BTreeMap<_, _> = self
                    .changes
                    .iter()
                    .enumerate()
                    .map(|(slot, &row)| (row, slot))
                    .collect();
                for (position, row) in self.visible.iter().enumerate() {
                    if let ReviewRow::Line(line) = row
                        && let Some(&slot) = wanted.get(line)
                    {
                        positions[slot] = Some(position);
                    }
                }
            }
            DiffMode::Split => {
                let mut before = BTreeMap::new();
                let mut after = BTreeMap::new();
                for (slot, &row) in self.changes.iter().enumerate() {
                    if let Some(line) = self.rows[row].disk {
                        before.insert(line, slot);
                    } else if let Some(line) = self.rows[row].editor {
                        after.insert(line, slot);
                    }
                }
                for (position, row) in self.split.iter().enumerate() {
                    if let SplitRow::Line {
                        before: old,
                        after: new,
                        ..
                    } = row
                    {
                        if let Some(&slot) = old.and_then(|line| before.get(&line)) {
                            positions[slot] = Some(position);
                        }
                        if let Some(&slot) = new.and_then(|line| after.get(&line)) {
                            positions[slot] = Some(position);
                        }
                    }
                }
            }
        }
        positions
            .into_iter()
            .map(|row| row.expect("each change must be represented in both layouts"))
            .collect()
    }

    fn new(
        repo: &Repository,
        path: PathBuf,
        buffer: Option<String>,
        dark: bool,
    ) -> anyhow::Result<Self> {
        let data = git::file_data(repo, &path, buffer, false)?;
        let rows = diff::compare(&data.base, &data.text);
        let before_newline = data.base.ends_with('\n');
        let after_newline = data.text.ends_with('\n');
        let before = SourcePreview::build(path.clone(), data.base, dark, &|| false)
            .ok_or_else(|| anyhow::anyhow!("Could not highlight base file"))?;
        let after = SourcePreview::build(path.clone(), data.text, dark, &|| false)
            .ok_or_else(|| anyhow::anyhow!("Could not highlight current file"))?;
        let visible = review_rows(&rows, false);
        let split = split_rows(&rows, &visible);
        Ok(Self {
            relative_path: path.strip_prefix(&repo.root).unwrap_or(&path).to_path_buf(),
            path,
            base: repo.base.clone(),
            before,
            after,
            changes: change_targets(&rows),
            jump_requested: false,
            rows,
            visible,
            full: false,
            split,
            scroll_y: 0.0,
            hunk: 0,
            before_newline,
            after_newline,
        })
    }
    fn show(&mut self, ui: &mut egui::Ui, mode: &mut DiffMode) {
        let added = self
            .rows
            .iter()
            .filter(|r| r.changed && r.editor.is_some())
            .count();
        let removed = self
            .rows
            .iter()
            .filter(|r| r.changed && r.disk.is_some())
            .count();
        let previous_mode = *mode;
        let height = crate::preferences::line_height(ui.ctx()).max(20.0);
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                chrome::GREEN,
                RichText::new(format!("+{added}")).strong().size(12.0),
            );
            ui.colored_label(
                deleted(),
                RichText::new(format!("−{removed}")).strong().size(12.0),
            );
            ui.separator();
            for (label, value) in [("Unified", DiffMode::Unified), ("Split", DiffMode::Split)] {
                if ui
                    .add(
                        egui::Button::new(RichText::new(label).size(12.0))
                            .selected(*mode == value)
                            .fill(if *mode == value {
                                chrome::SELECTED
                            } else {
                                chrome::PANEL
                            })
                            .stroke(egui::Stroke::NONE)
                            .corner_radius(4),
                    )
                    .clicked()
                {
                    *mode = value;
                }
            }
            ui.separator();
            if ui.checkbox(&mut self.full, "Full context").changed() {
                self.visible = review_rows(&self.rows, self.full);
                self.split = split_rows(&self.rows, &self.visible);
                self.jump_requested = true;
            }
            let count = self.changes.len();
            if count > 0 {
                self.hunk = self.hunk.min(count - 1);
                if ui.add(egui::Button::new("↑ Previous").small()).clicked() {
                    self.hunk =
                        navigation_target(&self.target_rows(*mode), self.scroll_y, height, false);
                    self.jump_requested = true;
                }
                if ui.add(egui::Button::new("↓ Next").small()).clicked() {
                    self.hunk =
                        navigation_target(&self.target_rows(*mode), self.scroll_y, height, true);
                    self.jump_requested = true;
                }
                ui.weak(RichText::new(format!("{} / {} hunks", self.hunk + 1, count)).size(10.0));
            }
        });
        if *mode != previous_mode {
            self.jump_requested = true;
        }
        let targets = self.target_rows(*mode);
        let jumped = std::mem::take(&mut self.jump_requested);
        if jumped && let Some(&row) = targets.get(self.hunk) {
            // Leave a little context above the selected change.
            self.scroll_y = (row as f32 * height - 2.0 * height).max(0.0);
        }
        let requested_y = self.scroll_y;
        if added + removed == 0 {
            ui.add_space(20.0);
            ui.colored_label(chrome::GREEN, "No changes relative to this base.");
            return;
        }
        ui.separator();
        // Diff rows are code, not dialog controls. Zero spacing keeps gutters and
        // change backgrounds continuous and makes virtualized row heights exact.
        ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
        let scale = crate::preferences::editor_font_size(ui.ctx()) / 13.0;
        match mode {
            DiffMode::Unified => {
                let output = egui::ScrollArea::both()
                    .id_salt(("unified-diff", &self.path))
                    .vertical_scroll_offset(self.scroll_y)
                    .auto_shrink([false, false])
                    .show_rows(
                        ui,
                        height,
                        self.visible.len() + (ui.available_height() / height).ceil() as usize,
                        |ui, range| {
                            let width = ui.available_width().max(
                                self.before.width.max(self.after.width) * scale
                                    + 100.0
                                    + if self.before_newline && self.after_newline {
                                        0.0
                                    } else {
                                        200.0
                                    },
                            );
                            ui.set_min_width(width);
                            for index in range.take_while(|&index| index < self.visible.len()) {
                                match &self.visible[index] {
                                    ReviewRow::Hunk(header) => {
                                        paint_hunk(ui, header, width, height)
                                    }
                                    ReviewRow::Line(line) => {
                                        let row = &self.rows[*line];
                                        let (preview, number, newline) =
                                            if let Some(line) = row.editor {
                                                (&self.after, line, self.after_newline)
                                            } else {
                                                (
                                                    &self.before,
                                                    row.disk.unwrap_or(0),
                                                    self.before_newline,
                                                )
                                            };
                                        let mark = if !row.changed {
                                            ' '
                                        } else if row.editor.is_some() {
                                            '+'
                                        } else {
                                            '-'
                                        };
                                        paint_diff_line(
                                            ui,
                                            preview,
                                            Some(number),
                                            &[row.disk, row.editor],
                                            mark,
                                            newline,
                                            width,
                                            height,
                                            None,
                                        );
                                    }
                                }
                            }
                        },
                    );
                self.scroll_y = output.state.offset.y;
            }
            DiffMode::Split => {
                let old_y = self.scroll_y;
                let mut next_y = old_y;
                ui.columns(2, |columns| {
                    for (side, ui) in columns.iter_mut().enumerate() {
                        let (preview, newline, heading) = if side == 0 {
                            (&self.before, self.before_newline, self.base.as_str())
                        } else {
                            (
                                &self.after,
                                self.after_newline,
                                "Working tree + unsaved edits",
                            )
                        };
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 26.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(rect, 0, chrome::TABS);
                        ui.painter().text(
                            rect.left_center() + egui::vec2(10.0, 0.0),
                            egui::Align2::LEFT_CENTER,
                            heading,
                            egui::FontId::proportional(11.0),
                            chrome::MUTED,
                        );
                        let output = egui::ScrollArea::both()
                            .id_salt(("split-diff", &self.path, side))
                            .scroll_bar_visibility(
                                egui::scroll_area::ScrollBarVisibility::AlwaysVisible,
                            )
                            .vertical_scroll_offset(old_y)
                            .auto_shrink([false, false])
                            .show_rows(
                                ui,
                                height,
                                self.split.len() + (ui.available_height() / height).ceil() as usize,
                                |ui, range| {
                                    let width = ui.available_width().max(
                                        preview.width * scale
                                            + 36.0
                                            + if newline { 0.0 } else { 200.0 },
                                    );
                                    ui.set_min_width(width);
                                    for index in range.take_while(|&index| index < self.split.len())
                                    {
                                        match &self.split[index] {
                                            SplitRow::Hunk(header) => {
                                                paint_hunk(ui, header, width, height)
                                            }
                                            SplitRow::Line {
                                                before,
                                                after,
                                                changed,
                                            } => {
                                                let line = if side == 0 { *before } else { *after };
                                                let mark = if !changed || line.is_none() {
                                                    ' '
                                                } else if side == 0 {
                                                    '-'
                                                } else {
                                                    '+'
                                                };
                                                let inline = before
                                                    .zip(*after)
                                                    .filter(|_| *changed)
                                                    .map(|(old, new)| {
                                                        let ranges = changed_ranges(
                                                            self.before.line_text(old),
                                                            self.after.line_text(new),
                                                        );
                                                        if side == 0 { ranges.0 } else { ranges.1 }
                                                    });
                                                paint_diff_line(
                                                    ui,
                                                    preview,
                                                    line,
                                                    &[line],
                                                    mark,
                                                    newline,
                                                    width,
                                                    height,
                                                    inline,
                                                );
                                            }
                                        }
                                    }
                                },
                            );
                        if (output.state.offset.y - old_y).abs() > 0.5 {
                            next_y = output.state.offset.y;
                        }
                    }
                });
                if (next_y - self.scroll_y).abs() > 0.5 {
                    ui.ctx().request_repaint();
                }
                self.scroll_y = next_y;
            }
        }
        if !jumped && (self.scroll_y - requested_y).abs() > 0.5 && !targets.is_empty() {
            let current = targets
                .partition_point(|&row| row as f32 * height <= self.scroll_y + 2.0 * height + 1.0)
                .saturating_sub(1);
            if current != self.hunk {
                self.hunk = current;
                ui.ctx().request_repaint();
            }
        }
    }
}

fn changed_ranges(before: &str, after: &str) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
    let prefix = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    let suffix = before[prefix..]
        .chars()
        .rev()
        .zip(after[prefix..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    (prefix..before.len() - suffix, prefix..after.len() - suffix)
}

fn paint_hunk(ui: &mut egui::Ui, header: &str, width: f32, height: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 0, Color32::from_rgb(28, 40, 56));
    ui.painter().text(
        rect.left_center() + egui::vec2(12.0, 0.0),
        egui::Align2::LEFT_CENTER,
        header,
        egui::FontId::monospace(11.0),
        Color32::from_rgb(130, 157, 190),
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_diff_line(
    ui: &mut egui::Ui,
    preview: &SourcePreview,
    line: Option<usize>,
    gutters: &[Option<usize>],
    mark: char,
    newline: bool,
    width: f32,
    height: f32,
    inline: Option<std::ops::Range<usize>>,
) {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
    let (background, gutter_bg, accent) = match mark {
        '+' => (
            Color32::from_rgb(25, 47, 36),
            Color32::from_rgb(30, 63, 43),
            chrome::GREEN,
        ),
        '-' => (
            Color32::from_rgb(56, 31, 37),
            Color32::from_rgb(75, 35, 43),
            deleted(),
        ),
        _ if line.is_none() => (Color32::from_rgb(29, 32, 38), chrome::TABS, chrome::MUTED),
        _ => (chrome::EDITOR, Color32::from_rgb(26, 29, 35), chrome::MUTED),
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 0, background);
    let gutter_width = 44.0 * gutters.len() as f32;
    let gutter = egui::Rect::from_min_max(
        rect.min,
        egui::pos2(rect.left() + gutter_width, rect.bottom()),
    );
    painter.rect_filled(gutter, 0, gutter_bg);
    for (index, number) in gutters.iter().enumerate() {
        if let Some(number) = number {
            painter.text(
                egui::pos2(
                    rect.left() + (index + 1) as f32 * 44.0 - 9.0,
                    rect.center().y,
                ),
                egui::Align2::RIGHT_CENTER,
                (number + 1).to_string(),
                egui::FontId::monospace(11.0),
                accent,
            );
        }
    }
    painter.vline(
        gutter.right(),
        rect.y_range(),
        egui::Stroke::new(1.0_f32, chrome::BORDER.gamma_multiply(0.5)),
    );
    if line.is_none() {
        return;
    }
    let line = line.unwrap();
    painter.text(
        egui::pos2(gutter.right() + 10.0, rect.center().y),
        egui::Align2::CENTER_CENTER,
        mark,
        egui::FontId::monospace(12.0),
        accent,
    );
    let mut job = preview.line_for_ui(ui, line, "");
    if let Some(range) = inline.filter(|range| !range.is_empty()) {
        let background = if mark == '+' {
            Color32::from_rgb(37, 88, 54)
        } else {
            Color32::from_rgb(110, 43, 53)
        };
        let mut sections = Vec::new();
        for section in job.sections {
            let start = section.byte_range.start;
            let end = section.byte_range.end;
            let mut cuts = vec![start, end];
            cuts.extend(
                [range.start, range.end]
                    .into_iter()
                    .filter(|&cut| start < cut && cut < end),
            );
            cuts.sort_unstable();
            cuts.dedup();
            for pair in cuts.windows(2) {
                let mut format = section.format.clone();
                if pair[0] >= range.start && pair[1] <= range.end {
                    format.background = background;
                }
                sections.push(egui::text::LayoutSection {
                    leading_space: section.leading_space,
                    byte_range: pair[0]..pair[1],
                    format,
                });
            }
        }
        job.sections = sections;
    }
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    let end = gutter.right() + 22.0 + galley.size().x;
    painter.galley(
        egui::pos2(
            gutter.right() + 22.0,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        chrome::TEXT,
    );
    if !newline && line + 1 == preview.len() {
        painter.text(
            egui::pos2(end + 16.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            "No newline at end of file",
            egui::FontId::proportional(10.0),
            chrome::MUTED,
        );
    }
    response.context_menu(|ui| {
        if ui.button("Copy line").clicked() {
            ui.ctx().copy_text(preview.line_text(line).to_owned());
            ui.close();
        }
    });
}

fn review_rows(rows: &[diff::Row], full: bool) -> Vec<ReviewRow> {
    let mut ranges: Vec<std::ops::Range<usize>> = vec![];
    if full {
        ranges.push(0..rows.len());
    } else {
        for (index, row) in rows.iter().enumerate().filter(|(_, row)| row.changed) {
            let range = index.saturating_sub(3)..(index + 4).min(rows.len());
            if let Some(previous) = ranges.last_mut()
                && range.start <= previous.end
            {
                previous.end = range.end;
            } else {
                ranges.push(range);
            }
            let _ = row;
        }
    }
    let mut result = vec![];
    for range in ranges {
        let old_before = rows[..range.start]
            .iter()
            .filter(|r| r.disk.is_some())
            .count();
        let new_before = rows[..range.start]
            .iter()
            .filter(|r| r.editor.is_some())
            .count();
        let old_count = rows[range.clone()]
            .iter()
            .filter(|r| r.disk.is_some())
            .count();
        let new_count = rows[range.clone()]
            .iter()
            .filter(|r| r.editor.is_some())
            .count();
        result.push(ReviewRow::Hunk(format!(
            "@@ -{},{} +{},{} @@",
            old_before + usize::from(old_count > 0),
            old_count,
            new_before + usize::from(new_count > 0),
            new_count
        )));
        result.extend(range.map(ReviewRow::Line));
    }
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn changed_file_click_opens_editor_and_context_menu_requests_diff() {
        use super::*;
        let ctx = egui::Context::default();
        let files = vec![git::ChangedFile {
            path: "main.rs".into(),
            uncommitted: true,
            ..Default::default()
        }];
        let tree = ChangeTree::build(&files, "");
        let mut time = 0.0;
        let mut frame = |events| {
            time += 0.1;
            let mut action = None;
            let output = ctx.run(
                egui::RawInput {
                    events,
                    time: Some(time),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(600.0, 400.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        action = tree.show(ui, &files, Path::new(""));
                    });
                },
            );
            (output, action)
        };
        frame(vec![]);
        let (output, _) = frame(vec![]);
        let point = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(t) if t.galley.text().contains("main.rs") => {
                    Some(t.pos + egui::vec2(10.0, 5.0))
                }
                _ => None,
            })
            .unwrap();
        let event = |point, button, pressed| {
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        frame(event(point, egui::PointerButton::Primary, true));
        assert_eq!(
            frame(event(point, egui::PointerButton::Primary, false)).1,
            Some(ChangeAction::Open("main.rs".into()))
        );
        frame(event(point, egui::PointerButton::Secondary, true));
        frame(event(point, egui::PointerButton::Secondary, false));
        let (output, _) = frame(vec![]);
        let point = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(t) if t.galley.text() == "View diff" => {
                    Some(t.pos + t.galley.size() * 0.5)
                }
                _ => None,
            })
            .expect("Explicit diff action");
        frame(event(point, egui::PointerButton::Primary, true));
        assert_eq!(
            frame(event(point, egui::PointerButton::Primary, false)).1,
            Some(ChangeAction::Diff("main.rs".into()))
        );
    }

    use super::*;
    #[test]
    fn split_replacements_preserve_both_files_and_pad_uneven_blocks() {
        let before = "same\nold one\nold two\nend\n";
        let after = "same\nnew one\nnew two\nextra\nend\n";
        let rows = diff::compare(before, after);
        let aligned = split_rows(&rows, &review_rows(&rows, true));
        let old: Vec<_> = aligned
            .iter()
            .filter_map(|row| match row {
                SplitRow::Line { before, .. } => *before,
                _ => None,
            })
            .collect();
        let new: Vec<_> = aligned
            .iter()
            .filter_map(|row| match row {
                SplitRow::Line { after, .. } => *after,
                _ => None,
            })
            .collect();
        assert_eq!(old, [0, 1, 2, 3]);
        assert_eq!(new, [0, 1, 2, 3, 4]);
        assert!(aligned.contains(&SplitRow::Line {
            before: Some(1),
            after: Some(1),
            changed: true
        }));
        assert!(aligned.contains(&SplitRow::Line {
            before: None,
            after: Some(3),
            changed: true
        }));
        for (old, new) in [("", "new\n"), ("old\n", "")] {
            let rows = diff::compare(old, new);
            let split = split_rows(&rows, &review_rows(&rows, false));
            assert!(
                split
                    .iter()
                    .any(|row| matches!(row, SplitRow::Line { changed: true, .. }))
            );
        }
    }

    #[test]
    fn inline_ranges_are_unicode_safe_and_do_not_overlap() {
        for (old, new, expected) in [
            ("let café = 1;", "let café = 20;", ("1", "20")),
            ("猫abc", "猫xyz", ("abc", "xyz")),
            ("same", "same", ("", "")),
            ("abc", "ab", ("c", "")),
            ("", "new", ("", "new")),
        ] {
            let (a, b) = changed_ranges(old, new);
            assert_eq!((&old[a], &new[b]), expected);
        }
    }

    #[test]
    fn unified_and_split_are_compact_and_keep_code_gutters_aligned() -> anyhow::Result<()> {
        let dir = crate::git::tests::setup()?;
        let repo = git::repository(dir.path(), None)?;
        let mut review = Review::new(
            &repo,
            repo.root.join("src/code.rs"),
            Some("fn main() {\n    first();\n    second();\n}\n".into()),
            true,
        )?;
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        for mut mode in [DiffMode::Unified, DiffMode::Split] {
            let mut output = None;
            for _ in 0..3 {
                output = Some(ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1000.0, 500.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            review.show(ui, &mut mode);
                            assert!(
                                ui.min_rect().bottom() <= 500.0,
                                "diff must fit the available height"
                            );
                        });
                    },
                ));
            }
            let output = output.unwrap();
            let labels: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape {
                        Some(text.galley.text())
                    } else {
                        None
                    }
                })
                .collect();
            assert!(labels.contains(&"Unified") && labels.contains(&"Split"));
            assert!(labels.iter().any(|label| label.contains("first()")));
            assert!(labels.iter().any(|label| label.contains("second()")));
            let positions: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.text().contains("first()")
                    {
                        Some(text.pos.y)
                    } else {
                        None
                    }
                })
                .collect();
            assert!(!positions.is_empty());
            let second = output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.text().contains("second()")
                    {
                        Some(text.pos.y)
                    } else {
                        None
                    }
                })
                .unwrap();
            let expected = crate::preferences::line_height(&ctx).max(20.0);
            assert!(
                (second - positions[0] - expected).abs() < 1.0,
                "code lines must be contiguous, without dialog spacing"
            );
        }
        Ok(())
    }

    #[test]
    fn split_scroll_tracks_wheel_input_on_either_pane() -> anyhow::Result<()> {
        let path = PathBuf::from("sample.rs");
        let source = (0..200)
            .map(|i| format!("const ITEM_{i}: usize = {i};\n"))
            .collect::<String>();
        let after = source.replace("ITEM_199: usize = 199", "ITEM_199: usize = 200");
        let rows = diff::compare(&source, &after);
        let visible = review_rows(&rows, true);
        let split = split_rows(&rows, &visible);
        let mut review = Review {
            path: path.clone(),
            relative_path: path.clone(),
            base: "main".into(),
            before: SourcePreview::build(path.clone(), source.clone(), true, &|| false).unwrap(),
            after: SourcePreview::build(path, after, true, &|| false).unwrap(),
            changes: change_targets(&rows),
            jump_requested: false,
            rows,
            visible,
            split,
            full: true,
            scroll_y: 0.0,
            hunk: 0,
            before_newline: true,
            after_newline: true,
        };
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let mut mode = DiffMode::Split;
        let mut time = 0.0;
        let mut frame = |events, review: &mut Review| {
            time += 0.1;
            ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 500.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| review.show(ui, &mut mode));
                },
            )
        };
        for _ in 0..3 {
            frame(vec![], &mut review);
        }
        for x in [750.0, 250.0] {
            let before = review.scroll_y;
            let position = egui::pos2(x, 200.0);
            frame(vec![egui::Event::PointerMoved(position)], &mut review);
            frame(
                vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -160.0),
                    modifiers: egui::Modifiers::NONE,
                }],
                &mut review,
            );
            for _ in 0..20 {
                frame(vec![], &mut review);
            }
            let output = frame(vec![], &mut review);
            assert!(
                review.scroll_y > before,
                "scrolling either pane must move the shared vertical offset"
            );
            let mut first = [None, None];
            for shape in &output.shapes {
                if let egui::Shape::Text(text) = &shape.shape
                    && text.galley.text().starts_with("const ITEM_")
                {
                    let side = usize::from(text.pos.x > 500.0);
                    first[side].get_or_insert((text.galley.text(), text.pos.y));
                }
            }
            let (left, right) = (first[0].unwrap(), first[1].unwrap());
            assert_eq!(
                left.0, right.0,
                "both panes must show the same first virtual row"
            );
            assert!(
                (left.1 - right.1).abs() < 1.0,
                "paired rows must stay vertically aligned"
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "read-only check against GLIMCODE_REVIEW_PROJECT and GLIMCODE_REVIEW_FILE"]
    fn external_repository_change_navigation() -> anyhow::Result<()> {
        let root = PathBuf::from(std::env::var("GLIMCODE_REVIEW_PROJECT")?);
        let repo = git::repository(&root, None)?;
        let path = root.join(std::env::var("GLIMCODE_REVIEW_FILE")?);
        let review = Review::new(&repo, path, None, true)?;
        assert_navigation_from_away(review);
        Ok(())
    }

    #[test]
    fn single_git_hunk_can_be_refocused_from_away_in_both_layouts() {
        let path = PathBuf::from("code.rs");
        let before = (0..80)
            .map(|i| format!("const ITEM_{i}: usize = {i};\n"))
            .collect::<String>();
        let after = before
            .replace("ITEM_35: usize = 35", "ITEM_35: usize = 3500")
            .replace("ITEM_37: usize = 37", "ITEM_37: usize = 3700");
        let rows = diff::compare(&before, &after);
        let visible = review_rows(&rows, false);
        let split = split_rows(&rows, &visible);
        let changes = change_targets(&rows);
        assert_eq!(changes.len(), 1, "nearby edits retain Git hunk grouping");
        assert_navigation_from_away(Review {
            path: path.clone(),
            relative_path: path.clone(),
            base: "main".into(),
            before: SourcePreview::build(path.clone(), before, true, &|| false).unwrap(),
            after: SourcePreview::build(path, after, true, &|| false).unwrap(),
            rows,
            visible,
            split,
            changes,
            full: false,
            scroll_y: 0.0,
            hunk: 0,
            jump_requested: false,
            before_newline: true,
            after_newline: true,
        });
    }

    fn assert_navigation_from_away(mut review: Review) {
        assert!(
            !review.changes.is_empty(),
            "fixture needs at least one Git hunk"
        );
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let mut time = 0.0;
        for full in [false, true] {
            review.full = full;
            review.visible = review_rows(&review.rows, full);
            review.split = split_rows(&review.rows, &review.visible);
            for mut mode in [DiffMode::Unified, DiffMode::Split] {
                let layout = mode;
                review.hunk = 0;
                review.scroll_y = 0.0;
                let mut frame = |events, review: &mut Review| {
                    time += 0.1;
                    ctx.run(
                        egui::RawInput {
                            time: Some(time),
                            events,
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(1200.0, 650.0),
                            )),
                            ..Default::default()
                        },
                        |ctx| {
                            egui::CentralPanel::default()
                                .show(ctx, |ui| review.show(ui, &mut mode));
                        },
                    )
                };
                frame(vec![], &mut review);
                let last = review.changes.len() - 1;
                // Refocus from above and below every hunk, including a single
                // hunk and full context far away from the selected change.
                for (label, target) in (0..=last)
                    .map(|i| ("↓ Next", i))
                    .chain((0..=last).rev().map(|i| ("↑ Previous", i)))
                {
                    let row = review.target_rows(layout)[target];
                    let height = crate::preferences::line_height(&ctx).max(20.0);
                    let offset = (row as f32 * height - 2.0 * height).max(0.0);
                    review.scroll_y = if label == "↓ Next" {
                        (offset - 5.0 * height).max(0.0)
                    } else {
                        offset + 5.0 * height
                    };
                    frame(vec![], &mut review);
                    let output = frame(vec![], &mut review);
                    let pos = output
                        .shapes
                        .iter()
                        .find_map(|shape| {
                            if let egui::Shape::Text(text) = &shape.shape
                                && text.galley.text() == label
                            {
                                Some(text.pos + text.galley.size() / 2.0)
                            } else {
                                None
                            }
                        })
                        .expect("navigation button should be visible");
                    frame(
                        vec![
                            egui::Event::PointerMoved(pos),
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed: true,
                                modifiers: egui::Modifiers::NONE,
                            },
                            egui::Event::PointerButton {
                                pos,
                                button: egui::PointerButton::Primary,
                                pressed: false,
                                modifiers: egui::Modifiers::NONE,
                            },
                        ],
                        &mut review,
                    );
                    assert_eq!(
                        review.hunk,
                        target,
                        "full={full}, split={}, button={label}",
                        layout == DiffMode::Split
                    );
                    let expected_row = review.target_rows(layout)[target];
                    let expected = (expected_row as f32 - 2.0).max(0.0)
                        * crate::preferences::line_height(&ctx).max(20.0);
                    assert!(
                        (review.scroll_y - expected).abs() < 1.0,
                        "navigation must scroll to the requested change in the real file"
                    );
                }
            }
        }
    }

    #[test]
    fn previous_next_navigate_real_changes_across_modes_context_and_manual_scroll()
    -> anyhow::Result<()> {
        let dir = crate::git::tests::setup()?;
        let repo = git::repository(dir.path(), None)?;
        let path = repo.root.join("src/code.rs");
        let before = (0..80)
            .map(|i| format!("const ITEM_{i}: usize = {i};\n"))
            .collect::<String>();
        let after = before
            .replace("ITEM_5: usize = 5", "ITEM_5: usize = 500")
            .replace("ITEM_35: usize = 35", "ITEM_35: usize = 3500")
            .replace("ITEM_75: usize = 75", "ITEM_75: usize = 7500");
        let rows = diff::compare(&before, &after);
        let visible = review_rows(&rows, false);
        let split = split_rows(&rows, &visible);
        let mut review = Review {
            path: path.clone(),
            relative_path: path.clone(),
            base: "main".into(),
            before: SourcePreview::build(path.clone(), before, true, &|| false).unwrap(),
            after: SourcePreview::build(path, after, true, &|| false).unwrap(),
            changes: change_targets(&rows),
            jump_requested: true,
            rows,
            visible,
            split,
            full: false,
            scroll_y: 0.0,
            hunk: 0,
            before_newline: true,
            after_newline: true,
        };
        assert_eq!(review.changes.len(), 3);
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let mut mode = DiffMode::Split;
        let mut time = 0.0;
        let mut frame = |events, review: &mut Review, mode: &mut DiffMode| {
            time += 0.1;
            ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 650.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| review.show(ui, mode));
                },
            )
        };
        let locate = |output: &egui::FullOutput, label: &str| {
            output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape
                        && text.galley.text() == label
                    {
                        Some(text.pos + text.galley.size() / 2.0)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| panic!("Missing button {label}"))
        };
        let click = |position| {
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        frame(vec![], &mut review, &mut mode);
        let mut output = frame(vec![], &mut review, &mut mode);
        for target in [1, 2] {
            output = frame(click(locate(&output, "↓ Next")), &mut review, &mut mode);
            assert_eq!(review.hunk, target);
            let expected = (review.target_rows(mode)[target] as f32 - 2.0)
                * crate::preferences::line_height(&ctx).max(20.0);
            assert!(
                (review.scroll_y - expected).abs() < 1.0,
                "even the last hunk must move to its target"
            );
        }
        output = frame(click(locate(&output, "↓ Next")), &mut review, &mut mode);
        assert_eq!(review.hunk, 2, "Next refocuses the last change");
        output = frame(click(locate(&output, "Unified")), &mut review, &mut mode);
        assert!(mode == DiffMode::Unified);
        assert_eq!(
            review.hunk, 2,
            "view switches must preserve the current change"
        );
        output = frame(
            click(locate(&output, "Full context")),
            &mut review,
            &mut mode,
        );
        assert!(review.full);
        assert_eq!(
            review.changes.len(),
            3,
            "full context must not turn all navigation into one hunk"
        );
        assert_eq!(review.hunk, 2);
        output = frame(click(locate(&output, "↑ Previous")), &mut review, &mut mode);
        assert_eq!(review.hunk, 1);
        let expected = (review.target_rows(mode)[1] as f32 - 2.0)
            * crate::preferences::line_height(&ctx).max(20.0);
        assert!((review.scroll_y - expected).abs() < 1.0);
        // Manually scroll back to the top; navigation must follow the viewport.
        let point = egui::pos2(700.0, 250.0);
        frame(
            vec![egui::Event::PointerMoved(point)],
            &mut review,
            &mut mode,
        );
        frame(
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, 5000.0),
                modifiers: egui::Modifiers::NONE,
            }],
            &mut review,
            &mut mode,
        );
        for _ in 0..25 {
            output = frame(vec![], &mut review, &mut mode);
        }
        assert_eq!(review.hunk, 0);
        output = frame(click(locate(&output, "↑ Previous")), &mut review, &mut mode);
        assert_eq!(review.hunk, 0, "Previous refocuses the first change");
        frame(click(locate(&output, "↓ Next")), &mut review, &mut mode);
        assert_eq!(
            review.hunk, 1,
            "Next must advance from the manually scrolled position"
        );
        output = frame(vec![], &mut review, &mut mode);
        frame(click(locate(&output, "Split")), &mut review, &mut mode);
        assert!(mode == DiffMode::Split);
        assert_eq!(review.hunk, 1);
        let expected = (review.target_rows(mode)[1] as f32 - 2.0)
            * crate::preferences::line_height(&ctx).max(20.0);
        assert!(
            (review.scroll_y - expected).abs() < 1.0,
            "expanded split mode must preserve the change target"
        );
        Ok(())
    }

    #[test]
    fn pr_diff_hunks_keep_context_and_line_numbers() {
        let before = (0..30).map(|n| format!("line {n}\n")).collect::<String>();
        let after = before
            .replace("line 5\n", "changed 5\n")
            .replace("line 25\n", "changed 25\n");
        let rows = diff::compare(&before, &after);
        let visible = review_rows(&rows, false);
        assert_eq!(
            visible
                .iter()
                .filter(|r| matches!(r, ReviewRow::Hunk(_)))
                .count(),
            2
        );
        assert!(visible.len() < rows.len());
        assert!(matches!(&visible[0],ReviewRow::Hunk(header) if header=="@@ -3,7 +3,7 @@"));
        let added = review_rows(&diff::compare("", "new\n"), false);
        assert!(matches!(&added[0],ReviewRow::Hunk(header) if header=="@@ -0,0 +1,1 @@"));
    }
    #[test]
    fn changes_tree_pr_diff_and_commit_description_render() -> anyhow::Result<()> {
        let dir = crate::git::tests::setup()?;
        let repo = git::repository(dir.path(), None)?;
        let path = repo.root.join("src/code.rs");
        let review = Review::new(
            &repo,
            path,
            Some("fn main() {\n    render();\n    added();\n}\n".into()),
            true,
        )?;
        let ctx = egui::Context::default();
        crate::chrome::install(&ctx);
        let mut tool = GitTool {
            repo: Some(repo.clone()),
            project: dir.path().to_path_buf(),
            visible: true,
            last_refresh: Some(Instant::now()),
            review: Some(review),
            review_open: true,
            ..Default::default()
        };
        tool.detail = Some(CommitDetail {
            sha: "a".repeat(40),
            author: "Test Author".into(),
            email: String::new(),
            date: "2026-10-17".into(),
            subject: "Add renderer".into(),
            body: "Commit explanation".into(),
            url: None,
            prs: vec![PullRequest {
                number: 42,
                title: "Rendering improvements".into(),
                url: "https://github.com/example/repo/pull/42".into(),
                body: "## Description\n\nImprove **navigation** and rendering.".into(),
                author: "author".into(),
                state: "merged".into(),
                base: "main".into(),
                head: "feature".into(),
                updated: "2026-10-17".into(),
            }],
            pr_note: None,
        });
        for detail in [false, true] {
            tool.review_open = !detail;
            tool.detail_open = detail;
            for _ in 0..2 {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1400.0, 900.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        tool.panel(ctx, dir.path(), &[repo.root.join("src/code.rs")]);
                        tool.windows(ctx);
                        if !detail
                            && let Some(rect) =
                                ctx.memory(|m| m.area_rect(egui::Id::new("branch-review")))
                        {
                            assert!(
                                rect.width() <= 1400.0 && rect.height() <= 900.0,
                                "review must fit the viewport: {rect:?}"
                            );
                        }
                    },
                );
            }
        }
        Ok(())
    }
}
