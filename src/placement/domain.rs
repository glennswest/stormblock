//! Failure domains — *what fails together*, as distinct from how fast and
//! how far (`StorageTier`, `Locality`).
//!
//! A domain is an ordered chain of `rung=value` pairs from the widest blast
//! radius to the narrowest: `site/building/room/row/rack/node/hba/shelf/bay/
//! drive`. Two slabs are the same domain *at rung R* when their chains agree
//! through R, so "spread across drives" and "spread across shelves" are the
//! same comparison at different depths (#71, #72). The chain is what the
//! engine keeps; who fills the upper rungs — stormdrive per drive (#70), an
//! orchestrator per node — is not its concern.
//!
//! A slab's default domain is the identity of the device it lives on, which
//! is enough for the single-node case that matters first: two legs of one
//! extent on two different drives.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::drive::DeviceId;

/// The rungs of the ladder, widest first. Unknown labels are allowed and
/// sort after these, in the order they were given.
pub const RUNGS: &[&str] = &[
    "site", "building", "room", "row", "rack", "node", "hba", "shelf", "bay", "drive",
];

/// The rung a policy spreads at when it does not say: one leg per drive.
pub const DEFAULT_RUNG: &str = "drive";

/// One rung of a chain.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Label {
    pub rung: String,
    pub value: String,
}

/// An ordered chain of labels, widest blast radius first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FailureDomain {
    pub chain: Vec<Label>,
}

fn rung_rank(rung: &str) -> usize {
    RUNGS.iter().position(|r| *r == rung).unwrap_or(RUNGS.len())
}

impl FailureDomain {
    pub fn new() -> Self {
        FailureDomain { chain: Vec::new() }
    }

    /// The domain a device is in by its own identity alone: `drive=<serial>`,
    /// or its path when there is no serial worth the name (a file device says
    /// `file`; the path is what tells two of them apart, and it is stable
    /// across a restart where the uuid is not — #65).
    pub fn from_device(id: &DeviceId) -> Self {
        let generic = id.serial.is_empty() || id.serial == "unknown" || id.serial == "file";
        let value = if generic {
            if id.path.is_empty() { id.uuid.to_string() } else { id.path.clone() }
        } else {
            id.serial.clone()
        };
        FailureDomain { chain: vec![Label { rung: "drive".into(), value }] }
    }

    /// Build from `rung=value` pairs in any order; known rungs are sorted
    /// widest first, unknown ones keep their given order after them.
    pub fn from_labels<I, K, V>(labels: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut chain: Vec<Label> = labels
            .into_iter()
            .map(|(k, v)| Label { rung: k.into(), value: v.into() })
            .collect();
        chain.sort_by_key(|l| rung_rank(&l.rung));
        FailureDomain { chain }
    }

    /// Parse `site=a/rack=1/drive=S123`. Empty input is the empty domain.
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut labels = Vec::new();
        for part in s.split('/').map(str::trim).filter(|p| !p.is_empty()) {
            let (k, v) = part
                .split_once('=')
                .ok_or_else(|| format!("failure-domain label '{part}' is not rung=value"))?;
            if k.trim().is_empty() || v.trim().is_empty() {
                return Err(format!("failure-domain label '{part}' has an empty side"));
            }
            labels.push((k.trim().to_string(), v.trim().to_string()));
        }
        Ok(Self::from_labels(labels))
    }

    /// Add or replace one rung, keeping the chain ordered.
    pub fn with(mut self, rung: &str, value: &str) -> Self {
        self.chain.retain(|l| l.rung != rung);
        self.chain.push(Label { rung: rung.into(), value: value.into() });
        self.chain.sort_by_key(|l| rung_rank(&l.rung));
        self
    }

    /// Extend with labels from a wider source (a drive's registration
    /// labels), keeping any rung this chain already names.
    pub fn merged_under(&self, outer: &FailureDomain) -> Self {
        let mut out = outer.clone();
        for l in &self.chain {
            out = out.with(&l.rung, &l.value);
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.chain.is_empty()
    }

    pub fn get(&self, rung: &str) -> Option<&str> {
        self.chain.iter().find(|l| l.rung == rung).map(|l| l.value.as_str())
    }

    /// The prefix of the chain through `rung` — the key two domains are
    /// compared by when spreading at that rung. A chain that does not name
    /// the rung is keyed by everything it has, which is the honest answer:
    /// with no shelf label, two drives are only known to be different drives.
    pub fn key_at(&self, rung: &str) -> &[Label] {
        match self.chain.iter().position(|l| l.rung == rung) {
            Some(i) => &self.chain[..=i],
            None => &self.chain[..],
        }
    }

    /// Whether two domains would fail together at `rung`.
    ///
    /// Two empty chains are *unknown*, and unknown is treated as shared —
    /// a policy that asks for separate drives must not be satisfied by two
    /// slabs nobody can tell apart.
    pub fn same_at(&self, other: &FailureDomain, rung: &str) -> bool {
        let a = self.key_at(rung);
        let b = other.key_at(rung);
        if a.is_empty() || b.is_empty() {
            return true;
        }
        a == b
    }
}

impl fmt::Display for FailureDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, l) in self.chain.iter().enumerate() {
            if i > 0 {
                write!(f, "/")?;
            }
            write!(f, "{}={}", l.rung, l.value)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn dev(serial: &str) -> DeviceId {
        DeviceId {
            uuid: Uuid::new_v4(),
            serial: serial.into(),
            model: "m".into(),
            path: "/dev/x".into(),
        }
    }

    #[test]
    fn device_identity_is_a_drive_rung() {
        let d = FailureDomain::from_device(&dev("S1"));
        assert_eq!(d.to_string(), "drive=S1");
        assert_eq!(d.get("drive"), Some("S1"));
    }

    #[test]
    fn two_drives_differ_at_drive_and_agree_at_shelf() {
        let a = FailureDomain::parse("shelf=X/drive=1").unwrap();
        let b = FailureDomain::parse("shelf=X/drive=2").unwrap();
        assert!(!a.same_at(&b, "drive"));
        assert!(a.same_at(&b, "shelf"));
        let c = FailureDomain::parse("shelf=Y/drive=3").unwrap();
        assert!(!a.same_at(&c, "shelf"));
    }

    #[test]
    fn parse_orders_rungs_widest_first() {
        let d = FailureDomain::parse("drive=1/site=a/rack=r").unwrap();
        assert_eq!(d.to_string(), "site=a/rack=r/drive=1");
    }

    #[test]
    fn unknown_is_treated_as_shared() {
        let a = FailureDomain::new();
        let b = FailureDomain::parse("drive=1").unwrap();
        assert!(a.same_at(&b, "drive"));
        assert!(a.same_at(&a, "drive"));
    }

    #[test]
    fn a_chain_without_the_rung_is_keyed_by_what_it_has() {
        let a = FailureDomain::parse("drive=1").unwrap();
        let b = FailureDomain::parse("drive=2").unwrap();
        // Asked about shelves, all we know is they are different drives.
        assert!(!a.same_at(&b, "shelf"));
    }

    #[test]
    fn merge_keeps_own_rungs_and_takes_the_rest() {
        let own = FailureDomain::parse("drive=1").unwrap();
        let drive_labels = FailureDomain::parse("shelf=S/bay=3/drive=WRONG").unwrap();
        let m = own.merged_under(&drive_labels);
        assert_eq!(m.to_string(), "shelf=S/bay=3/drive=1");
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(FailureDomain::parse("drive").is_err());
        assert!(FailureDomain::parse("=x").is_err());
        assert!(FailureDomain::parse("").unwrap().is_empty());
    }
}
