use super::*;

type Position = (u64, u16);
type HistorySelection = (Position, Position);

#[derive(Default)]
pub(super) struct TerminalView {
    pub scrollbar: scrollbar::Scrollbar,
    pub find: Find,
    pub selection: Option<HistorySelection>,
    pub selection_screen: bool,
    pub paste: Option<mpsc::Receiver<Result<ClipboardPaste>>>,
    // Keep attachments alive until the session closes: a CLI may read them
    // only when the user eventually submits the prompt.
    pub attachments: Vec<tempfile::TempPath>,
}

pub(super) struct ClipboardPaste {
    pub text: String,
    pub file: Option<tempfile::TempPath>,
}

impl TerminalView {
    pub fn paste(&mut self, ctx: &egui::Context) {
        if self.paste.is_some() {
            return;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        self.paste = Some(rx);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = read_clipboard();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    pub fn capture_selection(
        &mut self,
        screen: &vt100::Screen,
        selection: Option<((u16, u16), (u16, u16))>,
    ) {
        let top = screen.history_bounds().2;
        self.selection =
            selection.map(|(a, b)| ((top + u64::from(a.0), a.1), (top + u64::from(b.0), b.1)));
        self.selection_screen = screen.alternate_screen();
    }

    pub fn visible_selection(
        &mut self,
        screen: &vt100::Screen,
    ) -> Option<((u16, u16), (u16, u16))> {
        let (first, len, top) = screen.history_bounds();
        let (a, b) = self.selection?;
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        if self.selection_screen != screen.alternate_screen()
            || a.0 < first
            || b.0 >= first + len as u64
        {
            self.selection = None;
            return None;
        }
        let (rows, cols) = screen.size();
        if b.0 < top || a.0 >= top + u64::from(rows) {
            return None;
        }
        Some((
            if a.0 < top {
                (0, 0)
            } else {
                ((a.0 - top) as u16, a.1)
            },
            if b.0 >= top + u64::from(rows) {
                (rows - 1, cols - 1)
            } else {
                ((b.0 - top) as u16, b.1)
            },
        ))
    }
}

fn read_clipboard() -> Result<ClipboardPaste> {
    let mut clipboard = arboard::Clipboard::new().context("Could not open the clipboard")?;
    if let Ok(text) = clipboard.get_text()
        && !text.is_empty()
    {
        return Ok(ClipboardPaste { text, file: None });
    }
    let image = clipboard
        .get_image()
        .context("The clipboard has no text or image")?;
    save_clipboard_image(image)
}

pub(super) fn save_clipboard_image(image: arboard::ImageData<'_>) -> Result<ClipboardPaste> {
    let width = u32::try_from(image.width)?;
    let height = u32::try_from(image.height)?;
    let size = image
        .width
        .checked_mul(image.height)
        .and_then(|size| size.checked_mul(4))
        .context("Image dimensions are too large")?;
    if width == 0 || height == 0 || size > 128 * 1024 * 1024 || size != image.bytes.len() {
        bail!("Clipboard image is invalid or larger than 128 MiB");
    }
    let mut file = tempfile::Builder::new()
        .prefix("glimcode-paste-")
        .suffix(".png")
        .tempfile()?;
    image::write_buffer_with_format(
        &mut file,
        &image.bytes,
        width,
        height,
        image::ExtendedColorType::Rgba8,
        image::ImageFormat::Png,
    )?;
    let file = file.into_temp_path();
    let path = file.to_str().context("Temporary image path is not UTF-8")?;
    Ok(ClipboardPaste {
        text: format!("{} ", shell_words::quote(path)),
        file: Some(file),
    })
}

pub(super) fn selected_history(screen: &vt100::Screen, selection: HistorySelection) -> String {
    let (a, b) = selection;
    let (a, b) = if a <= b { (a, b) } else { (b, a) };
    let (first, len, _) = screen.history_bounds();
    if a.0 < first || b.0 >= first + len as u64 {
        return String::new();
    }
    let cols = screen.size().1;
    let mut text = String::new();
    for row in a.0..=b.0 {
        let start = if row == a.0 { a.1 } else { 0 };
        let end = if row == b.0 {
            b.1.saturating_add(1).min(cols)
        } else {
            cols
        };
        let mut line = String::new();
        for col in start..end {
            if let Some(cell) = screen.history_cell(row, col)
                && !cell.is_wide_continuation()
            {
                if cell.has_contents() {
                    line.push_str(cell.contents());
                } else {
                    line.push(' ');
                }
            }
        }
        text.push_str(if screen.history_wrapped(row) {
            &line
        } else {
            line.trim_end()
        });
        if row != b.0 && !screen.history_wrapped(row) {
            text.push('\n');
        }
    }
    text
}

#[derive(Default)]
pub(super) struct Find {
    pub open: bool,
    pub focus: bool,
    query: String,
    case_sensitive: bool,
    cached: (String, bool),
    pub matches: Vec<HistorySelection>,
    pub active: usize,
    pub step: i32,
    revision: u64,
    refreshed: Option<Instant>,
    focus_ids: Vec<egui::Id>,
}

impl Find {
    pub fn focused(&self, ctx: &egui::Context) -> bool {
        self.open && ctx.memory(|memory| self.focus_ids.iter().any(|id| memory.has_focus(*id)))
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, id: egui::Id) -> bool {
        self.focus_ids.clear();
        let mut close = false;
        egui::Frame::new()
            .fill(chrome::HEADER)
            .inner_margin(6)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let input_id = id.with("find");
                    if ui.memory(|m| m.has_focus(input_id)) {
                        if ui.input_mut(|i| i.consume_key(Modifiers::SHIFT, Key::Enter)) {
                            self.step = -1;
                        } else if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
                            self.step = 1;
                        }
                        close = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
                    }
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.query)
                            .id(input_id)
                            .return_key(None)
                            .char_limit(1024)
                            .desired_width(
                                (ui.available_width() - 210.0)
                                    .clamp(48.0, 300.0)
                                    .min(ui.available_width().max(1.0)),
                            )
                            .hint_text("Find in terminal…"),
                    );
                    self.focus_ids.push(response.id);
                    if std::mem::take(&mut self.focus) {
                        response.request_focus();
                    }
                    ui.memory_mut(|memory| {
                        memory.set_focus_lock_filter(
                            input_id,
                            egui::EventFilter {
                                escape: true,
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                ..Default::default()
                            },
                        )
                    });
                    let case = ui
                        .selectable_label(self.case_sensitive, "Aa")
                        .on_hover_text("Match case");
                    self.focus_ids.push(case.id);
                    if case.clicked() {
                        self.case_sensitive = !self.case_sensitive;
                    }
                    ui.weak(if self.query.is_empty() {
                        String::new()
                    } else if self.matches.is_empty() {
                        "No matches".into()
                    } else {
                        format!(
                            "{}/{}{}",
                            self.active + 1,
                            self.matches.len(),
                            if self.matches.len() == 10_000 {
                                "+"
                            } else {
                                ""
                            }
                        )
                    });
                    let previous = ui.small_button("↑").on_hover_text("Previous · Shift+Enter");
                    let next = ui.small_button("↓").on_hover_text("Next · Enter");
                    let dismiss = ui.small_button("×").on_hover_text("Close · Escape");
                    self.focus_ids.extend([previous.id, next.id, dismiss.id]);
                    if previous.clicked() {
                        self.step = -1;
                    }
                    if next.clicked() {
                        self.step = 1;
                    }
                    close |= dismiss.clicked();
                    for id in self.focus_ids.iter().skip(1) {
                        ui.memory_mut(|memory| {
                            memory.set_focus_lock_filter(
                                *id,
                                egui::EventFilter {
                                    escape: true,
                                    ..Default::default()
                                },
                            )
                        });
                    }
                });
            });
        if close {
            self.open = false;
        }
        close
    }

    pub fn refresh(&mut self, screen: &mut vt100::Screen, revision: u64) {
        if !self.open {
            return;
        }
        let query_changed = self.cached != (self.query.clone(), self.case_sensitive);
        let output_changed = self.revision != revision
            && self
                .refreshed
                .is_none_or(|time| time.elapsed() >= Duration::from_millis(200));
        if query_changed || output_changed {
            let current = self.matches.get(self.active).copied();
            self.matches = find_matches(screen, &self.query, self.case_sensitive);
            self.active = current
                .and_then(|current| self.matches.iter().position(|m| *m == current))
                .unwrap_or(0);
            self.cached = (self.query.clone(), self.case_sensitive);
            self.revision = revision;
            self.refreshed = Some(Instant::now());
        }
        let step = std::mem::take(&mut self.step);
        if !self.matches.is_empty() && (step != 0 || query_changed) {
            self.active =
                (self.active as i32 + step).rem_euclid(self.matches.len() as i32) as usize;
            let row = self.matches[self.active].0.0;
            let (first, len, _) = screen.history_bounds();
            let rows = u64::from(screen.size().0);
            let live_top = first + len as u64 - rows;
            screen.set_scrollback(live_top.saturating_sub(row.saturating_sub(rows / 2)) as usize);
        }
    }

    pub fn needs_refresh(&self, revision: u64) -> bool {
        self.open && self.revision != revision
    }

    pub fn highlight(&self, row: u64, col: u16) -> Option<Color32> {
        if !self.open {
            return None;
        }
        let position = (row, col);
        // Matches are in output order; only examine ranges starting before this cell.
        let end = self
            .matches
            .partition_point(|(start, _)| *start <= position);
        if end > 0 && self.matches[end - 1].1 >= position {
            Some(if end - 1 == self.active {
                Color32::from_rgb(108, 79, 27)
            } else {
                Color32::from_rgb(65, 59, 36)
            })
        } else {
            None
        }
    }
}

pub(super) fn find_matches(
    screen: &vt100::Screen,
    query: &str,
    case_sensitive: bool,
) -> Vec<HistorySelection> {
    if query.is_empty() {
        return vec![];
    }
    let needle = if case_sensitive {
        query.to_owned()
    } else {
        query.to_lowercase()
    };
    let (first, len, _) = screen.history_bounds();
    let mut matches = vec![];
    let mut text = String::new();
    let mut positions = vec![];
    for row in first..first + len as u64 {
        for col in 0..screen.size().1 {
            if let Some(cell) = screen.history_cell(row, col)
                && !cell.is_wide_continuation()
            {
                let contents = if cell.has_contents() {
                    cell.contents()
                } else {
                    " "
                };
                let contents = if case_sensitive {
                    contents.to_owned()
                } else {
                    contents.to_lowercase()
                };
                for _ in 0..contents.len() {
                    positions.push((row, col));
                }
                text.push_str(&contents);
            }
        }
        if !screen.history_wrapped(row) || row + 1 == first + len as u64 {
            for (start, matched) in text.match_indices(&needle) {
                matches.push((positions[start], positions[start + matched.len() - 1]));
                if matches.len() == 10_000 {
                    return matches;
                }
            }
            text.clear();
            positions.clear();
        }
    }
    matches
}

pub(super) fn named_link(
    screen: &vt100::Screen,
    point: (u16, u16),
) -> Option<(Link, (u16, u16), (u16, u16))> {
    let target = screen.hyperlink(point.0, point.1)?;
    let url = url::Url::parse(target).ok()?;
    let link = match url.scheme() {
        "http" | "https" => Link::Url(target.to_owned()),
        "file" => Link::File(url.to_file_path().ok()?.to_str()?.to_owned()),
        _ => return None,
    };
    let mut start = point;
    let mut end = point;
    while start.1 > 0 && screen.hyperlink(point.0, start.1 - 1) == Some(target) {
        start.1 -= 1;
    }
    while end.1 + 1 < screen.size().1 && screen.hyperlink(point.0, end.1 + 1) == Some(target) {
        end.1 += 1;
    }
    Some((link, start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_toolbar_wraps_within_narrow_terminal_widths() {
        for width in [180.0, 280.0, 420.0] {
            for size in [14.0, 24.0, 40.0] {
                let ctx = egui::Context::default();
                ctx.style_mut(|style| style.override_font_id = Some(FontId::proportional(size)));
                let mut find = Find {
                    open: true,
                    query: "build".into(),
                    matches: vec![((0, 0), (0, 4)); 10_000],
                    ..Default::default()
                };
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 500.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            let available = ui.available_rect_before_wrap();
                            find.ui(ui, egui::Id::new("find-layout"));
                            assert!(
                                ui.min_rect().right() <= available.right() + 1.0,
                                "toolbar overflow at width={width}, size={size}: {:?} vs {:?}",
                                ui.min_rect(),
                                available
                            );
                        });
                    },
                );
            }
        }
    }

    #[test]
    fn find_owns_keyboard_input_and_clipboard_results_paste_once() -> Result<()> {
        let ctx = egui::Context::default();
        let (tx, rx) = mpsc::sync_channel(128);
        let mut terminal = Terminal {
            visible: true,
            ..Default::default()
        };
        terminal.tabs.push(Session {
            id: 1,
            root: "/project".into(),
            shell: "test".into(),
            default_name: String::new(),
            screen: Arc::new(Mutex::new(ScreenState::default())),
            tx,
            stopped: Arc::new(AtomicBool::new(false)),
            killer: Arc::new(Mutex::new(None)),
            size: (24, 80),
            selection: None,
            scroll_remainder: 0.0,
            view: TerminalView::default(),
            persistent: None,
            notice: None,
        });
        terminal.tabs[0]
            .screen
            .lock()
            .unwrap()
            .parser
            .process(b"\x1b[?2004hbefore\r\nbefore");
        let mut focus = true;
        let mut frame = |terminal: &mut Terminal, events| {
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
                    terminal.shortcuts(ctx, Path::new("/project"));
                    egui::CentralPanel::default().show(ctx, |ui| {
                        render(ui, &mut terminal.tabs[0], true, &mut focus, 14.0);
                    });
                },
            );
        };
        let key = |key, modifiers| egui::Event::Key {
            key,
            modifiers,
            pressed: true,
            physical_key: None,
            repeat: false,
        };
        frame(&mut terminal, vec![]);
        frame(&mut terminal, vec![]);
        frame(&mut terminal, vec![key(Key::F, Modifiers::COMMAND)]);
        assert!(terminal.tabs[0].view.find.open);
        frame(&mut terminal, vec![egui::Event::Text("before".into())]);
        assert_eq!(terminal.tabs[0].view.find.matches.len(), 2);
        assert!(
            terminal.focused(&ctx),
            "find remains part of the terminal's keyboard scope"
        );
        frame(&mut terminal, vec![key(Key::Enter, Modifiers::NONE)]);
        assert_eq!(terminal.tabs[0].view.find.active, 1);
        frame(&mut terminal, vec![key(Key::Enter, Modifiers::SHIFT)]);
        assert_eq!(terminal.tabs[0].view.find.active, 0);
        frame(&mut terminal, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(!terminal.tabs[0].view.find.open);
        assert!(
            !rx.try_iter()
                .any(|command| matches!(command, Command::Input(_))),
            "find text and navigation never leak to the shell"
        );
        frame(&mut terminal, vec![key(Key::Enter, Modifiers::SHIFT)]);
        let inputs: Vec<_> = rx
            .try_iter()
            .filter_map(|command| match command {
                Command::Input(bytes) => Some(bytes),
                _ => None,
            })
            .collect();
        assert_eq!(inputs, vec![b"\x1b[13;2u".to_vec()]);

        let (tx, paste_rx) = mpsc::sync_channel(1);
        let paste = save_clipboard_image(arboard::ImageData {
            width: 1,
            height: 1,
            bytes: std::borrow::Cow::Borrowed(&[0, 0, 0, 255]),
        })?;
        let expected = format!("\x1b[200~{}\x1b[201~", paste.text).into_bytes();
        tx.send(Ok(paste))?;
        terminal.tabs[0].view.paste = Some(paste_rx);
        frame(&mut terminal, vec![]);
        frame(&mut terminal, vec![]);
        assert_eq!(terminal.tabs[0].view.attachments.len(), 1);
        let inputs: Vec<_> = rx
            .try_iter()
            .filter_map(|command| match command {
                Command::Input(bytes) => Some(bytes),
                _ => None,
            })
            .collect();
        assert_eq!(
            inputs,
            vec![expected],
            "an asynchronous image paste is sent once, without Enter"
        );
        Ok(())
    }

    #[test]
    fn search_finds_retained_output_unicode_and_wrapped_matches() {
        let mut parser = vt100::Parser::new(3, 8, 10);
        parser.process("before\r\n猫FoObarXYZ\r\nafter\r\nlast".as_bytes());
        assert!(
            !find_matches(parser.screen(), "before", true).is_empty(),
            "search includes scrollback"
        );
        let found = find_matches(parser.screen(), "foobarxyz", false);
        assert_eq!(found.len(), 1);
        assert!(found[0].0.0 < found[0].1.0, "match crosses a soft wrap");
        assert_eq!(selected_history(parser.screen(), found[0]), "FoObarXYZ");
        assert!(find_matches(parser.screen(), "foobarxyz", true).is_empty());
        let mut find = Find {
            open: true,
            query: "before".into(),
            ..Default::default()
        };
        find.refresh(parser.screen_mut(), 1);
        assert_eq!(find.matches.len(), 1);
        assert!(
            parser.screen().scrollback() > 0,
            "finding old output reveals it"
        );
        find.step = -1;
        find.refresh(parser.screen_mut(), 1);
        assert_eq!(find.active, 0, "navigation wraps");
    }

    #[test]
    fn history_selection_survives_scrolling_output_and_expires_on_eviction() {
        let mut parser = vt100::Parser::new(3, 12, 4);
        parser.process(b"one\r\ntwo\r\nthree");
        let mut view = TerminalView::default();
        view.capture_selection(parser.screen(), Some(((0, 0), (2, 4))));
        let original = view.selection.unwrap();
        assert_eq!(
            selected_history(parser.screen(), original),
            "one\ntwo\nthree"
        );
        parser.process(b"\r\nfour\r\nfive");
        assert_eq!(
            view.visible_selection(parser.screen()),
            Some(((0, 0), (0, 4)))
        );
        assert_eq!(
            selected_history(parser.screen(), original),
            "one\ntwo\nthree"
        );
        parser.screen_mut().set_scrollback(2);
        assert_eq!(
            view.visible_selection(parser.screen()),
            Some(((0, 0), (2, 4)))
        );
        parser.process(b"\r\nsix\r\nseven\r\neight");
        assert_eq!(view.visible_selection(parser.screen()), None);
        assert!(
            view.selection.is_none(),
            "evicted rows cannot select unrelated output"
        );
    }

    #[test]
    fn clipboard_image_is_a_valid_png_with_a_session_owned_lifetime() -> Result<()> {
        let result = save_clipboard_image(arboard::ImageData {
            width: 2,
            height: 1,
            bytes: std::borrow::Cow::Borrowed(&[255, 0, 0, 255, 0, 255, 0, 255]),
        })?;
        let args = shell_words::split(&result.text)?;
        assert_eq!(args.len(), 1);
        let path = PathBuf::from(&args[0]);
        let image = image::open(&path)?.into_rgba8();
        assert_eq!(image.dimensions(), (2, 1));
        assert_eq!(image.as_raw(), &[255, 0, 0, 255, 0, 255, 0, 255]);
        let mut view = TerminalView::default();
        view.attachments.push(result.file.unwrap());
        assert!(path.exists());
        drop(view);
        assert!(
            !path.exists(),
            "closing the session removes temporary images"
        );
        assert!(
            save_clipboard_image(arboard::ImageData {
                width: 2,
                height: 2,
                bytes: std::borrow::Cow::Borrowed(&[0; 4]),
            })
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn named_links_follow_cells_through_scroll_and_clear_on_overwrite() {
        let mut parser = vt100::Parser::new_with_callbacks(2, 20, 10, Callbacks::default());
        for byte in
            b"\x1b]8;id=build;https://example.com/a;b\x1b\\View build\x1b]8;;\x1b\\\r\nnext\r\nlast"
        {
            parser.process(&[*byte]);
        }
        parser.screen_mut().set_scrollback(1);
        let (link, start, end) = named_link(parser.screen(), (0, 4)).unwrap();
        assert_eq!(link, Link::Url("https://example.com/a;b".into()));
        assert_eq!((start, end), ((0, 0), (0, 9)));
        parser.screen_mut().set_scrollback(0);
        parser.process(b"\x1b[1;1Hoverwrite");
        assert!(named_link(parser.screen(), (0, 0)).is_none());
        parser.process(b"\x1b]8;;javascript:alert(1)\x1b\\unsafe\x1b]8;;\x1b\\");
        assert!(named_link(parser.screen(), (0, 10)).is_none());
    }

    #[test]
    fn search_results_and_link_metadata_have_bounds() {
        let mut parser = vt100::Parser::new(2, 200, 100);
        for _ in 0..100 {
            parser.process(format!("{}\r\n", "a".repeat(200)).as_bytes());
        }
        assert_eq!(find_matches(parser.screen(), "a", true).len(), 10_000);
        for i in 0..4100 {
            parser
                .screen_mut()
                .set_hyperlink(&format!("https://example.com/{i}"));
        }
        parser.process(b"x");
        let (row, col) = parser.screen().cursor_position();
        assert!(
            parser
                .screen()
                .hyperlink(row, col.saturating_sub(1))
                .is_none()
        );
    }
}
