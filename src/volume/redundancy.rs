//! Redundancy is a property of a **volume**, not of a drive.
//!
//! A node carries a mix: one volume mirrored two ways, another as 4+1 parity,
//! a golden's clones inheriting the golden's policy — all on the same drives.
//! The policy names *how many* copies or parity legs and *how far apart*
//! they must be (a failure-domain rung); placement does the rest, and the
//! rule is a boundary, not a preference: an extent whose legs cannot be put
//! on distinct domains is not allocated.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::placement::domain::{DEFAULT_RUNG, RUNGS};

/// How a volume is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Redundancy {
    /// One copy. What every volume was before this existed.
    None,
    /// `copies` full copies of every extent, each on its own domain.
    Mirror { copies: u8 },
    /// Stripes of `data` extents protected by `parity` parity legs
    /// (1 = RAID 5, 2 = RAID 6), every member on its own domain.
    Parity { data: u8, parity: u8 },
}

impl Default for Redundancy {
    fn default() -> Self {
        Redundancy::None
    }
}

impl Redundancy {
    /// Distinct domains one extent (or one stripe) needs.
    pub fn width(&self) -> usize {
        match *self {
            Redundancy::None => 1,
            Redundancy::Mirror { copies } => copies as usize,
            Redundancy::Parity { data, parity } => data as usize + parity as usize,
        }
    }

    /// Legs of each extent: the primary plus its mirrors.
    pub fn copies(&self) -> usize {
        match *self {
            Redundancy::Mirror { copies } => copies as usize,
            _ => 1,
        }
    }

    pub fn is_parity(&self) -> bool {
        matches!(self, Redundancy::Parity { .. })
    }

    /// Bytes of physical space per byte of data, as a ratio.
    pub fn overhead(&self) -> f64 {
        match *self {
            Redundancy::None => 1.0,
            Redundancy::Mirror { copies } => copies as f64,
            Redundancy::Parity { data, parity } => (data as f64 + parity as f64) / data as f64,
        }
    }

    /// How many legs (of one extent, or members of one stripe) may be lost
    /// before data is.
    pub fn tolerates(&self) -> usize {
        match *self {
            Redundancy::None => 0,
            Redundancy::Mirror { copies } => copies as usize - 1,
            Redundancy::Parity { parity, .. } => parity as usize,
        }
    }
}

/// The whole policy: the scheme and the rung it spreads at.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RedundancyPolicy {
    pub scheme: Redundancy,
    /// Failure-domain rung legs must differ at. `drive` unless told.
    pub spread: String,
}

impl Default for RedundancyPolicy {
    fn default() -> Self {
        RedundancyPolicy { scheme: Redundancy::None, spread: DEFAULT_RUNG.to_string() }
    }
}

impl RedundancyPolicy {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn mirror(copies: u8) -> Self {
        RedundancyPolicy { scheme: Redundancy::Mirror { copies }, spread: DEFAULT_RUNG.into() }
    }

    pub fn parity(data: u8, parity: u8) -> Self {
        RedundancyPolicy {
            scheme: Redundancy::Parity { data, parity },
            spread: DEFAULT_RUNG.into(),
        }
    }

    pub fn at(mut self, rung: &str) -> Self {
        self.spread = rung.to_string();
        self
    }

    pub fn is_none(&self) -> bool {
        self.scheme == Redundancy::None
    }

    /// Parse the spellings an operator writes:
    /// `none`, `mirror` (= 2), `mirror:3`, `raid1` (= mirror:2), `raid10`
    /// (= mirror:2 — striping is what organic placement already does),
    /// `raid5:4+1`, `raid5:5` (five members, one parity), `raid6:4+2`,
    /// `raid6:6`. An `@rung` suffix sets the spread: `mirror:2@shelf`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (scheme_str, rung) = match s.split_once('@') {
            Some((a, r)) => (a.trim(), r.trim()),
            None => (s, DEFAULT_RUNG),
        };
        if rung.is_empty() {
            return Err("empty spread rung after '@'".into());
        }
        let lower = scheme_str.to_ascii_lowercase();
        let (kind, arg) = match lower.split_once(':') {
            Some((k, a)) => (k.trim(), Some(a.trim())),
            None => (lower.as_str(), None),
        };
        let scheme = match kind {
            "" | "none" | "single" => Redundancy::None,
            "mirror" | "raid1" | "raid10" | "raid-1" | "raid-10" => {
                let copies: u8 = match arg {
                    None => 2,
                    Some(a) => a.parse().map_err(|_| format!("bad mirror count '{a}'"))?,
                };
                if copies < 2 {
                    return Err("a mirror needs at least 2 copies".into());
                }
                Redundancy::Mirror { copies }
            }
            "raid5" | "raid-5" | "raid6" | "raid-6" | "parity" => {
                let default_parity = if kind.contains('6') { 2 } else { 1 };
                let (data, parity) = match arg {
                    None => return Err(format!("{kind} needs a width, e.g. {kind}:4+{default_parity}")),
                    Some(a) => match a.split_once('+') {
                        Some((d, p)) => (
                            d.trim().parse::<u8>().map_err(|_| format!("bad data width '{d}'"))?,
                            p.trim().parse::<u8>().map_err(|_| format!("bad parity width '{p}'"))?,
                        ),
                        None => {
                            let members: u8 =
                                a.parse().map_err(|_| format!("bad member count '{a}'"))?;
                            if members <= default_parity {
                                return Err(format!("{kind}:{members} leaves no data members"));
                            }
                            (members - default_parity, default_parity)
                        }
                    },
                };
                if kind == "parity" && parity == 0 {
                    return Err("parity:D+P needs P >= 1".into());
                }
                if kind.contains('5') && parity != 1 {
                    return Err("raid5 has exactly one parity leg; use raid6 for two".into());
                }
                if kind.contains('6') && parity != 2 {
                    return Err("raid6 has exactly two parity legs".into());
                }
                if data < 2 {
                    return Err("parity needs at least 2 data members".into());
                }
                if parity > 2 {
                    return Err("at most 2 parity legs (P and Q)".into());
                }
                Redundancy::Parity { data, parity }
            }
            other => return Err(format!("unknown redundancy '{other}'")),
        };
        Ok(RedundancyPolicy { scheme, spread: rung.to_string() })
    }

    /// The canonical spelling `parse` accepts back.
    pub fn spelling(&self) -> String {
        let base = match self.scheme {
            Redundancy::None => "none".to_string(),
            Redundancy::Mirror { copies } => format!("mirror:{copies}"),
            Redundancy::Parity { data, parity: 1 } => format!("raid5:{data}+1"),
            Redundancy::Parity { data, parity } => format!("raid6:{data}+{parity}"),
        };
        if self.spread == DEFAULT_RUNG || self.is_none() {
            base
        } else {
            format!("{base}@{}", self.spread)
        }
    }

    /// A rung outside the ladder is allowed (the chain keeps unknown labels)
    /// but almost always a typo, so say so.
    pub fn rung_is_known(&self) -> bool {
        RUNGS.contains(&self.spread.as_str())
    }
}

impl fmt::Display for RedundancyPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.spelling())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spellings_round_trip() {
        for s in ["none", "mirror:2", "mirror:3", "raid5:4+1", "raid6:4+2", "mirror:2@shelf"] {
            let p = RedundancyPolicy::parse(s).unwrap();
            assert_eq!(p.spelling(), s);
        }
    }

    #[test]
    fn aliases() {
        assert_eq!(RedundancyPolicy::parse("mirror").unwrap().scheme, Redundancy::Mirror { copies: 2 });
        assert_eq!(RedundancyPolicy::parse("raid1").unwrap().scheme, Redundancy::Mirror { copies: 2 });
        assert_eq!(RedundancyPolicy::parse("raid10").unwrap().scheme, Redundancy::Mirror { copies: 2 });
        assert_eq!(RedundancyPolicy::parse("raid5:5").unwrap().scheme, Redundancy::Parity { data: 4, parity: 1 });
        assert_eq!(RedundancyPolicy::parse("raid6:6").unwrap().scheme, Redundancy::Parity { data: 4, parity: 2 });
        assert_eq!(RedundancyPolicy::parse("").unwrap().scheme, Redundancy::None);
    }

    #[test]
    fn rejects_nonsense() {
        assert!(RedundancyPolicy::parse("mirror:1").is_err());
        assert!(RedundancyPolicy::parse("raid5").is_err());
        assert!(RedundancyPolicy::parse("raid5:1+1").is_err());
        assert!(RedundancyPolicy::parse("raid5:4+2").is_err());
        assert!(RedundancyPolicy::parse("raid6:4+1").is_err());
        assert!(RedundancyPolicy::parse("raid7:3").is_err());
        assert!(RedundancyPolicy::parse("mirror@").is_err());
    }

    #[test]
    fn widths_and_tolerance() {
        assert_eq!(Redundancy::Mirror { copies: 3 }.width(), 3);
        assert_eq!(Redundancy::Mirror { copies: 3 }.tolerates(), 2);
        assert_eq!(Redundancy::Parity { data: 4, parity: 1 }.width(), 5);
        assert_eq!(Redundancy::Parity { data: 4, parity: 2 }.tolerates(), 2);
        assert_eq!(Redundancy::None.tolerates(), 0);
        assert!((Redundancy::Parity { data: 4, parity: 1 }.overhead() - 1.25).abs() < 1e-9);
    }
}
