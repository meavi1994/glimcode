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
            let mut close = false;
            egui::Window::new("Branch changes · PR diff")
                .id(egui::Id::new("branch-review")).title_bar(false).collapsible(false)
                .frame(chrome::popup_frame()).default_size([1120.0, 700.0]).show(ctx, |ui| {
                    chrome::dialog_style(ui, ui.available_width());
                    close = chrome::dialog_header(ui, "±", "Branch changes", "Review your work against the base branch", chrome::BLUE);
                    if let Some(review) = &mut self.review {
                        ui.horizontal(|ui| {
                            ui.strong(review.path.file_name().unwrap_or_default().to_string_lossy());
                            ui.weak(format!("{} → current file", review.base));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add_enabled_ui(review.path.exists(), |ui| chrome::dialog_button(ui, "Open in editor", None)).inner.clicked() { opened = Some(review.path.clone()); }
                            });
                        });
                        ui.label(egui::RichText::new("Branch commits, staged and working-tree changes, and unsaved edits.").size(12.0).color(chrome::MUTED));
                        chrome::dialog_divider(ui);
                        review.show(ui);
                    } else if let Some(note) = &self.review_note {
                        chrome::dialog_card(ui, |ui| { ui.label(note); });
                    } else { ui.horizontal(|ui| { ui.spinner(); ui.label("Preparing diff…"); }); }
                    close |= ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
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

struct Review {
    path: PathBuf,
    base: String,
    before: SourcePreview,
    after: SourcePreview,
    rows: Vec<diff::Row>,
    visible: Vec<ReviewRow>,
    full: bool,
    before_newline: bool,
    after_newline: bool,
}
enum ReviewRow {
    Hunk(String),
    Line(usize),
}
impl Review {
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
        Ok(Self {
            path,
            base: repo.base.clone(),
            before,
            after,
            rows,
            visible,
            full: false,
            before_newline,
            after_newline,
        })
    }
    fn show(&mut self, ui: &mut egui::Ui) {
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
        ui.horizontal(|ui| {
            ui.colored_label(marker(git::Mark::Added), format!("+{added}"));
            ui.colored_label(deleted(), format!("−{removed}"));
            if ui.checkbox(&mut self.full, "Show all context").changed() {
                self.visible = review_rows(&self.rows, self.full);
            }
        });
        ui.separator();
        if added + removed == 0 {
            ui.label("No changes relative to this base.");
            return;
        }
        egui::ScrollArea::both()
            .id_salt(("pr-diff-scroll", &self.path))
            .auto_shrink([false, false])
            .show_rows(
                ui,
                crate::preferences::line_height(ui.ctx()),
                self.visible.len(),
                |ui, range| {
                    ui.set_min_width(
                        self.before.width.max(self.after.width)
                            * crate::preferences::editor_font_size(ui.ctx())
                            / 13.0
                            + 120.0,
                    );
                    for index in range {
                        match &self.visible[index] {
                            ReviewRow::Hunk(header) => {
                                egui::Frame::new().fill(chrome::HEADER).show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.monospace(header);
                                });
                            }
                            ReviewRow::Line(index) => {
                                let row = &self.rows[*index];
                                let bg = if !row.changed {
                                    Color32::TRANSPARENT
                                } else if row.editor.is_some() {
                                    Color32::from_rgb(30, 64, 43)
                                } else {
                                    Color32::from_rgb(72, 34, 40)
                                };
                                egui::Frame::new().fill(bg).show(ui, |ui| {
                                    ui.set_min_width(ui.available_width());
                                    ui.horizontal(|ui| {
                                        for line in [row.disk, row.editor] {
                                            ui.add_sized(
                                                [
                                                    42.0 * crate::preferences::editor_font_size(
                                                        ui.ctx(),
                                                    ) / 13.0,
                                                    crate::preferences::line_height(ui.ctx()),
                                                ],
                                                egui::Label::new(
                                                    RichText::new(
                                                        line.map(|n| (n + 1).to_string())
                                                            .unwrap_or_default(),
                                                    )
                                                    .monospace()
                                                    .color(chrome::MUTED),
                                                ),
                                            );
                                        }
                                        ui.monospace(if !row.changed {
                                            " "
                                        } else if row.editor.is_some() {
                                            "+"
                                        } else {
                                            "−"
                                        });
                                        let (preview, line, newline) =
                                            if let Some(line) = row.editor {
                                                (&self.after, line, self.after_newline)
                                            } else {
                                                (
                                                    &self.before,
                                                    row.disk.unwrap_or(0),
                                                    self.before_newline,
                                                )
                                            };
                                        ui.add(
                                            egui::Label::new(preview.line_for_ui(ui, line, ""))
                                                .extend(),
                                        );
                                        if !newline && line + 1 == preview.len() {
                                            ui.weak("⏎ no newline at end of file");
                                        }
                                    });
                                });
                            }
                        }
                    }
                },
            );
    }
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
                    },
                );
            }
        }
        Ok(())
    }
}
