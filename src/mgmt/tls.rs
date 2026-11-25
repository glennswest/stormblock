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
//!
//! A configured file that is not there is "not yet" (#483): on a stormcos node
//! the engine serves the volumes stormcert runs from, so the pair, the node
//! CA, forge's CA and its CRL all appear after it starts. Each is taken up when
//! it appears, and `/api/v1/health` says which are still missing:
//!
//! | missing | meanwhile |
//! |---|---|
//! | `tls_cert` or `tls_key` | plain HTTP, as with no TLS configured; once a pair has been served, never plain again in this process |
//! | `tls_client_ca` | no certificate is a credential at the node token's tier |
//! | `tls_admin_ca` | no certificate is admin, nor a forge node |
//! | `tls_admin_crl` | forge's CA proves nothing: revocation cannot be checked, so no certificate is admin, nor a forge node (with the key unset, revocation is simply not checked) |
//!
//! A file that is there and does not parse is an error at start, as ever.

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
    /// Verified against forge's CA and not revoked, but not a listed admin:
    /// a node forge enrolled (#383, stormcert#78). It may only read a sealed
    /// volume's content and a release's manifest (`serve::api::forge_node_read`).
    pub forge_node: bool,
    /// Verified against the node CA, which is the apiserver's client CA too
    /// (stormcos#572): who the apiserver takes it for (#731). A destructive
    /// call it makes is reviewed for that user and those groups, so RBAC
    /// decides whether it is admin.
    pub kube: Option<super::kubeauth::KubeUser>,
}

impl ClientCert {
    pub fn from_der(der: &[u8]) -> Self {
        use sha2::Digest;
        let d = sha2::Sha256::digest(der);
        let hex: String = d.iter().take(8).map(|b| format!("{b:02x}")).collect();
        ClientCert { fingerprint: format!("sha256:{hex}"), node: false, admin: None, forge_node: false, kube: None }
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
    admin_subjects: Vec<String>,
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
        if c.node {
            c.kube = kube_identity(leaf.as_ref());
        }
        if !self.admin_cas.is_empty() && Self::verifies(&self.admin_cas, &self.crls, chain).is_some() {
            let ee = webpki::EndEntityCert::try_from(leaf).ok()?;
            c.admin = self
                .admin_names
                .iter()
                .find(|n| {
                    rustls::pki_types::ServerName::try_from(n.as_str())
                        .ok()
                        .is_some_and(|sn| ee.verify_is_valid_for_subject_name(&sn).is_ok())
                })
                .cloned()
                // By subject (#453): a forge client certificate carries no SAN
                // (stormcert#117), and is named by its one CN, as a node's
                // apiserver reads it.
                .or_else(|| {
                    let cn = subject_cn(leaf.as_ref())?;
                    self.admin_subjects.iter().find(|s| **s == cn && !cn.starts_with("system:")).cloned()
                });
            c.forge_node = c.admin.is_none();
        }
        (c.node || c.admin.is_some() || c.forge_node).then_some(c)
    }
}

/// The subject's common name of a DER certificate, when it has exactly one
/// (#453). Read only from a certificate the chain check has already passed.
/// A name that is not a string type, or two CNs, gives `None`: nothing to
/// match.
pub fn subject_cn(der: &[u8]) -> Option<String> {
    let mut cns = subject_values(der, OID_CN)?;
    (cns.len() == 1).then(|| cns.remove(0))
}

/// Who a client certificate is to the node's apiserver (#731): the user its
/// subject's one CN, the groups its O values, as kube-apiserver's x509
/// authenticator reads them, plus `system:authenticated`, which the apiserver
/// adds to every authenticated user. `None` without one CN, or with an O
/// that is not a string.
pub fn kube_identity(der: &[u8]) -> Option<super::kubeauth::KubeUser> {
    let username = subject_cn(der)?;
    let mut groups = subject_values(der, OID_O)?;
    groups.retain(|g| g != "system:authenticated");
    groups.push("system:authenticated".into());
    Some(super::kubeauth::KubeUser { username, groups })
}

/// One DER TLV at `at`: (tag, start of contents, end of contents).
fn der_tlv(b: &[u8], at: usize) -> Option<(u8, usize, usize)> {
    let tag = *b.get(at)?;
    let first = *b.get(at + 1)? as usize;
    let (len, start) = if first < 0x80 {
        (first, at + 2)
    } else {
        let n = first & 0x7F;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *b.get(at + 2 + i)? as usize;
        }
        (len, at + 2 + n)
    };
    let end = start.checked_add(len)?;
    (end <= b.len()).then_some((tag, start, end))
}

/// The names a serving certificate is valid for (#662): its subject
/// alternative names, DNS and IP, in the certificate's order.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct Sans {
    pub dns: Vec<String>,
    pub ip: Vec<std::net::IpAddr>,
}

const OID_SAN: &[u8] = &[0x55, 0x1D, 0x11];

/// A DER certificate's subject alternative names (#662). The walk:
/// Certificate → TBSCertificate → past (version) serial, signature, issuer,
/// validity, subject, key, the optional unique IDs → `[3]` extensions →
/// 2.5.29.17 → each GeneralName: `[2]` dNSName, `[7]` iPAddress (4 or 16
/// bytes). Other kinds are left out. `None` when the DER does not parse;
/// empty lists when the certificate has no SAN extension.
pub fn subject_alt_names(der: &[u8]) -> Option<Sans> {
    let tlv = der_tlv;
    const SEQ: u8 = 0x30;
    let (t, cert, _) = tlv(der, 0)?;
    if t != SEQ {
        return None;
    }
    let (t, tbs, tbs_end) = tlv(der, cert)?;
    if t != SEQ {
        return None;
    }
    let mut at = tbs;
    if tlv(der, at)?.0 == 0xA0 {
        at = tlv(der, at)?.2;
    }
    // serial, signature, issuer, validity, subject, subjectPublicKeyInfo.
    for _ in 0..6 {
        at = tlv(der, at)?.2;
    }
    let mut sans = Sans::default();
    while at < tbs_end {
        let (t, c, end) = tlv(der, at)?;
        at = end;
        if t != 0xA3 {
            continue; // [1], [2]: the unique IDs.
        }
        let (t, mut ext, exts_end) = tlv(der, c)?;
        if t != SEQ {
            return None;
        }
        while ext < exts_end {
            let (t, e, e_end) = tlv(der, ext)?;
            ext = e_end;
            if t != SEQ {
                return None;
            }
            let (t, o, o_end) = tlv(der, e)?;
            if t != 0x06 || &der[o..o_end] != OID_SAN {
                continue;
            }
            // critical BOOLEAN, if there; then the OCTET STRING.
            let mut v = o_end;
            if tlv(der, v)?.0 == 0x01 {
                v = tlv(der, v)?.2;
            }
            let (t, os, _) = tlv(der, v)?;
            if t != 0x04 {
                return None;
            }
            let (t, mut gn, gns_end) = tlv(der, os)?;
            if t != SEQ {
                return None;
            }
            while gn < gns_end {
                let (t, vs, ve) = tlv(der, gn)?;
                gn = ve;
                let b = &der[vs..ve];
                match t {
                    0x82 => sans.dns.push(std::str::from_utf8(b).ok()?.to_string()),
                    0x87 => match b.len() {
                        4 => sans.ip.push(std::net::IpAddr::from(<[u8; 4]>::try_from(b).ok()?)),
                        16 => sans.ip.push(std::net::IpAddr::from(<[u8; 16]>::try_from(b).ok()?)),
                        _ => return None,
                    },
                    _ => {}
                }
            }
        }
    }
    Some(sans)
}

/// Interfaces whose addresses are no caller's way to the node (#662): pods',
/// overlays', kube-proxy's IPVS (every ClusterIP). The same list stormcert's
/// `serving --node-names` leaves out (stormcert#173), so what the engine
/// calls uncovered is what a renewal would add.
const NOT_THE_NODES: &[&str] = &["lo", "cilium", "lxc", "veth", "flowsdn", "kube-ipvs", "docker", "cni", "flannel"];

/// Whether a caller off the node could reach it on `addr` (#662): a global
/// unicast address, not loopback, link-local, multicast, unspecified or
/// IPv4-mapped, on an interface that is the node's own.
pub fn callers_address(interface: &str, loopback: bool, addr: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    if loopback || NOT_THE_NODES.iter().any(|p| interface.starts_with(p)) {
        return false;
    }
    match addr {
        IpAddr::V4(a) => !(a.is_loopback() || a.is_link_local() || a.is_multicast() || a.is_unspecified() || a.is_broadcast()),
        IpAddr::V6(a) => {
            let s = a.segments();
            !(a.is_loopback()
                || a.is_multicast()
                || a.is_unspecified()
                || (s[0] & 0xffc0) == 0xfe80
                || a.to_ipv4_mapped().is_some())
        }
    }
}

/// The node's addresses (as [`callers_address`] counts them) that `sans`
/// does not name, sorted, each once.
pub fn uncovered(sans: &Sans, addrs: &[crate::netif::IfAddr]) -> Vec<std::net::IpAddr> {
    let mut v: Vec<std::net::IpAddr> = addrs
        .iter()
        .filter(|a| callers_address(&a.interface, a.loopback, a.address))
        .map(|a| a.address)
        .filter(|a| !sans.ip.contains(a))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// The SANs of the first certificate in the PEM file at `path`; `None` when
/// it is not there or does not parse.
fn served_names(path: &Path) -> Option<Sans> {
    let certs = read_certs(path).ok()??;
    subject_alt_names(certs.first()?.as_ref())
}

const OID_CN: &[u8] = &[0x55, 0x04, 0x03];
const OID_O: &[u8] = &[0x55, 0x04, 0x0A];

/// Every value of attribute `oid` in a DER certificate's subject, in order.
/// The walk is plain: Certificate → TBSCertificate → (version) serial,
/// signature, issuer, validity → subject → each RDN's attributes. `None`
/// when the DER does not parse, or a value of `oid` is not a string type.
fn subject_values(der: &[u8], oid: &[u8]) -> Option<Vec<String>> {
    let tlv = der_tlv;
    const SEQ: u8 = 0x30;
    const SET: u8 = 0x31;
    let (t, cert, _) = tlv(der, 0)?;
    if t != SEQ {
        return None;
    }
    let (t, tbs, tbs_end) = tlv(der, cert)?;
    if t != SEQ {
        return None;
    }
    let mut at = tbs;
    let (t, _, end) = tlv(der, at)?;
    if t == 0xA0 {
        at = end; // [0] version
    }
    // serial, signature, issuer, validity: skipped; then the subject.
    for _ in 0..4 {
        at = tlv(der, at)?.2;
    }
    let (t, mut rdn, subj_end) = tlv(der, at)?;
    if t != SEQ || subj_end > tbs_end {
        return None;
    }
    let mut found = Vec::new();
    while rdn < subj_end {
        let (t, mut atv, set_end) = tlv(der, rdn)?;
        if t != SET {
            return None;
        }
        while atv < set_end {
            let (t, inner, atv_end) = tlv(der, atv)?;
            if t != SEQ {
                return None;
            }
            let (t, o, o_end) = tlv(der, inner)?;
            if t == 0x06 && &der[o..o_end] == oid {
                let (st, vs, ve) = tlv(der, o_end)?;
                // UTF8String, PrintableString, IA5String, T61String.
                if !matches!(st, 0x0C | 0x13 | 0x16 | 0x14) {
                    return None;
                }
                found.push(std::str::from_utf8(&der[vs..ve]).ok()?.to_string());
            }
            atv = atv_end;
        }
        rdn = set_end;
    }
    Some(found)
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
    /// The SAN DNS names that are admin.
    pub admin_names: Vec<String>,
    /// The subject CNs that are admin (#453): a forge client certificate's
    /// name. With `admin_names` empty too, no certificate is.
    pub admin_subjects: Vec<String>,
}

impl TlsFiles {
    /// Every configured file, by its key in `[management]`.
    fn named(&self) -> Vec<(&'static str, &Path)> {
        let mut v = vec![("tls_cert", self.cert.as_path()), ("tls_key", self.key.as_path())];
        for (name, p) in [
            ("tls_client_ca", &self.client_ca),
            ("tls_admin_ca", &self.admin_ca),
            ("tls_admin_crl", &self.admin_crl),
        ] {
            if let Some(p) = p {
                v.push((name, p.as_path()));
            }
        }
        v
    }

    fn stamps(&self) -> Vec<Option<SystemTime>> {
        self.named()
            .into_iter()
            .map(|(_, p)| std::fs::metadata(p).and_then(|m| m.modified()).ok())
            .collect()
    }

    /// The keys of `stamps` whose file is not there.
    fn missing(&self, stamps: &[Option<SystemTime>]) -> Vec<&'static str> {
        self.named().into_iter().zip(stamps).filter(|(_, s)| s.is_none()).map(|((name, _), _)| name).collect()
    }
}

/// What a missing file means until it appears (#483), for the log.
fn meanwhile(key: &str) -> &'static str {
    match key {
        "tls_cert" | "tls_key" => "the API serves plain HTTP until the pair is there",
        "tls_client_ca" => "no client certificate is a credential at the node token's tier until it is there",
        "tls_admin_ca" => "no certificate is admin until it is there",
        _ => "forge's revocations cannot be checked: no certificate from forge's CA proves anything until it is there",
    }
}

/// The bytes of `path`; `None` when it is not there yet (#483). Anything else
/// that stops the read (permissions, an I/O error) is an error.
fn read_present(path: &Path, what: &str) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("failed to open {what} '{}': {e}", path.display())),
    }
}

type Certs = Vec<rustls::pki_types::CertificateDer<'static>>;

fn read_certs(path: &Path) -> anyhow::Result<Option<Certs>> {
    let Some(bytes) = read_present(path, "TLS cert")? else { return Ok(None) };
    let certs: Vec<_> = rustls_pemfile::certs(&mut bytes.as_slice())
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("failed to parse certificates in {}: {e}", path.display()))?;
    if certs.is_empty() {
        anyhow::bail!("no certificates found in {}", path.display());
    }
    Ok(Some(certs))
}

fn read_key(path: &Path) -> anyhow::Result<Option<rustls::pki_types::PrivateKeyDer<'static>>> {
    let Some(bytes) = read_present(path, "TLS key")? else { return Ok(None) };
    rustls_pemfile::private_key(&mut bytes.as_slice())
        .map_err(|e| anyhow::anyhow!("failed to parse TLS key: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {}", path.display()))
        .map(Some)
}

fn read_crls(path: &Path) -> anyhow::Result<Option<Vec<webpki::CertRevocationList<'static>>>> {
    let Some(bytes) = read_present(path, "forge CRL")? else { return Ok(None) };
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
        .collect::<anyhow::Result<Vec<_>>>()
        .map(Some)
}

/// What client certificates prove, for these files (#379).
pub fn classifier(files: &TlsFiles) -> anyhow::Result<Classifier> {
    let node_cas = match &files.client_ca { Some(p) => read_certs(p)?.unwrap_or_default(), None => Vec::new() };
    let admin_cas = match &files.admin_ca { Some(p) => read_certs(p)?.unwrap_or_default(), None => Vec::new() };
    let crls = match &files.admin_crl { Some(p) => read_crls(p)?, None => Some(Vec::new()) };
    // A CRL configured and not held fails closed (#483): whether a certificate
    // from forge's CA is revoked cannot be told, so it proves nothing, the
    // forge-node tier included. With no CRL configured it is the other way:
    // an operator's choice, warned of at start.
    let (admin_cas, crls) = match crls {
        Some(crls) => (admin_cas, crls),
        None => (Vec::new(), Vec::new()),
    };
    Ok(Classifier {
        node_cas,
        admin_cas,
        crls,
        admin_names: files.admin_names.clone(),
        admin_subjects: files.admin_subjects.clone(),
    })
}

/// The server configuration for these files; `None` while the pair is not
/// all there (#483). Whatever is there is parsed either way.
pub fn load(files: &TlsFiles) -> anyhow::Result<Option<ServerConfig>> {
    let certs = read_certs(&files.cert)?;
    let key = read_key(&files.key)?;
    let mut cas = Vec::new();
    for ca in [&files.client_ca, &files.admin_ca].into_iter().flatten() {
        if let Some(certs) = read_certs(ca)? {
            cas.push((ca, certs));
        }
    }
    let (Some(certs), Some(key)) = (certs, key) else { return Ok(None) };

    crate::http::ensure_crypto_provider();
    let builder = ServerConfig::builder();
    // Either CA is a root the handshake accepts; what a certificate proves
    // is decided after it, against each CA alone (`Classifier`).
    let builder = if cas.is_empty() {
        builder.with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        for (ca, certs) in cas {
            for c in certs {
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
        .map(Some)
        .map_err(|e| anyhow::anyhow!("invalid TLS configuration: {e}"))
}

/// How often a connection may look at the files.
const CHECK_EVERY: Duration = Duration::from_secs(5);

struct Current {
    /// `None` until a pair has been there (#483): the API is plain.
    acceptor: Option<TlsAcceptor>,
    classifier: Arc<Classifier>,
    stamps: Vec<Option<SystemTime>>,
    missing: Vec<&'static str>,
    checked: Instant,
    /// The served leaf's SANs (#662); `None` while no pair is served, or its
    /// certificate's SANs could not be read.
    names: Option<Sans>,
    /// The node's addresses the served certificate does not name, as last
    /// looked at (#662).
    uncovered: Vec<std::net::IpAddr>,
    /// `uncovered` has been looked at for this pair (and said).
    addresses_judged: bool,
}

/// What `/api/v1/health` says of the management TLS (#483).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TlsStatus {
    /// What the listener serves new connections: `https` or `http`.
    pub serving: &'static str,
    /// The configured keys whose file is not there yet.
    pub missing: Vec<&'static str>,
    /// `required`, or `optional` (#525): plain is taken beside TLS.
    pub mode: &'static str,
    /// The served certificate's subject alternative names (#662): a client
    /// on an address or name not here fails its name check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub names: Option<Sans>,
    /// The node's own addresses (global unicast, on its own interfaces) the
    /// served certificate does not name (#662): a caller on one of them is
    /// refused by its own name check. Empty when every one is named.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub uncovered: Vec<std::net::IpAddr>,
}

/// The acceptor, re-made when the files change.
pub struct Reloader {
    files: TlsFiles,
    current: Mutex<Current>,
    check_every: Duration,
    /// Moves when what an open connection was judged by no longer holds: the
    /// first pair (it was plain), or a CA or CRL changed (its certificate was
    /// judged at its handshake, and a CA that was not there asked for none).
    /// Open connections end after their request in flight and come back.
    judged: tokio::sync::watch::Sender<u64>,
    optional: bool,
    plain: Arc<super::plain::PlainTally>,
}

impl Reloader {
    /// Fails when a file that is there does not load: a node told to serve
    /// TLS must not start serving plain HTTP over a pair it cannot read. A
    /// file that is not there yet is no failure (#483), and is said.
    pub fn new(files: TlsFiles) -> anyhow::Result<Self> {
        Self::with_interval(files, CHECK_EVERY)
    }

    pub fn with_interval(files: TlsFiles, check_every: Duration) -> anyhow::Result<Self> {
        let stamps = files.stamps();
        let missing = files.missing(&stamps);
        let acceptor = load(&files)?.map(|cfg| TlsAcceptor::from(Arc::new(cfg)));
        let classifier = Arc::new(classifier(&files)?);
        for key in &missing {
            tracing::warn!("management TLS: {key} is configured and not there yet: {}", meanwhile(key));
        }
        let (judged, _) = tokio::sync::watch::channel(0);
        let names = if acceptor.is_some() { served_names(&files.cert) } else { None };
        let r = Reloader {
            files,
            current: Mutex::new(Current {
                acceptor,
                classifier,
                stamps,
                missing,
                checked: Instant::now(),
                names,
                uncovered: Vec::new(),
                addresses_judged: false,
            }),
            check_every,
            judged,
            optional: false,
            plain: Arc::new(super::plain::PlainTally::new(None)),
        };
        r.look_at_addresses();
        Ok(r)
    }

    /// Whether the served certificate names every address the node holds
    /// (#662): looked at with the files, said when that changes. Nothing is
    /// held while the kernel's list is read.
    fn look_at_addresses(&self) {
        let Some(names) = self.lock().names.clone() else { return };
        let now = uncovered(&names, &crate::netif::interface_addresses());
        let mut cur = self.lock();
        if cur.names.as_ref() != Some(&names) || (cur.addresses_judged && now == cur.uncovered) {
            return;
        }
        cur.addresses_judged = true;
        if now.is_empty() {
            tracing::info!("management TLS: the served certificate names every address this node holds (#662)");
        } else {
            let list: Vec<String> = now.iter().map(|a| a.to_string()).collect();
            let has: Vec<String> = names.ip.iter().map(|a| a.to_string()).chain(names.dns.iter().cloned()).collect();
            tracing::warn!(
                "management TLS: the served certificate does not name {} (it names {}): a client on {} fails its name check; \
                 stormcert's `serving --node-names` re-issues the pair with every address, and the engine takes it up when the files change (#662)",
                list.join(", "),
                has.join(", "),
                if now.len() == 1 { "it" } else { "them" }
            );
        }
        cur.uncovered = now;
    }

    /// `tls_mode` (#525), and where the first plain request of a caller and
    /// route is written down.
    pub fn with_mode(mut self, optional: bool, audit: Option<std::path::PathBuf>) -> Self {
        self.optional = optional;
        self.plain = Arc::new(super::plain::PlainTally::new(audit));
        self
    }

    /// Whether plain is taken beside TLS once there is a pair.
    pub fn optional(&self) -> bool {
        self.optional
    }

    /// What counts this listener's plain requests.
    pub fn plain(&self) -> Arc<super::plain::PlainTally> {
        self.plain.clone()
    }

    pub fn files(&self) -> &TlsFiles {
        &self.files
    }

    /// How often the files are looked at.
    pub fn check_every(&self) -> Duration {
        self.check_every
    }

    /// Whether new connections are served TLS.
    pub fn serving_tls(&self) -> bool {
        self.lock().acceptor.is_some()
    }

    /// For one connection, taken before its `acceptor()`: changes when the
    /// connection should end.
    pub fn watch_judged(&self) -> tokio::sync::watch::Receiver<u64> {
        self.judged.subscribe()
    }

    pub fn status(&self) -> TlsStatus {
        let cur = self.lock();
        TlsStatus {
            serving: if cur.acceptor.is_some() { "https" } else { "http" },
            missing: cur.missing.clone(),
            mode: if self.optional { "optional" } else { "required" },
            names: cur.names.clone(),
            uncovered: cur.uncovered.clone(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Current> {
        self.current.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Look at the files if it is time to. Nothing is held while they are
    /// read: on a node they live on a volume this engine serves, and health
    /// must answer whatever that volume is doing.
    pub fn refresh(&self) {
        let before = {
            let mut cur = self.lock();
            if cur.checked.elapsed() < self.check_every {
                return;
            }
            cur.checked = Instant::now();
            cur.stamps.clone()
        };
        self.reload_files(before);
        self.look_at_addresses();
    }

    /// The files read again when one changed since `before`.
    fn reload_files(&self, before: Vec<Option<SystemTime>>) {
        let stamps = self.files.stamps();
        let missing = self.files.missing(&stamps);
        if stamps == before {
            return;
        }
        let loaded = load(&self.files).and_then(|cfg| Ok((cfg, classifier(&self.files)?)));
        let names = served_names(&self.files.cert);
        let mut cur = self.lock();
        let was = std::mem::replace(&mut cur.missing, missing.clone());
        let (cfg, cls) = match loaded {
            Ok(l) => l,
            // Tried again at the next check: the files may be mid-write.
            Err(e) => {
                tracing::warn!(
                    "management TLS: the changed files do not load ({e}); still serving what was loaded before"
                );
                return;
            }
        };
        cur.classifier = Arc::new(cls);
        // A renewed pair alone judges nobody differently; the first two stamps
        // are the pair's (`named`).
        let mut end_open = before[2..] != stamps[2..];
        cur.stamps = stamps;
        for key in was.iter().filter(|k| !missing.contains(k)) {
            tracing::info!("management TLS: {key} is there now and taken up");
        }
        for key in missing.iter().filter(|k| !was.contains(k)) {
            tracing::warn!("management TLS: {key} is gone: {}", meanwhile(key));
        }
        match cfg {
            Some(cfg) => {
                let first = cur.acceptor.is_none();
                cur.acceptor = Some(TlsAcceptor::from(Arc::new(cfg)));
                // A new pair is judged afresh against the node's addresses.
                cur.names = names;
                cur.uncovered.clear();
                cur.addresses_judged = false;
                if first {
                    tracing::info!(
                        "management TLS: {} is there: new connections are HTTPS, plain ones end after their request",
                        self.files.cert.display()
                    );
                    end_open = true;
                } else {
                    tracing::info!(
                        "management TLS: {} re-read (renewed); new connections use it",
                        self.files.cert.display()
                    );
                }
            }
            // Never back to plain: what was served stays served.
            None if cur.acceptor.is_some() => tracing::warn!(
                "management TLS: the pair's files are gone; still serving the pair held in memory"
            ),
            None => {}
        }
        if end_open {
            self.judged.send_modify(|n| *n += 1);
        }
    }

    /// The acceptor for the next connection and what its client certificate
    /// proves; `None` while there is no pair (#483): serve it plain.
    pub fn acceptor(&self) -> Option<(TlsAcceptor, Arc<Classifier>)> {
        self.refresh();
        let cur = self.lock();
        cur.acceptor.clone().map(|a| (a, cur.classifier.clone()))
    }
}

/// What the client certificate of a finished handshake proves, if anything.
pub fn client_cert<IO>(stream: &tokio_rustls::server::TlsStream<IO>, classifier: &Classifier) -> Option<ClientCert> {
    let chain: Vec<rustls::pki_types::CertificateDer<'static>> =
        stream.get_ref().1.peer_certificates()?.iter().map(|c| c.clone().into_owned()).collect();
    classifier.classify(&chain)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netif::IfAddr;

    fn a(i: &str, addr: &str) -> IfAddr {
        IfAddr { interface: i.into(), address: addr.parse().unwrap(), prefix: 24, loopback: false }
    }

    #[test]
    fn a_callers_address_is_global_unicast_on_the_nodes_own_interface() {
        let ok = |i: &str, s: &str| callers_address(i, false, s.parse().unwrap());
        assert!(ok("eno1", "192.168.31.203"));
        assert!(ok("eno1.100", "10.1.2.3"));
        assert!(ok("eno1", "2001:db8::1"));
        assert!(!ok("eno1", "169.254.1.1"), "link-local v4");
        assert!(!ok("eno1", "fe80::1"), "link-local v6");
        assert!(!ok("eno1", "::ffff:192.0.2.1"), "v4-mapped");
        assert!(!ok("eno1", "127.0.0.2"));
        assert!(!ok("eno1", "224.0.0.1"));
        for pod in ["lo", "lxc12", "veth3", "cilium_host", "flowsdn0", "kube-ipvs0", "docker0", "cni0", "flannel.1"] {
            assert!(!ok(pod, "10.244.0.5"), "{pod}");
        }
        assert!(!callers_address("eno1", true, "192.0.2.1".parse().unwrap()), "IFF_LOOPBACK");
    }

    #[test]
    fn uncovered_is_what_the_sans_miss_sorted_once() {
        let sans = Sans { dns: vec!["forge1-p2".into()], ip: vec!["192.168.31.204".parse().unwrap()] };
        let addrs = [a("eno1", "192.168.31.204"), a("eno2", "192.168.31.203"), a("eno3", "192.168.31.203"), a("lxc1", "10.0.0.9")];
        assert_eq!(uncovered(&sans, &addrs), vec!["192.168.31.203".parse::<std::net::IpAddr>().unwrap()]);
    }

    #[test]
    fn subject_alt_names_of_garbage_is_none() {
        assert_eq!(subject_alt_names(&[]), None);
        assert_eq!(subject_alt_names(&[0x30, 0x03, 0x30, 0x01, 0x00]), None);
    }
}

