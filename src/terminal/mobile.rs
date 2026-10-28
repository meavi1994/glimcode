//! Add touch keys to the frontend bundled with the user's installed ttyd.
use super::*;
use base64::{Engine, engine::general_purpose::STANDARD};
use std::{
    net::{Ipv4Addr, TcpListener, TcpStream},
    process::{Command, Stdio},
};

const CONTROLS: &str = include_str!("mobile.js");
const MAX_PAGE: u64 = 16 * 1024 * 1024;

pub(super) fn prepare(
    executable: &Path,
    writable: bool,
    credential: &str,
    stop: &AtomicBool,
) -> Result<tempfile::NamedTempFile> {
    let mut page = tempfile::NamedTempFile::new()?;
    // A short-lived, authenticated loopback server supplies the exact frontend
    // shipped with this ttyd version. No WebSocket is opened and no shell starts.
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let address = reservation.local_addr()?;
    drop(reservation);
    let mut server = share::Server(
        Command::new(executable)
            .args([
                "--interface",
                "127.0.0.1",
                "--port",
                &address.port().to_string(),
                "--credential",
                credential,
                "--check-origin",
                "--debug",
                "1",
                "/usr/bin/false",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Could not prepare ttyd's mobile page")?,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(page);
        }
        if server.0.try_wait()?.is_some() {
            bail!("ttyd exited while preparing its mobile page");
        }
        if let Ok(stream) = TcpStream::connect_timeout(&address, Duration::from_millis(100)) {
            break stream;
        }
        if Instant::now() >= deadline {
            bail!("Timed out preparing ttyd's mobile page");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    write!(
        stream,
        "GET / HTTP/1.0\r\nHost: {address}\r\nAuthorization: Basic {}\r\nAccept-Encoding: identity\r\nConnection: close\r\n\r\n",
        STANDARD.encode(credential)
    )?;
    let mut response = Vec::new();
    stream.take(MAX_PAGE + 1).read_to_end(&mut response)?;
    if response.len() as u64 > MAX_PAGE {
        bail!("ttyd's browser page is too large");
    }
    let html = page_body(&response)?;
    page.write_all(customize(html, writable)?.as_bytes())?;
    page.flush()?;
    Ok(page)
}

fn page_body(response: &[u8]) -> Result<&str> {
    let response =
        std::str::from_utf8(response).context("ttyd returned a non-UTF-8 browser page")?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .context("Invalid ttyd page response")?;
    if !headers
        .lines()
        .next()
        .is_some_and(|line| line.split_whitespace().nth(1) == Some("200"))
    {
        bail!("ttyd did not return its browser page");
    }
    if headers
        .lines()
        .any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding:"))
    {
        bail!("Unsupported ttyd page transfer encoding");
    }
    Ok(body)
}

fn customize(html: &str, writable: bool) -> Result<String> {
    let head = html
        .to_ascii_lowercase()
        .find("<head>")
        .context("ttyd returned an unsupported browser page")?
        + 6;
    let mut result = String::with_capacity(html.len() + CONTROLS.len() + 200);
    result.push_str(&html[..head]);
    result.push_str("<meta name=\"viewport\" content=\"width=device-width,initial-scale=1,viewport-fit=cover\">");
    if writable {
        // Install before ttyd's scripts so its existing authenticated WebSocket
        // can carry touch input, including after ttyd reconnects.
        result.push_str("<script>");
        result.push_str(CONTROLS);
        result.push_str("</script>");
    }
    result.push_str(&html[head..]);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires ttyd and permission to listen on localhost TCP"]
    fn installed_ttyd_frontend_is_preserved_in_a_temporary_page() -> Result<()> {
        let executable = tmux::find_executable("ttyd").context("Install ttyd")?;
        let stop = AtomicBool::new(false);
        let page = prepare(&executable, true, "glimcode:mobile-page-test", &stop)?;
        let html = std::fs::read_to_string(page.path())?;
        assert!(html.contains(CONTROLS));
        assert!(html.contains("<title>ttyd"));
        assert!(html.contains("window.term"));
        let path = page.path().to_owned();
        drop(page);
        assert!(!path.exists());
        Ok(())
    }
    #[test]
    fn controls_precede_the_original_client_and_readonly_has_no_input_hook() -> Result<()> {
        let html =
            "<!doctype html><HTML><HEAD><script>original()</script></HEAD><body></body></HTML>";
        let writable = customize(html, true)?;
        assert!(writable.find(CONTROLS).unwrap() < writable.find("original()").unwrap());
        assert!(writable.contains("viewport-fit=cover"));
        let readonly = customize(html, false)?;
        assert!(!readonly.contains(CONTROLS));
        assert!(readonly.contains("original()"));
        assert!(customize("not a page", true).is_err());
        Ok(())
    }
    #[test]
    fn bootstrap_accepts_only_a_successful_plain_response() -> Result<()> {
        assert_eq!(
            page_body(b"HTTP/1.0 200 OK\r\nContent-Length: 5\r\n\r\nhello")?,
            "hello"
        );
        assert!(page_body(b"HTTP/1.1 401 Unauthorized\r\n\r\nsecret").is_err());
        assert!(
            page_body(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello").is_err()
        );
        Ok(())
    }
}
