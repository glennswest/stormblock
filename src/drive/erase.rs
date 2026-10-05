//! Secure delete (#286): what a freed slot is overwritten with before it goes
//! back to the free pool.
//!
//! A deleted volume's bytes used to stay on the media until a later tenant
//! happened to write over them (a new tenant never *reads* them: a first
//! write zero-fills the whole slot, #171, but the bytes were still on the
//! platter for anyone holding the drive). With an erase level set, a slot
//! whose last reference goes is marked [`Erasing`](super::slab::SlotState::Erasing)
//! instead of free, the background eraser overwrites it, and only then is it
//! freed the ordinary, durable way. See `docs/erase.md`.
//!
//! On flash an overwrite is not a guarantee: the FTL writes the new pattern
//! to fresh cells and the old ones keep their charge until garbage-collected.
//! Only a crypto-erase or the drive's own sanitize is complete there.

use std::fmt;
use std::str::FromStr;

/// How a freed slot is overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum EraseLevel {
    /// Not overwritten: freed (and discarded) as before #286.
    #[default]
    None,
    /// One pass of zeros.
    Once,
    /// DoD 5220.22-M (E): 0x00, 0xFF, random, the last pass read back.
    Dod3,
    /// DoD 5220.22-M (ECE): 0x00, 0xFF, random, random, 0x00, 0xFF, random,
    /// the last pass read back.
    Dod7,
}

/// One overwrite pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    Byte(u8),
    Random,
}

impl EraseLevel {
    pub const ALL: [EraseLevel; 4] = [EraseLevel::None, EraseLevel::Once, EraseLevel::Dod3, EraseLevel::Dod7];

    pub fn passes(self) -> &'static [Pass] {
        use Pass::*;
        match self {
            EraseLevel::None => &[],
            EraseLevel::Once => &[Byte(0)],
            EraseLevel::Dod3 => &[Byte(0x00), Byte(0xFF), Random],
            EraseLevel::Dod7 => &[Byte(0x00), Byte(0xFF), Random, Random, Byte(0x00), Byte(0xFF), Random],
        }
    }

    /// Whether the last pass is read back and compared.
    pub fn verifies(self) -> bool {
        matches!(self, EraseLevel::Dod3 | EraseLevel::Dod7)
    }

    /// The code kept in an `Erasing` slot entry (its share-count field).
    pub fn code(self) -> u32 {
        match self {
            EraseLevel::None => 0,
            EraseLevel::Once => 1,
            EraseLevel::Dod3 => 3,
            EraseLevel::Dod7 => 7,
        }
    }

    /// The level an `Erasing` entry asks for. An unknown code is erased with
    /// the most passes: never fewer than what was asked.
    pub fn from_code(code: u32) -> EraseLevel {
        match code {
            0 | 1 => EraseLevel::Once,
            3 => EraseLevel::Dod3,
            _ => EraseLevel::Dod7,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EraseLevel::None => "none",
            EraseLevel::Once => "once",
            EraseLevel::Dod3 => "dod3",
            EraseLevel::Dod7 => "dod7",
        }
    }
}

impl fmt::Display for EraseLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EraseLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "0" => Ok(EraseLevel::None),
            "once" | "zero" | "1" => Ok(EraseLevel::Once),
            "dod3" | "dod" | "3" => Ok(EraseLevel::Dod3),
            "dod7" | "7" => Ok(EraseLevel::Dod7),
            other => Err(format!("erase level {other:?}: expected none, once, dod3 or dod7")),
        }
    }
}

impl serde::Serialize for EraseLevel {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for EraseLevel {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Fill `buf` for `pass`. A random pass draws from the OS-seeded generator.
pub fn fill(pass: Pass, buf: &mut [u8]) {
    match pass {
        Pass::Byte(b) => buf.fill(b),
        Pass::Random => rand::RngCore::fill_bytes(&mut rand::thread_rng(), buf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_round_trip_through_their_code_and_name() {
        for l in EraseLevel::ALL {
            assert_eq!(l.as_str().parse::<EraseLevel>().unwrap(), l);
            if l != EraseLevel::None {
                assert_eq!(EraseLevel::from_code(l.code()), l);
            }
        }
        assert_eq!(EraseLevel::from_code(42), EraseLevel::Dod7);
        assert!("dod5".parse::<EraseLevel>().is_err());
    }

    #[test]
    fn the_dod_levels_have_their_passes() {
        assert_eq!(EraseLevel::Once.passes(), &[Pass::Byte(0)]);
        assert_eq!(EraseLevel::Dod3.passes().len(), 3);
        assert_eq!(EraseLevel::Dod7.passes().len(), 7);
        assert_eq!(EraseLevel::Dod7.passes()[6], Pass::Random);
        assert!(EraseLevel::Dod3.verifies() && !EraseLevel::Once.verifies());
    }
}
