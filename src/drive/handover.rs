//! What the next server needs to know, written by the one that has it.
//!
//! A ublk device outlives the process that created it, which is what makes a
//! handover possible — but the kernel remembers only the device, not what is
//! behind it. It can say `/dev/ublkb4` exists and which pid serves it; it has
//! no idea that it is the volume called `stormblock-data`.
//!
//! So the server that creates the devices writes the mapping down, and the
//! server that adopts them reads it. Before this, the list was maintained by
//! hand in two places — `rd.stormblock.mount=` on the kernel command line and
//! the `--volume` list in the boot unit — which had to agree exactly and in
//! order. They stopped agreeing the first time the node gained a volume:
//! standing the incumbent down stops **every** device it serves, so the two
//! that were left off the list were abandoned mounted, returning EIO, and the
//! engine could not even be restarted because its own root was among them.
//!
//! Two hand-written lists that must agree is a defect whatever they contain.
//! There is one list now, on the kernel command line, and everything after it
//! is derived.
//!
//! **In `/run`, deliberately.** The mapping is true for this boot and no
//! other: device ids are assigned in creation order each time. `/run` is tmpfs
//! and the initramfs moves it into the new root across `switch_root`, so the
//! record survives exactly as long as it is true. Putting it on the slab would
//! outlive its own accuracy.

use serde::{Deserialize, Serialize};

/// Where the record lives. In `/run` because it is per-boot state; see above.
pub const DEFAULT_PATH: &str = "/run/stormblock/handover.json";

/// One exported device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Device {
    /// The ublk device id — `/dev/ublkb{dev_id}`.
    pub dev_id: u32,
    /// The volume behind it, by name. A name rather than a UUID because it is
    /// what the node's operator and its logs both use, and it is resolved
    /// through the same metadata the successor has already loaded.
    pub volume: String,
}

/// Everything the successor needs to take over without being told.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Record {
    /// The slab(s) the volumes live on, as they were opened.
    pub slabs: Vec<String>,
    /// An explicit metadata directory, if one was used. Normally absent: a
    /// slab built by `image build` carries its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<String>,
    /// Every device that was exported, in device order.
    pub devices: Vec<Device>,
}

impl Record {
    /// The volume names in device order, which is the order an adopting server
    /// must present them in.
    pub fn volumes_in_device_order(&self) -> Vec<String> {
        let mut d = self.devices.clone();
        d.sort_by_key(|e| e.dev_id);
        d.into_iter().map(|e| e.volume).collect()
    }

    /// Write it where the successor will look.
    ///
    /// Atomically, because the successor may start at any moment: a torn
    /// record would be worse than none, since none falls back to the explicit
    /// list and half a record does not.
    pub fn write(&self, path: &std::path::Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::other(format!("encode handover record: {e}")))?;
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, path)
    }

    /// Read it, or `None` when there is none — which is not an error. A node
    /// where the devices were created by something that predates this record
    /// still adopts, from the explicit list.
    pub fn read(path: &std::path::Path) -> Option<Record> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_record() -> Record {
        Record {
            slabs: vec!["/dev/sda4".into()],
            meta: None,
            devices: vec![
                Device { dev_id: 0, volume: "stormpump".into() },
                Device { dev_id: 2, volume: "sbregistry".into() },
                Device { dev_id: 1, volume: "stormblock".into() },
            ],
        }
    }

    #[test]
    fn volumes_come_back_in_device_order() {
        // Written in whatever order the exports were assembled; read back in
        // the order the kernel numbered them, because that is the order an
        // adopting server has to hand them over in.
        assert_eq!(
            a_record().volumes_in_device_order(),
            vec!["stormpump", "stormblock", "sbregistry"]
        );
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("sb-handover-{}", std::process::id()));
        let path = dir.join("handover.json");
        let rec = a_record();
        rec.write(&path).expect("writes");
        assert_eq!(Record::read(&path), Some(rec));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_record_is_not_an_error() {
        // The fallback is the explicit --volume list, so absence has to be
        // reported as absence rather than as a failure.
        assert_eq!(Record::read(std::path::Path::new("/nonexistent/handover.json")), None);
    }
}
