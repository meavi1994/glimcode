//! ttyd child process: attach normally and report its PTY size to the editor.
use super::*;
use std::process::{Command as Process, Stdio};

const FLAG: &str = "--glimcode-ttyd-client";

pub(super) fn wrap(command: &mut Process, directory: &Path) -> Result<()> {
    let args: Vec<_> = command.get_args().map(|arg| arg.to_owned()).collect();
    let backend = args
        .iter()
        .position(|arg| arg == "--")
        .context("Missing ttyd command separator")?;
    let executable = std::env::current_exe()?;
    let mut wrapped = args[..=backend].to_vec();
    wrapped.extend([
        executable.into_os_string(),
        FLAG.into(),
        directory.as_os_str().to_owned(),
    ]);
    wrapped.extend_from_slice(&args[backend + 1..]);
    // Command has no argument replacement API; preserve its environment too.
    let mut replacement = Process::new(command.get_program());
    replacement.args(wrapped);
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            replacement.env(key, value);
        } else {
            replacement.env_remove(key);
        }
    }
    replacement.stdin(Stdio::null());
    *command = replacement;
    Ok(())
}

pub(crate) fn run() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(FLAG)) {
        return None;
    }
    let result = (|| -> Result<i32> {
        let directory = PathBuf::from(args.next().context("Missing size directory")?);
        let executable = args.next().context("Missing terminal backend")?;
        let path = directory.join(std::process::id().to_string());
        struct Remove(PathBuf);
        impl Drop for Remove {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _remove = Remove(path.clone());
        // The backend inherits ttyd's controlling terminal and foreground
        // process group. No pipe or extra terminal changes its input protocol.
        let mut child = NativeChild(Process::new(executable).args(args).spawn()?);
        let mut previous = None;
        loop {
            if let Some(status) = child.0.try_wait()? {
                return Ok(status.code().unwrap_or(1));
            }
            if let Some(size) = terminal_size() {
                let size = (size.rows, size.cols);
                if previous != Some(size) {
                    let pending = path.with_extension("pending");
                    std::fs::write(&pending, format!("{} {}", size.0, size.1))?;
                    std::fs::rename(pending, &path)?;
                    previous = Some(size);
                }
            }
            std::thread::sleep(Duration::from_millis(40));
        }
    })();
    Some(match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("Glimcode browser terminal: {error:#}");
            1
        }
    })
}

// Use the standard child directly here; killing the client must not kill the
// persistent shell, and a normal wrapper exit must reap its client process.
struct NativeChild(std::process::Child);
impl Drop for NativeChild {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn terminal_size() -> Option<PtySize> {
    #[cfg(unix)]
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        if libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) == 0
            && size.ws_row > 0
            && size.ws_col > 0
        {
            return Some(PtySize {
                rows: size.ws_row,
                cols: size.ws_col,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    }
    None
}

pub(super) fn connected_size(directory: &Path) -> Option<PtySize> {
    let mut latest = None;
    for entry in std::fs::read_dir(directory).ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        #[cfg(unix)]
        if unsafe { libc::kill(pid, 0) } != 0 {
            let _ = std::fs::remove_file(entry.path());
            continue;
        }
        let Some(size) = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|text| parse_size(&text))
        else {
            continue;
        };
        let Some(modified) = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
        else {
            continue;
        };
        if latest.as_ref().is_none_or(|(time, _)| modified > *time) {
            latest = Some((modified, size));
        }
    }
    latest.map(|(_, size)| size)
}

fn parse_size(text: &str) -> Option<PtySize> {
    let mut parts = text.split_whitespace();
    let rows = parts.next()?.parse::<u16>().ok()?.clamp(2, 200);
    let cols = parts.next()?.parse::<u16>().ok()?.clamp(10, 500);
    if parts.next().is_some() {
        return None;
    }
    Some(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_reports_follow_live_clients_and_ignore_incomplete_or_dead_reports() -> Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(connected_size(dir.path()).is_none());
        let report = dir.path().join(std::process::id().to_string());
        std::fs::write(&report, "45 42")?;
        let size = connected_size(dir.path()).unwrap();
        assert_eq!((size.rows, size.cols), (45, 42));
        std::fs::write(&report, "23 66")?;
        let size = connected_size(dir.path()).unwrap();
        assert_eq!((size.rows, size.cols), (23, 66));
        std::fs::write(&report, "23 ")?;
        assert!(connected_size(dir.path()).is_none());
        std::fs::remove_file(&report)?;
        #[cfg(unix)]
        {
            let dead = dir.path().join(i32::MAX.to_string());
            std::fs::write(&dead, "45 42")?;
            assert!(connected_size(dir.path()).is_none());
            assert!(!dead.exists());
        }
        Ok(())
    }
}
