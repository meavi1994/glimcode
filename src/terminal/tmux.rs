//! Persistent sessions on a private tmux server; editor PTYs own only clients.
use super::*;
use std::process::{Command as Process, Output};

const SERVER: &str = "glimcode";
const SESSION_FORMAT: &str =
    "#{session_id}\t#{session_name}\t#{session_attached}\t#{session_windows}\t#{pane_current_path}";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SessionInfo {
    pub id: String,
    pub name: String,
    pub clients: usize,
    pub windows: usize,
    pub directory: String,
}

fn parse_sessions(output: &str) -> Result<Vec<SessionInfo>> {
    let mut sessions = Vec::new();
    for line in output.lines() {
        let fields: Vec<_> = line.splitn(5, '\t').collect();
        if fields.len() != 5 {
            bail!("Could not read tmux session details");
        }
        if !valid_name(fields[1]) {
            continue;
        }
        if !valid_session_id(fields[0]) {
            bail!("Invalid tmux session ID");
        }
        sessions.push(SessionInfo {
            id: fields[0].into(),
            name: fields[1].into(),
            clients: fields[2].parse()?,
            windows: fields[3].parse()?,
            directory: fields[4].into(),
        });
    }
    sessions.sort_by(|a, b| (a.clients != 0, &a.name).cmp(&(b.clients != 0, &b.name)));
    Ok(sessions)
}

fn valid_session_id(id: &str) -> bool {
    id.strip_prefix('$')
        .is_some_and(|number| !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()))
}

pub(super) fn list_sessions() -> Result<Vec<SessionInfo>> {
    let mut sessions = if let Some(executable) = executable() {
        list_sessions_with(&executable)?
    } else {
        Vec::new()
    };
    if let Some(executable) = zmx::executable() {
        sessions.extend(zmx::list(&executable)?);
    }
    sessions.sort_by(|a, b| (a.clients != 0, &a.name).cmp(&(b.clients != 0, &b.name)));
    Ok(sessions)
}

fn server_process(executable: &Path) -> Process {
    let mut command = Process::new(executable);
    command
        .args(["-L", SERVER, "-u"])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    command
}

fn list_sessions_with(executable: &Path) -> Result<Vec<SessionInfo>> {
    let output = server_process(executable)
        .args(["list-sessions", "-F", SESSION_FORMAT])
        .output()?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.contains("no server running") || error.contains("No such file or directory") {
            return Ok(Vec::new());
        }
        bail!("tmux: {}", error.trim());
    }
    parse_sessions(&String::from_utf8(output.stdout)?)
}

pub(super) fn kill_session(session: &SessionInfo) -> Result<()> {
    if session.name.starts_with(zmx::PREFIX) {
        return zmx::kill(&zmx::executable().context("zmx is not installed")?, session);
    }
    kill_session_with(&executable().context("tmux is not installed")?, session)
}

fn kill_session_with(executable: &Path, session: &SessionInfo) -> Result<()> {
    if !valid_name(&session.name) || !valid_session_id(&session.id) {
        bail!("Invalid Glimcode session");
    }
    // A refreshed list may differ from the confirmation. Never kill a new
    // session merely because it reused the old name (or a restarted server's ID).
    if !list_sessions_with(executable)?
        .iter()
        .any(|s| s.id == session.id && s.name == session.name)
    {
        return Ok(());
    }
    let output = server_process(executable)
        .args(["kill-session", "-t", &session.id])
        .output()?;
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if !error.contains("can't find session")
            && !error.contains("no such session")
            && !error.contains("no server running")
        {
            bail!("tmux: {}", error.trim());
        }
    }
    Ok(())
}
#[derive(Clone)]
pub(super) struct Persistent {
    pub name: String,
    executable: PathBuf,
    closed: Arc<AtomicBool>,
    lifecycle: Arc<Mutex<()>>,
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    #[ignore = "requires tmux and permission to create local Unix sockets; run outside a sandbox"]
    fn cleared_prompt_redraws_do_not_enter_tmux_history() -> Result<()> {
        struct Cleanup(Persistent);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.close();
            }
        }
        for (scroll_on_clear, expected_history) in [(true, 2), (false, 0)] {
            let backend = Persistent::new(None).context("tmux is not installed")?;
            let _cleanup = Cleanup(backend.clone());
            let dir = tempfile::tempdir()?;
            let mut settings = Settings {
                shell: shell_words::join([
                    "/bin/sh",
                    "-c",
                    r"printf '\033[Hinstant prompt\033[H\033[Jinitial prompt\033[H\033[Jready prompt'; exec sleep 30",
                ]),
                ..Default::default()
            };
            if scroll_on_clear {
                settings.tmux_options.push(crate::preferences::tmux::Entry {
                    enabled: true,
                    command: "set -g scroll-on-clear on".into(),
                });
            }
            backend.attach(dir.path(), &settings, FALLBACK_SIZE)?;
            let target = format!("={}:", backend.name);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let output = backend.run(&["capture-pane", "-p", "-t", &target])?;
                if String::from_utf8_lossy(&output.stdout).contains("ready prompt") {
                    break;
                }
                anyhow::ensure!(Instant::now() < deadline, "prompt did not render");
                std::thread::sleep(Duration::from_millis(10));
            }
            let output = backend.run(&["display-message", "-p", "-t", &target, "#{history_size}"])?;
            assert_eq!(
                String::from_utf8(output.stdout)?.trim(),
                expected_history.to_string()
            );
        }
        Ok(())
    }

    #[test]
    fn pane_status_reads_activity_and_directory_from_the_target_session() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let executable = dir.path().join("tmux");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
[ "$1" = -L ] && [ "$2" = glimcode ] && [ "$3" = -u ] || exit 1
shift 3
[ "$1" = display-message ] && [ "$2" = -p ] && [ "$3" = -t ] && [ "$4" = '=glimcode-test:' ] || exit 2
printf '12345\t1\t/project with spaces\tand tab\n'
"#,
        )?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        let backend = Persistent {
            name: "glimcode-test".into(),
            executable,
            closed: Arc::new(AtomicBool::new(false)),
            lifecycle: Arc::new(Mutex::new(())),
        };
        assert_eq!(
            backend.pane_status(),
            Some((
                PathBuf::from("/project with spaces\tand tab"),
                Some(12345),
                true
            ))
        );
        Ok(())
    }

    #[test]
    fn reconnect_history_waits_for_our_client_and_reports_capture_failures() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let executable = dir.path().join("tmux");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
shift 3
case "$1" in
list-clients)
    marker="$(dirname "$0")/attached"
    if [ -f "$marker" ]; then printf '123\n'; else touch "$marker"; fi ;;
capture-pane)
    [ "$5" = '=glimcode-history:' ] || { echo "invalid pane target" >&2; exit 1; }
    [ -f "$(dirname "$0")/attached" ] || exit 2
    [ ! -f "$(dirname "$0")/fail" ] || { echo "capture failed" >&2; exit 3; }
    i=0; while [ "$i" -lt 50 ]; do printf 'restored line %s\n' "$i"; i=$((i + 1)); done ;;
*) exit 4 ;;
esac
"#,
        )?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        let backend = Persistent {
            name: "glimcode-history".into(),
            executable,
            closed: Arc::new(AtomicBool::new(false)),
            lifecycle: Arc::new(Mutex::new(())),
        };
        let history = backend.history(Some(123))?;
        assert_eq!(String::from_utf8(history)?.lines().count(), 50);
        std::fs::write(dir.path().join("fail"), "")?;
        assert!(
            backend
                .history(Some(123))
                .unwrap_err()
                .to_string()
                .contains("capture failed"),
            "history errors must not masquerade as an empty scrollback"
        );
        Ok(())
    }

    #[test]
    fn session_inventory_marks_detached_sorts_first_and_ignores_foreign_names() -> Result<()> {
        let sessions = parse_sessions(
            "$1\tglimcode-attached\t2\t1\t/project one\n$2\tpersonal\t0\t1\t/private\n$3\tglimcode-orphan\t0\t2\t/project\twith tab\n",
        )?;
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].name, "glimcode-orphan");
        assert_eq!(sessions[0].clients, 0);
        assert_eq!(sessions[0].directory, "/project\twith tab");
        assert_eq!(sessions[1].clients, 2);
        assert!(parse_sessions("$1\tglimcode-bad\tunknown\t1\t/project").is_err());
        assert!(parse_sessions("bad\tglimcode-bad\t0\t1\t/project").is_err());
        assert!(parse_sessions("")?.is_empty());
        Ok(())
    }

    #[test]
    fn session_termination_checks_identity_and_targets_only_the_private_server() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let executable = dir.path().join("tmux");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
[ "$1" = -L ] && [ "$2" = glimcode ] && [ "$3" = -u ] || exit 9
shift 3
case "$1" in
list-sessions) printf '$7\tglimcode-running\t0\t1\t/project\n' ;;
kill-session) [ "$2" = -t ] && [ "$3" = '$7' ] || exit 8
    touch "$(dirname "$0")/killed" ;;
*) exit 7 ;;
esac
"#,
        )?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        let mut session = list_sessions_with(&executable)?.remove(0);
        session.id = "$6".into();
        kill_session_with(&executable, &session)?;
        assert!(
            !dir.path().join("killed").exists(),
            "a reused name must not kill another session"
        );
        session.id = "$7".into();
        session.name = "glimcode-old".into();
        kill_session_with(&executable, &session)?;
        assert!(
            !dir.path().join("killed").exists(),
            "a reused ID must not kill another session"
        );
        session.name = "glimcode-running".into();
        kill_session_with(&executable, &session)?;
        assert!(dir.path().join("killed").is_file());
        session.id = "$7; kill-server".into();
        assert!(kill_session_with(&executable, &session).is_err());
        Ok(())
    }

    #[test]
    fn session_inventory_distinguishes_an_empty_server_from_access_errors() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let executable = dir.path().join("tmux");
        for (message, empty) in [
            ("no server running on /tmp/example", true),
            ("error connecting: No such file or directory", true),
            ("error connecting: Operation not permitted", false),
        ] {
            std::fs::write(
                &executable,
                format!("#!/bin/sh\necho '{}' >&2\nexit 1\n", message),
            )?;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
            let result = list_sessions_with(&executable);
            if empty {
                assert!(result?.is_empty());
            } else {
                assert!(result.is_err());
            }
        }
        Ok(())
    }

    #[test]
    fn restored_sessions_apply_the_editable_config_without_forced_overrides() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let executable = dir.path().join("tmux");
        std::fs::write(
            &executable,
            r#"#!/bin/sh
shift 3
case "$1" in
has-session) exit 0 ;;
source-file)
    [ "$2" = -t ] && [ "$3" = '=glimcode-test:' ] || exit 1
    cat "$4" > "$(dirname "$0")/applied" ;;
*) exit 2 ;;
esac
"#,
        )?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        let backend = Persistent {
            name: "glimcode-test".into(),
            executable,
            closed: Arc::new(AtomicBool::new(false)),
            lifecycle: Arc::new(Mutex::new(())),
        };
        let mut settings = Settings {
            shell: "/does/not/exist".into(),
            ..Default::default()
        };
        backend.attach(dir.path(), &settings, FALLBACK_SIZE)?;
        backend.attach(dir.path(), &settings, FALLBACK_SIZE)?;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("applied"))?,
            settings.tmux_config()?
        );
        settings.tmux_options = vec![
            crate::preferences::tmux::Entry {
                enabled: true,
                command: "set -g mouse on".into(),
            },
            crate::preferences::tmux::Entry {
                enabled: false,
                command: "unbind-key -q -a -T root".into(),
            },
        ];
        backend.attach(dir.path(), &settings, FALLBACK_SIZE)?;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("applied"))?,
            "set -g mouse on\n"
        );
        Ok(())
    }
}

pub(super) fn executable() -> Option<PathBuf> {
    find_executable("tmux")
}

pub(super) fn find_executable(name: &str) -> Option<PathBuf> {
    if !cfg!(unix) {
        return None;
    }
    let paths = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&paths)
        .chain(std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/bin")))
        .chain(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"].map(PathBuf::from))
        .map(|path| path.join(name))
        .find(|path| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                path.metadata()
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                path.is_file()
            }
        })
}

pub(super) fn valid_name(name: &str) -> bool {
    name.starts_with("glimcode-")
        && name.len() <= 100
        && name.len() > 9
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

impl Persistent {
    #[cfg(test)]
    pub fn new(name: Option<String>) -> Option<Self> {
        Self::new_with_backend(name, "tmux")
    }
    pub fn new_with_backend(name: Option<String>, backend: &str) -> Option<Self> {
        let use_zmx = name
            .as_ref()
            .is_some_and(|name| name.starts_with(zmx::PREFIX))
            || (name.is_none() && backend == "zmx");
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let executable = match if use_zmx {
            zmx::executable()
        } else {
            executable()
        } {
            Some(path) => path,
            None if name.as_ref().is_some_and(|name| valid_name(name)) => {
                PathBuf::from(if use_zmx { "zmx" } else { "tmux" })
            }
            None => return None,
        };
        Some(Self {
            executable,
            name: name.filter(|name| valid_name(name)).unwrap_or_else(|| {
                format!(
                    "{}{:x}-{:x}-{:x}",
                    if use_zmx { zmx::PREFIX } else { "glimcode-" },
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos(),
                    NEXT.fetch_add(1, Ordering::Relaxed),
                )
            }),
            closed: Arc::new(AtomicBool::new(false)),
            lifecycle: Arc::new(Mutex::new(())),
        })
    }
    pub fn is_zmx(&self) -> bool {
        self.name.starts_with(zmx::PREFIX)
    }
    pub fn kind(&self) -> &str {
        if self.is_zmx() { "zmx" } else { "tmux" }
    }
    pub fn attach_command(&self) -> String {
        if self.is_zmx() {
            format!(
                "env -u ZMX_SESSION -u ZMX_SESSION_PREFIX ZMX_DIR={} {} attach {}",
                shell_words::quote(&zmx::directory().to_string_lossy()),
                shell_words::quote(&self.executable.to_string_lossy()),
                self.name
            )
        } else {
            format!("tmux -L glimcode attach-session -t {}", self.name)
        }
    }
    pub fn executable(&self) -> &Path {
        &self.executable
    }
    fn process(&self) -> Process {
        server_process(&self.executable)
    }
    fn configured_process(&self, settings: &Settings) -> Result<Process> {
        let mut command = Process::new(&self.executable);
        command
            .args(["-L", SERVER])
            .args(settings.tmux_arguments()?)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        Ok(command)
    }
    fn run(&self, args: &[&str]) -> Result<Output> {
        let output = self
            .process()
            .args(args)
            .output()
            .context("Could not run tmux")?;
        if !output.status.success() {
            bail!("tmux: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(output)
    }
    #[cfg(test)]
    pub fn run_for_test(&self, args: &[&str]) -> Result<Output> {
        self.run(args)
    }
    pub fn exists(&self) -> bool {
        if self.is_zmx() {
            return zmx::list(&self.executable)
                .is_ok_and(|sessions| sessions.iter().any(|session| session.name == self.name));
        }
        self.run(&["has-session", "-t", &format!("={}", self.name)])
            .is_ok()
    }
    pub fn start(
        &self,
        root: &Path,
        settings: &Settings,
        size: PtySize,
        slave: &dyn portable_pty::SlavePty,
    ) -> Result<ManagedChild> {
        if !self.is_zmx() {
            return Ok(ManagedChild(
                slave.spawn_command(self.attach(root, settings, size)?)?,
            ));
        }
        // Unlike tmux, zmx creates the daemon when the client starts. Keep
        // close() serialized until that daemon exists, or a tab closed during
        // startup could leave an orphan behind.
        let _guard = self.lifecycle.lock().unwrap();
        if self.closed.load(Ordering::Relaxed) {
            bail!("Terminal session was closed");
        }
        let mut child = ManagedChild(slave.spawn_command(zmx::attach(self, root, settings)?)?);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.exists() {
            if let Some(status) = child.0.try_wait()? {
                bail!("zmx client exited during startup: {status}");
            }
            if Instant::now() >= deadline {
                bail!("Timed out waiting for zmx to start");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(child)
    }
    pub fn attach(
        &self,
        root: &Path,
        settings: &Settings,
        size: PtySize,
    ) -> Result<CommandBuilder> {
        let _guard = self.lifecycle.lock().unwrap();
        if self.closed.load(Ordering::Relaxed) {
            bail!("Terminal session was closed");
        }
        if self.is_zmx() {
            return zmx::attach(self, root, settings);
        }
        let mut config = tempfile::NamedTempFile::new()?;
        config.write_all(settings.tmux_config()?.as_bytes())?;
        config.flush()?;
        if !self.exists() {
            // Validate the configured shell only when creating a session. Reconnecting
            // must also work after the user's shell preferences have changed.
            settings.command(root)?;
            let mut shell = shell_args(&settings.shell)?;
            if shell.len() == 1
                && matches!(
                    shell_name(&settings.shell).as_str(),
                    "bash" | "zsh" | "fish"
                )
            {
                shell.push("-l".into());
            }
            // tmux treats a single command argument as shell code. Quote every
            // argument, including a lone executable, so paths and metacharacters
            // retain the same meaning as the direct PTY backend.
            // The outer editor PTY's termios does not configure tmux's pane PTY.
            // Disable flow control there too so Ctrl+S/Ctrl+Q reach the program.
            let shell = format!("stty -ixon; exec {}", shell_words::join(shell));
            let output = self
                .configured_process(settings)?
                .arg("-f")
                .arg(config.path())
                .args([
                    "new-session",
                    "-A",
                    "-d",
                    "-s",
                    &self.name,
                    "-x",
                    &size.cols.to_string(),
                    "-y",
                    &size.rows.to_string(),
                    "-c",
                ])
                .arg(root)
                .args([
                    "-e",
                    "TERM_PROGRAM=Glimcode",
                    "-e",
                    "COLORTERM=truecolor",
                    "--",
                    &shell,
                ])
                .output()
                .context(
                    "Could not start tmux. Install tmux and reopen the project to reconnect",
                )?;
            if !output.status.success() {
                bail!(
                    "Could not create tmux session: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        // The user owns the entire configuration. Reapply it on reconnect,
        // with an explicit pane context for commands and format expansion.
        let pane_target = format!("={}:", self.name);
        let output = self
            .configured_process(settings)?
            .args([
                "source-file",
                "-t",
                &pane_target,
                config
                    .path()
                    .to_str()
                    .context("tmux configuration path is not UTF-8")?,
            ])
            .output()
            .context("Could not apply tmux configuration")?;
        if !output.status.success() {
            bail!(
                "tmux configuration: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let mut command = CommandBuilder::new(&self.executable);
        command.args(["-L", SERVER]);
        command.args(settings.tmux_arguments()?);
        command.args(["attach-session", "-t", &format!("={}", self.name)]);
        command.cwd(root);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env_remove("TMUX");
        command.env_remove("TMUX_PANE");
        Ok(command)
    }
    pub fn pane_status(&self) -> Option<(PathBuf, Option<u64>, bool)> {
        if self.is_zmx() {
            let session = zmx::list(&self.executable)
                .ok()?
                .into_iter()
                .find(|s| s.name == self.name)?;
            let directory = PathBuf::from(session.directory);
            return directory.is_absolute().then_some((directory, None, false));
        }
        let output = self
            .run(&[
                "display-message",
                "-p",
                "-t",
                &format!("={}:", self.name),
                "#{window_activity}\t#{mouse_any_flag}\t#{pane_current_path}",
            ])
            .ok()?;
        let output = String::from_utf8(output.stdout).ok()?;
        let mut fields = output.trim_end_matches('\n').splitn(3, '\t');
        let activity = fields.next()?;
        let mouse = fields.next()? == "1";
        let path = fields.next()?;
        let path = PathBuf::from(path);
        path.is_absolute()
            .then_some((path, activity.parse().ok(), mouse))
    }
    pub fn mouse_reporting(&self) -> bool {
        !self.is_zmx()
            && self
                .run(&[
                    "display-message",
                    "-p",
                    "-t",
                    &format!("={}:", self.name),
                    "#{mouse_any_flag}",
                ])
                .is_ok_and(|output| output.stdout.trim_ascii() == b"1")
    }
    #[cfg(test)]
    pub fn directory(&self) -> Option<PathBuf> {
        self.pane_status().map(|(path, _, _)| path)
    }
    pub fn history(&self, client_pid: Option<u32>) -> Result<Vec<u8>> {
        if self.is_zmx() {
            return Ok(Vec::new());
        } // zmx restores its own snapshot.
        // Attaching can shrink/reflow an old pane and move visible rows into
        // tmux history. Capturing before our client attaches misses those rows.
        if let Some(pid) = client_pid {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let clients = self.run(&[
                    "list-clients",
                    "-t",
                    &format!("={}", self.name),
                    "-F",
                    "#{client_pid}",
                ])?;
                if String::from_utf8_lossy(&clients.stdout)
                    .lines()
                    .any(|line| line.trim().parse::<u32>() == Ok(pid))
                {
                    break;
                }
                if Instant::now() >= deadline {
                    bail!("Timed out waiting for the tmux client to attach");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let output = self.run(&[
            "capture-pane",
            "-p",
            "-e",
            "-t",
            &format!("={}:", self.name),
            "-S",
            "-5000",
            "-E",
            "-1",
        ])?;
        Ok(String::from_utf8(output.stdout)?
            .replace('\n', "\r\n")
            .into_bytes())
    }
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let session = self.clone();
        std::thread::spawn(move || {
            // Serialize with creation so closing during startup cannot leak a shell.
            let _guard = session.lifecycle.lock().unwrap();
            if session.is_zmx() {
                let _ = zmx::run(&session.executable, &["kill", &session.name, "--force"]);
            } else {
                let _ = session.run(&["kill-session", "-t", &format!("={}", session.name)]);
            }
        });
    }
}
