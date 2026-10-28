//! HTTPS terminates on this Mac and proxies only to a loopback ttyd server.
//! HTTP/1.1 avoids ttyd's HTTP/2 WebSocket/origin-check compatibility issue.
use super::*;
use rustls::{
    ServerConfig, ServerConnection,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
};
use std::net::{SocketAddr, TcpListener, TcpStream};

pub(super) struct Relay {
    listener: TcpListener,
    config: Arc<ServerConfig>,
    stop: Arc<AtomicBool>,
    clients: Vec<std::thread::JoinHandle<()>>,
}
impl Relay {
    pub fn bind(address: SocketAddr, material: &tls::Material) -> Result<Self> {
        let certificates = CertificateDer::pem_file_iter(&material.certificate)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let key = PrivateKeyDer::from_pem_file(&material.private_key)?;
        let mut config =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_no_client_auth()
                .with_single_cert(certificates, key)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let listener =
            TcpListener::bind(address).context("Could not listen for HTTPS connections")?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            config: Arc::new(config),
            stop: Arc::new(AtomicBool::new(false)),
            clients: vec![],
        })
    }
    pub fn poll(&mut self, backend: SocketAddr) -> Result<()> {
        self.clients.retain(|thread| !thread.is_finished());
        for _ in 0..32 {
            match self.listener.accept() {
                Ok((client, _)) => {
                    // Bound simultaneous handshakes and slow browser connections.
                    if self.clients.len() >= 32 {
                        continue;
                    }
                    let config = self.config.clone();
                    let stop = self.stop.clone();
                    self.clients.push(std::thread::spawn(move || {
                        let _ = proxy(client, backend, config, &stop);
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}
impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for client in self.clients.drain(..) {
            let _ = client.join();
        }
    }
}
fn temporary(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}
fn proxy(
    mut client: TcpStream,
    backend: SocketAddr,
    config: Arc<ServerConfig>,
    stop: &AtomicBool,
) -> Result<()> {
    client.set_nonblocking(true)?;
    client.set_nodelay(true)?;
    let mut upstream = TcpStream::connect_timeout(&backend, Duration::from_secs(1))?;
    upstream.set_nonblocking(true)?;
    upstream.set_nodelay(true)?;
    let mut connection = ServerConnection::new(config)?;
    connection.set_buffer_limit(Some(64 * 1024));
    let started = Instant::now();
    let mut pending_upstream = vec![];
    let mut pending_tls = vec![];
    let mut upstream_closed = false;
    let mut buffer = [0u8; 8192];
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if connection.is_handshaking() && started.elapsed() > Duration::from_secs(10) {
            return Ok(());
        }
        let mut progressed = false;
        if pending_upstream.len() < 64 * 1024 && connection.wants_read() {
            match connection.read_tls(&mut client) {
                Ok(0) => return Ok(()),
                Ok(_) => {
                    connection.process_new_packets()?;
                    progressed = true;
                }
                Err(error) if temporary(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if !connection.is_handshaking() && pending_upstream.len() < 64 * 1024 {
            match connection.reader().read(&mut buffer) {
                Ok(count) if count > 0 => {
                    pending_upstream.extend_from_slice(&buffer[..count]);
                    progressed = true;
                }
                Ok(_) => {}
                Err(error) if temporary(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if !pending_upstream.is_empty() {
            match upstream.write(&pending_upstream) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    pending_upstream.drain(..count);
                    progressed = true;
                }
                Err(error) if temporary(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if !connection.is_handshaking() && pending_tls.is_empty() && !upstream_closed {
            match upstream.read(&mut buffer) {
                Ok(0) => upstream_closed = true,
                Ok(count) => {
                    pending_tls.extend_from_slice(&buffer[..count]);
                    progressed = true;
                }
                Err(error) if temporary(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if !pending_tls.is_empty() {
            match connection.writer().write(&pending_tls) {
                Ok(0) => {}
                Ok(count) => {
                    pending_tls.drain(..count);
                    progressed = true;
                }
                Err(error) if temporary(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if upstream_closed && pending_tls.is_empty() {
            connection.send_close_notify();
        }
        if connection.wants_write() {
            match connection.write_tls(&mut client) {
                Ok(count) => progressed |= count > 0,
                Err(error) if temporary(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if upstream_closed && pending_tls.is_empty() && !connection.wants_write() {
            return Ok(());
        }
        if !progressed {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
