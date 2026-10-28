//! Reusable HTTPS identity stored under ~/.glimcode/terminal-tls.
use super::*;
use std::process::{Command as Process, Output, Stdio};

pub(super) struct Material {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub fingerprint: String,
}
fn openssl(args: &[&std::ffi::OsStr]) -> Result<Output> {
    let executable = tmux::find_executable("openssl").context(
        "OpenSSL is required for HTTPS sharing. Install it with `brew install openssl`.",
    )?;
    let output = Process::new(executable)
        .args(args)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        bail!(
            "HTTPS certificate error: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}
fn arguments<'a>(values: &'a [&'a str]) -> Vec<&'a std::ffi::OsStr> {
    values.iter().map(std::ffi::OsStr::new).collect()
}
impl Material {
    pub fn prepare(options: &share::Options) -> Result<Option<Self>> {
        if !options.https || options.custom_tls {
            return Self::prepare_in(options, Path::new("."));
        }
        let directory = crate::preferences::path()
            .and_then(|path| path.parent().map(|parent| parent.join("terminal-tls")))
            .context("No home directory for the HTTPS certificate")?;
        Self::prepare_in(options, &directory)
    }
    pub(super) fn prepare_in(options: &share::Options, destination: &Path) -> Result<Option<Self>> {
        if !options.https {
            return Ok(None);
        }
        if options.custom_tls {
            if options.certificate.trim().is_empty() || options.private_key.trim().is_empty() {
                bail!(
                    "Choose both a PEM certificate and its private key before starting HTTPS sharing."
                );
            }
            let mut material = Self {
                certificate: crate::preferences::expand_home_path(options.certificate.trim()),
                private_key: crate::preferences::expand_home_path(options.private_key.trim()),
                fingerprint: String::new(),
            };
            for (label, path) in [
                ("Certificate", &material.certificate),
                ("Private key", &material.private_key),
            ] {
                if !path.is_file() {
                    bail!("{label} file does not exist: {}", path.display());
                }
            }
            material.validate()?;
            return Ok(Some(material));
        }
        // Reuse the identity across shares and restarts. A complete directory
        // is published atomically so concurrent editor instances cannot expose
        // a half-written certificate/key pair or overwrite each other's key.
        static GENERATION: Mutex<()> = Mutex::new(());
        let _guard = GENERATION.lock().unwrap();
        if destination.exists() {
            return Self::load_saved(destination).map(Some);
        }
        let parent = destination
            .parent()
            .context("No HTTPS certificate directory")?;
        std::fs::create_dir_all(parent)?;
        let directory = tempfile::Builder::new()
            .prefix(".terminal-tls-")
            .tempdir_in(parent)?;
        let certificate = directory.path().join("terminal-cert.pem");
        let private_key = directory.path().join("terminal-key.pem");
        let config = directory.path().join("openssl.cnf");
        let mut settings = String::from(
            "[req]\nprompt=no\ndistinguished_name=dn\nx509_extensions=extensions\n[dn]\nCN=Glimcode terminal\n[extensions]\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=@addresses\n[addresses]\nDNS.1=localhost\nIP.1=127.0.0.1\n",
        );
        for (index, address) in share::network_addresses().iter().enumerate() {
            settings.push_str(&format!("IP.{}={address}\n", index + 2));
        }
        std::fs::write(&config, settings)?;
        let mut key_file = std::fs::OpenOptions::new();
        key_file.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            key_file.mode(0o600);
        }
        drop(key_file.open(&private_key)?);
        let mut args = arguments(&[
            "req", "-x509", "-newkey", "rsa:2048", "-sha256", "-nodes", "-batch", "-days", "365",
            "-config",
        ]);
        args.push(config.as_os_str());
        args.push(std::ffi::OsStr::new("-keyout"));
        args.push(private_key.as_os_str());
        args.push(std::ffi::OsStr::new("-out"));
        args.push(certificate.as_os_str());
        openssl(&args).context("Could not generate a local HTTPS certificate")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&private_key, std::fs::Permissions::from_mode(0o600))?;
        }
        let mut material = Self {
            certificate,
            private_key,
            fingerprint: String::new(),
        };
        material.validate()?;
        match std::fs::rename(directory.path(), destination) {
            Ok(()) => {}
            Err(_) if destination.exists() => return Self::load_saved(destination).map(Some),
            Err(error) => return Err(error).context("Could not save the HTTPS certificate"),
        }
        material.certificate = destination.join("terminal-cert.pem");
        material.private_key = destination.join("terminal-key.pem");
        Ok(Some(material))
    }
    fn load_saved(directory: &Path) -> Result<Self> {
        let mut material = Self {
            certificate: directory.join("terminal-cert.pem"),
            private_key: directory.join("terminal-key.pem"),
            fingerprint: String::new(),
        };
        material.validate().with_context(|| format!(
            "Could not use the saved HTTPS identity in {}. Choose your own certificate or replace this identity to renew it", directory.display()
        ))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &material.private_key,
                std::fs::Permissions::from_mode(0o600),
            )?;
        }
        Ok(material)
    }
    fn validate(&mut self) -> Result<()> {
        let cert = openssl(&[
            std::ffi::OsStr::new("x509"),
            std::ffi::OsStr::new("-in"),
            self.certificate.as_os_str(),
            std::ffi::OsStr::new("-pubkey"),
            std::ffi::OsStr::new("-noout"),
        ])?;
        let key = openssl(&[
            std::ffi::OsStr::new("pkey"),
            std::ffi::OsStr::new("-in"),
            self.private_key.as_os_str(),
            std::ffi::OsStr::new("-pubout"),
            std::ffi::OsStr::new("-passin"),
            std::ffi::OsStr::new("pass:"),
        ])
        .context("Use a readable, unencrypted PEM private key")?;
        if cert.stdout != key.stdout {
            bail!("The HTTPS certificate and private key do not match.");
        }
        let mut args = arguments(&["x509", "-in"]);
        args.push(self.certificate.as_os_str());
        args.extend(arguments(&["-checkend", "0", "-noout"]));
        openssl(&args).context("The HTTPS certificate is expired")?;
        let mut args = arguments(&["x509", "-in"]);
        args.push(self.certificate.as_os_str());
        args.extend(arguments(&["-noout", "-fingerprint", "-sha256"]));
        let output = openssl(&args)?;
        let text = String::from_utf8(output.stdout)?;
        self.fingerprint = text
            .split_once('=')
            .context("Could not read the HTTPS certificate fingerprint")?
            .1
            .trim()
            .to_owned();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_certificate_has_matching_key_sans_and_private_permissions() -> Result<()> {
        let options = share::Options::default();
        assert!(options.https);
        let storage = tempfile::tempdir()?;
        let identity = storage.path().join("terminal-tls");
        let material = Material::prepare_in(&options, &identity)?.expect("HTTPS certificate");
        let certificate = material.certificate.clone();
        assert_eq!(material.fingerprint.len(), 95);
        let output = openssl(&[
            std::ffi::OsStr::new("x509"),
            std::ffi::OsStr::new("-in"),
            certificate.as_os_str(),
            std::ffi::OsStr::new("-noout"),
            std::ffi::OsStr::new("-text"),
        ])?;
        let text = String::from_utf8(output.stdout)?;
        assert!(text.contains("DNS:localhost"));
        assert!(text.contains("IP Address:127.0.0.1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&material.private_key)?
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let custom = share::Options {
            custom_tls: true,
            certificate: certificate.to_string_lossy().into_owned(),
            private_key: material.private_key.to_string_lossy().into_owned(),
            ..Default::default()
        };
        assert_eq!(
            Material::prepare(&custom)?.unwrap().fingerprint,
            material.fingerprint
        );
        let reused = Material::prepare_in(&options, &identity)?.unwrap();
        assert_eq!(reused.fingerprint, material.fingerprint);
        assert_eq!(
            std::fs::read(&reused.private_key)?,
            std::fs::read(&material.private_key)?
        );
        let second =
            Material::prepare_in(&options, &storage.path().join("other-identity"))?.unwrap();
        let mismatch = share::Options {
            private_key: second.private_key.to_string_lossy().into_owned(),
            ..custom
        };
        assert!(
            Material::prepare(&mismatch)
                .err()
                .unwrap()
                .to_string()
                .contains("do not match")
        );
        drop(material);
        assert!(
            certificate.exists(),
            "generated certificate and key survive the end of a share"
        );
        Ok(())
    }
    #[test]
    fn saved_identity_is_not_overwritten_when_corrupt() -> Result<()> {
        let storage = tempfile::tempdir()?;
        let identity = storage.path().join("terminal-tls");
        std::fs::create_dir(&identity)?;
        let key = identity.join("terminal-key.pem");
        std::fs::write(&key, "existing identity")?;
        assert!(Material::prepare_in(&share::Options::default(), &identity).is_err());
        assert_eq!(std::fs::read_to_string(key)?, "existing identity");
        Ok(())
    }

    #[test]
    fn https_never_silently_falls_back_to_http() {
        assert!(
            Material::prepare(&share::Options {
                custom_tls: true,
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            Material::prepare(&share::Options {
                https: false,
                ..Default::default()
            })
            .unwrap()
            .is_none()
        );
    }
}
