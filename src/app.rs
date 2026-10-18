use crate::{
    chrome,
    document::{Content, Document, MarkdownMode},
    navigation::{self, Location, SearchKind, SearchUpdate},
};
use eframe::egui::{self, Align, Color32, FontId, Key, RichText};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

enum Pending {
    Close(usize),
    Reload(usize),
    Exit,
    SwitchProject(PathBuf),
}

enum FrameAction {
    Open(PathBuf, Option<(usize, usize)>),
    Save(usize),
    Close(usize),
    Activate(usize),
    Reload(usize),
    GitDiff(usize),
    SwitchProject(PathBuf),
}

#[derive(Clone, Copy, PartialEq)]
enum Picker {
    Everywhere,
    Files,
    Symbols,
    Recent,
    Line,
}

pub struct Coder {
    root: PathBuf,
    files: Vec<PathBuf>,
    tree: crate::explorer::FileTree,
    selected_category: Option<navigation::MatchCategory>,
    extension_filter: Option<String>,
    result_scope: crate::search_view::Scope,
    file_filter: String,
    files_rx: Option<Receiver<anyhow::Result<Vec<PathBuf>>>>,
    documents: Vec<Document>,
    active: usize,
    query: String,
    search_kind: SearchKind,
    results: Vec<Location>,
    search_rx: Option<Receiver<SearchUpdate>>,
    search_cancel: Arc<AtomicBool>,
    open_matches: usize,
    results_revision: u64,
    search_view: crate::search_view::SearchView,
    search_note: String,
    cache: CommonMarkCache,
    status: String,
    error: Option<String>,
    pending: Option<Pending>,
    allow_exit: bool,
    csv_headers: bool,
    history: Vec<Location>,
    picker: Option<Picker>,
    picker_query: String,
    picker_selection: usize,
    recent_file_searches: Vec<PathBuf>,
    picker_focus: bool,
    last_shift: Option<Instant>,
    shift_down: bool,
    find_open: bool,
    find: String,
    replacement: String,
    find_focus: bool,
    file_find: crate::file_find::FileFind,
    replace_open: bool,
    defer_actions: bool,
    actions: Vec<FrameAction>,
    show_project: bool,
    show_structure: bool,
    git: crate::git_view::GitTool,
    terminal: crate::terminal::Terminal,
    structure_filter: String,
    show_file_filter: bool,
    search_open: bool,
    search_docked: bool,
    result_selection: usize,
    search_focus: bool,
    preview: crate::preview::PreviewService,
    file_action: Option<crate::explorer::FileAction>,
    file_action_name: String,
    file_action_focus: bool,
}

impl Coder {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        chrome::install(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        cc.egui_ctx
            .add_image_loader(Arc::new(crate::markdown_images::BadgeLoader::default()));
        let argument = std::env::args_os().nth(1).map(PathBuf::from);
        let root = argument
            .as_ref()
            .filter(|p| p.is_dir())
            .cloned()
            .or_else(|| {
                argument
                    .as_ref()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
            })
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let mut app = Self::empty(root.canonicalize().unwrap_or(root));
        app.terminal.load_settings();
        app.terminal.settings.apply(&cc.egui_ctx);
        app.refresh_files();
        if let Some(file) = argument.filter(|p| p.is_file()) {
            app.open(file, None);
        }
        #[cfg(feature = "ui-capture")]
        if let Ok(mode) = std::env::var("CODER_UI_PREVIEW") {
            app.files = navigation::files(&app.root).unwrap_or_default();
            app.tree = crate::explorer::FileTree::build(&app.root, &app.files);
            app.files_rx = None;
            if mode.starts_with("csv-") {
                app.show_project = false;
                if mode == "csv-source"
                    && let Some(doc) = app.documents.get_mut(app.active)
                {
                    let _ = doc.set_csv_source(true);
                }
                return app;
            }
            if mode == "blame" {
                if app.documents.is_empty()
                    && let Some(path) = app
                        .files
                        .iter()
                        .find(|path| path.extension().is_some_and(|ext| ext == "go"))
                        .or(app.files.first())
                        .cloned()
                {
                    app.open(path, None);
                }
                app.git.refresh(&app.root, &cc.egui_ctx);
                app.git.repo = crate::git::repository(&app.root, None).ok();
                app.git.show_blame = true;
                if let Some(doc) = app.documents.get_mut(app.active) {
                    doc.jump = Some((28, 1));
                }
                return app;
            }
            if mode == "git" || mode == "git-diff" {
                app.git.refresh(&app.root, &cc.egui_ctx);
                app.git.repo = crate::git::repository(&app.root, None).ok();
                app.git.visible = true;
                app.git.show_blame = true;
                if mode == "git-diff"
                    && let Some(doc) = app.documents.get(app.active)
                {
                    app.git.open_review(doc.path.clone(), None, &cc.egui_ctx);
                }
                return app;
            }
            if mode == "terminal" || mode == "terminal-settings" {
                if app.documents.is_empty() && app.root.join("src/terminal.rs").is_file() {
                    app.open(app.root.join("src/terminal.rs"), None);
                }
                app.terminal.new_tab(&app.root, &cc.egui_ctx);
                if mode == "terminal" {
                    app.terminal.new_tab(&app.root, &cc.egui_ctx);
                    app.terminal.new_tab(&app.root, &cc.egui_ctx);
                }
                if mode == "terminal-settings" {
                    app.terminal.settings.editor_font_size = 18.0;
                    app.terminal.settings.terminal_font_size = 16.0;
                    app.terminal.settings.apply(&cc.egui_ctx);
                    app.terminal.open_settings();
                }
                return app;
            }
            if mode.starts_with("dialog-") {
                app.open(app.root.join("src/app.rs"), None);
                match mode.as_str() {
                    "dialog-new" => {
                        app.file_action =
                            Some(crate::explorer::FileAction::NewFile(app.root.join("src")));
                        app.file_action_name = "new_component.rs".into();
                        app.file_action_focus = true;
                    }
                    "dialog-rename" => {
                        app.file_action = Some(crate::explorer::FileAction::Rename(
                            app.root.join("src/app.rs"),
                        ));
                        app.file_action_name = "workspace.rs".into();
                        app.file_action_focus = true;
                    }
                    "dialog-delete" => {
                        app.file_action = Some(crate::explorer::FileAction::Delete(
                            app.root.join("src/app.rs"),
                        ));
                    }
                    "dialog-error" => {
                        app.error = Some("The file changed on disk since it was opened. Reload the saved version before trying again.".into());
                    }
                    "dialog-reload" => {
                        app.pending = Some(Pending::Reload(app.active));
                    }
                    _ => {
                        if let Some(doc) = app.documents.get_mut(app.active) {
                            doc.dirty = true;
                        }
                        app.pending = Some(Pending::Close(app.active));
                    }
                }
                return app;
            }
            if mode == "empty" {
                return app;
            }
            if mode == "folding" {
                return app;
            }
            if mode == "source" {
                if let Some(doc) = app.documents.get_mut(app.active)
                    && let Content::Text(text) = &doc.content
                    && let Some(index) = text.find('{')
                {
                    let prefix = &text[..index];
                    doc.jump = Some((
                        prefix.bytes().filter(|b| *b == b'\n').count() + 1,
                        prefix.rsplit('\n').next().unwrap_or("").len() + 1,
                    ));
                }
                return app;
            }
            if mode == "markdown" {
                if let Some(doc) = app.documents.get_mut(app.active) {
                    doc.markdown = MarkdownMode::Preview;
                }
                app.show_project = false;
                return app;
            }
            for path in [
                "Cargo.toml",
                "examples/welcome.md",
                "examples/inventory.csv",
                "src/navigation.rs",
            ] {
                app.open(app.root.join(path), None);
            }
            if mode == "find" {
                app.find_open = true;
                app.find_focus = true;
                app.find = "source".into();
            }
            if mode == "csv" {
                app.open(app.root.join("examples/inventory.csv"), None);
            }
            if mode == "json" {
                app.open(app.root.join("examples/manifest.json"), Some((7, 7)));
            }
            if mode == "structure" || mode == "borders" || mode == "compact" {
                app.show_structure = true;
                if mode == "borders" || mode == "compact" {
                    app.open(app.root.join("src/app.rs"), None);
                    app.terminal.new_tab(&app.root, &cc.egui_ctx);
                    if mode == "compact" {
                        cc.egui_ctx
                            .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                                1000.0, 700.0,
                            )));
                    }
                }
            }
            if mode == "search" {
                app.query = "Symbol".into();
                if let Ok(results) =
                    navigation::search(&app.root, &app.query, SearchKind::References)
                {
                    app.search_note = format!(
                        "{} matches in project{}",
                        results.locations.len(),
                        if results.truncated { " (limited)" } else { "" }
                    );
                    app.results = results.locations;
                    app.search_open = true;
                }
            } else if mode == "files" {
                app.show_picker(Picker::Files);
                app.picker_query = "nav".into();
            }
        }
        app
    }

    fn empty(root: PathBuf) -> Self {
        Self {
            root,
            files: vec![],
            tree: Default::default(),
            selected_category: None,
            extension_filter: None,
            result_scope: Default::default(),
            file_filter: String::new(),
            files_rx: None,
            documents: vec![],
            active: 0,
            query: String::new(),
            search_kind: SearchKind::Text,
            results: vec![],
            search_rx: None,
            search_cancel: Arc::new(AtomicBool::new(false)),
            open_matches: 0,
            results_revision: 0,
            search_view: Default::default(),
            search_note: String::new(),
            cache: CommonMarkCache::default(),
            status: "Ready".into(),
            error: None,
            pending: None,
            allow_exit: false,
            csv_headers: true,
            history: vec![],
            picker: None,
            picker_query: String::new(),
            picker_selection: 0,
            recent_file_searches: vec![],
            picker_focus: false,
            last_shift: None,
            shift_down: false,
            find_open: false,
            find: String::new(),
            replacement: String::new(),
            find_focus: false,
            file_find: Default::default(),
            replace_open: false,
            defer_actions: false,
            actions: vec![],
            show_project: true,
            show_structure: false,
            git: Default::default(),
            terminal: Default::default(),
            structure_filter: String::new(),
            show_file_filter: false,
            search_open: false,
            search_docked: false,
            result_selection: 0,
            search_focus: false,
            preview: Default::default(),
            file_action: None,
            file_action_name: String::new(),
            file_action_focus: false,
        }
    }

    fn request_project(&mut self, root: PathBuf) {
        self.commit_current_sheet();
        if self.documents.iter().any(|doc| doc.dirty) {
            self.pending = Some(Pending::SwitchProject(root));
        } else if self.defer_actions {
            self.actions.push(FrameAction::SwitchProject(root));
        } else {
            self.switch_project(root);
        }
    }

    fn switch_project(&mut self, root: PathBuf) {
        self.search_cancel.store(true, Ordering::Relaxed);
        self.search_rx = None;
        self.files_rx = None;
        self.preview.clear();
        self.root = root.canonicalize().unwrap_or(root);
        self.documents.clear();
        self.active = 0;
        self.files.clear();
        self.tree = Default::default();
        self.history.clear();
        self.recent_file_searches.clear();
        self.results.clear();
        self.results_revision += 1;
        self.search_view = Default::default();
        self.result_scope = Default::default();
        self.selected_category = None;
        self.extension_filter = None;
        self.result_selection = 0;
        self.open_matches = 0;
        self.query.clear();
        self.search_note.clear();
        self.search_open = false;
        self.file_filter.clear();
        self.structure_filter.clear();
        self.find_open = false;
        self.find.clear();
        self.file_find = Default::default();
        self.replacement.clear();
        self.picker = None;
        self.file_action = None;
        self.git = Default::default();
        self.status = "Project opened".into();
        self.refresh_files();
    }

    fn refresh_files(&mut self) {
        let root = self.root.clone();
        let (tx, rx) = mpsc::channel();
        self.files_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(navigation::files(&root));
        });
    }

    fn open(&mut self, path: PathBuf, target: Option<(usize, usize)>) {
        if self.defer_actions {
            self.queue_action(FrameAction::Open(path, target));
            return;
        }
        let canonical = path.canonicalize().unwrap_or(path);
        if let Some(index) = self.documents.iter().position(|d| d.path == canonical) {
            self.active = index;
        } else {
            match Document::open(canonical, self.csv_headers) {
                Ok(doc) => {
                    self.documents.push(doc);
                    self.active = self.documents.len() - 1;
                }
                Err(error) => {
                    self.error = Some(format!("{error:#}"));
                    return;
                }
            }
        }
        if let Some(target) = target {
            self.documents[self.active].jump = Some(target);
            if self.documents[self.active].is_markdown() {
                self.documents[self.active].markdown = MarkdownMode::Split;
            }
        }
    }

    fn current_location(&self) -> Option<Location> {
        let doc = self.documents.get(self.active)?;
        let Content::Text(text) = &doc.content else {
            return None;
        };
        let byte = text
            .char_indices()
            .nth(doc.cursor)
            .map_or(text.len(), |(b, _)| b);
        let prefix = &text[..byte];
        Some(Location {
            category: navigation::MatchCategory::Other,
            path: doc.path.clone(),
            line: prefix.bytes().filter(|&b| b == b'\n').count() + 1,
            column: prefix.rsplit('\n').next().unwrap_or("").len() + 1,
            preview: String::new(),
        })
    }

    fn navigate(&mut self, location: Location) {
        if let Some(current) = self.current_location() {
            self.history.push(current);
        }
        self.open(location.path, Some((location.line, location.column)));
    }

    fn save(&mut self, index: usize) -> bool {
        if self.defer_actions {
            self.queue_action(FrameAction::Save(index));
            return true;
        }
        match self.documents[index].save() {
            Ok(()) => {
                self.status = format!("Saved {}", self.documents[index].path.display());
                true
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                false
            }
        }
    }

    fn close(&mut self, index: usize) {
        if self.defer_actions {
            self.queue_action(FrameAction::Close(index));
            return;
        }
        self.documents.remove(index);
        if self.active > index {
            self.active -= 1;
        }
        self.active = self.active.min(self.documents.len().saturating_sub(1));
    }

    fn queue_action(&mut self, action: FrameAction) {
        self.commit_current_sheet();
        self.actions.push(action);
    }

    fn commit_current_sheet(&mut self) {
        if let Some(doc) = self.documents.get_mut(self.active)
            && let Content::Csv(sheet) = &mut doc.content
        {
            sheet.commit_edit();
        }
    }

    fn opened_search_paths(&self) -> Vec<PathBuf> {
        let mut seen = std::collections::HashSet::new();
        std::iter::once(self.active)
            .chain((0..self.documents.len()).filter(|index| *index != self.active))
            .filter_map(|index| self.documents.get(index).map(|doc| doc.path.clone()))
            .chain(
                self.git
                    .repo
                    .iter()
                    .flat_map(|repo| repo.files.iter().map(|file| repo.root.join(&file.path))),
            )
            .filter(|path| seen.insert(path.clone()))
            .collect()
    }

    fn update_search_scope(&mut self) {
        use crate::search_view::Scope;
        let paths = match self.result_scope {
            Scope::Project => None,
            Scope::Opened => Some(self.opened_search_paths()),
            Scope::Current => Some(
                self.documents
                    .get(self.active)
                    .map(|doc| doc.path.clone())
                    .into_iter()
                    .collect(),
            ),
        };
        self.search_view.set_paths(paths);
    }

    fn start_search(&mut self, kind: SearchKind, use_cursor: bool) {
        self.search_cancel.store(true, Ordering::Relaxed);
        self.search_cancel = Arc::new(AtomicBool::new(false));
        self.search_rx = None;
        if use_cursor
            && let Some(doc) = self.documents.get(self.active)
            && let Content::Text(text) = &doc.content
        {
            let word = navigation::word_at(text, doc.cursor);
            if !word.is_empty() {
                self.query = word;
            }
        }
        if self.query.trim().is_empty() {
            return;
        }
        self.search_kind = kind;
        self.search_open = true;
        self.picker = None;
        self.result_selection = 0;
        self.preview.clear();
        self.selected_category = None;
        self.extension_filter = None;
        self.results.clear();
        self.results_revision += 1;
        self.open_matches = 0;
        self.search_note = "Searching opened and changed files…".into();
        let root = self.root.clone();
        let query = self.query.clone();
        let buffers = std::iter::once(self.active)
            .chain((0..self.documents.len()).filter(|index| *index != self.active))
            .filter_map(|index| self.documents.get(index))
            .map(|doc| {
                let source = match &doc.content {
                    Content::Text(text) => text.clone(),
                    Content::Csv(sheet) => sheet.serialize().unwrap_or_default(),
                };
                (doc.path.clone(), source)
            })
            .collect();
        let mut priority_paths = self.opened_search_paths();
        let git_known = self.git.repo.is_some();
        let cancelled = self.search_cancel.clone();
        let (tx, rx) = mpsc::channel();
        self.search_rx = Some(rx);
        std::thread::spawn(move || {
            // A search can start before the first background Git refresh completes.
            if !git_known
                && !cancelled.load(Ordering::Relaxed)
                && let Ok(repo) = crate::git::repository(&root, None)
            {
                priority_paths.extend(repo.files.iter().map(|file| repo.root.join(&file.path)));
            }
            navigation::search_workspace(
                &root,
                &query,
                kind,
                buffers,
                priority_paths,
                cancelled,
                |update| {
                    let _ = tx.send(update);
                },
            );
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        self.git.poll(&self.root, ctx);
        if let Some(rx) = &self.files_rx
            && let Ok(result) = rx.try_recv()
        {
            match result {
                Ok(files) => {
                    self.tree = crate::explorer::FileTree::build(&self.root, &files);
                    self.files = files;
                }
                Err(error) => self.error = Some(format!("{error:#}")),
            }
            self.files_rx = None;
        }
        let updates: Vec<_> = self
            .search_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        let mut completed = false;
        for update in updates {
            match update {
                SearchUpdate::OpenFiles(locations) => {
                    self.results_revision += 1;
                    self.open_matches += locations.len();
                    self.results.extend(locations);
                    self.search_note = format!(
                        "{} opened/changed-file matches · searching project…",
                        self.open_matches
                    );
                }
                SearchUpdate::Project(locations) => {
                    self.results_revision += 1;
                    self.results.extend(locations);
                    self.search_note = format!(
                        "{} matches · {} from opened/changed files · searching…",
                        self.results.len(),
                        self.open_matches
                    );
                }
                SearchUpdate::Finished { truncated } => {
                    self.search_note = format!(
                        "{} matches{} · {} from opened/changed files",
                        self.results.len(),
                        if truncated { " (limited to 2,000)" } else { "" },
                        self.open_matches
                    );
                    completed = true;
                }
                SearchUpdate::Failed(error) => {
                    self.search_note = format!(
                        "{} opened/changed-file matches · project search failed",
                        self.open_matches
                    );
                    self.error = Some(error);
                    completed = true;
                }
            }
        }
        if completed {
            self.search_rx = None;
            if self.search_kind == SearchKind::Definitions && self.results.len() == 1 {
                self.navigate(self.results[0].clone());
                if !self.search_docked {
                    self.search_open = false;
                }
            }
        }
        self.preview.poll();
        for doc in &mut self.documents {
            if let Some(time) = doc.outline_refresh {
                if time.elapsed() >= Duration::from_millis(300) {
                    if let Content::Text(text) = &doc.content {
                        doc.outline = navigation::symbols(&doc.path, text);
                    }
                    doc.outline_refresh = None;
                } else {
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
            }
        }
        if self.files_rx.is_some() || self.search_rx.is_some() || self.preview.loading() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn open_search(&mut self) {
        self.picker = None;
        self.search_open = true;
        self.search_focus = true;
        self.commit_current_sheet();
    }

    fn toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar")
            .exact_height(42.0)
            .frame(
                egui::Frame::new()
                    .fill(chrome::PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 5)),
            )
            .show(ctx, |ui| {
                ui.visuals_mut().widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
                ui.visuals_mut().widgets.inactive.bg_stroke = egui::Stroke::NONE;
                ui.horizontal(|ui| {
                    let name = self
                        .root
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    let (mark, _) =
                        ui.allocate_exact_size(egui::vec2(25.0, 25.0), egui::Sense::hover());
                    ui.painter()
                        .rect_filled(mark, 7, Color32::from_rgb(105, 79, 161));
                    ui.painter().text(
                        mark.center(),
                        egui::Align2::CENTER_CENTER,
                        "C",
                        FontId::proportional(16.0),
                        Color32::WHITE,
                    );
                    ui.label(RichText::new(name).strong());
                    ui.add_space(12.0);
                    ui.menu_button("File", |ui| {
                        if ui.button("Open folder…").clicked()
                            && let Some(root) = rfd::FileDialog::new().pick_folder()
                        {
                            self.request_project(root);
                            ui.close();
                        }
                        if ui.button("Open file…").clicked()
                            && let Some(path) = rfd::FileDialog::new().pick_file()
                        {
                            self.open(path, None);
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                !self.documents.is_empty(),
                                egui::Button::new("Save    ⌘/Ctrl S"),
                            )
                            .clicked()
                        {
                            self.save(self.active);
                            ui.close();
                        }
                        if ui.button("Refresh project").clicked() {
                            self.refresh_files();
                            ui.close();
                        }
                        ui.separator();
                        ui.checkbox(&mut self.csv_headers, "CSV: first row is header");
                    });
                    ui.menu_button("Navigate", |ui| {
                        for (label, mode) in [
                            ("Go to File    Double Shift", Picker::Everywhere),
                            ("Go to File    ⌘⇧O / Ctrl Shift N", Picker::Files),
                            ("File Structure    ⌘/Ctrl F12", Picker::Symbols),
                            ("Recent Files    ⌘/Ctrl E", Picker::Recent),
                            ("Go to Line    ⌘L / Ctrl G", Picker::Line),
                        ] {
                            if ui.button(label).clicked() {
                                self.show_picker(mode);
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button("Go to Declaration    ⌘/Ctrl B").clicked() {
                            self.start_search(SearchKind::Definitions, true);
                            ui.close();
                        }
                        if ui.button("Find Usages    Alt F7").clicked() {
                            self.start_search(SearchKind::References, true);
                            ui.close();
                        }
                        if ui
                            .add_enabled(
                                !self.history.is_empty(),
                                egui::Button::new("Back    Alt Left"),
                            )
                            .clicked()
                            && let Some(location) = self.history.pop()
                        {
                            self.open(location.path, Some((location.line, location.column)));
                            ui.close();
                        }
                    });
                    ui.menu_button("View", |ui| {
                        ui.checkbox(&mut self.show_project, "Project tool window");
                        if ui
                            .checkbox(&mut self.show_structure, "Structure tool window")
                            .changed()
                            && self.show_structure
                        {
                            self.git.visible = false;
                        }
                        if ui
                            .checkbox(&mut self.git.visible, "Changes tool window")
                            .changed()
                            && self.git.visible
                        {
                            self.show_structure = false;
                        }
                        ui.separator();
                        if ui.button("Terminal    Alt F12 / ⌘/Ctrl `").clicked() {
                            self.terminal.toggle(&self.root, ctx);
                            ui.close();
                        }
                        ui.separator();
                        egui::widgets::global_theme_preference_buttons(ui);
                    });
                    ui.menu_button("Settings", |ui| {
                        if ui.button("Preferences…").clicked() {
                            self.terminal.open_settings();
                            ui.close();
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Go to File  ·  Double Shift")
                                        .color(chrome::TEXT),
                                )
                                .fill(chrome::EDITOR)
                                .corner_radius(6),
                            )
                            .clicked()
                        {
                            self.show_picker(Picker::Everywhere);
                        }
                        if chrome::icon_button(
                            ui,
                            chrome::Icon::Search,
                            "Find in Files (⌘/Ctrl Shift F)",
                        )
                        .clicked()
                        {
                            self.open_search();
                        }
                    });
                });
            });
    }

    fn tool_rail(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("tool-rail")
            .exact_width(42.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(chrome::PANEL).inner_margin(5))
            .show(ctx, |ui| {
                ui.add_space(7.0);
                if chrome::tool_button(ui, 0, self.show_project)
                    .on_hover_text("Project")
                    .clicked()
                {
                    self.show_project = !self.show_project;
                }
                ui.add_space(10.0);
                if chrome::tool_button(ui, 1, self.search_open)
                    .on_hover_text("Find in Files")
                    .clicked()
                {
                    self.open_search();
                }
                ui.add_space((ui.available_height() - 45.0).max(10.0));
                if chrome::tool_button(ui, 3, self.terminal.visible)
                    .on_hover_text("Terminal · Alt F12")
                    .clicked()
                {
                    self.terminal.toggle(&self.root, ctx);
                }
            });
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        let mut open = None;
        let mut force_root = false;
        egui::SidePanel::left("explorer")
            .default_width(285.0)
            .width_range(160.0..=500.0)
            .frame(
                egui::Frame::new()
                    .fill(chrome::PANEL)
                    .inner_margin(egui::Margin::symmetric(9, 8)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.spacing_mut().interact_size.y = 23.0;
                ui.horizontal(|ui| {
                    ui.menu_button(RichText::new("Project").strong(), |ui| {
                        ui.checkbox(&mut self.show_file_filter, "Filter project files");
                        if ui.button("Refresh project").clicked() {
                            self.refresh_files();
                            ui.close();
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if chrome::icon_button(ui, chrome::Icon::HideLeft, "Hide Project").clicked()
                        {
                            self.show_project = false;
                        }
                    });
                });
                ui.horizontal(|ui| {
                    for action in [0, 1, 2] {
                        if chrome::tree_button(ui, action).clicked() {
                            if action == 0 {
                                self.file_filter.clear();
                                self.tree.reveal_active();
                                force_root = true;
                            } else {
                                self.tree.expand_all(action == 1);
                                force_root = true;
                            }
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{} files", self.files.len()))
                                .size(11.0)
                                .color(chrome::MUTED),
                        );
                    });
                });
                ui.add_space(10.0);
                if self.show_file_filter {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.file_filter)
                            .hint_text("Filter files…")
                            .desired_width(f32::INFINITY),
                    );
                }
                egui::ScrollArea::both()
                    .id_salt("files")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let name = self
                            .root
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .to_string();
                        egui::CollapsingHeader::new(RichText::new(name).strong())
                            .id_salt(("project-root", &self.root))
                            .default_open(true)
                            .open(force_root.then_some(true))
                            .show(ui, |ui| {
                                open = self.tree.show(
                                    ui,
                                    &self.root,
                                    self.documents.get(self.active).map(|d| d.path.as_path()),
                                    if self.show_file_filter {
                                        &self.file_filter
                                    } else {
                                        ""
                                    },
                                );
                            })
                            .header_response
                            .context_menu(|ui| {
                                if let Some(action) =
                                    crate::explorer::context_menu(ui, &self.root, &self.root, true)
                                {
                                    self.tree.action = Some(action);
                                }
                            });
                    });
            });
        if let Some(action) = self.tree.action.take() {
            self.file_action_name = match &action {
                crate::explorer::FileAction::Rename(path) => path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                _ => String::new(),
            };
            self.file_action_focus = true;
            self.file_action = Some(action);
        }
        if let Some(path) = open {
            self.open(path, None);
        }
    }

    fn structure_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("right-tool-rail")
            .exact_width(32.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(chrome::PANEL).inner_margin(2))
            .show(ctx, |ui| {
                for (name, active) in [
                    ("Structure", self.show_structure),
                    ("Changes", self.git.visible),
                ] {
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(28.0, 105.0), egui::Sense::click());
                    if active || response.hovered() {
                        ui.painter().rect_filled(
                            rect,
                            4.0,
                            if active {
                                chrome::ACTIVE_TAB
                            } else {
                                chrome::HEADER
                            },
                        );
                        if active {
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(
                                    rect.left_top() + egui::vec2(0.0, 8.0),
                                    egui::vec2(2.0, rect.height() - 16.0),
                                ),
                                1,
                                chrome::BLUE,
                            );
                        }
                    }
                    let galley = ui.fonts_mut(|fonts| {
                        fonts.layout_no_wrap(name.into(), FontId::proportional(12.0), chrome::TEXT)
                    });
                    let position =
                        rect.center() + egui::vec2(galley.size().y / 2.0, -galley.size().x / 2.0);
                    ui.painter().add(
                        egui::epaint::TextShape::new(position, galley, chrome::TEXT)
                            .with_angle(std::f32::consts::FRAC_PI_2),
                    );
                    if response
                        .on_hover_text(format!("Show / hide {name}"))
                        .clicked()
                    {
                        if name == "Structure" {
                            self.show_structure = !self.show_structure;
                            self.git.visible = false;
                        } else {
                            self.git.visible = !self.git.visible;
                            self.show_structure = false;
                        }
                    }
                }
            });
        if self.git.visible {
            let dirty: Vec<_> = self
                .documents
                .iter()
                .filter(|doc| doc.dirty)
                .map(|doc| doc.path.clone())
                .collect();
            if let Some(action) = self.git.panel(ctx, &self.root, &dirty) {
                match action {
                    crate::git_view::ChangeAction::Open(path) => self.open(path, None),
                    crate::git_view::ChangeAction::Diff(path) => {
                        let buffer = self
                            .documents
                            .iter()
                            .find(|doc| doc.path == path && doc.dirty)
                            .and_then(|doc| match &doc.content {
                                Content::Text(text) => Some(text.clone()),
                                Content::Csv(sheet) => sheet.serialize().ok(),
                            });
                        self.git.open_review(path, buffer, ctx);
                    }
                }
            }
            return;
        }
        if !self.show_structure {
            return;
        }
        let mut target = None;
        egui::SidePanel::right("structure")
            .default_width(255.0)
            .width_range(180.0..=500.0)
            .frame(egui::Frame::new().fill(chrome::PANEL).inner_margin(10))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    chrome::heading_icon(ui, chrome::Icon::Structure, chrome::PURPLE);
                    ui.strong("Structure");
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if chrome::icon_button(ui, chrome::Icon::HideRight, "Hide Structure")
                            .clicked()
                        {
                            self.show_structure = false;
                        }
                    });
                });
                ui.add(
                    egui::TextEdit::singleline(&mut self.structure_filter)
                        .hint_text("Filter symbols…")
                        .desired_width(f32::INFINITY),
                );
                ui.separator();
                if let Some(doc) = self.documents.get(self.active) {
                    ui.horizontal(|ui| {
                        ui.label(chrome::file_icon(&doc.path));
                        ui.label(doc.path.file_name().unwrap_or_default().to_string_lossy());
                    });
                    if navigation::language(&doc.path).is_none() {
                        ui.weak(
                            "Structure is available for Java, Kotlin, Go, Rust, Python, JS and TS.",
                        );
                    }
                    let filter = self.structure_filter.to_lowercase();
                    let symbols: Vec<_> = doc
                        .outline
                        .iter()
                        .filter(|symbol| {
                            symbol.name.to_lowercase().contains(&filter)
                                || symbol.kind.to_lowercase().contains(&filter)
                        })
                        .collect();
                    let syntax = match &doc.content {
                        Content::Text(text) => Some(crate::highlighting::highlight(
                            ui,
                            text,
                            doc.path
                                .extension()
                                .and_then(|ext| ext.to_str())
                                .unwrap_or(""),
                        )),
                        _ => None,
                    };
                    let colors = syntax
                        .as_ref()
                        .map(|job| crate::highlighting::symbol_colors(job, &symbols))
                        .unwrap_or_default();
                    ui.weak(format!("{} symbols", symbols.len()));
                    egui::ScrollArea::both()
                        .id_salt("structure-symbols")
                        .show_rows(ui, 24.0, symbols.len(), |ui, rows| {
                            for index in rows {
                                let symbol = symbols[index];
                                let (rect, response) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), 24.0),
                                    egui::Sense::click(),
                                );
                                if response.hovered() {
                                    ui.painter().rect_filled(rect, 3.0, chrome::HEADER);
                                }
                                let name_rect = egui::Rect::from_min_max(
                                    rect.min,
                                    egui::pos2(
                                        (rect.right() - 42.0).max(rect.left()),
                                        rect.bottom(),
                                    ),
                                );
                                let color = colors.get(index).copied().unwrap_or(chrome::TEXT);
                                let badge = egui::Rect::from_center_size(
                                    rect.left_center() + egui::vec2(13.0, 0.0),
                                    egui::vec2(20.0, 18.0),
                                );
                                ui.painter()
                                    .rect_filled(badge, 4, color.gamma_multiply(0.13));
                                ui.painter().text(
                                    badge.center(),
                                    egui::Align2::CENTER_CENTER,
                                    crate::highlighting::symbol_badge(&symbol.kind),
                                    FontId::monospace(10.0),
                                    color,
                                );
                                ui.painter().with_clip_rect(name_rect).text(
                                    rect.left_center() + egui::vec2(30.0, 0.0),
                                    egui::Align2::LEFT_CENTER,
                                    &symbol.name,
                                    FontId::proportional(13.0),
                                    color,
                                );
                                ui.painter().text(
                                    rect.right_center() - egui::vec2(5.0, 0.0),
                                    egui::Align2::RIGHT_CENTER,
                                    symbol.line.to_string(),
                                    FontId::proportional(11.0),
                                    chrome::MUTED,
                                );
                                if response
                                    .on_hover_text(format!("{} · {}", symbol.name, symbol.kind))
                                    .clicked()
                                {
                                    target = Some(Location {
                                        path: doc.path.clone(),
                                        line: symbol.line,
                                        column: symbol.column,
                                        preview: String::new(),
                                        category: navigation::MatchCategory::Declaration,
                                    });
                                }
                            }
                        });
                } else {
                    ui.weak("Open a source file to see its structure.");
                }
            });
        if let Some(location) = target {
            self.navigate(location);
        }
    }

    fn results_panel(&mut self, ctx: &egui::Context) {
        if !self.search_open {
            return;
        }
        if self.search_docked {
            egui::TopBottomPanel::bottom("search-results")
                .resizable(true)
                .default_height(330.0)
                .frame(egui::Frame::new().fill(chrome::PANEL).inner_margin(12))
                .show(ctx, |ui| {
                    self.search_contents(ui, true);
                });
        } else {
            let size = ctx.content_rect().size();
            let width = (size.x - 100.0).clamp(480.0, 980.0);
            let mut open = true;
            egui::Window::new("Find in Files")
                .id(egui::Id::new("find-in-files"))
                .title_bar(false)
                .collapsible(false)
                .resizable(true)
                .open(&mut open)
                .default_pos(egui::pos2((size.x - width) / 2.0, 85.0))
                .default_width(width - 48.0)
                .min_width(480.0)
                .default_height((size.y * 0.78).clamp(350.0, 720.0))
                .min_height(350.0)
                .frame(chrome::popup_frame())
                .show(ctx, |ui| {
                    chrome::dialog_style(ui, ui.available_width());
                    self.search_contents(ui, false);
                });
            if !open {
                self.search_open = false;
            }
        }
    }

    fn search_contents(&mut self, ui: &mut egui::Ui, docked: bool) {
        use crate::search_view::Scope;
        self.update_search_scope();
        self.search_view.refresh(
            &self.results,
            self.results_revision,
            self.selected_category,
            self.extension_filter.as_deref(),
        );
        let minimum_height = (ui.available_height() - 32.0).max(250.0);
        if !docked {
            ui.set_min_height(minimum_height);
        }
        ui.horizontal(|ui| {
            chrome::heading_icon(ui, chrome::Icon::Search, chrome::BLUE);
            ui.label(RichText::new("Find in Files").size(18.0).strong());
            ui.label(RichText::new(&self.search_note).color(chrome::MUTED));
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if chrome::icon_button(
                    ui,
                    if docked {
                        chrome::Icon::HideDown
                    } else {
                        chrome::Icon::Close
                    },
                    if docked {
                        "Hide search results"
                    } else {
                        "Close search (Esc)"
                    },
                )
                .clicked()
                {
                    self.search_open = false;
                }
                if chrome::icon_button(
                    ui,
                    if docked {
                        chrome::Icon::External
                    } else {
                        chrome::Icon::Dock
                    },
                    if docked {
                        "Open search in a floating window"
                    } else {
                        "Dock search results below the editor"
                    },
                )
                .clicked()
                {
                    self.search_docked = !docked;
                }
            });
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Find").size(16.0).color(chrome::MUTED));
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .frame(false)
                    .font(FontId::proportional(20.0))
                    .hint_text("Search in project…")
                    .desired_width((ui.available_width() - 255.0).max(100.0)),
            );
            if self.search_focus {
                field.request_focus();
                self.search_focus = false;
            }
            let run = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let mut words = self.search_kind != SearchKind::Text;
            let words_changed = ui.checkbox(&mut words, "Whole words")
                .on_hover_text("Match complete words: generate matches generate() and build-generate, but excludes generated and generate_keys.").changed();
            if run || chrome::dialog_button(ui, "Search", Some(chrome::BLUE)).clicked() || words_changed {
                self.search_rx = None;
                self.start_search(
                    if words {
                        SearchKind::References
                    } else {
                        SearchKind::Text
                    },
                    false,
                );
            }
        });
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(" In Project ").background_color(chrome::HEADER));
            ui.label(
                RichText::new(self.root.file_name().unwrap_or_default().to_string_lossy())
                    .color(chrome::MUTED),
            );
            for (scope, label, tooltip) in [
                (
                    Scope::Opened,
                    "Opened",
                    "Editor tabs and files in the Git Changes tree",
                ),
                (Scope::Current, "Current", "Only the current editor file"),
            ] {
                if ui
                    .add(
                        egui::Button::new(label)
                            .selected(self.result_scope == scope)
                            .fill(if self.result_scope == scope {
                                chrome::SELECTED
                            } else {
                                chrome::HEADER
                            })
                            .stroke(egui::Stroke::NONE)
                            .corner_radius(5),
                    )
                    .on_hover_text(format!(
                        "{tooltip}. Click again to search all project files."
                    ))
                    .clicked()
                {
                    self.result_scope.toggle(scope);
                    self.result_selection = 0;
                    self.update_search_scope();
                }
            }
            let extensions = self.search_view.extensions.clone();
            egui::ComboBox::from_id_salt("result-extension")
                .selected_text(self.extension_filter.as_deref().unwrap_or("All file types"))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.extension_filter, None, "All file types");
                    for extension in extensions {
                        ui.selectable_value(
                            &mut self.extension_filter,
                            Some(extension.clone()),
                            extension,
                        );
                    }
                });
            if self.search_rx.is_some() {
                ui.spinner();
            }
        });
        self.search_view.refresh(
            &self.results,
            self.results_revision,
            self.selected_category,
            self.extension_filter.as_deref(),
        );
        ui.horizontal_wrapped(|ui| {
            category_toggle(
                ui,
                &mut self.selected_category,
                None,
                format!("All {}", self.search_view.total),
            );
            for category in navigation::MatchCategory::ALL {
                category_toggle(
                    ui,
                    &mut self.selected_category,
                    Some(category),
                    format!(
                        "{} {}",
                        category.label(),
                        self.search_view.counts[category as usize]
                    ),
                );
            }
        });
        self.search_view.refresh(
            &self.results,
            self.results_revision,
            self.selected_category,
            self.extension_filter.as_deref(),
        );
        let visible = self.search_view.indices.clone();
        self.result_selection = self.result_selection.min(visible.len().saturating_sub(1));
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowDown)) {
            self.result_selection =
                (self.result_selection + 1).min(visible.len().saturating_sub(1));
        }
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowUp)) {
            self.result_selection = self.result_selection.saturating_sub(1);
        }
        let mut navigate = None;
        let space = (ui.available_height() - if docked { 168.0 } else { 240.0 }).max(40.0);
        let list_height = space * 0.45;
        let preview_height = space * 0.55;
        egui::ScrollArea::vertical()
            .id_salt("search-matches")
            .auto_shrink([false, false])
            .min_scrolled_height(list_height)
            .max_height(list_height)
            .show_rows(ui, 30.0, visible.len(), |ui, range| {
                if visible.is_empty() {
                    ui.weak(if self.search_rx.is_some() {
                        "Searching…"
                    } else {
                        "No matching results"
                    });
                }
                for index in range {
                    let location = &self.results[visible[index]];
                    let selected = index == self.result_selection;
                    let (rect, response) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 30.0),
                        egui::Sense::click(),
                    );
                    if selected || response.hovered() {
                        ui.painter().rect_filled(
                            rect,
                            4.0,
                            if selected {
                                chrome::SELECTED
                            } else {
                                chrome::HEADER
                            },
                        );
                    }
                    let suffix = format!(
                        "{}{} :{}",
                        if visible[index] < self.open_matches {
                            "Open · "
                        } else {
                            ""
                        },
                        location
                            .path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                        location.line
                    );
                    let galley = ui.fonts_mut(|fonts| {
                        fonts.layout_no_wrap(
                            suffix,
                            FontId::proportional(13.0),
                            if selected {
                                chrome::TEXT
                            } else {
                                chrome::MUTED
                            },
                        )
                    });
                    let right_width = galley.size().x;
                    ui.painter().galley(
                        egui::pos2(
                            rect.right() - right_width - 10.0,
                            rect.center().y - galley.size().y / 2.0,
                        ),
                        galley,
                        chrome::TEXT,
                    );
                    let mut painter = ui.painter().clone();
                    painter.set_clip_rect(
                        ui.clip_rect()
                            .intersect(rect.shrink2(egui::vec2(8.0, 0.0)))
                            .intersect(egui::Rect::from_min_max(
                                rect.min,
                                egui::pos2(rect.right() - right_width - 24.0, rect.bottom()),
                            )),
                    );
                    let galley = ui.fonts_mut(|fonts| {
                        fonts.layout_job(chrome::highlighted(
                            location.preview.trim_start(),
                            &self.query,
                            14.0,
                        ))
                    });
                    painter.galley(
                        rect.left_center() + egui::vec2(10.0, -galley.size().y / 2.0),
                        galley,
                        chrome::TEXT,
                    );
                    if response.clicked() {
                        self.result_selection = index;
                    }
                    if response.double_clicked() {
                        navigate = Some(location.clone());
                    }
                }
            });
        if let Some(index) = visible.get(self.result_selection) {
            let location = self.results[*index].clone();
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(chrome::file_icon(&location.path));
                ui.strong(
                    location
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                );
                ui.label(
                    RichText::new(relative(
                        location.path.parent().unwrap_or(Path::new("")),
                        &self.root,
                    ))
                    .color(chrome::MUTED),
                );
            });
            self.search_preview(ui, &location, preview_height);
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter)) {
                navigate = Some(location);
            }
        } else {
            ui.separator();
            ui.horizontal(|ui| {
                ui.strong("Preview");
                ui.weak("Select a result to browse its file");
            });
            egui::Frame::new()
                .fill(chrome::EDITOR)
                .inner_margin(8)
                .show(ui, |ui| {
                    ui.allocate_space(egui::vec2(ui.available_width(), preview_height));
                });
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.weak(format!(
                "{} results  ·  Arrow keys to preview  ·  Enter to open",
                visible.len()
            ));
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if chrome::dialog_button(ui, "Open in Editor", Some(chrome::BLUE)).clicked()
                    && let Some(index) = visible.get(self.result_selection)
                {
                    navigate = Some(self.results[*index].clone());
                }
            });
        });
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            self.search_open = false;
        }
        if let Some(location) = navigate {
            self.navigate(location);
            if !docked {
                self.search_open = false;
            }
        }
    }

    fn search_preview(&mut self, ui: &mut egui::Ui, location: &Location, height: f32) {
        let dark = ui.visuals().dark_mode;
        if self.preview.needs(&location.path, dark) {
            let source = self
                .documents
                .iter()
                .find(|doc| doc.path == location.path)
                .and_then(|doc| {
                    if let Content::Text(text) = &doc.content {
                        Some(text.clone())
                    } else {
                        None
                    }
                });
            self.preview.request(location.path.clone(), source, dark);
        }
        let Some(preview) = self
            .preview
            .current
            .as_mut()
            .filter(|preview| preview.path == location.path && preview.dark == dark)
        else {
            egui::Frame::new()
                .fill(chrome::EDITOR)
                .inner_margin(8)
                .show(ui, |ui| {
                    ui.set_min_size(egui::vec2(ui.available_width(), height));
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("Loading preview…");
                    });
                });
            ui.ctx().request_repaint_after(Duration::from_millis(16));
            return;
        };
        let scale = crate::preferences::editor_font_size(ui.ctx()) / 13.0;
        let row_height = crate::preferences::line_height(ui.ctx());
        let jump = preview
            .select(location.line, location.column)
            .map(|offset| offset * scale);
        egui::Frame::new()
            .fill(chrome::EDITOR)
            .inner_margin(8)
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.spacing_mut().interact_size.y = row_height;
                let mut scroll = egui::ScrollArea::both()
                    .id_salt(("search-preview", &location.path))
                    .auto_shrink([false, false])
                    .min_scrolled_height(height)
                    .max_height(height);
                if let Some(offset) = jump {
                    scroll = scroll.vertical_scroll_offset(offset);
                }
                scroll.show_rows(ui, row_height, preview.len(), |ui, visible| {
                    ui.set_min_width(ui.available_width().max(preview.width * scale));
                    for index in visible {
                        if index + 1 == location.line {
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(
                                    ui.next_widget_position(),
                                    egui::vec2(ui.available_width(), row_height),
                                ),
                                0.0,
                                Color32::from_rgb(38, 40, 44),
                            );
                        }
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [42.0 * scale, row_height],
                                egui::Label::new(
                                    RichText::new((index + 1).to_string())
                                        .monospace()
                                        .color(chrome::MUTED),
                                ),
                            );
                            ui.add(
                                egui::Label::new(preview.line_for_ui(ui, index, &self.query))
                                    .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        });
                    }
                });
            });
    }

    fn refresh_file_find(&mut self) -> bool {
        let Some(doc) = self.documents.get(self.active) else {
            return self.file_find.refresh(Path::new(""), "", &self.find, 0);
        };
        let Content::Text(text) = &doc.content else {
            return self.file_find.refresh(&doc.path, "", &self.find, 0);
        };
        let caret = text
            .char_indices()
            .nth(doc.cursor)
            .map_or(text.len(), |(byte, _)| byte);
        self.file_find.refresh(&doc.path, text, &self.find, caret)
    }

    fn find_bar(&mut self, ui: &mut egui::Ui) -> bool {
        use egui::Modifiers as M;
        let id = ui.make_persistent_id("file-find");
        let focused = self.find_focus || ui.memory(|m| m.has_focus(id));
        let mut direction = None;
        let mut close = false;
        if focused {
            ui.input_mut(|i| {
                if i.consume_key(M::NONE, Key::ArrowUp) || i.consume_key(M::SHIFT, Key::Enter) {
                    direction = Some(true);
                }
                if i.consume_key(M::NONE, Key::ArrowDown) || i.consume_key(M::NONE, Key::Enter) {
                    direction = Some(false);
                }
                close = i.consume_key(M::NONE, Key::Escape);
            });
        }
        let mut scroll = self.refresh_file_find();
        egui::Frame::new()
            .fill(chrome::PANEL)
            .inner_margin(6)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.find)
                            .id(id)
                            .hint_text("Find in file")
                            .desired_width(240.0),
                    );
                    if self.find_focus {
                        field.request_focus();
                        self.find_focus = false;
                    }
                    if field.changed() {
                        scroll |= self.refresh_file_find();
                    }
                    if let Some(backwards) = direction {
                        self.file_find.step(backwards);
                        scroll = true;
                        field.request_focus();
                    }
                    ui.label(self.file_find.counter());
                    let enabled = !self.file_find.matches.is_empty();
                    for (label, backwards, hint) in [
                        ("↑", true, "Previous occurrence (Up / Shift+Enter)"),
                        ("↓", false, "Next occurrence (Down / Enter)"),
                    ] {
                        if ui
                            .add_enabled(enabled, egui::Button::new(label))
                            .on_hover_text(hint)
                            .clicked()
                        {
                            self.file_find.step(backwards);
                            scroll = true;
                            field.request_focus();
                        }
                    }
                    ui.toggle_value(&mut self.replace_open, "Replace");
                    close |= chrome::icon_button(ui, chrome::Icon::Close, "Close find bar (Esc)")
                        .clicked();
                });
                if self.replace_open {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.replacement)
                                .hint_text("Replace with")
                                .desired_width(240.0),
                        );
                        if ui
                            .add_enabled(
                                !self.file_find.matches.is_empty(),
                                egui::Button::new("Replace all"),
                            )
                            .clicked()
                        {
                            if let Some(doc) = self.documents.get_mut(self.active)
                                && let Content::Text(text) = &mut doc.content
                            {
                                *text = text.replace(&self.find, &self.replacement);
                                doc.dirty = *text != doc.original;
                                doc.outline_refresh = Some(Instant::now());
                            }
                            scroll |= self.refresh_file_find();
                        }
                    });
                }
            });
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                    tab: false,
                },
            )
        });
        if close {
            self.find_open = false;
            if let Some(id) = self
                .documents
                .get(self.active)
                .and_then(|doc| doc.editor_id)
            {
                ui.memory_mut(|m| m.request_focus(id));
            }
        }
        scroll
    }

    fn editor(&mut self, ctx: &egui::Context) {
        let mut clicked_token = None;
        let mut clicked_blame = None;
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(chrome::EDITOR).inner_margin(0))
            .show(ctx, |ui| {
                let mut close = None;
                let mut activate = None;
                egui::Frame::new()
                    .fill(chrome::TABS)
                    .inner_margin(egui::Margin {
                        left: 6,
                        right: 6,
                        top: 5,
                        bottom: 0,
                    })
                    .show(ui, |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("tabs")
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    for (index, doc) in self.documents.iter().enumerate() {
                                        let title = doc
                                            .path
                                            .file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy();
                                        let tab = chrome::workspace_tab(
                                            ui,
                                            ui.make_persistent_id(("editor-tab", &doc.path)),
                                            &title,
                                            chrome::TabKind::File(&doc.path),
                                            index == self.active,
                                            doc.dirty,
                                            &doc.path.display().to_string(),
                                        );
                                        if tab.activate {
                                            activate = Some(index);
                                        }
                                        if tab.close {
                                            close = Some(index);
                                        }
                                    }
                                });
                            });
                    });
                if let Some(index) = activate {
                    self.queue_action(FrameAction::Activate(index));
                }
                if let Some(index) = close {
                    if self.documents[index].dirty {
                        self.pending = Some(Pending::Close(index));
                        self.commit_current_sheet();
                    } else {
                        self.close(index);
                    }
                }
                ui.separator();
                let mut find_scroll = false;
                if self.find_open {
                    find_scroll = self.find_bar(ui);
                }
                let git_data = self
                    .documents
                    .get(self.active)
                    .and_then(|doc| match &doc.content {
                        Content::Text(text) => self.git.ensure_file(&doc.path, text, ctx),
                        Content::Csv(_) => None,
                    });
                let Some(doc) = self.documents.get_mut(self.active) else {
                    ui.vertical_centered(|ui| {
                        ui.add_space(100.0);
                        ui.label(
                            RichText::new("Your workspace is ready")
                                .size(25.0)
                                .strong()
                                .color(chrome::TEXT),
                        );
                        ui.add_space(8.0);
                        ui.label("Open a file from Project, or start with a different folder.");
                        ui.add_space(20.0);
                        ui.horizontal(|ui| {
                            let width = 260.0;
                            ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
                            if chrome::dialog_button(ui, "Open file…", Some(chrome::BLUE)).clicked()
                                && let Some(path) = rfd::FileDialog::new().pick_file()
                            {
                                self.open(path, None);
                            }
                            if chrome::dialog_button(ui, "Open folder…", None).clicked()
                                && let Some(root) = rfd::FileDialog::new().pick_folder()
                            {
                                self.request_project(root);
                            }
                        });
                        ui.add_space(24.0);
                        ui.label(
                            RichText::new(
                                "Double Shift  ·  Go to file     ⌘/Ctrl Shift F  ·  Find in files",
                            )
                            .size(12.0)
                            .color(chrome::MUTED),
                        );
                    });
                    return;
                };
                ui.ctx().request_repaint_after(Duration::from_millis(500));
                let can_reload = doc.can_reload();
                let file_actions = file_toolbar(
                    ui,
                    &doc.path,
                    &self.root,
                    FileToolbarState {
                        dirty: doc.dirty,
                        can_reload,
                        git: self.git.repo.is_some(),
                        blame: self.git.show_blame,
                        blame_note: git_data
                            .as_deref()
                            .and_then(|data| data.blame_note.as_deref())
                            .or(self.git.file_note.as_deref()),
                    },
                );
                if file_actions.reveal {
                    self.show_project = true;
                    self.tree.reveal_active();
                }
                if file_actions.blame {
                    self.git.show_blame = !self.git.show_blame;
                }
                if file_actions.diff {
                    self.actions.push(FrameAction::GitDiff(self.active));
                }
                if file_actions.reload {
                    if let Content::Csv(sheet) = &mut doc.content {
                        sheet.commit_edit();
                    }
                    self.actions.push(FrameAction::Reload(self.active));
                }
                let mut csv_mode = None;
                if doc.is_csv() {
                    let source = doc.csv_is_source();
                    egui::Frame::new().fill(chrome::TABS)
                        .inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.horizontal(|ui| {
                                chrome::heading_icon(ui, chrome::Icon::Table, chrome::BLUE);
                                ui.label(RichText::new("Delimited data").strong().color(chrome::TEXT));
                                ui.add_space(10.0);
                                egui::Frame::new().fill(chrome::EDITOR).corner_radius(7).inner_margin(3).show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 3.0;
                                        for (label, mode) in [("Data", false), ("Source", true)] {
                                            if ui.add(egui::Button::new(RichText::new(label)
                                                .color(if source == mode { chrome::TEXT } else { chrome::MUTED }))
                                                .fill(if source == mode { chrome::SELECTED } else { egui::Color32::TRANSPARENT })
                                                .stroke(egui::Stroke::NONE).corner_radius(5)
                                                .min_size(egui::vec2(70.0, 28.0))).clicked() && source != mode {
                                                csv_mode = Some(mode);
                                            }
                                        }
                                    });
                                });
                                if ui.available_width() > 240.0 {
                                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                        ui.weak(if source { "Plain text · edits sync to Data" } else { "Double-click a cell to edit" });
                                    });
                                }
                            });
                        });
                    if let Some(error) = &doc.csv_error {
                        egui::Frame::new().fill(chrome::WARNING.gamma_multiply(0.12))
                            .inner_margin(12).show(ui, |ui| {
                                ui.colored_label(chrome::WARNING, "Could not open Data view");
                                ui.add(egui::Label::new(error).wrap());
                                ui.weak("Correct the source and select Data again. Your edits are retained.");
                            });
                    }
                    if csv_mode == Some(true) && let Content::Csv(sheet) = &mut doc.content {
                        sheet.commit_edit();
                    }
                }
                let editor_query = if self.find_open { &self.find } else { "" };
                let markdown = doc.is_markdown();
                if markdown {
                    ui.horizontal(|ui| {
                        ui.selectable_value(&mut doc.markdown, MarkdownMode::Edit, "Source");
                        ui.selectable_value(&mut doc.markdown, MarkdownMode::Split, "Split");
                        ui.selectable_value(&mut doc.markdown, MarkdownMode::Preview, "Preview");
                    });
                }
                let path = doc.path.clone();
                ui.push_id(&path, |ui| match &mut doc.content {
                    Content::Csv(sheet) => {
                        doc.dirty |= sheet.ui(ui);
                    }
                    Content::Text(text) => {
                        let mut changed = false;
                        if markdown && doc.markdown == MarkdownMode::Preview {
                            markdown_preview(ui, &mut self.cache, text, &path);
                        } else if markdown && doc.markdown == MarkdownMode::Split {
                            ui.columns(2, |columns| {
                                (changed, clicked_token, clicked_blame) = text_editor(
                                    &mut columns[0],
                                    text,
                                    &path,
                                    &mut doc.cursor,
                                    &mut doc.jump,
                                    &mut doc.editor_id,
                                    EditorFind {
                                        query: editor_query,
                                        active: self.file_find.active().cloned(),
                                        scroll: find_scroll,
                                        git: git_data.as_deref(),
                                        blame: self.git.show_blame,
                                    },
                                );
                                markdown_preview(&mut columns[1], &mut self.cache, text, &path);
                            });
                        } else {
                            (changed, clicked_token, clicked_blame) = text_editor(
                                ui,
                                text,
                                &path,
                                &mut doc.cursor,
                                &mut doc.jump,
                                &mut doc.editor_id,
                                EditorFind {
                                    query: editor_query,
                                    active: self.file_find.active().cloned(),
                                    scroll: find_scroll,
                                    git: git_data.as_deref(),
                                    blame: self.git.show_blame,
                                },
                            );
                        }
                        if changed {
                            doc.dirty = *text != doc.original;
                            doc.outline_refresh = Some(Instant::now());
                        }
                    }
                });
                if let Some(source) = csv_mode {
                    if let Err(error) = doc.set_csv_source(source) {
                        doc.csv_error = Some(format!("{error:#}"));
                    }
                    ui.ctx().request_repaint();
                }
            });
        if let Some(blame) = clicked_blame {
            self.git.show_commit(blame, ctx);
        }
        if let Some(token) = clicked_token {
            // A new click supersedes an earlier search; its detached worker cannot replace these results.
            self.search_rx = None;
            self.query = token;
            self.start_search(SearchKind::References, false);
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(error) = self.error.clone() {
            egui::Modal::new(egui::Id::new("action-error"))
                .frame(chrome::popup_frame())
                .show(ctx, |ui| {
                    chrome::dialog_style(ui, 470.0);
                    let close = chrome::dialog_header(
                        ui,
                        "!",
                        "Could not complete action",
                        "Something needs your attention",
                        chrome::DANGER,
                    );
                    chrome::dialog_card(ui, |ui| {
                        ui.add(egui::Label::new(&error).wrap());
                    });
                    chrome::dialog_divider(ui);
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if chrome::dialog_button(ui, "Got it", Some(chrome::BLUE)).clicked()
                            || close
                            || ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape))
                        {
                            self.error = None;
                        }
                    });
                });
        }
        if let Some(action) = self.file_action.clone() {
            use crate::explorer::FileAction;
            let (title, path) = match &action {
                FileAction::NewFile(path) => ("New file", path),
                FileAction::Rename(path) => ("Rename", path),
                FileAction::Delete(path) => ("Delete file?", path),
            };
            let mut apply = false;
            let mut cancel = false;
            egui::Modal::new(egui::Id::new("file-action"))
                .frame(chrome::popup_frame()).show(ctx, |ui| {
                    chrome::dialog_style(ui, 460.0);
                    let deleting = matches!(action, FileAction::Delete(_));
                    let creating = matches!(action, FileAction::NewFile(_));
                    cancel = chrome::dialog_header(ui,
                        if deleting { "−" } else if creating { "+" } else { "Aa" },
                        title,
                        if deleting { "This action cannot be undone" } else if creating { "Add something new to your project" } else { "Give it a new name" },
                        if deleting { chrome::DANGER } else { chrome::BLUE });
                    chrome::dialog_path(ui, path);
                    ui.add_space(4.0);
                    if deleting {
                        ui.add(egui::Label::new("The file will be permanently deleted. Any unsaved edits in its open tab will also be lost.").wrap());
                    } else {
                        chrome::dialog_section(ui, if creating { "File name" } else { "New name" }, "Include the extension, for example main.rs");
                        let response = ui.add(egui::TextEdit::singleline(&mut self.file_action_name)
                            .desired_width(f32::INFINITY).margin(egui::vec2(12.0, 10.0)).hint_text("Enter a name…"));
                        if std::mem::take(&mut self.file_action_focus) { response.request_focus(); }
                        apply = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter));
                    }
                    cancel |= ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape));
                    chrome::dialog_divider(ui);
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        let label = if deleting { "Delete file" } else if creating { "Create file" } else { "Rename" };
                        apply |= ui.add_enabled_ui(deleting || !self.file_action_name.trim().is_empty(), |ui| {
                            chrome::dialog_button(ui, label, Some(if deleting { chrome::DANGER } else { chrome::BLUE }))
                        }).inner.clicked();
                        cancel |= chrome::dialog_button(ui, "Cancel", None).clicked();
                    });
                });
            if apply {
                match self.apply_file_action(action) {
                    Ok(()) => self.file_action = None,
                    Err(error) => {
                        self.file_action = None;
                        self.error = Some(error.to_string());
                    }
                }
            } else if cancel {
                self.file_action = None;
            }
        }
        if self.pending.is_some() {
            let mut action = 0;
            egui::Modal::new(egui::Id::new("unsaved"))
                .frame(chrome::popup_frame())
                .show(ctx, |ui| {
                    chrome::dialog_style(ui, 500.0);
                    let reload = matches!(self.pending, Some(Pending::Reload(_)));
                    if chrome::dialog_header(
                        ui,
                        if reload { "↻" } else { "●" },
                        if reload {
                            "Reload from disk?"
                        } else {
                            "Save your changes?"
                        },
                        if reload {
                            "Replace the current version with the saved file"
                        } else {
                            "You have work that hasn’t been saved yet"
                        },
                        chrome::WARNING,
                    ) {
                        action = 3;
                    }
                    match self.pending {
                        Some(Pending::Close(i) | Pending::Reload(i)) => {
                            chrome::dialog_path(ui, &self.documents[i].path)
                        }
                        _ => {
                            let dirty: Vec<_> =
                                self.documents.iter().filter(|doc| doc.dirty).collect();
                            chrome::dialog_card(ui, |ui| {
                                ui.label(
                                    RichText::new(format!(
                                        "{} unsaved {}",
                                        dirty.len(),
                                        if dirty.len() == 1 { "file" } else { "files" }
                                    ))
                                    .strong(),
                                );
                                for doc in dirty.iter().take(3) {
                                    ui.label(
                                        doc.path.file_name().unwrap_or_default().to_string_lossy(),
                                    );
                                }
                                if dirty.len() > 3 {
                                    ui.weak(format!("and {} more", dirty.len() - 3));
                                }
                            });
                        }
                    }
                    ui.label(if reload {
                        "Your unsaved edits will be discarded. The file on disk will stay as it is."
                    } else {
                        "Save your work before continuing, or discard the unsaved changes."
                    });
                    chrome::dialog_divider(ui);
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if reload {
                            if chrome::dialog_button(ui, "Reload file", Some(chrome::BLUE))
                                .clicked()
                            {
                                action = 2;
                            }
                        } else {
                            if chrome::dialog_button(ui, "Save and continue", Some(chrome::BLUE))
                                .clicked()
                            {
                                action = 1;
                            }
                            if chrome::dialog_button(ui, "Discard changes", None).clicked() {
                                action = 2;
                            }
                        }
                        if chrome::dialog_button(ui, "Cancel", None).clicked() {
                            action = 3;
                        }
                    });
                    if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                        action = 3;
                    }
                });
            if action == 3 {
                self.pending = None;
            } else if action != 0 {
                let saved = if action == 2 {
                    true
                } else {
                    match self.pending {
                        Some(Pending::Close(index) | Pending::Reload(index)) => self.save(index),
                        _ => {
                            let mut success = true;
                            for i in 0..self.documents.len() {
                                if self.documents[i].dirty && !self.save(i) {
                                    success = false;
                                    break;
                                }
                            }
                            success
                        }
                    }
                };
                if saved {
                    match self.pending.take() {
                        Some(Pending::Close(index)) => self.close(index),
                        Some(Pending::Reload(index)) => {
                            if let Err(error) = self.documents[index].reload(self.csv_headers) {
                                self.error = Some(error.to_string());
                            }
                        }
                        Some(Pending::SwitchProject(root)) => self.switch_project(root),
                        Some(Pending::Exit) => {
                            self.allow_exit = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        None => {}
                    }
                } else {
                    self.pending = None;
                }
            }
        }
    }

    fn apply_file_action(&mut self, action: crate::explorer::FileAction) -> anyhow::Result<()> {
        use crate::explorer::FileAction;
        let name = self.file_action_name.trim();
        if !matches!(action, FileAction::Delete(_)) {
            anyhow::ensure!(
                !name.is_empty()
                    && Path::new(name).components().count() == 1
                    && matches!(
                        Path::new(name).components().next(),
                        Some(std::path::Component::Normal(_))
                    ),
                "Enter a single file name without directory separators."
            );
        }
        match action {
            FileAction::NewFile(folder) => {
                let path = folder.join(name);
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                self.open(path, None);
            }
            FileAction::Rename(path) => {
                let target = path.with_file_name(name);
                if path != target {
                    anyhow::ensure!(
                        !target.exists() && !target.is_symlink(),
                        "A file or folder with that name already exists."
                    );
                    std::fs::rename(&path, &target)?;
                    for doc in &mut self.documents {
                        if let Ok(suffix) = doc.path.strip_prefix(&path) {
                            doc.path = if suffix.as_os_str().is_empty() {
                                target.clone()
                            } else {
                                target.join(suffix)
                            };
                            doc.outline = match &doc.content {
                                Content::Text(text) => navigation::symbols(&doc.path, text),
                                _ => vec![],
                            };
                        }
                    }
                    self.history.clear();
                    self.recent_file_searches.clear();
                }
            }
            FileAction::Delete(path) => {
                std::fs::remove_file(&path)?;
                self.documents.retain(|doc| doc.path != path);
                self.active = self.active.min(self.documents.len().saturating_sub(1));
                self.history.retain(|location| location.path != path);
                self.recent_file_searches.retain(|recent| recent != &path);
            }
        }
        self.refresh_files();
        Ok(())
    }

    fn show_picker(&mut self, mode: Picker) {
        self.search_open = false;
        self.picker = Some(mode);
        self.picker_query.clear();
        self.picker_selection = 0;
        self.picker_focus = true;
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        use egui::Modifiers as M;
        let key = |modifiers, key| ctx.input_mut(|input| input.consume_key(modifiers, key));
        let mac = cfg!(target_os = "macos");
        let modifiers = ctx.input(|i| i.modifiers);
        let shift_pressed = modifiers.shift
            && !self.shift_down
            && !modifiers.alt
            && !modifiers.command
            && !modifiers.ctrl;
        self.shift_down = modifiers.shift;
        if shift_pressed {
            if self
                .last_shift
                .is_some_and(|time| time.elapsed() < Duration::from_millis(400))
            {
                self.show_picker(Picker::Everywhere);
                self.last_shift = None;
            } else {
                self.last_shift = Some(Instant::now());
            }
        }
        if ctx.input(|i| {
            i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Text(_) | egui::Event::Key { pressed: true, .. }
                )
            })
        }) {
            self.last_shift = None;
        }
        if key(M::COMMAND | M::SHIFT, if mac { Key::O } else { Key::N }) {
            self.show_picker(Picker::Files);
        }
        if key(M::COMMAND, Key::E) {
            self.show_picker(Picker::Recent);
        }
        if key(M::COMMAND, Key::F12) {
            self.show_picker(Picker::Symbols);
        }
        if key(M::COMMAND, if mac { Key::L } else { Key::G }) {
            self.show_picker(Picker::Line);
        }
        if key(M::COMMAND, Key::B) {
            self.start_search(SearchKind::Definitions, true);
        }
        if key(M::ALT, Key::F7) {
            self.start_search(SearchKind::References, true);
        }
        if mac && key(M::ALT, Key::F12) {
            self.start_search(SearchKind::References, true);
        }
        if key(M::COMMAND | M::SHIFT, Key::F) {
            self.open_search();
        } else if key(M::COMMAND, Key::F) {
            self.find_open = true;
            self.find_focus = true;
        } else if key(M::COMMAND, Key::R) {
            self.find_open = true;
            self.replace_open = true;
            self.find_focus = true;
        }
        if key(M::CTRL, Key::Tab) && !self.documents.is_empty() {
            self.queue_action(FrameAction::Activate(
                (self.active + 1) % self.documents.len(),
            ));
        }
        if key(M::CTRL | M::SHIFT, Key::Tab) && !self.documents.is_empty() {
            self.queue_action(FrameAction::Activate(
                (self.active + self.documents.len() - 1) % self.documents.len(),
            ));
        }
        if key(M::COMMAND, Key::W) && !self.documents.is_empty() {
            if self.documents[self.active].dirty {
                self.pending = Some(Pending::Close(self.active));
                self.commit_current_sheet();
            } else {
                self.close(self.active);
            }
        }
        if (key(M::ALT, Key::ArrowLeft) || key(M::COMMAND, Key::OpenBracket))
            && let Some(location) = self.history.pop()
        {
            self.open(location.path, Some((location.line, location.column)));
        }
        if self.picker.is_none()
            && let Some(doc) = self.documents.get_mut(self.active)
            && doc
                .editor_id
                .is_some_and(|id| ctx.memory(|memory| memory.has_focus(id)))
        {
            let duplicate = key(M::COMMAND, Key::D);
            let comment = key(M::COMMAND, Key::Slash);
            if (duplicate || comment)
                && let Content::Text(text) = &mut doc.content
            {
                edit_line(text, doc.cursor, &doc.path, duplicate);
                doc.dirty = true;
                doc.outline_refresh = Some(Instant::now());
            }
        }
    }

    fn picker_ui(&mut self, ctx: &egui::Context) {
        let Some(mode) = self.picker else {
            return;
        };
        let mut open = None;
        let mut close = false;
        egui::Modal::new(egui::Id::new("navigation-picker"))
            .frame(chrome::popup_frame())
            .show(ctx, |ui| {
                chrome::dialog_style(ui, (ctx.content_rect().width() - 120.0).clamp(450.0, 820.0));
                close |= chrome::dialog_header(
                    ui,
                    "⌕",
                    match mode {
                        Picker::Symbols => "File structure",
                        Picker::Line => "Go to line",
                        Picker::Recent => "Recent files",
                        _ => "Go to file",
                    },
                    "↑ ↓ select  ·  Enter open  ·  Esc close",
                    chrome::BLUE,
                );
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.picker_query)
                        .frame(false)
                        .font(FontId::proportional(21.0))
                        .desired_width(f32::INFINITY)
                        .hint_text(if mode == Picker::Line {
                            "Line number, optionally :column"
                        } else if mode == Picker::Symbols {
                            "Filter symbols in this file"
                        } else {
                            "File name, optionally :line or :line:column"
                        }),
                );
                if self.picker_focus {
                    field.request_focus();
                    self.picker_focus = false;
                }
                if field.changed() {
                    self.picker_selection = 0;
                }
                let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter));
                if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                    close = true;
                }
                ui.separator();
                let (file_query, target) = if matches!(mode, Picker::Symbols | Picker::Line) {
                    (self.picker_query.as_str(), None)
                } else {
                    split_file_location(&self.picker_query)
                };
                let query = file_query.to_lowercase();
                if mode == Picker::Line {
                    if enter {
                        let mut parts = query.split(':');
                        if let Some(line) = parts.next().and_then(|p| p.parse::<usize>().ok())
                            && let Some(doc) = self.documents.get(self.active)
                        {
                            open = Some(Location {
                                category: navigation::MatchCategory::Other,
                                path: doc.path.clone(),
                                line: line.max(1),
                                column: parts
                                    .next()
                                    .and_then(|p| p.parse::<usize>().ok())
                                    .unwrap_or(1)
                                    .max(1),
                                preview: String::new(),
                            });
                        }
                    }
                } else {
                    let mut entries: Vec<(String, Location)> = vec![];
                    if mode == Picker::Symbols
                        && let Some(doc) = self.documents.get(self.active)
                    {
                        for symbol in &doc.outline {
                            if symbol.name.to_lowercase().contains(&query) {
                                entries.push((
                                    format!("{}   :{}", symbol.name, symbol.line),
                                    Location {
                                        category: navigation::MatchCategory::Other,
                                        path: doc.path.clone(),
                                        line: symbol.line,
                                        column: symbol.column,
                                        preview: String::new(),
                                    },
                                ));
                            }
                        }
                    } else {
                        if query.is_empty() {
                            ui.weak("Recently chosen files first");
                        }
                        let paths = file_candidates(
                            &self.recent_file_searches,
                            &self
                                .documents
                                .iter()
                                .rev()
                                .map(|doc| doc.path.clone())
                                .collect::<Vec<_>>(),
                            if mode == Picker::Recent {
                                &[]
                            } else {
                                &self.files
                            },
                            &self.root,
                            &query,
                        );
                        for path in paths {
                            entries.push((
                                relative(&path, &self.root),
                                Location {
                                    category: navigation::MatchCategory::Other,
                                    path,
                                    line: target.map_or(1, |(line, _)| line),
                                    column: target.map_or(1, |(_, column)| column),
                                    preview: String::new(),
                                },
                            ));
                        }
                    }
                    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowDown)) {
                        self.picker_selection =
                            (self.picker_selection + 1).min(entries.len().saturating_sub(1));
                    }
                    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowUp)) {
                        self.picker_selection = self.picker_selection.saturating_sub(1);
                    }
                    self.picker_selection =
                        self.picker_selection.min(entries.len().saturating_sub(1));
                    egui::ScrollArea::vertical()
                        .min_scrolled_height(120.0)
                        .auto_shrink([false, true])
                        .max_height((ctx.content_rect().height() * 0.42).clamp(160.0, 360.0))
                        .show(ui, |ui| {
                            for (index, (label, location)) in entries.iter().enumerate() {
                                let selected = index == self.picker_selection;
                                let (rect, response) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), 32.0),
                                    egui::Sense::click(),
                                );
                                if selected || response.hovered() {
                                    ui.painter().rect_filled(
                                        rect,
                                        5.0,
                                        if selected {
                                            chrome::SELECTED
                                        } else {
                                            chrome::HEADER
                                        },
                                    );
                                }
                                let display =
                                    if mode == Picker::Symbols || label.starts_with("Symbol:") {
                                        label.to_owned()
                                    } else {
                                        location
                                            .path
                                            .file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy()
                                            .to_string()
                                    };
                                let galley = ui.fonts_mut(|f| {
                                    f.layout_job(chrome::highlighted(&display, file_query, 16.0))
                                });
                                let filename_rect = egui::Rect::from_min_max(
                                    rect.min + egui::vec2(34.0, 0.0),
                                    egui::pos2(rect.left() + rect.width() * 0.62, rect.bottom()),
                                );
                                ui.painter()
                                    .with_clip_rect(filename_rect.intersect(ui.clip_rect()))
                                    .galley(
                                        rect.left_center()
                                            + egui::vec2(34.0, -galley.size().y / 2.0),
                                        galley,
                                        chrome::TEXT,
                                    );
                                ui.put(
                                    egui::Rect::from_min_size(
                                        rect.min + egui::vec2(6.0, 4.0),
                                        egui::vec2(24.0, 24.0),
                                    ),
                                    egui::Label::new(chrome::file_icon(&location.path)),
                                );
                                let parent = relative(
                                    location.path.parent().unwrap_or(Path::new("")),
                                    &self.root,
                                );
                                ui.painter()
                                    .with_clip_rect(
                                        egui::Rect::from_min_max(
                                            egui::pos2(filename_rect.right() + 16.0, rect.top()),
                                            rect.max,
                                        )
                                        .intersect(ui.clip_rect()),
                                    )
                                    .text(
                                        rect.right_center() - egui::vec2(12.0, 0.0),
                                        egui::Align2::RIGHT_CENTER,
                                        parent,
                                        FontId::proportional(13.0),
                                        chrome::MUTED,
                                    );
                                if response.clicked() || (enter && index == self.picker_selection) {
                                    open = Some(location.clone());
                                }
                            }
                        });
                    if entries.is_empty() {
                        ui.weak("No matching files or symbols");
                    }
                }
                chrome::dialog_divider(ui);
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    close |= chrome::dialog_button(ui, "Cancel", None).clicked();
                });
            });
        if let Some(location) = open {
            if !matches!(mode, Picker::Line | Picker::Symbols) {
                self.recent_file_searches
                    .retain(|path| path != &location.path);
                self.recent_file_searches.insert(0, location.path.clone());
                self.recent_file_searches.truncate(50);
            }
            self.navigate(location);
            self.picker = None;
        } else if close {
            self.picker = None;
        }
    }

    fn draw(&mut self, ctx: &egui::Context) {
        self.defer_actions = true;
        self.poll(ctx);
        let blocked = self.pending.is_some()
            || self.terminal.modal()
            || self.file_action.is_some()
            || self.error.is_some()
            || self.picker.is_some()
            || (self.search_open && !self.search_docked);
        if !blocked
            && (ctx.input_mut(|i| i.consume_key(egui::Modifiers::ALT, Key::F12))
                || ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::Backtick)))
        {
            self.terminal.toggle(&self.root, ctx);
        }
        if !blocked {
            self.terminal.shortcuts(ctx, &self.root);
        }
        if !blocked && !self.terminal.focused(ctx) {
            self.shortcuts(ctx);
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::S))
                && !self.documents.is_empty()
            {
                self.save(self.active);
            }
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::SHIFT, Key::F12)) {
                self.start_search(SearchKind::References, true);
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::F12)) {
                self.start_search(SearchKind::Definitions, true);
            }
        }
        if ctx.input(|i| i.viewport().close_requested())
            && !self.allow_exit
            && self.documents.iter().any(|d| d.dirty)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.pending = Some(Pending::Exit);
            self.commit_current_sheet();
        }
        self.toolbar(ctx);
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.small(&self.status);
                ui.separator();
                ui.small("UTF-8");
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if let Some(location) = self.current_location() {
                        ui.small(format!("{}:{}", location.line, location.column));
                    }
                    ui.small("LF");
                });
            });
        });
        self.tool_rail(ctx);
        self.structure_panel(ctx);
        self.terminal.panel(ctx, &self.root, !blocked);
        if self.search_docked {
            self.results_panel(ctx);
        }
        if self.show_project {
            self.sidebar(ctx);
        }
        if self.picker.is_some() {
            chrome::clear_resize_cursor(ctx);
        }
        self.picker_ui(ctx);
        self.editor(ctx);
        if !self.search_docked {
            self.results_panel(ctx);
        }
        self.defer_actions = false;
        for action in std::mem::take(&mut self.actions) {
            match action {
                FrameAction::Open(path, target) => self.open(path, target),
                FrameAction::SwitchProject(root) => self.switch_project(root),
                FrameAction::Save(index) => {
                    self.save(index);
                }
                FrameAction::Close(index) => self.close(index),
                FrameAction::Activate(index) => self.active = index,
                FrameAction::GitDiff(index) => {
                    let doc = &self.documents[index];
                    let buffer = if doc.dirty {
                        match &doc.content {
                            Content::Text(text) => Some(text.clone()),
                            Content::Csv(sheet) => sheet.serialize().ok(),
                        }
                    } else {
                        None
                    };
                    self.git.open_review(doc.path.clone(), buffer, ctx);
                }
                FrameAction::Reload(index) => {
                    if self.documents[index].dirty {
                        self.pending = Some(Pending::Reload(index));
                    } else if let Err(error) = self.documents[index].reload(self.csv_headers) {
                        self.error = Some(error.to_string());
                    }
                }
            }
        }
        if let Some(path) = self.git.windows(ctx) {
            self.open(path, None);
        }
        if self.pending.is_some()
            || self.terminal.modal()
            || self.file_action.is_some()
            || self.error.is_some()
            || self.picker.is_some()
        {
            chrome::clear_resize_cursor(ctx);
        }
        self.dialogs(ctx);
        self.terminal.windows(ctx);
        let mut panels = Vec::new();
        if self.show_project {
            panels.push(("explorer", true));
        }
        if self.show_structure {
            panels.push(("structure", true));
        }
        if self.git.visible {
            panels.push(("git-changes-panel", true));
        }
        if self.terminal.visible {
            panels.push(("terminal-panel", false));
        }
        if self.search_open && self.search_docked {
            panels.push(("search-results", false));
        }
        let mut windows = self.git.resize_windows();
        if self.search_open && !self.search_docked {
            windows.push(egui::Id::new("find-in-files"));
        }
        let modal = self.pending.is_some()
            || self.terminal.modal()
            || self.file_action.is_some()
            || self.error.is_some()
            || self.picker.is_some();
        chrome::finish_resize_borders(ctx, &panels, &windows, modal);
    }
}

impl eframe::App for Coder {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.draw(ctx);
        #[cfg(feature = "ui-capture")]
        if let Ok(path) = std::env::var("CODER_CAPTURE_TO") {
            let screenshot = ctx.input(|i| {
                i.events.iter().find_map(|event| {
                    if let egui::Event::Screenshot { image, .. } = event {
                        Some(image.clone())
                    } else {
                        None
                    }
                })
            });
            if let Some(capture) = screenshot {
                image::save_buffer(
                    path,
                    capture.as_raw(),
                    capture.width() as u32,
                    capture.height() as u32,
                    image::ColorType::Rgba8,
                )
                .expect("save UI capture");
                self.allow_exit = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                if ctx.cumulative_pass_nr() >= 12
                    && ctx.input(|i| i.time) >= 8.0
                    && !self.preview.loading()
                {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
                }
                ctx.request_repaint();
            }
        }
    }
}

impl Drop for Coder {
    fn drop(&mut self) {
        self.search_cancel.store(true, Ordering::Relaxed);
    }
}

/// Parse numeric suffixes from the right, preserving colons in the path itself.
fn split_file_location(query: &str) -> (&str, Option<(usize, usize)>) {
    let query = query.trim();
    let Some((prefix, last)) = query.rsplit_once(':') else {
        return (query, None);
    };
    let Ok(number) = last.parse::<usize>() else {
        return (query, None);
    };
    if let Some((path, line)) = prefix.rsplit_once(':')
        && let Ok(line) = line.parse::<usize>()
        && !path.is_empty()
    {
        return (path, Some((line.max(1), number.max(1))));
    }
    if prefix.is_empty() {
        return (query, None);
    }
    (prefix, Some((number.max(1), 1)))
}

fn file_candidates(
    recent: &[PathBuf],
    open: &[PathBuf],
    files: &[PathBuf],
    root: &Path,
    query: &str,
) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    recent
        .iter()
        .chain(open)
        .chain(files)
        .filter(|path| path.starts_with(root))
        .filter(|path| relative(path, root).to_lowercase().contains(query))
        .filter(|path| seen.insert((*path).clone()))
        .take(100)
        .cloned()
        .collect()
}

struct FileToolbarState<'a> {
    dirty: bool,
    can_reload: bool,
    git: bool,
    blame: bool,
    blame_note: Option<&'a str>,
}

#[derive(Default)]
struct FileToolbarActions {
    blame: bool,
    diff: bool,
    reload: bool,
    reveal: bool,
}

fn file_toolbar(
    ui: &mut egui::Ui,
    path: &Path,
    root: &Path,
    state: FileToolbarState<'_>,
) -> FileToolbarActions {
    let mut actions = FileToolbarActions::default();
    let bar = egui::Frame::new().fill(chrome::TABS)
        .inner_margin(egui::Margin::symmetric(10, 3)).show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.spacing_mut().item_spacing.x = 6.0;
            let width = ui.available_width();
            let compact = width < 440.0;
            let blame_label = if compact { "" } else { "Blame" };
            let diff_label = if width < 620.0 { "" } else { "Branch diff" };
            let (status, color, status_hint) = if state.dirty {
                ("Unsaved", chrome::WARNING, "This file has unsaved editor changes")
            } else if state.can_reload {
                ("Disk changed", chrome::WARNING, "The saved file changed outside the editor. Reload to read its latest contents.")
            } else { ("Saved", chrome::GREEN, "No unsaved editor changes") };
            let status_label = if compact { "" } else { status };
            let reserve = chrome::status_width(ui, status_label) + 28.0 + 22.0
                + if state.git { chrome::action_chip_width(ui, blame_label) + chrome::action_chip_width(ui, diff_label) + 12.0 } else { 0.0 };
            ui.horizontal(|ui| {
                actions.reveal = file_breadcrumb(ui, path, root, (width - reserve).max(36.0));
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    actions.reload = ui.add_enabled_ui(state.can_reload, |ui| {
                        chrome::action_chip(ui, chrome::Icon::Refresh, "", false,
                            if state.can_reload { "Reload from disk · unsaved changes require confirmation" } else { "Reload from disk · contents already match" })
                    }).inner.clicked();
                    if state.git {
                        actions.diff = chrome::action_chip(ui, chrome::Icon::Branch, diff_label, false, "Review branch changes against the base branch").clicked();
                        actions.blame = chrome::action_chip(ui, chrome::Icon::History, blame_label, state.blame,
                            state.blame_note.unwrap_or(if state.blame { "Hide line authors and commit annotations" } else { "Show line authors and commit annotations" })).clicked();
                    }
                    let (divider, _) = ui.allocate_exact_size(egui::vec2(8.0, 18.0), egui::Sense::hover());
                    ui.painter().vline(divider.center().x, divider.y_range(), egui::Stroke::new(1.0_f32, chrome::BORDER));
                    chrome::status_pill(ui, status_label, color).on_hover_text(status_hint);
                });
            });
        });
    ui.painter().line_segment(
        [
            bar.response.rect.left_bottom(),
            bar.response.rect.right_bottom(),
        ],
        egui::Stroke::new(1.0_f32, chrome::BORDER),
    );
    actions
}

fn file_breadcrumb(ui: &mut egui::Ui, path: &Path, root: &Path, width: f32) -> bool {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 28.0), egui::Sense::click());
    let relative = path.strip_prefix(root).unwrap_or(path);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let parents: Vec<_> = relative
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let text_width = (rect.width() - 28.0).max(8.0);
    let mut skip = 0;
    let mut prefix;
    loop {
        prefix = if skip < parents.len() {
            format!(
                "{}{}  ›  ",
                if skip > 0 { "…  ›  " } else { "" },
                parents[skip..].join("  ›  ")
            )
        } else if !parents.is_empty() {
            "…  ›  ".to_owned()
        } else {
            String::new()
        };
        let measured = ui.fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(
                    format!("{prefix}{name}"),
                    FontId::proportional(12.0),
                    chrome::TEXT,
                )
                .size()
                .x
        });
        if measured <= text_width || skip >= parents.len() {
            break;
        }
        skip += 1;
    }
    if response.hovered() {
        ui.painter()
            .rect_filled(rect, 4, chrome::HEADER.gamma_multiply(0.5));
    }
    let (icon, color) = chrome::file_kind(path);
    ui.painter().text(
        rect.left_center() + egui::vec2(9.0, 0.0),
        egui::Align2::CENTER_CENTER,
        icon,
        FontId::monospace(10.0),
        color,
    );
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &prefix,
        0.0,
        egui::TextFormat::simple(FontId::proportional(12.0), chrome::MUTED),
    );
    job.append(
        &name,
        0.0,
        egui::TextFormat::simple(FontId::proportional(12.0), Color32::from_rgb(225, 230, 239)),
    );
    job.wrap.max_width = text_width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
    ui.painter().with_clip_rect(rect).galley(
        egui::pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0),
        galley,
        chrome::TEXT,
    );
    let mut reveal = false;
    response.context_menu(|ui| {
        for (label, text) in [
            ("Copy file name", name.to_string()),
            ("Copy absolute path", path.display().to_string()),
            (
                "Copy path from content root",
                relative.display().to_string(),
            ),
        ] {
            if ui.button(label).clicked() {
                ui.ctx().copy_text(text);
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Reveal in Project").clicked() {
            reveal = true;
            ui.close();
        }
    });
    response.on_hover_text(format!(
        "{}\nRight-click to copy the path or reveal in Project",
        path.display()
    ));
    reveal
}

const BLAME_WIDTH: f32 = 156.0;
const BLAME_HOVER: Color32 = Color32::from_rgb(36, 47, 63);

fn blame_accent(blame: &crate::git::BlameLine) -> Color32 {
    if blame.local() {
        chrome::WARNING
    } else {
        [
            chrome::BLUE,
            chrome::PURPLE,
            chrome::GREEN,
            Color32::from_rgb(113, 185, 204),
        ][(egui::util::hash(&blame.commit) % 4) as usize]
    }
}

fn blame_date(date: &str) -> String {
    let parts: Vec<_> = date.split('-').collect();
    if parts.len() == 3
        && let (Ok(month), Ok(day)) = (parts[1].parse::<usize>(), parts[2].parse::<usize>())
        && let Some(name) = [
            "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
        ]
        .get(month.wrapping_sub(1))
    {
        return format!("{name} {day}");
    }
    date.to_owned()
}

fn relative(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn category_toggle(
    ui: &mut egui::Ui,
    selected: &mut Option<navigation::MatchCategory>,
    category: Option<navigation::MatchCategory>,
    label: String,
) {
    let active = *selected == category;
    if ui
        .add(
            egui::Button::new(label)
                .selected(active)
                .fill(if active {
                    chrome::SELECTED
                } else {
                    Color32::TRANSPARENT
                })
                .stroke(egui::Stroke::NONE)
                .corner_radius(5),
        )
        .clicked()
    {
        *selected = category;
    }
}

fn markdown_preview(ui: &mut egui::Ui, cache: &mut CommonMarkCache, text: &str, path: &Path) {
    let scroll = egui::ScrollArea::vertical();
    #[cfg(feature = "ui-capture")]
    let scroll = if std::env::var("CODER_UI_PREVIEW").as_deref() == Ok("markdown") {
        scroll.vertical_scroll_offset(0.0)
    } else {
        scroll
    };
    scroll
        .id_salt("markdown-preview")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Frame::new().inner_margin(24).show(ui, |ui| {
                let style = ui.style_mut();
                style.override_font_id = None;
                style.visuals.override_text_color = None;
                style.visuals.hyperlink_color = chrome::BLUE;
                if style.visuals.dark_mode {
                    style.visuals.widgets.noninteractive.fg_stroke.color = chrome::TEXT;
                    style.visuals.widgets.active.fg_stroke.color = chrome::TEXT;
                }
                style
                    .text_styles
                    .insert(egui::TextStyle::Body, FontId::proportional(15.0));
                style.text_styles.insert(
                    egui::TextStyle::Heading,
                    FontId::new(32.0, egui::FontFamily::Name("heading".into())),
                );
                style.spacing.item_spacing.y = 12.0;
                style.url_in_tooltip = true;
                let directory =
                    url::Url::from_directory_path(path.parent().unwrap_or(Path::new(".")))
                        .expect("document directory is absolute");
                let base = directory.clone();
                let html = move |ui: &mut egui::Ui, source: &str| {
                    // Render inert image tags only; never execute embedded HTML or scripts.
                    let render = |ui: &mut egui::Ui| {
                        for image in html_images(source) {
                            let Ok(uri) = base.join(&image.src) else {
                                continue;
                            };
                            if !matches!(uri.scheme(), "http" | "https" | "file") {
                                continue;
                            }
                            let mut widget = egui::Image::new(uri.to_string())
                                .max_width(ui.available_width())
                                .maintain_aspect_ratio(true);
                            if let Some(width) = image.width {
                                widget = widget
                                    .fit_to_original_size(1.0)
                                    .max_width(width.min(ui.available_width()));
                            }
                            let linked = base.join(&image.href).ok().filter(|url| {
                                !image.href.is_empty() && matches!(url.scheme(), "http" | "https")
                            });
                            if linked.is_some() {
                                widget = widget.sense(egui::Sense::click());
                            }
                            let response = ui.add(widget).on_hover_text(image.alt);
                            if let Some(link) = linked {
                                if response.hovered() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                if response.clicked() {
                                    ui.ctx().open_url(egui::OpenUrl::new_tab(link.to_string()));
                                }
                            }
                        }
                    };
                    if source.contains("data-coder-images") {
                        ui.horizontal_wrapped(render);
                    } else {
                        render(ui);
                    }
                };
                let prepared = crate::markdown_images::prepare(text);
                CommonMarkViewer::new()
                    .default_implicit_uri_scheme(directory.to_string())
                    .max_image_width(Some(ui.available_width().max(1.0) as usize))
                    .render_html_fn(Some(&html))
                    .show(ui, cache, &prepared);
            });
        });
}

#[derive(Debug)]
struct HtmlImage {
    src: String,
    href: String,
    alt: String,
    width: Option<f32>,
}

fn html_images(source: &str) -> Vec<HtmlImage> {
    use std::sync::OnceLock;
    static TAG: OnceLock<regex::Regex> = OnceLock::new();
    static ATTR: OnceLock<regex::Regex> = OnceLock::new();
    let tags = TAG.get_or_init(|| regex::Regex::new(r"(?is)<img\b[^>]*>").unwrap());
    let attrs = ATTR.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)\b(src|alt|width|data-href)\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#,
        )
        .unwrap()
    });
    tags.find_iter(source)
        .filter_map(|tag| {
            let mut image = HtmlImage {
                src: String::new(),
                href: String::new(),
                alt: String::new(),
                width: None,
            };
            for attr in attrs.captures_iter(tag.as_str()) {
                let value = attr
                    .get(2)
                    .or_else(|| attr.get(3))
                    .or_else(|| attr.get(4))
                    .unwrap()
                    .as_str()
                    .replace("&quot;", "\"")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&amp;", "&");
                match attr[1].to_ascii_lowercase().as_str() {
                    "src" => image.src = value,
                    "data-href" => image.href = value,
                    "alt" => image.alt = value,
                    "width" => {
                        image.width = value
                            .parse::<f32>()
                            .ok()
                            .filter(|v| v.is_finite() && *v > 0.0)
                    }
                    _ => {}
                }
            }
            (!image.src.is_empty()).then_some(image)
        })
        .collect()
}

#[derive(Default)]
struct EditorFind<'a> {
    query: &'a str,
    active: Option<std::ops::Range<usize>>,
    scroll: bool,
    git: Option<&'a crate::git::FileData>,
    blame: bool,
}

fn text_editor(
    ui: &mut egui::Ui,
    text: &mut String,
    path: &Path,
    cursor: &mut usize,
    jump: &mut Option<(usize, usize)>,
    editor_id: &mut Option<egui::Id>,
    find: EditorFind<'_>,
) -> (bool, Option<String>, Option<crate::git::BlameLine>) {
    let id = ui.make_persistent_id("source-editor");
    let font_size = crate::preferences::editor_font_size(ui.ctx());
    *editor_id = Some(id);
    let find_offset = find
        .active
        .as_ref()
        .filter(|_| find.scroll && !find.query.is_empty())
        .map(|range| text[..range.start.min(text.len())].chars().count());
    if let Some(offset) = find_offset {
        let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(offset),
            )));
        state.store(ui.ctx(), id);
        *cursor = offset;
    }
    let jump_offset = jump
        .take()
        .map(|(line, column)| navigation::char_offset(text, line, column));
    if let Some(offset) = jump_offset {
        let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(offset),
            )));
        state.store(ui.ctx(), id);
        ui.memory_mut(|memory| memory.request_focus(id));
        *cursor = offset;
    }
    let selected = egui::TextEdit::load_state(ui.ctx(), id)
        .and_then(|state| state.cursor.char_range())
        .filter(|range| !range.is_empty())
        .map(|range| {
            let indices = range.as_sorted_char_range();
            text.chars()
                .skip(indices.start)
                .take(indices.len())
                .collect::<String>()
        })
        .unwrap_or_default();
    let occurrence = if find.query.is_empty() {
        selected.as_str()
    } else {
        find.query
    };
    let extension = path.extension().and_then(|s| s.to_str()).unwrap_or("txt");
    let fold_id = id.with(("folds", path));
    let mut folds = ui
        .ctx()
        .data_mut(|data| data.get_temp::<crate::folding::State>(fold_id))
        .unwrap_or_default();
    folds.sync(text);
    let job = crate::highlighting::highlight(ui, text, extension);
    let fold_regions = ui.ctx().memory_mut(|memory| {
        memory.caches.cache::<egui::cache::FrameCache<Vec<crate::folding::Region>, crate::folding::Computer>>()
            .get((&job, extension))
    });
    if let Some(offset) = jump_offset.or(find_offset) {
        folds.reveal(&fold_regions, offset..=offset);
    }
    #[cfg(feature = "ui-capture")]
    if std::env::var("CODER_UI_PREVIEW").as_deref() == Ok("folding")
        && !ui
            .ctx()
            .data_mut(|data| data.get_temp::<bool>(fold_id.with("capture-seeded")))
            .unwrap_or(false)
    {
        if let Some(region) = fold_regions
            .iter()
            .find(|r| r.first > 0 && r.preview.contains("supportsTablet"))
        {
            folds.collapsed.insert(region.start);
        }
        ui.ctx()
            .data_mut(|data| data.insert_temp(fold_id.with("capture-seeded"), true));
    }
    let active_folds = folds.active(&fold_regions);
    let source_hash = egui::util::hash(text.as_str());
    let mut layouter = |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, _wrap_width: f32| {
        let mut job = crate::highlighting::highlight(ui, buffer.as_str(), extension);
        chrome::mark_matches(&mut job, occurrence);
        if !find.query.is_empty()
            && let Some(active) = &find.active
        {
            for section in &mut job.sections {
                if section.byte_range.start >= active.start && section.byte_range.end <= active.end
                {
                    section.format.background = chrome::SELECTED;
                    section.format.underline = egui::Stroke::new(2.0_f32, chrome::BLUE);
                }
            }
        }

        let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
        // Edits always lay out the full new buffer; anchors are rebuilt next frame.
        if egui::util::hash(buffer.as_str()) == source_hash {
            crate::folding::layout(galley, &active_folds)
        } else {
            galley
        }
    };
    let mut changed = false;
    let mut clicked_token = None;
    let mut clicked_blame = None;
    ui.painter().rect_filled(
        ui.available_rect_before_wrap(),
        0.0,
        if ui.visuals().dark_mode {
            chrome::EDITOR
        } else {
            Color32::WHITE
        },
    );
    egui::ScrollArea::both()
        .id_salt("source-scroll")
        .show(ui, |ui| {
            let gutter_width =
                (if find.blame { BLAME_WIDTH + 8.0 } else { 0.0 }) + 62.0 * font_size / 13.0;
            let (gutter, _) =
                ui.allocate_exact_size(egui::vec2(gutter_width, 0.0), egui::Sense::hover());
            ui.horizontal_top(|ui| {
                ui.add_space(gutter_width);
                let background = ui.painter().add(egui::Shape::Noop);
                let guide_layer = ui.painter().add(egui::Shape::Noop);
                let git_layer = ui.painter().add(egui::Shape::Noop);
                let blame_hover_layer = ui.painter().add(egui::Shape::Noop);
                let output = egui::TextEdit::multiline(text)
                    .id(id)
                    .font(FontId::monospace(font_size))
                    .code_editor()
                    .frame(false)
                    .desired_width(f32::INFINITY)
                    .desired_rows(30)
                    .layouter(&mut layouter)
                    .show(ui);
                if let Some(range) = output.cursor_range {
                    *cursor = range.primary.index;
                }
                if output.response.hovered()
                    && ui.input(|i| i.modifiers.command)
                    && let Some(pointer) = ui.input(|i| i.pointer.hover_pos())
                    && let Some((range, token)) =
                        hovered_token(text, &output.galley, pointer - output.galley_pos)
                {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    let left = output
                        .galley
                        .pos_from_cursor(egui::text::CCursor::new(range.start));
                    let right = output
                        .galley
                        .pos_from_cursor(egui::text::CCursor::new(range.end));
                    let rect = egui::Rect::from_min_max(left.min, right.max)
                        .translate(output.galley_pos.to_vec2());
                    ui.painter()
                        .rect_filled(rect, 2.0, chrome::BLUE.gamma_multiply(0.16));
                    ui.painter().line_segment(
                        [rect.left_bottom(), rect.right_bottom()],
                        egui::Stroke::new(1.0_f32, chrome::BLUE),
                    );
                    if ui.input(|i| i.pointer.primary_clicked()) {
                        *cursor = range.start;
                        clicked_token = Some(token);
                    }
                }
                let caret = output
                    .galley
                    .pos_from_cursor(egui::text::CCursor::new(*cursor));
                let line_rect = egui::Rect::from_min_max(
                    egui::pos2(
                        output.response.rect.left(),
                        output.galley_pos.y + caret.top(),
                    ),
                    egui::pos2(
                        output.response.rect.right(),
                        output.galley_pos.y + caret.bottom(),
                    ),
                );
                ui.painter().set(
                    background,
                    egui::Shape::rect_filled(
                        line_rect,
                        0.0,
                        if ui.visuals().dark_mode {
                            Color32::from_rgb(38, 40, 44)
                        } else {
                            Color32::from_rgb(242, 247, 255)
                        },
                    ),
                );
                let mut guides = vec![];
                let guide_color = if ui.visuals().dark_mode {
                    Color32::from_rgb(61, 64, 69)
                } else {
                    Color32::from_gray(212)
                };
                let structure = ui.ctx().memory_mut(|memory| {
                    memory
                        .caches
                        .cache::<egui::cache::FrameCache<
                            crate::editor_structure::Structure,
                            crate::editor_structure::Computer,
                        >>()
                        .get(output.galley.job.as_ref())
                });
                let active_pair = structure.matching(*cursor);
                for guide in &structure.guides {
                    if let (Some(start), Some(end)) = (
                        output.galley.rows.get(guide.start),
                        output.galley.rows.get(guide.end),
                    ) {
                        if start.size.y <= 0.0 {
                            continue;
                        }
                        let x = output
                            .galley
                            .pos_from_cursor(egui::text::CCursor::new(guide.anchor))
                            .left()
                            + output.galley_pos.x;
                        let top = (output.galley_pos.y + start.pos.y + start.size.y)
                            .max(ui.clip_rect().top());
                        let bottom = (output.galley_pos.y + end.pos.y).min(ui.clip_rect().bottom());
                        if top < bottom {
                            guides.push(egui::Shape::line_segment(
                                [egui::pos2(x, top), egui::pos2(x, bottom)],
                                egui::Stroke::new(1.0_f32, guide_color),
                            ));
                        }
                    }
                }
                if let Some((open, close)) = active_pair {
                    for index in [open, close] {
                        let start = output
                            .galley
                            .pos_from_cursor(egui::text::CCursor::new(index));
                        let end = output
                            .galley
                            .pos_from_cursor(egui::text::CCursor::new(index + 1));
                        if start.height() <= 0.0 {
                            continue;
                        }
                        let rect = egui::Rect::from_min_max(
                            start.min,
                            egui::pos2(end.left(), start.bottom()),
                        )
                        .translate(output.galley_pos.to_vec2());
                        ui.painter()
                            .rect_filled(rect, 2.0, chrome::BLUE.gamma_multiply(0.18));
                        ui.painter().rect_stroke(
                            rect,
                            2.0,
                            egui::Stroke::new(1.0_f32, chrome::BLUE),
                            egui::StrokeKind::Inside,
                        );
                    }
                }
                // Folding controls occupy a separate gutter column so they never select text.
                let mut toggle_fold = None;
                for region in &fold_regions {
                    let Some(row) = output.galley.rows.get(region.first) else {
                        continue;
                    };
                    if row.size.y <= 0.0 {
                        continue;
                    }
                    let y = output.galley_pos.y + row.pos.y;
                    let rect = egui::Rect::from_min_size(
                        egui::pos2(output.galley_pos.x - 22.0, y),
                        egui::vec2(15.0, row.size.y),
                    );
                    if !ui.clip_rect().intersects(rect) {
                        continue;
                    }
                    let collapsed = folds.collapsed.contains(&region.start);
                    let response = ui
                        .interact(rect, fold_id.with(region.start), egui::Sense::click())
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text(if collapsed {
                            "Expand block"
                        } else {
                            "Collapse block"
                        });
                    let center = rect.center();
                    let points = if collapsed {
                        vec![
                            center + egui::vec2(-2.0, -4.0),
                            center + egui::vec2(2.0, 0.0),
                            center + egui::vec2(-2.0, 4.0),
                        ]
                    } else {
                        vec![
                            center + egui::vec2(-4.0, -2.0),
                            center + egui::vec2(0.0, 2.0),
                            center + egui::vec2(4.0, -2.0),
                        ]
                    };
                    ui.painter().add(egui::Shape::line(
                        points,
                        egui::Stroke::new(1.2_f32, chrome::MUTED),
                    ));
                    if response.clicked() {
                        toggle_fold = Some(region);
                    }
                    if collapsed {
                        let preview = ui.painter().layout_no_wrap(
                            region.preview.clone(),
                            FontId::monospace(font_size),
                            chrome::MUTED,
                        );
                        let preview_rect = egui::Rect::from_min_size(
                            egui::pos2(output.galley_pos.x + row.size.x + 4.0, y),
                            preview.size(),
                        );
                        ui.painter().rect_filled(preview_rect, 2.0, chrome::PANEL);
                        ui.painter()
                            .galley(preview_rect.min, preview, chrome::MUTED);
                        if ui
                            .interact(
                                preview_rect,
                                fold_id.with(("preview", region.start)),
                                egui::Sense::click(),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text("Expand block")
                            .clicked()
                        {
                            toggle_fold = Some(region);
                        }
                    }
                }
                if let Some(region) = toggle_fold {
                    if !folds.collapsed.remove(&region.start) {
                        folds.collapsed.insert(region.start);
                        let mut state = output.state.clone();
                        let offset = region.start + 1;
                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::one(
                                egui::text::CCursor::new(offset),
                            )));
                        state.store(ui.ctx(), id);
                        *cursor = offset;
                    }
                    ui.ctx().request_repaint();
                } else if let Some(range) = output.cursor_range {
                    let range = range.as_sorted_char_range();
                    if folds.reveal(&fold_regions, range.start..=range.end) {
                        ui.ctx().request_repaint();
                    }
                }
                let data = find
                    .git
                    .filter(|data| data.path == path && data.text == *text);
                let mut git_shapes = vec![];
                let mut marks = data.map(|data| data.marks.iter().peekable());
                let mut previous_blame: Option<&crate::git::BlameLine> = None;
                let mut group_row = 0;
                for (index, row) in output.galley.rows.iter().enumerate() {
                    let y = output.galley_pos.y + row.pos.y;
                    if row.size.y <= 0.0 {
                        continue;
                    }
                    if y + row.size.y >= ui.clip_rect().top() && y <= ui.clip_rect().bottom() {
                        if let Some(marks) = &mut marks {
                            while marks.peek().is_some_and(|(line, _)| *line < index) {
                                marks.next();
                            }
                            while let Some((line, mark)) = marks.peek() {
                                if *line != index {
                                    break;
                                }
                                let color = crate::git_view::marker(*mark);
                                let x = output.galley_pos.x - 6.0;
                                if *mark == crate::git::Mark::Deleted {
                                    git_shapes.push(egui::Shape::convex_polygon(
                                        vec![
                                            egui::pos2(x - 2.0, y),
                                            egui::pos2(x + 5.0, y + 3.5),
                                            egui::pos2(x - 2.0, y + 7.0),
                                        ],
                                        color,
                                        egui::Stroke::NONE,
                                    ));
                                } else {
                                    git_shapes.push(egui::Shape::rect_filled(
                                        egui::Rect::from_min_size(
                                            egui::pos2(x, y),
                                            egui::vec2(3.0, row.size.y),
                                        ),
                                        1.0,
                                        color,
                                    ));
                                    git_shapes.push(egui::Shape::rect_filled(
                                        egui::Rect::from_min_size(
                                            egui::pos2(output.galley_pos.x, y),
                                            egui::vec2(output.response.rect.width(), row.size.y),
                                        ),
                                        0.0,
                                        color.gamma_multiply(0.055),
                                    ));
                                }
                                marks.next();
                            }
                        }
                        if find.blame {
                            let hover_row = egui::Rect::from_min_max(
                                egui::pos2(gutter.left(), y),
                                egui::pos2(output.response.rect.right(), y + row.size.y),
                            );
                            if ui.rect_contains_pointer(hover_row) {
                                let highlight = egui::Rect::from_min_max(
                                    egui::pos2(gutter.left() + BLAME_WIDTH + 4.0, y),
                                    hover_row.max,
                                );
                                ui.painter().set(
                                    blame_hover_layer,
                                    egui::Shape::rect_filled(
                                        highlight,
                                        0.0,
                                        if ui.visuals().dark_mode {
                                            BLAME_HOVER
                                        } else {
                                            Color32::from_rgb(223, 234, 249)
                                        },
                                    ),
                                );
                            }
                            let rect = egui::Rect::from_min_size(
                                egui::pos2(gutter.left(), y),
                                egui::vec2(BLAME_WIDTH, row.size.y),
                            );
                            if let Some(blame) = data.and_then(|data| data.blame.get(index)) {
                                let start = previous_blame.is_none_or(|previous| {
                                    previous.commit != blame.commit
                                        || previous.author != blame.author
                                        || previous.date != blame.date
                                });
                                let clipped_start =
                                    start && rect.top() < ui.clip_rect().top() - 0.5;
                                group_row = if start { 0 } else { group_row + 1 };
                                let accent = blame_accent(blame);
                                let response = ui.interact(
                                    rect,
                                    id.with(("blame", index)),
                                    egui::Sense::click(),
                                );
                                ui.painter().rect_filled(
                                    rect,
                                    0.0,
                                    if response.hovered() {
                                        chrome::HEADER
                                    } else if ui.visuals().dark_mode {
                                        Color32::from_rgb(31, 35, 42)
                                    } else {
                                        Color32::from_rgb(244, 246, 249)
                                    },
                                );
                                ui.painter().line_segment(
                                    [rect.right_top(), rect.right_bottom()],
                                    egui::Stroke::new(1.0_f32, chrome::BORDER),
                                );
                                ui.painter().line_segment(
                                    [
                                        rect.left_top()
                                            + egui::vec2(5.0, if start { 4.0 } else { 0.0 }),
                                        rect.left_bottom() + egui::vec2(5.0, 0.0),
                                    ],
                                    egui::Stroke::new(2.0_f32, accent.gamma_multiply(0.65)),
                                );
                                if start && !clipped_start {
                                    if previous_blame.is_some() {
                                        ui.painter().line_segment(
                                            [
                                                rect.left_top() + egui::vec2(12.0, 0.0),
                                                rect.right_top() - egui::vec2(4.0, 0.0),
                                            ],
                                            egui::Stroke::new(1.0_f32, chrome::BORDER),
                                        );
                                    }
                                    let date = if blame.local() {
                                        String::new()
                                    } else {
                                        blame_date(&blame.date)
                                    };
                                    let date_width = ui.fonts_mut(|fonts| {
                                        fonts
                                            .layout_no_wrap(
                                                date.clone(),
                                                FontId::proportional(10.0),
                                                chrome::MUTED,
                                            )
                                            .size()
                                            .x
                                    });
                                    let label = if blame.local() {
                                        "Uncommitted"
                                    } else {
                                        &blame.author
                                    };
                                    let mut job = egui::text::LayoutJob::simple_singleline(
                                        label.to_owned(),
                                        FontId::proportional(11.5),
                                        if blame.local() {
                                            chrome::WARNING
                                        } else {
                                            chrome::TEXT
                                        },
                                    );
                                    job.wrap.max_width =
                                        (rect.width() - date_width - 25.0).max(20.0);
                                    job.wrap.max_rows = 1;
                                    job.wrap.break_anywhere = true;
                                    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                                    ui.painter().with_clip_rect(rect).galley(
                                        egui::pos2(
                                            rect.left() + 13.0,
                                            rect.center().y - galley.size().y / 2.0,
                                        ),
                                        galley,
                                        chrome::TEXT,
                                    );
                                    ui.painter().text(
                                        rect.right_center() - egui::vec2(7.0, 0.0),
                                        egui::Align2::RIGHT_CENTER,
                                        date,
                                        FontId::proportional(10.0),
                                        chrome::MUTED,
                                    );
                                } else if group_row == 1 {
                                    let summary = if blame.local() {
                                        "Changes in this editor"
                                    } else {
                                        &blame.summary
                                    };
                                    let mut job = egui::text::LayoutJob::simple_singleline(
                                        summary.to_owned(),
                                        FontId::proportional(10.5),
                                        chrome::MUTED,
                                    );
                                    job.wrap.max_width = rect.width() - 20.0;
                                    job.wrap.max_rows = 1;
                                    job.wrap.break_anywhere = true;
                                    let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                                    ui.painter().with_clip_rect(rect).galley(
                                        egui::pos2(
                                            rect.left() + 13.0,
                                            rect.center().y - galley.size().y / 2.0,
                                        ),
                                        galley,
                                        chrome::MUTED,
                                    );
                                }
                                previous_blame = if clipped_start { None } else { Some(blame) };
                                if response
                                    .on_hover_text(format!(
                                        "{} · {}\n{}\n{}\nClick for commit and PR details",
                                        blame.author, blame.date, blame.summary, blame.commit
                                    ))
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .clicked()
                                {
                                    clicked_blame = Some(blame.clone());
                                }
                            } else {
                                previous_blame = None;
                                if index == 0 {
                                    ui.painter().text(
                                        rect.left_center() + egui::vec2(10.0, 0.0),
                                        egui::Align2::LEFT_CENTER,
                                        if data.is_some() {
                                            "Blame unavailable"
                                        } else {
                                            "Loading blame…"
                                        },
                                        FontId::proportional(11.0),
                                        chrome::MUTED,
                                    );
                                }
                            }
                        }
                        ui.painter().text(
                            egui::pos2(gutter.right() - 26.0, y),
                            egui::Align2::RIGHT_TOP,
                            (index + 1).to_string(),
                            FontId::monospace(font_size),
                            Color32::from_gray(128),
                        );
                    }
                }
                let separator_x = output.galley_pos.x - 10.0;
                guides.push(egui::Shape::line_segment(
                    [
                        egui::pos2(separator_x, ui.clip_rect().top()),
                        egui::pos2(separator_x, ui.clip_rect().bottom()),
                    ],
                    egui::Stroke::new(1.0_f32, guide_color),
                ));
                let margin_x = output.galley_pos.x
                    + 120.0
                        * ui.fonts_mut(|fonts| {
                            fonts.glyph_width(&FontId::monospace(font_size), 'M')
                        });
                guides.push(egui::Shape::line_segment(
                    [
                        egui::pos2(margin_x, ui.clip_rect().top()),
                        egui::pos2(margin_x, ui.clip_rect().bottom()),
                    ],
                    egui::Stroke::new(1.0_f32, guide_color),
                ));
                ui.painter().set(guide_layer, egui::Shape::Vec(guides));
                ui.painter().set(git_layer, egui::Shape::Vec(git_shapes));
                if jump_offset.is_some() || find_offset.is_some() {
                    let caret = output
                        .galley
                        .pos_from_cursor(egui::text::CCursor::new(*cursor));
                    ui.scroll_to_rect(
                        caret.translate(output.galley_pos.to_vec2()),
                        Some(Align::Center),
                    );
                }
                changed = output.response.changed();
            });
        });
    folds.sync(text);
    ui.ctx().data_mut(|data| data.insert_temp(fold_id, folds));
    (changed, clicked_token, clicked_blame)
}

/// Hit-test glyph cells rather than rounded carets, so whitespace and line ends never link.
fn hovered_token(
    text: &str,
    galley: &egui::Galley,
    position: egui::Vec2,
) -> Option<(std::ops::Range<usize>, String)> {
    let mut offset = 0;
    for row in &galley.rows {
        if position.y >= row.pos.y && position.y < row.pos.y + row.size.y {
            for (column, glyph) in row.glyphs.iter().enumerate() {
                let x = row.pos.x + glyph.pos.x;
                if position.x >= x && position.x < x + glyph.advance_width {
                    let index = offset + column;
                    let token = navigation::token_at(text, index)?;
                    let start = index
                        - text.chars().collect::<Vec<_>>()[..index]
                            .iter()
                            .copied()
                            .rev()
                            .take_while(|c| c.is_alphanumeric() || *c == '_')
                            .count();
                    return Some((start..start + token.chars().count(), token));
                }
            }
            return None;
        }
        offset += row.char_count_including_newline();
    }
    None
}

fn edit_line(text: &mut String, cursor: usize, path: &Path, duplicate: bool) {
    let byte = text
        .char_indices()
        .nth(cursor)
        .map_or(text.len(), |(byte, _)| byte);
    let start = text[..byte].rfind('\n').map_or(0, |i| i + 1);
    let end = text[byte..].find('\n').map_or(text.len(), |i| byte + i);
    if duplicate {
        let line = text[start..end].to_owned();
        text.insert_str(end, &format!("\n{line}"));
    } else {
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let marker = match extension {
            "py" | "sh" | "bash" | "toml" | "yaml" | "yml" | "rb" => "#",
            _ => "//",
        };
        let indent = text[start..end].len() - text[start..end].trim_start().len();
        let point = start + indent;
        if text[point..end].starts_with(marker) {
            let mut finish = point + marker.len();
            if text[finish..end].starts_with(' ') {
                finish += 1;
            }
            text.replace_range(point..finish, "");
        } else {
            text.insert_str(point, &format!("{marker} "));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_view::{extension, result_visible};
    #[test]
    fn file_toolbar_fits_narrow_views_and_keeps_reload_and_blame_actions_separate() {
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let frame = |width, can_reload, events| {
            let mut actions = FileToolbarActions::default();
            let mut bounds = egui::Rect::NOTHING;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(width, 240.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        bounds = ui.available_rect_before_wrap();
                        actions = file_toolbar(
                            ui,
                            Path::new("/project/hack/apidiff-changelog/main.go"),
                            Path::new("/project"),
                            FileToolbarState {
                                dirty: false,
                                can_reload,
                                git: true,
                                blame: false,
                                blame_note: None,
                            },
                        );
                        assert!(
                            ui.min_rect().right() <= bounds.right() + 1.0,
                            "toolbar must fit at {width}"
                        );
                        assert!(
                            ui.cursor().top() - bounds.top() <= 42.0,
                            "toolbar must stay compact: {:?}",
                            ui.cursor()
                        );
                    });
                },
            );
            (output, actions, bounds)
        };
        for width in [280.0, 500.0, 1000.0] {
            frame(width, false, vec![]);
            let (output, _, _) = frame(width, false, vec![]);
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text().ends_with("main.go"))));
        }
        frame(1000.0, false, vec![]);
        let (output, _, bounds) = frame(1000.0, false, vec![]);
        let point = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Blame" => {
                    Some(text.pos + text.galley.size() / 2.0)
                }
                _ => None,
            })
            .unwrap();
        for pressed in [true, false] {
            let (_, actions, _) = frame(
                1000.0,
                false,
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
            if !pressed {
                assert!(actions.blame);
                assert!(!actions.diff && !actions.reload);
            }
        }
        let point = egui::pos2(bounds.right() - 24.0, bounds.top() + 17.0);
        for enabled in [false, true] {
            frame(1000.0, enabled, vec![]);
            frame(1000.0, enabled, vec![]);
            for pressed in [true, false] {
                let (_, actions, _) = frame(
                    1000.0,
                    enabled,
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
                if !pressed {
                    assert_eq!(actions.reload, enabled);
                    assert!(!actions.diff && !actions.blame);
                }
            }
        }
    }

    #[test]
    fn blame_groups_repeated_commits_and_restores_context_when_scrolled() {
        let ctx = egui::Context::default();
        let source = (0..70)
            .map(|index| format!("// source line {index}\n"))
            .collect::<String>();
        let entry = crate::git::BlameLine {
            commit: "a".repeat(40),
            author: "Test Author".into(),
            date: "2026-05-12".into(),
            summary: "Initial version".into(),
        };
        let data = crate::git::FileData {
            path: "sample.rs".into(),
            base: source.clone(),
            text: source.clone(),
            marks: vec![],
            blame: vec![entry; 71],
            blame_note: None,
        };
        let mut text = source.clone();
        let mut cursor = 0;
        let mut jump = None;
        let mut editor_id = None;
        let mut time = 0.0;
        let mut frame = |target, events| {
            jump = target;
            time += 0.1;
            let mut clicked = None;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 300.0),
                    )),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        clicked = text_editor(
                            ui,
                            &mut text,
                            Path::new("sample.rs"),
                            &mut cursor,
                            &mut jump,
                            &mut editor_id,
                            EditorFind {
                                git: Some(&data),
                                blame: true,
                                ..Default::default()
                            },
                        )
                        .2;
                    });
                },
            );
            (output, clicked, editor_id.unwrap())
        };
        frame(None, vec![]);
        let (output, _, id) = frame(None, vec![]);
        let count = |output: &egui::FullOutput, needle| {
            output.shapes.iter().filter(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == needle)).count()
        };
        assert_eq!(count(&output, "Test Author"), 1);
        assert_eq!(count(&output, "Initial version"), 1);
        let point = ctx
            .read_response(id.with(("blame", 4_usize)))
            .unwrap()
            .rect
            .center();
        for pressed in [true, false] {
            let (_, clicked, _) = frame(
                None,
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
            if !pressed {
                assert_eq!(clicked.unwrap().commit, "a".repeat(40));
            }
        }
        frame(Some((50, 1)), vec![]);
        let mut output = None;
        for _ in 0..12 {
            output = Some(frame(None, vec![]).0);
        }
        let output = output.unwrap();
        assert_eq!(
            count(&output, "Test Author"),
            1,
            "scrolled commit continuation must show its author again"
        );
        assert!(
            output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == "Test Author" =>
                        Some(text.pos.y + 0.5 >= shape.clip_rect.top()),
                    _ => None,
                })
                .all(|visible| visible),
            "the visible commit heading must not be clipped by the top of the editor"
        );
        assert!(
            ctx.read_response(id.with(("blame", 0_usize))).is_none(),
            "the original group heading must be out of view"
        );
        assert_eq!(text, source);
    }

    #[test]
    fn explorer_actions_preserve_buffers_and_do_not_overwrite_files() -> anyhow::Result<()> {
        use crate::explorer::FileAction;
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let folder = root.join("folder");
        std::fs::create_dir(&folder)?;
        let mut app = Coder::empty(root.clone());
        app.file_action_name = "new.rs".into();
        app.apply_file_action(FileAction::NewFile(folder.clone()))?;
        let path = folder.join("new.rs");
        assert_eq!(app.documents.len(), 1);
        app.documents[0].content = Content::Text("unsaved edits".into());
        app.documents[0].dirty = true;
        app.file_action_name = "renamed.rs".into();
        app.apply_file_action(FileAction::Rename(path.clone()))?;
        let renamed = folder.join("renamed.rs");
        assert!(!path.exists());
        assert_eq!(app.documents[0].path, renamed);
        app.documents[0].save()?;
        assert_eq!(std::fs::read_to_string(&renamed)?, "unsaved edits");
        assert!(
            app.apply_file_action(FileAction::NewFile(folder.clone()))
                .is_err()
        );
        app.file_action_name = "moved".into();
        app.apply_file_action(FileAction::Rename(folder))?;
        let moved = root.join("moved/renamed.rs");
        assert_eq!(app.documents[0].path, moved);
        for name in ["../outside", "", ".", "/absolute"] {
            app.file_action_name = name.into();
            assert!(
                app.apply_file_action(FileAction::NewFile(root.clone()))
                    .is_err()
            );
        }
        app.apply_file_action(FileAction::Delete(moved.clone()))?;
        assert!(!moved.exists());
        assert!(app.documents.is_empty());
        Ok(())
    }

    #[test]
    fn all_panel_borders_keep_idle_and_drag_cursors_and_yield_to_dialogs() -> anyhow::Result<()> {
        use egui::{CursorIcon as C, Event as E};
        let dir = tempfile::tempdir()?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        app.terminal.settings.shell = "/bin/sh".into();
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        app.terminal.new_tab(dir.path(), &ctx);
        let frame = |app: &mut Coder, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1400.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            )
        };
        for panel in [
            "explorer",
            "structure",
            "git-changes-panel",
            "terminal-panel",
            "search-results",
        ] {
            app.show_structure = panel == "structure";
            app.git.visible = panel == "git-changes-panel";
            app.search_open = panel == "search-results";
            app.search_docked = true;
            for _ in 0..4 {
                frame(&mut app, vec![]);
            }
            let id = egui::Id::new(panel).with("__resize");
            let handle = ctx.read_response(id).unwrap().rect;
            let horizontal = matches!(panel, "explorer" | "structure" | "git-changes-panel");
            assert!(
                (if horizontal {
                    handle.width()
                } else {
                    handle.height()
                }) >= 16.0
            );
            let cursor = if horizontal {
                C::ResizeHorizontal
            } else {
                C::ResizeVertical
            };
            for delta in [-7.0, -3.0, 0.0, 3.0, 7.0] {
                let point = handle.center()
                    + if horizontal {
                        egui::vec2(delta, 0.0)
                    } else {
                        egui::vec2(0.0, delta)
                    };
                frame(&mut app, vec![E::PointerMoved(point)]);
                for _ in 0..5 {
                    assert_eq!(
                        frame(&mut app, vec![]).platform_output.cursor_icon,
                        cursor,
                        "idle {panel} at {delta}"
                    );
                }
            }
            let point = handle.center();
            frame(
                &mut app,
                vec![
                    E::PointerMoved(point),
                    E::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            let moved = point
                + if horizontal {
                    egui::vec2(35.0, 0.0)
                } else {
                    egui::vec2(0.0, -35.0)
                };
            assert_eq!(
                frame(&mut app, vec![E::PointerMoved(moved)])
                    .platform_output
                    .cursor_icon,
                cursor,
                "drag {panel}"
            );
            assert_eq!(frame(&mut app, vec![]).platform_output.cursor_icon, cursor);
            frame(
                &mut app,
                vec![E::PointerButton {
                    pos: moved,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
            let resized = ctx.read_response(id).unwrap().rect;
            assert!(
                (resized.center() - handle.center()).length() > 10.0,
                "{panel} must actually resize"
            );
            app.error = Some("A dialog is open".into());
            frame(&mut app, vec![E::PointerMoved(resized.center())]);
            for _ in 0..6 {
                assert!(
                    !chrome::is_resize_cursor(frame(&mut app, vec![]).platform_output.cursor_icon),
                    "background border through modal: {panel}"
                );
            }
            app.error = None;
            for _ in 0..3 {
                frame(&mut app, vec![]);
            }
        }
        Ok(())
    }

    #[test]
    fn floating_dialog_edges_and_corners_keep_resize_cursors_over_panels() -> anyhow::Result<()> {
        use egui::{CursorIcon as C, Event as E};
        let dir = tempfile::tempdir()?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        app.show_structure = true;
        app.search_open = true;
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let frame = |app: &mut Coder, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1400.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            )
        };
        for _ in 0..6 {
            frame(&mut app, vec![]);
        }
        let layer = egui::LayerId::new(egui::Order::Middle, egui::Id::new("find-in-files"));
        let base = egui::Id::new(layer).with("edge_drag");
        for (edge, expected) in [
            ("left", C::ResizeHorizontal),
            ("right", C::ResizeHorizontal),
            ("top", C::ResizeVertical),
            ("bottom", C::ResizeVertical),
            ("left_top", C::ResizeNwSe),
            ("right_bottom", C::ResizeNwSe),
            ("right_top", C::ResizeNeSw),
            ("left_bottom", C::ResizeNeSw),
        ] {
            let point = ctx.read_response(base.with(edge)).unwrap().rect.center();
            frame(&mut app, vec![E::PointerMoved(point)]);
            for _ in 0..8 {
                assert_eq!(
                    frame(&mut app, vec![]).platform_output.cursor_icon,
                    expected,
                    "floating {edge}"
                );
            }
        }
        let panel_point = ctx
            .read_response(egui::Id::new("structure").with("__resize"))
            .unwrap()
            .rect
            .center();
        assert!(
            ctx.memory(|m| m.area_rect(layer.id))
                .unwrap()
                .contains(panel_point)
        );
        frame(&mut app, vec![E::PointerMoved(panel_point)]);
        assert!(
            !chrome::is_resize_cursor(frame(&mut app, vec![]).platform_output.cursor_icon),
            "floating dialog content must cover the panel border"
        );
        let before = ctx.memory(|m| m.area_rect(layer.id)).unwrap();
        let point = ctx.read_response(base.with("right")).unwrap().rect.center();
        frame(
            &mut app,
            vec![
                E::PointerMoved(point),
                E::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let moved = point - egui::vec2(60.0, 0.0);
        assert_eq!(
            frame(&mut app, vec![E::PointerMoved(moved)])
                .platform_output
                .cursor_icon,
            C::ResizeHorizontal
        );
        frame(
            &mut app,
            vec![E::PointerButton {
                pos: moved,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        for _ in 0..3 {
            frame(&mut app, vec![]);
        }
        let after = ctx.memory(|m| m.area_rect(layer.id)).unwrap();
        assert!(
            before.width() - after.width() > 40.0,
            "dialog content must not force its old width: {before:?} -> {after:?}"
        );
        for _ in 0..8 {
            frame(&mut app, vec![]);
            assert_eq!(
                ctx.memory(|m| m.area_rect(layer.id)).unwrap(),
                after,
                "idle window geometry must not oscillate after resizing"
            );
        }
        app.error = Some("Nested confirmation".into());
        let point = ctx.read_response(base.with("left")).unwrap().rect.center();
        frame(&mut app, vec![E::PointerMoved(point)]);
        for _ in 0..6 {
            assert!(!chrome::is_resize_cursor(
                frame(&mut app, vec![]).platform_output.cursor_icon
            ));
        }
        Ok(())
    }

    #[test]
    fn panel_resize_cursor_stays_stable() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        for x in 320..335 {
            let mut icons = vec![];
            for frame in 0..8 {
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1400.0, 900.0),
                        )),
                        events: vec![egui::Event::PointerMoved(egui::pos2(x as f32, 300.0))],
                        ..Default::default()
                    },
                    |ctx| app.draw(ctx),
                );
                if frame > 2 {
                    let resize = ctx
                        .read_response(egui::Id::new("explorer").with("__resize"))
                        .unwrap();
                    if resize.rect.contains(egui::pos2(x as f32, 300.0)) {
                        assert_eq!(
                            output.platform_output.cursor_icon,
                            egui::CursorIcon::ResizeHorizontal
                        );
                    }
                    icons.push(output.platform_output.cursor_icon);
                }
            }
            assert!(
                icons.windows(2).all(|pair| pair[0] == pair[1]),
                "x={x}: {icons:?}"
            );
        }
        app.terminal.settings.shell = "/bin/sh".into();
        app.terminal.new_tab(dir.path(), &ctx);
        let frame = |app: &mut Coder, pointer| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1400.0, 900.0),
                    )),
                    events: vec![egui::Event::PointerMoved(pointer)],
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            )
        };
        for _ in 0..3 {
            frame(&mut app, egui::pos2(700.0, 400.0));
        }
        let handle = ctx
            .read_response(egui::Id::new("terminal-panel").with("__resize"))
            .unwrap()
            .rect;
        for delta in [-2.0, 0.0, 2.0] {
            let pointer = handle.center() + egui::vec2(0.0, delta);
            for _ in 0..8 {
                let output = frame(&mut app, pointer);
                assert_eq!(
                    output.platform_output.cursor_icon,
                    egui::CursorIcon::ResizeVertical
                );
            }
        }
        Ok(())
    }

    #[test]
    fn scope_buttons_toggle_and_opened_includes_git_tree_files() -> anyhow::Result<()> {
        use crate::search_view::Scope;
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        for name in ["open.rs", "changed.rs", "other.rs"] {
            std::fs::write(root.join(name), "fn needle() {}\n")?;
        }
        let mut app = Coder::empty(root.clone());
        app.open(root.join("open.rs"), None);
        app.git.repo = Some(crate::git::Repository {
            root: root.clone(),
            branch: "feature".into(),
            head: "a".repeat(40),
            base: "main".into(),
            merge_base: None,
            branches: vec![],
            remote: None,
            files: vec![
                crate::git::ChangedFile {
                    path: "changed.rs".into(),
                    committed: true,
                    ..Default::default()
                },
                crate::git::ChangedFile {
                    path: "open.rs".into(),
                    uncommitted: true,
                    ..Default::default()
                },
            ],
        });
        assert_eq!(
            app.opened_search_paths(),
            vec![root.join("open.rs"), root.join("changed.rs")]
        );
        app.results = ["open.rs", "changed.rs", "other.rs"]
            .into_iter()
            .map(|name| Location {
                path: root.join(name),
                line: 1,
                column: 4,
                preview: "fn needle() {}".into(),
                category: navigation::MatchCategory::Declaration,
            })
            .collect();
        app.query = "needle".into();
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let mut time = 0.0;
        let mut frame = |app: &mut Coder, events| {
            time += 0.1;
            ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.search_contents(ui, true));
                },
            )
        };
        frame(&mut app, vec![]);
        let output = frame(&mut app, vec![]);
        let position = |label: &str| {
            output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(t) if t.galley.text() == label => {
                        Some(t.pos + t.galley.size() * 0.5)
                    }
                    _ => None,
                })
                .expect("Scope button")
        };
        let mut click = |app: &mut Coder, point| {
            for pressed in [true, false] {
                frame(
                    app,
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
        };
        click(&mut app, position("Opened"));
        assert_eq!(app.result_scope, Scope::Opened);
        assert_eq!(app.search_view.indices, vec![0, 1]);
        click(&mut app, position("Current"));
        assert_eq!(app.result_scope, Scope::Current);
        assert_eq!(app.search_view.indices, vec![0]);
        click(&mut app, position("Current"));
        assert_eq!(app.result_scope, Scope::Project);
        assert_eq!(app.search_view.indices, vec![0, 1, 2]);
        click(&mut app, position("Opened"));
        click(&mut app, position("Opened"));
        assert_eq!(app.result_scope, Scope::Project);
        assert_eq!(app.search_view.indices, vec![0, 1, 2]);
        Ok(())
    }

    #[test]
    fn file_queries_accept_line_and_column_suffixes() {
        assert_eq!(
            split_file_location("abcd.txt:23"),
            ("abcd.txt", Some((23, 1)))
        );
        assert_eq!(
            split_file_location("abcd.txt:23:24"),
            ("abcd.txt", Some((23, 24)))
        );
        assert_eq!(split_file_location("猫.rs:2:3"), ("猫.rs", Some((2, 3))));
        assert_eq!(
            split_file_location("C:\\code\\a.rs:8"),
            ("C:\\code\\a.rs", Some((8, 1)))
        );
        assert_eq!(split_file_location("file:word"), ("file:word", None));
    }
    #[test]
    fn file_search_prioritizes_recent_choices_without_duplicates() {
        let root = Path::new("/project");
        let a = root.join("a.rs");
        let b = root.join("b.rs");
        let c = root.join("c.rs");
        assert_eq!(
            file_candidates(
                std::slice::from_ref(&b),
                std::slice::from_ref(&a),
                &[a.clone(), b.clone(), c.clone()],
                root,
                ""
            ),
            vec![b, a, c]
        );
    }
    #[test]
    fn category_toggles_are_mutually_exclusive_and_all_restores_results() {
        use navigation::MatchCategory;
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let mut selected = None;
        let mut frame = |events: Vec<egui::Event>| {
            let output = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            for (label, category) in [
                                ("All", None),
                                ("Declarations", Some(MatchCategory::Declaration)),
                                ("Usages", Some(MatchCategory::Usage)),
                            ] {
                                category_toggle(ui, &mut selected, category, label.into());
                            }
                        });
                    });
                },
            );
            (output, selected)
        };
        let (output, selected) = frame(vec![]);
        assert_eq!(selected, None);
        let positions: Vec<_> = ["Declarations", "Usages", "All"]
            .iter()
            .map(|label| {
                output
                    .shapes
                    .iter()
                    .find_map(|shape| {
                        if let egui::Shape::Text(text) = &shape.shape
                            && text.galley.text() == *label
                        {
                            Some(text.pos + text.galley.size() / 2.0)
                        } else {
                            None
                        }
                    })
                    .unwrap()
            })
            .collect();
        for (point, expected) in positions.into_iter().zip([
            Some(MatchCategory::Declaration),
            Some(MatchCategory::Usage),
            None,
        ]) {
            for pressed in [true, false] {
                let (_, actual) = frame(vec![
                    egui::Event::PointerMoved(point),
                    egui::Event::PointerButton {
                        pos: point,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]);
                if !pressed {
                    assert_eq!(actual, expected);
                }
            }
        }
    }

    #[test]
    fn styled_search_windows_render_preview_and_file_picker() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("sample.rs");
        std::fs::write(&path, "fn sample() {}\n")?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        app.open(path.clone(), None);
        app.files = vec![path.clone()];
        app.tree = crate::explorer::FileTree::build(&app.root, &app.files);
        app.query = "sample".into();
        app.results = vec![Location {
            path,
            line: 1,
            column: 4,
            category: navigation::MatchCategory::Declaration,
            preview: "fn sample() {}".into(),
        }];
        app.search_note = "1 match".into();
        app.search_open = true;
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        for mode in [0, 1, 2] {
            if mode == 1 {
                app.search_docked = true;
            }
            if mode == 2 {
                app.show_picker(Picker::Files);
            }
            let mut output = None;
            for _ in 0..4 {
                output = Some(ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1400.0, 900.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| app.draw(ctx),
                ));
            }
            let output = output.unwrap();
            let labels: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|s| {
                    if let egui::Shape::Text(text) = &s.shape {
                        Some(text.galley.text().to_owned())
                    } else {
                        None
                    }
                })
                .collect();
            assert!(labels.iter().any(|label| label
                == if mode == 2 {
                    "Go to file"
                } else {
                    "Find in Files"
                }));
            assert!(app.error.is_none());
            if mode == 0 {
                let before = ctx
                    .memory(|memory| memory.area_rect(egui::Id::new("find-in-files")))
                    .unwrap();
                let saved = std::mem::take(&mut app.results);
                for _ in 0..4 {
                    let _ = ctx.run(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(1400.0, 900.0),
                            )),
                            ..Default::default()
                        },
                        |ctx| app.draw(ctx),
                    );
                }
                let after = ctx
                    .memory(|memory| memory.area_rect(egui::Id::new("find-in-files")))
                    .unwrap();
                assert!(
                    (before.height() - after.height()).abs() < 2.0,
                    "empty results must not shrink the search window: {before:?} -> {after:?}"
                );
                assert!(
                    after.height() <= 900.0,
                    "search window must fit the viewport"
                );
                app.results = saved;
            }
        }
        Ok(())
    }
    #[test]
    fn category_and_extension_filters_combine() {
        let result = Location {
            path: PathBuf::from("a.RS"),
            line: 1,
            column: 1,
            preview: String::new(),
            category: navigation::MatchCategory::String,
        };
        assert!(result_visible(&result, None, Some(".rs")));
        assert!(!result_visible(&result, None, Some(".py")));
        assert!(!result_visible(
            &result,
            Some(navigation::MatchCategory::Usage),
            None
        ));
        assert_eq!(extension(Path::new("Makefile")), "(no extension)");
    }

    #[test]
    fn command_click_uses_pointer_token_rather_than_previous_caret() {
        let ctx = egui::Context::default();
        let mut source = "fn old() {}\nfn needle() {}".to_owned();
        let mut cursor = 3;
        let mut jump = None;
        let mut editor_id = None;
        let mut time = 0.0;
        let mut frame = |events: Vec<egui::Event>, modifiers: egui::Modifiers| {
            time += 0.1;
            let mut token = None;
            let output = ctx.run(
                egui::RawInput {
                    time: Some(time),
                    modifiers,
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 700.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        token = text_editor(
                            ui,
                            &mut source,
                            Path::new("a.rs"),
                            &mut cursor,
                            &mut jump,
                            &mut editor_id,
                            EditorFind::default(),
                        )
                        .1;
                    });
                },
            );
            (output, token)
        };
        frame(vec![], egui::Modifiers::NONE);
        let (output, _) = frame(vec![], egui::Modifiers::NONE);
        let point = output
            .shapes
            .iter()
            .find_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape
                    && text.galley.text().contains("needle")
                {
                    let rect = text.galley.pos_from_cursor(egui::text::CCursor::new(17));
                    Some(text.pos + egui::vec2(rect.left() + 1.0, rect.center().y))
                } else {
                    None
                }
            })
            .expect("source text rendered");
        let modifiers = egui::Modifiers::COMMAND;
        let (hover, token) = frame(vec![egui::Event::PointerMoved(point)], modifiers);
        assert_eq!(
            hover.platform_output.cursor_icon,
            egui::CursorIcon::PointingHand
        );
        assert!(token.is_none(), "hover alone must not start a search");
        let (plain, _) = frame(vec![], egui::Modifiers::NONE);
        assert_ne!(
            plain.platform_output.cursor_icon,
            egui::CursorIcon::PointingHand
        );
        frame(
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers,
                },
            ],
            modifiers,
        );
        let (_, token) = frame(
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers,
                },
            ],
            modifiers,
        );
        assert_eq!(token.as_deref(), Some("needle"));
    }
    #[test]
    fn markdown_preview_uses_heading_sizes_and_inert_html_images() {
        let images = html_images(
            r#"<img width='100' alt="K8s" src="logo.png"><img src='https://example.org/a?x=1&amp;y=2' width=80>"#,
        );
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].src, "logo.png");
        assert_eq!(images[0].width, Some(100.0));
        assert_eq!(images[1].src, "https://example.org/a?x=1&y=2");
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let mut cache = CommonMarkCache::default();
        let output = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                markdown_preview(
                    ui,
                    &mut cache,
                    "# Large heading\n\nBody [link](https://example.org)",
                    Path::new("/tmp/README.md"),
                );
            });
        });
        let heading = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(t) if t.galley.text() == "Large heading" => Some(t),
                _ => None,
            })
            .expect("heading rendered");
        assert!(heading.galley.job.sections[0].format.font_id.size >= 30.0);
        assert_eq!(
            ctx.style().text_styles[&egui::TextStyle::Body].size,
            13.0,
            "preview styles stay local"
        );
    }

    #[test]
    fn token_hit_testing_covers_glyphs_and_rejects_whitespace_and_line_ends() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |ctx| {
            let source = "old café()\n猫 next";
            let galley = ctx.fonts_mut(|fonts| {
                fonts.layout_no_wrap(source.into(), FontId::monospace(13.0), chrome::TEXT)
            });
            for (index, expected) in [
                (4, Some("café")),
                (7, Some("café")),
                (3, None),
                (8, None),
                (11, Some("猫")),
            ] {
                let start = galley.pos_from_cursor(egui::text::CCursor::new(index));
                let point = egui::vec2(start.left() + 1.0, start.center().y);
                assert_eq!(
                    hovered_token(source, &galley, point)
                        .map(|(_, word)| word)
                        .as_deref(),
                    expected,
                    "character {index}"
                );
            }
            assert!(
                hovered_token(source, &galley, egui::vec2(galley.size().x + 40.0, 5.0)).is_none()
            );
        });
    }

    #[test]
    fn file_find_arrows_wrap_matches_and_keep_search_focus() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("sample.rs");
        let source = "// 猫 apps\n".to_owned() + &"// filler\n".repeat(80) + "// apps\n";
        std::fs::write(&path, &source)?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        app.open(path, None);
        app.find = "apps".into();
        app.find_open = true;
        app.find_focus = true;
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let mut time = 0.0;
        let mut frame = |app: &mut Coder, events| {
            time += 0.1;
            ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 700.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            )
        };
        frame(&mut app, vec![]);
        frame(&mut app, vec![]);
        let focus = ctx.memory(|m| m.focused());
        assert!(focus.is_some());
        assert_ne!(focus, app.documents[0].editor_id);
        assert_eq!(app.file_find.counter(), "1/2");
        let key = |key| {
            [true, false]
                .into_iter()
                .map(|pressed| egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                })
                .collect()
        };
        frame(&mut app, key(Key::ArrowDown));
        assert_eq!(app.file_find.counter(), "2/2");
        let target = source[..source.rfind("apps").unwrap()].chars().count();
        assert_eq!(app.documents[0].cursor, target);
        assert_eq!(ctx.memory(|m| m.focused()), focus);
        frame(&mut app, key(Key::ArrowDown));
        assert_eq!(app.file_find.counter(), "1/2");
        frame(&mut app, key(Key::ArrowUp));
        assert_eq!(app.file_find.counter(), "2/2");
        assert_eq!(ctx.memory(|m| m.focused()), focus);
        // Editing the field after navigation must not insert into the source file.
        frame(&mut app, vec![egui::Event::Text("z".into())]);
        assert_eq!(app.file_find.counter(), "0/0");
        if let Content::Text(text) = &app.documents[0].content {
            assert_eq!(text, &source);
        }
        frame(&mut app, key(Key::Escape));
        assert!(!app.find_open);
        assert_eq!(ctx.memory(|m| m.focused()), app.documents[0].editor_id);
        Ok(())
    }

    #[test]
    fn switching_projects_closes_old_tabs_and_resets_navigation_but_protects_edits()
    -> anyhow::Result<()> {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let file = first.path().join("main.rs");
        std::fs::write(&file, "fn main() {}")?;
        let mut app = Coder::empty(first.path().to_path_buf());
        app.open(file, None);
        app.documents[0].dirty = true;
        app.request_project(second.path().to_path_buf());
        assert!(matches!(app.pending, Some(Pending::SwitchProject(_))));
        assert_eq!(app.documents.len(), 1);
        assert_eq!(app.root, first.path());
        // Cancel leaves the original project intact; discarding explicitly permits the switch.
        app.pending = None;
        app.documents[0].dirty = false;
        app.find_open = true;
        app.query = "main".into();
        app.history.push(Location {
            path: app.documents[0].path.clone(),
            line: 1,
            column: 1,
            preview: String::new(),
            category: navigation::MatchCategory::Usage,
        });
        app.request_project(second.path().to_path_buf());
        assert_eq!(app.root, second.path().canonicalize()?);
        assert!(app.documents.is_empty());
        assert!(app.results.is_empty());
        assert!(app.history.is_empty());
        assert!(app.query.is_empty());
        assert!(!app.find_open);
        assert_eq!(app.active, 0);
        Ok(())
    }

    #[test]
    fn clicking_blame_annotation_returns_its_commit_without_editing_source() {
        let ctx = egui::Context::default();
        let source = "fn main() {\n    render();\n}\n";
        let mut text = source.to_owned();
        let mut cursor = 0;
        let mut jump = None;
        let mut id = None;
        let entry = crate::git::BlameLine {
            commit: "a".repeat(40),
            author: "Test Author".into(),
            date: "2026-10-17".into(),
            summary: "Implement render".into(),
        };
        let data = crate::git::FileData {
            path: "sample.rs".into(),
            base: String::new(),
            text: source.into(),
            marks: vec![(1, crate::git::Mark::Added)],
            blame: vec![entry.clone(); 3],
            blame_note: None,
        };
        let mut time = 0.0;
        let mut frame = |events| {
            time += 0.1;
            let mut clicked = None;
            let output = ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 700.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        clicked = text_editor(
                            ui,
                            &mut text,
                            Path::new("sample.rs"),
                            &mut cursor,
                            &mut jump,
                            &mut id,
                            EditorFind {
                                git: Some(&data),
                                blame: true,
                                ..Default::default()
                            },
                        )
                        .2;
                    });
                },
            );
            (output, clicked)
        };
        frame(vec![]);
        let (output, _) = frame(vec![]);
        let point = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text().contains("Test Author") => {
                    Some(text.pos + egui::vec2(6.0, 5.0))
                }
                _ => None,
            })
            .expect("Blame annotation should be visible");
        let (hovered, _) = frame(vec![egui::Event::PointerMoved(point)]);
        assert!(hovered.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Rect(rect) if rect.fill == BLAME_HOVER
                && rect.rect.left() > point.x && rect.rect.top() <= point.y
                && rect.rect.bottom() >= point.y && rect.rect.width() > 100.0)));
        for pressed in [true, false] {
            let (_, clicked) = frame(vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            if !pressed {
                assert_eq!(clicked.unwrap().commit, entry.commit);
            }
        }
        assert_eq!(text, source);
    }

    #[test]
    fn fold_gutter_and_preview_toggle_without_editing_and_jump_reveals_source() {
        let ctx = egui::Context::default();
        let source = "{\n  \"ios\": {\n    \"supportsTablet\": true\n  },\n  \"web\": {}\n}\n";
        let mut text = source.to_owned();
        let mut cursor = 0;
        let mut jump = None;
        let mut id = None;
        let mut time = 0.0;
        let mut frame = |events, target| {
            jump = target;
            time += 0.1;
            let output = ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 700.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        text_editor(
                            ui,
                            &mut text,
                            Path::new("sample.json"),
                            &mut cursor,
                            &mut jump,
                            &mut id,
                            EditorFind::default(),
                        );
                    });
                },
            );
            let key = id.unwrap().with(("folds", Path::new("sample.json")));
            let folded = ctx
                .data_mut(|d| d.get_temp::<crate::folding::State>(key))
                .unwrap()
                .collapsed
                .len();
            (output, folded)
        };
        frame(vec![], None);
        let (output, _) = frame(vec![], None);
        let source_shape = output
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text() == source => Some(t),
                _ => None,
            })
            .unwrap();
        let point = source_shape.pos + egui::vec2(-14.0, source_shape.galley.rows[1].pos.y + 6.0);
        let click = |point, pressed| {
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        frame(click(point, true), None);
        assert_eq!(frame(click(point, false), None).1, 1);
        let (output, folded) = frame(vec![], None);
        assert_eq!(folded, 1, "A fold must remain collapsed after the click");
        let preview = output
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text().contains("lines)") => Some(t),
                _ => None,
            })
            .expect("Collapsed inline preview");
        let point = preview.pos + egui::vec2(5.0, 5.0);
        frame(click(point, true), None);
        assert_eq!(frame(click(point, false), None).1, 0);
        frame(
            click(
                source_shape.pos + egui::vec2(-14.0, source_shape.galley.rows[1].pos.y + 6.0),
                true,
            ),
            None,
        );
        frame(
            click(
                source_shape.pos + egui::vec2(-14.0, source_shape.galley.rows[1].pos.y + 6.0),
                false,
            ),
            None,
        );
        assert_eq!(frame(vec![], Some((3, 5))).1, 0);
        assert_eq!(text, source);
        assert_eq!(cursor, navigation::char_offset(source, 3, 5));
    }

    #[test]
    fn current_line_controls_preserve_unicode_and_indentation() {
        let path = Path::new("a.rs");
        let mut text = "  let café = 1;\n".to_owned();
        edit_line(&mut text, 8, path, false);
        assert_eq!(text, "  // let café = 1;\n");
        edit_line(&mut text, 8, path, false);
        assert_eq!(text, "  let café = 1;\n");
        edit_line(&mut text, 8, path, true);
        assert_eq!(text, "  let café = 1;\n  let café = 1;\n");
    }
    #[test]
    fn csv_source_switch_commits_live_cells_and_retains_invalid_source() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("table.csv");
        std::fs::write(&path, "name,value\nAlice,10\nBob,2\n")?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        app.open(path, None);
        app.show_project = false;
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let mut time = 0.0;
        let mut frame = |app: &mut Coder, events| {
            time += 0.1;
            ctx.run(
                egui::RawInput {
                    time: Some(time),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 800.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            )
        };
        let position = |output: &egui::FullOutput, label: &str| {
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
                .unwrap_or_else(|| panic!("Missing label: {label}"))
        };
        let click = |point| {
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                },
            ]
        };
        frame(&mut app, vec![]);
        let output = frame(&mut app, vec![]);
        frame(&mut app, click(position(&output, "Alice")));
        frame(
            &mut app,
            vec![egui::Event::Key {
                key: Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        frame(&mut app, vec![]);
        let output = frame(&mut app, vec![egui::Event::Text("Z".into())]);
        frame(&mut app, click(position(&output, "Source")));
        assert!(app.documents[0].csv_is_source());
        assert!(app.documents[0].source_text()?.contains('Z'));
        app.documents[0].content = Content::Text("a,b\n1,2,3\n".into());
        let output = frame(&mut app, vec![]);
        frame(&mut app, click(position(&output, "Data")));
        assert!(app.documents[0].csv_is_source());
        assert!(app.documents[0].csv_error.is_some());
        assert_eq!(app.documents[0].source_text()?, "a,b\n1,2,3\n");
        Ok(())
    }

    #[test]
    fn native_views_layout_without_panicking() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        for (name, text) in [
            ("main.rs", "fn main() {}"),
            ("table.csv", "name,value\nAlice,10\nBob,2\n"),
            (
                "README.md",
                "# Heading\n\n- [x] Task\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n~~old~~ **bold**\n",
            ),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, text)?;
            app.open(path, None);
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1400.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            );
            assert!(app.error.is_none());
        }
        Ok(())
    }
}
