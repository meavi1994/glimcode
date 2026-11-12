//! Black-box compatibility checks for zmx, using Glimcode's actual PTY/parser.
use super::tests::wait_for;
use super::*;

#[test]
#[ignore = "requires zmx and permission to create local Unix sockets"]
fn zmx_reconnect_preserves_panel_geometry_before_attaching() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let settings = Settings {
        shell: "/bin/sh".into(),
        terminal_backend: "zmx".into(),
        tmux_enabled: true,
        ..Default::default()
    };
    let ctx = egui::Context::default();
    let mut terminal = Terminal {
        settings: settings.clone(),
        ..Default::default()
    };
    terminal.new_tab(dir.path(), &ctx);
    let backend = terminal.tabs[0].persistent.clone().unwrap();
    struct Cleanup(tmux::Persistent);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = zmx::run(self.0.executable(), &["kill", &self.0.name, "--force"]);
        }
    }
    let _cleanup = Cleanup(backend);
    ctx.data_mut(|d| {
        d.insert_persisted(
            egui::Id::new("terminal-panel"),
            egui::containers::panel::PanelState {
                rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 450.0)),
            },
        )
    });
    let frame = |ctx: &egui::Context, terminal: &mut Terminal| {
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 600.0),
                )),
                ..Default::default()
            },
            |ctx| terminal.panel(ctx, dir.path(), true),
        );
    };
    frame(&ctx, &mut terminal);
    let original = terminal.tabs[0].size;
    wait_for(&terminal.tabs[0].screen, |s| s.status == "Running");
    let snapshot = terminal.snapshot();
    let screen = terminal.tabs[0].screen.clone();
    drop(terminal);
    wait_for(&screen, |s| s.exited);
    let reopened_ctx = egui::Context::default();
    let mut reopened = Terminal {
        settings,
        ..Default::default()
    };
    reopened.restore(&snapshot);
    frame(&reopened_ctx, &mut reopened);
    assert_eq!(reopened.tabs[0].size, original);
    wait_for(&reopened.tabs[0].screen, |s| s.status == "Running");
    reopened.tabs[0].send(Command::Input(
        b"stty size; printf 'GEOMETRY_RESTORED\\n'\n".to_vec(),
    ));
    wait_for(&reopened.tabs[0].screen, |s| {
        s.parser.screen().contents().contains("GEOMETRY_RESTORED")
            && s.parser
                .screen()
                .contents()
                .contains(&format!("{} {}", original.0, original.1))
    });
    Ok(())
}

#[test]
#[ignore = "requires zmx, ttyd and permission to create Unix and TCP sockets"]
fn zmx_stopping_unused_share_preserves_resized_hidden_desktop() -> Result<()> {
    use std::net::{Ipv4Addr, TcpListener};
    let ctx = egui::Context::default();
    let dir = tempfile::tempdir()?;
    let mut terminal = Terminal {
        settings: Settings {
            shell: "/bin/sh".into(),
            terminal_backend: "zmx".into(),
            tmux_enabled: true,
            ..Default::default()
        },
        ..Default::default()
    };
    terminal.new_tab(dir.path(), &ctx);
    let backend = terminal.tabs[0].persistent.clone().unwrap();
    struct Cleanup(tmux::Persistent);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = zmx::run(self.0.executable(), &["kill", &self.0.name, "--force"]);
        }
    }
    let _cleanup = Cleanup(backend.clone());
    let frame = |terminal: &mut Terminal, dimensions| {
        let _ = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, dimensions)),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default()
                    .show(ctx, |ui| terminal.contents(ui, dir.path(), true));
            },
        );
    };
    frame(&mut terminal, egui::vec2(1000.0, 600.0));
    let original = terminal.tabs[0].size;
    wait_for(&terminal.tabs[0].screen, |s| s.status == "Running");
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = reservation.local_addr()?.port();
    drop(reservation);
    let share = share::Share::start(
        backend,
        share::Options {
            https: false,
            port,
            ..Default::default()
        },
        ctx.clone(),
        terminal.tabs[0].resize(),
    )?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !matches!(share.status(), share::Status::Running) {
        if let share::Status::Failed(e) = share.status() {
            bail!("share startup: {e}");
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    frame(&mut terminal, egui::vec2(800.0, 400.0));
    let desired = terminal.tabs[0].size;
    assert_ne!(desired, original);
    wait_for(&terminal.tabs[0].screen, |s| {
        s.parser.screen().size() == desired
    });
    assert!(share.size().is_none());
    terminal.visible = false;
    drop(share);
    // The input marker is a barrier after any resize queued by Share::drop.
    terminal.tabs[0].send(Command::Input(
        b"printf '\\033]2;SHARE_STOPPED\\007'\n".to_vec(),
    ));
    wait_for(&terminal.tabs[0].screen, |s| {
        s.parser.callbacks().title == "SHARE_STOPPED"
    });
    assert_eq!(
        terminal.tabs[0]
            .screen
            .lock()
            .unwrap()
            .parser
            .screen()
            .size(),
        desired
    );
    let phone = PtySize {
        rows: 45,
        cols: 42,
        ..FALLBACK_SIZE
    };
    terminal.tabs[0].resize().remote(Some(phone));
    wait_for(&terminal.tabs[0].screen, |s| {
        s.parser.screen().size() == (phone.rows, phone.cols)
    });
    terminal.tabs[0].resize().remote(None);
    wait_for(&terminal.tabs[0].screen, |s| {
        s.parser.screen().size() == desired
    });
    Ok(())
}

#[test]
#[ignore = "requires GLIMCODE_ZMX_PATH and permission to create local Unix sockets"]
fn zmx_output_reflows_after_narrowing_and_restoring_desktop_width() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let backend = tmux::Persistent::new_with_backend(None, "zmx")
        .context("Install zmx 0.8.1 or set GLIMCODE_ZMX_PATH")?;
    struct Cleanup(tmux::Persistent);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = zmx::run(self.0.executable(), &["kill", &self.0.name, "--force"]);
        }
    }
    let _cleanup = Cleanup(backend.clone());
    let tab = Session::spawn_with(
        1,
        dir.path().into(),
        Settings {
            shell: "/bin/sh".into(),
            ..Default::default()
        },
        egui::Context::default(),
        Some(backend),
        None,
    );
    let resize = |cols| {
        assert!(tab.send(Command::Resize(PtySize {
            rows: 24,
            cols,
            ..FALLBACK_SIZE
        })));
        wait_for(&tab.screen, |s| s.parser.screen().size() == (24, cols));
    };
    resize(80);
    let line = "REFLOW:abcdefghijklmnopqrstuvwxyz0123456789:END";
    assert!(tab.send(Command::Input(
        format!("stty -echo; printf '\\033[2J\\033[H{line}\\r\\n'\n").into_bytes()
    )));
    wait_for(&tab.screen, |s| s.parser.screen().contents().contains(line));
    for cols in [20, 80, 30, 80] {
        resize(cols);
        let state = tab.screen.lock().unwrap();
        assert!(state.parser.screen().contents().contains(line));
        if cols == 80 {
            assert_eq!(state.parser.screen().cell(0, 46).unwrap().contents(), "D");
        }
    }
    resize(20);
    assert!(tab.send(Command::Input(
        format!("printf '{line}\\r\\n'\n").into_bytes()
    )));
    wait_for(&tab.screen, |s| {
        s.parser.screen().contents().matches(line).count() == 2
    });
    resize(80);
    assert_eq!(
        tab.screen
            .lock()
            .unwrap()
            .parser
            .screen()
            .contents()
            .matches(line)
            .count(),
        2
    );
    Ok(())
}

#[test]
#[ignore = "requires GLIMCODE_ZMX_PATH and permission to create local Unix sockets"]
fn zmx_restores_fullscreen_input_unicode_and_shell_across_disconnect() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let backend = tmux::Persistent::new_with_backend(None, "zmx")
        .context("Install zmx 0.8.1 or set GLIMCODE_ZMX_PATH")?;
    assert!(backend.is_zmx());
    let name = backend.name.as_str();
    let executable = backend.executable().to_path_buf();
    let ctx = egui::Context::default();
    let command = |args: &[&str]| {
        let mut command = zmx::process(&executable);
        command.args(args);
        command
    };
    struct Cleanup(std::process::Command);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.output();
        }
    }
    let _cleanup = Cleanup(command(&["kill", name, "--force"]));
    let settings = Settings {
        shell: "/bin/sh".into(),
        terminal_backend: "zmx".into(),
        ..Default::default()
    };
    let spawn = |id| {
        let mut settings = settings.clone();
        if id > 1 {
            settings.shell = "/does/not/exist".into();
        }
        let tab = Session::spawn_with(
            id,
            dir.path().into(),
            settings,
            ctx.clone(),
            Some(backend.clone()),
            None,
        );
        assert!(tab.send(Command::Resize(PtySize {
            rows: 24,
            cols: 80,
            ..FALLBACK_SIZE
        })));
        tab
    };
    let tab = spawn(1);
    let tree = "│   ├── 日本語-é-😀";
    assert!(tab.send(Command::Input(format!(
        "export GLIMCODE_ZMX_KEEP=kept; printf 'PID:%s:READY\\n' \"$$\"; i=0; while [ $i -lt 120 ]; do printf '{tree}-%s\\n' \"$i\"; i=$((i+1)); done; printf 'TREE_DONE\\n'\n"
    ).into_bytes())));
    wait_for(&tab.screen, |s| {
        s.parser.screen().contents().contains("TREE_DONE")
            && s.parser
                .screen()
                .contents()
                .contains(&format!("{tree}-119"))
    });
    let history = command(&["history", name]).output()?;
    assert!(
        history.status.success(),
        "{}",
        String::from_utf8_lossy(&history.stderr)
    );
    let history = String::from_utf8(history.stdout)?;
    let pid = regex::Regex::new(r"(?m)^PID:(\d+):READY$")?
        .captures(&history)
        .context("interactive shell PID must be retained")?[1]
        .to_owned();
    assert!(
        history.contains(&format!("{tree}-0")),
        "Unicode scrollback must be retained"
    );

    let report = dir.path().join("input.bin");
    let mouse = b"\x1b[<64;3;4M\x1b[<65;3;4M\x1b[<0;3;4M\x1b[<0;3;4m";
    let input = format!(
        "stty raw -echo; printf '\\033[?1049h\\033[?1002h\\033[?1006h\\033[?2004h\\033[2J\\033[HFULLSCREEN_READY'; dd bs=1 count={} of={} 2>/dev/null; printf '\\033[?1002l\\033[?1006l\\033[?2004l\\033[?1049l'; stty sane\n",
        mouse.len(),
        shell_words::quote(&report.to_string_lossy())
    );
    assert!(tab.send(Command::Input(input.into_bytes())));
    wait_for(&tab.screen, |s| {
        s.parser.screen().alternate_screen()
            && s.parser.screen().contents().contains("FULLSCREEN_READY")
            && s.parser.screen().mouse_protocol_mode() == vt100::MouseProtocolMode::ButtonMotion
    });
    let screen = tab.screen.clone();
    drop(tab);
    wait_for(&screen, |s| s.exited);

    let inventory = zmx::list(&executable)?;
    let live = inventory
        .iter()
        .find(|session| session.name == name)
        .context("detached zmx session must remain listed")?;
    assert_eq!(live.clients, 0);
    let tab = spawn(2);
    wait_for(&tab.screen, |s| {
        s.parser.screen().alternate_screen()
            && s.parser.screen().contents().contains("FULLSCREEN_READY")
    });
    {
        let state = tab.screen.lock().unwrap();
        assert_eq!(
            state.parser.screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::ButtonMotion,
            "reattach must restore application mouse tracking"
        );
        assert_eq!(
            state.parser.screen().mouse_protocol_encoding(),
            vt100::MouseProtocolEncoding::Sgr
        );
        assert!(state.parser.screen().bracketed_paste());
    }
    assert!(tab.send(Command::Resize(PtySize {
        rows: 31,
        cols: 101,
        ..FALLBACK_SIZE
    })));
    assert!(tab.send(Command::Input(mouse.to_vec())));
    wait_for(&tab.screen, |s| !s.parser.screen().alternate_screen());
    assert_eq!(
        std::fs::read(&report)?,
        mouse,
        "wheel and click bytes must reach the app unchanged"
    );
    assert!(tab.send(Command::Input(
        b"printf 'RESTORED:%s:%s:END\\n' \"$GLIMCODE_ZMX_KEEP\" \"$$\"; stty size\n".to_vec()
    )));
    wait_for(&tab.screen, |s| {
        s.parser
            .screen()
            .contents()
            .contains(&format!("RESTORED:kept:{pid}:END"))
            && s.parser.screen().contents().contains("31 101")
    });
    drop(tab);
    Ok(())
}

#[test]
#[ignore = "requires GLIMCODE_ZMX_PATH and permission to create local Unix sockets"]
fn zmx_close_during_startup_does_not_leave_a_daemon() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let ctx = egui::Context::default();
    for send_size in [false, true] {
        let mut terminal = Terminal::default();
        terminal.settings.terminal_backend = "zmx".into();
        terminal.settings.shell = "/bin/sh".into();
        terminal.new_tab(dir.path(), &ctx);
        let backend = terminal.tabs[0]
            .persistent
            .clone()
            .context("zmx must be installed")?;
        let screen = terminal.tabs[0].screen.clone();
        if send_size {
            terminal.tabs[0].send(Command::Resize(FALLBACK_SIZE));
        }
        terminal.close(0);
        wait_for(&screen, |s| s.exited);
        let deadline = Instant::now() + Duration::from_secs(3);
        while backend.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!backend.exists());
    }
    Ok(())
}
