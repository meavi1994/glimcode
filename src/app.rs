use crate::markdown::MarkdownCache;
use crate::{
    chrome,
    document::{Content, Document, MarkdownMode},
    navigation::{self, Location, SearchKind, SearchUpdate},
    ranking,
};
use eframe::egui::{self, Align, Color32, FontId, Key, RichText};
use egui_commonmark::CommonMarkViewer;
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
    SwitchWorktree(PathBuf),
    OpenProjectInstance(PathBuf),
}

#[derive(Clone, Copy, PartialEq)]
enum Picker {
    Everywhere,
    Files,
    Symbols,
    Recent,
    Line,
}

/// What a click most likely means, inferred from what was clicked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Intent {
    /// The click landed on a declaration: the user wants to see its usages.
    Usages,
    /// The click landed on a usage or an import: the user wants the
    /// declaration.
    Declarations,
    /// The click landed on prose or an unclassified spot: show every match.
    Everything,
}

impl Intent {
    /// The results tab that answers this intent.
    fn category(self) -> Option<navigation::MatchCategory> {
        match self {
            Intent::Usages => Some(navigation::MatchCategory::Usage),
            Intent::Declarations => Some(navigation::MatchCategory::Declaration),
            Intent::Everything => None,
        }
    }

    /// Infer the intent from the syntactic role of the clicked occurrence.
    fn for_category(category: navigation::MatchCategory) -> Self {
        match category {
            navigation::MatchCategory::Declaration => Intent::Usages,
            navigation::MatchCategory::Usage | navigation::MatchCategory::Import => {
                Intent::Declarations
            }
            _ => Intent::Everything,
        }
    }
}

/// A click that is waiting for its search results.
struct PendingClick {
    token: String,
    intent: Intent,
    /// Present when the click could be analysed; used to rank candidates.
    context: Option<ranking::ClickContext>,
}

/// Ranking of the candidates of a click. It runs on a worker because it reads
/// candidate files; the ordering it produces is deterministic and local.
struct FocusSearch {
    rx: Receiver<Vec<ranking::Ranked>>,
    cancel: Arc<AtomicBool>,
    token: String,
}

#[derive(Clone, Debug)]
struct TextClick {
    char_index: usize,
    token: String,
}

/// How many navigation steps are remembered.
const MAX_HISTORY: usize = 200;
const PROJECT_REFRESH_INTERVAL: Duration = Duration::from_secs(2);

struct WorktreeSession {
    documents: Vec<Document>,
    active: usize,
    terminal: crate::terminal::Terminal,
    history: Vec<Location>,
}

pub struct Coder {
    worktrees: crate::worktrees::Manager,
    session_store: Option<crate::session::Store>,
    worktree_sessions: std::collections::BTreeMap<PathBuf, WorktreeSession>,
    root: PathBuf,
    files: Vec<PathBuf>,
    folders: Vec<PathBuf>,
    tree: crate::explorer::FileTree,
    selected_category: Option<navigation::MatchCategory>,
    extension_filter: Option<String>,
    result_scope: crate::search_view::Scope,
    file_filter: String,
    files_rx: Option<Receiver<anyhow::Result<navigation::WorkspacePaths>>>,
    files_checked: Instant,
    documents: Vec<Document>,
    active: usize,
    query: String,
    search_kind: SearchKind,
    exclude_test_files: bool,
    results: Vec<Location>,
    search_rx: Option<Receiver<SearchUpdate>>,
    search_cancel: Arc<AtomicBool>,
    open_matches: usize,
    results_revision: u64,
    search_view: crate::search_view::SearchView,
    search_note: String,
    cache: MarkdownCache,
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
    show_worktrees: bool,
    show_project: bool,
    files_changed_only: bool,
    show_structure: bool,
    git: crate::git_view::GitTool,
    terminal: crate::terminal::Terminal,
    structure_filter: String,
    show_file_filter: bool,
    search_open: bool,
    search_docked: bool,
    search_split: f32,
    result_selection: usize,
    search_focus: bool,
    preview: crate::preview::PreviewService,
    search_highlight: crate::search_highlight::SearchHighlight,
    /// Set when a dialog closes, so the editor takes the keyboard back.
    focus_editor: bool,
    /// Colors of the changed files, kept until the repository changes.
    git_colors: (u64, Arc<std::collections::BTreeMap<PathBuf, Color32>>),
    goto: Option<FocusSearch>,
    pending_click: Option<PendingClick>,
    pending_reveal: bool,
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
        #[cfg(feature = "ui-capture")]
        let argument = std::env::var_os("GLIMCODE_PREVIEW_FILE")
            .map(PathBuf::from)
            .or(argument);
        let root = argument
            .as_ref()
            .filter(|p| p.is_dir())
            .cloned()
            .or_else(|| {
                argument
                    .as_ref()
                    .and_then(|p| p.parent().map(Path::to_path_buf))
            })
            .or_else(crate::session::last_project)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let mut app = Self::empty(root.canonicalize().unwrap_or(root));
        let capturing =
            cfg!(feature = "ui-capture") && std::env::var_os("CODER_CAPTURE_TO").is_some();
        if capturing {
            // Visual previews read preferences without creating or saving user state.
            if let Some(settings) = crate::preferences::path()
                .and_then(|path| crate::preferences::Settings::load(&path).ok())
            {
                app.terminal.settings = settings;
            }
        } else {
            app.terminal.load_settings();
        }
        app.terminal.settings.apply(&cc.egui_ctx);
        app.session_store = (!capturing)
            .then(crate::preferences::path)
            .flatten()
            .and_then(|path| {
                path.parent()
                    .map(|parent| crate::session::Store::new(parent.join("projects")))
            });
        app.restore_session();
        app.refresh_files();
        if let Some(file) = argument.filter(|p| p.is_file()) {
            app.open(file, None);
        }
        #[cfg(feature = "ui-capture")]
        if let Ok(mode) = std::env::var("CODER_UI_PREVIEW") {
            let paths = navigation::workspace_paths(&app.root).unwrap_or_default();
            app.files = paths.files;
            app.folders = paths.folders;
            app.tree =
                crate::explorer::FileTree::build_with_folders(&app.root, &app.files, &app.folders);
            app.files_rx = None;
            if mode == "workspace-search" {
                app.show_project = false;
                app.query =
                    std::env::var("GLIMCODE_PREVIEW_QUERY").unwrap_or_else(|_| "main".into());
                app.start_search(SearchKind::References, false);
                return app;
            }
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
                app.picker_query =
                    std::env::var("GLIMCODE_PREVIEW_QUERY").unwrap_or_else(|_| "nav".into());
                if let Ok(size) = std::env::var("GLIMCODE_PREVIEW_DIALOG_FONT_SIZE")
                    && let Ok(size) = size.parse::<f32>()
                {
                    app.terminal.settings.dialog_font_size = size;
                    app.terminal.settings.apply(&cc.egui_ctx);
                }
            }
        }
        app
    }

    fn empty(root: PathBuf) -> Self {
        Self {
            worktrees: Default::default(),
            session_store: None,
            worktree_sessions: Default::default(),
            root,
            files: vec![],
            folders: vec![],
            tree: Default::default(),
            selected_category: None,
            extension_filter: None,
            result_scope: Default::default(),
            file_filter: String::new(),
            files_rx: None,
            files_checked: Instant::now(),
            documents: vec![],
            active: 0,
            query: String::new(),
            search_kind: SearchKind::Text,
            exclude_test_files: true,
            results: vec![],
            search_rx: None,
            search_cancel: Arc::new(AtomicBool::new(false)),
            open_matches: 0,
            results_revision: 0,
            search_view: Default::default(),
            search_note: String::new(),
            cache: MarkdownCache::default(),
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
            show_worktrees: true,
            show_project: true,
            files_changed_only: false,
            show_structure: false,
            git: Default::default(),
            terminal: Default::default(),
            structure_filter: String::new(),
            show_file_filter: false,
            search_open: false,
            search_docked: false,
            search_split: 0.7,
            result_selection: 0,
            search_focus: false,
            preview: Default::default(),
            search_highlight: Default::default(),
            focus_editor: false,
            git_colors: (0, Arc::new(std::collections::BTreeMap::new())),
            goto: None,
            pending_click: None,
            pending_reveal: false,
            file_action: None,
            file_action_name: String::new(),
            file_action_focus: false,
        }
    }

    /// Ask the user before a step that would lose work.
    ///
    /// A question that is already on screen is kept, so a request is never
    /// silently dropped.
    fn ask(&mut self, pending: Pending) {
        if self.pending.is_none() {
            self.pending = Some(pending);
        }
    }

    fn request_project(&mut self, root: PathBuf) {
        self.commit_current_sheet();
        if self.documents.iter().any(|doc| doc.dirty) {
            self.ask(Pending::SwitchProject(root));
        } else if self.defer_actions {
            self.actions.push(FrameAction::SwitchProject(root));
        } else {
            self.switch_project(root);
        }
    }

    fn session_snapshot(
        &self,
        documents: &[Document],
        active: usize,
        terminal: &crate::terminal::Terminal,
    ) -> serde_json::Value {
        let tabs: Vec<_> = documents.iter().map(|doc| {
            let (line, column) = doc.jump.unwrap_or_else(|| doc.cursor_line_column(doc.cursor));
            serde_json::json!({"path": doc.path.to_string_lossy(), "line": line, "column": column})
        }).collect();
        serde_json::json!({"tabs": tabs, "active_path": documents.get(active).map(|doc| doc.path.to_string_lossy()),
            "terminal": terminal.snapshot(), "worktrees": self.show_worktrees, "files": self.show_project,
            "structure": self.show_structure, "changed_only": self.files_changed_only})
    }

    fn save_session(&self) {
        let Some(store) = &self.session_store else {
            return;
        };
        let value = if let Some(session) = self.worktree_sessions.get(&self.root) {
            self.session_snapshot(&session.documents, session.active, &session.terminal)
        } else {
            self.session_snapshot(&self.documents, self.active, &self.terminal)
        };
        if let Err(error) = store.save(&self.root, value) {
            eprintln!("Could not save workspace session: {error}");
        }
    }

    fn restore_session(&mut self) {
        if self.worktree_sessions.contains_key(&self.root) {
            return;
        }
        let Some(value) = self
            .session_store
            .as_ref()
            .and_then(|store| store.load(&self.root))
        else {
            return;
        };
        for tab in value["tabs"].as_array().into_iter().flatten().take(256) {
            let Some(path) = tab["path"]
                .as_str()
                .map(PathBuf::from)
                .filter(|path| path.is_file())
            else {
                continue;
            };
            if self.documents.iter().any(|doc| doc.path == path) {
                continue;
            }
            if let Ok(mut doc) = Document::open(path, self.csv_headers) {
                doc.jump = Some((
                    tab["line"].as_u64().unwrap_or(1).max(1) as usize,
                    tab["column"].as_u64().unwrap_or(1).max(1) as usize,
                ));
                self.documents.push(doc);
            }
        }
        self.active = value["active_path"]
            .as_str()
            .and_then(|path| {
                self.documents
                    .iter()
                    .position(|doc| doc.path == Path::new(path))
            })
            .unwrap_or(0);
        self.show_worktrees = value["worktrees"].as_bool().unwrap_or(true);
        self.show_project = value["files"].as_bool().unwrap_or(true);
        self.show_structure = value["structure"].as_bool().unwrap_or(false);
        self.files_changed_only = value["changed_only"].as_bool().unwrap_or(false);
        self.terminal.restore(&value["terminal"]);
    }

    fn switch_project(&mut self, root: PathBuf) {
        self.save_session();
        self.search_cancel.store(true, Ordering::Relaxed);
        self.search_rx = None;
        self.files_rx = None;
        self.preview.clear();
        self.root = root.canonicalize().unwrap_or(root);
        self.documents.clear();
        self.active = 0;
        self.files.clear();
        self.folders.clear();
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
        self.pending_click = None;
        if let Some(goto) = self.goto.take() {
            goto.cancel.store(true, Ordering::Relaxed);
        }
        self.file_action = None;
        self.git = Default::default();
        self.restore_session();
        self.status = "Project opened".into();
        self.refresh_files();
    }

    fn switch_worktree(&mut self, root: PathBuf) {
        if self.defer_actions {
            self.queue_action(FrameAction::SwitchWorktree(root));
            return;
        }
        self.commit_current_sheet();
        let mut terminal = crate::terminal::Terminal::default();
        terminal.settings = self.terminal.settings.clone();
        let session = WorktreeSession {
            documents: std::mem::take(&mut self.documents),
            active: self.active,
            terminal: std::mem::replace(&mut self.terminal, terminal),
            history: std::mem::take(&mut self.history),
        };
        self.worktree_sessions.insert(self.root.clone(), session);
        self.switch_project(root);
        if let Some(session) = self.worktree_sessions.remove(&self.root) {
            self.documents = session.documents;
            self.active = session.active;
            self.terminal = session.terminal;
            self.history = session.history;
        }
        self.focus_editor = true;
    }

    fn refresh_files(&mut self) {
        // Never overlap workspace walks, including manual refreshes.
        if self.files_rx.is_some() {
            return;
        }
        self.files_checked = Instant::now();
        let root = self.root.clone();
        let (tx, rx) = mpsc::channel();
        self.files_rx = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(navigation::workspace_paths(&root));
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
            if let Err(error) = self.documents[index].refresh_if_clean(self.csv_headers) {
                self.status = format!("Could not reload {}: {error}", canonical.display());
            }
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

    fn apply_review_revert(&mut self, revert: crate::git_view::RevertHunk, ctx: &egui::Context) {
        self.open(revert.path.clone(), None);
        if let Some(doc) = self
            .documents
            .get_mut(self.active)
            .filter(|doc| doc.path == revert.path)
        {
            if doc.source_text().ok().as_deref() != Some(revert.expected.as_str()) {
                self.error = Some("The file changed since this diff was shown. Reopen the diff before applying or undoing changes.".into());
            } else {
                if let Err(error) = doc.set_csv_source(true) {
                    self.error = Some(error.to_string());
                    return;
                }
                if let Some(id) = doc.editor_id {
                    let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
                    let mut undoer = state.undoer();
                    let cursor = state.cursor.char_range().unwrap_or_default();
                    undoer.add_undo(&(cursor, revert.expected.clone()));
                    undoer.add_undo(&(cursor, revert.replacement.clone()));
                    state.set_undoer(undoer);
                    state.store(ctx, id);
                }
                let Content::Text(text) = &mut doc.content else {
                    return;
                };
                self.git.record_review_edit(&revert);
                *text = revert.replacement.into();
                doc.dirty = true;
                doc.mark_changed();
                doc.outline_refresh = Some(Instant::now());
                doc.jump = Some((revert.line, 0));
                if doc.is_markdown() {
                    doc.markdown = MarkdownMode::Edit;
                }
                if let Ok(source) = doc.source_text() {
                    self.git.refresh_review_buffer(&doc.path, source, ctx);
                }
            }
        }
    }

    fn current_location(&self) -> Option<Location> {
        let doc = self.documents.get(self.active)?;
        let Content::Text(_) = &doc.content else {
            return None;
        };
        let (line, column) = doc.cursor_line_column(doc.cursor);
        Some(Location {
            category: navigation::MatchCategory::Other,
            path: doc.path.clone(),
            line,
            column,
            preview: String::new(),
        })
    }

    fn navigate(&mut self, location: Location) {
        if let Some(current) = self.current_location() {
            self.history.push(current);
            // The history is a convenience, not a log: it never grows without
            // bound during a session.
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
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

    fn close(&mut self, index: usize, ctx: &egui::Context) {
        if self.defer_actions {
            self.queue_action(FrameAction::Close(index));
            return;
        }
        // egui keeps a full copy of the text for every undo point in the
        // persisted editor state. Releasing it here keeps a closed tab from
        // holding on to its whole history.
        if let Some(id) = self.documents.get(index).and_then(|doc| doc.editor_id) {
            release_editor_state(ctx, id);
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
        self.exclude_test_files = true;
        self.run_search(kind, use_cursor);
    }

    fn run_search(&mut self, kind: SearchKind, use_cursor: bool) {
        self.search_cancel.store(true, Ordering::Relaxed);
        self.search_cancel = Arc::new(AtomicBool::new(false));
        self.search_rx = None;
        // A manual search replaces any pending click. The ranking of the
        // previous click is cancelled as well: it would move the selection of
        // the new search and report a token that is not searched for.
        self.cancel_focus();
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
            // The results of the previous query do not answer this one.
            self.results.clear();
            self.results_revision += 1;
            self.open_matches = 0;
            self.result_selection = 0;
            self.preview.clear();
            self.search_highlight.clear();
            self.search_note = "Type something to search".into();
            return;
        }
        self.search_kind = kind;
        self.search_open = true;
        self.picker = None;
        self.result_selection = 0;
        self.preview.clear();
        self.search_highlight.clear();
        self.selected_category = None;
        self.extension_filter = None;
        self.results.clear();
        self.results_revision += 1;
        self.open_matches = 0;
        self.search_note = "Searching opened and changed files…".into();
        let root = self.root.clone();
        let query = self.query.clone();
        // Only the files that changed are copied. The rest is read by the
        // worker, so a search does not freeze the interface.
        let buffers = std::iter::once(self.active)
            .chain((0..self.documents.len()).filter(|index| *index != self.active))
            .filter_map(|index| self.documents.get(index))
            .map(|doc| {
                let source = doc.dirty.then(|| doc.source_text().unwrap_or_default());
                (doc.path.clone(), source)
            })
            .collect::<Vec<_>>();
        let mut priority_paths = self.opened_search_paths();
        let git_known = self.git.repo.is_some();
        let exclude_tests = self.exclude_test_files;
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
                exclude_tests,
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
        self.search_highlight.poll();
        if let Some(rx) = &self.files_rx
            && let Ok(result) = rx.try_recv()
        {
            match result {
                Ok(paths) => {
                    if self.files != paths.files || self.folders != paths.folders {
                        let mut tree_files = paths.files.clone();
                        tree_files.extend(self.git_colors.1.keys().cloned());
                        self.tree.refresh(&self.root, &tree_files, &paths.folders);
                        self.files = paths.files;
                        self.folders = paths.folders;
                    }
                }
                Err(error) => self.error = Some(format!("{error:#}")),
            }
            self.files_rx = None;
        }
        if self.files_rx.is_none() && self.files_checked.elapsed() >= PROJECT_REFRESH_INTERVAL {
            self.refresh_files();
        }
        // Keep checking even when the editor is idle or no file is open.
        ctx.request_repaint_after(Duration::from_millis(500));
        if let Some(doc) = self.documents.get_mut(self.active) {
            match doc.reload_if_clean(self.csv_headers) {
                Ok(true) => {
                    self.status = format!("Reloaded {} from disk", doc.path.display());
                    self.preview.clear();
                    self.search_highlight.clear();
                }
                Ok(false) => {}
                Err(error) => {
                    self.status = format!("Could not reload {}: {error}", doc.path.display())
                }
            }
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
            self.resolve_pending_click();
        }
        self.preview.poll();
        for doc in &mut self.documents {
            doc.poll_outline(ctx);
        }
        self.poll_focus();
        if self.files_rx.is_some()
            || self.search_rx.is_some()
            || self.goto.is_some()
            || self.preview.loading()
        {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }

    /// The ranking of a click runs on a worker. When it finishes, the best
    /// candidate is focused in the results list.
    fn poll_focus(&mut self) {
        let Some(search) = &self.goto else {
            return;
        };
        let token = search.token.clone();
        match search.rx.try_recv() {
            Ok(ranked) => {
                self.goto = None;
                let Some(best) = ranked.first() else {
                    return;
                };
                self.search_view.refresh(
                    &self.results,
                    self.results_revision,
                    self.selected_category,
                    self.extension_filter.as_deref(),
                );
                if let Some(index) = self.search_view.indices.iter().position(|index| {
                    let location = &self.results[*index];
                    location.path == best.location.path
                        && location.line == best.location.line
                        && location.column == best.location.column
                }) {
                    self.result_selection = index;
                    self.pending_reveal = true;
                }
                let reason = best
                    .reasons
                    .first()
                    .map(|(label, _)| reason_label(label))
                    .unwrap_or("");
                self.status = format!(
                    "{}:{} · {token} · {reason}",
                    best.location
                        .path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy(),
                    best.location.line
                );
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.goto = None;
            }
        }
    }

    /// The text of the open files that changed.
    ///
    /// The files that were not changed are read by the worker, so a click
    /// does not copy every open file.
    fn editor_buffers(&self) -> Vec<(PathBuf, String)> {
        self.documents
            .iter()
            .filter(|doc| doc.dirty)
            .map(|doc| {
                let source = match &doc.content {
                    Content::Text(text) => text.to_string(),
                    Content::Csv(sheet) => sheet.serialize().unwrap_or_default(),
                };
                (doc.path.clone(), source)
            })
            .collect()
    }

    /// Cmd/Ctrl-click. The clicked occurrence decides what the user is after:
    /// a declaration leads to its usages, a usage leads to the declaration,
    /// and anything else shows every match.
    fn click_search(&mut self, click: TextClick) {
        let Some(doc) = self.documents.get(self.active) else {
            return;
        };
        let Content::Text(text) = &doc.content else {
            return;
        };
        let open_paths = self.documents.iter().map(|doc| doc.path.clone()).collect();
        let recent_paths = self.recent_file_searches.iter().cloned().collect();
        let Some(context) = ranking::ClickContext::new(
            &self.root,
            &doc.path,
            text,
            click.char_index,
            open_paths,
            recent_paths,
        ) else {
            if !click.token.is_empty() {
                self.open_token_search(click.token, Intent::Everything, None);
            }
            return;
        };
        let intent = Intent::for_category(context.category);
        let token = context.token.clone();
        self.open_token_search(token, intent, Some(context));
    }

    /// "Go to Declaration": the user asked for the declaration explicitly.
    fn goto_declaration(&mut self) {
        let Some(doc) = self.documents.get(self.active) else {
            return;
        };
        let Content::Text(text) = &doc.content else {
            return;
        };
        let mut index = doc.cursor;
        if navigation::token_at(text, index).is_none() {
            if index == 0 {
                return;
            }
            index -= 1;
        }
        let Some(token) = navigation::token_at(text, index) else {
            return;
        };
        let open_paths = self.documents.iter().map(|doc| doc.path.clone()).collect();
        let recent_paths = self.recent_file_searches.iter().cloned().collect();
        let context = ranking::ClickContext::new(
            &self.root,
            &doc.path,
            text,
            index,
            open_paths,
            recent_paths,
        );
        self.open_token_search(token, Intent::Declarations, context);
    }

    /// Open the search dialog on a token with the tab that answers the
    /// intent. The candidates are ranked once the results arrive.
    fn open_token_search(
        &mut self,
        token: String,
        intent: Intent,
        context: Option<ranking::ClickContext>,
    ) {
        self.cancel_focus();
        self.query = token.clone();
        self.start_search(SearchKind::References, false);
        // `start_search` resets the category filter, so the intent is applied
        // afterwards.
        self.selected_category = intent.category();
        self.result_selection = 0;
        self.pending_reveal = true;
        self.pending_click = Some(PendingClick {
            token: token.clone(),
            intent,
            context,
        });
        self.status = match intent {
            Intent::Usages => format!("Usages of {token}…"),
            Intent::Declarations => format!("Declarations of {token}…"),
            Intent::Everything => format!("Matches for {token}…"),
        };
    }

    fn cancel_focus(&mut self) {
        if let Some(previous) = self.goto.take() {
            previous.cancel.store(true, Ordering::Relaxed);
        }
        self.pending_click = None;
    }

    /// Called when the search of a click finished: fall back to every match if
    /// the intended tab is empty, and rank the candidates so the most likely
    /// declaration comes first.
    fn resolve_pending_click(&mut self) {
        let Some(pending) = self.pending_click.take() else {
            return;
        };
        if let Some(category) = pending.intent.category()
            && !self
                .results
                .iter()
                .any(|location| location.category == category)
        {
            // Nothing matches the intent; show every hit instead of an empty
            // tab.
            self.selected_category = None;
        }
        if pending.intent != Intent::Declarations {
            return;
        }
        let Some(context) = pending.context else {
            return;
        };
        let candidates: Vec<Location> = self
            .results
            .iter()
            .filter(|location| location.category == navigation::MatchCategory::Declaration)
            .cloned()
            .collect();
        if candidates.is_empty() {
            return;
        }
        let buffers = self.editor_buffers();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let ranked = ranking::rank(&context, candidates, &buffers, &worker_cancel);
            let _ = tx.send(ranked);
        });
        self.goto = Some(FocusSearch {
            rx,
            cancel,
            token: pending.token,
        });
    }

    fn open_search(&mut self) {
        if !self.exclude_test_files {
            self.start_search(self.search_kind, false);
        }
        self.picker = None;
        self.search_open = true;
        self.search_focus = true;
        self.commit_current_sheet();
    }

    fn toolbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("toolbar")
            .exact_height(44.0)
            .frame(
                egui::Frame::new()
                    .fill(chrome::PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 6)),
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
                    ui.painter().rect_filled(mark, 7, chrome::SELECTED);
                    ui.painter().text(
                        mark.center(),
                        egui::Align2::CENTER_CENTER,
                        "G",
                        FontId::proportional(16.0),
                        Color32::WHITE,
                    );
                    ui.add_sized(
                        [140.0_f32.min(ui.available_width() * 0.18), 28.0],
                        egui::Label::new(RichText::new(name).strong()).truncate(),
                    )
                    .on_hover_text(self.root.display().to_string());
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
                            self.git.refresh(&self.root, ctx);
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
                            self.goto_declaration();
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
                        ui.checkbox(&mut self.show_worktrees, "Worktrees tool window");
                        if ui
                            .checkbox(&mut self.show_project, "Files tool window")
                            .changed()
                            && self.show_project
                        {
                            self.show_structure = false;
                            self.git.visible = false;
                        }
                        if ui
                            .checkbox(&mut self.show_structure, "Structure tool window")
                            .changed()
                            && self.show_structure
                        {
                            self.git.visible = false;
                            self.show_project = false;
                        }
                        ui.separator();
                        if ui.button("Terminal    Alt F12 / ⌘/Ctrl `").clicked() {
                            self.terminal.toggle(&self.root, ctx);
                            ui.close();
                        }
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
                                    RichText::new("Search files…    Double Shift")
                                        .color(chrome::TEXT),
                                )
                                .fill(chrome::EDITOR)
                                .stroke(egui::Stroke::new(1.0_f32, chrome::BORDER))
                                .corner_radius(8),
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
                if chrome::navigation_button(
                    ui,
                    chrome::Icon::Branch,
                    self.show_worktrees,
                    "Show / hide Worktrees",
                )
                .clicked()
                {
                    self.show_worktrees = !self.show_worktrees;
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

    fn worktrees_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("worktrees-panel")
            .default_width(255.0)
            .width_range(200.0..=450.0)
            .frame(egui::Frame::new().fill(chrome::SIDEBAR).inner_margin(10))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    chrome::heading_icon(ui, chrome::Icon::Branch, chrome::BLUE);
                    ui.strong("Worktrees");
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if chrome::icon_button(ui, chrome::Icon::HideLeft, "Hide Worktrees")
                            .clicked()
                        {
                            self.show_worktrees = false;
                        }
                    });
                });
                ui.separator();
                let mut dirty: Vec<_> = self
                    .worktree_sessions
                    .iter()
                    .filter(|(_, session)| session.documents.iter().any(|doc| doc.dirty))
                    .map(|(path, _)| path.clone())
                    .collect();
                if self.documents.iter().any(|doc| doc.dirty) {
                    dirty.push(self.root.clone());
                }
                if let Some(root) = self.worktrees.content(ui, &self.root, &dirty) {
                    self.switch_worktree(root);
                }
            });
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        let mut open = None;
        let mut git_action = None;
        let mut force_root = self.tree.reveal_pending();
        if force_root {
            self.files_changed_only = false;
        }
        // The colors of the changed files are kept until the repository or the
        // edited documents change, so the tree is not rebuilt every frame.
        let dirty: Vec<PathBuf> = self
            .documents
            .iter()
            .filter(|doc| doc.dirty)
            .map(|doc| doc.path.clone())
            .collect();
        let colors_key = eframe::egui::util::hash((self.git.generation, &dirty));
        if self.git_colors.0 != colors_key {
            let colors = self
                .git
                .repo
                .as_ref()
                .map(|repo| {
                    crate::git_view::tree_files(repo, &dirty)
                        .iter()
                        .map(|file| {
                            (
                                repo.root.join(&file.path),
                                crate::git_view::file_color(file),
                            )
                        })
                        .collect::<std::collections::BTreeMap<_, _>>()
                })
                .unwrap_or_default();
            let mut tree_files = self.files.clone();
            tree_files.extend(colors.keys().cloned());
            self.tree.refresh(&self.root, &tree_files, &self.folders);
            self.git_colors = (colors_key, Arc::new(colors));
        }
        let colors = self.git_colors.1.clone();
        egui::SidePanel::right("explorer")
            .default_width(285.0)
            .width_range(160.0..=500.0)
            .frame(
                egui::Frame::new()
                    .fill(chrome::SIDEBAR)
                    .inner_margin(egui::Margin::symmetric(9, 8)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.spacing_mut().interact_size.y = 26.0;
                ui.horizontal(|ui| {
                    ui.menu_button(RichText::new("Files").strong(), |ui| {
                        ui.checkbox(&mut self.show_file_filter, "Filter project files");
                        if ui.button("Refresh project").clicked() {
                            self.refresh_files();
                            self.git.refresh(&self.root, ctx);
                            ui.close();
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if chrome::icon_button(ui, chrome::Icon::HideRight, "Hide Files").clicked()
                        {
                            self.show_project = false;
                        }
                    });
                });
                let changed_count = self
                    .git
                    .repo
                    .as_ref()
                    .map(|repo| crate::git_view::tree_files(repo, &dirty).len())
                    .unwrap_or(0);
                egui::Frame::new()
                    .fill(chrome::PANEL)
                    .corner_radius(6)
                    .inner_margin(3)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 3.0;
                        let width = (ui.available_width() - 3.0) / 2.0;
                        ui.horizontal(|ui| {
                            for (changed_only, label) in [
                                (false, "All files".to_owned()),
                                (true, format!("Changed · {changed_count}")),
                            ] {
                                let selected = self.files_changed_only == changed_only;
                                let response = ui.add_sized(
                                    egui::vec2(width, 25.0),
                                    egui::Button::selectable(
                                        selected,
                                        RichText::new(&label).size(12.0).color(if selected {
                                            chrome::TEXT
                                        } else {
                                            chrome::MUTED
                                        }),
                                    )
                                    .fill(if selected {
                                        chrome::HEADER
                                    } else {
                                        chrome::PANEL
                                    })
                                    .stroke(egui::Stroke::NONE)
                                    .corner_radius(4)
                                    .wrap_mode(egui::TextWrapMode::Truncate),
                                );
                                if response.on_hover_text(label).clicked() {
                                    self.files_changed_only = changed_only;
                                }
                            }
                        });
                    });
                ui.add_space(6.0);
                git_action = self.git.content(ui, &self.root, &dirty, false);
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
                            RichText::new(format!(
                                "{} files",
                                if self.files_changed_only {
                                    changed_count
                                } else {
                                    self.files.len()
                                }
                            ))
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
                if self.files_changed_only {
                    let mut hide_tests = !self.git.include_tests;
                    ui.checkbox(&mut hide_tests, RichText::new("Hide test files").size(12.0));
                    self.git.include_tests = !hide_tests;
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
                                open = self.tree.show_scoped(
                                    ui,
                                    &self.root,
                                    self.documents.get(self.active).map(|d| d.path.as_path()),
                                    if self.show_file_filter {
                                        &self.file_filter
                                    } else {
                                        ""
                                    },
                                    &colors,
                                    self.files_changed_only,
                                    self.files_changed_only && !self.git.include_tests,
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
        if let Some(path) = self.tree.diff.take() {
            git_action = Some(crate::git_view::ChangeAction::Diff(path));
        }
        if let Some(action) = git_action {
            self.apply_file_panel_action(action, ctx);
        }
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

    fn apply_file_panel_action(
        &mut self,
        action: crate::git_view::ChangeAction,
        ctx: &egui::Context,
    ) {
        match action {
            crate::git_view::ChangeAction::File(action) => {
                self.tree.action = Some(action);
            }
            crate::git_view::ChangeAction::Open(path) => self.open(path, None),
            crate::git_view::ChangeAction::Diff(path) => {
                let buffer = self
                    .documents
                    .iter()
                    .find(|doc| doc.path == path && doc.dirty)
                    .and_then(|doc| match &doc.content {
                        Content::Text(text) => Some(text.to_string()),
                        Content::Csv(sheet) => sheet.serialize().ok(),
                    });
                self.git.open_review(path, buffer, ctx);
            }
        }
    }

    fn structure_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("right-tool-rail")
            .exact_width(32.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(chrome::PANEL).inner_margin(2))
            .show(ctx, |ui| {
                for (name, active) in [
                    (
                        "Files",
                        (self.show_project || self.git.visible) && !self.show_structure,
                    ),
                    ("Structure", self.show_structure),
                ] {
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(28.0, 84.0), egui::Sense::click());
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
                        let open = !active;
                        self.show_project = name == "Files" && open;
                        self.show_structure = name == "Structure" && open;
                        self.git.visible = false;
                    }
                }
            });
        if self.git.visible {
            self.git.visible = false;
            self.show_project = true;
            self.show_structure = false;
            self.files_changed_only = true;
        }
        if !self.show_structure {
            if self.show_project {
                self.sidebar(ctx);
            }
            return;
        }
        let mut target = None;
        egui::SidePanel::right("structure")
            .default_width(255.0)
            .width_range(180.0..=500.0)
            .frame(egui::Frame::new().fill(chrome::SIDEBAR).inner_margin(10))
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
                        Content::Text(text) if !crate::editor_analysis::is_large(text) => {
                            // The colors are the ones the editor paints, so
                            // the same cached result is used.
                            let fingerprint = egui::util::hash(text.as_str());
                            Some(
                                syntax_cache(
                                    ui,
                                    &doc.path,
                                    text,
                                    doc.path
                                        .extension()
                                        .and_then(|ext| ext.to_str())
                                        .unwrap_or(""),
                                    fingerprint,
                                )
                                .job,
                            )
                        }
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
            // The window has to fit into the viewport, also on a small
            // screen, or its field is cut off and cannot be reached.
            let width = (size.x - 100.0)
                .clamp(320.0, 980.0)
                .min(size.x - 32.0)
                .max(240.0);
            let mut open = true;
            egui::Window::new("Find in Files")
                .id(egui::Id::new("find-in-files"))
                .title_bar(false)
                .collapsible(false)
                .resizable(true)
                .open(&mut open)
                .default_pos(egui::pos2(((size.x - width) / 2.0).max(8.0), 85.0))
                .default_width(width - 48.0)
                .min_width(240.0)
                .default_height((size.y * 0.78).clamp(350.0, 720.0))
                .min_height(350.0)
                .frame(chrome::popup_frame().inner_margin(14))
                .show(ctx, |ui| {
                    chrome::dialog_style(ui, ui.available_width());
                    self.search_contents(ui, false);
                });
            if !open {
                self.search_open = false;
                self.focus_editor = true;
            }
        }
    }

    fn search_contents(&mut self, ui: &mut egui::Ui, docked: bool) {
        use crate::search_view::Scope;
        chrome::dialog_typography(ui);
        let result_size = crate::preferences::search_results_font_size(ui.ctx());
        let filter_size = crate::preferences::search_filters_font_size(ui.ctx());
        let row_height =
            ui.fonts_mut(|fonts| fonts.row_height(&FontId::proportional(result_size))) + 14.0;
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 4.0);
        ui.spacing_mut().button_padding = egui::vec2(10.0, 4.0);
        ui.spacing_mut().interact_size.y = 26.0;
        self.update_search_scope();
        self.search_view.refresh(
            &self.results,
            self.results_revision,
            self.selected_category,
            self.extension_filter.as_deref(),
        );
        let minimum_height = (ui.available_height() - 32.0).max(250.0);
        if docked {
            ui.set_min_height(ui.available_height());
        } else {
            ui.set_min_height(minimum_height);
        }
        ui.horizontal(|ui| {
            chrome::heading_icon(ui, chrome::Icon::Search, chrome::BLUE);
            ui.label(
                RichText::new("Find in Files")
                    .size(crate::preferences::dialog_font_size(ui.ctx()) + 2.0)
                    .strong(),
            );
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
                    self.focus_editor = true;
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
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Find").size(crate::preferences::dialog_font_size(ui.ctx()) + 2.0).color(chrome::MUTED));
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .frame(false)
                    .font(FontId::proportional(crate::preferences::dialog_font_size(ui.ctx()) + 2.0))
                    .hint_text("Search in project…")
                    .desired_width((ui.available_width() - 420.0).max(100.0)),
            );
            if self.search_focus {
                field.request_focus();
                self.search_focus = false;
            }
            let run = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let mut words = self.search_kind != SearchKind::Text;
            let words_changed = ui.checkbox(&mut words, RichText::new("Whole words").size(filter_size))
                .on_hover_text("Match complete words: generate matches generate() and build-generate, but excludes generated and generate_keys.").changed();
            let tests_changed = ui.checkbox(&mut self.exclude_test_files, RichText::new("Exclude test files").size(filter_size))
                .on_hover_text("Skip common test filenames and folders in project search and Cmd/Ctrl-click navigation. Turn off to include tests.")
                .changed();
            let kind = if words { SearchKind::References } else { SearchKind::Text };
            if run || chrome::dialog_button(ui, "Search", Some(chrome::BLUE)).clicked() {
                self.start_search(kind, false);
            } else if words_changed {
                self.run_search(kind, false);
            } else if tests_changed {
                self.run_search(self.search_kind, false);
            }
        });
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            crate::preferences::search_filter_style(ui);
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
        ui.horizontal_wrapped(|ui| {
            crate::preferences::search_filter_style(ui);
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
        // The filter buttons may have changed the category.
        self.search_view.refresh(
            &self.results,
            self.results_revision,
            self.selected_category,
            self.extension_filter.as_deref(),
        );
        let visible = self.search_view.indices.clone();
        self.result_selection = self.result_selection.min(visible.len().saturating_sub(1));
        let mut reveal_selection = self.pending_reveal;
        self.pending_reveal = false;
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowDown)) {
            if !visible.is_empty() {
                self.result_selection = (self.result_selection + 1) % visible.len();
            }
            reveal_selection = true;
        }
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::ArrowUp)) {
            if !visible.is_empty() {
                self.result_selection = (self.result_selection + visible.len() - 1) % visible.len();
            }
            reveal_selection = true;
        }
        let mut navigate = None;
        // Reserve only the preview heading, frame padding and divider; the rest is content.
        let space = (ui.available_height() - 80.0).max(0.0);
        let list_height = space * self.search_split;
        let preview_height = space - list_height;
        let dark = ui.visuals().dark_mode;
        self.search_highlight.set_dark(dark);
        let mut matches = egui::ScrollArea::vertical()
            .id_salt("search-matches")
            .auto_shrink([false, false])
            .min_scrolled_height(list_height)
            .max_height(list_height);
        if reveal_selection {
            let id = ui.make_persistent_id(egui::Id::new("search-matches"));
            let offset = egui::scroll_area::State::load(ui.ctx(), id)
                .unwrap_or_default()
                .offset
                .y;
            let row_top = self.result_selection as f32 * (row_height + ui.spacing().item_spacing.y);
            let offset = if row_top < offset {
                row_top
            } else if row_top + row_height > offset + list_height {
                row_top + row_height - list_height
            } else {
                offset
            };
            matches = matches.vertical_scroll_offset(offset);
        }
        let mut wanted: Vec<PathBuf> = vec![];
        matches.show_rows(ui, row_height, visible.len(), |ui, range| {
            if visible.is_empty() {
                ui.weak(if self.search_rx.is_some() {
                    "Searching…"
                } else if self.results.is_empty() {
                    "No matches found. Try a different search."
                } else {
                    "No results match these filters. Clear a category or file-type filter."
                });
            }
            for index in range {
                let location = &self.results[visible[index]];
                let selected = index == self.result_selection;
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), row_height),
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
                let response = response.on_hover_text(format!(
                    "{}:{}:{}\nDouble-click or press Enter to open",
                    location.path.display(),
                    location.line,
                    location.column
                ));
                let suffix =
                    search_result_label(location, &self.root, visible[index] < self.open_matches);
                let mut metadata = egui::text::LayoutJob::simple_singleline(
                    suffix,
                    FontId::proportional(result_size),
                    if selected {
                        chrome::TEXT
                    } else {
                        chrome::MUTED
                    },
                );
                metadata.wrap.max_width = (rect.width() * 0.42).max(0.0);
                metadata.wrap.max_rows = 1;
                metadata.wrap.break_anywhere = true;
                let galley = ui.fonts_mut(|fonts| fonts.layout_job(metadata));
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
                // The line is colored with the syntax of the file it comes
                // from. Until that file has been highlighted the plain match is
                // shown, so a row is never blank.
                let mut job = self
                    .search_highlight
                    .line(&location.path, location.line)
                    .unwrap_or_else(|| {
                        egui::text::LayoutJob::simple(
                            location.preview.trim_start().to_owned(),
                            FontId::monospace(result_size),
                            chrome::TEXT,
                            f32::INFINITY,
                        )
                    });
                chrome::trim_start(&mut job);
                for section in &mut job.sections {
                    section.format.font_id.size = result_size;
                }
                chrome::mark_matches(&mut job, &self.query);
                let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                painter.galley(
                    rect.left_center() + egui::vec2(10.0, -galley.size().y / 2.0),
                    galley,
                    chrome::TEXT,
                );
                if !wanted.contains(&location.path) {
                    wanted.push(location.path.clone());
                }
                if response.clicked() {
                    self.result_selection = index;
                }
                if response.double_clicked() {
                    navigate = Some(location.clone());
                }
            }
        });
        // Highlight the files of the visible rows in the background, so the
        // colors arrive a moment later without blocking the frame.
        for path in wanted {
            self.search_highlight.request(&path, || {
                self.documents
                    .iter()
                    .find(|doc| doc.path == path)
                    .and_then(|doc| match &doc.content {
                        Content::Text(text) => Some(text.to_string()),
                        Content::Csv(_) => None,
                    })
            });
        }
        let (divider, drag) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::drag());
        ui.painter().hline(
            divider.x_range(),
            divider.center().y,
            egui::Stroke::new(
                1.0_f32,
                if drag.hovered() || drag.dragged() {
                    chrome::BLUE
                } else {
                    chrome::BORDER
                },
            ),
        );
        if drag.dragged() && space > 0.0 {
            self.search_split =
                (self.search_split + ui.input(|i| i.pointer.delta().y) / space).clamp(0.35, 0.85);
        }
        drag.on_hover_cursor(egui::CursorIcon::ResizeVertical)
            .on_hover_text("Drag to resize results and preview · ↑/↓ to select · Enter to open");
        // The key is taken even when there is nothing to open, so it does not
        // reach the editor or the terminal behind the panel.
        let enter = ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter));
        if let Some(index) = visible.get(self.result_selection) {
            let location = self.results[*index].clone();
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
            if enter {
                navigate = Some(location);
            }
        } else {
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
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            self.search_open = false;
            self.focus_editor = true;
        }
        if let Some(location) = navigate {
            self.navigate(location);
            if !docked {
                self.search_open = false;
                self.focus_editor = true;
            }
        }
    }

    fn search_preview(&mut self, ui: &mut egui::Ui, location: &Location, height: f32) {
        let dark = ui.visuals().dark_mode;
        let document = self
            .documents
            .iter()
            .find(|doc| doc.path == location.path && doc.dirty);
        let revision = document.map(Document::revision);
        if self.preview.needs(&location.path, dark, revision) {
            // Match the search worker: clean files come from disk, while
            // unsaved edits (including CSV grids) come from the editor.
            let source = document.map(|doc| doc.source_text().unwrap_or_default());
            self.preview
                .request(location.path.clone(), source, dark, revision);
        }
        // Watch disk versions even when the search panel is otherwise idle.
        if revision.is_none() {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
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
        let jump = preview.select(location.line, location.column, row_height);
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
                                chrome::HEADER.gamma_multiply(0.6),
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
            return self
                .file_find
                .refresh(Path::new(""), "", &self.find, 0, || 0);
        };
        let Content::Text(text) = &doc.content else {
            return self
                .file_find
                .refresh(&doc.path, "", &self.find, doc.revision(), || 0);
        };
        self.file_find
            .refresh(&doc.path, text, &self.find, doc.revision(), || {
                text.char_indices()
                    .nth(doc.cursor)
                    .map_or(text.len(), |(byte, _)| byte)
            })
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
                                *text = text.replace(&self.find, &self.replacement).into();
                                doc.dirty = *text != doc.original;
                                doc.mark_changed();
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
        // Keep scrollbars outside the neighbouring panels' resize hit zones.
        // Floating bars otherwise grow under a splitter, lose hover, shrink,
        // and repeatedly regain hover while the pointer stays still.
        let style = ctx.style();
        let resize_gap = (style.interaction.resize_grab_radius_side
            + style.visuals.clip_rect_margin)
            .ceil() as i8
            + 1;
        let margin = egui::Margin {
            left: if self.show_worktrees { resize_gap } else { 0 },
            right: if self.show_project || self.show_structure || self.git.visible {
                resize_gap
            } else {
                0
            },
            bottom: if self.terminal.visible || (self.search_open && self.search_docked) {
                resize_gap
            } else {
                0
            },
            top: 0,
        };
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(chrome::EDITOR).inner_margin(margin))
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
                        ui.horizontal(|ui| {
                            egui::ScrollArea::horizontal()
                                .id_salt("tabs")
                                .max_width((ui.available_width() - 40.0).max(0.0))
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
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                if chrome::icon_button(
                                    ui,
                                    chrome::Icon::HideDown,
                                    "Minimize editor · show terminal",
                                )
                                .clicked()
                                {
                                    self.commit_current_sheet();
                                    self.focus_editor = false;
                                    self.terminal.maximize(&self.root, ctx);
                                }
                            });
                        });
                    });
                if let Some(index) = activate {
                    self.queue_action(FrameAction::Activate(index));
                }
                if let Some(index) = close {
                    if self.documents[index].dirty {
                        self.ask(Pending::Close(index));
                        self.commit_current_sheet();
                    } else {
                        self.close(index, ctx);
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
                        Content::Text(text) => self.git.cached_file(&doc.path, text),
                        Content::Csv(_) => None,
                    });
                let Some(doc) = self.documents.get_mut(self.active) else {
                    ui.set_clip_rect(ui.clip_rect().intersect(ui.max_rect()).shrink(8.0));
                    egui::ScrollArea::vertical().scroll_source(egui::scroll_area::ScrollSource { drag: false, ..Default::default() }).id_salt("welcome").show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.add_space((ui.available_height() * 0.16).clamp(24.0, 100.0));
                        let (mark, _) = ui.allocate_exact_size(egui::vec2(64.0, 64.0), egui::Sense::hover());
                        ui.painter().rect_filled(mark, 18, chrome::SELECTED);
                        ui.painter().text(mark.center(), egui::Align2::CENTER_CENTER, "G", FontId::proportional(34.0), chrome::BLUE);
                        ui.add_space(18.0);
                        ui.label(RichText::new("Make room for your next idea.").size(26.0).strong());
                        ui.add_space(6.0);
                        ui.label(RichText::new("Find a file, explore a branch, or pick up where you left off.").color(chrome::MUTED));
                        ui.add_space(24.0);
                        let width = 350.0_f32.min(ui.available_width());
                        ui.scope(|ui| {
                            ui.set_max_width(width);
                            for (title, shortcut, action) in [
                                ("Search files", "Double Shift", 0),
                                ("Find in project", "⌘/Ctrl Shift F", 1),
                                ("Recent files", "⌘/Ctrl E", 2),
                                ("Open folder…", "Browse", 3),
                            ] {
                                let button = egui::Button::new(format!("{title}     ·     {shortcut}"))
                                    .fill(chrome::PANEL)
                                    .stroke(egui::Stroke::new(1.0_f32, chrome::BORDER))
                                    .corner_radius(8)
                                    .min_size(egui::vec2(width, 42.0));
                                if ui.add(button).clicked() {
                                    match action {
                                        0 => self.show_picker(Picker::Everywhere),
                                        1 => self.open_search(),
                                        2 => self.show_picker(Picker::Recent),
                                        _ => if let Some(root) = rfd::FileDialog::new().pick_folder() { self.request_project(root); },
                                    }
                                }
                                ui.add_space(5.0);
                            }
                        });
                        ui.add_space(22.0);
                        ui.label(RichText::new("WORKTREES  /  CODE  /  REVIEW  /  TERMINAL").size(10.0).color(chrome::MUTED));
                    });
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
                    self.files_changed_only = false;
                    self.show_structure = false;
                    self.git.visible = false;
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
                if let Some(data) = git_data.as_deref()
                    && let Content::Text(text) = &doc.content
                {
                    let line = text.chars().take(doc.cursor).filter(|ch| *ch == '\n').count();
                    if let Some(target) = editor_diff_navigation(ui, &data.change_lines, line) {
                        doc.jump = Some((target + 1, 1));
                        if doc.markdown == MarkdownMode::Preview {
                            doc.markdown = MarkdownMode::Edit;
                        }
                    }
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
                        let changed = sheet.ui(ui);
                        if changed {
                            // The grid changed, so the cached hash of the
                            // buffer is stale.
                            doc.mark_changed();
                        }
                        doc.dirty |= changed;
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
                            doc.mark_changed();
                            doc.outline_refresh = Some(Instant::now());
                        }
                    }
                });
                if let Content::Text(text) = &doc.content {
                    self.git.ensure_file(&doc.path, text, ctx);
                }
                if let Some(source) = csv_mode {
                    if let Err(error) = doc.set_csv_source(source) {
                        doc.csv_error = Some(format!("{error:#}"));
                    }
                    ui.ctx().request_repaint();
                }
                // A dialog that took the keyboard gives it back to the editor,
                // unless the terminal is the one being typed into.
                if std::mem::take(&mut self.focus_editor)
                    && !self.terminal.focused(ctx)
                    && let Some(id) = doc.editor_id
                {
                    ctx.memory_mut(|memory| memory.request_focus(id));
                }
            });
        if let Some(blame) = clicked_blame {
            self.git.show_commit(blame, ctx);
        }
        if let Some(click) = clicked_token {
            self.click_search(click);
        }
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        if let Some(error) = self.error.clone() {
            egui::Modal::new(egui::Id::new("action-error"))
                .frame(chrome::popup_frame())
                .show(ctx, |ui| {
                    chrome::modal_style(ui, 470.0);
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
                    chrome::modal_style(ui, 460.0);
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
                    chrome::modal_style(ui, 500.0);
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
                            // The document may have been closed while the
                            // question was on screen.
                            match self.documents.get(i) {
                                Some(doc) => chrome::dialog_path(ui, &doc.path),
                                None => {
                                    self.pending = None;
                                    return;
                                }
                            }
                        }
                        _ => {
                            let dirty: Vec<_> = self
                                .documents
                                .iter()
                                .chain(
                                    self.worktree_sessions
                                        .values()
                                        .flat_map(|session| session.documents.iter()),
                                )
                                .filter(|doc| doc.dirty)
                                .collect();
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
                            if matches!(self.pending, Some(Pending::Exit)) {
                                for session in self.worktree_sessions.values_mut() {
                                    for doc in &mut session.documents {
                                        if doc.dirty
                                            && let Err(error) = doc.save()
                                        {
                                            self.error = Some(format!("{error:#}"));
                                            success = false;
                                        }
                                    }
                                }
                            }
                            success
                        }
                    }
                };
                if saved {
                    match self.pending.take() {
                        Some(Pending::Close(index)) => self.close(index, ctx),
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
                            doc.outline.clear();
                            doc.outline_refresh = Some(Instant::now());
                        }
                    }
                    self.history.clear();
                    self.recent_file_searches.clear();
                }
            }
            FileAction::Delete(path) => {
                std::fs::remove_file(&path)?;
                // Removing a document before the active one shifts every later
                // tab down, so the active index has to follow.
                let removed = self.documents.iter().position(|doc| doc.path == path);
                self.documents.retain(|doc| doc.path != path);
                if removed.is_some() {
                    if let Some(index) = removed
                        && self.active > index
                    {
                        self.active -= 1;
                    }
                    self.active = self.active.min(self.documents.len().saturating_sub(1));
                }
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

    fn open_terminal_link(&mut self, link: crate::terminal::FileLink) {
        let query = crate::terminal::file_link_query(&link.query);
        let (file, target) = split_file_location(&query);
        let target = target.map(|(line, _)| (line, 1));
        let resolved = self.resolve_terminal_path(file, &link.directory);
        if let Some(path) = resolved {
            if path.is_dir() {
                let root = self
                    .root
                    .canonicalize()
                    .unwrap_or_else(|_| self.root.clone());
                if !path.starts_with(&root) {
                    self.actions.push(FrameAction::OpenProjectInstance(path));
                    return;
                }
                self.terminal.maximized = false;
                self.show_project = true;
                self.files_changed_only = false;
                self.show_structure = false;
                self.git.visible = false;
                self.file_filter.clear();
                let path = self.root.join(path.strip_prefix(&root).unwrap_or(&path));
                self.tree.reveal_path(&self.root, &path);
            } else {
                self.terminal.maximized = false;
                self.open(path, target);
                self.focus_editor = true;
            }
        } else {
            self.show_picker(Picker::Files);
            self.picker_query = query;
        }
    }

    fn resolve_terminal_path(&self, file: &str, directory: &Path) -> Option<PathBuf> {
        let path = if let Some(relative) = file.strip_prefix("~/") {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(relative))
        } else if file == "~" {
            std::env::var_os("HOME").map(PathBuf::from)
        } else {
            Some(PathBuf::from(file))
        }?;
        let existing = |path: PathBuf| {
            (path.is_file() || path.is_dir()).then(|| path.canonicalize().unwrap_or(path))
        };
        if path.is_absolute() {
            return existing(path);
        }
        let root = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let directory = directory
            .canonicalize()
            .unwrap_or_else(|_| directory.to_owned());
        for base in directory.ancestors() {
            if base != directory && !base.starts_with(&root) {
                break;
            }
            if let Some(found) = existing(base.join(&path)) {
                return Some(found);
            }
        }
        if let Some(found) = existing(root.join(&path)) {
            return Some(found);
        }
        // Logs sometimes include the project name or Git's a/ and b/ prefixes.
        if path.components().next().map(|c| c.as_os_str()) == root.file_name()
            && let Some(parent) = root.parent()
            && let Some(found) = existing(parent.join(&path))
        {
            return Some(found);
        }
        for prefix in ["a", "b"] {
            if let Ok(relative) = path.strip_prefix(prefix)
                && let Some(found) = existing(root.join(relative))
            {
                return Some(found);
            }
        }
        // A unique suffix can resolve filenames printed relative to a build
        // directory. Ambiguous matches go to the picker instead of guessing.
        let suffix = path.strip_prefix(".").unwrap_or(&path);
        let mut matches = self.files.iter().chain(&self.folders).filter(|candidate| {
            candidate.ends_with(suffix) && (candidate.is_file() || candidate.is_dir())
        });
        let first = matches.next()?;
        if matches.any(|candidate| candidate != first) {
            return None;
        }
        existing(first.clone())
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
            self.goto_declaration();
        }
        if key(M::ALT, Key::F7) {
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
                self.ask(Pending::Close(self.active));
                self.commit_current_sheet();
            } else {
                self.close(self.active, ctx);
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
                doc.mark_changed();
                doc.outline_refresh = Some(Instant::now());
            }
        }
    }

    fn picker_ui(&mut self, ctx: &egui::Context) {
        let dialog_size = crate::preferences::dialog_font_size(ctx);
        let Some(mode) = self.picker else {
            return;
        };
        let mut open = None;
        let mut close = false;
        egui::Modal::new(egui::Id::new("navigation-picker"))
            .frame(chrome::popup_frame())
            .show(ctx, |ui| {
                chrome::modal_style(ui, (ctx.content_rect().width() - 120.0).clamp(450.0, 820.0));
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
                        .font(FontId::proportional(dialog_size + 8.0))
                        .desired_width(f32::INFINITY)
                        .hint_text(if mode == Picker::Line {
                            "Line number, optionally :column"
                        } else if mode == Picker::Symbols {
                            "Filter symbols in this file"
                        } else {
                            "File or folder path, optionally :line or :line:column"
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
                            if mode == Picker::Recent {
                                &[]
                            } else {
                                &self.folders
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
                    let previous_selection = self.picker_selection;
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
                                let display =
                                    if mode == Picker::Symbols || label.starts_with("Symbol:") {
                                        label.to_owned()
                                    } else {
                                        location
                                            .path
                                            .file_name()
                                            .unwrap_or_default()
                                            .to_string_lossy()
                                            .into_owned()
                                    };
                                let parent = (mode != Picker::Symbols).then(|| {
                                    let parent = relative(
                                        location.path.parent().unwrap_or(Path::new("")),
                                        &self.root,
                                    );
                                    if parent.is_empty() {
                                        ".".into()
                                    } else {
                                        parent
                                    }
                                });
                                let response = picker_result_row(
                                    ui,
                                    &display,
                                    parent.as_deref(),
                                    &location.path,
                                    self.folders.binary_search(&location.path).is_ok(),
                                    &highlight_term(&display, file_query),
                                    &highlight_term(parent.as_deref().unwrap_or(""), file_query),
                                    dialog_size,
                                    selected,
                                );
                                if selected && self.picker_selection != previous_selection {
                                    ui.scroll_to_rect(response.rect, None);
                                }
                                if response.clicked() || (enter && index == self.picker_selection) {
                                    open = Some(location.clone());
                                }
                            }
                        });
                    if entries.is_empty() {
                        ui.weak("No matching files, folders or symbols");
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
            if location.path.is_dir() {
                self.show_project = true;
                self.files_changed_only = false;
                self.show_structure = false;
                self.git.visible = false;
                self.file_filter.clear();
                self.tree.reveal_path(&self.root, &location.path);
            } else {
                self.navigate(location);
            }
            self.picker = None;
            self.focus_editor = true;
        } else if close {
            self.picker = None;
            self.focus_editor = true;
        }
    }

    fn draw(&mut self, ctx: &egui::Context) {
        self.defer_actions = true;
        self.poll(ctx);
        self.worktrees.poll();
        let blocked = self.pending.is_some()
            || self.terminal.modal()
            || self.worktrees.modal()
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
            if !self.terminal.focused(ctx) {
                self.git.review_shortcuts(ctx);
            }
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
            && (self.documents.iter().any(|d| d.dirty)
                || self
                    .worktree_sessions
                    .values()
                    .any(|session| session.documents.iter().any(|d| d.dirty)))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.ask(Pending::Exit);
            self.commit_current_sheet();
        }
        self.toolbar(ctx);
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::new()
                    .fill(chrome::TABS)
                    .inner_margin(egui::Margin::symmetric(12, 5)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(chrome::GREEN, "●");
                    if let Some(repo) = &self.git.repo {
                        chrome::heading_icon(ui, chrome::Icon::Branch, chrome::BLUE);
                        ui.label(RichText::new(&repo.branch).size(11.0).color(chrome::BLUE));
                        ui.separator();
                    }
                    ui.label(RichText::new(&self.status).size(11.0).color(chrome::MUTED));
                    ui.separator();
                    ui.small("UTF-8");
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if let Some(location) = self.current_location() {
                            ui.small(format!("{}:{}", location.line, location.column));
                        }
                        ui.small(
                            self.documents
                                .get(self.active)
                                .map_or("LF", |doc| doc.line_ending),
                        );
                    });
                });
            });
        self.tool_rail(ctx);
        // Keep the right tools full height; the terminal extends under Worktrees.
        self.structure_panel(ctx);
        // Keep this frame's placement stable when a header button toggles it.
        let terminal_maximized = self.terminal.visible && self.terminal.maximized;
        if !terminal_maximized {
            self.terminal.panel(ctx, &self.root, !blocked);
        }
        if self.show_worktrees {
            self.worktrees_panel(ctx);
        }
        if self.search_docked {
            self.results_panel(ctx);
        }

        if self.picker.is_some() {
            chrome::clear_resize_cursor(ctx);
        }
        self.picker_ui(ctx);
        if terminal_maximized {
            self.terminal.central_panel(ctx, &self.root, !blocked);
        } else {
            self.editor(ctx);
        }
        if let Some(link) = crate::terminal::take_file_link(ctx) {
            self.open_terminal_link(link);
        }
        if !self.search_docked {
            self.results_panel(ctx);
        }
        self.defer_actions = false;
        for action in std::mem::take(&mut self.actions) {
            match action {
                FrameAction::Open(path, target) => self.open(path, target),
                FrameAction::SwitchProject(root) => self.switch_project(root),
                FrameAction::SwitchWorktree(root) => self.switch_worktree(root),
                FrameAction::OpenProjectInstance(root) => {
                    if let Err(error) = open_project_instance(&root) {
                        self.error =
                            Some(format!("Could not open project in a new instance: {error}"));
                    }
                }
                FrameAction::Save(index) => {
                    self.save(index);
                }
                FrameAction::Close(index) => self.close(index, ctx),
                FrameAction::Activate(index) => {
                    if index < self.documents.len() {
                        self.active = index;
                    }
                }
                FrameAction::GitDiff(index) => {
                    // The index was taken in an earlier frame, and a document
                    // may have been closed in between.
                    let Some(doc) = self.documents.get(index) else {
                        continue;
                    };
                    let buffer = if doc.dirty {
                        match &doc.content {
                            Content::Text(text) => Some(text.to_string()),
                            Content::Csv(sheet) => sheet.serialize().ok(),
                        }
                    } else {
                        None
                    };
                    self.git.open_review(doc.path.clone(), buffer, ctx);
                }
                FrameAction::Reload(index) => {
                    let Some(doc) = self.documents.get(index) else {
                        continue;
                    };
                    if doc.dirty {
                        self.ask(Pending::Reload(index));
                    } else if let Err(error) = self.documents[index].reload(self.csv_headers) {
                        self.error = Some(error.to_string());
                    }
                }
            }
        }
        if let Some(path) = self.git.windows(ctx) {
            self.open(path, None);
        }
        if let Some(revert) = self.git.pending_revert.take() {
            self.apply_review_revert(revert, ctx);
        }
        if self.pending.is_some()
            || self.terminal.modal()
            || self.worktrees.modal()
            || self.file_action.is_some()
            || self.error.is_some()
            || self.picker.is_some()
        {
            chrome::clear_resize_cursor(ctx);
        }
        self.dialogs(ctx);
        let editing_preferences = self.terminal.settings_open;
        self.terminal.windows(ctx);
        if editing_preferences && !self.terminal.settings_open {
            // Certificate overrides are global even for cached worktree terminals.
            for session in self.worktree_sessions.values_mut() {
                session
                    .terminal
                    .settings
                    .terminal_tls_certificate
                    .clone_from(&self.terminal.settings.terminal_tls_certificate);
                session
                    .terminal
                    .settings
                    .terminal_tls_private_key
                    .clone_from(&self.terminal.settings.terminal_tls_private_key);
            }
        }
        self.worktrees.dialog(ctx);
        let mut panels = Vec::new();
        if self.show_worktrees {
            panels.push(("worktrees-panel", true));
        }
        if self.show_project && !self.show_structure && !self.git.visible {
            panels.push(("explorer", true));
        }
        if self.show_structure {
            panels.push(("structure", true));
        }
        if self.terminal.visible && !terminal_maximized {
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
            || self.worktrees.modal()
            || self.file_action.is_some()
            || self.error.is_some()
            || self.picker.is_some();
        chrome::finish_resize_borders(ctx, &panels, &windows, modal);
    }
}

impl eframe::App for Coder {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.draw(ctx);
        // Persist session IDs immediately, so an editor crash can reconnect too.
        if self.terminal.take_metadata_changed() {
            self.save_session();
        }
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
        self.save_session();
        self.search_cancel.store(true, Ordering::Relaxed);
    }
}

/// Parse numeric suffixes from the right, preserving colons in the path itself.
fn open_project_instance(root: &Path) -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    let mut child = project_instance_command(&executable, root).spawn()?;
    // Reap the independent process without blocking either editor's UI.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn project_instance_command(executable: &Path, root: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(executable);
    command
        .arg(root)
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for name in [
        "GLIMCODE_PREVIEW_FILE",
        "CODER_UI_PREVIEW",
        "CODER_CAPTURE_TO",
    ] {
        command.env_remove(name);
    }
    command
}

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

/// Allocate the entire result once; painting icons must not move the list cursor.
#[allow(clippy::too_many_arguments)]
fn reason_label(label: &str) -> &'static str {
    match label {
        "definition" => "declaration",
        "kind" => "declaration shape",
        "file" => "same or imported file",
        "scope" => "same scope",
        "name" => "exact name",
        "stem" => "file name matches",
        "path" => "path matches",
        "visible" => "visibility",
        "top" => "top level",
        "doc" => "documented",
        "prior" => "declaration kind",
        "unique" => "only definition",
        "specific" => "specific name",
        "near" => "close to click",
        "above" => "above the click",
        "recent" => "open recently",
        "focus" => "focused file",
        "dir" => "next to the file",
        "noise" => "test/vendor penalty",
        "size" => "large file penalty",
        "dup" => "duplicate name penalty",
        _ => "match",
    }
}

/// The part of the term that the text holds, so the mark is visible.
///
/// A path is typed with separators, and the name of a file holds only its
/// last part. Marking that part shows why the row is in the list.
fn highlight_term(text: &str, query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let lower = text.to_lowercase();
    if lower.contains(&query.to_lowercase()) {
        return query.to_owned();
    }
    query
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty() && lower.contains(&part.to_lowercase()))
        .unwrap_or(query)
        .to_owned()
}

#[allow(clippy::too_many_arguments)]
fn picker_result_row(
    ui: &mut egui::Ui,
    name: &str,
    parent: Option<&str>,
    path: &Path,
    folder: bool,
    name_query: &str,
    parent_query: &str,
    font_size: f32,
    selected: bool,
) -> egui::Response {
    let width = ui.available_width();
    let text_left = 38.0;
    let padding = 8.0;
    let gap = 3.0;
    let text_width = (width - text_left - 12.0).max(1.0);
    let layout = |text: &str, query: &str, size: f32, color: Color32| {
        let mut job = chrome::highlighted(text, query, size);
        for section in &mut job.sections {
            if section.format.background == Color32::TRANSPARENT {
                section.format.color = color;
            }
        }
        job.wrap.max_width = text_width;
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        ui.fonts_mut(|fonts| fonts.layout_job(job))
    };
    let name = layout(name, name_query, font_size, chrome::TEXT);
    let parent = parent.map(|parent| {
        layout(
            parent,
            parent_query,
            (font_size - 1.0).max(10.0),
            chrome::MUTED,
        )
    });
    let height =
        padding * 2.0 + name.size().y + parent.as_ref().map_or(0.0, |parent| gap + parent.size().y);
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, height.max(36.0)), egui::Sense::click());
    let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
    if selected || response.hovered() {
        painter.rect_filled(
            rect,
            5.0,
            if selected {
                chrome::SELECTED
            } else {
                chrome::HEADER
            },
        );
    }
    let name_origin = rect.min + egui::vec2(text_left, padding);
    let icon_center = egui::pos2(rect.left() + 19.0, name_origin.y + name.size().y / 2.0);
    if folder {
        chrome::folder_colored(
            &painter,
            egui::Rect::from_center_size(icon_center, egui::vec2(23.0, 23.0)),
            chrome::WARNING,
        );
    } else {
        let (icon, color) = chrome::file_kind(path);
        painter.text(
            icon_center,
            egui::Align2::CENTER_CENTER,
            icon,
            FontId::monospace(12.0),
            color,
        );
    }
    if let Some(parent) = parent {
        painter.galley(
            name_origin + egui::vec2(0.0, name.size().y + gap),
            parent,
            chrome::MUTED,
        );
    }
    painter.galley(name_origin, name, chrome::TEXT);
    response.on_hover_text(path.display().to_string())
}

fn file_candidates(
    recent: &[PathBuf],
    open: &[PathBuf],
    files: &[PathBuf],
    folders: &[PathBuf],
    root: &Path,
    query: &str,
) -> Vec<PathBuf> {
    // Borrow candidate paths; retain at most the visible result limit even for
    // very large repositories. Source order breaks ties in favor of recents.
    let query = query.to_lowercase().replace('\\', "/");
    let mut best: Vec<((u8, usize), &PathBuf)> = Vec::with_capacity(100);
    for (order, path) in recent
        .iter()
        .chain(open)
        .chain(folders)
        .chain(files)
        .enumerate()
    {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let score = if query.is_empty() {
            0
        } else {
            let relative = relative.to_string_lossy().to_lowercase().replace('\\', "/");
            let name = relative.rsplit('/').next().unwrap_or(&relative);
            if name == query || relative == query {
                0
            } else if Path::new(name)
                .file_stem()
                .is_some_and(|stem| stem == query.as_str())
            {
                1
            } else if name.starts_with(&query) {
                2
            } else if name.contains(&query) {
                3
            } else if relative.contains(&query) {
                4
            } else {
                continue;
            }
        };
        let key = (score, order);
        let index = best.partition_point(|(existing, _)| *existing <= key);
        if index < 100 && !best.iter().any(|(_, existing)| *existing == path) {
            if best.len() == 100 {
                best.pop();
            }
            best.insert(index, (key, path));
        }
        if query.is_empty() && best.len() == 100 {
            break;
        }
    }
    best.into_iter().map(|(_, path)| path.clone()).collect()
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

fn editor_diff_target(targets: &[usize], line: usize, next: bool) -> Option<usize> {
    if next {
        targets
            .iter()
            .copied()
            .find(|target| *target > line)
            .or_else(|| targets.last().copied())
    } else {
        targets
            .iter()
            .copied()
            .rev()
            .find(|target| *target < line)
            .or_else(|| targets.first().copied())
    }
}

fn editor_diff_navigation(ui: &mut egui::Ui, targets: &[usize], line: usize) -> Option<usize> {
    if targets.is_empty() {
        return None;
    }
    let mut target = None;
    let bar = egui::Frame::new()
        .fill(chrome::TABS)
        .inner_margin(egui::Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("Branch changes")
                        .size(11.5)
                        .color(chrome::MUTED),
                )
                .on_hover_text("Changes in this file against the branch base");
                egui::Frame::new()
                    .fill(chrome::PANEL)
                    .corner_radius(5)
                    .inner_margin(2)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.horizontal(|ui| {
                            for (label, next, hint) in
                                [("↑", false, "Previous change"), ("↓", true, "Next change")]
                            {
                                let enabled = if next {
                                    targets.last().is_some_and(|last| line < *last)
                                } else {
                                    targets.first().is_some_and(|first| line > *first)
                                };
                                if ui
                                    .add_enabled(
                                        enabled,
                                        egui::Button::new(RichText::new(label).size(15.0))
                                            .min_size(egui::vec2(24.0, 22.0))
                                            .frame(false),
                                    )
                                    .on_hover_text(hint)
                                    .clicked()
                                {
                                    target = editor_diff_target(targets, line, next);
                                }
                            }
                            let current = targets.partition_point(|target| *target <= line);
                            ui.add_space(3.0);
                            ui.label(
                                RichText::new(format!("{current} / {}", targets.len()))
                                    .size(11.5)
                                    .color(chrome::TEXT),
                            )
                            .on_hover_text("Current change / total changes in this file");
                            ui.add_space(4.0);
                        });
                    });
            });
        });
    ui.painter().hline(
        bar.response.rect.x_range(),
        bar.response.rect.bottom(),
        egui::Stroke::new(1.0_f32, chrome::BORDER),
    );
    target
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

fn search_result_label(location: &Location, root: &Path, open: bool) -> String {
    let path = location.path.strip_prefix(root).unwrap_or(&location.path);
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    format!(
        "{}{}:{}{}",
        if open { "Open · " } else { "" },
        name,
        location.line,
        parent.map_or_else(String::new, |parent| format!(" · {}", parent.display()))
    )
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

fn markdown_preview(ui: &mut egui::Ui, cache: &mut MarkdownCache, text: &str, path: &Path) {
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
                // A document that could not be resolved to an absolute path
                // has no directory to resolve the images against. The preview
                // is still shown, without the images.
                let directory =
                    url::Url::from_directory_path(path.parent().unwrap_or(Path::new("."))).ok();
                let base = directory.clone();
                let html = move |ui: &mut egui::Ui, source: &str| {
                    // Render inert image tags only; never execute embedded HTML or scripts.
                    let render = |ui: &mut egui::Ui| {
                        for image in html_images(source) {
                            let Some(base) = &base else {
                                continue;
                            };
                            let Ok(uri) = base.join(&image.src) else {
                                continue;
                            };
                            if !matches!(uri.scheme(), "http" | "https" | "file") {
                                continue;
                            }
                            let mut widget = egui::Image::new(uri.to_string())
                                .fit_to_original_size(1.0)
                                .max_width(ui.available_width())
                                .maintain_aspect_ratio(true);
                            if let Some(width) = image.width {
                                widget = widget.max_width(width.min(ui.available_width()));
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
                cache.prepare(text);
                let mut viewer = CommonMarkViewer::new()
                    .max_image_width(Some(ui.available_width().max(1.0) as usize))
                    .render_html_fn(Some(&html));
                if let Some(directory) = &directory {
                    viewer = viewer.default_implicit_uri_scheme(directory.to_string());
                }
                viewer.show_events(ui, &mut cache.native, cache.events.iter().cloned());
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

/// egui stores a full copy of the text for every undo point, so a large file
/// would otherwise keep hundreds of copies of itself. The budget keeps the
/// undo history of one file near a megabyte.
fn undo_budget(len: usize) -> usize {
    const MIB: usize = 1024 * 1024;
    if len <= 64 * 1024 {
        100
    } else if len <= 256 * 1024 {
        40
    } else if len <= MIB {
        16
    } else if len <= 4 * MIB {
        8
    } else {
        4
    }
}

/// Give the editor an undo history that fits the size of the file. The
/// history is only replaced when the budget changes, so typing does not throw
/// away the undo stack.
fn apply_undo_budget(ui: &egui::Ui, id: egui::Id, len: usize) {
    let budget = undo_budget(len);
    let key = id.with("undo-budget");
    if ui.ctx().data(|data| data.get_temp::<usize>(key)) == Some(budget) {
        return;
    }
    let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
    let settings = egui::util::undoer::Settings {
        max_undos: budget,
        ..Default::default()
    };
    let undoer: egui::util::undoer::Undoer<(egui::text::CCursorRange, String)> =
        egui::util::undoer::Undoer::with_settings(settings);
    state.set_undoer(undoer);
    state.store(ui.ctx(), id);
    ui.ctx().data_mut(|data| data.insert_temp(key, budget));
}

/// Drop the undo history of a closed file, so its text is not kept in the
/// persisted editor state.
fn release_editor_state(ctx: &egui::Context, id: egui::Id) {
    ctx.data_mut(|data| {
        if let Some(mut state) = data.get_persisted::<egui::widgets::text_edit::TextEditState>(id) {
            state.clear_undoer();
            data.insert_persisted(id, state);
        }
    });
}

/// The syntax colors of a document, kept between frames.
///
/// The text is parsed and laid out once per change instead of once per
/// frame, and the editor and the panels share one copy of the result.
#[derive(Clone)]
struct SyntaxCache {
    path: PathBuf,
    /// The fingerprint of the text the colors belong to.
    fingerprint: u64,
    extension: String,
    dark: bool,
    /// The size of the editor font, which the colors are laid out for.
    size: f32,
    job: Arc<egui::text::LayoutJob>,
    regions: Arc<Vec<crate::folding::Region>>,
}

/// The syntax colors of the text, computed when the text changed.
///
/// The caller passes the fingerprint of the text, so the text is not hashed
/// again for every panel that shows it.
fn syntax_cache(
    ui: &egui::Ui,
    path: &Path,
    text: &str,
    extension: &str,
    fingerprint: u64,
) -> SyntaxCache {
    // One entry for the editor, so the memory of a file is released when
    // another one is shown.
    let id = egui::Id::new("editor-syntax");
    let dark = ui.visuals().dark_mode;
    let size = crate::preferences::editor_font_size(ui.ctx());
    let cached = ui.ctx().data_mut(|data| data.get_temp::<SyntaxCache>(id));
    if let Some(cache) = cached
        && cache.path == path
        && cache.fingerprint == fingerprint
        && cache.extension == extension
        && cache.dark == dark
        && cache.size == size
    {
        return cache;
    }
    let job = crate::highlighting::highlight(ui, text, extension);
    let structure = crate::editor_structure::Structure::build(&job);
    let regions = crate::folding::regions(&job.text, &structure, extension);
    let cache = SyntaxCache {
        path: path.to_path_buf(),
        fingerprint,
        extension: extension.to_owned(),
        dark,
        size,
        job: job.clone(),
        regions: Arc::new(regions),
    };
    ui.ctx()
        .data_mut(|data| data.insert_temp(id, cache.clone()));
    cache
}

fn text_editor(
    ui: &mut egui::Ui,
    text: &mut dyn egui::TextBuffer,
    path: &Path,
    cursor: &mut usize,
    jump: &mut Option<(usize, usize)>,
    editor_id: &mut Option<egui::Id>,
    find: EditorFind<'_>,
) -> (bool, Option<TextClick>, Option<crate::git::BlameLine>) {
    let id = ui.make_persistent_id("source-editor");
    let font_size = crate::preferences::editor_font_size(ui.ctx());
    *editor_id = Some(id);
    apply_undo_budget(ui, id, text.as_str().len());
    let find_offset = find
        .active
        .as_ref()
        .filter(|_| find.scroll && !find.query.is_empty())
        .map(|range| {
            text.as_str()[..range.start.min(text.as_str().len())]
                .chars()
                .count()
        });
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
        .map(|(line, column)| navigation::char_offset(text.as_str(), line, column));
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
            text.as_str()
                .chars()
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
    // One fingerprint of the document per frame, shared by the fold state
    // and the layout cache.
    let source_hash = egui::util::hash(text.as_str());
    folds.sync(source_hash);
    let large = crate::editor_analysis::is_large(text.as_str());
    let analysis = large
        .then(|| crate::editor_analysis::get(ui, text.as_str(), path))
        .flatten();
    let small_regions: Arc<Vec<crate::folding::Region>> = if large {
        Arc::new(Vec::new())
    } else {
        syntax_cache(ui, path, text.as_str(), extension, source_hash).regions
    };
    let fold_regions = analysis
        .as_ref()
        .map_or(small_regions.as_slice(), |a| a.folds.as_slice());
    if let Some(offset) = jump_offset.or(find_offset) {
        folds.reveal(fold_regions, offset..=offset);
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
    let active_folds = folds.active(fold_regions);
    let large_layout = large.then(|| crate::editor_analysis::layout_cache(ui, id));
    let folded = egui::util::hash(&folds.collapsed);
    let mut layouter = |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, _wrap_width: f32| {
        // The buffer is the text we were handed, so its fingerprint is known.
        let buffer_hash = source_hash;
        let syntax = analysis
            .as_ref()
            .filter(|a| a.syntax.text.len() == buffer.as_str().len());
        let layout_key = egui::util::hash((
            buffer_hash,
            syntax.map(|a| a.key),
            font_size.to_bits(),
            ui.visuals().dark_mode,
            ui.ctx().pixels_per_point().to_bits(),
            occurrence,
            &find.active,
            folded,
        ));
        if let Some(cache) = &large_layout
            && let Some(cached) = cache.lock().unwrap().as_ref()
            && cached.key == layout_key
        {
            return cached.galley.clone();
        }
        let mut job = if large {
            syntax.map_or_else(
                || {
                    egui::text::LayoutJob::simple(
                        buffer.as_str().to_owned(),
                        FontId::monospace(font_size),
                        if ui.visuals().dark_mode {
                            chrome::TEXT
                        } else {
                            Color32::BLACK
                        },
                        f32::INFINITY,
                    )
                },
                |a| a.syntax.clone(),
            )
        } else {
            // The job is shared with the panels that show the same text, so
            // it is copied only when it is changed here.
            let mut shared = crate::highlighting::highlight(ui, buffer.as_str(), extension);
            std::mem::take(&mut *Arc::make_mut(&mut shared))
        };
        for section in &mut job.sections {
            section.format.font_id.size = font_size;
        }
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
        let galley = if buffer_hash == source_hash {
            crate::folding::layout(galley, &active_folds)
        } else {
            galley
        };
        if let Some(cache) = &large_layout {
            *cache.lock().unwrap() = Some(crate::editor_analysis::CachedLayout {
                key: layout_key,
                galley: galley.clone(),
            });
        }
        galley
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
                let output = egui::TextEdit::multiline(&mut *text)
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
                        hovered_token(text.as_str(), &output.galley, pointer - output.galley_pos)
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
                        clicked_token = Some(TextClick {
                            char_index: range.start,
                            token,
                        });
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
                            chrome::HEADER.gamma_multiply(0.6)
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
                let small_structure = if large {
                    crate::editor_structure::Structure::default()
                } else {
                    ui.ctx().memory_mut(|memory| {
                        memory
                            .caches
                            .cache::<egui::cache::FrameCache<
                                crate::editor_structure::Structure,
                                crate::editor_structure::Computer,
                            >>()
                            .get(output.galley.job.as_ref())
                    })
                };
                let structure = analysis.as_ref().map_or(&small_structure, |a| &a.structure);
                let active_pair = structure.matching(*cursor);
                let caret_line = output
                    .galley
                    .layout_from_cursor(egui::text::CCursor::new(*cursor))
                    .row;
                let active_guide = structure.active_guide(caret_line);
                for guide in &structure.guides {
                    if let (Some(start), Some(end)) = (
                        output.galley.rows.get(guide.start),
                        output.galley.rows.get(guide.end),
                    ) {
                        if start.size.y <= 0.0 {
                            continue;
                        }
                        let top = (output.galley_pos.y + start.pos.y + start.size.y)
                            .max(ui.clip_rect().top());
                        let bottom = (output.galley_pos.y
                            + end.pos.y
                            + if guide.inclusive_end { end.size.y } else { 0.0 })
                        .min(ui.clip_rect().bottom());
                        if top < bottom {
                            let x = output
                                .galley
                                .pos_from_cursor(egui::text::CCursor::new(guide.anchor))
                                .left()
                                + output.galley_pos.x;
                            guides.push(egui::Shape::line_segment(
                                [egui::pos2(x, top), egui::pos2(x, bottom)],
                                egui::Stroke::new(
                                    if active_guide
                                        .is_some_and(|active| std::ptr::eq(active, guide))
                                    {
                                        2.0_f32
                                    } else {
                                        1.0_f32
                                    },
                                    if active_guide
                                        .is_some_and(|active| std::ptr::eq(active, guide))
                                    {
                                        chrome::BLUE
                                    } else {
                                        guide_color
                                    },
                                ),
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
                for region in fold_regions {
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
                    if folds.reveal(fold_regions, range.start..=range.end) {
                        ui.ctx().request_repaint();
                    }
                }
                let data = find
                    .git
                    .filter(|data| data.path == path && data.text == text.as_str());
                let mut git_shapes = vec![];
                let mut marks = data.map(|data| data.marks.iter().peekable());
                let mut previous_blame: Option<&crate::git::BlameLine> = None;
                let mut group_row = 0;
                let first_row = output.galley.rows.partition_point(|row| {
                    output.galley_pos.y + row.pos.y + row.size.y < ui.clip_rect().top()
                });
                for (index, row) in output
                    .galley
                    .rows
                    .iter()
                    .enumerate()
                    .skip(first_row)
                    .take_while(|(_, row)| {
                        output.galley_pos.y + row.pos.y <= ui.clip_rect().bottom()
                    })
                {
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
    // An edit invalidates the collapsed regions, so the fingerprint is taken
    // again only when the text actually changed.
    let changed_hash = if changed {
        egui::util::hash(text.as_str())
    } else {
        source_hash
    };
    folds.sync(changed_hash);
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
                    // Walk back over the identifier without materializing the
                    // whole document as a character vector.
                    let byte = text
                        .char_indices()
                        .nth(index)
                        .map_or(text.len(), |(byte, _)| byte);
                    let mut start = index;
                    for character in text[..byte].chars().rev() {
                        if character.is_alphanumeric() || character == '_' {
                            start -= 1;
                        } else {
                            break;
                        }
                    }
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

    #[test]
    fn terminal_links_resolve_from_directory_or_project_and_prefill_search() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested)?;
        std::fs::write(nested.join("main.rs"), "fn main() {}\n")?;
        std::fs::write(dir.path().join("root.rs"), "// root\n")?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        for (query, expected) in [
            ("main.rs:42:43", nested.join("main.rs")),
            ("root.rs:42-7", dir.path().join("root.rs")),
        ] {
            app.terminal.maximized = true;
            app.open_terminal_link(crate::terminal::FileLink {
                query: query.into(),
                directory: nested.clone(),
            });
            assert_eq!(app.documents[app.active].path, expected.canonicalize()?);
            assert_eq!(app.documents[app.active].jump, Some((42, 1)));
            assert!(!app.terminal.maximized);
        }
        app.open_terminal_link(crate::terminal::FileLink {
            query: "src/missing.rs:42:43-50".into(),
            directory: nested,
        });
        assert!(app.picker == Some(Picker::Files));
        assert_eq!(app.picker_query, "src/missing.rs:42");
        assert!(app.picker_focus);
        assert_eq!(app.documents.len(), 2);
        Ok(())
    }
    #[test]
    fn terminal_paths_resolve_folders_absolute_ancestors_and_unambiguous_suffixes()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let nested = root.join("nested");
        let deep = nested.join("build/deep");
        let source = nested.join("src/main.rs");
        std::fs::create_dir_all(&deep)?;
        std::fs::create_dir_all(source.parent().unwrap())?;
        std::fs::write(&source, "fn main() {}\n")?;
        let mut app = Coder::empty(root.clone());
        app.files = vec![source.clone()];
        app.folders = vec![nested.clone(), nested.join("src")];
        for (query, expected) in [
            (source.to_string_lossy().into_owned(), source.clone()),
            (nested.to_string_lossy().into_owned(), nested.clone()),
            ("src/main.rs".into(), source.clone()),
            ("main.rs".into(), source.clone()),
            ("../../src/main.rs".into(), source.clone()),
            ("a/nested/src/main.rs".into(), source.clone()),
            (
                format!(
                    "{}/nested/src/main.rs",
                    root.file_name().unwrap().to_string_lossy()
                ),
                source.clone(),
            ),
        ] {
            assert_eq!(
                app.resolve_terminal_path(&query, &deep),
                Some(expected),
                "{query}"
            );
        }
        app.open_terminal_link(crate::terminal::FileLink {
            query: format!("{}.", nested.display()),
            directory: deep.clone(),
        });
        assert!(app.tree.reveal_pending());
        assert!(app.show_project && !app.show_structure && !app.git.visible);
        assert!(app.documents.is_empty());
        assert_eq!(app.root, root);
        let other = root.join("other/main.rs");
        std::fs::create_dir_all(other.parent().unwrap())?;
        std::fs::write(&other, "// other\n")?;
        app.files.push(other);
        assert_eq!(app.resolve_terminal_path("main.rs", &deep), None);
        let external = tempfile::tempdir()?;
        app.open(source, None);
        app.documents[0].dirty = true;
        app.terminal.maximized = true;
        app.open_terminal_link(crate::terminal::FileLink {
            query: external.path().to_string_lossy().into_owned(),
            directory: deep,
        });
        assert!(
            matches!(app.actions.last(), Some(FrameAction::OpenProjectInstance(path)) if *path == external.path().canonicalize().unwrap())
        );
        assert!(app.pending.is_none());
        assert_eq!(app.root, root);
        assert!(app.documents[0].dirty);
        assert!(
            app.terminal.maximized,
            "external folders must leave this instance's layout intact"
        );
        let command = project_instance_command(
            Path::new("/Applications/Glimcode.app/Contents/MacOS/glimcode"),
            external.path(),
        );
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![external.path().as_os_str()]
        );
        assert_eq!(command.get_current_dir(), Some(external.path()));
        Ok(())
    }

    use crate::search_view::{extension, result_visible};
    #[test]
    fn review_revert_updates_the_buffer_and_rejects_a_stale_diff() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("sample.rs");
        std::fs::write(&path, "current\n")?;
        let path = path.canonicalize()?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        let ctx = egui::Context::default();
        app.apply_review_revert(
            crate::git_view::RevertHunk {
                path: path.clone(),
                expected: "current\n".into(),
                replacement: "base\n".into(),
                line: 0,
                undo: false,
            },
            &ctx,
        );
        assert!(app.error.is_none());
        assert!(app.documents[app.active].dirty);
        assert_eq!(app.documents[app.active].source_text()?, "base\n");
        assert_eq!(std::fs::read_to_string(&path)?, "current\n");
        app.apply_review_revert(
            crate::git_view::RevertHunk {
                path: path.clone(),
                expected: "current\n".into(),
                replacement: "stale\n".into(),
                line: 0,
                undo: true,
            },
            &ctx,
        );
        assert!(app.error.as_deref().unwrap().contains("changed since"));
        assert_eq!(app.documents[app.active].source_text()?, "base\n");
        app.documents[app.active].save()?;
        assert_eq!(std::fs::read_to_string(path)?, "base\n");
        Ok(())
    }

    #[test]
    fn project_timer_tracks_external_creates_renames_and_deletes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        std::fs::create_dir(root.join("src"))?;
        std::fs::write(root.join("src/old.rs"), "fn old() {}")?;
        let mut app = Coder::empty(root.clone());
        let ctx = egui::Context::default();
        let refresh = |app: &mut Coder| {
            app.files_checked = Instant::now() - PROJECT_REFRESH_INTERVAL;
            app.poll(&ctx);
            assert!(app.files_rx.is_some());
            let started = app.files_checked;
            app.refresh_files();
            assert_eq!(app.files_checked, started, "an in-flight scan is reused");
            let deadline = Instant::now() + Duration::from_secs(10);
            while app.files_rx.is_some() {
                app.poll(&ctx);
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        refresh(&mut app);
        std::fs::create_dir(root.join("src/nested"))?;
        let new = root.join("src/nested/external.rs");
        std::fs::write(&new, "fn external() {}")?;
        refresh(&mut app);
        assert!(app.files.contains(&new));
        assert!(app.folders.contains(&root.join("src/nested")));
        app.tree.expand_all(true);
        let mut output = None;
        for _ in 0..3 {
            output = Some(ctx.run(Default::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    app.tree.show(ui, &root, None, "", &Default::default());
                });
            }));
        }
        assert!(
            output
                .unwrap()
                .shapes
                .iter()
                .any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text() == "external.rs"))
        );
        let renamed = new.with_file_name("renamed.rs");
        std::fs::rename(&new, &renamed)?;
        refresh(&mut app);
        assert!(!app.files.contains(&new));
        assert!(app.files.contains(&renamed));
        std::fs::remove_file(&renamed)?;
        refresh(&mut app);
        assert!(!app.files.contains(&renamed));
        assert!(app.error.is_none());
        Ok(())
    }

    #[test]
    fn the_picker_marks_the_part_of_the_term_that_is_in_the_row() {
        // The term is in the name.
        assert_eq!(highlight_term("app.rs", "app"), "app");
        // A path is typed with separators, and the name holds the last part.
        assert_eq!(highlight_term("app.rs", "src/app"), "app");
        // The term is in the directory.
        assert_eq!(highlight_term("src", "src/app"), "src");
        // Nothing of the term is in the text, so the term is kept.
        assert_eq!(highlight_term("main.go", "zzz"), "zzz");
        assert_eq!(highlight_term("main.go", ""), "");
    }
    #[test]
    fn search_rows_disambiguate_names_and_explain_empty_filters() {
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let mut app = Coder::empty(PathBuf::from("/project"));
        app.query = "needle".into();
        for parent in ["src/client", "src/server"] {
            app.results.push(Location {
                path: PathBuf::from(format!("/project/{parent}/index.ts")),
                line: 7,
                column: 1,
                preview: "needle()".into(),
                category: navigation::MatchCategory::Declaration,
            });
        }
        app.results_revision = 1;
        let frame = |app: &mut Coder| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(850.0, 650.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| app.search_contents(ui, true));
                },
            )
        };
        frame(&mut app);
        let output = frame(&mut app);
        for expected in ["index.ts:7 · src/client", "index.ts:7 · src/server"] {
            assert!(
                output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.text() == expected)),
                "missing {expected}"
            );
        }
        app.selected_category = Some(navigation::MatchCategory::Comment);
        let output = frame(&mut app);
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text().contains("No results match these filters"))));
        app.results.clear();
        app.results_revision += 1;
        let output = frame(&mut app);
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.text().contains("No matches found"))));
    }

    #[test]
    fn search_results_and_filter_fonts_update_independently_in_both_layouts() {
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let mut app = Coder::empty(PathBuf::from("/project"));
        app.query = "needle".into();
        app.results = vec![Location {
            path: "/project/sample.ts".into(),
            line: 7,
            column: 1,
            preview: "needle value".into(),
            category: navigation::MatchCategory::Declaration,
        }];
        app.results_revision = 1;
        for (docked, results, filters) in [(false, 23.0, 11.0), (true, 12.0, 22.0)] {
            crate::preferences::Settings {
                editor_font_size: 16.0,
                dialog_font_size: 19.0,
                search_results_font_size: results,
                search_filters_font_size: filters,
                ..Default::default()
            }
            .apply(&ctx);
            let mut frame = || {
                ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1800.0, 1200.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default()
                            .show(ctx, |ui| app.search_contents(ui, docked));
                    },
                )
            };
            frame();
            let output = frame();
            let size = |label: &str| {
                output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.text() == label => {
                            Some(text.galley.job.sections[0].format.font_id.size)
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("missing label: {label}"))
            };
            assert_eq!(size("needle value"), results);
            assert_eq!(size("sample.ts:7"), results);
            assert_eq!(size("All 1"), filters);
            assert_eq!(size("Declarations 1"), filters);
            assert_eq!(size("Opened"), filters);
            assert_eq!(size("Search"), 19.0);
            assert_eq!(crate::preferences::editor_font_size(&ctx), 16.0);
        }
    }

    #[test]
    #[ignore = "large-file performance check; run with --release --ignored --nocapture"]
    fn large_typescript_editor_benchmark() {
        let ctx = egui::Context::default();
        let mut source = (0..17_500).map(|i| format!(
            "export function item{i}(input: string): string {{\n    const value = input.trim(); // sample content with some text and additional context\n    return value + \"item{i}\";\n}}\n"
        )).collect::<String>();
        let mut cursor = 0;
        let mut jump = Some((1, 1));
        let mut editor_id = None;
        let mut time = 0.0;
        let mut frame = |source: &mut String, events, target: Option<(usize, usize)>| {
            if let Some(target) = target {
                jump = Some(target);
            }
            time += 0.05;
            let start = Instant::now();
            let mut ready = false;
            let output = ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1400.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        text_editor(
                            ui,
                            source,
                            Path::new("large.ts"),
                            &mut cursor,
                            &mut jump,
                            &mut editor_id,
                            EditorFind::default(),
                        );
                        ready = crate::editor_analysis::get(ui, source, Path::new("large.ts"))
                            .is_some();
                    });
                },
            );
            (start.elapsed(), output, ready)
        };
        eprintln!(
            "Source: {} bytes / {} lines",
            source.len(),
            source.lines().count()
        );
        let (initial, _, _) = frame(&mut source, vec![], None);
        let (steady, _, _) = frame(&mut source, vec![], None);
        // Editing must preserve the full buffer, including the final lines.
        let before = source.clone();
        let id = ctx
            .memory(|memory| memory.focused())
            .unwrap_or_else(|| egui::Id::new("source-editor"));
        ctx.memory_mut(|memory| memory.request_focus(id));
        let (typing, _, _) = frame(&mut source, vec![egui::Event::Text("猫".into())], None);
        assert_eq!(source, format!("猫{before}"));
        eprintln!("Initial: {initial:?}; steady: {steady:?}; typing: {typing:?}");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let (_, _, ready) = frame(&mut source, vec![], None);
            if ready {
                break;
            }
            assert!(Instant::now() < deadline, "syntax analysis did not finish");
            std::thread::sleep(Duration::from_millis(20));
        }
        let (highlighted, _, _) = frame(&mut source, vec![], None);
        let (highlighted_steady, _, _) = frame(&mut source, vec![], None);
        eprintln!("Highlighted: {highlighted:?}; highlighted steady: {highlighted_steady:?}");
        let (jump_time, _, _) = frame(&mut source, vec![], Some((70_000, 1)));
        let (bottom_frame, _, _) = frame(&mut source, vec![], None);
        eprintln!("Jump to EOF: {jump_time:?}; bottom frame: {bottom_frame:?}");
        assert!(bottom_frame < Duration::from_millis(200));
        drop(frame);
        assert_eq!(cursor, navigation::char_offset(&source, 70_000, 1));
        assert!(highlighted_steady < Duration::from_millis(200));
        assert!(
            steady < Duration::from_millis(200),
            "steady frame: {steady:?}"
        );
        assert!(typing < Duration::from_secs(1), "typing frame: {typing:?}");
    }

    #[test]
    fn editor_diff_buttons_navigate_from_the_caret_and_clamp_at_boundaries() {
        let targets = [2, 20, 45];
        for (line, next, expected) in [
            (0, true, 2),
            (2, true, 20),
            (30, true, 45),
            (99, true, 45),
            (99, false, 45),
            (45, false, 20),
            (10, false, 2),
            (0, false, 2),
        ] {
            assert_eq!(editor_diff_target(&targets, line, next), Some(expected));
        }
        assert_eq!(editor_diff_target(&[], 0, true), None);
        let ctx = egui::Context::default();
        let frame = |events| {
            let mut target = None;
            let output = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        target = editor_diff_navigation(ui, &targets, 2);
                    });
                },
            );
            (output, target)
        };
        frame(vec![]);
        let (output, _) = frame(vec![]);
        let point = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "↓" => {
                    Some(text.pos + text.galley.size() / 2.0)
                }
                _ => None,
            })
            .unwrap();
        for pressed in [true, false] {
            let (_, target) = frame(vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ]);
            if !pressed {
                assert_eq!(target, Some(20));
            }
        }
    }

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
            change_lines: vec![],
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
    fn maximized_terminal_replaces_editor_and_restores_docked_height() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("keep-open.rs");
        std::fs::write(&path, "fn preserved() {}\n")?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        let path = path.canonicalize()?;
        app.open(path.clone(), None);
        app.terminal.settings.shell = "/bin/sh".into();
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        app.terminal.new_tab(dir.path(), &ctx);
        let frame = |app: &mut Coder| {
            for _ in 0..3 {
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
            ctx.read_response(egui::Id::new(("terminal-input", 1_u64)))
                .unwrap()
                .rect
        };
        let docked = frame(&mut app);
        app.terminal.maximize(dir.path(), &ctx);
        let expanded = frame(&mut app);
        assert!(expanded.height() > docked.height() + 200.0);
        assert!(expanded.top() < docked.top());
        assert_eq!(app.documents[app.active].path, path);
        assert_eq!(
            app.terminal.snapshot()["roots"].as_array().unwrap().len(),
            1
        );
        app.terminal.maximized = false;
        let restored = frame(&mut app);
        assert!((restored.height() - docked.height()).abs() < 1.0);
        assert_eq!(app.documents[app.active].path, path);
        app.terminal.visible = false;
        app.terminal.maximize(dir.path(), &ctx);
        let expanded_from_hidden = frame(&mut app);
        assert!(app.terminal.visible);
        assert!(app.terminal.focused(&ctx));
        assert!((expanded_from_hidden.height() - expanded.height()).abs() < 1.0);
        assert_eq!(app.documents[app.active].path, path);
        assert_eq!(
            app.terminal.snapshot()["roots"].as_array().unwrap().len(),
            1,
            "showing the terminal must reuse its existing session"
        );
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
            "worktrees-panel",
            "explorer",
            "structure",
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
            let horizontal = matches!(
                panel,
                "worktrees-panel" | "explorer" | "structure" | "git-changes-panel"
            );
            assert!(
                (if horizontal {
                    handle.width()
                } else {
                    handle.height()
                }) >= 2.0 * ctx.style().interaction.resize_grab_radius_side
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
                        "idle {panel} at {delta}: original={handle:?} current={:?}",
                        ctx.read_response(id).map(|r| r.rect)
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
    fn unified_files_panel_switches_scope_and_keeps_deleted_changes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let changed = root.join("changed.txt");
        let normal = root.join("normal.txt");
        std::fs::write(&changed, "changed")?;
        std::fs::write(&normal, "normal")?;
        let mut app = Coder::empty(root.clone());
        app.files = vec![changed.clone(), normal];
        app.tree = crate::explorer::FileTree::build_with_folders(&root, &app.files, &app.folders);
        app.git.repo = Some(crate::git::Repository {
            root: root.clone(),
            branch: "feature".into(),
            head: "a".repeat(40),
            base: "main".into(),
            merge_base: None,
            branches: vec!["main".into()],
            remote: None,
            files: vec![
                crate::git::ChangedFile {
                    path: "changed.txt".into(),
                    uncommitted: true,
                    ..Default::default()
                },
                crate::git::ChangedFile {
                    path: "deleted.txt".into(),
                    deleted: true,
                    committed: true,
                    ..Default::default()
                },
            ],
        });
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
                |ctx| app.sidebar(ctx),
            )
        };
        for _ in 0..3 {
            frame(&mut app, vec![]);
        }
        app.open(changed.clone(), None);
        let output = frame(&mut app, vec![]);
        let selected = |output: &egui::FullOutput, label: &str| {
            let position = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.text() == label => Some(text.pos),
                    _ => None,
                })
                .expect("file row is visible");
            output.shapes.iter().any(|shape| matches!(
                &shape.shape,
                egui::Shape::Rect(rect) if rect.fill == chrome::SELECTED && rect.rect.contains(position)
            ))
        };
        assert!(selected(&output, "changed.txt"));
        let has = |output: &egui::FullOutput, label: &str| {
            output.shapes.iter().any(|shape|
            matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == label))
        };
        assert!(has(&output, "normal.txt"));
        let pos = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(text) if text.galley.text() == "Changed · 2" => {
                    Some(text.pos + text.galley.size() / 2.0)
                }
                _ => None,
            })
            .expect("changed filter is visible");
        for pressed in [true, false] {
            frame(
                &mut app,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
        }
        assert!(app.files_changed_only);
        let output = frame(&mut app, vec![]);
        assert!(!has(&output, "normal.txt"));
        assert!(has(&output, "deleted.txt"));
        assert!(
            selected(&output, "changed.txt"),
            "Changed must retain the same selection highlight"
        );
        app.show_file_filter = true;
        app.file_filter = "deleted".into();
        let filtered = frame(&mut app, vec![]);
        assert!(has(&filtered, "deleted.txt"));
        assert!(!has(&filtered, "changed.txt"));
        app.file_filter.clear();
        assert!(
            ctx.read_response(egui::Id::new("git-changes-panel").with("__resize"))
                .is_none()
        );
        app.tree.reveal_active();
        frame(&mut app, vec![]);
        assert!(!app.files_changed_only, "reveal must restore all files");
        Ok(())
    }

    #[test]
    fn right_tool_tabs_and_left_worktree_toggle_are_independent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let frame = |app: &mut Coder, events| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1400.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            );
        };
        for _ in 0..4 {
            frame(&mut app, vec![]);
        }
        let left = ctx
            .read_response(egui::Id::new("worktrees-panel").with("__resize"))
            .unwrap()
            .rect;
        let right = ctx
            .read_response(egui::Id::new("explorer").with("__resize"))
            .unwrap()
            .rect;
        assert!(left.center().x < 700.0 && right.center().x > 700.0);
        let click = |app: &mut Coder, x, y| {
            let pos = egui::pos2(x, y);
            frame(
                app,
                vec![
                    egui::Event::PointerMoved(pos),
                    egui::Event::PointerButton {
                        pos,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
            );
            frame(
                app,
                vec![egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        };
        click(&mut app, 1384.0, 175.0);
        assert!(app.show_structure && !app.show_project && !app.git.visible);
        click(&mut app, 1384.0, 70.0);
        assert!(app.show_project && !app.show_structure && !app.git.visible);
        click(&mut app, 20.0, 65.0);
        assert!(!app.show_worktrees && app.show_project);
        click(&mut app, 20.0, 65.0);
        assert!(app.show_worktrees && app.show_project);
        Ok(())
    }

    #[test]
    fn panel_resize_cursor_stays_stable() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        app.show_worktrees = false;
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
        let edge = ctx
            .read_response(egui::Id::new("explorer").with("__resize"))
            .unwrap()
            .rect
            .center()
            .x as i32;
        for x in edge - 7..edge + 8 {
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
    fn editor_scrollbars_do_not_overlap_panel_resize_boundaries() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("wide.rs");
        std::fs::write(
            &path,
            format!("// {}\n", "long line ".repeat(100)).repeat(80),
        )?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        app.open(path, None);
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
        for _ in 0..4 {
            frame(&mut app, egui::pos2(700.0, 300.0));
        }
        let handle = ctx
            .read_response(egui::Id::new("terminal-panel").with("__resize"))
            .unwrap()
            .rect;
        let editor_id = app.documents[0].editor_id.unwrap();
        let editor = ctx.read_response(editor_id).unwrap();
        assert!(
            editor.interact_rect.bottom() < handle.top(),
            "editor and its scrollbars must end above the splitter: {editor:?}, {handle:?}"
        );
        for delta in -12..=12 {
            let pos = egui::pos2(700.0, handle.center().y + delta as f32);
            let mut icons = Vec::new();
            for _ in 0..12 {
                let output = frame(&mut app, pos);
                icons.push(output.platform_output.cursor_icon);
            }
            assert!(
                icons[2..].windows(2).all(|pair| pair[0] == pair[1]),
                "boundary cursor flickered at {pos:?}: {icons:?}"
            );
            if handle.contains(pos) {
                assert_eq!(*icons.last().unwrap(), egui::CursorIcon::ResizeVertical);
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
    fn search_test_toggle_defaults_on_and_reruns_navigation_search() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let production = root.join("main.ts");
        let test = root.join("main.test.ts");
        for path in [&production, &test] {
            std::fs::write(path, "function needle() {}\n")?;
        }
        let mut app = Coder::empty(root.clone());
        assert!(app.exclude_test_files);
        app.open(production.clone(), None);
        // Cursor navigation and Cmd-click both start the shared search pipeline.
        app.query = "needle".into();
        app.start_search(SearchKind::References, false);
        let collect = |app: &Coder| -> std::collections::BTreeSet<PathBuf> {
            let rx = app.search_rx.as_ref().unwrap();
            let mut paths = std::collections::BTreeSet::new();
            loop {
                match rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("search completed")
                {
                    SearchUpdate::OpenFiles(rows) | SearchUpdate::Project(rows) => {
                        paths.extend(rows.into_iter().map(|row| row.path));
                    }
                    SearchUpdate::Finished { .. } => break,
                    SearchUpdate::Failed(error) => panic!("{error}"),
                }
            }
            paths
        };
        assert_eq!(collect(&app), [production.clone()].into());
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
        let point = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Text(t) if t.galley.text() == "Exclude test files" => {
                    Some(t.pos + t.galley.size() / 2.0)
                }
                _ => None,
            })
            .expect("test filter checkbox");
        for exclude in [false, true] {
            let previous_cancel = app.search_cancel.clone();
            for pressed in [true, false] {
                frame(
                    &mut app,
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
            assert_eq!(app.exclude_test_files, exclude);
            assert!(previous_cancel.load(Ordering::Relaxed));
            assert_eq!(
                collect(&app),
                if exclude {
                    [production.clone()].into()
                } else {
                    [production.clone(), test.clone()].into()
                }
            );
        }
        // Each new search restores exclusion, including cursor navigation and reopening Find.
        app.documents[app.active].cursor = 10; // Inside `needle`.
        for kind in [
            SearchKind::Text,
            SearchKind::References,
            SearchKind::Definitions,
        ] {
            app.exclude_test_files = false;
            app.start_search(kind, true);
            assert!(app.exclude_test_files);
            assert_eq!(collect(&app), [production.clone()].into());
        }
        app.exclude_test_files = false;
        app.open_search();
        assert!(app.exclude_test_files);
        assert_eq!(collect(&app), [production.clone()].into());
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
    fn picker_lists_empty_folders_and_enter_reveals_them_without_opening_a_document()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let folder = root.join("src/nested/empty");
        std::fs::create_dir_all(&folder)?;
        std::fs::write(root.join("src/main.rs"), "fn main() {}")?;
        let paths = navigation::workspace_paths(&root)?;
        assert!(paths.folders.contains(&folder));
        let mut app = Coder::empty(root.clone());
        app.files = paths.files;
        app.folders = paths.folders;
        app.tree = crate::explorer::FileTree::build_with_folders(&root, &app.files, &app.folders);
        app.show_project = false;
        app.show_picker(Picker::Files);
        app.picker_query = "empty".into();
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        let frame = |app: &mut Coder, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(700.0, 700.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| app.draw(ctx),
            )
        };
        frame(&mut app, vec![]);
        let output = frame(&mut app, vec![]);
        for label in ["empty", "src/nested"] {
            assert!(
                output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Text(text) if text.galley.text() == label)),
                "missing {label}"
            );
        }
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
        assert!(app.picker.is_none());
        assert!(app.show_project);
        assert!(app.documents.is_empty());
        assert!(app.tree.reveal_pending());
        frame(&mut app, vec![]);
        assert!(!app.tree.reveal_pending());
        assert!(app.error.is_none());
        Ok(())
    }

    #[test]
    fn picker_rows_keep_icons_text_and_hit_targets_in_separate_bounds_at_every_font_size() {
        for scale in [1.0, 2.0] {
            for font_size in [12.0, 14.0, 20.0, 28.0] {
                for width in [280.0, 620.0] {
                    let ctx = egui::Context::default();
                    chrome::install(&ctx);
                    ctx.set_pixels_per_point(scale);
                    let mut rows = vec![];
                    let output = ctx.run(egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 900.0))),
                        ..Default::default()
                    }, |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            for (index, name) in ["api", "main.go", "a_very_long_file_name_that_should_be_truncated_猫.rs", "main_test.go"].iter().enumerate() {
                                let response = picker_result_row(ui, name,
                                    Some("staging/src/k8s.io/apiserver/pkg/admission/plugin/webhook/testing/long_parent"),
                                    Path::new(name), index == 0, "main", "main", font_size, index == 0);
                                rows.push(response.rect);
                            }
                            assert!(ui.min_rect().bottom() >= rows.last().unwrap().bottom());
                        });
                    });
                    for pair in rows.windows(2) {
                        assert!(
                            pair[0].bottom() <= pair[1].top(),
                            "overlapping result bounds at font {font_size}, scale {scale}"
                        );
                    }
                    let mut text_count = 0;
                    let mut folder_paths = 0;
                    for shape in &output.shapes {
                        match &shape.shape {
                            egui::Shape::Text(text) => {
                                let bounds =
                                    egui::Rect::from_min_size(text.pos, text.galley.size());
                                assert!(
                                    rows.iter().any(|row| row.expand(0.5).contains_rect(bounds)),
                                    "text escapes its result: {} at font {font_size}, scale {scale}",
                                    text.galley.text()
                                );
                                assert_eq!(
                                    text.galley.rows.len(),
                                    1,
                                    "result text must remain one line"
                                );
                                text_count += 1;
                            }
                            egui::Shape::Path(path)
                                if path.stroke.color
                                    == egui::epaint::ColorMode::Solid(chrome::WARNING) =>
                            {
                                folder_paths += 1;
                            }
                            _ => {}
                        }
                    }
                    assert_eq!(text_count, 11); // Two text lines per result plus three file icons.
                    assert_eq!(folder_paths, 1); // A drawn folder, independent of font glyph support.
                }
            }
        }
    }

    #[test]
    fn matching_folders_are_not_capped_out_by_their_contents() {
        let root = Path::new("/project");
        let folder = root.join("sources");
        let files: Vec<_> = (0..150).map(|i| folder.join(format!("{i}.rs"))).collect();
        let found = file_candidates(
            &[],
            &[],
            &files,
            std::slice::from_ref(&folder),
            root,
            "sources",
        );
        assert_eq!(found.len(), 100);
        assert_eq!(found[0], folder);
    }

    #[test]
    fn file_search_ranks_exact_names_before_recent_and_path_only_matches() {
        let root = Path::new("/project");
        let mut files: Vec<_> = (0..500)
            .map(|i| root.join(format!("main/archive/{i}.rs")))
            .collect();
        let exact = root.join("src/main.rs");
        let prefix = root.join("src/main_test.rs");
        files.extend([prefix.clone(), exact.clone()]);
        let recent = files[0].clone();
        let found = file_candidates(&[recent], &[], &files, &[], root, "main");
        assert_eq!(found.len(), 100);
        assert_eq!(found[0], exact);
        assert_eq!(found[1], prefix);
        assert_eq!(
            file_candidates(&[], &[], &files, &[], root, "SRC\\MAIN.RS"),
            vec![exact]
        );
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
                &[],
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
    fn search_keyboard_scrolls_virtual_results_and_wraps_without_a_footer() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("sample.rs");
        std::fs::write(&path, "// preview\n".repeat(60))?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        app.query = "result".into();
        app.results = (0..40)
            .map(|index| Location {
                path: path.clone(),
                line: index + 1,
                column: 1,
                preview: format!("result {index}"),
                category: navigation::MatchCategory::Usage,
            })
            .collect();
        let ctx = egui::Context::default();
        chrome::install(&ctx);
        crate::preferences::Settings {
            search_results_font_size: 30.0,
            ..Default::default()
        }
        .apply(&ctx);
        let mut time = 0.0;
        let mut frame = |app: &mut Coder, key: Option<Key>| {
            time += 0.1;
            let mut id = egui::Id::NULL;
            let mut bounds = egui::Rect::NOTHING;
            let output = ctx.run(
                egui::RawInput {
                    time: Some(time),
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 700.0),
                    )),
                    events: key
                        .into_iter()
                        .map(|key| egui::Event::Key {
                            key,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers: egui::Modifiers::NONE,
                        })
                        .collect(),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        id = ui.make_persistent_id(egui::Id::new("search-matches"));
                        app.search_contents(ui, false);
                        bounds = ui.min_rect();
                    });
                },
            );
            (
                output,
                egui::scroll_area::State::load(&ctx, id)
                    .unwrap_or_default()
                    .offset
                    .y,
                bounds,
            )
        };
        for _ in 0..3 {
            frame(&mut app, None);
        }
        for _ in 0..25 {
            frame(&mut app, Some(Key::ArrowDown));
        }
        let (output, offset, bounds) = frame(&mut app, None);
        assert_eq!(app.result_selection, 25);
        assert!(offset > 0.0, "Down must scroll past the last visible row");
        assert!(
            bounds.bottom() <= 700.0,
            "search content must fit its height: {bounds:?}"
        );
        assert!(output.shapes.iter().any(
            |shape| matches!(&shape.shape, egui::Shape::Text(t) if t.galley.text() == "result 25")
        ));
        assert!(!output.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(t) if t.galley.text() == "Open in Editor")));
        app.result_selection = 39;
        frame(&mut app, Some(Key::ArrowDown));
        assert_eq!(app.result_selection, 0);
        assert_eq!(frame(&mut app, None).1, 0.0);
        frame(&mut app, Some(Key::ArrowUp));
        assert_eq!(app.result_selection, 39);
        assert!(frame(&mut app, None).1 > 0.0);
        Ok(())
    }

    #[test]
    fn search_preview_and_navigation_use_disk_for_clean_tabs_and_buffers_for_edits()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let path = root.join("changed.rs");
        let other = root.join("other.rs");
        std::fs::write(&path, "fn needle() {}\n")?;
        std::fs::write(&other, "fn other() {}\n")?;
        let mut app = Coder::empty(root);
        app.open(path.clone(), None);
        app.open(other, None);
        let ctx = egui::Context::default();
        chrome::install(&ctx);

        for (disk, buffer) in [
            ("// external\n\nfn needle() {}\n", None),
            ("// changed again\n\n\nfn needle() {}\n", None),
            (
                "// disk differs\n\n\n\nfn needle() {}\n",
                Some("// unsaved\nfn needle() {}\n"),
            ),
        ] {
            app.active = 1; // Leave the changed file in an inactive tab.
            std::fs::write(&path, disk)?;
            if let Some(buffer) = buffer {
                app.documents[0].content = Content::Text(buffer.into());
                app.documents[0].dirty = true;
                app.documents[0].mark_changed();
            }
            let expected = buffer.unwrap_or(disk);
            app.query = "needle".into();
            app.run_search(SearchKind::References, false);
            wait_for_search(&mut app, &ctx);
            assert!(app.search_rx.is_none());
            assert_eq!(app.results.len(), 1);
            let location = app.results[0].clone();
            assert_eq!(location.path, path);
            let line = expected
                .lines()
                .position(|line| line.contains("needle"))
                .unwrap()
                + 1;
            assert_eq!(location.line, line);

            let started = Instant::now();
            loop {
                app.preview.poll();
                let _ = ctx.run(egui::RawInput::default(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        app.search_preview(ui, &location, 300.0);
                    });
                });
                if app
                    .preview
                    .current
                    .as_ref()
                    .is_some_and(|preview| preview.source() == expected)
                {
                    break;
                }
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "preview must use the same content as search"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                app.preview.current.as_ref().unwrap().line_text(line - 1),
                "fn needle() {}"
            );
            app.navigate(location);
            assert_eq!(app.documents[app.active].source_text()?, expected);
            assert_eq!(app.documents[app.active].jump, Some((line, 4)));
            assert_eq!(app.documents[app.active].dirty, buffer.is_some());
        }
        Ok(())
    }

    #[test]
    fn styled_search_windows_render_preview_and_file_picker() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("sample.rs");
        std::fs::write(&path, "fn sample() {}\n")?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        app.open(path.clone(), None);
        app.files = vec![path.clone()];
        app.tree =
            crate::explorer::FileTree::build_with_folders(&app.root, &app.files, &app.folders);
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
    fn cmd_click_opens_the_search_dialog_on_the_tab_that_matches_the_intent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let declaration = root.join("declaration.rs");
        let usage = root.join("usage.rs");
        std::fs::write(&declaration, "fn needle() {}\n").unwrap();
        std::fs::write(&usage, "fn other() { needle(); }\n").unwrap();
        let mut app = Coder::empty(root.clone());
        app.open(usage.clone(), None);
        let ctx = egui::Context::default();
        chrome::install(&ctx);

        // A click on a usage asks for the declarations.
        let source = std::fs::read_to_string(&usage).unwrap();
        app.click_search(TextClick {
            char_index: source.find("needle").unwrap(),
            token: "needle".into(),
        });
        assert!(app.search_open, "the click must open the search dialog");
        assert!(matches!(
            app.selected_category,
            Some(navigation::MatchCategory::Declaration)
        ));
        wait_for_search(&mut app, &ctx);
        assert!(matches!(
            app.selected_category,
            Some(navigation::MatchCategory::Declaration)
        ));
        // The best declaration is ranked first and focused.
        wait_for_focus(&mut app);
        app.search_view.refresh(
            &app.results,
            app.results_revision,
            app.selected_category,
            app.extension_filter.as_deref(),
        );
        let focused = &app.results[app.search_view.indices[app.result_selection]];
        assert_eq!(focused.path, declaration);

        // A click on the declaration asks for the usages.
        app.open(declaration.clone(), None);
        let source = std::fs::read_to_string(&declaration).unwrap();
        app.click_search(TextClick {
            char_index: source.find("needle").unwrap(),
            token: "needle".into(),
        });
        assert!(matches!(
            app.selected_category,
            Some(navigation::MatchCategory::Usage)
        ));
        wait_for_search(&mut app, &ctx);
        assert!(matches!(
            app.selected_category,
            Some(navigation::MatchCategory::Usage)
        ));
        assert!(app.results.iter().any(|location| location.path == usage
            && location.category == navigation::MatchCategory::Usage));
    }

    /// Wait until the background search of a click finished.
    fn wait_for_search(app: &mut Coder, ctx: &egui::Context) {
        for _ in 0..500 {
            app.poll(ctx);
            if app.search_rx.is_none() && app.pending_click.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until the ranking of a click finished.
    fn wait_for_focus(app: &mut Coder) {
        for _ in 0..500 {
            app.poll_focus();
            if app.goto.is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
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
        assert_eq!(token.map(|click| click.token).as_deref(), Some("needle"));
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
        let mut cache = MarkdownCache::default();
        let output = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                markdown_preview(
                    ui,
                    &mut cache,
                    "# Large heading\n\nBody [link](https://example.org)\n\n- [x] Done\n\n| Engine | Support |\n| --- | --- |\n| Comrak | GFM |\n\n```rust\nfn main() {}\n```\n",
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
        for expected in ["Done", "Engine", "Comrak", "fn main() {}"] {
            assert!(
                output.shapes.iter().any(|shape| matches!(
                    &shape.shape,
                    egui::Shape::Text(text) if text.galley.text().contains(expected)
                )),
                "Missing rendered Markdown content: {expected}"
            );
        }
        assert_eq!(
            ctx.style().text_styles[&egui::TextStyle::Body].size,
            13.0,
            "preview styles stay local"
        );
    }

    #[test]
    fn markdown_images_use_natural_size_constrained_by_pane_width() {
        for pane_width in [480.0, 960.0] {
            for (source, dimensions, explicit_width) in [
                (
                    "Before\n\n![Screenshot](image.svg)\n\nAfter",
                    [1200, 600],
                    None,
                ),
                (
                    "[![Screenshot](image.svg)](https://example.org)",
                    [1200, 600],
                    None,
                ),
                (
                    "<img src=\"image.svg\" width=\"240\">",
                    [1200, 600],
                    Some(240.0),
                ),
                ("![Badge](image.svg)", [80, 20], Some(80.0)),
            ] {
                let ctx = egui::Context::default();
                chrome::install(&ctx);
                egui_extras::install_image_loaders(&ctx);
                let svg = format!(
                    r#"<svg xmlns="http://www.w3.org/2000/svg" width="{}" height="{}"><rect width="100%" height="100%" fill="white"/></svg>"#,
                    dimensions[0], dimensions[1]
                );
                // Give the fixture priority over the asynchronous file loader.
                let bytes = Arc::new(egui::load::DefaultBytesLoader::default());
                bytes.insert("file:///tmp/image.svg", svg.into_bytes());
                ctx.add_bytes_loader(bytes);
                let mut cache = MarkdownCache::default();
                for _ in 0..2 {
                    let output = ctx.run(
                        egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(pane_width, 900.0),
                            )),
                            ..Default::default()
                        },
                        |ctx| {
                            egui::CentralPanel::default().show(ctx, |ui| {
                                markdown_preview(
                                    ui,
                                    &mut cache,
                                    source,
                                    Path::new("/tmp/README.md"),
                                );
                            });
                        },
                    );
                    let bounds = output
                        .shapes
                        .iter()
                        .find_map(|shape| match &shape.shape {
                            egui::Shape::Rect(rect) if rect.brush.is_some() => Some(rect.rect),
                            _ => None,
                        })
                        .unwrap_or_else(|| panic!("Image did not render: {source}"));
                    if let Some(width) = explicit_width {
                        assert!(
                            (bounds.width() - width).abs() < 1.0,
                            "Incorrect image width: {bounds:?}"
                        );
                    } else {
                        assert!(
                            bounds.width() > pane_width - 100.0,
                            "Screenshot shrank to a thumbnail: {bounds:?}"
                        );
                        assert!(bounds.width() < pane_width, "Screenshot must fit the pane");
                    }
                    let aspect = dimensions[0] as f32 / dimensions[1] as f32;
                    assert!((bounds.width() / bounds.height() - aspect).abs() < 0.01);
                }
            }
        }
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
    fn closing_the_search_dialog_gives_the_keyboard_back_to_the_editor() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("sample.rs");
        std::fs::write(&path, "fn main() {}\n")?;
        let mut app = Coder::empty(dir.path().to_path_buf());
        app.open(path, None);
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
        // Open the search dialog and let it take the keyboard.
        app.open_search();
        frame(&mut app, vec![]);
        frame(&mut app, vec![]);
        assert!(app.search_open);
        // Escape closes it, and the editor gets the keyboard back.
        let escape = [
            egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        frame(&mut app, escape.to_vec());
        assert!(!app.search_open);
        frame(&mut app, vec![]);
        assert_eq!(ctx.memory(|m| m.focused()), app.documents[0].editor_id);
        Ok(())
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
            assert_eq!(text.as_str(), source.as_str());
        }
        frame(&mut app, key(Key::Escape));
        assert!(!app.find_open);
        assert_eq!(ctx.memory(|m| m.focused()), app.documents[0].editor_id);
        Ok(())
    }

    #[test]
    fn project_sessions_restore_tabs_cursor_layout_and_skip_missing_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("project");
        let other = dir.path().join("other");
        let state = dir.path().join("sessions");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&other)?;
        let root = root.canonicalize()?;
        let file = root.join("unicode.rs");
        let deleted = root.join("deleted.rs");
        std::fs::write(&file, "fn main() {\n    let 猫 = 1;\n}\n")?;
        std::fs::write(&deleted, "temporary")?;
        let mut app = Coder::empty(root.clone());
        app.session_store = Some(crate::session::Store::new(state.clone()));
        app.open(deleted.clone(), None);
        app.open(file.clone(), Some((2, 9)));
        app.show_worktrees = false;
        app.show_structure = true;
        app.show_project = false;
        app.switch_project(other);
        std::fs::remove_file(deleted)?;
        app.switch_project(root.clone());
        assert_eq!(app.documents.len(), 1);
        assert_eq!(app.documents[app.active].path, file);
        assert_eq!(app.documents[app.active].jump, Some((2, 9)));
        assert!(!app.show_worktrees && app.show_structure && !app.show_project);
        drop(app);
        let mut reopened = Coder::empty(root);
        reopened.session_store = Some(crate::session::Store::new(state));
        reopened.restore_session();
        assert_eq!(reopened.documents.len(), 1);
        assert_eq!(reopened.documents[0].jump, Some((2, 9)));
        Ok(())
    }

    #[test]
    fn disk_sessions_keep_each_worktrees_tabs_when_buffers_move_into_memory() -> anyhow::Result<()>
    {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let a = first.path().join("one.rs");
        let b = second.path().join("two.rs");
        std::fs::write(&a, "one")?;
        std::fs::write(&b, "two")?;
        let first_root = first.path().canonicalize()?;
        let second_root = second.path().canonicalize()?;
        let mut app = Coder::empty(first_root.clone());
        app.session_store = Some(crate::session::Store::new(state.path().join("projects")));
        app.open(a.clone(), None);
        app.switch_worktree(second_root.clone());
        app.open(b.clone(), None);
        app.switch_worktree(first_root.clone());
        assert_eq!(app.documents.len(), 1);
        assert_eq!(app.documents[0].path, a.canonicalize()?);
        let store = crate::session::Store::new(state.path().join("projects"));
        assert_eq!(
            store.load(&first_root).unwrap()["tabs"][0]["path"],
            a.canonicalize()?.to_string_lossy().as_ref()
        );
        assert_eq!(
            store.load(&second_root).unwrap()["tabs"][0]["path"],
            b.canonicalize()?.to_string_lossy().as_ref()
        );
        Ok(())
    }

    #[test]
    fn switching_worktrees_preserves_unsaved_buffers_and_active_tabs() -> anyhow::Result<()> {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let file = first.path().join("main.rs");
        std::fs::write(&file, "original")?;
        let mut app = Coder::empty(first.path().canonicalize()?);
        app.open(file, None);
        app.documents[0].dirty = true;
        app.switch_worktree(second.path().canonicalize()?);
        assert!(app.documents.is_empty());
        assert!(app.pending.is_none());
        app.switch_worktree(first.path().canonicalize()?);
        assert_eq!(app.documents.len(), 1);
        assert!(app.documents[0].dirty);
        assert_eq!(app.active, 0);
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
            change_lines: vec![1],
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
