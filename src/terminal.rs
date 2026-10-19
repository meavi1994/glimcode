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
    time::Duration,
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
}
impl vt100::Callbacks for Callbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = clean_title(&String::from_utf8_lossy(title));
    }
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.first() == Some(&b"7".as_slice())
            && let Some(path) = params.get(1)
            && let Ok(url) = url::Url::parse(&String::from_utf8_lossy(path))
            && let Ok(path) = url.to_file_path()
        {
            self.directory = Some(path);
        }
    }
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if i1.is_some() {
            return;
        }
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (c, first) {
            ('n', 6) => {
                let (row, col) = screen.cursor_position();
                self.replies
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            ('n', 5) => self.replies.extend_from_slice(b"\x1b[0n"),
            ('c', 0) => self.replies.extend_from_slice(b"\x1b[?1;2c"),
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
        let writer_tx = tx.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<()> {
                let pair = portable_pty::native_pty_system().openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })?;
                let mut reader = pair.master.try_clone_reader()?;
                let mut writer = pair.master.take_writer()?;
                let mut child = ManagedChild(pair.slave.spawn_command(settings.command(&cwd)?)?);
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
                        let replies = {
                            let mut state = output.lock().unwrap();
                            state.parser.process(&buffer[..count]);
                            std::mem::take(&mut state.parser.callbacks_mut().replies)
                        };
                        if !replies.is_empty() {
                            let _ = writer_tx.try_send(Command::Input(replies));
                        }
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
                    match rx.recv_timeout(Duration::from_millis(30)) {
                        Ok(Command::Input(bytes)) => {
                            writer.write_all(&bytes)?;
                            writer.flush()?;
                        }
                        Ok(Command::Resize(size)) => {
                            pair.master.resize(size)?;
                            shared
                                .lock()
                                .unwrap()
                                .parser
                                .screen_mut()
                                .set_size(size.rows, size.cols);
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
            size: (24, 80),
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
                            render(
                                ui,
                                tab,
                                enabled && !self.settings_open,
                                &mut self.focus,
                                self.settings.terminal_font_size,
                            );
                        }
                    });
            });
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
            return Some(vec![letter]);
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
        Key::Enter => "\r",
        Key::Tab => {
            if m.shift {
                "\x1b[Z"
            } else {
                "\t"
            }
        }
        Key::Backspace => {
            if m.alt {
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

fn render(ui: &mut egui::Ui, tab: &mut Session, enabled: bool, focus: &mut bool, font_size: f32) {
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
    if size != tab.size
        && tab.send(Command::Resize(PtySize {
            rows: size.0,
            cols: size.1,
            pixel_width: 0,
            pixel_height: 0,
        }))
    {
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
    if response.drag_started()
        && let Some(pos) = ui.input(|i| i.pointer.press_origin())
    {
        let p = point(pos);
        tab.selection = Some((p, p));
    }
    if response.dragged()
        && let Some(pos) = response.interact_pointer_pos()
        && let Some((_, end)) = &mut tab.selection
    {
        *end = point(pos);
    }
    if response.clicked() {
        tab.selection = None;
    }
    let mut state = tab.screen.lock().unwrap();
    if response.hovered() {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        tab.scroll_remainder += scroll / cell.y;
        let lines = tab.scroll_remainder.trunc() as i64;
        if lines != 0 {
            tab.scroll_remainder -= lines as f32;
            let offset = state.parser.screen().scrollback() as i64 + lines;
            state
                .parser
                .screen_mut()
                .set_scrollback(offset.max(0) as usize);
            tab.selection = None;
        }
    }
    if enabled
        && response.double_clicked()
        && let Some(pos) = response.interact_pointer_pos()
    {
        tab.selection = terminal_word(state.parser.screen(), point(pos));
    }
    let mut outgoing = vec![];
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
        for col in 0..size.1 {
            let Some(c) = screen.cell(row, col) else {
                continue;
            };
            let origin = rect.min + egui::vec2(f32::from(col) * cell.x, f32::from(row) * cell.y);
            let cell_rect = egui::Rect::from_min_size(
                origin,
                egui::vec2(cell.x * if c.is_wide() { 2.0 } else { 1.0 }, cell.y),
            );
            let mut fg = color(c.fgcolor(), chrome::TEXT);
            let mut bg = color(c.bgcolor(), chrome::EDITOR);
            if c.inverse() {
                std::mem::swap(&mut fg, &mut bg);
            }
            if selection.is_some_and(|(start, end)| (row, col) >= start && (row, col) <= end) {
                bg = chrome::SELECTED;
            }
            if bg != chrome::EDITOR {
                painter.rect_filled(cell_rect, 0.0, bg);
            }
            if !c.is_wide_continuation() && !c.contents().is_empty() {
                painter.text(
                    origin,
                    egui::Align2::LEFT_TOP,
                    c.contents(),
                    font.clone(),
                    if c.dim() { fg.gamma_multiply(0.6) } else { fg },
                );
            }
            if c.underline() {
                painter.line_segment(
                    [cell_rect.left_bottom(), cell_rect.right_bottom()],
                    egui::Stroke::new(1.0_f32, fg),
                );
            }
        }
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
    #[cfg(unix)]
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
    }
}
