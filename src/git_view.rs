use crate::{
    chrome, diff,
    git::{self, BlameLine, CommitDetail, FileData, PullRequest, Repository},
    preview::SourcePreview,
};
use eframe::egui::{self, Color32, RichText};
use std::{
    collections::{BTreeMap, VecDeque},
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
    File(crate::explorer::FileAction),
}
impl ChangeAction {
    fn absolute(self, root: &Path) -> Self {
        match self {
            Self::Open(path) => Self::Open(root.join(path)),
            Self::Diff(path) => Self::Diff(root.join(path)),
            Self::File(action) => Self::File(action),
        }
    }
}

#[derive(Default)]
pub struct GitTool {
    pub visible: bool,
    pub show_blame: bool,
    pub repo: Option<Repository>,
    /// Bumped whenever the repository is replaced, so caches that depend on
    /// the changed files can be dropped.
    pub generation: u64,
    pub note: Option<String>,
    pub file_note: Option<String>,
    project: PathBuf,
    selected_base: Option<String>,
    base_filter: String,
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
    pub pending_revert: Option<RevertHunk>,
    review_undo: VecDeque<ReviewEdit>,
    review_mode: DiffMode,
    review_rx: Option<Job<Review>>,
    review_update_rx: Option<Job<ReviewUpdate>>,
    review_next: Option<(Repository, PathBuf, Option<String>, bool)>,
    review_open: bool,
    review_note: Option<String>,
    detail: Option<CommitDetail>,
    detail_rx: Option<Job<CommitDetail>>,
    detail_open: bool,
    detail_note: Option<String>,
    description_cache: egui_commonmark::CommonMarkCache,
    filter: String,
    include_tests: bool,
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
                    self.generation = self.generation.wrapping_add(1);
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
        if let Some(result) = received(&self.review_update_rx) {
            self.review_update_rx = None;
            match result {
                Ok(update) => {
                    if let Some(review) = self.review.as_mut()
                        && review.path == update.path
                        && review.after.source() == update.expected
                    {
                        review.after_newline = update.after.source().ends_with('\n');
                        review.after = update.after;
                        review.rows = update.rows;
                        review.changes = change_targets(&review.rows);
                        review.visible = update.visible;
                        review.split = update.split;
                        review.hunk = review.hunk.min(review.changes.len().saturating_sub(1));
                        ctx.data_mut(|data| {
                            data.remove::<DiffSelection>(egui::Id::new((
                                "diff-selection",
                                &review.path,
                            )));
                        });
                    }
                }
                Err(note) => self.review_note = Some(note),
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
            || self.review_update_rx.is_some()
            || self.detail_rx.is_some()
        {
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if self.repo.is_some() {
            ctx.request_repaint_after(Duration::from_secs(2));
        }
    }
    pub fn cached_file(&self, path: &Path, source: &str) -> Option<Arc<FileData>> {
        let repo = self.repo.as_ref()?;
        if self
            .selected_base
            .as_deref()
            .is_some_and(|base| base != repo.base)
        {
            return None;
        }
        self.file
            .as_ref()
            .map(|(_, data)| data)
            .filter(|data| data.path == path && data.text == source)
            .cloned()
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
            let wait = self
                .desired
                .is_some_and(|(_, time)| time.elapsed() < Duration::from_millis(500));
            if wait {
                ctx.request_repaint_after(Duration::from_millis(500));
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
        self.review_update_rx = None;
        self.review_undo.clear();
        ctx.data_mut(|data| {
            data.remove::<DiffSelection>(egui::Id::new(("diff-selection", &path)));
        });
        self.review = None;
        self.review_note = None;
        let dark = ctx.style().visuals.dark_mode;
        if self.review_rx.is_some() {
            self.review_next = Some((repo, path, buffer, dark));
        } else {
            self.review_rx = Some(worker(ctx, move || Review::new(&repo, path, buffer, dark)));
        }
    }
    pub fn review_shortcuts(&mut self, ctx: &egui::Context) {
        if self.review_open
            && ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::Z))
        {
            self.queue_review_undo();
        }
    }

    fn queue_review_undo(&mut self) {
        if !self.review_open || self.review_update_rx.is_some() || self.pending_revert.is_some() {
            return;
        }
        let Some(review) = &self.review else {
            return;
        };
        let Some(edit) = self.review_undo.back() else {
            return;
        };
        let source = review.after.source();
        let range = edit.start..edit.start + edit.inserted.len();
        if source.get(range.clone()) != Some(edit.inserted.as_str()) {
            return;
        }
        let mut replacement = source.to_owned();
        replacement.replace_range(range, &edit.removed);
        self.pending_revert = Some(RevertHunk {
            path: review.path.clone(),
            expected: source.to_owned(),
            replacement,
            line: source[..edit.start]
                .bytes()
                .filter(|&byte| byte == b'\n')
                .count(),
            undo: true,
        });
    }

    /// Record only edits accepted by the editor against the expected buffer.
    pub fn record_review_edit(&mut self, edit: &RevertHunk) {
        if !self.review_open
            || !self
                .review
                .as_ref()
                .is_some_and(|review| review.path == edit.path)
        {
            return;
        }
        if edit.undo {
            self.review_undo.pop_back();
            return;
        }
        let (old, new) = changed_ranges(&edit.expected, &edit.replacement);
        if old.is_empty() && new.is_empty() {
            return;
        }
        self.review_undo.push_back(ReviewEdit {
            start: old.start,
            removed: edit.expected[old].to_owned(),
            inserted: edit.replacement[new].to_owned(),
        });
        // Bound both step count and retained text; always retain the newest edit.
        let mut bytes: usize = self
            .review_undo
            .iter()
            .map(|edit| edit.removed.len() + edit.inserted.len())
            .sum();
        while self.review_undo.len() > 1
            && (self.review_undo.len() > 128 || bytes > 32 * 1024 * 1024)
        {
            let edit = self.review_undo.pop_front().unwrap();
            bytes -= edit.removed.len() + edit.inserted.len();
        }
    }

    /// Rebuild the working side in the background while keeping the review visible.
    pub fn refresh_review_buffer(&mut self, path: &Path, source: String, ctx: &egui::Context) {
        let Some(review) = self
            .review
            .as_ref()
            .filter(|review| self.review_open && review.path == path)
        else {
            return;
        };
        let path = review.path.clone();
        let before = review.before.source().to_owned();
        let expected = review.after.source().to_owned();
        let full = review.full;
        let dark = ctx.style().visuals.dark_mode;
        self.review_update_rx = Some(worker(ctx, move || {
            let rows = diff::compare(&before, &source);
            let visible = review_rows(&rows, full);
            let split = split_rows(&rows, &visible);
            let after = SourcePreview::build(path.clone(), source, dark, &|| false)
                .ok_or_else(|| anyhow::anyhow!("Could not highlight updated file"))?;
            Ok(ReviewUpdate {
                path,
                expected,
                after,
                rows,
                visible,
                split,
            })
        }));
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
    #[cfg(test)]
    pub fn panel(
        &mut self,
        ctx: &egui::Context,
        project: &Path,
        dirty: &[PathBuf],
    ) -> Option<ChangeAction> {
        if !self.visible {
            return None;
        }
        let mut selected = None;
        egui::SidePanel::right("git-changes-panel")
            .default_width(320.0)
            .width_range(240.0..=650.0)
            .show(ctx, |ui| {
                selected = self.content(ui, project, dirty, true);
            });
        selected
    }
    pub fn content(
        &mut self,
        ui: &mut egui::Ui,
        project: &Path,
        dirty: &[PathBuf],
        changes_only: bool,
    ) -> Option<ChangeAction> {
        let ctx = ui.ctx().clone();
        if self.last_refresh.is_none() {
            self.refresh(project, &ctx);
        }
        let mut selected = None;
        let mut refresh = false;
        if let Some(repo) = &self.repo {
            let previous_base = self.selected_base.as_deref().unwrap_or(&repo.base);
            let mut base = previous_base.to_owned();
            egui::Frame::new()
                .fill(chrome::EDITOR)
                .stroke(egui::Stroke::new(1.0_f32, chrome::BORDER))
                .corner_radius(7)
                .inner_margin(egui::Margin::symmetric(10, 5))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 7.0;
                    egui::CollapsingHeader::new(
                        RichText::new("COMPARE CHANGES")
                            .size(10.0)
                            .strong()
                            .color(chrome::MUTED),
                    )
                    .id_salt("git-comparison-card")
                    .default_open(false)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            chrome::heading_icon(ui, chrome::Icon::Branch, chrome::BLUE);
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&repo.branch).strong().color(chrome::TEXT),
                                )
                                .truncate(),
                            )
                            .on_hover_text(format!("Current branch: {}", repo.branch));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if self.repo_rx.is_some() {
                                        ui.spinner();
                                    } else if chrome::icon_button(
                                        ui,
                                        chrome::Icon::Refresh,
                                        "Refresh comparison",
                                    )
                                    .clicked()
                                    {
                                        refresh = true;
                                    }
                                },
                            );
                        });
                        ui.label(
                            RichText::new("Compare against")
                                .size(11.0)
                                .color(chrome::MUTED),
                        );
                        ui.add_enabled_ui(self.repo_rx.is_none(), |ui| {
                            ui.spacing_mut().button_padding = egui::vec2(10.0, 8.0);
                            egui::ComboBox::from_id_salt("git-base")
                                .selected_text(RichText::new(&base).color(chrome::BLUE))
                                .width(ui.available_width())
                                .height(260.0)
                                .wrap_mode(egui::TextWrapMode::Truncate)
                                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                                .show_ui(ui, |ui| {
                                    ui.set_max_width(ui.available_width());
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.base_filter)
                                            .hint_text("Find a branch…")
                                            .desired_width(ui.available_width()),
                                    );
                                    ui.separator();
                                    let filter = chrome::CaseFilter::new(self.base_filter.trim());
                                    let mut found = false;
                                    for branch in &repo.branches {
                                        if !filter.matches(branch) {
                                            continue;
                                        }
                                        found = true;
                                        let response = ui
                                            .add(
                                                egui::Button::selectable(
                                                    base == *branch,
                                                    RichText::new(branch).size(12.0),
                                                )
                                                .min_size(egui::vec2(ui.available_width(), 30.0))
                                                .wrap_mode(egui::TextWrapMode::Truncate),
                                            )
                                            .on_hover_text(branch);
                                        if response.clicked() {
                                            base.clone_from(branch);
                                            self.base_filter.clear();
                                            ui.close();
                                        }
                                    }
                                    if !found {
                                        ui.label(
                                            RichText::new("No matching branches")
                                                .color(chrome::MUTED),
                                        );
                                    }
                                })
                                .response
                                .on_hover_text(&base);
                        });
                        if let Some(pr) = &self.pr {
                            ui.add(egui::Hyperlink::from_label_and_url(
                                format!("PR #{} ↗", pr.number),
                                &pr.url,
                            ))
                            .on_hover_text(&pr.title);
                        }
                        egui::CollapsingHeader::new(
                            RichText::new("Comparison details")
                                .size(11.0)
                                .color(chrome::MUTED),
                        )
                        .id_salt("files-git-comparison")
                        .default_open(false)
                        .show(ui, |ui| {
                            if let Some(base) = &repo.merge_base {
                                ui.label(
                                    RichText::new(format!(
                                        "Merge base {}",
                                        &base[..base.len().min(10)]
                                    ))
                                    .size(11.0)
                                    .color(chrome::MUTED),
                                );
                            }
                            if let Some(note) = &self.pr_note {
                                ui.weak("PR information unavailable").on_hover_text(note);
                            } else if self.pr_rx.is_some() {
                                ui.weak("Finding branch PR…");
                            }
                            ui.horizontal_wrapped(|ui| {
                                ui.colored_label(committed(), "● Committed");
                                ui.colored_label(working(), "● Uncommitted");
                                ui.colored_label(deleted(), "● Deleted");
                            });
                        });
                    });
                });
            ui.add_space(6.0);
            if base != previous_base {
                self.selected_base = Some(base);
                refresh = true;
                self.file = None;
                self.failed_file = None;
            }
            if changes_only {
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text("Filter changed files…")
                        .desired_width(f32::INFINITY),
                );
                let mut hide_tests = !self.include_tests;
                ui.checkbox(&mut hide_tests, RichText::new("Hide test files").size(12.0))
                    .on_hover_text(
                        "Hide files in test folders and files with common test naming conventions.",
                    );
                self.include_tests = !hide_tests;
                let files = tree_files(repo, dirty);
                let total = files.len();
                let hidden_tests = if hide_tests {
                    files
                        .iter()
                        .filter(|file| crate::test_files::is_test_file(&file.path))
                        .count()
                } else {
                    0
                };
                let tree = ChangeTree::build(&files, &self.filter, hide_tests);
                let shown = tree.file_count();
                if hide_tests || !self.filter.is_empty() {
                    let detail = if hidden_tests > 0 {
                        format!(
                            " · {hidden_tests} test {} hidden",
                            if hidden_tests == 1 { "file" } else { "files" }
                        )
                    } else {
                        String::new()
                    };
                    ui.label(
                        RichText::new(format!("{shown} of {total} changed files{detail}"))
                            .size(11.0)
                            .color(chrome::MUTED),
                    );
                } else {
                    ui.weak(format!("{total} changed files · click to open"));
                }
                ui.separator();
                egui::ScrollArea::both()
                    .id_salt("changed-file-tree")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        selected = tree
                            .show(ui, &files, Path::new(""), &repo.root)
                            .map(|action| action.absolute(&repo.root));
                    });
                if files.is_empty() {
                    ui.weak("No changes relative to the selected base.");
                } else if shown == 0 {
                    ui.weak("No changed files match the current filters.");
                }
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
        if refresh {
            self.pr_key = None;
            self.file = None;
            self.failed_file = None;
            self.refresh(project, &ctx);
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
            let mut undo_requested = false;
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
                    chrome::dialog_typography(ui);
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
                    ui.spacing_mut().button_padding = egui::vec2(9.0, 4.0);
                    ui.spacing_mut().interact_size.y = 26.0;
                    ui.horizontal(|ui| {
                        chrome::heading_icon(ui, chrome::Icon::Branch, chrome::BLUE);
                        ui.label(
                            RichText::new("Branch changes")
                                .size(crate::preferences::dialog_font_size(ui.ctx()) + 1.0)
                                .strong(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            close =
                                chrome::icon_button(ui, chrome::Icon::Close, "Close diff (Esc)")
                                    .clicked();
                            if let Some(review) = &self.review {
                                ui.weak(
                                    RichText::new(format!("{} → working tree", review.base))
                                        .size(crate::preferences::dialog_font_size(ui.ctx())),
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
                                            undo_requested = ui
                                                .add_enabled(
                                                    !self.review_undo.is_empty()
                                                        && self.review_update_rx.is_none()
                                                        && self.pending_revert.is_none(),
                                                    egui::Button::new("Undo applied change")
                                                        .frame(false),
                                                )
                                                .on_hover_text(
                                                    "Undo the last applied change (⌘/Ctrl Z)",
                                                )
                                                .clicked();
                                        },
                                    );
                                });
                            });
                        ui.add_enabled_ui(self.review_update_rx.is_none(), |ui| {
                            review.show(ui, &mut self.review_mode);
                        });
                        let id = egui::Id::new(("revert-hunk", &review.path));
                        if let Some(hunk) = ui.ctx().data_mut(|data| data.remove_temp::<usize>(id))
                        {
                            review.hunk = hunk;
                            self.pending_revert = review.revert_hunk();
                        }
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
            if undo_requested {
                self.queue_review_undo();
            }
            if close {
                self.review_open = false;
                self.review_undo.clear();
                // The review holds two copies of the compared files, so they
                // are released when the window closes.
                self.review = None;
                self.review_update_rx = None;
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
                        ui.label(
                            RichText::new(&detail.subject)
                                .size(crate::preferences::dialog_font_size(ui.ctx()) + 4.0)
                                .strong(),
                        );
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
/// Use the same status precedence in Project and Changes, including unsaved buffers.
pub fn tree_files(repo: &Repository, dirty: &[PathBuf]) -> Vec<git::ChangedFile> {
    crate::ignore_rules::with_global_ignores(&repo.root, |ignores| {
        tree_files_with_ignores(repo, dirty, ignores)
    })
}

fn tree_files_with_ignores(
    repo: &Repository,
    dirty: &[PathBuf],
    ignores: &mut crate::ignore_rules::GlobalIgnores,
) -> Vec<git::ChangedFile> {
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
    files.retain(|file| !ignores.is_ignored(&repo.root.join(&file.path)));
    files
}

pub fn file_color(file: &git::ChangedFile) -> Color32 {
    if file.deleted {
        deleted()
    } else if file.uncommitted {
        working()
    } else {
        committed()
    }
}

pub fn committed() -> Color32 {
    Color32::from_rgb(82, 168, 104)
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
        git::Mark::Modified => Color32::from_rgb(100, 165, 245),
        git::Mark::Deleted => deleted(),
    }
}

#[derive(Default)]
struct ChangeTree {
    children: BTreeMap<String, ChangeTree>,
    file: Option<usize>,
}
impl ChangeTree {
    fn build(files: &[git::ChangedFile], filter: &str, hide_tests: bool) -> Self {
        let mut tree = Self::default();
        let filter = filter.to_lowercase();
        for (index, file) in files.iter().enumerate() {
            if !file.path.to_string_lossy().to_lowercase().contains(&filter)
                || (hide_tests && crate::test_files::is_test_file(&file.path))
            {
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
    fn file_count(&self) -> usize {
        usize::from(self.file.is_some())
            + self.children.values().map(Self::file_count).sum::<usize>()
    }
    fn show(
        &self,
        ui: &mut egui::Ui,
        files: &[git::ChangedFile],
        directory: &Path,
        root: &Path,
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
                    let color = file_color(file);
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
                        ui.separator();
                        if let Some(action) =
                            crate::explorer::context_menu(ui, root, &root.join(&file.path), false)
                        {
                            selected = Some(ChangeAction::File(action));
                        }
                    });
                } else {
                    let header = chrome::folder_header(
                        ui,
                        ui.make_persistent_id(("git-folder", &path)),
                        &name,
                        true,
                        None,
                        |ui| {
                            if let Some(path) = node.show(ui, files, &path, root) {
                                selected = Some(path);
                            }
                        },
                    );
                    header.context_menu(|ui| {
                        if let Some(action) =
                            crate::explorer::context_menu(ui, root, &root.join(&path), true)
                        {
                            selected = Some(ChangeAction::File(action));
                        }
                    });
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

pub struct RevertHunk {
    pub path: PathBuf,
    pub expected: String,
    pub replacement: String,
    pub line: usize,
    pub undo: bool,
}

struct ReviewEdit {
    start: usize,
    removed: String,
    inserted: String,
}

#[derive(Clone, Default)]
struct DiffSelection {
    pane: usize,
    anchor: (usize, usize),
    end: (usize, usize),
    active: bool,
    dragging: bool,
}
impl DiffSelection {
    fn range(&self, pane: usize, row: usize, len: usize) -> Option<std::ops::Range<usize>> {
        if !self.active || self.pane != pane {
            return None;
        }
        let (start, end) = (self.anchor.min(self.end), self.anchor.max(self.end));
        if row < start.0 || row > end.0 {
            return None;
        }
        Some(
            (if row == start.0 { start.1.min(len) } else { 0 })..(if row == end.0 {
                end.1.min(len)
            } else {
                len
            }),
        )
    }
}

struct ReviewUpdate {
    path: PathBuf,
    expected: String,
    after: SourcePreview,
    rows: Vec<diff::Row>,
    visible: Vec<ReviewRow>,
    split: Vec<SplitRow>,
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

/// Review actions operate on contiguous edits, independently of the context
/// that groups nearby edits under a shared Git-style hunk header.
fn change_targets(rows: &[diff::Row]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(index, row)| {
            (row.changed && (index == 0 || !rows[index - 1].changed)).then_some(index)
        })
        .collect()
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
    fn revert_hunk(&self) -> Option<RevertHunk> {
        let start = *self.changes.get(self.hunk)?;
        let end = start
            + self.rows[start..]
                .iter()
                .take_while(|row| row.changed)
                .count();
        let old_start = self.rows[..start]
            .iter()
            .filter(|row| row.disk.is_some())
            .count();
        let new_start = self.rows[..start]
            .iter()
            .filter(|row| row.editor.is_some())
            .count();
        let old_count = self.rows[start..end]
            .iter()
            .filter(|row| row.disk.is_some())
            .count();
        let new_count = self.rows[start..end]
            .iter()
            .filter(|row| row.editor.is_some())
            .count();
        let before = self.before.source();
        let after = self.after.source();
        let byte = |text: &str, line: usize| {
            text.split_inclusive('\n')
                .take(line)
                .map(str::len)
                .sum::<usize>()
        };
        let mut replacement = after.to_owned();
        replacement.replace_range(
            byte(after, new_start)..byte(after, new_start + new_count),
            &before[byte(before, old_start)..byte(before, old_start + old_count)],
        );
        Some(RevertHunk {
            path: self.path.clone(),
            expected: after.to_owned(),
            replacement,
            line: new_start,
            undo: false,
        })
    }

    fn selected_text(&self, selection: &DiffSelection) -> String {
        let lines: Vec<(usize, &str)> = if selection.pane == 0 {
            self.visible
                .iter()
                .enumerate()
                .filter_map(|(index, row)| {
                    let ReviewRow::Line(row) = row else {
                        return None;
                    };
                    let row = &self.rows[*row];
                    Some((
                        index,
                        if let Some(line) = row.editor {
                            self.after.line_text(line)
                        } else {
                            self.before.line_text(row.disk?)
                        },
                    ))
                })
                .collect()
        } else {
            self.split
                .iter()
                .enumerate()
                .filter_map(|(index, row)| {
                    let SplitRow::Line { before, after, .. } = row else {
                        return None;
                    };
                    Some((
                        index,
                        if selection.pane == 1 {
                            self.before.line_text((*before)?)
                        } else {
                            self.after.line_text((*after)?)
                        },
                    ))
                })
                .collect()
        };
        lines
            .into_iter()
            .filter_map(|(row, text)| {
                let range = selection.range(selection.pane, row, text.chars().count())?;
                Some(
                    text.chars()
                        .skip(range.start)
                        .take(range.len())
                        .collect::<String>(),
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

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
        let selection_id = egui::Id::new(("diff-selection", &self.path));
        ui.interact(
            ui.available_rect_before_wrap(),
            selection_id,
            egui::Sense::focusable_noninteractive(),
        );
        let mut selection = ui
            .ctx()
            .data_mut(|data| data.get_temp::<DiffSelection>(selection_id))
            .unwrap_or_default();
        let previous_mode = *mode;
        let height = crate::preferences::line_height(ui.ctx()).max(20.0);
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                chrome::GREEN,
                RichText::new(format!("+{added}"))
                    .strong()
                    .size(crate::preferences::dialog_font_size(ui.ctx())),
            );
            ui.colored_label(
                deleted(),
                RichText::new(format!("−{removed}"))
                    .strong()
                    .size(crate::preferences::dialog_font_size(ui.ctx())),
            );
            ui.separator();
            for (label, value) in [("Unified", DiffMode::Unified), ("Split", DiffMode::Split)] {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(label)
                                .size(crate::preferences::dialog_font_size(ui.ctx())),
                        )
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
                selection.active = false;
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
                ui.weak(
                    RichText::new(format!("{} / {} changes", self.hunk + 1, count))
                        .size(crate::preferences::dialog_font_size(ui.ctx())),
                );
            }
        });
        if *mode != previous_mode {
            self.jump_requested = true;
            selection.active = false;
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
                    .scroll_source(egui::scroll_area::ScrollSource {
                        drag: false,
                        ..egui::scroll_area::ScrollSource::ALL
                    })
                    .vertical_scroll_offset(self.scroll_y)
                    .auto_shrink([false, false])
                    .show_rows(
                        ui,
                        height,
                        self.visible.len() + (ui.available_height() / height).ceil() as usize,
                        |ui, range| {
                            let width = ui.available_width().max(
                                self.before.width.max(self.after.width) * scale
                                    + 150.0
                                    + if self.before_newline && self.after_newline {
                                        0.0
                                    } else {
                                        200.0
                                    },
                            );
                            ui.set_min_width(width);
                            scroll_diff_selection(ui, &selection, 0);
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
                                            &mut selection,
                                            0,
                                            index,
                                            selection_id,
                                            targets
                                                .binary_search(&index)
                                                .ok()
                                                .filter(|_| self.path.is_file()),
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
                let mut gutter_rows = Vec::new();
                let mut gutter_rect = egui::Rect::NOTHING;
                ui.scope(|ui| {
                    ui.spacing_mut().item_spacing.x = 28.0;
                    ui.columns(2, |columns| {
                        gutter_rect = egui::Rect::from_min_max(
                            egui::pos2(columns[0].max_rect().right(), columns[0].max_rect().top()),
                            egui::pos2(
                                columns[1].max_rect().left(),
                                columns[1].max_rect().bottom(),
                            ),
                        );
                        for (side, ui) in columns.iter_mut().enumerate() {
                            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
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
                                .scroll_source(egui::scroll_area::ScrollSource {
                                    drag: false,
                                    ..egui::scroll_area::ScrollSource::ALL
                                })
                                .scroll_bar_visibility(
                                    egui::scroll_area::ScrollBarVisibility::AlwaysVisible,
                                )
                                .vertical_scroll_offset(old_y)
                                .auto_shrink([false, false])
                                .show_rows(
                                    ui,
                                    height,
                                    self.split.len()
                                        + (ui.available_height() / height).ceil() as usize,
                                    |ui, range| {
                                        let width = ui.available_width().max(
                                            preview.width * scale
                                                + 110.0
                                                + if newline { 0.0 } else { 200.0 },
                                        );
                                        ui.set_min_width(width);
                                        scroll_diff_selection(ui, &selection, side + 1);
                                        for index in
                                            range.take_while(|&index| index < self.split.len())
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
                                                    let line =
                                                        if side == 0 { *before } else { *after };
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
                                                            if side == 0 {
                                                                ranges.0
                                                            } else {
                                                                ranges.1
                                                            }
                                                        });
                                                    if side == 0 {
                                                        let top = ui.next_widget_position().y;
                                                        let rect = egui::Rect::from_min_max(
                                                            egui::pos2(gutter_rect.left(), top),
                                                            egui::pos2(
                                                                gutter_rect.right(),
                                                                top + height,
                                                            ),
                                                        );
                                                        let clip = egui::Rect::from_min_max(
                                                            egui::pos2(
                                                                gutter_rect.left(),
                                                                ui.clip_rect().top(),
                                                            ),
                                                            egui::pos2(
                                                                gutter_rect.right(),
                                                                ui.clip_rect().bottom(),
                                                            ),
                                                        );
                                                        gutter_rows
                                                            .push((index, rect, clip, *changed));
                                                    }
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
                                                        &mut selection,
                                                        side + 1,
                                                        index,
                                                        selection_id,
                                                        None,
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
                });
                ui.painter().rect_filled(gutter_rect, 0, chrome::PANEL);
                for (index, rect, clip, changed) in gutter_rows {
                    let painter = ui.painter().with_clip_rect(clip);
                    if changed {
                        painter.rect_filled(rect, 0, chrome::BLUE.gamma_multiply(0.12));
                    }
                    if self.path.is_file()
                        && let Ok(hunk) = targets.binary_search(&index)
                        && rect.intersects(clip)
                    {
                        paint_apply_arrow(ui, rect.intersect(clip), selection_id, 1, index, hunk);
                    }
                }
                if (next_y - self.scroll_y).abs() > 0.5 {
                    ui.ctx().request_repaint();
                }
                self.scroll_y = next_y;
            }
        }
        if let Some(hunk) = ui
            .ctx()
            .data_mut(|data| data.remove_temp::<usize>(selection_id.with("apply-hunk")))
        {
            ui.ctx().data_mut(|data| {
                data.insert_temp(egui::Id::new(("revert-hunk", &self.path)), hunk)
            });
        }
        if selection.dragging && !ui.input(|input| input.pointer.primary_down()) {
            ui.memory_mut(|memory| memory.request_focus(selection_id));
            selection.dragging = false;
        }
        if selection.active && ui.memory(|memory| memory.has_focus(selection_id)) {
            if ui.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::A)) {
                selection.anchor = (0, 0);
                selection.end = (usize::MAX, usize::MAX);
            }
            let copy = ui.input_mut(|input| {
                let copy = input
                    .events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Copy));
                input
                    .events
                    .retain(|event| !matches!(event, egui::Event::Copy));
                copy || input.consume_key(egui::Modifiers::COMMAND, egui::Key::C)
            });
            if copy {
                ui.ctx().copy_text(self.selected_text(&selection));
            }
        }
        if ui.input(|input| input.pointer.primary_down()) {
            ui.ctx().request_repaint();
        }
        ui.ctx()
            .data_mut(|data| data.insert_temp(selection_id, selection));
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

/// Move the viewport while dragging at its edge; wheel scrolling remains independent.
fn scroll_diff_selection(ui: &mut egui::Ui, selection: &DiffSelection, pane: usize) {
    if !selection.dragging || selection.pane != pane || !ui.input(|i| i.pointer.primary_down()) {
        return;
    }
    if let Some(pointer) = ui.input(|i| i.pointer.interact_pos()) {
        let clip = ui.clip_rect();
        if pointer.x < clip.left() || pointer.x > clip.right() {
            return;
        }
        let delta = if pointer.y < clip.top() + 20.0 {
            12.0
        } else if pointer.y > clip.bottom() - 20.0 {
            -12.0
        } else {
            0.0
        };
        if delta != 0.0 {
            ui.scroll_with_delta(egui::vec2(0.0, delta));
            ui.ctx().request_repaint();
        }
    }
}

/// A fixed divider control, separate from selectable and horizontally scrolling code.
fn paint_apply_arrow(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    selection_id: egui::Id,
    pane: usize,
    row: usize,
    hunk: usize,
) {
    let response = ui
        .interact(
            rect,
            selection_id.with(("apply-diff-hunk", pane, row)),
            egui::Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Apply this change from base to working file (unsaved; undo in editor)");
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    if response.hovered() {
        painter.rect_filled(rect.shrink(1.0), 3, chrome::SELECTED);
    }
    let center = rect.center();
    let color = if response.hovered() {
        chrome::TEXT
    } else {
        chrome::BLUE
    };
    for offset in [-3.0, 3.0] {
        painter.add(egui::Shape::line(
            vec![
                center + egui::vec2(offset - 2.0, -4.0),
                center + egui::vec2(offset + 2.0, 0.0),
                center + egui::vec2(offset - 2.0, 4.0),
            ],
            egui::Stroke::new(1.4_f32, color),
        ));
    }
    if response.clicked() {
        ui.ctx()
            .data_mut(|data| data.insert_temp(selection_id.with("apply-hunk"), hunk));
    }
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
    selection: &mut DiffSelection,
    pane: usize,
    row: usize,
    selection_id: egui::Id,
    apply_hunk: Option<usize>,
) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click_and_drag());
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
    let action_gutter = pane == 0;
    let edge = rect.right().min(ui.clip_rect().right());
    let action_rect = egui::Rect::from_min_max(
        egui::pos2(edge - 30.0, rect.top()),
        egui::pos2(edge, rect.bottom()),
    );
    let text_clip = if action_gutter {
        ui.clip_rect().intersect(egui::Rect::from_min_max(
            rect.min,
            egui::pos2(edge - 30.0, rect.bottom()),
        ))
    } else {
        rect.intersect(ui.clip_rect())
    };
    let painter = ui.painter().with_clip_rect(text_clip);
    painter.rect_filled(rect, 0, background);
    if action_gutter {
        ui.painter().rect_filled(action_rect, 0, chrome::PANEL);
        if let Some(hunk) = apply_hunk {
            paint_apply_arrow(ui, action_rect, selection_id, pane, row, hunk);
        }
    }
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
    let mut galley = ui.fonts_mut(|fonts| fonts.layout_job(job.clone()));
    let text_pos = egui::pos2(
        gutter.right() + 22.0,
        rect.center().y - galley.size().y / 2.0,
    );
    if let Some(pointer) = ui.input(|input| input.pointer.interact_pos())
        && text_clip.contains(pointer)
    {
        let column = galley.cursor_from_pos(pointer - text_pos).index;
        if response.hovered() && ui.input(|input| input.pointer.primary_pressed()) {
            *selection = DiffSelection {
                pane,
                anchor: (row, column),
                end: (row, column),
                active: true,
                dragging: true,
            };
            ui.memory_mut(|memory| memory.request_focus(selection_id));
        } else if selection.dragging
            && selection.pane == pane
            && ui.input(|input| input.pointer.primary_down())
        {
            selection.end = (row, column);
        }
        if response.clicked() || response.dragged() {
            ui.memory_mut(|memory| memory.request_focus(selection_id));
        }
        if response.double_clicked() {
            let chars: Vec<_> = job.text.chars().collect();
            let mut start = column.min(chars.len());
            let mut end = start;
            let word = |c: char| c.is_alphanumeric() || c == '_';
            while start > 0 && word(chars[start - 1]) {
                start -= 1;
            }
            while end < chars.len() && word(chars[end]) {
                end += 1;
            }
            selection.anchor = (row, start);
            selection.end = (row, end);
        }
        if response.triple_clicked() {
            selection.anchor = (row, 0);
            selection.end = (row, job.text.chars().count());
        }
        ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
    }
    if let Some(range) = selection.range(pane, row, job.text.chars().count()) {
        let byte = |column| {
            job.text
                .char_indices()
                .nth(column)
                .map_or(job.text.len(), |(byte, _)| byte)
        };
        let range = byte(range.start)..byte(range.end);
        let mut sections = Vec::new();
        for section in &job.sections {
            let mut cuts = vec![section.byte_range.start, section.byte_range.end];
            cuts.extend(
                [range.start, range.end]
                    .into_iter()
                    .filter(|cut| section.byte_range.contains(cut)),
            );
            cuts.sort_unstable();
            cuts.dedup();
            for pair in cuts.windows(2) {
                let mut part = section.clone();
                part.byte_range = pair[0]..pair[1];
                if pair[0] >= range.start && pair[1] <= range.end {
                    part.format.background = ui.visuals().selection.bg_fill;
                    part.format.color = ui.visuals().selection.stroke.color;
                }
                sections.push(part);
            }
        }
        job.sections = sections;
        galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    }
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
        if selection.active && ui.button("Copy selection").clicked() {
            ui.memory_mut(|memory| memory.request_focus(selection_id));
            ui.input_mut(|input| input.events.push(egui::Event::Copy));
            ui.close();
        }
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
    use super::*;

    #[test]
    fn changed_tree_filters_global_patterns_for_git_entries_and_unsaved_files() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        let global = root.join("global.gitignore");
        std::fs::write(&global, "**/tmp/\n*.log\n/root-only.rs\n!keep.log\n")?;
        let repo = Repository {
            root: root.into(),
            branch: "main".into(),
            head: String::new(),
            base: "main".into(),
            merge_base: None,
            branches: vec![],
            remote: None,
            files: vec![
                git::ChangedFile {
                    path: "tmp/tracked.rs".into(),
                    committed: true,
                    ..Default::default()
                },
                git::ChangedFile {
                    path: "nested/tmp/deleted.rs".into(),
                    deleted: true,
                    ..Default::default()
                },
                git::ChangedFile {
                    path: "debug.log".into(),
                    uncommitted: true,
                    ..Default::default()
                },
                git::ChangedFile {
                    path: "root-only.rs".into(),
                    added: true,
                    ..Default::default()
                },
                git::ChangedFile {
                    path: "src/main.rs".into(),
                    committed: true,
                    ..Default::default()
                },
                git::ChangedFile {
                    path: "keep.log".into(),
                    uncommitted: true,
                    ..Default::default()
                },
            ],
        };
        let dirty = [
            root.join("tmp/unsaved.rs"),
            root.join("src/debug.log"),
            root.join("src/new.rs"),
        ];
        let mut ignores = crate::ignore_rules::GlobalIgnores::load(root, Some(&global))?;
        let files = tree_files_with_ignores(&repo, &dirty, &mut ignores);
        assert_eq!(
            files
                .iter()
                .map(|file| file.path.as_path())
                .collect::<Vec<_>>(),
            vec![
                Path::new("src/main.rs"),
                Path::new("keep.log"),
                Path::new("src/new.rs")
            ]
        );
        assert_eq!(ChangeTree::build(&files, "", false).file_count(), 3);
        assert_eq!(
            repo.files.len(),
            6,
            "filtering the UI must preserve the Git snapshot"
        );
        Ok(())
    }
    fn review_fixture(before: &str, after: &str) -> Review {
        let path = PathBuf::from("sample.rs");
        let rows = diff::compare(before, after);
        let visible = review_rows(&rows, false);
        let split = split_rows(&rows, &visible);
        Review {
            path: path.clone(),
            relative_path: path.clone(),
            base: "main".into(),
            before: SourcePreview::build(path.clone(), before.into(), true, &|| false).unwrap(),
            after: SourcePreview::build(path, after.into(), true, &|| false).unwrap(),
            changes: change_targets(&rows),
            rows,
            visible,
            split,
            full: false,
            scroll_y: 0.0,
            hunk: 0,
            jump_requested: false,
            before_newline: before.ends_with('\n'),
            after_newline: after.ends_with('\n'),
        }
    }

    #[test]
    fn revert_hunk_preserves_other_edits_unicode_and_line_endings() {
        for (before, after) in [
            ("", "猫\n"),
            ("猫\n", ""),
            ("a\nb", "a"),
            ("a\n", "a"),
            ("a", "a\n"),
            ("a\r\n猫\r\n", "a\r\n犬\r\n"),
            ("a\nb\nc\n", "a\nx\ny\nc\n"),
        ] {
            let review = review_fixture(before, after);
            let revert = review.revert_hunk().unwrap();
            assert_eq!(revert.expected, after);
            assert_eq!(revert.replacement, before);
        }
        let before = (0..30).map(|i| format!("line {i}\n")).collect::<String>();
        let after = before
            .replace("line 2\n", "猫\n")
            .replace("line 25\n", "犬\n");
        let mut review = review_fixture(&before, &after);
        assert_eq!(review.changes.len(), 2);
        assert_eq!(
            review.revert_hunk().unwrap().replacement,
            before.replace("line 25\n", "犬\n")
        );
        review.hunk = 1;
        assert_eq!(
            review.revert_hunk().unwrap().replacement,
            before.replace("line 2\n", "猫\n")
        );
    }

    #[test]
    fn nearby_edits_revert_independently_inside_one_context_hunk() {
        for (before, after, first_reverted, second_reverted) in [
            (
                "# generated\nvendordiff.patch\n",
                "# generated\n\nvendordiff.patch\nmain.go\n",
                "# generated\nvendordiff.patch\nmain.go\n",
                "# generated\n\nvendordiff.patch\n",
            ),
            (
                "α\nkeep\n猫\n",
                "犬\nkeep\n鳥\n",
                "α\nkeep\n鳥\n",
                "犬\nkeep\n猫\n",
            ),
            (
                "remove first\nkeep\nremove last\n",
                "keep\n",
                "remove first\nkeep\n",
                "keep\nremove last\n",
            ),
        ] {
            let mut review = review_fixture(before, after);
            assert_eq!(diff::hunk_targets(&review.rows).len(), 1);
            assert_eq!(review.changes.len(), 2);
            assert_eq!(review.revert_hunk().unwrap().replacement, first_reverted);
            review.hunk = 1;
            assert_eq!(review.revert_hunk().unwrap().replacement, second_reverted);
            for full in [false, true] {
                review.visible = review_rows(&review.rows, full);
                review.split = split_rows(&review.rows, &review.visible);
                for mode in [DiffMode::Split, DiffMode::Unified] {
                    let targets = review.target_rows(mode);
                    assert_eq!(targets.len(), 2);
                    assert!(targets[0] < targets[1]);
                    let offset = (targets[0] as f32 - 2.0).max(0.0) * 20.0;
                    assert_eq!(navigation_target(&targets, offset, 20.0, true), 1);
                }
            }
        }
        // A replacement is one block even though the raw diff has deletions and insertions.
        assert_eq!(review_fixture("a\nb\n", "x\ny\nz\n").changes.len(), 1);
    }

    #[test]
    fn applying_hunks_keeps_review_open_and_refreshes_until_no_changes_remain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.rs");
        let middle = (0..1).map(|i| format!("context {i}\n")).collect::<String>();
        let before = format!("old first\n{middle}old last\n");
        let after = format!("new first\n{middle}new last\n");
        std::fs::write(&path, &after).unwrap();
        let mut review = review_fixture(&before, &after);
        review.path = path.clone();
        review.full = true;
        review.visible = review_rows(&review.rows, true);
        review.split = split_rows(&review.rows, &review.visible);
        let mut tool = GitTool {
            project: dir.path().to_path_buf(),
            review: Some(review),
            review_open: true,
            ..Default::default()
        };
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        for remaining in [1, 0] {
            ctx.data_mut(|data| data.insert_temp(egui::Id::new(("revert-hunk", &path)), 0_usize));
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    assert!(tool.windows(ctx).is_none());
                },
            );
            assert!(tool.review_open, "applying a hunk must keep the diff open");
            let revert = tool.pending_revert.take().unwrap();
            tool.review.as_mut().unwrap().scroll_y = 42.0;
            tool.refresh_review_buffer(&path, revert.replacement.clone(), &ctx);
            assert!(
                tool.review.is_some(),
                "keep the current diff visible while refreshing"
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            while tool.review_update_rx.is_some() {
                assert!(Instant::now() < deadline, "review refresh timed out");
                tool.poll(dir.path(), &ctx);
                std::thread::sleep(Duration::from_millis(2));
            }
            let review = tool.review.as_ref().unwrap();
            assert!(tool.review_open);
            assert_eq!(review.after.source(), revert.replacement);
            assert_eq!(review.changes.len(), remaining);
            assert_eq!(review.scroll_y, 42.0);
            assert!(review.full);
        }
        assert_eq!(tool.review.as_ref().unwrap().after.source(), before);
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            after,
            "review updates must not save the file"
        );
    }

    #[test]
    fn diff_undo_restores_applied_changes_in_reverse_order_without_closing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.rs");
        let before = "# generated\nvendordiff.patch\n";
        let after = "# generated\n\nvendordiff.patch\nmain.go\n";
        std::fs::write(&path, after).unwrap();
        let mut review = review_fixture(before, after);
        review.path = path.clone();
        let mut tool = GitTool {
            project: dir.path().to_path_buf(),
            review: Some(review),
            review_open: true,
            ..Default::default()
        };
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let accept = |tool: &mut GitTool, edit: RevertHunk| {
            tool.record_review_edit(&edit);
            tool.refresh_review_buffer(&path, edit.replacement, &ctx);
            let deadline = Instant::now() + Duration::from_secs(3);
            while tool.review_update_rx.is_some() {
                assert!(Instant::now() < deadline);
                tool.poll(dir.path(), &ctx);
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        for _ in 0..2 {
            let edit = tool.review.as_ref().unwrap().revert_hunk().unwrap();
            accept(&mut tool, edit);
        }
        assert_eq!(tool.review.as_ref().unwrap().after.source(), before);
        assert_eq!(tool.review_undo.len(), 2);
        let frame = |tool: &mut GitTool, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    tool.review_shortcuts(ctx);
                    tool.windows(ctx);
                },
            )
        };
        frame(&mut tool, vec![]);
        let output = frame(&mut tool, vec![]);
        let point = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Undo applied change" => {
                    Some(text.pos + text.galley.size() / 2.0)
                }
                _ => None,
            })
            .expect("Undo remains visible after applying the final change");
        for pressed in [true, false] {
            frame(
                &mut tool,
                vec![
                    egui::Event::PointerMoved(point),
                    egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        let edit = tool
            .pending_revert
            .take()
            .expect("Undo button queues an edit");
        assert!(edit.undo);
        assert_eq!(edit.replacement, "# generated\nvendordiff.patch\nmain.go\n");
        accept(&mut tool, edit);
        assert_eq!(tool.review_undo.len(), 1);
        frame(
            &mut tool,
            vec![egui::Event::Key {
                key: egui::Key::Z,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }],
        );
        let edit = tool
            .pending_revert
            .take()
            .expect("Cmd/Ctrl Z queues an undo");
        assert_eq!(edit.replacement, after);
        accept(&mut tool, edit);
        assert!(tool.review_open);
        assert!(tool.review_undo.is_empty());
        assert_eq!(tool.review.as_ref().unwrap().changes.len(), 2);
        assert_eq!(tool.review.as_ref().unwrap().after.source(), after);
        tool.queue_review_undo();
        assert!(tool.pending_revert.is_none());
        assert_eq!(std::fs::read_to_string(path).unwrap(), after);
    }

    #[test]
    fn diff_undo_stores_only_changed_text_and_preserves_unicode_and_newlines() {
        for (before, after) in [
            ("α\r\n猫\r\n", "α\r\n犬\r\n"),
            ("α", "α\n"),
            ("", "猫\n"),
            ("猫\n", ""),
        ] {
            let mut tool = GitTool {
                review: Some(review_fixture(before, after)),
                review_open: true,
                ..Default::default()
            };
            let edit = tool.review.as_ref().unwrap().revert_hunk().unwrap();
            tool.record_review_edit(&edit);
            tool.review = Some(review_fixture(before, &edit.replacement));
            tool.queue_review_undo();
            let undo = tool.pending_revert.take().unwrap();
            assert_eq!(undo.expected, before);
            assert_eq!(undo.replacement, after);
            // Failed application leaves the history intact; consume it only after success.
            assert_eq!(tool.review_undo.len(), 1);
            tool.record_review_edit(&undo);
            assert!(tool.review_undo.is_empty());
        }
        let context = "// unchanged context\n".repeat(1_000);
        let before = format!("{context}old\n");
        let after = format!("{context}new\n");
        let mut tool = GitTool {
            review: Some(review_fixture(&before, &after)),
            review_open: true,
            ..Default::default()
        };
        let edit = tool.review.as_ref().unwrap().revert_hunk().unwrap();
        tool.record_review_edit(&edit);
        let stored = tool.review_undo.back().unwrap();
        assert_eq!(stored.removed.len() + stored.inserted.len(), 6);
    }

    #[test]
    fn inline_arrows_apply_the_clicked_hunk_in_both_layouts() {
        for mut mode in [DiffMode::Unified, DiffMode::Split] {
            let file = tempfile::NamedTempFile::new().unwrap();
            let middle = (0..1).map(|i| format!("context {i}\n")).collect::<String>();
            let before = format!("old first\n{middle}old last\n");
            let after = format!("new first\n{middle}new last\n");
            let mut review = review_fixture(&before, &after);
            assert_eq!(diff::hunk_targets(&review.rows).len(), 1);
            assert_eq!(review.changes.len(), 2);
            review.path = file.path().to_path_buf();
            let ctx = egui::Context::default();
            chrome::install(&ctx);
            let initial_mode = mode;
            let mut frame = |events, review: &mut Review| {
                ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1200.0, 900.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| review.show(ui, &mut mode));
                    },
                )
            };
            frame(vec![], &mut review);
            let output = frame(vec![], &mut review);
            let _ = output;
            let pane = if initial_mode == DiffMode::Split {
                1
            } else {
                0
            };
            let targets = review.target_rows(initial_mode);
            let selection_id = egui::Id::new(("diff-selection", &review.path));
            let arrows: Vec<_> = targets
                .iter()
                .map(|row| {
                    ctx.read_response(selection_id.with(("apply-diff-hunk", pane, *row)))
                        .expect("change arrow must be interactive")
                        .rect
                })
                .collect();
            assert_eq!(arrows.len(), 2);
            if initial_mode == DiffMode::Split {
                assert!((arrows[0].center().x - 600.0).abs() < 25.0);
                assert!(arrows.iter().all(|rect| rect.width() <= 28.0));
            }
            let point = arrows[1].center();
            for pressed in [true, false] {
                frame(
                    vec![
                        egui::Event::PointerMoved(point),
                        egui::Event::PointerButton {
                            pos: point,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                    &mut review,
                );
            }
            let hunk = ctx.data_mut(|data| {
                data.remove_temp::<usize>(egui::Id::new(("revert-hunk", &review.path)))
            });
            assert_eq!(hunk, Some(1));
            review.hunk = hunk.unwrap();
            assert_eq!(
                review.revert_hunk().unwrap().replacement,
                format!("new first\n{middle}old last\n")
            );
        }
    }

    #[test]
    fn diff_drag_selection_copies_across_lines_in_both_layouts() {
        for mut mode in [DiffMode::Unified, DiffMode::Split] {
            let ctx = egui::Context::default();
            chrome::install(&ctx);
            let mut review = review_fixture("old\nsecond\n", "猫 first\nsecond\n");
            let mut time = 0.0;
            let mut frame = |events, review: &mut Review| {
                time += 0.1;
                ctx.run(
                    egui::RawInput {
                        time: Some(time),
                        events,
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1200.0, 500.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| review.show(ui, &mut mode));
                    },
                )
            };
            let mut output = frame(vec![], &mut review);
            for _ in 0..2 {
                output = frame(vec![], &mut review);
            }
            let locate = |text: &str| {
                output
                    .shapes
                    .iter()
                    .filter_map(|shape| {
                        if let egui::Shape::Text(text_shape) = &shape.shape {
                            if text_shape.galley.text() == text {
                                return Some((text_shape.pos, text_shape.galley.size()));
                            }
                        }
                        None
                    })
                    .last()
                    .unwrap()
            };
            let (first, size) = locate("猫 first");
            let (last, last_size) = locate("second");
            let start = first + egui::vec2(0.1, size.y / 2.0);
            let end = last + egui::vec2(last_size.x + 1.0, last_size.y / 2.0);
            frame(
                vec![
                    egui::Event::PointerMoved(start),
                    egui::Event::PointerButton {
                        pos: start,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                &mut review,
            );
            frame(vec![egui::Event::PointerMoved(end)], &mut review);
            frame(
                vec![egui::Event::PointerButton {
                    pos: end,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                &mut review,
            );
            let copied = frame(vec![egui::Event::Copy], &mut review);
            assert!(copied.platform_output.commands.iter().any(|command|
                matches!(command, egui::OutputCommand::CopyText(text) if text == "猫 first\nsecond")),
                "selection must copy source without gutters or diff markers: {:?}", copied.platform_output.commands);
            let selection = ctx
                .data_mut(|data| {
                    data.get_temp::<DiffSelection>(egui::Id::new(("diff-selection", &review.path)))
                })
                .unwrap();
            assert!(
                !selection.dragging,
                "release must end the drag without clearing selection"
            );
            frame(
                vec![egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::COMMAND,
                }],
                &mut review,
            );
            let copied = frame(vec![egui::Event::Copy], &mut review);
            assert!(copied.platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text.ends_with("猫 first\nsecond"))));
        }
    }

    #[test]
    fn both_trees_render_matching_status_colors_and_unsaved_overrides() {
        let root = Path::new("/project");
        let repo = Repository {
            root: root.into(),
            branch: "main".into(),
            head: String::new(),
            base: "main".into(),
            merge_base: None,
            branches: vec![],
            remote: None,
            files: vec![
                git::ChangedFile {
                    path: "committed.ts".into(),
                    committed: true,
                    ..Default::default()
                },
                git::ChangedFile {
                    path: "working.ts".into(),
                    uncommitted: true,
                    ..Default::default()
                },
                git::ChangedFile {
                    path: "deleted.ts".into(),
                    committed: true,
                    deleted: true,
                    ..Default::default()
                },
            ],
        };
        let paths: Vec<_> = ["committed.ts", "working.ts", "deleted.ts", "clean.ts"]
            .iter()
            .map(|name| root.join(name))
            .collect();
        let mut project = crate::explorer::FileTree::build(root, &paths);
        let ctx = egui::Context::default();
        for unsaved in [true, false] {
            let dirty = if unsaved {
                vec![root.join("committed.ts")]
            } else {
                vec![]
            };
            let files = tree_files(&repo, &dirty);
            let colors = files
                .iter()
                .map(|file| (root.join(&file.path), file_color(file)))
                .collect();
            let changes = ChangeTree::build(&files, "", false);
            let output = ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.columns(2, |columns| {
                        project.show(
                            &mut columns[0],
                            root,
                            Some(&root.join("committed.ts")),
                            "",
                            &colors,
                        );
                        changes.show(&mut columns[1], &files, Path::new(""), root);
                    });
                });
            });
            let rendered = |label: &str| {
                output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.text() == label => {
                            Some(text.galley.job.sections[0].format.color)
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("missing label: {label}"))
            };
            let green = Color32::from_rgb(82, 168, 104);
            let expected = if unsaved { working() } else { green };
            assert_eq!(
                rendered("committed.ts"),
                expected,
                "selection keeps the Git status color"
            );
            assert_eq!(
                rendered(if unsaved {
                    "C+W  committed.ts"
                } else {
                    "C  committed.ts"
                }),
                expected
            );
            assert_eq!(rendered("working.ts"), working());
            assert_eq!(rendered("W  working.ts"), working());
            assert_eq!(rendered("deleted.ts"), deleted());
            assert_eq!(rendered("D  deleted.ts"), deleted());
            assert_eq!(rendered("clean.ts"), chrome::TEXT);
        }
        assert_eq!(
            marker(git::Mark::Modified),
            Color32::from_rgb(100, 165, 245)
        );
    }

    #[test]
    fn editor_git_work_waits_for_idle_even_without_a_previous_file() {
        let ctx = egui::Context::default();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("large.ts");
        let mut tool = GitTool::default();
        tool.repo = Some(git::Repository {
            root: root.path().to_owned(),
            branch: "main".into(),
            head: String::new(),
            base: "main".into(),
            merge_base: None,
            branches: vec![],
            files: vec![],
            remote: None,
        });
        assert!(tool.ensure_file(&path, "first", &ctx).is_none());
        assert!(
            tool.file_rx.is_none(),
            "opening must not start Git work before the editor appears"
        );
        tool.desired.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(1);
        tool.ensure_file(&path, "edited", &ctx);
        assert!(tool.file_rx.is_none(), "typing resets the idle delay");
        tool.desired.as_mut().unwrap().1 = Instant::now() - Duration::from_secs(1);
        tool.ensure_file(&path, "edited", &ctx);
        assert!(
            tool.file_rx.is_some(),
            "Git work starts once the buffer settles"
        );
    }

    #[test]
    fn comparison_selector_fits_narrow_panels_and_branch_selection_updates_base() {
        for width in [160.0_f32, 240.0, 360.0] {
            let ctx = egui::Context::default();
            chrome::install(&ctx);
            let mut tool = GitTool {
                repo: Some(Repository {
                    root: "/project".into(),
                    branch: "feature/a-very-long-current-branch-name".into(),
                    head: String::new(),
                    base: "origin/a-very-long-base-branch-name".into(),
                    merge_base: None,
                    branches: vec!["main".into(), "origin/a-very-long-base-branch-name".into()],
                    remote: None,
                    files: vec![],
                }),
                last_refresh: Some(Instant::now()),
                ..Default::default()
            };
            let frame = |tool: &mut GitTool, events| {
                ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 700.0),
                        )),
                        events,
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            tool.content(ui, Path::new("/project"), &[], false);
                        });
                    },
                )
            };
            let click = |tool: &mut GitTool, point| {
                frame(
                    tool,
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
                    tool,
                    vec![egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                )
            };
            frame(&mut tool, vec![]);
            let collapsed = frame(&mut tool, vec![]);
            assert!(!collapsed.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text().contains("origin/a-very-long-base"))));
            let header = collapsed
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == "COMPARE CHANGES" => {
                        Some(text.pos + text.galley.size() / 2.0)
                    }
                    _ => None,
                })
                .unwrap();
            click(&mut tool, header);
            // Finish the disclosure animation before checking the dropdown.
            ctx.style_mut(|style| style.animation_time = 0.0);
            let expanded = frame(&mut tool, vec![]);
            let point = expanded
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text)
                        if text.galley.text().contains("origin/a-very-long-base") =>
                    {
                        assert!(
                            text.pos.x + text.galley.size().x <= width,
                            "base label overflows at {width}"
                        );
                        Some(text.pos + text.galley.size() / 2.0)
                    }
                    _ => None,
                })
                .expect("expanding comparison must show the branch selector");
            click(&mut tool, point);
            let output = frame(&mut tool, vec![]);
            let branch = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == "main" => {
                        Some(text.pos + text.galley.size() / 2.0)
                    }
                    _ => None,
                })
                .expect("branch menu must open");
            click(&mut tool, branch);
            assert_eq!(tool.selected_base.as_deref(), Some("main"));
        }
    }

    #[test]
    fn hide_tests_filters_tree_without_changing_file_indices() {
        use super::*;
        let files: Vec<_> = [
            "src/main.go",
            "src/main_test.go",
            "tests/input.json",
            "src/Contest.java",
        ]
        .into_iter()
        .map(|path| git::ChangedFile {
            path: path.into(),
            ..Default::default()
        })
        .collect();
        assert_eq!(ChangeTree::build(&files, "", false).file_count(), 4);
        let tree = ChangeTree::build(&files, "", true);
        assert_eq!(tree.file_count(), 2);
        assert!(!tree.children.contains_key("tests"));
        let src = &tree.children["src"];
        assert_eq!(src.children["main.go"].file, Some(0));
        assert_eq!(src.children["Contest.java"].file, Some(3));
        assert_eq!(ChangeTree::build(&files, "MAIN", true).file_count(), 1);
        assert_eq!(ChangeTree::build(&files, "test", true).file_count(), 1); // Contest.java
        assert_eq!(ChangeTree::build(&files, "tests/", true).file_count(), 0);
        assert_eq!(ChangeTree::build(&files, "", false).file_count(), 4);
    }

    #[test]
    fn hide_tests_checkbox_filters_unsaved_files_and_can_restore_them() {
        use super::*;
        let root = PathBuf::from("/example");
        let mut tool = GitTool {
            visible: true,
            last_refresh: Some(Instant::now()),
            repo: Some(Repository {
                root: root.clone(),
                branch: "feature".into(),
                head: "abc".into(),
                base: "main".into(),
                merge_base: None,
                branches: vec!["main".into()],
                remote: None,
                files: vec![git::ChangedFile {
                    path: "main.go".into(),
                    ..Default::default()
                }],
            }),
            ..Default::default()
        };
        let dirty = [root.join("main_test.go")];
        let ctx = egui::Context::default();
        let mut time = 0.0;
        let mut frame = |tool: &mut GitTool, events| {
            time += 0.1;
            ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 700.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    tool.panel(ctx, &root, &dirty);
                },
            )
        };
        frame(&mut tool, vec![]);
        let output = frame(&mut tool, vec![]);
        let text = |output: &egui::FullOutput| -> Vec<String> {
            output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(t) => Some(t.galley.text().to_owned()),
                    _ => None,
                })
                .collect()
        };
        assert!(!tool.include_tests);
        assert!(!text(&output).iter().any(|s| s.contains("main_test.go")));
        let point = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(t) if t.galley.text() == "Hide test files" => {
                    Some(t.pos + t.galley.size() / 2.0)
                }
                _ => None,
            })
            .unwrap();
        for hidden in [false, true] {
            frame(
                &mut tool,
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
            let output = frame(
                &mut tool,
                vec![egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            assert_eq!(!tool.include_tests, hidden);
            let labels = text(&output);
            assert_eq!(labels.iter().any(|s| s.contains("main_test.go")), !hidden);
            assert!(labels.iter().any(|s| s.contains("main.go")));
            if hidden {
                assert!(
                    labels
                        .iter()
                        .any(|s| s == "1 of 2 changed files · 1 test file hidden")
                );
            }
        }
        assert_eq!(tool.repo.unwrap().files.len(), 1);
    }

    #[test]
    fn changed_file_click_opens_editor_and_context_menu_requests_diff() {
        use super::*;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        let ctx = egui::Context::default();
        let files = vec![git::ChangedFile {
            path: "main.rs".into(),
            uncommitted: true,
            ..Default::default()
        }];
        let tree = ChangeTree::build(&files, "", false);
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
                        action = tree.show(ui, &files, Path::new(""), root);
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
        let row_point = point;
        let (mut output, _) = frame(vec![]);
        for label in [
            "Copy file name",
            "Copy absolute path",
            "Copy path from content root",
            "Rename…",
            "Delete file…",
        ] {
            assert!(
                output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.text() == label)),
                "missing {label}"
            );
        }
        let copy = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Copy absolute path" => {
                    Some(text.pos + text.galley.size() * 0.5)
                }
                _ => None,
            })
            .unwrap();
        frame(event(copy, egui::PointerButton::Primary, true));
        let copied = frame(event(copy, egui::PointerButton::Primary, false)).0;
        assert!(copied.platform_output.commands.iter().any(|command| matches!(command,
            egui::OutputCommand::CopyText(text) if text == &root.join("main.rs").to_string_lossy())));
        frame(vec![]);
        frame(vec![egui::Event::PointerMoved(row_point)]);
        frame(event(row_point, egui::PointerButton::Secondary, true));
        frame(event(row_point, egui::PointerButton::Secondary, false));
        (output, _) = frame(vec![]);
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
        for mut mode in [DiffMode::Unified, DiffMode::Split] {
            let ctx = egui::Context::default();
            chrome::install(&ctx);
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
    fn single_change_can_be_refocused_from_away_in_both_layouts() {
        let path = PathBuf::from("code.rs");
        let before = (0..80)
            .map(|i| format!("const ITEM_{i}: usize = {i};\n"))
            .collect::<String>();
        let after = before.replace("ITEM_35: usize = 35", "ITEM_35: usize = 3500");
        let rows = diff::compare(&before, &after);
        let visible = review_rows(&rows, false);
        let split = split_rows(&rows, &visible);
        let changes = change_targets(&rows);
        assert_eq!(diff::hunk_targets(&rows).len(), 1);
        assert_eq!(changes.len(), 1);
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
