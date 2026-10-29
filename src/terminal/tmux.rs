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
    let executable = executable().context("tmux is not installed")?;
    list_sessions_with(&executable)
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
const CONFIG: &str = "\
set -g status off
set -g prefix None
set -g prefix2 None
unbind-key -q -a -T prefix
unbind-key -q -a -T root
bind-key -T root WheelUpPane send-keys -M
bind-key -T root WheelDownPane send-keys -M
set -g mouse off
set -g default-shell /bin/sh
set -g default-terminal xterm-256color
set -g history-limit 5000
set -g destroy-unattached off
set -g set-titles on
set -g set-titles-string '#{pane_title}'
set -g focus-events on
set -s escape-time 0
set -s extended-keys on
set -as terminal-overrides ',xterm-256color:smcup@:rmcup@:XT:RGB'
";

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
    fn restored_session_configuration_accepts_missing_tables_and_literal_targets() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let executable = dir.path().join("tmux");
        // Model tmux's distinct session-option and pane target lookup rules,
        // with key tables already removed by startup or a previous reconnect.
        // No socket is needed, so this also covers reconnect in the sandbox.
        std::fs::write(
            &executable,
            r#"#!/bin/sh
shift 3
if [ "$1" = has-session ]; then exit 0; fi
wheels=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        set-option)
            if [ "$3" != glimcode-test ]; then
                echo "no such session: $3" >&2
                exit 1
            fi
            shift 5 ;;
        unbind-key)
            if [ "$2" != -q ]; then
                echo "table prefix doesn't exist yet" >&2
                exit 1
            fi
            shift 5 ;;
        bind-key)
            [ "$2" = -T ] && [ "$3" = root ] && [ "$5" = send-keys ] && [ "$6" = -M ] || exit 4
            case "$4" in WheelUpPane|WheelDownPane) ;; *) exit 5 ;; esac
            wheels=$((wheels + 1))
            shift 6 ;;
        if-shell)
            [ "$3" = '=glimcode-test:' ] || exit 2
            shift 6 ;;
        ';') shift ;;
        *) exit 3 ;;
    esac
done
[ "$wheels" -eq 2 ] || exit 6
"#,
        )?;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700))?;
        let backend = Persistent {
            name: "glimcode-test".into(),
            executable,
            closed: Arc::new(AtomicBool::new(false)),
            lifecycle: Arc::new(Mutex::new(())),
        };
        let settings = Settings {
            shell: "/does/not/exist".into(),
            ..Settings::default()
        };
        backend.attach(dir.path(), &settings, FALLBACK_SIZE)?;
        backend.attach(dir.path(), &settings, FALLBACK_SIZE)?;
        assert_eq!(backend.name, "glimcode-test");
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
    pub fn new(name: Option<String>) -> Option<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let executable = match executable() {
            Some(path) => path,
            None if name.as_ref().is_some_and(|name| valid_name(name)) => PathBuf::from("tmux"),
            None => return None,
        };
        Some(Self {
            executable,
            name: name.filter(|name| valid_name(name)).unwrap_or_else(|| {
                format!(
                    "glimcode-{:x}-{:x}-{:x}",
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
    pub fn executable(&self) -> &Path {
        &self.executable
    }
    fn process(&self) -> Process {
        server_process(&self.executable)
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
        self.run(&["has-session", "-t", &format!("={}", self.name)])
            .is_ok()
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
            let mut config = tempfile::NamedTempFile::new()?;
            config.write_all(CONFIG.as_bytes())?;
            config.flush()?;
            let output = self
                .process()
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
        // -f is read only when the server first starts. Repair restored sessions
        // too: tmux's root mouse bindings can enter copy-mode and copy into its
        // private buffer on mouse-up, bypassing Glimcode's selection/clipboard.
        // Keep wheel forwarding: tmux dispatches application mouse events
        // through root bindings too, even with its own mouse controls off.
        // These bindings only pass events to the program, never enter copy-mode.
        // Session options use a literal session name. Unlike has-session,
        // their lookup does not reliably accept the '=' exact-match prefix.
        // Pane commands need the ':' separator to disambiguate the session
        // component from a window/pane name.
        let target = self.name.as_str();
        let pane_target = format!("={}:", self.name);
        self.run(&[
            "set-option",
            "-t",
            target,
            "mouse",
            "off",
            ";",
            "set-option",
            "-t",
            target,
            "prefix",
            "None",
            ";",
            "set-option",
            "-t",
            target,
            "prefix2",
            "None",
            ";",
            "set-option",
            "-t",
            target,
            "status",
            "off",
            ";",
            "unbind-key",
            "-q",
            "-a",
            "-T",
            "prefix",
            ";",
            "unbind-key",
            "-q",
            "-a",
            "-T",
            "root",
            ";",
            "bind-key",
            "-T",
            "root",
            "WheelUpPane",
            "send-keys",
            "-M",
            ";",
            "bind-key",
            "-T",
            "root",
            "WheelDownPane",
            "send-keys",
            "-M",
            ";",
            "if-shell",
            "-t",
            &pane_target,
            "-F",
            "#{==:#{pane_mode},copy-mode}",
            &format!(
                "send-keys -t {} -X cancel",
                shell_words::quote(&pane_target)
            ),
        ])?;
        let mut command = CommandBuilder::new(&self.executable);
        command.args([
            "-L",
            SERVER,
            "-u",
            "attach-session",
            "-t",
            &format!("={}", self.name),
        ]);
        command.cwd(root);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        command.env_remove("TMUX");
        command.env_remove("TMUX_PANE");
        Ok(command)
    }
    pub fn directory(&self) -> Option<PathBuf> {
        let output = self
            .run(&[
                "display-message",
                "-p",
                "-t",
                &format!("={}", self.name),
                "#{pane_current_path}",
            ])
            .ok()?;
        let path = PathBuf::from(
            String::from_utf8(output.stdout)
                .ok()?
                .trim_end_matches('\n'),
        );
        path.is_absolute().then_some(path)
    }
    pub fn history(&self) -> Vec<u8> {
        self.run(&[
            "capture-pane",
            "-p",
            "-e",
            "-t",
            &format!("={}", self.name),
            "-S",
            "-5000",
            "-E",
            "-1",
        ])
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .replace('\n', "\r\n")
                .into_bytes()
        })
        .unwrap_or_default()
    }
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        let session = self.clone();
        std::thread::spawn(move || {
            // Serialize with creation so closing during startup cannot leak a shell.
            let _guard = session.lifecycle.lock().unwrap();
            let _ = session.run(&["kill-session", "-t", &format!("={}", session.name)]);
        });
    }
}
