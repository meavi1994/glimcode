//! Serialize desktop and browser geometry through one deduplicated sender.
use super::*;

#[derive(Default)]
pub(super) struct ResizeState {
    desktop: Option<PtySize>,
    remote: Option<PtySize>,
    sent: Option<PtySize>,
}

#[derive(Clone)]
pub(super) struct ResizeSender {
    tx: SyncSender<Command>,
    state: Arc<Mutex<ResizeState>>,
}

impl ResizeSender {
    pub fn new(tx: SyncSender<Command>, state: Arc<Mutex<ResizeState>>) -> Self {
        Self { tx, state }
    }

    pub fn desktop(&self, size: PtySize) -> bool {
        let mut state = self.state.lock().unwrap();
        state.desktop = Some(size);
        self.send(&mut state)
    }

    pub fn remote(&self, size: Option<PtySize>) -> bool {
        let mut state = self.state.lock().unwrap();
        state.remote = size;
        self.send(&mut state)
    }

    pub fn retry(&self) {
        self.send(&mut self.state.lock().unwrap());
    }

    fn send(&self, state: &mut ResizeState) -> bool {
        let Some(mut size) = state.remote.or(state.desktop) else {
            return true;
        };
        size.pixel_width = 0;
        size.pixel_height = 0;
        if state.sent == Some(size) {
            return true;
        }
        if self.tx.try_send(Command::Resize(size)).is_err() {
            return false;
        }
        state.sent = Some(size);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: u64) -> (Session, mpsc::Receiver<Command>) {
        let (tx, rx) = mpsc::sync_channel(128);
        (
            Session {
                id,
                root: "/tmp".into(),
                shell: "test".into(),
                default_name: String::new(),
                screen: Arc::new(Mutex::new(ScreenState::default())),
                tx,
                resize: Default::default(),
                stopped: Arc::new(AtomicBool::new(false)),
                killer: Arc::new(Mutex::new(None)),
                size: (0, 0),
                selection: None,
                scroll_remainder: 0.0,
                view: TerminalView::default(),
                persistent: None,
                notice: None,
            },
            rx,
        )
    }

    fn frame(ctx: &egui::Context, terminal: &mut Terminal, panel: bool) {
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 600.0),
                )),
                ..Default::default()
            },
            |ctx| {
                if panel {
                    terminal.panel(ctx, Path::new("/tmp"), true);
                } else {
                    egui::CentralPanel::default()
                        .show(ctx, |ui| terminal.contents(ui, Path::new("/tmp"), true));
                }
            },
        );
    }

    #[test]
    fn find_and_tab_switching_do_not_resize_any_session() {
        let ctx = egui::Context::default();
        let (a, ra) = session(1);
        let (b, rb) = session(2);
        let mut terminal = Terminal {
            tabs: vec![a, b],
            ..Default::default()
        };
        frame(&ctx, &mut terminal, false);
        let original = terminal.tabs[0].size;
        ra.try_iter().for_each(drop);
        rb.try_iter().for_each(drop);
        for (active, find) in [(0, true), (1, false), (0, true), (0, false)] {
            terminal.active = active;
            terminal.tabs[active].view.find.open = find;
            for _ in 0..3 {
                frame(&ctx, &mut terminal, false);
            }
            assert_eq!(terminal.tabs[active].size, original);
            assert!(!ra.try_iter().any(|c| matches!(c, Command::Resize(_))));
            assert!(!rb.try_iter().any(|c| matches!(c, Command::Resize(_))));
        }
    }

    #[test]
    fn restored_panel_height_is_applied_before_the_first_resize() {
        let ctx = egui::Context::default();
        let (tab, _rx) = session(1);
        let mut terminal = Terminal {
            tabs: vec![tab],
            visible: true,
            ..Default::default()
        };
        ctx.data_mut(|d| {
            d.insert_persisted(
                egui::Id::new("terminal-panel"),
                egui::containers::panel::PanelState {
                    rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 450.0)),
                },
            )
        });
        frame(&ctx, &mut terminal, true);
        let original = terminal.tabs[0].size;
        let snapshot = terminal.snapshot();
        let mut reopened = Terminal::default();
        reopened.restore(&snapshot);
        reopened.restored_tabs.clear();
        let (tab, rx) = session(2);
        reopened.tabs.push(tab);
        frame(&egui::Context::default(), &mut reopened, true);
        let resizes: Vec<_> = rx
            .try_iter()
            .filter_map(|c| {
                if let Command::Resize(s) = c {
                    Some((s.rows, s.cols))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(resizes, vec![original]);
        assert_eq!(reopened.panel_height, terminal.panel_height);
    }

    #[test]
    fn browser_resizes_are_deduplicated_and_restore_the_latest_desktop() {
        let (tx, rx) = mpsc::sync_channel(16);
        let resize = ResizeSender::new(tx, Default::default());
        let desktop = PtySize {
            rows: 30,
            cols: 100,
            ..FALLBACK_SIZE
        };
        let phone = PtySize {
            rows: 45,
            cols: 42,
            ..FALLBACK_SIZE
        };
        assert!(resize.desktop(FALLBACK_SIZE));
        assert!(resize.remote(None)); // Stopping a share without a browser is a no-op.
        assert!(resize.remote(Some(phone)));
        assert!(resize.desktop(desktop));
        assert!(resize.remote(Some(phone)));
        assert!(resize.remote(None));
        assert!(resize.desktop(desktop));
        let sizes: Vec<_> = rx
            .try_iter()
            .map(|c| match c {
                Command::Resize(s) => s,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(sizes, vec![FALLBACK_SIZE, phone, desktop]);
    }

    #[test]
    fn a_full_queue_does_not_lose_the_desired_geometry() {
        let (tx, rx) = mpsc::sync_channel(1);
        let resize = ResizeSender::new(tx, Default::default());
        assert!(resize.desktop(FALLBACK_SIZE));
        let phone = PtySize {
            rows: 45,
            cols: 42,
            ..FALLBACK_SIZE
        };
        assert!(!resize.remote(Some(phone)));
        rx.try_recv().unwrap();
        resize.retry();
        assert!(matches!(rx.try_recv(), Ok(Command::Resize(s)) if s == phone));
        assert!(resize.remote(None));
        assert!(matches!(rx.try_recv(), Ok(Command::Resize(s)) if s == FALLBACK_SIZE));
    }
}
