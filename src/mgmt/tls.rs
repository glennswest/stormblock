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

/// A request that came over a connection whose client certificate proved
/// something: the node CA issued it (the node token's tier, #203), or forge's
/// CA issued it to an admin identity (#379). Put on every request of that
/// connection, read by `mgmt::auth`. A certificate that proves neither — one
/// forge issued to another node — is no credential and is not put on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCert {
    /// `sha256:<first 16 hex digits>` of the certificate: who, in the audit
    /// log.
    pub fingerprint: String,
    /// Verified against the node CA: the node token's tier.
    pub node: bool,
    /// Verified against forge's CA, not revoked by forge's CRL, and valid for
    /// this listed name (`tls_admin_names`): the admin tier.
    pub admin: Option<String>,
}

impl ClientCert {
    pub fn from_der(der: &[u8]) -> Self {
        use sha2::Digest;
        let d = sha2::Sha256::digest(der);
        let hex: String = d.iter().take(8).map(|b| format!("{b:02x}")).collect();
        ClientCert { fingerprint: format!("sha256:{hex}"), node: false, admin: None }
    }
}

/// What a client certificate proves (#379): the trust anchors of the node CA
/// and of forge's CA, forge's CRLs and the names that are admin. Built with
/// the acceptor, so a renewed CA or CRL is read the same way the pair is.
#[derive(Default)]
pub struct Classifier {
    node_cas: Vec<rustls::pki_types::CertificateDer<'static>>,
    admin_cas: Vec<rustls::pki_types::CertificateDer<'static>>,
    crls: Vec<webpki::CertRevocationList<'static>>,
    admin_names: Vec<String>,
}

impl Classifier {
    /// Whether `chain` (leaf first) verifies against `cas` for client auth,
    /// with `crls` when there are some.
    fn verifies(
        cas: &[rustls::pki_types::CertificateDer<'static>],
        crls: &[webpki::CertRevocationList<'static>],
        chain: &[rustls::pki_types::CertificateDer<'static>],
    ) -> Option<()> {
        let leaf = chain.first()?;
        let ee = webpki::EndEntityCert::try_from(leaf).ok()?;
        let anchors: Vec<_> = cas.iter().filter_map(|c| webpki::anchor_from_trusted_cert(c).ok()).collect();
        if anchors.is_empty() {
            return None;
        }
        let refs: Vec<&webpki::CertRevocationList<'_>> = crls.iter().collect();
        let revocation = if refs.is_empty() {
            None
        } else {
            Some(
                webpki::RevocationOptionsBuilder::new(&refs)
                    .ok()?
                    .with_depth(webpki::RevocationCheckDepth::EndEntity)
                    .with_status_policy(webpki::UnknownStatusPolicy::Deny)
                    .build(),
            )
        };
        let algs = rustls::crypto::ring::default_provider().signature_verification_algorithms.all;
        ee.verify_for_usage(
            algs,
            &anchors,
            &chain[1..],
            rustls::pki_types::UnixTime::now(),
            webpki::KeyUsage::client_auth(),
            revocation,
            None,
        )
        .ok()
        .map(|_| ())
    }

    /// What `chain` proves; `None` when nothing.
    pub fn classify(&self, chain: &[rustls::pki_types::CertificateDer<'static>]) -> Option<ClientCert> {
        let leaf = chain.first()?;
        let mut c = ClientCert::from_der(leaf.as_ref());
        c.node = !self.node_cas.is_empty() && Self::verifies(&self.node_cas, &[], chain).is_some();
        if !self.admin_cas.is_empty() && !self.admin_names.is_empty() && Self::verifies(&self.admin_cas, &self.crls, chain).is_some() {
            let ee = webpki::EndEntityCert::try_from(leaf).ok()?;
            c.admin = self.admin_names.iter().find(|n| {
                rustls::pki_types::ServerName::try_from(n.as_str())
                    .ok()
                    .is_some_and(|sn| ee.verify_is_valid_for_subject_name(&sn).is_ok())
            }).cloned();
        }
        (c.node || c.admin.is_some()).then_some(c)
    }
}

/// The files the listener serves from.
#[derive(Debug, Clone)]
pub struct TlsFiles {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub client_ca: Option<PathBuf>,
    /// Forge's CA (#379): a certificate it issued to a listed name is admin.
    pub admin_ca: Option<PathBuf>,
    /// Forge's CRL(s), PEM or DER (#379, stormcert#61).
    pub admin_crl: Option<PathBuf>,
    /// The SAN DNS names that are admin. None listed: no certificate is.
    pub admin_names: Vec<String>,
}

impl TlsFiles {
    fn paths(&self) -> Vec<&Path> {
        let mut v = vec![self.cert.as_path(), self.key.as_path()];
        for p in [&self.client_ca, &self.admin_ca, &self.admin_crl].into_iter().flatten() {
            v.push(p.as_path());
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

fn read_crls(path: &Path) -> anyhow::Result<Vec<webpki::CertRevocationList<'static>>> {
    let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("forge CRL {}: {e}", path.display()))?;
    let ders: Vec<Vec<u8>> = if bytes.starts_with(b"-----") {
        rustls_pemfile::crls(&mut std::io::BufReader::new(bytes.as_slice()))
            .map(|r| r.map(|c| c.as_ref().to_vec()))
            .collect::<Result<_, _>>()
            .map_err(|e| anyhow::anyhow!("forge CRL {}: {e}", path.display()))?
    } else {
        vec![bytes]
    };
    if ders.is_empty() {
        anyhow::bail!("no CRL in {}", path.display());
    }
    ders.iter()
        .map(|d| {
            webpki::BorrowedCertRevocationList::from_der(d)
                .and_then(|c| c.to_owned())
                .map(webpki::CertRevocationList::from)
                .map_err(|e| anyhow::anyhow!("forge CRL {}: {e:?}", path.display()))
        })
        .collect()
}

/// What client certificates prove, for these files (#379).
pub fn classifier(files: &TlsFiles) -> anyhow::Result<Classifier> {
    Ok(Classifier {
        node_cas: match &files.client_ca { Some(p) => read_certs(p)?, None => Vec::new() },
        admin_cas: match &files.admin_ca { Some(p) => read_certs(p)?, None => Vec::new() },
        crls: match &files.admin_crl { Some(p) => read_crls(p)?, None => Vec::new() },
        admin_names: files.admin_names.clone(),
    })
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
    // Either CA is a root the handshake accepts; what a certificate proves
    // is decided after it, against each CA alone (`Classifier`).
    let cas: Vec<&PathBuf> = [&files.client_ca, &files.admin_ca].into_iter().flatten().collect();
    let builder = if cas.is_empty() {
        builder.with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        for ca in &cas {
            for c in read_certs(ca)? {
                roots
                    .add(c)
                    .map_err(|e| anyhow::anyhow!("client CA {}: {e}", ca.display()))?;
            }
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            // Asked for, not required: the token and the probes still
            // connect without one.
            .allow_unauthenticated()
            .build()
            .map_err(|e| anyhow::anyhow!("client CAs: {e}"))?;
        builder.with_client_cert_verifier(verifier)
    };
    builder
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("invalid TLS configuration: {e}"))
}

/// How often a connection may look at the files.
const CHECK_EVERY: Duration = Duration::from_secs(5);

struct Current {
    acceptor: TlsAcceptor,
    classifier: Arc<Classifier>,
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
        let classifier = Arc::new(classifier(&files)?);
        Ok(Reloader {
            files,
            current: Mutex::new(Current { acceptor, classifier, stamps, checked: Instant::now() }),
            check_every,
        })
    }

    pub fn files(&self) -> &TlsFiles {
        &self.files
    }

    /// The acceptor for the next connection, and what its client certificate
    /// proves.
    pub fn acceptor(&self) -> (TlsAcceptor, Arc<Classifier>) {
        let mut cur = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if cur.checked.elapsed() >= self.check_every {
            cur.checked = Instant::now();
            let stamps = self.files.stamps();
            if stamps != cur.stamps {
                match load(&self.files).and_then(|cfg| Ok((cfg, classifier(&self.files)?))) {
                    Ok((cfg, cls)) => {
                        cur.acceptor = TlsAcceptor::from(Arc::new(cfg));
                        cur.classifier = Arc::new(cls);
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
        (cur.acceptor.clone(), cur.classifier.clone())
    }
}

/// What the client certificate of a finished handshake proves, if anything.
pub fn client_cert<IO>(stream: &tokio_rustls::server::TlsStream<IO>, classifier: &Classifier) -> Option<ClientCert> {
    let chain: Vec<rustls::pki_types::CertificateDer<'static>> =
        stream.get_ref().1.peer_certificates()?.iter().map(|c| c.clone().into_owned()).collect();
    classifier.classify(&chain)
}
