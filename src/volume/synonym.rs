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

/// The node's synonyms, persisted as `<data_dir>/synonyms.json`.
#[derive(Debug, Serialize, Deserialize)]
pub struct SynonymStore {
    pub version: u32,
    /// Keyed `namespace/name`, ordered so the file reads the same twice.
    pub synonyms: BTreeMap<String, Synonym>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl Default for SynonymStore {
    fn default() -> Self {
        SynonymStore { version: 1, synonyms: BTreeMap::new(), path: None }
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
                    SynonymStore { version: 1, synonyms: BTreeMap::new(), path: Some(path) }
                }
            },
            Err(_) => SynonymStore { version: 1, synonyms: BTreeMap::new(), path: Some(path) },
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
    use super::*;

    fn vol() -> VolumeId {
        VolumeId(uuid::Uuid::new_v4())
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
}
