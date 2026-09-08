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

/// A local disk the boot laid out but did not fill.
///
/// The migration is minutes of background copying and the process that laid
/// the slabs has seconds to live: it is the initramfs engine, and `switch_root`
/// deletes the filesystem its binary came from the moment the successor takes
/// the ublk devices over. Running the copy there meant it was killed part-way
/// through, every time, leaving a slab that is real, incomplete, and unable to
/// boot the node — which is the shape the local-slab probe now has to reject.
///
/// So the long-lived process does the long-running job. The boot lays the
/// structure, which is fast and bounded, writes down what it laid, and the
/// engine that adopts the devices moves the extents at its leisure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlowOver {
    /// The disk, for the log line. Nothing resolves anything through it.
    pub disk: String,
    /// The slab the goldens are migrating *into*, by id. By id rather than by
    /// role, because after the successor opens both the appliance's slabs and
    /// this disk's there are two system slabs registered and one of them is
    /// the source.
    pub system_slab: String,
    /// The local data slab, laid at the same time. Writable volumes belong on
    /// it — that is the whole point of taking the drive — and it is named here
    /// so the successor does not have to guess which of the two it is.
    pub data_slab: String,
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
    /// A local disk laid out by this boot, waiting to be filled. Absent on a
    /// node that has no local disk, and on every record written before this
    /// field existed — which is why it defaults rather than being required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_over: Option<FlowOver>,
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
            flow_over: None,
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
    fn a_record_without_a_flow_over_still_reads() {
        // Every record written before the field existed lacks it, and a node
        // mid-upgrade reads one of those with a binary that has it. Absence
        // has to mean "no local disk", not "unreadable record" — which would
        // send the successor to the explicit --volume list and stand down
        // every device it was supposed to adopt.
        let json = br#"{"slabs":["/dev/sda4"],"devices":[{"dev_id":0,"volume":"stormpump"}]}"#;
        let rec: Record = serde_json::from_slice(json).expect("reads without flow_over");
        assert_eq!(rec.flow_over, None);
        assert_eq!(rec.volumes_in_device_order(), vec!["stormpump"]);
    }

    #[test]
    fn a_flow_over_round_trips() {
        let mut rec = a_record();
        rec.flow_over = Some(FlowOver {
            disk: "/dev/sda".into(),
            system_slab: "8aa6b985-3f4d-4130-99cb-154b56dcb68b".into(),
            data_slab: "f86ee673-57da-4aa5-961c-168c263de265".into(),
        });
        let bytes = serde_json::to_vec(&rec).expect("encodes");
        assert_eq!(serde_json::from_slice::<Record>(&bytes).expect("decodes"), rec);
    }

    #[test]
    fn a_missing_record_is_not_an_error() {
        // The fallback is the explicit --volume list, so absence has to be
        // reported as absence rather than as a failure.
        assert_eq!(Record::read(std::path::Path::new("/nonexistent/handover.json")), None);
    }
}
