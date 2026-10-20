//! Native PTY sessions, with bounded scrollback and background I/O.
use crate::chrome;
use anyhow::{Context, Result, bail};
use eframe::egui::{self, Color32, FontId, Key, Modifiers};
use portable_pty::{ChildKiller, CommandBuilder, PtySize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    time::{Duration, Instant},
};

pub use crate::preferences::Settings;

impl Settings {
    fn command(&self, root: &Path) -> Result<CommandBuilder> {
        // Accept a quoted executable path and optional shell arguments; never use a wrapper shell.
        let args = shell_args(&self.shell)?;
        let executable = args.first().context("Choose a shell path")?;
        if executable.contains(std::path::MAIN_SEPARATOR) && !Path::new(executable).is_file() {
            bail!("Shell executable does not exist: {executable}");
        }
        let mut command = CommandBuilder::new(executable);
        command.args(&args[1..]);
        if args.len() == 1 && matches!(shell_name(&self.shell).as_str(), "bash" | "zsh" | "fish") {
            command.arg("-l");
        }
        command.cwd(root);
        command.env("PWD", root);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env("TERM_PROGRAM", "Glimcode");
        // The parent terminal's identity does not describe this emulator.
        for name in [
            "TERM_PROGRAM_VERSION",
            "TERM_SESSION_ID",
            "ITERM_SESSION_ID",
            "ITERM_PROFILE",
            "LC_TERMINAL",
            "LC_TERMINAL_VERSION",
            "KITTY_WINDOW_ID",
            "WT_SESSION",
            "VTE_VERSION",
            "TMUX",
            "TMUX_PANE",
            "STY",
        ] {
            command.env_remove(name);
        }
        Ok(command)
    }
}
fn shell_args(shell: &str) -> Result<Vec<String>> {
    let shell = shell.trim();
    if Path::new(shell).is_file() {
        return Ok(vec![shell.to_owned()]);
    }
    let mut args = shell_words::split(shell).context("Invalid shell path or arguments")?;
    if let Some(executable) = args.first_mut()
        && let Some(relative) = executable.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        *executable = PathBuf::from(home)
            .join(relative)
            .to_string_lossy()
            .into_owned();
    }
    Ok(args)
}

fn shell_name(shell: &str) -> String {
    shell_args(shell)
        .ok()
        .and_then(|args| args.first().cloned())
        .and_then(|path| {
            Path::new(&path)
                .file_name()
                .map(|name| name.to_string_lossy().trim_end_matches(".exe").to_owned())
        })
        .unwrap_or_else(|| "shell".into())
}
fn clean_title(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(60)
        .collect()
}
fn smart_title(title: &str, directory: &Path, shell: &str) -> String {
    let shell = shell_name(shell);
    let title = clean_title(title);
    if !title.is_empty() && !["shell", "terminal", "Terminal", &shell].contains(&title.as_str()) {
        if let Some((host, path)) = title.split_once(':')
            && host.contains('@')
            && !path.contains(' ')
        {
            return format!(
                "{}:{}",
                host.rsplit('@').next().unwrap_or(host),
                Path::new(path)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            );
        }
        if title.starts_with('/') && !title.contains(' ') {
            return format!(
                "{} · {shell}",
                Path::new(&title)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
            );
        }
        return title;
    }
    format!(
        "{} · {shell}",
        directory
            .file_name()
            .unwrap_or(directory.as_os_str())
            .to_string_lossy()
    )
}

#[derive(Default)]
struct Callbacks {
    title: String,
    directory: Option<PathBuf>,
    replies: Vec<u8>,
    focus_reporting: bool,
    last_focus: Option<bool>,
}
impl vt100::Callbacks for Callbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = clean_title(&String::from_utf8_lossy(title));
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.get(1) == Some(&b"?".as_slice()) {
            let color = match params.first().copied() {
                Some(b"10") => Some(chrome::TEXT),
                Some(b"11") => Some(chrome::EDITOR),
                Some(b"12") => Some(chrome::TEXT),
                _ => None,
            };
            if let Some(color) = color {
                self.replies.extend_from_slice(
                    format!(
                        "\x1b]{};rgb:{:04x}/{:04x}/{:04x}\x1b\\",
                        String::from_utf8_lossy(params[0]),
                        u16::from(color.r()) * 257,
                        u16::from(color.g()) * 257,
                        u16::from(color.b()) * 257,
                    )
                    .as_bytes(),
                );
            }
        }
        if params.first() == Some(&b"7".as_slice())
            && let Some(path) = params.get(1)
            && let Ok(url) = url::Url::parse(&String::from_utf8_lossy(path))
            && let Ok(path) = url.to_file_path()
        {
            self.directory = Some(path);
        }
    }
    fn unhandled_escape(&mut self, _: &mut vt100::Screen, i1: Option<u8>, _: Option<u8>, c: u8) {
        if i1.is_none() && c == b'Z' {
            self.replies.extend_from_slice(b"\x1b[?1;2c");
        }
    }
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, i2, c, first) {
            (None, None, 'n', 6) | (Some(b'?'), None, 'n', 6) => {
                let (row, col) = screen.cursor_position();
                self.replies.extend_from_slice(
                    format!(
                        "\x1b[{}{};{}R",
                        if i1.is_some() { "?" } else { "" },
                        row + 1,
                        col + 1
                    )
                    .as_bytes(),
                );
            }
            (None, None, 'n', 5) => self.replies.extend_from_slice(b"\x1b[0n"),
            (None, None, 'c', 0) => self.replies.extend_from_slice(b"\x1b[?1;2c"),
            (Some(b'>'), None, 'c', 0) => self.replies.extend_from_slice(b"\x1b[>0;0;0c"),
            (None, None, 't', 18) => {
                let (rows, cols) = screen.size();
                self.replies
                    .extend_from_slice(format!("\x1b[8;{rows};{cols}t").as_bytes());
            }
            (Some(b'?'), None, 'h' | 'l', _) => {
                if params.iter().any(|p| p.first() == Some(&1004)) {
                    self.focus_reporting = c == 'h';
                    self.last_focus = None;
                }
            }
            (Some(b'?'), Some(b'$'), 'p', mode) => {
                // Report only capabilities we implement. Unknown extensions stay unsupported.
                let enabled = match mode {
                    1 => Some(screen.application_cursor()),
                    25 => Some(!screen.hide_cursor()),
                    1004 => Some(self.focus_reporting),
                    1049 => Some(screen.alternate_screen()),
                    2004 => Some(screen.bracketed_paste()),
                    _ => None,
                };
                let status = enabled.map_or(0, |enabled| if enabled { 1 } else { 2 });
                self.replies
                    .extend_from_slice(format!("\x1b[?{mode};{status}$y").as_bytes());
            }
            _ => {}
        }
    }
}
struct ScreenState {
    parser: vt100::Parser<Callbacks>,
    status: String,
    exited: bool,
    error: Option<String>,
}
impl Default for ScreenState {
    fn default() -> Self {
        Self {
            parser: vt100::Parser::new_with_callbacks(24, 80, 5000, Callbacks::default()),
            status: "Starting shell…".into(),
            exited: false,
            error: None,
        }
    }
}
enum Command {
    Input(Vec<u8>),
    Resize(PtySize),
}
type Killer = Arc<Mutex<Option<Box<dyn ChildKiller + Send + Sync>>>>;
struct ManagedChild(Box<dyn portable_pty::Child + Send + Sync>);
impl Drop for ManagedChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct Session {
    id: u64,
    root: PathBuf,
    shell: String,
    default_name: String,
    screen: Arc<Mutex<ScreenState>>,
    tx: SyncSender<Command>,
    stopped: Arc<AtomicBool>,
    killer: Killer,
    size: (u16, u16),
    selection: Option<((u16, u16), (u16, u16))>,
    scroll_remainder: f32,
}
/// The size a shell falls back to when the interface never reports one.
const FALLBACK_SIZE: PtySize = PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
};

/// How long the shell waits for the real terminal size before it starts.
///
/// The panel reports its size in the first frame, so this only matters when a
/// tab is created while the panel is never shown.
const SIZE_WAIT: Duration = Duration::from_secs(5);

/// Wait for the first size the interface reports.
///
/// The shell is started with the size it is given, because a shell that reads
/// the size of the terminal once, when it starts, keeps the prompt it built
/// for that size. Commands that arrive first are handed back so they are not
/// lost.
fn wait_for_size(rx: &mpsc::Receiver<Command>) -> (PtySize, Vec<Command>) {
    let deadline = Instant::now() + SIZE_WAIT;
    let mut queued = vec![];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return (FALLBACK_SIZE, queued);
        }
        match rx.recv_timeout(remaining) {
            Ok(Command::Resize(size)) => return (size, queued),
            Ok(command) => queued.push(command),
            Err(_) => return (FALLBACK_SIZE, queued),
        }
    }
}

impl Session {
    fn spawn(id: u64, root: PathBuf, settings: Settings, ctx: egui::Context) -> Self {
        let screen = Arc::new(Mutex::new(ScreenState::default()));
        let stopped = Arc::new(AtomicBool::new(false));
        let killer: Killer = Arc::new(Mutex::new(None));
        let (tx, rx) = mpsc::sync_channel(128);
        let shared = screen.clone();
        let stop = stopped.clone();
        let kill = killer.clone();
        let cwd = root.clone();
        let shell = settings.shell.clone();
        let default_name = settings.tab_name.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                // A shell reads the size of the terminal once, when it starts,
                // and builds its prompt from that number. A prompt that is
                // right aligned keeps the width it was built with until the
                // next command, so the real size is awaited before the shell
                // starts. Otherwise the first prompt is laid out for the
                // fallback size and the time sits in the middle of the line.
                let (size, queued) = wait_for_size(&rx);
                let mut queued: std::collections::VecDeque<Command> = queued.into();
                // The pty is opened with the real size. Changing the size of
                // an existing pty afterwards is not seen by the shell that is
                // started with it.
                let pair = portable_pty::native_pty_system().openpty(size)?;
                let mut reader = pair.master.try_clone_reader()?;
                let mut writer = pair.master.take_writer()?;
                // The screen starts at the fallback size, so it is told about
                // the real one before the first output arrives.
                shared
                    .lock()
                    .unwrap()
                    .parser
                    .screen_mut()
                    .set_size(size.rows, size.cols);
                let command = settings.command(&cwd)?;
                let mut child = ManagedChild(pair.slave.spawn_command(command)?);
                drop(pair.slave);
                *kill.lock().unwrap() = Some(child.0.clone_killer());
                shared.lock().unwrap().status = "Running".into();
                let output = shared.clone();
                let repaint = ctx.clone();
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 8192];
                    loop {
                        let count = match reader.read(&mut buffer) {
                            Ok(0) | Err(_) => break,
                            Ok(count) => count,
                        };
                        output.lock().unwrap().parser.process(&buffer[..count]);
                        repaint.request_repaint();
                    }
                });
                loop {
                    if stop.load(Ordering::Relaxed) {
                        let _ = child.0.kill();
                        let _ = child.0.wait();
                        break;
                    }
                    if let Some(status) = child.0.try_wait()? {
                        shared.lock().unwrap().status = format!("Exited · {status}");
                        break;
                    }
                    // Replies have their own buffer so a busy input queue cannot drop them.
                    let replies =
                        std::mem::take(&mut shared.lock().unwrap().parser.callbacks_mut().replies);
                    if !replies.is_empty() {
                        writer.write_all(&replies)?;
                        writer.flush()?;
                    }
                    // Commands that arrived while the size was awaited run
                    // before the ones that arrive later.
                    let command = match queued.pop_front() {
                        Some(command) => Ok(command),
                        None => rx.recv_timeout(Duration::from_millis(10)),
                    };
                    match command {
                        Ok(Command::Input(bytes)) => {
                            writer.write_all(&bytes)?;
                            writer.flush()?;
                        }
                        Ok(Command::Resize(size)) => {
                            // Keep output from being parsed with the old geometry after SIGWINCH.
                            let mut state = shared.lock().unwrap();
                            pair.master.resize(size)?;
                            state.parser.screen_mut().set_size(size.rows, size.cols);
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            let _ = child.0.kill();
                            let _ = child.0.wait();
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                // The master is closed here; the reader drains any final output independently.
                Ok(())
            })();
            let mut state = shared.lock().unwrap();
            if let Err(error) = result {
                state.error = Some(format!("{error:#}"));
                state.status = "Shell unavailable".into();
            }
            state.exited = true;
            *kill.lock().unwrap() = None;
            ctx.request_repaint();
        });
        Self {
            id,
            root,
            shell,
            default_name,
            screen,
            tx,
            stopped,
            killer,
            // Unknown, so the first frame always reports the real size.
            size: (0, 0),
            selection: None,
            scroll_remainder: 0.0,
        }
    }
    fn send(&self, command: Command) -> bool {
        self.tx.try_send(command).is_ok()
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(mut killer) = self.killer.lock().unwrap().take() {
            let _ = killer.kill();
        }
    }
}

#[derive(Default)]
pub struct Terminal {
    pub visible: bool,
    pub settings_open: bool,
    pub settings: Settings,
    draft: Settings,
    settings_error: Option<String>,
    tabs: Vec<Session>,
    active: usize,
    /// The last size the panel reported, shared with the tabs in the
    /// background.
    size: Option<PtySize>,
    next_id: u64,
    focus: bool,
    pending_close: Option<u64>,
}
impl Terminal {
    pub fn load_settings(&mut self) {
        match Settings::load_saved() {
            Ok(settings) => self.settings = settings,
            Err(error) => {
                self.settings_error = Some(format!("Could not load preferences: {error}"))
            }
        }
    }
    pub fn focused(&self, ctx: &egui::Context) -> bool {
        self.visible
            && self.tabs.get(self.active).is_some_and(|tab| {
                ctx.memory(|m| m.has_focus(egui::Id::new(("terminal-input", tab.id))))
            })
    }
    pub fn open_settings(&mut self) {
        self.draft = self.settings.clone();
        self.settings_open = true;
    }
    pub fn new_tab(&mut self, root: &Path, ctx: &egui::Context) {
        self.next_id += 1;
        self.tabs.push(Session::spawn(
            self.next_id,
            root.to_path_buf(),
            self.settings.clone(),
            ctx.clone(),
        ));
        self.active = self.tabs.len() - 1;
        self.visible = true;
        self.focus = true;
    }
    pub fn toggle(&mut self, root: &Path, ctx: &egui::Context) {
        self.visible = !self.visible;
        if self.visible {
            if self.tabs.is_empty() {
                self.new_tab(root, ctx);
            } else {
                self.focus = true;
            }
        }
    }
    pub fn panel(&mut self, ctx: &egui::Context, root: &Path, enabled: bool) {
        if !self.visible {
            return;
        }
        if self.tabs.is_empty() {
            self.new_tab(root, ctx);
        }
        let labels = self.labels();
        let mut new = false;
        let mut close = None;
        egui::TopBottomPanel::bottom("terminal-panel")
            .resizable(true)
            .default_height(290.0)
            .min_height(120.0)
            .frame(egui::Frame::new().fill(chrome::EDITOR))
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(chrome::PANEL)
                    .inner_margin(egui::Margin::symmetric(8, 6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            chrome::heading_icon(ui, chrome::Icon::Terminal, chrome::GREEN);
                            ui.strong("Terminal");
                            ui.add_space(4.0);
                            egui::ScrollArea::horizontal()
                                .id_salt("terminal-tabs")
                                .max_width((ui.available_width() - 80.0).max(80.0))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.spacing_mut().item_spacing.x = 4.0;
                                        for (index, tab) in self.tabs.iter().enumerate() {
                                            let exited = tab.screen.lock().unwrap().exited;
                                            let response = chrome::workspace_tab(
                                                ui,
                                                ui.make_persistent_id(("terminal-tab", tab.id)),
                                                &labels[index],
                                                chrome::TabKind::Terminal { exited },
                                                self.active == index,
                                                false,
                                                &format!("{}\n{}", tab.root.display(), tab.shell),
                                            );
                                            if response.activate {
                                                self.active = index;
                                                self.focus = true;
                                            }
                                            if response.close {
                                                close = Some(index);
                                            }
                                        }
                                    });
                                });
                            if chrome::icon_button(
                                ui,
                                chrome::Icon::Plus,
                                "New terminal in the project directory (⌘/Ctrl T)",
                            )
                            .clicked()
                            {
                                new = true;
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if chrome::icon_button(
                                        ui,
                                        chrome::Icon::HideDown,
                                        "Hide terminal · sessions keep running",
                                    )
                                    .clicked()
                                    {
                                        self.visible = false;
                                    }
                                },
                            );
                        });
                    });
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        if let Some(tab) = self.tabs.get_mut(self.active) {
                            let size = render(
                                ui,
                                tab,
                                enabled && !self.settings_open,
                                &mut self.focus,
                                self.settings.terminal_font_size,
                            );
                            self.size = Some(size);
                        }
                    });
            });
        // Every tab learns the size of the panel, not only the one that is on
        // screen. A shell that starts with the wrong width builds its prompt
        // for that width, and a right aligned prompt keeps it until the next
        // command.
        if let Some(size) = self.size {
            for (index, tab) in self.tabs.iter_mut().enumerate() {
                if index != self.active && tab.size == (0, 0) {
                    tab.send(Command::Resize(size));
                }
            }
        }
        if new {
            self.new_tab(root, ctx);
        }
        if let Some(index) = close {
            self.request_close(index);
        }
    }
    pub fn shortcuts(&mut self, ctx: &egui::Context, root: &Path) {
        if !self.focused(ctx) {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::T)) {
            self.new_tab(root, ctx);
        }
        if ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::W)) {
            self.request_close(self.active);
        }
    }
    fn request_close(&mut self, index: usize) {
        if self.tabs[index].screen.lock().unwrap().exited {
            self.close(index);
        } else {
            self.pending_close = Some(self.tabs[index].id);
        }
    }
    fn close(&mut self, index: usize) {
        self.tabs.remove(index);
        if self.active > index {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        if self.tabs.is_empty() {
            self.visible = false;
        }
        self.focus = true;
    }
    fn labels(&self) -> Vec<String> {
        let mut counts = std::collections::HashMap::new();
        self.tabs
            .iter()
            .map(|tab| {
                let state = tab.screen.lock().unwrap();
                let base = if tab.default_name.trim().is_empty() {
                    let events = state.parser.callbacks();
                    smart_title(
                        &events.title,
                        events.directory.as_deref().unwrap_or(&tab.root),
                        &tab.shell,
                    )
                } else {
                    clean_title(&tab.default_name)
                };
                let count = counts.entry(base.clone()).or_insert(0);
                *count += 1;
                let label = if *count > 1 {
                    format!("{base} ({count})")
                } else {
                    base
                };
                if state.exited {
                    format!("{label} · exited")
                } else {
                    label
                }
            })
            .collect()
    }
    pub fn windows(&mut self, ctx: &egui::Context) {
        if self.settings_open {
            let mut save = false;
            let mut cancel = false;
            egui::Modal::new(egui::Id::new("preferences-dialog"))
                .frame(chrome::popup_frame()).show(ctx, |ui| {
                    chrome::modal_style(ui, 560.0);
                    cancel |= chrome::dialog_header(ui, "⚙", "Preferences", "Make this workspace feel like yours", chrome::BLUE);
                    egui::ScrollArea::vertical().id_salt("preferences-body")
                        .max_height((ctx.content_rect().height() - 250.0).max(180.0))
                        .show(ui, |ui| {
                            chrome::dialog_section(ui, "Typography", "Set independent sizes for your editor, terminal, and search.");
                            chrome::dialog_card(ui, |ui| {
                                for (label, detail, size) in [
                                    ("Editor", "Code, previews, and diffs", &mut self.draft.editor_font_size),
                                    ("Dialogs", "Window text, labels, fields, and buttons", &mut self.draft.dialog_font_size),
                                    ("Terminal", "Shell input and output", &mut self.draft.terminal_font_size),
                                    ("Search results", "Matching text, file names, and line numbers", &mut self.draft.search_results_font_size),
                                    ("Search filters", "All, Declarations, Usages, and scope menus", &mut self.draft.search_filters_font_size),
                                ] {
                                    ui.horizontal(|ui| {
                                        let control_width = (crate::preferences::dialog_font_size(ui.ctx()) * 5.5).max(84.0);
                                        let control_height = (crate::preferences::dialog_font_size(ui.ctx()) + 20.0).max(34.0);
                                        let label_width = (ui.available_width() - control_width - 16.0).max(140.0);
                                        ui.vertical(|ui| {
                                            ui.set_width(label_width);
                                            ui.label(egui::RichText::new(label).strong());
                                            ui.label(egui::RichText::new(detail).size(crate::preferences::dialog_font_size(ui.ctx())).color(chrome::MUTED));
                                        });
                                        ui.add_sized([control_width, control_height], egui::DragValue::new(size).range(8.0..=40.0).speed(0.5).suffix(" pt"));
                                    });
                                }
                                ui.add_space(4.0);
                                ui.label(egui::RichText::new("Font changes apply to open editors, dialogs, search, and terminal sessions when saved.").size(crate::preferences::dialog_font_size(ui.ctx())).color(chrome::MUTED));
                            });
                            ui.add_space(10.0);
                            chrome::dialog_section(ui, "Shell & sessions", "Choose how new terminal sessions start.");
                            chrome::dialog_card(ui, |ui| {
                                ui.label(egui::RichText::new("Shell executable").strong());
                                ui.horizontal(|ui| {
                                    ui.add(egui::TextEdit::singleline(&mut self.draft.shell)
                                        .desired_width((ui.available_width() - 130.0).max(120.0))
                                        .margin(egui::vec2(10.0, 9.0)).hint_text("/bin/zsh"));
                                    if chrome::dialog_button(ui, "Browse…", None).clicked()
                                        && let Some(path) = rfd::FileDialog::new().set_title("Select shell executable").pick_file() {
                                        self.draft.shell = shell_words::quote(&path.to_string_lossy()).into_owned();
                                    }
                                });
                                egui::ComboBox::from_id_salt("known-shells")
                                    .selected_text("Choose an installed shell…").width(ui.available_width())
                                    .show_ui(ui, |ui| {
                                        let mut shells = std::fs::read_to_string("/etc/shells").unwrap_or_default().lines()
                                            .filter(|line| line.starts_with('/')).map(str::to_owned).collect::<Vec<_>>();
                                        shells.push(Settings::default().shell);
                                        shells.sort(); shells.dedup();
                                        for shell in shells {
                                            if ui.selectable_label(self.draft.shell == shell, &shell).clicked() { self.draft.shell = shell; }
                                        }
                                    });
                                ui.add_space(6.0);
                                ui.label(egui::RichText::new("Default tab name").strong());
                                ui.add(egui::TextEdit::singleline(&mut self.draft.tab_name).desired_width(f32::INFINITY)
                                    .margin(egui::vec2(10.0, 9.0)).hint_text("Automatic · project and shell title"));
                                ui.label(egui::RichText::new("Leave empty for automatic names. Duplicates get (2), (3), …").size(crate::preferences::dialog_font_size(ui.ctx())).color(chrome::MUTED));
                            });
                            ui.label(egui::RichText::new("New sessions start in the project directory. Existing sessions keep their shell and working directory.").size(crate::preferences::dialog_font_size(ui.ctx())).color(chrome::MUTED));
                            if let Some(error) = &self.settings_error {
                                ui.add_space(4.0);
                                chrome::dialog_card(ui, |ui| { ui.colored_label(chrome::DANGER, error); });
                            }
                        });
                    chrome::dialog_divider(ui);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        save = chrome::dialog_button(ui, "Save changes", Some(chrome::BLUE)).clicked();
                        cancel |= chrome::dialog_button(ui, "Cancel", None).clicked();
                    });
                    cancel |= ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
                });
            if save {
                let result = self.draft.command(Path::new(".")).and_then(|_| {
                    self.draft.save(
                        &crate::preferences::path().context("No home directory for preferences")?,
                    )
                });
                match result {
                    Ok(()) => {
                        self.settings = self.draft.clone();
                        self.settings.apply(ctx);
                        self.settings_error = None;
                        self.settings_open = false;
                    }
                    Err(error) => self.settings_error = Some(format!("{error:#}")),
                }
            } else if cancel {
                self.settings_open = false;
            }
        }
        if let Some(id) = self.pending_close {
            egui::Modal::new(egui::Id::new("close-terminal"))
                .frame(chrome::popup_frame())
                .show(ctx, |ui| {
                    chrome::modal_style(ui, 460.0);
                    let mut cancel = chrome::dialog_header(
                        ui,
                        ">_",
                        "Close terminal session?",
                        "The shell and its running command will end",
                        chrome::WARNING,
                    );
                    if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                        let labels = self.labels();
                        chrome::dialog_card(ui, |ui| {
                            ui.label(
                                egui::RichText::new(&labels[index])
                                    .strong()
                                    .color(chrome::TEXT),
                            );
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(
                                        self.tabs[index].root.display().to_string(),
                                    )
                                    .size(crate::preferences::dialog_font_size(ui.ctx()))
                                    .color(chrome::MUTED),
                                )
                                .wrap(),
                            );
                        });
                    }
                    ui.label("To keep this session running, cancel and hide the terminal instead.");
                    chrome::dialog_divider(ui);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if chrome::dialog_button(ui, "Close session", Some(chrome::DANGER))
                            .clicked()
                        {
                            if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                                self.close(index);
                            }
                            self.pending_close = None;
                        }
                        cancel |= chrome::dialog_button(ui, "Keep session", None).clicked();
                    });
                    cancel |= ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
                    if cancel {
                        self.pending_close = None;
                    }
                });
        }
    }
    pub fn modal(&self) -> bool {
        self.settings_open || self.pending_close.is_some()
    }
}

fn key_bytes(key: Key, m: Modifiers, application: bool) -> Option<Vec<u8>> {
    if m.mac_cmd {
        return None;
    }
    if m.ctrl {
        let letter = match key {
            Key::A => 1,
            Key::B => 2,
            Key::C => 3,
            Key::D => 4,
            Key::E => 5,
            Key::F => 6,
            Key::G => 7,
            Key::H => 8,
            Key::I => 9,
            Key::J => 10,
            Key::K => 11,
            Key::L => 12,
            Key::M => 13,
            Key::N => 14,
            Key::O => 15,
            Key::P => 16,
            Key::Q => 17,
            Key::R => 18,
            Key::S => 19,
            Key::T => 20,
            Key::U => 21,
            Key::V => 22,
            Key::W => 23,
            Key::X => 24,
            Key::Y => 25,
            Key::Z => 26,
            Key::OpenBracket => 27,
            Key::Backslash => 28,
            Key::CloseBracket => 29,
            Key::Num6 => 30,
            Key::Minus => 31,
            Key::Space => 0,
            _ => 255,
        };
        if letter != 255 {
            return Some(if m.alt {
                vec![27, letter]
            } else {
                vec![letter]
            });
        }
    }
    let arrow = match key {
        Key::ArrowUp => Some('A'),
        Key::ArrowDown => Some('B'),
        Key::ArrowRight => Some('C'),
        Key::ArrowLeft => Some('D'),
        Key::Home => Some('H'),
        Key::End => Some('F'),
        _ => None,
    };
    if let Some(arrow) = arrow {
        let modifier = 1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.ctrl);
        return Some(if modifier > 1 {
            format!("\x1b[1;{modifier}{arrow}").into_bytes()
        } else {
            format!("\x1b{}{arrow}", if application { 'O' } else { '[' }).into_bytes()
        });
    }
    let plain = match key {
        Key::Enter => {
            if m.alt {
                "\x1b\r"
            } else {
                "\r"
            }
        }
        Key::Tab => {
            if m.shift {
                "\x1b[Z"
            } else {
                "\t"
            }
        }
        Key::Backspace => {
            if m.ctrl {
                "\x08"
            } else if m.alt {
                "\x1b\x7f"
            } else {
                "\x7f"
            }
        }
        Key::Escape => "\x1b",
        Key::Delete => "\x1b[3~",
        Key::Insert => "\x1b[2~",
        Key::PageUp => "\x1b[5~",
        Key::PageDown => "\x1b[6~",
        Key::F1 => "\x1bOP",
        Key::F2 => "\x1bOQ",
        Key::F3 => "\x1bOR",
        Key::F4 => "\x1bOS",
        Key::F5 => "\x1b[15~",
        Key::F6 => "\x1b[17~",
        Key::F7 => "\x1b[18~",
        Key::F8 => "\x1b[19~",
        Key::F9 => "\x1b[20~",
        Key::F10 => "\x1b[21~",
        Key::F11 => "\x1b[23~",
        Key::F12 => "\x1b[24~",
        _ => return None,
    };
    let modifier = 1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.ctrl);
    if modifier > 1
        && matches!(
            key,
            Key::Insert
                | Key::Delete
                | Key::PageUp
                | Key::PageDown
                | Key::F5
                | Key::F6
                | Key::F7
                | Key::F8
                | Key::F9
                | Key::F10
                | Key::F11
                | Key::F12
        )
    {
        return Some(format!("{};{modifier}~", plain.trim_end_matches('~')).into_bytes());
    }
    if modifier > 1 && matches!(key, Key::F1 | Key::F2 | Key::F3 | Key::F4) {
        return Some(format!("\x1b[1;{modifier}{}", plain.chars().last().unwrap()).into_bytes());
    }
    Some(plain.as_bytes().to_vec())
}
fn color(color: vt100::Color, default: Color32) -> Color32 {
    match color {
        vt100::Color::Default => default,
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        vt100::Color::Idx(index) => {
            const BASIC: [(u8, u8, u8); 16] = [
                (30, 31, 34),
                (205, 94, 106),
                (106, 171, 115),
                (205, 174, 88),
                (95, 151, 226),
                (181, 123, 189),
                (90, 173, 185),
                (188, 190, 196),
                (115, 119, 127),
                (245, 121, 135),
                (151, 207, 144),
                (239, 216, 141),
                (135, 183, 250),
                (216, 165, 222),
                (138, 211, 221),
                (240, 241, 244),
            ];
            let (r, g, b) = if index < 16 {
                BASIC[index as usize]
            } else if index < 232 {
                let n = index - 16;
                let part = |v| if v == 0 { 0 } else { 55 + 40 * v };
                (part(n / 36), part(n / 6 % 6), part(n % 6))
            } else {
                let v = 8 + (index - 232) * 10;
                (v, v, v)
            };
            Color32::from_rgb(r, g, b)
        }
    }
}
/// Keep path and URL punctuation inside a word; quotes and shell separators bound it.
fn terminal_word(screen: &vt100::Screen, point: (u16, u16)) -> Option<((u16, u16), (u16, u16))> {
    let (rows, cols) = screen.size();
    let is_word = |(row, col)| {
        screen.cell(row, col).is_some_and(|cell| {
            cell.is_wide_continuation()
                || (!cell.contents().is_empty()
                    && cell
                        .contents()
                        .chars()
                        .all(|c| !c.is_whitespace() && !"\"'`()[]{}<>|;,".contains(c)))
        })
    };
    if point.0 >= rows || point.1 >= cols || !is_word(point) {
        return None;
    }
    let (mut start, mut end) = (point, point);
    loop {
        let previous = if start.1 > 0 {
            (start.0, start.1 - 1)
        } else if start.0 > 0 && screen.row_wrapped(start.0 - 1) {
            (start.0 - 1, cols - 1)
        } else {
            break;
        };
        if !is_word(previous) {
            break;
        }
        start = previous;
    }
    loop {
        let next = if end.1 + 1 < cols {
            (end.0, end.1 + 1)
        } else if end.0 + 1 < rows && screen.row_wrapped(end.0) {
            (end.0 + 1, 0)
        } else {
            break;
        };
        if !is_word(next) {
            break;
        }
        end = next;
    }
    Some((start, end))
}

fn mouse_bytes(screen: &vt100::Screen, code: u8, point: (u16, u16), released: bool) -> Vec<u8> {
    let (row, col) = (point.0 + 1, point.1 + 1);
    match screen.mouse_protocol_encoding() {
        vt100::MouseProtocolEncoding::Sgr => format!(
            "\x1b[<{code};{col};{row}{}",
            if released { 'm' } else { 'M' }
        )
        .into_bytes(),
        encoding => {
            let code = if released { (code & 28) | 3 } else { code };
            let mut bytes = b"\x1b[M".to_vec();
            for value in [
                u32::from(code) + 32,
                u32::from(col) + 32,
                u32::from(row) + 32,
            ] {
                if encoding == vt100::MouseProtocolEncoding::Utf8 {
                    let mut buf = [0; 4];
                    bytes.extend_from_slice(
                        char::from_u32(value)
                            .unwrap()
                            .encode_utf8(&mut buf)
                            .as_bytes(),
                    );
                } else if value <= 255 {
                    bytes.push(value as u8);
                } else {
                    return vec![];
                }
            }
            bytes
        }
    }
}

/// The text of one terminal row and the color of every run in it.
///
/// Every cell contributes one character, so that the row keeps the columns of
/// the grid. A cell that was never written is a blank.
fn row_runs(
    screen: &vt100::Screen,
    row: u16,
    cols: u16,
    selection: Option<((u16, u16), (u16, u16))>,
) -> (String, Vec<(std::ops::Range<usize>, Color32)>) {
    let mut text = String::with_capacity(cols as usize);
    let mut runs: Vec<(std::ops::Range<usize>, Color32)> = vec![];
    let mut start = 0usize;
    let mut color: Option<Color32> = None;
    for col in 0..cols {
        let Some(c) = screen.cell(row, col) else {
            // A cell outside the screen is a blank, but the row still needs
            // one character for it.
            text.push(' ');
            continue;
        };
        let (fg, _) = cell_colors(c, selection, row, col);
        let cell_color = if c.dim() { fg.gamma_multiply(0.6) } else { fg };
        match color {
            None => color = Some(cell_color),
            Some(current) if current != cell_color => {
                runs.push((start..text.len(), current));
                start = text.len();
                color = Some(cell_color);
            }
            Some(_) => {}
        }
        if c.contents().is_empty() {
            text.push(' ');
        } else {
            text.push_str(c.contents());
        }
    }
    if let Some(current) = color {
        runs.push((start..text.len(), current));
    }
    (text, runs)
}

/// The foreground and background of one terminal cell, with the selection
/// applied.
fn cell_colors(
    c: &vt100::Cell,
    selection: Option<((u16, u16), (u16, u16))>,
    row: u16,
    col: u16,
) -> (Color32, Color32) {
    let mut fg = color(c.fgcolor(), chrome::TEXT);
    let mut bg = color(c.bgcolor(), chrome::EDITOR);
    if c.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    if selection.is_some_and(|(start, end)| (row, col) >= start && (row, col) <= end) {
        bg = chrome::SELECTED;
    }
    (fg, bg)
}

fn render(
    ui: &mut egui::Ui,
    tab: &mut Session,
    enabled: bool,
    focus: &mut bool,
    font_size: f32,
) -> PtySize {
    let id = egui::Id::new(("terminal-input", tab.id));
    let font = FontId::monospace(font_size);
    let cell = ui.fonts_mut(|f| egui::vec2(f.glyph_width(&font, 'M'), f.row_height(&font) + 2.0));
    let (_, rect) = ui.allocate_space(ui.available_size().max(egui::vec2(10.0, 30.0)));
    let response = ui
        .interact(rect, id, egui::Sense::click_and_drag())
        .on_hover_cursor(egui::CursorIcon::Text);
    if response.dragged() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
    }
    let size = (
        (rect.height() / cell.y).floor().clamp(2.0, 200.0) as u16,
        (rect.width() / cell.x).floor().clamp(10.0, 500.0) as u16,
    );
    let pty = PtySize {
        rows: size.0,
        cols: size.1,
        pixel_width: 0,
        pixel_height: 0,
    };
    if size != tab.size && tab.send(Command::Resize(pty)) {
        tab.size = size;
        tab.selection = None;
    }
    if enabled && (*focus || response.clicked()) {
        ui.memory_mut(|m| m.request_focus(id));
        *focus = false;
    }
    ui.memory_mut(|m| {
        m.set_focus_lock_filter(
            id,
            egui::EventFilter {
                tab: true,
                horizontal_arrows: true,
                vertical_arrows: true,
                escape: true,
            },
        )
    });
    let focused = enabled && ui.memory(|m| m.has_focus(id));
    let point = |pos: egui::Pos2| {
        (
            ((pos.y - rect.top()) / cell.y)
                .floor()
                .clamp(0.0, f32::from(size.0 - 1)) as u16,
            ((pos.x - rect.left()) / cell.x)
                .floor()
                .clamp(0.0, f32::from(size.1 - 1)) as u16,
        )
    };
    let mouse_mode = tab
        .screen
        .lock()
        .unwrap()
        .parser
        .screen()
        .mouse_protocol_mode();
    let report_mouse = enabled
        && ui.input(|i| i.focused)
        && !tab.screen.lock().unwrap().exited
        && !ui.input(|i| i.modifiers.shift)
        && mouse_mode != vt100::MouseProtocolMode::None;
    if !report_mouse
        && response.drag_started()
        && let Some(pos) = ui.input(|i| i.pointer.press_origin())
    {
        let p = point(pos);
        tab.selection = Some((p, p));
    }
    if !report_mouse
        && response.dragged()
        && let Some(pos) = response.interact_pointer_pos()
        && let Some((_, end)) = &mut tab.selection
    {
        *end = point(pos);
    }
    if response.clicked() {
        tab.selection = None;
    }
    let mut state = tab.screen.lock().unwrap();
    let mut outgoing = vec![];
    let terminal_focused = focused && ui.input(|i| i.focused);
    if !state.exited
        && state.parser.callbacks().focus_reporting
        && state.parser.callbacks().last_focus != Some(terminal_focused)
    {
        outgoing.extend_from_slice(if terminal_focused {
            b"\x1b[I"
        } else {
            b"\x1b[O"
        });
        state.parser.callbacks_mut().last_focus = Some(terminal_focused);
    }
    if report_mouse {
        for event in ui.input(|i| i.events.clone()) {
            let (pos, code, released) = match event {
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } if rect.contains(pos) => {
                    if !pressed && mouse_mode == vt100::MouseProtocolMode::Press {
                        continue;
                    }
                    let code = match button {
                        egui::PointerButton::Primary => 0,
                        egui::PointerButton::Middle => 1,
                        egui::PointerButton::Secondary => 2,
                        _ => continue,
                    } + 8 * u8::from(modifiers.alt)
                        + 16 * u8::from(modifiers.ctrl);
                    (pos, code, !pressed)
                }
                egui::Event::PointerMoved(pos) if rect.contains(pos) => {
                    let button = ui.input(|i| {
                        if i.pointer.primary_down() {
                            Some(0)
                        } else if i.pointer.middle_down() {
                            Some(1)
                        } else if i.pointer.secondary_down() {
                            Some(2)
                        } else {
                            None
                        }
                    });
                    if mouse_mode != vt100::MouseProtocolMode::AnyMotion
                        && !(mouse_mode == vt100::MouseProtocolMode::ButtonMotion
                            && button.is_some())
                    {
                        continue;
                    }
                    (pos, 32 + button.unwrap_or(3), false)
                }
                _ => continue,
            };
            outgoing.extend(mouse_bytes(
                state.parser.screen(),
                code,
                point(pos),
                released,
            ));
        }
    }
    if enabled && ui.input(|i| i.focused) && !state.exited && response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        tab.scroll_remainder += scroll / cell.y;
        let lines = tab.scroll_remainder.trunc() as i64;
        if lines != 0 {
            tab.scroll_remainder -= lines as f32;
            if report_mouse {
                if let Some(pos) = response.hover_pos() {
                    for _ in 0..lines.unsigned_abs().min(100) {
                        outgoing.extend(mouse_bytes(
                            state.parser.screen(),
                            if lines > 0 { 64 } else { 65 },
                            point(pos),
                            false,
                        ));
                    }
                }
            } else if state.parser.screen().alternate_screen() && !ui.input(|i| i.modifiers.shift) {
                for _ in 0..lines.unsigned_abs().min(100) {
                    outgoing.extend(
                        key_bytes(
                            if lines > 0 {
                                Key::ArrowUp
                            } else {
                                Key::ArrowDown
                            },
                            Modifiers::NONE,
                            state.parser.screen().application_cursor(),
                        )
                        .unwrap(),
                    );
                }
            } else {
                let offset = state.parser.screen().scrollback() as i64 + lines;
                state
                    .parser
                    .screen_mut()
                    .set_scrollback(offset.max(0) as usize);
            }
            tab.selection = None;
        }
    }
    if enabled
        && !report_mouse
        && response.double_clicked()
        && let Some(pos) = response.interact_pointer_pos()
    {
        tab.selection = terminal_word(state.parser.screen(), point(pos));
    }
    let mut copy = false;
    if focused && !state.exited {
        let events = ui.input_mut(|i| {
            let mut events = vec![];
            i.events.retain(|event| {
                if matches!(
                    event,
                    egui::Event::Text(_)
                        | egui::Event::Paste(_)
                        | egui::Event::Copy
                        | egui::Event::Cut
                        | egui::Event::Key { .. }
                        | egui::Event::Ime(_)
                ) {
                    events.push(event.clone());
                    false
                } else {
                    true
                }
            });
            events
        });
        let clipboard_copy = events
            .iter()
            .any(|event| matches!(event, egui::Event::Copy));
        let clipboard_paste = events
            .iter()
            .any(|event| matches!(event, egui::Event::Paste(_)));
        for event in events {
            match event {
                egui::Event::Text(text) => {
                    if ui.input(|i| i.modifiers.alt) && !cfg!(target_os = "macos") {
                        outgoing.push(27);
                    }
                    outgoing.extend_from_slice(text.as_bytes());
                }
                egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                    outgoing.extend_from_slice(text.as_bytes())
                }
                egui::Event::Paste(text) => {
                    let text = text.replace("\r\n", "\n").replace('\r', "\n");
                    if state.parser.screen().bracketed_paste() {
                        outgoing.extend_from_slice(b"\x1b[200~");
                        outgoing.extend_from_slice(text.as_bytes());
                        outgoing.extend_from_slice(b"\x1b[201~");
                    } else {
                        outgoing.extend_from_slice(text.replace('\n', "\r").as_bytes());
                    }
                }
                egui::Event::Cut => {
                    if cfg!(target_os = "macos") || ui.input(|i| i.modifiers.shift) {
                        copy = true;
                    } else {
                        outgoing.push(24);
                    }
                }
                egui::Event::Copy => {
                    if cfg!(target_os = "macos") || ui.input(|i| i.modifiers.shift) {
                        copy = true;
                    } else {
                        outgoing.push(3);
                    }
                }
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if modifiers.ctrl && modifiers.shift && key == Key::C {
                        copy = true;
                    } else if !(clipboard_copy
                        && key == Key::C
                        && modifiers.ctrl
                        && !cfg!(target_os = "macos"))
                        && !(clipboard_paste
                            && ((key == Key::V && modifiers.command)
                                || (key == Key::Insert && modifiers.shift)))
                        && let Some(bytes) =
                            key_bytes(key, modifiers, state.parser.screen().application_cursor())
                    {
                        outgoing.extend(bytes);
                    }
                }
                _ => {}
            }
        }
    }
    let mut selection = tab
        .selection
        .map(|(a, b)| if a <= b { (a, b) } else { (b, a) });
    if !report_mouse {
        response.context_menu(|ui| {
            if ui
                .add_enabled(selection.is_some(), egui::Button::new("Copy selection"))
                .clicked()
            {
                copy = true;
                ui.close();
            }
            if ui.button("Copy visible output").clicked() {
                ui.ctx().copy_text(state.parser.screen().contents());
                ui.close();
            }
            if ui.button("Paste").clicked() {
                ui.memory_mut(|m| m.request_focus(id));
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                ui.close();
            }
            if ui.button("Scroll to bottom").clicked() {
                state.parser.screen_mut().set_scrollback(0);
                ui.close();
            }
        });
    }
    if copy && let Some((start, end)) = selection {
        ui.ctx().copy_text(state.parser.screen().contents_between(
            start.0,
            start.1,
            end.0,
            end.1 + 1,
        ));
    }
    if !outgoing.is_empty() {
        state.parser.screen_mut().set_scrollback(0);
        tab.selection = None;
        selection = None;
        if outgoing.len() > 1024 * 1024 {
            state.error = Some("Paste is limited to 1 MiB. Paste a smaller section.".into());
        } else if !tab.send(Command::Input(outgoing)) {
            state.error =
                Some("Terminal input is busy. Try again after the command catches up.".into());
        } else {
            state.error = None;
        }
    }
    let screen = state.parser.screen();
    let painter = ui.painter().with_clip_rect(rect);
    for row in 0..size.0 {
        // The background and the underline belong to the cell, not to the
        // text, so they are painted for every cell.
        for col in 0..size.1 {
            let Some(c) = screen.cell(row, col) else {
                continue;
            };
            let origin = rect.min + egui::vec2(f32::from(col) * cell.x, f32::from(row) * cell.y);
            let cell_rect = egui::Rect::from_min_size(
                origin,
                egui::vec2(cell.x * if c.is_wide() { 2.0 } else { 1.0 }, cell.y),
            );
            let (fg, bg) = cell_colors(c, selection, row, col);
            if bg != chrome::EDITOR {
                painter.rect_filled(cell_rect, 0.0, bg);
            }
            if c.underline() {
                painter.line_segment(
                    [cell_rect.left_bottom(), cell_rect.right_bottom()],
                    egui::Stroke::new(1.0_f32, fg),
                );
            }
        }
        // A row is painted as one text run, so the font is laid out once per
        // row instead of once per cell. Wide characters do not keep the grid
        // alignment inside a single run, so those rows are painted cell by
        // cell.
        let wide = (0..size.1).any(|col| screen.cell(row, col).is_some_and(|cell| cell.is_wide()));
        if wide {
            for col in 0..size.1 {
                let Some(c) = screen.cell(row, col) else {
                    continue;
                };
                let origin =
                    rect.min + egui::vec2(f32::from(col) * cell.x, f32::from(row) * cell.y);
                if !c.is_wide_continuation() && !c.contents().is_empty() {
                    let (fg, _) = cell_colors(c, selection, row, col);
                    painter.text(
                        origin,
                        egui::Align2::LEFT_TOP,
                        c.contents(),
                        font.clone(),
                        if c.dim() { fg.gamma_multiply(0.6) } else { fg },
                    );
                }
            }
            continue;
        }
        let mut job = egui::text::LayoutJob::default();
        let (text, runs) = row_runs(screen, row, size.1, selection);
        for (range, color) in runs {
            job.append(
                &text[range],
                0.0,
                egui::TextFormat {
                    font_id: font.clone(),
                    color,
                    ..Default::default()
                },
            );
        }
        if job.sections.is_empty() {
            continue;
        }
        let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
        painter.galley(
            rect.min + egui::vec2(0.0, f32::from(row) * cell.y),
            galley,
            chrome::TEXT,
        );
    }
    if focused && !screen.hide_cursor() && screen.scrollback() == 0 && !state.exited {
        let (row, col) = screen.cursor_position();
        let origin = rect.min + egui::vec2(f32::from(col) * cell.x, f32::from(row) * cell.y);
        painter.rect_filled(
            egui::Rect::from_min_size(origin, cell),
            0.0,
            chrome::TEXT.gamma_multiply(0.35),
        );
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::IMERect(egui::Rect::from_min_size(
                origin, cell,
            )));
    }
    if let Some(error) = &state.error {
        painter.text(
            rect.left_bottom(),
            egui::Align2::LEFT_BOTTOM,
            error,
            FontId::proportional(12.0),
            Color32::from_rgb(230, 115, 125),
        );
    } else if state.exited || state.status == "Starting shell…" {
        painter.text(
            rect.left_bottom(),
            egui::Align2::LEFT_BOTTOM,
            &state.status,
            FontId::proportional(12.0),
            chrome::MUTED,
        );
    }
    pty
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn double_click_words_preserve_paths_colors_unicode_and_wrapped_lines() {
        let path = "hack/apidiff-changelog/main.go";
        let mut parser = vt100::Parser::new(8, 80, 20);
        parser.process(format!("modified:  \x1b[31m\"{path}\"\x1b[0m other").as_bytes());
        for col in 12..12 + path.len() as u16 {
            let (start, end) = terminal_word(parser.screen(), (0, col)).unwrap();
            assert_eq!(
                parser
                    .screen()
                    .contents_between(start.0, start.1, end.0, end.1 + 1),
                path
            );
        }
        assert!(terminal_word(parser.screen(), (0, 10)).is_none());
        assert!(terminal_word(parser.screen(), (0, 11)).is_none()); // Quote.
        assert!(terminal_word(parser.screen(), (7, 0)).is_none()); // Empty cell.
        let mut wrapped = vt100::Parser::new(8, 16, 20);
        wrapped.process(path.as_bytes());
        let (start, end) = terminal_word(wrapped.screen(), (1, 4)).unwrap();
        assert_eq!(
            wrapped
                .screen()
                .contents_between(start.0, start.1, end.0, end.1 + 1),
            path
        );
        let mut unicode = vt100::Parser::new(8, 80, 20);
        unicode.process("'/tmp/猫/file.rs'\r\nseparate".as_bytes());
        // The second column of a wide glyph must select the same complete path.
        let (start, end) = terminal_word(unicode.screen(), (0, 7)).unwrap();
        assert_eq!(
            unicode
                .screen()
                .contents_between(start.0, start.1, end.0, end.1 + 1),
            "/tmp/猫/file.rs"
        );
        let (start, end) = terminal_word(unicode.screen(), (1, 3)).unwrap();
        assert_eq!(
            unicode
                .screen()
                .contents_between(start.0, start.1, end.0, end.1 + 1),
            "separate"
        );
    }

    #[test]
    fn terminal_double_click_selects_path_copies_it_and_shows_text_cursor() {
        let ctx = egui::Context::default();
        let (tx, rx) = mpsc::sync_channel(128);
        let screen = Arc::new(Mutex::new(ScreenState::default()));
        let path = "hack/apidiff-changelog/main.go";
        screen
            .lock()
            .unwrap()
            .parser
            .process(format!("modified:  {path}\r\n").as_bytes());
        let mut tab = Session {
            id: 1,
            root: "/project".into(),
            shell: "/bin/sh".into(),
            default_name: String::new(),
            screen,
            tx,
            stopped: Arc::new(AtomicBool::new(false)),
            killer: Arc::new(Mutex::new(None)),
            size: (24, 80),
            selection: None,
            scroll_remainder: 0.0,
        };
        let mut focus = true;
        let mut time = 0.0;
        let mut frame = |tab: &mut Session, events| {
            time += 0.08;
            let mut point = egui::Pos2::ZERO;
            let output = ctx.run(
                egui::RawInput {
                    time: Some(time),
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 500.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let font = FontId::monospace(14.0);
                        let cell = ui.fonts_mut(|f| {
                            egui::vec2(f.glyph_width(&font, 'M'), f.row_height(&font) + 2.0)
                        });
                        point = ui.available_rect_before_wrap().min
                            + egui::vec2(cell.x * 20.5, cell.y / 2.0);
                        render(ui, tab, true, &mut focus, 14.0);
                    });
                },
            );
            (output, point)
        };
        frame(&mut tab, vec![]);
        let (_, point) = frame(&mut tab, vec![]);
        let (output, _) = frame(&mut tab, vec![egui::Event::PointerMoved(point)]);
        assert_eq!(output.platform_output.cursor_icon, egui::CursorIcon::Text);
        for _ in 0..2 {
            for pressed in [true, false] {
                frame(
                    &mut tab,
                    vec![
                        egui::Event::PointerMoved(point),
                        egui::Event::PointerButton {
                            pos: point,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: Modifiers::NONE,
                        },
                    ],
                );
            }
        }
        assert_eq!(
            tab.selection,
            Some(((0, 11), (0, 11 + path.len() as u16 - 1)))
        );
        // Ctrl+Shift+C copies on every platform; a bare Copy event is Ctrl+C
        // on Linux/Windows and must remain a shell interrupt there.
        let (output, _) = frame(
            &mut tab,
            vec![egui::Event::Key {
                key: Key::C,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::CTRL | Modifiers::SHIFT,
            }],
        );
        let copied_path = |output: &egui::FullOutput| {
            output.platform_output.commands.iter().any(
                |command| matches!(command, egui::OutputCommand::CopyText(text) if text == path),
            )
        };
        assert!(copied_path(&output));
        assert!(
            !rx.try_iter()
                .any(|command| matches!(command, Command::Input(_))),
            "copying must not send characters to the shell"
        );
        let (output, _) = frame(&mut tab, vec![egui::Event::Copy]);
        if cfg!(target_os = "macos") {
            assert!(copied_path(&output));
        } else {
            assert!(!copied_path(&output));
            let input: Vec<_> = rx
                .try_iter()
                .filter_map(|command| match command {
                    Command::Input(bytes) => Some(bytes),
                    _ => None,
                })
                .flatten()
                .collect();
            assert_eq!(input, [3], "Ctrl+C must remain a shell interrupt");
        }
        // An ordinary click still clears selection.
        for pressed in [true, false] {
            frame(
                &mut tab,
                vec![egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: Modifiers::NONE,
                }],
            );
        }
        assert!(tab.selection.is_none());
    }

    fn wait_for(state: &Arc<Mutex<ScreenState>>, condition: impl Fn(&ScreenState) -> bool) {
        let start = std::time::Instant::now();
        loop {
            {
                let state = state.lock().unwrap();
                if condition(&state) {
                    return;
                }
                assert!(
                    start.elapsed() < Duration::from_secs(8),
                    "terminal timeout: {} {:?} {}",
                    state.status,
                    state.error,
                    state.parser.screen().contents()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(unix)]
    #[test]
    fn terminal_shortcuts_open_and_close_the_active_session() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let ctx = egui::Context::default();
        let mut terminal = Terminal::default();
        terminal.settings.shell = "/bin/sh".into();
        terminal.new_tab(&root, &ctx);
        let frame = |terminal: &mut Terminal, key: Option<Key>| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1000.0, 700.0),
                    )),
                    events: key
                        .map(|key| {
                            vec![egui::Event::Key {
                                key,
                                physical_key: None,
                                pressed: true,
                                repeat: false,
                                modifiers: Modifiers::COMMAND,
                            }]
                        })
                        .unwrap_or_default(),
                    ..Default::default()
                },
                |ctx| {
                    terminal.shortcuts(ctx, &root);
                    terminal.panel(ctx, &root, true);
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.label("Editor");
                    });
                },
            );
        };
        frame(&mut terminal, None);
        frame(&mut terminal, None);
        frame(&mut terminal, Some(Key::T));
        assert_eq!(terminal.tabs.len(), 2);
        assert_eq!(terminal.active, 1);
        assert_eq!(terminal.tabs[1].root, root);
        frame(&mut terminal, None);
        frame(&mut terminal, Some(Key::W));
        assert_eq!(terminal.pending_close, Some(terminal.tabs[1].id));
        terminal.close(1);
        terminal.pending_close = None;
        frame(&mut terminal, None);
        assert_eq!(terminal.tabs.len(), 1);
        assert!(terminal.focused(&ctx));
        Ok(())
    }

    #[test]
    fn terminal_keeps_arrows_tab_and_escape_and_sends_them_to_session() {
        let ctx = egui::Context::default();
        let (tx, rx) = mpsc::sync_channel(128);
        let mut tab = Session {
            id: 1,
            root: PathBuf::from("/tmp"),
            shell: "test".into(),
            default_name: "Local".into(),
            screen: Arc::new(Mutex::new(ScreenState::default())),
            tx,
            stopped: Arc::new(AtomicBool::new(false)),
            killer: Arc::new(Mutex::new(None)),
            size: (24, 80),
            selection: None,
            scroll_remainder: 0.0,
        };
        let mut focus = true;
        for key in [
            None,
            None,
            Some(Key::ArrowUp),
            Some(Key::ArrowDown),
            Some(Key::ArrowLeft),
            Some(Key::ArrowRight),
            Some(Key::Tab),
            Some(Key::Escape),
        ] {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 600.0),
                    )),
                    events: key
                        .map(|key| {
                            vec![egui::Event::Key {
                                key,
                                physical_key: None,
                                pressed: true,
                                repeat: false,
                                modifiers: Modifiers::NONE,
                            }]
                        })
                        .unwrap_or_default(),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        ui.button("Other component").clicked();
                        render(ui, &mut tab, true, &mut focus, 14.0);
                    });
                },
            );
            assert!(ctx.memory(|m| m.has_focus(egui::Id::new(("terminal-input", 1_u64)))));
        }
        let bytes: Vec<u8> = rx
            .try_iter()
            .filter_map(|command| match command {
                Command::Input(bytes) => Some(bytes),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(bytes, b"\x1b[A\x1b[B\x1b[D\x1b[C\t\x1b");
    }

    #[test]
    fn settings_round_trip_and_titles_have_sensible_fallbacks() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config/terminal.json");
        let settings = Settings {
            shell: "/bin/zsh -l".into(),
            tab_name: "Local".into(),
            ..Default::default()
        };
        settings.save(&path)?;
        assert_eq!(Settings::load(&path)?.shell, settings.shell);
        assert_eq!(Settings::load(&path)?.tab_name, "Local");
        assert_eq!(
            smart_title("", Path::new("/work/project"), "/bin/zsh -l"),
            "project · zsh"
        );
        assert_eq!(
            smart_title("zsh", Path::new("/work/project"), "/bin/zsh"),
            "project · zsh"
        );
        assert_eq!(
            smart_title("cargo test\x07", Path::new("/work/project"), "/bin/zsh"),
            "cargo test"
        );
        assert_eq!(
            smart_title("/work/other", Path::new("/work/project"), "/bin/zsh"),
            "other · zsh"
        );
        assert_eq!(
            smart_title(
                "user@server:/work/remote",
                Path::new("/work/project"),
                "/bin/zsh"
            ),
            "server:remote"
        );
        assert!(
            Settings {
                shell: "/does/not/exist".into(),
                tab_name: String::new(),
                ..Default::default()
            }
            .command(dir.path())
            .is_err()
        );
        assert!(
            Settings {
                shell: "'unterminated".into(),
                tab_name: String::new(),
                ..Default::default()
            }
            .command(dir.path())
            .is_err()
        );
        Ok(())
    }
    #[test]
    fn ansi_parser_tracks_titles_colors_queries_and_alternate_screen() {
        let mut parser = vt100::Parser::new_with_callbacks(5, 40, 100, Callbacks::default());
        parser.process(b"\x1b]2;cargo test\x07\x1b[31mred\x1b[0m\x1b[6n");
        assert_eq!(parser.callbacks().title, "cargo test");
        assert_eq!(
            parser.screen().cell(0, 0).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
        assert_eq!(parser.callbacks().replies, b"\x1b[1;4R");
        parser.process(b"\x1b[?1049hfull screen\x1b[?2004h");
        assert!(parser.screen().alternate_screen());
        assert!(parser.screen().bracketed_paste());
        parser.process(b"\x1b[?1049l");
        assert_eq!(parser.screen().contents(), "red");
    }
    #[cfg(unix)]
    #[test]
    #[ignore = "manual CLI startup smoke test; set GLIMCODE_TEST_CLI to an interactive command"]
    fn installed_cli_starts_in_embedded_terminal() -> Result<()> {
        let command = std::env::var("GLIMCODE_TEST_CLI").context("Set GLIMCODE_TEST_CLI")?;
        let dir = tempfile::tempdir()?;
        let tab = Session::spawn(
            1,
            dir.path().canonicalize()?,
            Settings {
                shell: command,
                ..Default::default()
            },
            egui::Context::default(),
        );
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
            let state = tab.screen.lock().unwrap();
            assert!(
                !state.exited,
                "CLI exited: {} {:?} {}",
                state.status,
                state.error,
                state.parser.screen().contents()
            );
        }
        let state = tab.screen.lock().unwrap();
        assert!(state.error.is_none());
        assert!(!state.parser.screen().contents().trim().is_empty());
        println!("CLI startup screen: {}", state.parser.screen().contents());
        drop(state);
        if let Ok(input) = std::env::var("GLIMCODE_TEST_INPUT") {
            assert!(tab.send(Command::Input(input.as_bytes().to_vec())));
            wait_for(&tab.screen, |s| {
                s.parser.screen().contents().contains(&input)
            });
            assert!(tab.send(Command::Input(vec![21]))); // Clear the unsubmitted input.
        }
        drop(tab);
        Ok(())
    }

    #[test]
    fn terminal_queries_reply_to_fragmented_cli_probes_without_claiming_unknown_modes() {
        let mut parser = vt100::Parser::new_with_callbacks(30, 100, 100, Callbacks::default());
        let queries = b"\x1b]11;?\x07\x1b[>c\x1b[18t\x1b[?2004$p\x1b[?2026$p\x1b[?2004h\x1b[?2004$p\x1b[?1004h\x1b[?1004$p";
        for byte in queries {
            parser.process(&[*byte]);
        }
        assert_eq!(parser.callbacks().replies,
            b"\x1b]11;rgb:1c1c/1e1e/2323\x1b\\\x1b[>0;0;0c\x1b[8;30;100t\x1b[?2004;2$y\x1b[?2026;0$y\x1b[?2004;1$y\x1b[?1004;1$y");
        assert!(parser.callbacks().focus_reporting);
        parser.process(b"\x1b[?1004l");
        assert!(!parser.callbacks().focus_reporting);
    }

    #[test]
    fn modified_editing_and_function_keys_preserve_modifiers() {
        assert_eq!(
            key_bytes(Key::Backspace, Modifiers::CTRL, false),
            Some(vec![8])
        );
        assert_eq!(
            key_bytes(Key::Enter, Modifiers::ALT, false),
            Some(b"\x1b\r".to_vec())
        );
        assert_eq!(
            key_bytes(Key::C, Modifiers::CTRL | Modifiers::ALT, false),
            Some(vec![27, 3])
        );
        assert_eq!(
            key_bytes(Key::Delete, Modifiers::CTRL, false),
            Some(b"\x1b[3;5~".to_vec())
        );
        assert_eq!(
            key_bytes(Key::F1, Modifiers::SHIFT, false),
            Some(b"\x1b[1;2P".to_vec())
        );
        assert_eq!(
            key_bytes(Key::F5, Modifiers::ALT, false),
            Some(b"\x1b[15;3~".to_vec())
        );
    }

    #[test]
    fn terminal_routes_mouse_wheel_focus_and_alternate_screen_scroll_to_the_app() {
        let ctx = egui::Context::default();
        let (tx, rx) = mpsc::sync_channel(128);
        let screen = Arc::new(Mutex::new(ScreenState::default()));
        screen
            .lock()
            .unwrap()
            .parser
            .process(b"\x1b[?1049h\x1b[?1000h\x1b[?1006h\x1b[?1004h");
        let mut tab = Session {
            id: 1,
            root: "/tmp".into(),
            shell: "test".into(),
            default_name: String::new(),
            screen: screen.clone(),
            tx,
            stopped: Arc::new(AtomicBool::new(false)),
            killer: Arc::new(Mutex::new(None)),
            size: (24, 80),
            selection: None,
            scroll_remainder: 0.0,
        };
        let mut focus = true;
        let mut frame = |events, focused| {
            let _ = ctx.run(
                egui::RawInput {
                    events,
                    focused,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 600.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default()
                        .show(ctx, |ui| render(ui, &mut tab, true, &mut focus, 14.0));
                },
            );
            rx.try_iter()
                .filter_map(|command| match command {
                    Command::Input(bytes) => Some(bytes),
                    _ => None,
                })
                .flatten()
                .collect::<Vec<u8>>()
        };
        frame(vec![], true);
        let pos = egui::pos2(100.0, 100.0);
        let bytes = frame(
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: Modifiers::NONE,
                },
            ],
            true,
        );
        assert!(String::from_utf8_lossy(&bytes).contains("\x1b[<0;"));
        frame(
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
            true,
        );
        let bytes = frame(
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, 120.0),
                modifiers: Modifiers::NONE,
            }],
            true,
        );
        assert!(String::from_utf8_lossy(&bytes).contains("\x1b[<64;"));
        assert_eq!(frame(vec![], false), b"\x1b[O");
        assert!(frame(vec![], true).starts_with(b"\x1b[I"));
        screen
            .lock()
            .unwrap()
            .parser
            .process(b"\x1b[?1000l\x1b[?1h");
        let bytes = frame(
            vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -120.0),
                modifiers: Modifiers::NONE,
            }],
            true,
        );
        assert!(String::from_utf8_lossy(&bytes).contains("\x1bOB"));
    }

    #[test]
    fn mouse_reports_use_requested_encoding_and_release_semantics() {
        let mut parser = vt100::Parser::new(30, 100, 0);
        parser.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(
            mouse_bytes(parser.screen(), 0, (4, 9), false),
            b"\x1b[<0;10;5M"
        );
        assert_eq!(
            mouse_bytes(parser.screen(), 0, (4, 9), true),
            b"\x1b[<0;10;5m"
        );
        assert_eq!(
            mouse_bytes(parser.screen(), 64, (4, 9), false),
            b"\x1b[<64;10;5M"
        );
        parser.process(b"\x1b[?1006l");
        assert_eq!(mouse_bytes(parser.screen(), 0, (4, 9), true), b"\x1b[M#*%");
    }

    #[cfg(unix)]
    #[test]
    fn real_pty_answers_interactive_startup_queries() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let expected = b"\x1b]11;rgb:1c1c/1e1e/2323\x1b\\\x1b[>0;0;0c\x1b[?2004;2$y";
        let script = format!(
            r#"stty raw -echo; printf '\033]11;?\007\033[>c\033[?2004$p'; reply=$(dd bs=1 count={} 2>/dev/null | od -An -tx1); stty sane; printf 'PROBE:%s:DONE\n' "$reply""#,
            expected.len()
        );
        let settings = Settings {
            shell: format!("/bin/sh -c {}", shell_words::quote(&script)),
            ..Default::default()
        };
        let tab = Session::spawn(
            1,
            dir.path().to_path_buf(),
            settings,
            egui::Context::default(),
        );
        wait_for(&tab.screen, |s| {
            s.parser.screen().contents().contains(":DONE")
        });
        let output = tab.screen.lock().unwrap().parser.screen().contents();
        let actual: String = output
            .split("PROBE:")
            .nth(1)
            .unwrap()
            .split(":DONE")
            .next()
            .unwrap()
            .split_whitespace()
            .collect();
        let expected: String = expected.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test]
    fn keyboard_maps_control_and_application_sequences() {
        assert_eq!(key_bytes(Key::C, Modifiers::CTRL, false), Some(vec![3]));
        assert_eq!(
            key_bytes(Key::ArrowUp, Modifiers::NONE, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            key_bytes(Key::ArrowUp, Modifiers::NONE, true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            key_bytes(Key::ArrowLeft, Modifiers::CTRL, false),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            key_bytes(Key::Tab, Modifiers::SHIFT, false),
            Some(b"\x1b[Z".to_vec())
        );
    }
    #[test]
    fn a_row_keeps_one_character_per_cell_so_the_columns_line_up() {
        // A right aligned prompt leaves a long gap between the text and the
        // time. The gap has to keep its place in the grid, so every cell
        // contributes one character to the row.
        let mut parser = vt100::Parser::new(4, 40, 10);
        parser.process(b"left");
        parser.process(b"\x1b[1;36mright\x1b[0m");
        let screen = parser.screen();
        let (text, runs) = row_runs(screen, 0, 40, None);
        assert_eq!(text.chars().count(), 40, "one character per cell");
        assert_eq!(&text[..4], "left");
        assert_eq!(&text[4..9], "right");
        assert!(
            text[9..].chars().all(|character| character == ' '),
            "the rest of the row is blank: {text:?}"
        );
        assert_eq!(runs.len(), 3, "the text, the color and the blank after it");
        assert_eq!(&text[runs[0].0.clone()], "left");
        assert_eq!(&text[runs[1].0.clone()], "right");
        assert_eq!(&text[runs[2].0.clone()], " ".repeat(31));
        // The runs cover the row without a gap.
        assert_eq!(runs[0].0.start, 0);
        assert_eq!(runs.last().unwrap().0.end, text.len());
    }

    #[test]
    fn the_shell_starts_with_the_size_the_interface_reports() -> Result<()> {
        // A shell reads the size of the terminal once, when it starts, and
        // builds its prompt from that number. A right aligned prompt keeps the
        // width it was built with, so the shell must start with the real size
        // and not with a fallback that is corrected later.
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let ctx = egui::Context::default();
        let mut terminal = Terminal::default();
        terminal.settings.shell = "/bin/sh -c 'stty size'".into();
        terminal.settings.tab_name = "Local".into();
        terminal.new_tab(&root, &ctx);
        let state = terminal.tabs[0].screen.clone();
        assert!(terminal.tabs[0].send(Command::Resize(PtySize {
            rows: 30,
            cols: 132,
            pixel_width: 0,
            pixel_height: 0,
        })));
        wait_for(&state, |s| s.parser.screen().contents().contains("30 132"));
        Ok(())
    }

    #[test]
    fn the_first_reported_size_is_used_and_earlier_commands_are_kept() {
        let (tx, rx) = mpsc::channel();
        tx.send(Command::Input(b"typed".to_vec())).unwrap();
        tx.send(Command::Resize(PtySize {
            rows: 30,
            cols: 132,
            pixel_width: 0,
            pixel_height: 0,
        }))
        .unwrap();
        let (size, queued) = wait_for_size(&rx);
        assert_eq!((size.rows, size.cols), (30, 132));
        assert_eq!(queued.len(), 1);
        assert!(matches!(queued.first(), Some(Command::Input(bytes)) if bytes == b"typed"));
    }

    #[test]
    fn a_shell_without_a_reported_size_falls_back_after_a_short_wait() {
        let (_tx, rx) = mpsc::channel();
        let (size, queued) = wait_for_size(&rx);
        assert_eq!(
            (size.rows, size.cols),
            (FALLBACK_SIZE.rows, FALLBACK_SIZE.cols)
        );
        assert!(queued.is_empty());
    }

    #[test]
    fn real_pty_starts_each_tab_in_project_resizes_and_closes() -> Result<()> {
        let first = tempfile::tempdir()?;
        let second = tempfile::tempdir()?;
        let root = first.path().canonicalize()?;
        let other = second.path().canonicalize()?;
        let ctx = egui::Context::default();
        let mut terminal = Terminal::default();
        terminal.settings.shell = "/bin/sh".into();
        terminal.settings.tab_name = "Local".into();
        terminal.new_tab(&root, &ctx);
        let first_state = terminal.tabs[0].screen.clone();
        assert!(terminal.tabs[0].send(Command::Input(
            b"cd /; printf '\\033]2;first tab\\007'; printf 'FIRST_OK\\n'\r".to_vec()
        )));
        wait_for(&first_state, |s| {
            s.parser.callbacks().title == "first tab"
                && s.parser.screen().contents().contains("FIRST_OK")
        });
        terminal.new_tab(&other, &ctx);
        assert_eq!(terminal.labels(), vec!["Local", "Local (2)"]);
        terminal.settings.tab_name = "Changed default".into();
        assert_eq!(terminal.labels(), vec!["Local", "Local (2)"]);
        let state = terminal.tabs[1].screen.clone();
        assert!(terminal.tabs[1].send(Command::Resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0
        })));
        assert!(
            terminal.tabs[1].send(Command::Input(
                b"printf 'PWD='; pwd -P; stty size; printf '\\033[32mPTY_OK\\033[0m\\n'; exit\r"
                    .to_vec()
            ))
        );
        wait_for(&state, |s| {
            s.exited
                && s.parser.screen().contents().contains("PTY_OK")
                && s.parser.screen().contents().contains("30 100")
        });
        let output = state.lock().unwrap().parser.screen().contents();
        assert!(
            output.contains(&format!("PWD={}", other.display())),
            "{output}"
        );
        assert_eq!(state.lock().unwrap().parser.screen().size(), (30, 100));
        drop(terminal);
        wait_for(&first_state, |s| s.exited);
        Ok(())
    }
    #[test]
    fn focus_persists_and_paste_uses_terminal_input_not_an_editor() {
        let ctx = egui::Context::default();
        let (tx, rx) = mpsc::sync_channel(128);
        let screen = Arc::new(Mutex::new(ScreenState::default()));
        screen.lock().unwrap().parser.process(b"\x1b[?2004h");
        let mut tab = Session {
            id: 1,
            root: "/project".into(),
            shell: "/bin/sh".into(),
            default_name: String::new(),
            screen,
            tx,
            stopped: Arc::new(AtomicBool::new(false)),
            killer: Arc::new(Mutex::new(None)),
            size: (24, 80),
            selection: None,
            scroll_remainder: 0.0,
        };
        let mut focus = true;
        let mut frame = |events, size| {
            let _ = ctx.run(
                egui::RawInput {
                    events,
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 400.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default()
                        .show(ctx, |ui| render(ui, &mut tab, true, &mut focus, size));
                },
            );
        };
        frame(vec![], 13.0);
        frame(vec![], 13.0);
        assert!(ctx.memory(|m| m.has_focus(egui::Id::new(("terminal-input", 1u64)))));
        frame(vec![egui::Event::Text("echo 猫".into())], 13.0);
        frame(vec![egui::Event::Paste("first\r\nsecond".into())], 13.0);
        frame(vec![egui::Event::Cut], 13.0);
        frame(vec![], 26.0);
        let commands: Vec<_> = rx.try_iter().collect();
        let sizes: Vec<_> = commands
            .iter()
            .filter_map(|command| match command {
                Command::Resize(size) => Some((size.rows, size.cols)),
                _ => None,
            })
            .collect();
        assert!(sizes.last().unwrap().0 < sizes.first().unwrap().0);
        assert!(sizes.last().unwrap().1 < sizes.first().unwrap().1);
        let bytes: Vec<Vec<u8>> = commands
            .into_iter()
            .filter_map(|command| match command {
                Command::Input(bytes) => Some(bytes),
                _ => None,
            })
            .collect();
        assert_eq!(bytes[0], "echo 猫".as_bytes());
        assert_eq!(bytes[1], b"\x1b[200~first\nsecond\x1b[201~");
        if !cfg!(target_os = "macos") {
            assert_eq!(bytes[2], [24]);
        }
    }
}
