//! Persistent sessions on a private tmux server; editor PTYs own only clients.
use super::*;
use std::process::{Command as Process, Output};

const SERVER: &str = "glimcode";
const CONFIG: &str = "\
set -g status off
set -g prefix None
set -g prefix2 None
unbind-key -a
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
        let mut command = Process::new(&self.executable);
        command
            .args(["-L", SERVER, "-u"])
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        command
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
