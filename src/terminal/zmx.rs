//! Optional zmx session persistence. Live bytes and reattach snapshots are
//! delivered by zmx itself, without tmux configuration or history replay.
use super::*;
use std::process::{Command as Process, Output};

pub(super) const PREFIX: &str = "glimcode-zmx-";

pub(super) fn executable() -> Option<PathBuf> {
    std::env::var_os("GLIMCODE_ZMX_PATH")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| tmux::find_executable("zmx"))
}
pub(super) fn directory() -> PathBuf {
    std::env::var_os("GLIMCODE_ZMX_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            crate::preferences::path()
                .and_then(|p| p.parent().map(|p| p.join("zmx")))
                .unwrap_or_else(|| std::env::temp_dir().join("glimcode-zmx"))
        })
}
pub(super) fn process(executable: &Path) -> Process {
    let mut command = Process::new(executable);
    command
        .env("ZMX_DIR", directory())
        .env("ZMX_NO_DETACH_KEY", "1")
        .env_remove("ZMX_SESSION")
        .env_remove("ZMX_SESSION_PREFIX");
    command
}
pub(super) fn run(executable: &Path, args: &[&str]) -> Result<Output> {
    let output = process(executable)
        .args(args)
        .output()
        .context("Could not run zmx")?;
    if !output.status.success() {
        bail!("zmx: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output)
}
pub(super) fn list(executable: &Path) -> Result<Vec<tmux::SessionInfo>> {
    let output = run(executable, &["list"])?;
    parse_sessions(&String::from_utf8(output.stdout)?)
}
fn parse_sessions(output: &str) -> Result<Vec<tmux::SessionInfo>> {
    let mut sessions = Vec::new();
    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let fields: std::collections::HashMap<_, _> = line
            .trim_start()
            .split('\t')
            .filter_map(|field| field.split_once('='))
            .collect();
        let name = fields
            .get("name")
            .copied()
            .context("Missing zmx session name")?;
        if !name.starts_with(PREFIX) || !tmux::valid_name(name) {
            continue;
        }
        let pid = fields
            .get("pid")
            .context("Missing zmx PID")?
            .parse::<u32>()?;
        let created = fields
            .get("created")
            .context("Missing zmx creation time")?
            .parse::<u64>()?;
        let clients = fields
            .get("clients")
            .context("Missing zmx client count")?
            .parse()?;
        let directory = fields
            .get("cwd")
            .or_else(|| fields.get("start_dir"))
            .copied()
            .unwrap_or("");
        let directory = url::Url::parse(directory)
            .ok()
            .filter(|url| url.scheme() == "file")
            .and_then(|url| {
                // OSC 7 includes the local machine's hostname. Rebuild a
                // local file URL before converting its percent-encoded path.
                url::Url::parse(&format!("file://{}", url.path()))
                    .ok()?
                    .to_file_path()
                    .ok()
            })
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| directory.into());
        sessions.push(tmux::SessionInfo {
            id: format!("zmx:{pid}:{created}"),
            name: name.into(),
            clients,
            windows: 1,
            directory,
        });
    }
    Ok(sessions)
}
pub(super) fn attach(
    session: &tmux::Persistent,
    root: &Path,
    settings: &Settings,
) -> Result<CommandBuilder> {
    let version = run(session.executable(), &["version"])?;
    if !String::from_utf8_lossy(&version.stdout)
        .lines()
        .next()
        .is_some_and(|line| line.split_whitespace().eq(["zmx", "0.8.1"]))
    {
        bail!(
            "The experimental zmx backend requires zmx 0.8.1; keep existing session daemons on the same version"
        );
    }
    let mut command = if session.exists() {
        CommandBuilder::new(session.executable())
    } else {
        let mut shell = settings.command(root)?;
        let mut argv = vec![
            session.executable().as_os_str().to_owned(),
            "attach".into(),
            session.name.clone().into(),
        ];
        argv.extend(shell.get_argv().iter().cloned());
        *shell.get_argv_mut() = argv;
        shell
    };
    if command.get_argv().len() == 1 {
        command.args(["attach", &session.name]);
    }
    command.cwd(root);
    command.env("ZMX_DIR", directory());
    command.env("ZMX_NO_DETACH_KEY", "1");
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "Glimcode");
    for variable in ["ZMX_SESSION", "ZMX_SESSION_PREFIX", "TMUX", "TMUX_PANE"] {
        command.env_remove(variable);
    }
    Ok(command)
}
pub(super) fn kill(executable: &Path, session: &tmux::SessionInfo) -> Result<()> {
    if !session.name.starts_with(PREFIX) || !tmux::valid_name(&session.name) {
        bail!("Invalid Glimcode zmx session");
    }
    if list(executable)?
        .iter()
        .any(|live| live.id == session.id && live.name == session.name)
    {
        run(executable, &["kill", &session.name, "--force"])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn saved_session_backend_takes_precedence_over_new_tab_preferences() {
        let tmux = tmux::Persistent::new_with_backend(Some("glimcode-test".into()), "zmx").unwrap();
        assert!(!tmux.is_zmx());
        let zmx =
            tmux::Persistent::new_with_backend(Some("glimcode-zmx-test".into()), "tmux").unwrap();
        assert!(zmx.is_zmx());
        assert!(zmx.attach_command().contains("-u ZMX_SESSION"));
    }

    #[test]
    fn zmx_inventory_preserves_identity_filters_foreign_sessions_and_decodes_cwd() -> Result<()> {
        let sessions = parse_sessions(
            "  name=glimcode-zmx-test\tpid=12\tclients=1\tcreated=123\tcwd=file://host/project%20one\nname=personal\tpid=13\tclients=0\tcreated=124\n",
        )?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "zmx:12:123");
        assert_eq!(sessions[0].directory, "/project one");
        assert_eq!(sessions[0].clients, 1);
        assert!(parse_sessions("name=glimcode-zmx-test\tpid=bad\tclients=0\tcreated=1").is_err());
        Ok(())
    }
}
