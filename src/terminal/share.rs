//! A browser client for an existing tmux session. Never owns the shell itself.
use super::*;
use std::{
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    process::{Child, Command as Process, Stdio},
};

#[derive(Clone)]
pub(super) struct Options {
    pub network: bool,
    pub writable: bool,
    pub port: u16,
    pub https: bool,
    pub custom_tls: bool,
    pub certificate: String,
    pub private_key: String,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            network: false,
            writable: true,
            port: 7681,
            https: true,
            custom_tls: false,
            certificate: String::new(),
            private_key: String::new(),
        }
    }
}
impl Options {
    pub fn from_settings(settings: &Settings, port: u16) -> Self {
        Self {
            port,
            custom_tls: !settings.terminal_tls_certificate.trim().is_empty()
                || !settings.terminal_tls_private_key.trim().is_empty(),
            certificate: settings.terminal_tls_certificate.trim().to_owned(),
            private_key: settings.terminal_tls_private_key.trim().to_owned(),
            ..Default::default()
        }
    }
    pub fn scheme(&self) -> &'static str {
        if self.https { "https" } else { "http" }
    }
}

#[derive(Clone)]
pub(super) enum Status {
    Starting,
    Running,
    Failed(String),
    Stopped,
}

pub(super) struct Share {
    pub options: Options,
    pub password: String,
    status: Arc<Mutex<Status>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    fingerprint: Arc<Mutex<Option<String>>>,
}
impl Share {
    pub fn start(session: tmux::Persistent, options: Options, ctx: egui::Context) -> Result<Self> {
        let executable = tmux::find_executable("ttyd").context(
            "ttyd is not installed. Install it with `brew install ttyd` on macOS, then try again.",
        )?;
        let mut random = [0u8; 18];
        std::fs::File::open("/dev/urandom")
            .context("Could not generate a sharing password")?
            .read_exact(&mut random)?;
        let password: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let fingerprint = Arc::new(Mutex::new(None));
        let worker_fingerprint = fingerprint.clone();
        let status = Arc::new(Mutex::new(Status::Starting));
        let stop = Arc::new(AtomicBool::new(false));
        let shared = status.clone();
        let stopped = stop.clone();
        let settings = options.clone();
        let credential = format!("glimcode:{password}");
        let worker = std::thread::spawn(move || {
            let result = run(
                &session,
                &executable,
                &settings,
                &credential,
                &stopped,
                &shared,
                &ctx,
                &worker_fingerprint,
            );
            *shared.lock().unwrap() = match result {
                Ok(()) => Status::Stopped,
                Err(error) => Status::Failed(format!("{error:#}")),
            };
            ctx.request_repaint();
        });
        Ok(Self {
            options,
            password,
            status,
            stop,
            worker: Some(worker),
            fingerprint,
        })
    }
    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
    pub fn active(&self) -> bool {
        matches!(self.status(), Status::Starting | Status::Running)
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
    pub fn fingerprint(&self) -> Option<String> {
        self.fingerprint.lock().unwrap().clone()
    }
    pub fn local_url(&self) -> String {
        format!(
            "{}://127.0.0.1:{}",
            self.options.scheme(),
            self.options.port
        )
    }
}
impl Drop for Share {
    fn drop(&mut self) {
        self.stop();
        // Reap ttyd before the editor exits; a signal alone leaves a race with
        // process shutdown and can expose an orphaned sharing endpoint.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn command(
    executable: &Path,
    session: &tmux::Persistent,
    options: &Options,
    credential: &str,
) -> Process {
    let mut command = Process::new(executable);
    command.args([
        "--port",
        &options.port.to_string(),
        "--interface",
        if options.network {
            "0.0.0.0"
        } else {
            "127.0.0.1"
        },
        "--credential",
        credential,
        "--check-origin",
        "--debug",
        "3",
    ]);
    if options.writable {
        command.arg("--writable");
    }
    command
        .arg(session.executable())
        .args(["-L", "glimcode", "attach-session"]);
    if !options.writable {
        command.arg("-r");
    }
    command
        .args(["-t", &format!("={}", session.name)])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null());
    command
}
pub(super) struct Server(pub(super) Child);
impl Drop for Server {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        // Let ttyd close its browser clients and detach their tmux clients.
        #[cfg(unix)]
        unsafe {
            libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if matches!(self.0.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn run(
    session: &tmux::Persistent,
    executable: &Path,
    options: &Options,
    credential: &str,
    stop: &AtomicBool,
    status: &Mutex<Status>,
    ctx: &egui::Context,
    fingerprint: &Mutex<Option<String>>,
) -> Result<()> {
    if !session.exists() {
        bail!("The tmux session is not running. Wait for the terminal to start, then try again.");
    }
    let address = SocketAddr::from((
        if options.network {
            Ipv4Addr::UNSPECIFIED
        } else {
            Ipv4Addr::LOCALHOST
        },
        options.port,
    ));
    let material = tls::Material::prepare(options)?;
    let mut relay = match &material {
        Some(material) => {
            *fingerprint.lock().unwrap() = Some(material.fingerprint.clone());
            Some(https::Relay::bind(address, material)?)
        }
        None => {
            drop(TcpListener::bind(address).with_context(|| {
                format!(
                    "Cannot share on port {}. Choose a different port",
                    options.port
                )
            })?);
            None
        }
    };
    let mut launch_options = options.clone();
    if options.https {
        // The public port belongs to the on-Mac TLS relay. ttyd is reachable
        // only through loopback, including when sharing with other devices.
        let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        launch_options.port = reservation.local_addr()?.port();
        launch_options.https = false;
        launch_options.network = false;
        drop(reservation);
    }
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    // Keep the installed ttyd's own frontend and add our touch controls. The
    // temporary page must outlive the server (ttyd reopens it on each request).
    let page = mobile::prepare(executable, options.writable, credential, stop)?;
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let log = tempfile::NamedTempFile::new()?;
    let mut child = Server(
        command_with_page(
            executable,
            session,
            &launch_options,
            credential,
            page.path(),
        )
        .stdout(Stdio::null())
        .stderr(Stdio::from(log.reopen()?))
        .spawn()
        .context("Could not start ttyd")?,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ready = false;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some(exit) = child.0.try_wait()? {
            // ttyd logs can include its command line. Do not surface credentials.
            let mut detail = String::new();
            let _ = log.reopen()?.take(4096).read_to_string(&mut detail);
            let detail = detail.replace(credential, "[credentials hidden]").replace(
                credential
                    .split_once(':')
                    .map_or(credential, |(_, secret)| secret),
                "[password hidden]",
            );
            bail!(
                "ttyd exited ({exit}). {}",
                detail.chars().take(4096).collect::<String>()
            );
        }
        if !ready {
            let address = SocketAddr::from((Ipv4Addr::LOCALHOST, launch_options.port));
            if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
                ready = true;
                *status.lock().unwrap() = Status::Running;
                ctx.request_repaint();
            } else if Instant::now() >= deadline {
                bail!("ttyd did not become ready. Check the listen port and try again.");
            }
        }
        if ready && let Some(relay) = &mut relay {
            relay.poll(SocketAddr::from((Ipv4Addr::LOCALHOST, launch_options.port)))?;
        }
        std::thread::sleep(Duration::from_millis(if relay.is_some() {
            10
        } else {
            100
        }));
    }
}

fn command_with_page(
    executable: &Path,
    session: &tmux::Persistent,
    options: &Options,
    credential: &str,
    page: &Path,
) -> Process {
    let process = command(executable, session, options, credential);
    // Options must precede the tmux command, whose own arguments may start '-'.
    let args: Vec<_> = process.get_args().map(|arg| arg.to_owned()).collect();
    let mut with_page = Process::new(executable);
    with_page.arg("--index").arg(page).args(args);
    with_page
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null());
    with_page
}

/// URL setters escape credentials instead of interpolating them into a URL.
/// Browsers may ignore userinfo; the regular Basic-auth prompt remains available.
pub(super) fn login_url(address: &str, password: &str) -> Result<String> {
    let mut url = url::Url::parse(address)?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        bail!("Invalid terminal sharing address");
    }
    url.set_username("glimcode")
        .map_err(|_| anyhow::anyhow!("Cannot add login details to this link"))?;
    url.set_password(Some(password))
        .map_err(|_| anyhow::anyhow!("Cannot add login details to this link"))?;
    Ok(url.into())
}

/// Discover LAN and Tailscale IPv4 addresses without sending network traffic.
pub(super) fn network_urls(port: u16, https: bool) -> Vec<String> {
    let scheme = if https { "https" } else { "http" };
    network_addresses()
        .iter()
        .map(|ip| format!("{scheme}://{ip}:{port}"))
        .collect()
}

pub(super) fn network_addresses() -> Vec<Ipv4Addr> {
    let mut urls = vec![];
    #[cfg(unix)]
    unsafe {
        let mut addresses: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut addresses) == 0 {
            let mut current = addresses;
            while !current.is_null() {
                let item = &*current;
                if !item.ifa_addr.is_null()
                    && (*item.ifa_addr).sa_family as i32 == libc::AF_INET
                    && item.ifa_flags & libc::IFF_UP as u32 != 0
                {
                    let address = &*(item.ifa_addr as *const libc::sockaddr_in);
                    let ip = Ipv4Addr::from(address.sin_addr.s_addr.to_ne_bytes());
                    if !ip.is_loopback() && !ip.is_unspecified() {
                        urls.push(ip);
                    }
                }
                current = item.ifa_next;
            }
            libc::freeifaddrs(addresses);
        }
    }
    urls.sort();
    urls.dedup();
    urls
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn new_shares_use_preferences_and_https_for_all_links() -> Result<()> {
        let settings = Settings {
            terminal_tls_certificate: "/my/certificate.pem".into(),
            terminal_tls_private_key: "/my/private-key.pem".into(),
            ..Default::default()
        };
        let options = Options::from_settings(&settings, 9000);
        assert!(options.https && options.custom_tls);
        assert_eq!(options.certificate, settings.terminal_tls_certificate);
        assert_eq!(options.private_key, settings.terminal_tls_private_key);
        let link = login_url("https://192.168.1.2:9000", "secret")?;
        let parsed = url::Url::parse(&link)?;
        assert_eq!(parsed.scheme(), "https");
        assert_eq!(parsed.host_str(), Some("192.168.1.2"));
        assert_eq!(parsed.username(), "glimcode");
        assert_eq!(parsed.password(), Some("secret"));
        assert!(
            network_urls(9000, true)
                .iter()
                .all(|url| url.starts_with("https://"))
        );
        assert!(!Options::from_settings(&Settings::default(), 9000).custom_tls);
        Ok(())
    }

    #[test]
    fn login_links_preserve_the_host_and_escape_password_delimiters() -> Result<()> {
        let text = login_url("http://192.168.1.2:7681", "a@b:/?#%")?;
        let url = url::Url::parse(&text)?;
        assert_eq!(url.host_str(), Some("192.168.1.2"));
        assert_eq!(url.port(), Some(7681));
        assert_eq!(url.username(), "glimcode");
        assert!(text.contains("a%40b%3A%2F%3F%23%"));
        assert!(url.query().is_none());
        assert!(url.fragment().is_none());
        assert!(login_url("file:///tmp/example", "password").is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires ttyd, curl, OpenSSL, and permission to listen on localhost TCP"]
    fn ttyd_https_validates_the_certificate_and_requires_authentication() -> Result<()> {
        let executable = tmux::find_executable("ttyd").context("Install ttyd")?;
        let curl = tmux::find_executable("curl").context("Install curl")?;
        let directory = tempfile::tempdir()?;
        let material =
            tls::Material::prepare_in(&Options::default(), &directory.path().join("tls"))?.unwrap();
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let options = Options {
            port,
            custom_tls: true,
            certificate: material.certificate.to_string_lossy().into_owned(),
            private_key: material.private_key.to_string_lossy().into_owned(),
            ..Default::default()
        };
        // Serving the authenticated page does not create a terminal; ttyd only
        // attaches to tmux when a browser opens its WebSocket.
        let session = tmux::Persistent::new(Some("glimcode-https-test".into())).unwrap();
        let backend_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let backend = backend_listener.local_addr()?;
        drop(backend_listener);
        let launch_options = Options {
            port: backend.port(),
            https: false,
            ..options.clone()
        };
        let log = tempfile::NamedTempFile::new()?;
        let mut child = Server(
            command(
                &executable,
                &session,
                &launch_options,
                "glimcode:test-password",
            )
            .stdout(Stdio::null())
            .stderr(Stdio::from(log.reopen()?))
            .spawn()?,
        );
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let mut relay = https::Relay::bind(address, &material)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(child.0.try_wait()?.is_none(), "ttyd failed to start");
            if TcpStream::connect_timeout(&backend, Duration::from_millis(100)).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "ttyd startup timed out");
            std::thread::sleep(Duration::from_millis(30));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                relay.poll(backend).unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        for (credentials, expected) in [(None, "401"), (Some("glimcode:test-password"), "200")] {
            let mut request = Process::new(&curl);
            request
                .args([
                    "--noproxy",
                    "*",
                    "--silent",
                    "--show-error",
                    "--verbose",
                    "--max-time",
                    "3",
                    "--cacert",
                ])
                .arg(&material.certificate)
                .args(["--output", "/dev/null", "--write-out", "%{http_code}"]);
            if let Some(credentials) = credentials {
                request.args(["--user", credentials]);
            }
            let result = request.arg(format!("https://127.0.0.1:{port}")).output()?;
            assert!(
                result.status.success(),
                "TLS verification failed: {}\nServer: {}",
                String::from_utf8_lossy(&result.stderr),
                std::fs::read_to_string(log.path()).unwrap_or_default()
            );
            assert_eq!(String::from_utf8(result.stdout)?, expected);
        }
        let upgrade = Process::new(&curl)
            .args(["--noproxy", "*", "--silent", "--max-time", "1", "--cacert"])
            .arg(&material.certificate)
            .args([
                "--user",
                "glimcode:test-password",
                "--output",
                "/dev/null",
                "--write-out",
                "%{http_code}",
                "-H",
                "Connection: Upgrade",
                "-H",
                "Upgrade: websocket",
                "-H",
                "Sec-WebSocket-Version: 13",
                "-H",
                "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
                "-H",
                "Sec-WebSocket-Protocol: tty",
                "-H",
            ])
            .arg(format!("Origin: https://127.0.0.1:{port}"))
            .arg(format!("https://127.0.0.1:{port}/ws"))
            .output()?;
        // The WebSocket intentionally stays open until curl's timeout. Its 101
        // proves WSS upgrades and origin checks survive the local TLS relay.
        assert_eq!(String::from_utf8(upgrade.stdout)?, "101");
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        drop(child);
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires tmux, ttyd, and permission to create Unix and TCP sockets"]
    fn browser_sharing_requires_auth_and_stop_preserves_the_shell() -> Result<()> {
        let session = tmux::Persistent::new(None).context("Install tmux")?;
        struct Cleanup(tmux::Persistent);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.close();
            }
        }
        let _cleanup = Cleanup(session.clone());
        let dir = tempfile::tempdir()?;
        let settings = Settings {
            shell: "/bin/sh".into(),
            ..Default::default()
        };
        // Create the persistent pane; ttyd will attach browser clients to it.
        let _ = session.attach(dir.path(), &settings, FALLBACK_SIZE)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let share = Share::start(
            session.clone(),
            Options {
                port,
                https: false,
                ..Default::default()
            },
            egui::Context::default(),
        )?;
        let deadline = Instant::now() + Duration::from_secs(6);
        while matches!(share.status(), Status::Starting) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(
            matches!(share.status(), Status::Running),
            "ttyd did not start"
        );
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut response = [0u8; 1024];
        let count = stream.read(&mut response)?;
        assert!(
            String::from_utf8_lossy(&response[..count]).contains("401"),
            "unauthenticated access must be rejected"
        );
        drop(stream);
        drop(share);
        assert!(
            session.exists(),
            "stopping sharing must preserve the tmux shell"
        );
        assert!(
            TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(),
            "stopping sharing must close the browser endpoint"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn stopping_the_server_reaps_the_process() -> Result<()> {
        let child = Process::new("/bin/sleep").arg("60").spawn()?;
        let pid = child.id() as libc::pid_t;
        drop(Server(child));
        let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(result, -1, "the server process must already be reaped");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
        Ok(())
    }

    #[test]
    fn sharing_attaches_only_the_selected_session_with_authentication() -> Result<()> {
        let Some(session) = tmux::Persistent::new(Some("glimcode-share-test".into())) else {
            return Ok(());
        };
        let options = Options {
            certificate: "/certificate path/cert.pem".into(),
            private_key: "/private key/key.pem".into(),
            ..Default::default()
        };
        let process = command(
            Path::new("/path with spaces/ttyd"),
            &session,
            &options,
            "glimcode:secret",
        );
        let args: Vec<_> = process
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|v| v == ["--interface", "127.0.0.1"]));
        assert!(
            args.windows(2)
                .any(|v| v == ["--credential", "glimcode:secret"])
        );
        assert!(args.contains(&"--check-origin".into()));
        assert!(args.contains(&"--writable".into()));
        assert!(
            !args.contains(&"--ssl".into()),
            "TLS terminates on this Mac before the loopback ttyd service"
        );
        assert!(args.ends_with(&["-t".into(), "=glimcode-share-test".into()]));
        assert!(
            !args.contains(&"-d".into()),
            "browser must not detach the desktop client"
        );
        let readonly = command(
            Path::new("ttyd"),
            &session,
            &Options {
                network: true,
                writable: false,
                port: 8001,
                https: false,
                ..Default::default()
            },
            "glimcode:secret",
        );
        let args: Vec<_> = readonly
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|v| v == ["--interface", "0.0.0.0"]));
        assert!(!args.contains(&"--writable".into()));
        assert!(!args.contains(&"--ssl".into()));
        assert!(
            args.contains(&"-r".into()),
            "read-only must also apply to the tmux client"
        );
        let page = command_with_page(
            Path::new("ttyd"),
            &session,
            &options,
            "glimcode:secret",
            Path::new("/temporary page/index.html"),
        );
        let args: Vec<_> = page
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..2], &["--index", "/temporary page/index.html"]);
        assert!(
            args.windows(2)
                .any(|v| v == ["--credential", "glimcode:secret"])
        );
        assert!(args.ends_with(&["-t".into(), "=glimcode-share-test".into()]));
        Ok(())
    }
}
