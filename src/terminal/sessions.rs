//! Inspect the dedicated Glimcode server without blocking the UI thread.
use super::*;

enum Reply {
    Listed(std::result::Result<Vec<tmux::SessionInfo>, String>),
    Killed(std::result::Result<tmux::SessionInfo, String>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> tmux::SessionInfo {
        tmux::SessionInfo {
            id: "$7".into(),
            name: "glimcode-test".into(),
            clients: 0,
            windows: 1,
            directory: "/project".into(),
        }
    }

    #[test]
    fn failed_termination_keeps_confirmation_open_with_error() {
        let ctx = egui::Context::default();
        let (tx, rx) = mpsc::channel();
        let mut manager = Manager {
            open: true,
            pending: Some(session()),
            rx: Some(rx),
            ..Default::default()
        };
        tx.send(Reply::Killed(Err("tmux: access denied".into())))
            .unwrap();
        assert!(manager.poll(&ctx).is_none());
        assert!(manager.open);
        assert_eq!(manager.pending, Some(session()));
        assert_eq!(manager.error.as_deref(), Some("tmux: access denied"));
        assert!(manager.rx.is_none());
    }

    #[test]
    fn refresh_keeps_inventory_and_dialog_geometry_stable() {
        let ctx = egui::Context::default();
        let mut manager = Manager {
            open: true,
            sessions: vec![session()],
            refreshed: Some(Instant::now()),
            ..Default::default()
        };
        let frame = |manager: &mut Manager| {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 900.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    manager.show(ctx, &Default::default());
                },
            );
            ctx.memory(|memory| memory.area_rect(egui::Id::new("tmux-session-manager")))
                .unwrap()
        };
        for _ in 0..3 {
            frame(&mut manager);
        }
        let before = frame(&mut manager);
        assert!(before.width() >= 960.0 && before.height() >= 650.0);
        let (tx, rx) = mpsc::channel();
        manager.rx = Some(rx);
        for _ in 0..3 {
            assert_eq!(frame(&mut manager), before);
            assert_eq!(manager.sessions, vec![session()]);
        }
        tx.send(Reply::Listed(Ok(vec![session()]))).unwrap();
        assert_eq!(frame(&mut manager), before);
        assert_eq!(frame(&mut manager), before);
    }

    #[test]
    fn manager_renders_inventory_and_confirmation_with_no_terminal_tabs() {
        let ctx = egui::Context::default();
        let mut manager = Manager {
            open: true,
            sessions: vec![session()],
            refreshed: Some(Instant::now()),
            ..Default::default()
        };
        for confirming in [false, true] {
            manager.pending = confirming.then(session);
            for _ in 0..2 {
                let output = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1000.0, 700.0),
                        )),
                        ..Default::default()
                    },
                    |ctx| {
                        manager.show(ctx, &Default::default());
                    },
                );
                assert!(!output.shapes.is_empty());
            }
            assert!(manager.open);
        }
    }
}

#[derive(Default)]
pub(super) struct Manager {
    pub open: bool,
    sessions: Vec<tmux::SessionInfo>,
    pending: Option<tmux::SessionInfo>,
    error: Option<String>,
    rx: Option<mpsc::Receiver<Reply>>,
    refreshed: Option<Instant>,
    killing: bool,
}

impl Manager {
    pub fn open(&mut self, ctx: &egui::Context) {
        self.open = true;
        if self.rx.is_none() {
            self.refresh(ctx);
        }
    }

    fn refresh(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.refreshed = Some(Instant::now());
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = tmux::list_sessions().map_err(|error| format!("{error:#}"));
            let _ = tx.send(Reply::Listed(result));
            ctx.request_repaint();
        });
    }

    fn kill(&mut self, session: tmux::SessionInfo, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.error = None;
        self.killing = true;
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = tmux::kill_session(&session)
                .map(|()| session)
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(Reply::Killed(result));
            ctx.request_repaint();
        });
    }

    fn poll(&mut self, ctx: &egui::Context) -> Option<String> {
        let result = self.rx.as_ref()?.try_recv();
        let reply = match result {
            Ok(reply) => reply,
            Err(mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(100));
                return None;
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.rx = None;
                self.killing = false;
                self.error =
                    Some("The session operation stopped unexpectedly. Please retry.".into());
                return None;
            }
        };
        self.rx = None;
        match reply {
            Reply::Listed(Ok(sessions)) => {
                self.sessions = sessions;
                self.error = None;
            }
            Reply::Killed(Ok(session)) => {
                self.killing = false;
                self.pending = None;
                self.refresh(ctx);
                return Some(session.name);
            }
            Reply::Listed(Err(error)) => self.error = Some(error),
            Reply::Killed(Err(error)) => {
                self.killing = false;
                self.error = Some(error);
            }
        }
        None
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        local: &std::collections::HashSet<String>,
    ) -> Option<String> {
        let killed = self.poll(ctx);
        if !self.open {
            return killed;
        }
        if self.rx.is_none()
            && self.pending.is_none()
            && self
                .refreshed
                .is_none_or(|time| time.elapsed() >= Duration::from_secs(5))
        {
            self.refresh(ctx);
        }
        ctx.request_repaint_after(Duration::from_secs(5));
        let busy = self.rx.is_some();
        let mut dismiss = false;
        let mut refresh = false;
        let mut kill = None;
        egui::Modal::new(egui::Id::new("tmux-session-manager"))
            .frame(chrome::popup_frame())
            .show(ctx, |ui| {
                chrome::modal_style(ui, 960.0);
                let height = (ctx.content_rect().height() * 0.78)
                    .min(720.0)
                    .min((ctx.content_rect().height() - 100.0).max(180.0));
                ui.set_min_height(height);
                let close = chrome::dialog_header(ui, ">_", "Terminal sessions", "All sessions on Glimcode’s tmux server", chrome::BLUE);
                // Keep the result visible until a requested termination finishes.
                dismiss = close && !self.killing;
                if let Some(session) = self.pending.clone() {
                    ui.heading("End this session?");
                    ui.label(egui::RichText::new(&session.name).monospace());
                    ui.label("This ends the shell and its running programs. Attached terminals and shared browser clients will disconnect.");
                    if session.clients != 0 {
                        ui.colored_label(chrome::WARNING, format!("{} client(s) currently attached", session.clients));
                    }
                    chrome::dialog_divider(ui);
                    ui.add_enabled_ui(!busy, |ui| {
                        ui.horizontal(|ui| {
                            if chrome::dialog_button(ui, "End session", Some(chrome::DANGER)).clicked() {
                                kill = Some(session);
                            }
                            if chrome::dialog_button(ui, "Cancel", None).clicked() {
                                self.pending = None;
                                self.error = None;
                            }
                        });
                    });
                } else {
                    ui.label("Orphan = no tmux clients attached. It may belong to a closed project and can still be restored later.");
                    ui.horizontal(|ui| {
                        let orphans = self.sessions.iter().filter(|s| s.clients == 0).count();
                        ui.label(format!("{} sessions · {orphans} orphans", self.sessions.len()));
                        refresh = ui.button("Refresh").clicked() && !busy;
                    });
                    chrome::dialog_divider(ui);
                    egui::ScrollArea::vertical().id_salt("tmux-session-list")
                        .max_height((height - 220.0).max(100.0))
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            if self.sessions.is_empty() && !busy && self.error.is_none() {
                                ui.label("No Glimcode tmux sessions are running.");
                            }
                            for session in &self.sessions {
                                ui.push_id(&session.id, |ui| {
                                    chrome::dialog_card(ui, |ui| {
                                        ui.horizontal(|ui| {
                                            let orphan = session.clients == 0;
                                            ui.colored_label(if orphan { chrome::WARNING } else { chrome::GREEN }, if orphan { "Orphan" } else { "Attached" });
                                            if local.contains(&session.name) {
                                                ui.small("This workspace");
                                            }
                                            ui.label(format!("{} clients · {} windows", session.clients, session.windows));
                                        });
                                        ui.add(egui::Label::new(egui::RichText::new(&session.name).monospace()).wrap());
                                        ui.add(egui::Label::new(&session.directory).wrap());
                                        ui.horizontal(|ui| {
                                            if ui.button("Copy session ID").clicked() {
                                                ui.ctx().copy_text(session.name.clone());
                                            }
                                            if ui.button("End session…").clicked() {
                                                self.pending = Some(session.clone());
                                                self.error = None;
                                            }
                                        });
                                    });
                                });
                            }
                        });
                }
                // Reserve the footer even while idle. Adding/removing a spinner
                // used to change the modal's size and recenter it on each refresh.
                ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 48.0), egui::Layout::top_down(egui::Align::Min), |ui| {
                    if busy {
                        ui.horizontal(|ui| { ui.spinner(); ui.label(if self.killing { "Ending session…" } else { "Refreshing…" }); });
                    }
                    if let Some(error) = &self.error {
                        ui.add(egui::Label::new(egui::RichText::new(error).color(chrome::DANGER)).wrap());
                    }
                });
                if !busy && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                    if self.pending.is_some() { self.pending = None; self.error = None; }
                    else { dismiss = true; }
                }
            });
        if dismiss {
            self.open = false;
            self.pending = None;
        } else if let Some(session) = kill {
            self.kill(session, ctx);
        } else if refresh {
            self.refresh(ctx);
        }
        killed
    }
}
