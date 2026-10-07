//! Synonyms — a stable name that points at a volume, and can be re-pointed.
//!
//! A consumer refers to storage by a name it chose once: `fedora-43`,
//! `sbregistry/nginx`, `node-root`. What that name should resolve to changes
//! — a new golden is imported, a clone is promoted, a version is rolled back
//! — and every consumer holding the old uuid has no way to learn that, while
//! every consumer holding only a *name* has no way to know whether what it
//! resolved yesterday is still what the name means today.
//!
//! A synonym is the binding, kept apart from the volume on purpose:
//!
//! - **It is a name record, not a volume.** A volume is extents; a synonym is
//!   a pointer. Making the alias a volume would give it slots, a redundancy
//!   policy and a place in the GEM, none of which it has any business having,
//!   and would make "delete the alias" ambiguous with "delete the data".
//! - **It is a mutable pointer with a version.** Re-pointing is the normal
//!   operation, and every re-point bumps a monotonic `version`. A client that
//!   remembers the version it resolved can ask whether the answer changed
//!   without re-reading the target, which is the whole point: the name is
//!   stable, so *something* has to carry the change.
//! - **The target may be elsewhere.** `Target::Volume` is a volume on this
//!   node; `Target::Remote` is a URI another node serves (`nvme-tcp://…`).
//!   Resolution says which, so a caller learns it is being sent off-node
//!   rather than finding out when the I/O is slow.
//!
//! What a synonym deliberately does not do is pin. It resolves to whatever it
//! points at *now*; a consumer that must not be moved under its feet records
//! the `(version, target)` it resolved and compares on its next start.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::extent::VolumeId;

const SYNONYMS_FILE: &str = "synonyms.json";

/// The default namespace, for callers that do not care about them.
pub const DEFAULT_NAMESPACE: &str = "default";

/// The namespace a machine's boot image is assigned in, by host name.
pub const BOOTHOST_NS: &str = "boothost";
/// Each host's own sealed golden, by host name. Kept by the engine, never
/// by a caller.
pub const HOSTGOLDEN_NS: &str = "hostgolden";
/// What a host seen for the first time boots (stormbootx#15).
pub const DEFAULT_HOST: &str = "default";

/// What a synonym points at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// A volume on this node.
    Volume { id: VolumeId },
    /// Storage another node serves. Held as the attach URI the drive layer
    /// already accepts (`nvme-tcp://host:port/<nqn>?nsid=N`), so resolving a
    /// synonym and attaching what it names are the same vocabulary.
    Remote { uri: String },
}

impl Target {
    pub fn volume_id(&self) -> Option<VolumeId> {
        match self {
            Target::Volume { id } => Some(*id),
            Target::Remote { .. } => None,
        }
    }

    pub fn as_str(&self) -> String {
        match self {
            Target::Volume { id } => id.0.to_string(),
            Target::Remote { uri } => uri.clone(),
        }
    }
}

/// A name that resolves to a volume, and the history of what it meant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Synonym {
    pub namespace: String,
    pub name: String,
    pub target: Target,
    /// Bumped on every re-point, never reused, never lowered. A client that
    /// holds a version can ask "still this?" in one call.
    pub version: u64,
    /// Free-form, for the version the *content* is: an image tag, a build id.
    /// The engine never interprets it — it is what a consumer wanted to
    /// record about what it pointed the name at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// What it pointed at before, most recent first, capped. Kept so a
    /// rollback is a lookup rather than an archaeology exercise.
    #[serde(default)]
    pub history: Vec<Previous>,
    pub created_at: u64,
    pub updated_at: u64,
}

/// One earlier meaning of a name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Previous {
    pub target: Target,
    pub version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// When this target stopped being what the name meant.
    pub replaced_at: u64,
}

/// How many earlier targets a synonym remembers. Enough to roll back a bad
/// publish and see the shape of recent ones; not an audit log, which belongs
/// somewhere that is not reloaded into memory on every start.
const HISTORY: usize = 16;

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Why a synonym operation was refused.
#[derive(Debug)]
pub enum SynonymError {
    NotFound(String),
    Exists(String),
    InvalidName(String),
    /// A rollback with nothing to roll back to.
    NoHistory(String),
    /// Two hosts would share a name or an alias (#199). The message names
    /// both.
    Conflict(String),
}

impl std::fmt::Display for SynonymError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SynonymError::NotFound(k) => write!(f, "no synonym {k}"),
            SynonymError::Exists(k) => write!(f, "synonym {k} already exists"),
            SynonymError::InvalidName(why) => write!(f, "invalid synonym name: {why}"),
            SynonymError::NoHistory(k) => {
                write!(f, "synonym {k} has no earlier target to roll back to")
            }
            SynonymError::Conflict(why) => write!(f, "{why}"),
        }
    }
}

impl std::error::Error for SynonymError {}

/// A namespaced name, as one key.
pub fn key(namespace: &str, name: &str) -> String {
    format!("{namespace}/{name}")
}

/// Split `ns/name`, or `name` in the default namespace.
///
/// A name may not contain a slash, so the split is unambiguous in both
/// directions — `default/nginx` and `nginx` are the same synonym.
pub fn split(key: &str) -> (String, String) {
    match key.split_once('/') {
        Some((ns, name)) => (ns.to_string(), name.to_string()),
        None => (DEFAULT_NAMESPACE.to_string(), key.to_string()),
    }
}

/// A name is a name: no slashes (they separate the namespace), no
/// whitespace, and not empty. Deliberately not a uuid either — a synonym
/// whose name parses as a uuid would shadow the volume with that id
/// everywhere a caller may pass "an id or a name".
fn check_name(namespace: &str, name: &str) -> Result<(), SynonymError> {
    for (what, s) in [("namespace", namespace), ("name", name)] {
        if s.is_empty() {
            return Err(SynonymError::InvalidName(format!("{what} must not be empty")));
        }
        if s.contains('/') {
            return Err(SynonymError::InvalidName(format!("{what} must not contain '/'")));
        }
        if s.chars().any(char::is_whitespace) {
            return Err(SynonymError::InvalidName(format!("{what} must not contain whitespace")));
        }
    }
    if name.parse::<uuid::Uuid>().is_ok() {
        return Err(SynonymError::InvalidName(format!(
            "{name} is a uuid, and would shadow the volume with that id"
        )));
    }
    Ok(())
}

/// A machine, by the name it is known by (#199).
///
/// The name is the host's DNS name, and is the key of its `boothost/<name>`
/// assignment and `hostgolden/<name>` golden. Aliases are the other things a
/// machine may claim as — its SMBIOS serial, its MACs — and resolve to the
/// same host. Nothing becomes an alias by itself: MicroCloud nodes share a
/// chassis serial, so the serial a claim arrives with says nothing on its own
/// about which machine it is.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Host {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Names the host had before a rename. Its boot clones and host goldens
    /// carry the name they were made under, so collecting them has to know
    /// every name the host has had.
    #[serde(default)]
    pub former_names: Vec<String>,
    /// What the machine's boot agent does before it claims (#148).
    #[serde(default, skip_serializing_if = "BootIntent::is_auto")]
    pub intent: BootIntent,
    /// The boot clone a claim handed out while the intent was `install`: the
    /// one install whose completion may set the intent back to `local`.
    /// Cleared whenever the intent is set, so a report from an install that
    /// began before the request never answers for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_claim: Option<VolumeId>,
    /// Whether the machine must prove its boot with a TPM 2.0 quote (#216).
    /// Set by the platform or an admin, never by the machine: a node that
    /// could write it could downgrade its own attestation. Unset reads as
    /// `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<TpmMark>,
    /// When `tpm` was last set or cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm_set_at: Option<u64>,
    /// The last boot claim served to this machine (#216): what stormcert's
    /// boot-chain attestation reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_claim: Option<ClaimRecord>,
    #[serde(default)]
    pub created_at: u64,
    #[serde(default)]
    pub updated_at: u64,
}

/// What a machine's boot agent does before it claims (#148, stormbootx#11).
///
/// Read by firmware from `GET /api/v1/synonyms/boothost/<name>/intent`, which
/// treats any doubt — a 404, an error, a word it does not know — as `auto`,
/// so nothing here can keep a machine from booting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootIntent {
    /// Claim and boot the image, as a machine always has.
    #[default]
    Auto,
    /// Claim, boot and install over the local disk: its system half laid
    /// again, its data half kept (#311); a disk with no data slab is laid
    /// fresh. One-shot: set back to `local` once the node reports the
    /// flow-over done. There is no `upgrade` (#234): a release change is
    /// staged on the running node (#122), not decided at boot.
    Install,
    /// Boot the local disk at once: no claim, no clone.
    Local,
}

impl BootIntent {
    pub fn is_auto(&self) -> bool {
        *self == BootIntent::Auto
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            BootIntent::Auto => "auto",
            BootIntent::Install => "install",
            BootIntent::Local => "local",
        }
    }
}

impl std::str::FromStr for BootIntent {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(BootIntent::Auto),
            "install" => Ok(BootIntent::Install),
            "local" => Ok(BootIntent::Local),
            other => Err(format!("intent {other:?}: expected install, local or auto")),
        }
    }
}

/// A machine's TPM mark (#216): what its attestation must carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TpmMark {
    /// A TPM 2.0 quote is mandatory; the boot chain alone is refused.
    Required,
    /// The machine has no TPM: the boot chain is the evidence.
    None,
}

impl TpmMark {
    pub fn as_str(&self) -> &'static str {
        match self {
            TpmMark::Required => "required",
            TpmMark::None => "none",
        }
    }
}

impl std::str::FromStr for TpmMark {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "required" => Ok(TpmMark::Required),
            "none" => Ok(TpmMark::None),
            other => Err(format!("tpm {other:?}: expected required or none")),
        }
    }
}

/// One boot claim, as the engine served it (#216). Written by the claim path
/// only: the machine causes it by claiming, it never supplies any of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRecord {
    /// The boot clone handed out.
    pub clone: VolumeId,
    pub clone_name: String,
    pub claimed_at: u64,
    /// What the machine claimed as: its name, an alias, or `default`.
    pub claimed_as: String,
    /// The host NQNs the clone was bound to (#210). Empty when the node has
    /// no NVMe/TCP listener to serve it on.
    #[serde(default)]
    pub host_nqns: Vec<String>,
    /// The machine's own sealed golden the clone was made from.
    pub host_golden: VolumeId,
    /// The golden `boothost/<name>` assigned, which the host golden is a
    /// clone of.
    pub golden: VolumeId,
    /// `boothost/<name>`'s version when it was claimed.
    pub assignment_version: u64,
}

/// What reporting an install done did (#148).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallDone {
    /// The intent was `install` for this clone, and is now `local`.
    Reset,
    /// The intent was not `install`: nothing to do (a repeated report).
    NotRequested,
}

/// What two host identifiers are compared by: case-insensitive, and a MAC in
/// any common spelling (`AA:BB:…`, `aa-bb-…`, `aabb.ccdd.eeff`, bare hex) as
/// its 12 hex digits.
pub fn host_match_key(s: &str) -> String {
    let t = s.trim();
    let hex: String = t.chars().filter(|c| !matches!(c, ':' | '-' | '.')).collect();
    let mac_shaped = hex.len() == 12
        && hex.chars().all(|c| c.is_ascii_hexdigit())
        && t.chars().all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '-' | '.'));
    if mac_shaped {
        hex.to_ascii_lowercase()
    } else {
        t.to_ascii_lowercase()
    }
}

/// How an alias is stored: a MAC written with separators becomes
/// `aa:bb:cc:dd:ee:ff`; anything else is kept as given.
pub fn normalize_alias(s: &str) -> String {
    let t = s.trim();
    let key = host_match_key(t);
    let separated = t.contains(':') || t.contains('-') || t.contains('.');
    if separated && key.len() == 12 && key.chars().all(|c| c.is_ascii_hexdigit()) {
        key.as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(":")
    } else {
        t.to_string()
    }
}

/// How a claim of `boothost/<name>` found its host (#204).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamedClaim {
    /// The name is a host's (its name, an alias, a former name).
    Known,
    /// A provisional host (`mac-<hex>`) found by `by`, renamed to the name.
    Renamed { from: String, by: &'static str },
    /// A host with a name of its own, found by `by`: the claimed name was
    /// added as an alias.
    Aliased { name: String, by: &'static str },
    /// Nobody: a new host, to be pinned to the default, with this MAC.
    New { mac: Option<String> },
}

/// What a machine is called before it has a name: `mac-<12 hex digits>` of
/// the first NIC's MAC it claimed `boothost/default` with (#200). `None` when
/// `mac` is not a MAC, or is all zeros or all ones.
pub fn provisional_host_name(mac: &str) -> Option<String> {
    let hex = host_match_key(mac);
    let is_mac = hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit());
    if !is_mac || hex == "000000000000" || hex == "ffffffffffff" {
        return None;
    }
    Some(format!("mac-{hex}"))
}

/// Whether a host still has the provisional name it booted the default
/// under — a machine nobody has named yet (#200).
pub fn is_provisional(name: &str) -> bool {
    name.strip_prefix("mac-")
        .is_some_and(|m| m.len() == 12 && m.chars().all(|c| c.is_ascii_hexdigit()))
}

/// A host name or alias is a synonym name, and never `default`.
fn check_host_name(name: &str) -> Result<(), SynonymError> {
    check_name(BOOTHOST_NS, name)?;
    if name.eq_ignore_ascii_case(DEFAULT_HOST) {
        return Err(SynonymError::InvalidName(format!(
            "{DEFAULT_HOST} is what a new machine boots, not a host"
        )));
    }
    Ok(())
}

/// The node's synonyms, persisted as `<data_dir>/synonyms.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SynonymStore {
    pub version: u32,
    /// Keyed `namespace/name`, ordered so the file reads the same twice.
    pub synonyms: BTreeMap<String, Synonym>,
    /// Boot hosts with aliases or a rename behind them, keyed by name (#199).
    /// A host with neither is just its `boothost/<name>` synonym.
    #[serde(default)]
    pub hosts: BTreeMap<String, Host>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Default for SynonymStore {
    fn default() -> Self {
        SynonymStore { version: 1, synonyms: BTreeMap::new(), hosts: BTreeMap::new(), path: None }
    }
}

impl SynonymStore {
    /// In-memory only — for a node with no `--data-dir`, and for tests. A
    /// name that does not survive a restart is worse than no name at all, so
    /// this is not something to configure by accident.
    pub fn in_memory() -> Self {
        SynonymStore::default()
    }

    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(SYNONYMS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<SynonymStore>(&raw) {
                Ok(mut s) => {
                    s.path = Some(path);
                    s
                }
                Err(e) => {
                    // Keep the bad file. A name nobody can resolve is a
                    // failed boot; one silently overwritten is a failed boot
                    // with nothing left to explain it.
                    let bak = path.with_extension("json.corrupt");
                    tracing::error!(
                        "corrupt {} ({e}) — preserved as {}",
                        path.display(),
                        bak.display()
                    );
                    let _ = std::fs::rename(&path, &bak);
                    SynonymStore { path: Some(path), ..SynonymStore::default() }
                }
            },
            Err(_) => SynonymStore { path: Some(path), ..SynonymStore::default() },
        }
    }

    pub fn get(&self, namespace: &str, name: &str) -> Option<&Synonym> {
        self.synonyms.get(&key(namespace, name))
    }

    /// Resolve `ns/name` or a bare `name`.
    pub fn find(&self, k: &str) -> Option<&Synonym> {
        let (ns, name) = split(k);
        self.get(&ns, &name)
    }

    /// Every synonym, optionally in one namespace.
    pub fn list(&self, namespace: Option<&str>) -> Vec<&Synonym> {
        self.synonyms
            .values()
            .filter(|s| namespace.map_or(true, |ns| s.namespace == ns))
            .collect()
    }

    /// Every synonym pointing at a volume — what makes deleting it a
    /// question rather than a silent break.
    pub fn pointing_at(&self, id: &VolumeId) -> Vec<&Synonym> {
        self.synonyms
            .values()
            .filter(|s| s.target.volume_id().as_ref() == Some(id))
            .collect()
    }

    /// Create a synonym. Fails if the name is taken: re-pointing is
    /// [`repoint`](Self::repoint), a different verb on purpose, because
    /// "create" silently moving an existing name is how a consumer ends up
    /// on storage nobody meant to give it.
    pub fn create(
        &mut self,
        namespace: &str,
        name: &str,
        target: Target,
        label: Option<String>,
        description: Option<String>,
    ) -> Result<&Synonym, SynonymError> {
        check_name(namespace, name)?;
        let k = key(namespace, name);
        if self.synonyms.contains_key(&k) {
            return Err(SynonymError::Exists(k));
        }
        // A new host may not be named what another host answers to.
        if namespace == BOOTHOST_NS {
            if let Some(other) = self.host_of(name) {
                if host_match_key(&other) != host_match_key(name) {
                    return Err(SynonymError::Conflict(format!(
                        "{name} is an alias of host {other}; claim or assign it as {other}"
                    )));
                }
            }
        }
        let t = now();
        let syn = Synonym {
            namespace: namespace.to_string(),
            name: name.to_string(),
            target,
            version: 1,
            label,
            description,
            history: Vec::new(),
            created_at: t,
            updated_at: t,
        };
        self.synonyms.insert(k.clone(), syn);
        self.persist();
        Ok(self.synonyms.get(&k).unwrap())
    }

    /// Point an existing name somewhere else. The version bumps, the old
    /// target goes into history, and a re-point to the target it already has
    /// still bumps — a client asking "did this change" is asking about the
    /// publish, and a republish of the same content is a change to it.
    pub fn repoint(
        &mut self,
        namespace: &str,
        name: &str,
        target: Target,
        label: Option<String>,
    ) -> Result<&Synonym, SynonymError> {
        let k = key(namespace, name);
        let syn = self.synonyms.get_mut(&k).ok_or_else(|| SynonymError::NotFound(k.clone()))?;
        let t = now();
        syn.history.insert(
            0,
            Previous {
                target: std::mem::replace(&mut syn.target, target),
                version: syn.version,
                label: syn.label.take(),
                replaced_at: t,
            },
        );
        syn.history.truncate(HISTORY);
        syn.version += 1;
        syn.label = label;
        syn.updated_at = t;
        self.persist();
        Ok(self.synonyms.get(&k).unwrap())
    }

    /// Put a name back to what it meant before. A re-point like any other:
    /// the version goes *up*, because versions are monotonic and a client
    /// that saw the bad publish must see a change when it is undone.
    pub fn rollback(&mut self, namespace: &str, name: &str) -> Result<&Synonym, SynonymError> {
        let k = key(namespace, name);
        let syn = self.synonyms.get(&k).ok_or_else(|| SynonymError::NotFound(k.clone()))?;
        let prev = syn.history.first().ok_or_else(|| SynonymError::NoHistory(k.clone()))?;
        let (target, label) = (prev.target.clone(), prev.label.clone());
        self.repoint(namespace, name, target, label)
    }

    pub fn remove(&mut self, namespace: &str, name: &str) -> Result<Synonym, SynonymError> {
        let k = key(namespace, name);
        let gone = self.synonyms.remove(&k).ok_or(SynonymError::NotFound(k))?;
        self.persist();
        Ok(gone)
    }

    /// The host a name or alias belongs to: a host record's name or one of
    /// its aliases, or a `boothost/<name>` assignment's name. `None` for a
    /// machine nobody has heard of.
    pub fn host_of(&self, k: &str) -> Option<String> {
        let want = host_match_key(k);
        if want.is_empty() {
            return None;
        }
        for h in self.hosts.values() {
            if host_match_key(&h.name) == want || h.aliases.iter().any(|a| host_match_key(a) == want) {
                return Some(h.name.clone());
            }
        }
        self.synonyms
            .values()
            .filter(|s| s.namespace == BOOTHOST_NS && s.name != DEFAULT_HOST)
            .find(|s| host_match_key(&s.name) == want)
            .map(|s| s.name.clone())
    }

    /// A host's record, or the bare record a host with only an assignment
    /// has.
    pub fn host(&self, name: &str) -> Option<Host> {
        if let Some(h) = self.hosts.get(name) {
            return Some(h.clone());
        }
        self.get(BOOTHOST_NS, name).map(|s| Host {
            name: s.name.clone(),
            created_at: s.created_at,
            updated_at: s.updated_at,
            ..Host::default()
        })
    }

    /// Every boot host: the ones with a record and the ones that are only an
    /// assignment, by name.
    pub fn hosts(&self) -> Vec<Host> {
        let mut names: std::collections::BTreeSet<String> = self.hosts.keys().cloned().collect();
        for s in self.list(Some(BOOTHOST_NS)) {
            if s.name != DEFAULT_HOST {
                names.insert(s.name.clone());
            }
        }
        names.iter().filter_map(|n| self.host(n)).collect()
    }

    /// Set a host's aliases, replacing the ones it had. Refused, naming both
    /// hosts, when an alias is another host's name or alias (#199).
    pub fn set_aliases(&mut self, name: &str, aliases: &[String]) -> Result<Host, SynonymError> {
        check_host_name(name)?;
        let name = match self.host_of(name) {
            Some(h) if host_match_key(&h) == host_match_key(name) => h,
            Some(h) => {
                return Err(SynonymError::Conflict(format!(
                    "{name} is an alias of host {h}; set aliases on {h}"
                )))
            }
            None => name.to_string(),
        };
        let mut wanted: Vec<String> = Vec::new();
        for a in aliases {
            let a = normalize_alias(a);
            check_host_name(&a).map_err(|e| match e {
                SynonymError::InvalidName(why) => SynonymError::InvalidName(format!("alias {a}: {why}")),
                other => other,
            })?;
            if host_match_key(&a) == host_match_key(&name)
                || wanted.iter().any(|w| host_match_key(w) == host_match_key(&a))
            {
                continue;
            }
            wanted.push(a);
        }
        let clashes: Vec<String> = wanted
            .iter()
            .filter_map(|a| {
                self.host_of(a)
                    .filter(|o| *o != name)
                    .map(|o| format!("{a} already names host {o}"))
            })
            .collect();
        if !clashes.is_empty() {
            return Err(SynonymError::Conflict(format!(
                "two hosts may not share an alias: {} (for host {name})",
                clashes.join("; ")
            )));
        }
        let t = now();
        let h = self.hosts.entry(name.clone()).or_insert_with(|| Host {
            name: name.clone(),
            created_at: t,
            updated_at: t,
            ..Host::default()
        });
        h.aliases = wanted;
        h.updated_at = t;
        let out = h.clone();
        self.persist();
        Ok(out)
    }

    /// The host this is an **alias** of — never an assignment's name (#204).
    /// What a serial may be matched by: an alias is set by an operator, on
    /// one host (two may not share one), so a chassis serial every blade of a
    /// MicroCloud reports means nothing unless someone said it means one
    /// machine. A `boothost/<serial>` assignment (server1's old trial one,
    /// #249) is not that.
    pub fn alias_owner(&self, k: &str) -> Option<String> {
        let want = host_match_key(k);
        if want.is_empty() {
            return None;
        }
        self.hosts
            .values()
            .find(|h| h.aliases.iter().any(|a| host_match_key(a) == want))
            .map(|h| h.name.clone())
    }

    /// Which host a claim of `boothost/<claimed>` is, when the machine says
    /// its MAC and serial too (#204, stormbootx#23: a name from DNS).
    ///
    /// A name some host has (its own, an alias, a former name) is that host,
    /// whatever the MAC says. Otherwise the machine may already be a host
    /// under another name:
    /// - its MAC's host, else the host its serial is an alias of;
    /// - a provisional one (`mac-<hex>`, #200) is **renamed** to the claimed
    ///   name, its old name kept as an alias — "named after the first boot",
    ///   done by DNS;
    /// - one with a real name keeps it: the claimed name becomes one more
    ///   alias of it, and the claim is that host's (two DNS names for one
    ///   machine is the operator's to settle, not this claim's).
    ///
    /// Neither: a new host of the claimed name, which the caller pins to the
    /// default, with the MAC (never the serial) as its alias.
    pub fn resolve_named_claim(
        &mut self,
        claimed: &str,
        mac: Option<&str>,
        serial: Option<&str>,
    ) -> Result<(String, NamedClaim), SynonymError> {
        if let Some(h) = self.host_of(claimed) {
            return Ok((h, NamedClaim::Known));
        }
        let mac = mac.filter(|m| provisional_host_name(m).is_some());
        let by_mac = mac.and_then(|m| self.host_of(m));
        let found = match by_mac {
            Some(h) => Some((h, "mac")),
            None => serial.filter(|s| !s.trim().is_empty()).and_then(|s| self.alias_owner(s)).map(|h| (h, "serial")),
        };
        match found {
            Some((h, by)) if is_provisional(&h) => {
                let renamed = self.rename_host(&h, claimed, true)?;
                Ok((renamed.name, NamedClaim::Renamed { from: h, by }))
            }
            Some((h, by)) => {
                let mut aliases = self.host(&h).map(|x| x.aliases).unwrap_or_default();
                aliases.push(claimed.to_string());
                self.set_aliases(&h, &aliases)?;
                Ok((h, NamedClaim::Aliased { name: claimed.to_string(), by }))
            }
            None => Ok((claimed.to_string(), NamedClaim::New { mac: mac.map(normalize_alias) })),
        }
    }

    /// The host a machine claiming `boothost/default` with this MAC is (#200):
    /// the host the MAC is an alias of, or the one still (or once) called
    /// `mac-<hex>`, or else a new provisional host of that name with the MAC
    /// as its alias. Returns the host's name and whether it was made now.
    pub fn provisional_host(&mut self, mac: &str) -> Result<(String, bool), SynonymError> {
        let name = provisional_host_name(mac)
            .ok_or_else(|| SynonymError::InvalidName(format!("{mac:?} is not a NIC's MAC address")))?;
        if let Some(h) = self.host_of(mac).or_else(|| self.host_of(&name)) {
            return Ok((h, false));
        }
        let hex = &name["mac-".len()..];
        let alias = hex
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect::<Vec<_>>()
            .join(":");
        let t = now();
        self.hosts.insert(
            name.clone(),
            Host { name: name.clone(), aliases: vec![alias], created_at: t, updated_at: t, ..Host::default() },
        );
        self.persist();
        Ok((name, true))
    }

    /// Rename a host, keeping everything it has: its assignment and host
    /// golden move to the new name with their versions and history, its
    /// aliases stay, and the old name is remembered so its clones are still
    /// collected. The old name stays an alias unless `keep_alias` is false,
    /// so a machine still claiming by it boots as before.
    pub fn rename_host(&mut self, from: &str, to: &str, keep_alias: bool) -> Result<Host, SynonymError> {
        let old = self
            .host_of(from)
            .ok_or_else(|| SynonymError::NotFound(key(BOOTHOST_NS, from)))?;
        if old == DEFAULT_HOST {
            return Err(SynonymError::InvalidName(format!("{DEFAULT_HOST} is not a host")));
        }
        check_host_name(to)?;
        if old == to {
            return self.host(&old).ok_or_else(|| SynonymError::NotFound(key(BOOTHOST_NS, from)));
        }
        if let Some(other) = self.host_of(to).filter(|o| *o != old) {
            return Err(SynonymError::Conflict(format!(
                "cannot rename {old} to {to}: {to} already names host {other}"
            )));
        }
        for ns in [BOOTHOST_NS, HOSTGOLDEN_NS] {
            let k = key(ns, to);
            if self.synonyms.contains_key(&k) && host_match_key(to) != host_match_key(&old) {
                return Err(SynonymError::Exists(k));
            }
        }
        let t = now();
        for ns in [BOOTHOST_NS, HOSTGOLDEN_NS] {
            if let Some(mut s) = self.synonyms.remove(&key(ns, &old)) {
                s.name = to.to_string();
                s.updated_at = t;
                self.synonyms.insert(key(ns, to), s);
            }
        }
        let mut h = self.hosts.remove(&old).unwrap_or(Host {
            name: old.clone(),
            created_at: t,
            updated_at: t,
            ..Host::default()
        });
        h.name = to.to_string();
        h.aliases.retain(|a| host_match_key(a) != host_match_key(to));
        if keep_alias && host_match_key(&old) != host_match_key(to) {
            h.aliases.push(old.clone());
        }
        if !h.former_names.contains(&old) {
            h.former_names.push(old);
        }
        h.updated_at = t;
        self.hosts.insert(to.to_string(), h.clone());
        self.persist();
        Ok(h)
    }

    /// Set a host's boot intent (#148). `name` is the host's name or an
    /// alias; a host nobody has heard of is not found. Returns the host.
    pub fn set_intent(&mut self, name: &str, intent: BootIntent) -> Result<Host, SynonymError> {
        let host = self.host_of(name).ok_or_else(|| SynonymError::NotFound(key(BOOTHOST_NS, name)))?;
        let t = now();
        let base = self.host(&host).unwrap_or_default();
        let h = self.hosts.entry(host.clone()).or_insert(base);
        h.intent = intent;
        h.install_claim = None;
        h.updated_at = t;
        let out = h.clone();
        self.persist();
        Ok(out)
    }

    /// Set (or, with `None`, clear) a machine's TPM mark (#216). `name` is
    /// the host's name; a machine not yet known is recorded, so the platform
    /// can mark it as it joins the fleet, before its first claim.
    pub fn set_tpm(&mut self, name: &str, tpm: Option<TpmMark>) -> Result<Host, SynonymError> {
        check_host_name(name)?;
        let host = match self.host_of(name) {
            Some(h) if host_match_key(&h) == host_match_key(name) => h,
            Some(h) => {
                return Err(SynonymError::Conflict(format!(
                    "{name} is an alias of host {h}; mark {h}"
                )))
            }
            None => name.to_string(),
        };
        let t = now();
        let base = self.host(&host).unwrap_or(Host {
            name: host.clone(),
            created_at: t,
            ..Host::default()
        });
        let h = self.hosts.entry(host).or_insert(base);
        h.tpm = tpm;
        h.tpm_set_at = Some(t);
        h.updated_at = t;
        let out = h.clone();
        self.persist();
        Ok(out)
    }

    /// Record the boot claim just served to `host` (#216).
    pub fn note_claim(&mut self, host: &str, rec: ClaimRecord) {
        let base = self.host(host).unwrap_or(Host {
            name: host.to_string(),
            created_at: rec.claimed_at,
            ..Host::default()
        });
        let h = self.hosts.entry(host.to_string()).or_insert(base);
        h.last_claim = Some(rec);
        self.persist();
    }

    /// A claim of `host` handed out `clone`: when the intent is `install`,
    /// that clone is the install whose completion resets it. The later of
    /// the firmware's and the initramfs's claims is the one that installs.
    pub fn note_install_claim(&mut self, host: &str, clone: VolumeId) {
        if let Some(h) = self.hosts.get_mut(host).filter(|h| h.intent == BootIntent::Install) {
            h.install_claim = Some(clone);
            self.persist();
        }
    }

    /// The node installed from `clone` has finished its flow-over: an
    /// `install` intent that clone was claimed under goes back to `local`.
    /// Refused when the intent is `install` for another clone — a report of
    /// an install that began before the request, or one made up.
    pub fn install_done(&mut self, name: &str, clone: VolumeId) -> Result<(Host, InstallDone), SynonymError> {
        let host = self.host_of(name).ok_or_else(|| SynonymError::NotFound(key(BOOTHOST_NS, name)))?;
        let Some(h) = self.hosts.get_mut(&host).filter(|h| h.intent == BootIntent::Install) else {
            let h = self.host(&host).unwrap_or_default();
            return Ok((h, InstallDone::NotRequested));
        };
        if h.install_claim != Some(clone) {
            let why = match h.install_claim {
                Some(c) => format!("the install requested for {host} is the one booted from {}, not {}", c.0, clone.0),
                None => format!("no claim of {host} has been made since its install was requested"),
            };
            return Err(SynonymError::Conflict(why));
        }
        h.intent = BootIntent::Local;
        h.install_claim = None;
        h.updated_at = now();
        let out = h.clone();
        self.persist();
        Ok((out, InstallDone::Reset))
    }

    pub fn persist(&self) {
        let Some(path) = &self.path else { return };
        let bytes = match serde_json::to_vec_pretty(self) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("failed to serialize synonyms: {e}");
                return;
            }
        };
        let tmp = path.with_extension("json.tmp");
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_named_claim_reaches_the_machine_it_already_is() {
        use super::*;
        let mut st = SynonymStore::in_memory();
        let (prov, _) = st.provisional_host("aa:bb:cc:dd:ee:01").unwrap();
        assert_eq!(prov, "mac-aabbccddee01");

        // A DNS name with the provisional host's MAC: renamed, the old name kept.
        let (h, how) = st.resolve_named_claim("server3", Some("AA-BB-CC-DD-EE-01"), None).unwrap();
        assert_eq!(h, "server3");
        assert_eq!(how, NamedClaim::Renamed { from: prov.clone(), by: "mac" });
        assert_eq!(st.host_of(&prov).as_deref(), Some("server3"));
        assert_eq!(st.host_of("aa:bb:cc:dd:ee:01").as_deref(), Some("server3"));
        // Again: now simply that host.
        assert_eq!(st.resolve_named_claim("server3", Some("aa:bb:cc:dd:ee:01"), None).unwrap().1, NamedClaim::Known);

        // A second DNS name for a named machine: an alias, not a rename.
        let (h, how) = st.resolve_named_claim("blade3", Some("aa:bb:cc:dd:ee:01"), None).unwrap();
        assert_eq!(h, "server3");
        assert_eq!(how, NamedClaim::Aliased { name: "blade3".into(), by: "mac" });
        assert_eq!(st.host_of("blade3").as_deref(), Some("server3"));

        // A serial an operator set on one host finds it; a serial nobody set does not.
        st.set_aliases("C2NR0Q2", &["C2NR0Q2-SN".to_string()]).unwrap();
        let (h, how) = st.resolve_named_claim("stormblock1", None, Some("c2nr0q2-sn")).unwrap();
        assert_eq!(h, "C2NR0Q2");
        assert_eq!(how, NamedClaim::Aliased { name: "stormblock1".into(), by: "serial" });
        // An assignment named by a serial (server1's trial one) is not an alias.
        st.create(BOOTHOST_NS, "S11075924402016", Target::Volume { id: VolumeId(uuid::Uuid::new_v4()) }, None, None).unwrap();
        let (h, how) = st.resolve_named_claim("server8", None, Some("S11075924402016")).unwrap();
        assert_eq!((h.as_str(), how), ("server8", NamedClaim::New { mac: None }));

        // Nobody: a new host, with the MAC to alias.
        let (h, how) = st.resolve_named_claim("server4", Some("aa:bb:cc:dd:ee:04"), None).unwrap();
        assert_eq!((h.as_str(), how), ("server4", NamedClaim::New { mac: Some("aa:bb:cc:dd:ee:04".into()) }));
        // A MAC that is no MAC is not used.
        let (_, how) = st.resolve_named_claim("server5", Some("not-a-mac"), None).unwrap();
        assert_eq!(how, NamedClaim::New { mac: None });
    }

    use super::*;

    fn vol() -> VolumeId {
        VolumeId(uuid::Uuid::new_v4())
    }

    #[test]
    fn a_host_answers_to_its_name_and_its_aliases() {
        let mut s = SynonymStore::in_memory();
        s.create(BOOTHOST_NS, "stormblock1", Target::Volume { id: vol() }, None, None).unwrap();
        let h = s
            .set_aliases("stormblock1", &["C2NR0Q2".into(), "AA-BB-CC-DD-EE-FF".into()])
            .unwrap();
        assert_eq!(h.aliases, vec!["C2NR0Q2".to_string(), "aa:bb:cc:dd:ee:ff".to_string()]);
        for k in ["stormblock1", "STORMBLOCK1", "C2NR0Q2", "c2nr0q2", "aa:bb:cc:dd:ee:ff", "AABBCCDDEEFF", "aabb.ccdd.eeff"] {
            assert_eq!(s.host_of(k).as_deref(), Some("stormblock1"), "{k}");
        }
        assert_eq!(s.host_of("someone-else"), None);
        assert_eq!(s.host_of(DEFAULT_HOST), None, "default is not a host");
    }

    #[test]
    fn two_hosts_never_share_an_alias() {
        let mut s = SynonymStore::in_memory();
        s.create(BOOTHOST_NS, "mc1", Target::Volume { id: vol() }, None, None).unwrap();
        s.create(BOOTHOST_NS, "mc2", Target::Volume { id: vol() }, None, None).unwrap();
        s.set_aliases("mc1", &["CHASSIS9".into()]).unwrap();
        let e = s.set_aliases("mc2", &["chassis9".into()]).unwrap_err().to_string();
        assert!(e.contains("mc1") && e.contains("mc2") && e.contains("chassis9"), "{e}");
        // Nor may an alias be another host's name, or a new host be named
        // what another host answers to.
        let e = s.set_aliases("mc2", &["MC1".into()]).unwrap_err().to_string();
        assert!(e.contains("mc1") && e.contains("mc2"), "{e}");
        let e = s
            .create(BOOTHOST_NS, "CHASSIS9", Target::Volume { id: vol() }, None, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("mc1"), "{e}");
        assert!(s.host("mc2").unwrap().aliases.is_empty(), "a refused set changes nothing");
    }

    #[test]
    fn a_rename_keeps_the_assignment_golden_and_history() {
        let mut s = SynonymStore::in_memory();
        let (a, b, g) = (vol(), vol(), vol());
        s.create(BOOTHOST_NS, "C2NR0Q2", Target::Volume { id: a }, None, None).unwrap();
        s.repoint(BOOTHOST_NS, "C2NR0Q2", Target::Volume { id: b }, None).unwrap();
        s.create(HOSTGOLDEN_NS, "C2NR0Q2", Target::Volume { id: g }, None, None).unwrap();
        s.set_aliases("C2NR0Q2", &["aa:bb:cc:dd:ee:01".into()]).unwrap();

        let h = s.rename_host("C2NR0Q2", "stormblock1", true).unwrap();
        assert_eq!(h.name, "stormblock1");
        assert_eq!(h.former_names, vec!["C2NR0Q2".to_string()]);
        assert!(h.aliases.contains(&"C2NR0Q2".to_string()), "the old name stays an alias");
        assert!(h.aliases.contains(&"aa:bb:cc:dd:ee:01".to_string()));

        let moved = s.get(BOOTHOST_NS, "stormblock1").unwrap();
        assert_eq!((moved.version, moved.target.volume_id()), (2, Some(b)));
        assert_eq!(moved.history[0].target.volume_id(), Some(a));
        assert_eq!(s.get(HOSTGOLDEN_NS, "stormblock1").unwrap().target.volume_id(), Some(g));
        assert!(s.get(BOOTHOST_NS, "C2NR0Q2").is_none());
        assert_eq!(s.host_of("C2NR0Q2").as_deref(), Some("stormblock1"));
        assert_eq!(s.hosts().len(), 1);
    }

    #[test]
    fn a_rename_onto_another_host_is_refused() {
        let mut s = SynonymStore::in_memory();
        s.create(BOOTHOST_NS, "a1", Target::Volume { id: vol() }, None, None).unwrap();
        s.create(BOOTHOST_NS, "b1", Target::Volume { id: vol() }, None, None).unwrap();
        s.set_aliases("b1", &["SER-B".into()]).unwrap();
        for to in ["b1", "ser-b"] {
            let e = s.rename_host("a1", to, true).unwrap_err().to_string();
            assert!(e.contains("a1") && e.contains("b1"), "{e}");
        }
        // Without the alias, the old name no longer answers.
        s.rename_host("a1", "a2", false).unwrap();
        assert_eq!(s.host_of("a1"), None);
        assert_eq!(s.host("a2").unwrap().former_names, vec!["a1".to_string()]);
    }

    #[test]
    fn a_default_claims_mac_is_a_host_of_its_own_until_named() {
        let mut s = SynonymStore::in_memory();
        assert_eq!(provisional_host_name("AA-BB-CC-DD-EE-01").as_deref(), Some("mac-aabbccddee01"));
        for bad in ["", "C2NR0Q2", "00:00:00:00:00:00", "ff:ff:ff:ff:ff:ff", "aa:bb:cc:dd:ee"] {
            assert!(s.provisional_host(bad).is_err(), "{bad:?}");
        }
        assert!(s.hosts().is_empty(), "a refused MAC makes no host");

        let (a, made) = s.provisional_host("aa:bb:cc:dd:ee:01").unwrap();
        assert_eq!((a.as_str(), made), ("mac-aabbccddee01", true));
        assert!(is_provisional(&a));
        assert_eq!(s.host(&a).unwrap().aliases, vec!["aa:bb:cc:dd:ee:01".to_string()]);
        let (b, _) = s.provisional_host("aa:bb:cc:dd:ee:02").unwrap();
        assert_ne!(a, b, "two MACs, two machines");
        assert_eq!(s.provisional_host("AABBCCDDEE01").unwrap(), (a.clone(), false), "the same MAC, the same host");

        // Named: the MAC and the provisional name both still mean it.
        s.create(BOOTHOST_NS, &a, Target::Volume { id: vol() }, None, None).unwrap();
        s.rename_host(&a, "server3", true).unwrap();
        assert!(!is_provisional("server3"));
        assert_eq!(s.provisional_host("aa:bb:cc:dd:ee:01").unwrap(), ("server3".to_string(), false));
        assert_eq!(s.host_of("mac-aabbccddee01").as_deref(), Some("server3"));
        assert_eq!(s.hosts().len(), 2);
    }

    #[test]
    fn hosts_survive_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut s = SynonymStore::load(dir.path());
            s.create(BOOTHOST_NS, "n1", Target::Volume { id: vol() }, None, None).unwrap();
            s.set_aliases("n1", &["S1".into()]).unwrap();
        }
        let s = SynonymStore::load(dir.path());
        assert_eq!(s.host_of("s1").as_deref(), Some("n1"));
        // A file written before hosts existed still loads.
        let old = r#"{"version":1,"synonyms":{}}"#;
        std::fs::write(dir.path().join(SYNONYMS_FILE), old).unwrap();
        assert!(SynonymStore::load(dir.path()).hosts.is_empty());
    }

    #[test]
    fn a_name_resolves_and_re_points() {
        let mut s = SynonymStore::in_memory();
        let a = vol();
        let b = vol();
        s.create(DEFAULT_NAMESPACE, "fedora-43", Target::Volume { id: a }, None, None).unwrap();
        assert_eq!(s.find("fedora-43").unwrap().target.volume_id(), Some(a));
        assert_eq!(s.find("default/fedora-43").unwrap().version, 1);

        let after = s
            .repoint(DEFAULT_NAMESPACE, "fedora-43", Target::Volume { id: b }, Some("2".into()))
            .unwrap();
        assert_eq!(after.target.volume_id(), Some(b));
        assert_eq!(after.version, 2, "a re-point is what a client watches for");
        assert_eq!(after.history[0].target.volume_id(), Some(a));
    }

    #[test]
    fn a_rollback_goes_forward_in_version() {
        let mut s = SynonymStore::in_memory();
        let (a, b) = (vol(), vol());
        s.create("images", "nginx", Target::Volume { id: a }, Some("1.0".into()), None).unwrap();
        s.repoint("images", "nginx", Target::Volume { id: b }, Some("2.0".into())).unwrap();
        let back = s.rollback("images", "nginx").unwrap();
        assert_eq!(back.target.volume_id(), Some(a));
        assert_eq!(back.label.as_deref(), Some("1.0"));
        assert_eq!(back.version, 3, "monotonic: undoing a publish is still a change");
    }

    #[test]
    fn namespaces_keep_names_apart() {
        let mut s = SynonymStore::in_memory();
        let (a, b) = (vol(), vol());
        s.create("tenant-a", "root", Target::Volume { id: a }, None, None).unwrap();
        s.create("tenant-b", "root", Target::Volume { id: b }, None, None).unwrap();
        assert_eq!(s.find("tenant-a/root").unwrap().target.volume_id(), Some(a));
        assert_eq!(s.find("tenant-b/root").unwrap().target.volume_id(), Some(b));
        assert_eq!(s.list(Some("tenant-a")).len(), 1);
        assert_eq!(s.list(None).len(), 2);
        // A bare name is the default namespace, and neither of these is in it.
        assert!(s.find("root").is_none());
    }

    #[test]
    fn a_second_create_is_refused_and_a_uuid_name_is_too() {
        let mut s = SynonymStore::in_memory();
        let a = vol();
        s.create(DEFAULT_NAMESPACE, "golden", Target::Volume { id: a }, None, None).unwrap();
        assert!(matches!(
            s.create(DEFAULT_NAMESPACE, "golden", Target::Volume { id: vol() }, None, None),
            Err(SynonymError::Exists(_))
        ));
        assert!(matches!(
            s.create(DEFAULT_NAMESPACE, &a.0.to_string(), Target::Volume { id: a }, None, None),
            Err(SynonymError::InvalidName(_))
        ));
    }

    #[test]
    fn what_points_at_a_volume_is_answerable() {
        let mut s = SynonymStore::in_memory();
        let a = vol();
        s.create(DEFAULT_NAMESPACE, "one", Target::Volume { id: a }, None, None).unwrap();
        s.create("other", "two", Target::Volume { id: a }, None, None).unwrap();
        s.create(DEFAULT_NAMESPACE, "far", Target::Remote { uri: "nvme-tcp://h:4420/nqn".into() }, None, None)
            .unwrap();
        let mut names: Vec<&str> = s.pointing_at(&a).iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["one", "two"]);
    }

    #[test]
    fn a_store_survives_a_restart() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = vol();
        {
            let mut s = SynonymStore::load(dir.path());
            s.create(DEFAULT_NAMESPACE, "node-root", Target::Volume { id: a }, None, None).unwrap();
        }
        let s = SynonymStore::load(dir.path());
        assert_eq!(s.find("node-root").unwrap().target.volume_id(), Some(a));
        assert_eq!(s.find("node-root").unwrap().version, 1);
    }

    #[test]
    fn an_intent_is_the_hosts_and_answers_to_its_aliases() {
        let mut s = SynonymStore::in_memory();
        assert!(matches!(s.set_intent("nobody", BootIntent::Local), Err(SynonymError::NotFound(_))));
        // A machine that booted the default: its MAC, bare, reaches it.
        let (host, _) = s.provisional_host("AC:1F:6B:8A:A7:9C").unwrap();
        assert_eq!(s.host(&host).unwrap().intent, BootIntent::Auto, "never set is auto");
        let h = s.set_intent("ac1f6b8aa79c", BootIntent::Local).unwrap();
        assert_eq!((h.name.as_str(), h.intent), ("mac-ac1f6b8aa79c", BootIntent::Local));
        // A rename carries it, and the MAC still reaches it.
        s.rename_host(&host, "server1", true).unwrap();
        assert_eq!(s.host(&s.host_of("ac1f6b8aa79c").unwrap()).unwrap().intent, BootIntent::Local);
        // A host that is only an assignment gets a record.
        s.create(BOOTHOST_NS, "server2", Target::Volume { id: vol() }, None, None).unwrap();
        assert_eq!(s.set_intent("SERVER2", BootIntent::Install).unwrap().intent, BootIntent::Install);
        assert_eq!(s.host("server2").unwrap().intent, BootIntent::Install);
        for (w, want) in [("Install", BootIntent::Install), (" local ", BootIntent::Local), ("AUTO", BootIntent::Auto)] {
            assert_eq!(w.parse::<BootIntent>().unwrap(), want);
        }
        assert!("reinstall".parse::<BootIntent>().is_err());
    }

    #[test]
    fn install_is_one_shot_and_only_for_the_clone_claimed_under_it() {
        let mut s = SynonymStore::in_memory();
        s.create(BOOTHOST_NS, "server1", Target::Volume { id: vol() }, None, None).unwrap();
        let (before, firmware, initramfs) = (vol(), vol(), vol());
        // A claim before the request is not the install.
        s.note_install_claim("server1", before);
        assert_eq!(s.install_done("server1", before).unwrap().1, InstallDone::NotRequested);
        s.set_intent("server1", BootIntent::Install).unwrap();
        assert!(matches!(s.install_done("server1", before), Err(SynonymError::Conflict(_))));
        // Firmware claims, then the initramfs: the later clone installs.
        s.note_install_claim("server1", firmware);
        s.note_install_claim("server1", initramfs);
        assert!(matches!(s.install_done("server1", firmware), Err(SynonymError::Conflict(_))));
        assert_eq!(s.host("server1").unwrap().intent, BootIntent::Install);
        let (h, done) = s.install_done("server1", initramfs).unwrap();
        assert_eq!((h.intent, done, h.install_claim), (BootIntent::Local, InstallDone::Reset, None));
        // Reported twice: nothing more to do.
        assert_eq!(s.install_done("server1", initramfs).unwrap().1, InstallDone::NotRequested);
        // Asked again: the old report does not answer for the new request.
        s.set_intent("server1", BootIntent::Install).unwrap();
        assert!(s.install_done("server1", initramfs).is_err());
    }

    #[test]
    fn an_intent_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let clone = vol();
        {
            let mut s = SynonymStore::load(dir.path());
            s.create(BOOTHOST_NS, "server1", Target::Volume { id: vol() }, None, None).unwrap();
            s.set_intent("server1", BootIntent::Install).unwrap();
            s.note_install_claim("server1", clone);
        }
        let mut s = SynonymStore::load(dir.path());
        let h = s.host("server1").unwrap();
        assert_eq!((h.intent, h.install_claim), (BootIntent::Install, Some(clone)));
        assert_eq!(s.install_done("server1", clone).unwrap().1, InstallDone::Reset);
    }
}
