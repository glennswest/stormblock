//! The management API's TLS (#203): the node's stormcert pair, and the node
//! CA a client certificate is verified against.
//!
//! A client certificate is asked for, never required: a caller with the
//! token and a health probe connect as before. One the node CA issued is a
//! credential at the node token's tier (`mgmt::auth`), so a caller that holds
//! a stormcert client pair needs no shared token on the wire at all.
//!
//! stormcert renews the pair, so the files are watched: every connection
//! checks (at most every few seconds) whether one changed, and a set that
//! loads replaces the one being served. One that does not load is said and
//! the old one kept — a half-written renewal must not take the API down.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;

/// A request that came over a connection whose client certificate the node
/// CA verified. Put on every request of that connection, read by
/// `mgmt::auth`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCert {
    /// `sha256:<first 16 hex digits>` of the certificate: who, in the audit
    /// log.
    pub fingerprint: String,
}

impl ClientCert {
    pub fn from_der(der: &[u8]) -> Self {
        use sha2::Digest;
        let d = sha2::Sha256::digest(der);
        let hex: String = d.iter().take(8).map(|b| format!("{b:02x}")).collect();
        ClientCert { fingerprint: format!("sha256:{hex}") }
    }
}

/// The files the listener serves from.
#[derive(Debug, Clone)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub client_ca: Option<PathBuf>,
}

impl TlsFiles {
    fn paths(&self) -> Vec<&Path> {
        let mut v = vec![self.cert.as_path(), self.key.as_path()];
        if let Some(ca) = &self.client_ca {
            v.push(ca.as_path());
        }
        v
    }

    fn stamps(&self) -> Vec<Option<SystemTime>> {
        self.paths()
            .into_iter()
            .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .collect()
    }
}

fn read_certs(path: &Path) -> anyhow::Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let f = std::fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("failed to open TLS cert '{}': {e}", path.display()))?;
    let certs: Vec<_> = rustls_pemfile::certs(&mut std::io::BufReader::new(f))
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("failed to parse certificates in {}: {e}", path.display()))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", path.display());
    }
    Ok(certs)
}

/// The server configuration for these files.
pub fn load(files: &TlsFiles) -> anyhow::Result<ServerConfig> {
    let certs = read_certs(&files.cert)?;
    let key_file = std::fs::File::open(&files.key)
        .map_err(|e| anyhow::anyhow!("failed to open TLS key '{}': {e}", files.key.display()))?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(key_file))
        .map_err(|e| anyhow::anyhow!("failed to parse TLS key: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", files.key.display()))?;

    crate::http::ensure_crypto_provider();
    let builder = ServerConfig::builder();
    let builder = match &files.client_ca {
        None => builder.with_no_client_auth(),
        Some(ca) => {
            let mut roots = rustls::RootCertStore::empty();
            for c in read_certs(ca)? {
                roots
                    .add(c)
                    .map_err(|e| anyhow::anyhow!("client CA {}: {e}", ca.display()))?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                // Asked for, not required: the token and the probes still
                // connect without one.
                .allow_unauthenticated()
                .build()
                .map_err(|e| anyhow::anyhow!("client CA {}: {e}", ca.display()))?;
            builder.with_client_cert_verifier(verifier)
        }
    };
    builder
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("invalid TLS configuration: {e}"))
}

/// How often a connection may look at the files.
const CHECK_EVERY: Duration = Duration::from_secs(5);

struct Current {
    acceptor: TlsAcceptor,
    stamps: Vec<Option<SystemTime>>,
    checked: Instant,
}

/// The acceptor, re-made when the files change.
pub struct Reloader {
    files: TlsFiles,
    current: Mutex<Current>,
    check_every: Duration,
}

impl Reloader {
    /// Fails when the files do not load now: a node told to serve TLS must
    /// not start serving plain HTTP, nor nothing quietly.
    pub fn new(files: TlsFiles) -> anyhow::Result<Self> {
        Self::with_interval(files, CHECK_EVERY)
    }

    pub fn with_interval(files: TlsFiles, check_every: Duration) -> anyhow::Result<Self> {
        let stamps = files.stamps();
        let acceptor = TlsAcceptor::from(Arc::new(load(&files)?));
        Ok(Reloader {
            files,
            current: Mutex::new(Current { acceptor, stamps, checked: Instant::now() }),
            check_every,
        })
    }

    pub fn files(&self) -> &TlsFiles {
        &self.files
    }

    /// The acceptor for the next connection.
    pub fn acceptor(&self) -> TlsAcceptor {
        let mut cur = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if cur.checked.elapsed() >= self.check_every {
            cur.checked = Instant::now();
            let stamps = self.files.stamps();
            if stamps != cur.stamps {
                match load(&self.files) {
                    Ok(cfg) => {
                        cur.acceptor = TlsAcceptor::from(Arc::new(cfg));
                        cur.stamps = stamps;
                        tracing::info!(
                            "management TLS: {} re-read (renewed); new connections use it",
                            self.files.cert.display()
                        );
                    }
                    // Tried again at the next check: the files may be
                    // mid-write.
                    Err(e) => tracing::warn!(
                        "management TLS: the changed files do not load ({e}); still serving the previous pair"
                    ),
                }
            }
        }
        cur.acceptor.clone()
    }
}

/// The client certificate a finished handshake verified, if any.
pub fn client_cert<IO>(stream: &tokio_rustls::server::TlsStream<IO>) -> Option<ClientCert> {
    stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| ClientCert::from_der(c.as_ref()))
}
