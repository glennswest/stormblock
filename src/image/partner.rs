//! The drive that holds a node's other half (#402).
//!
//! A node whose system half has a drive of its own (the X9 blades' SSD) and
//! whose data half stays on another (the HDD) boots from both: either one
//! alone is half a node. `rd.stormblock.slab=` names one drive, and with two
//! drives either may enumerate as `sda`. So the pairing is on the disk: the
//! system slab laid on a drive of its own records its data slab's id in its
//! header ([`Slab::partner`]). From the system drive, the data slab is looked
//! for by that id; from the data drive, the system slab that names it. Never
//! by path, which moves, and never by role, which another node's drive (a
//! shelf, a spare) can share.

use std::sync::Arc;

use crate::drive::discover::slabs_in_partitions;
use crate::drive::slab::{Slab, SlabId, SlabRole};
use crate::drive::BlockDevice;

/// The slabs on a drive: the drive itself when it is one, else its partitions'.
pub async fn slabs_on(dev: &Arc<dyn BlockDevice>) -> Vec<Slab> {
    match Slab::open(dev.clone()).await {
        Ok(s) => vec![s],
        Err(_) => slabs_in_partitions(dev).await.into_iter().map(|f| f.slab).collect(),
    }
}

/// One slab as the pairing reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlabSeen {
    pub id: SlabId,
    pub role: SlabRole,
    pub partner: Option<SlabId>,
}

/// The slabs on a drive, as the pairing reads them.
pub async fn seen_on(dev: &Arc<dyn BlockDevice>) -> Vec<SlabSeen> {
    slabs_on(dev)
        .await
        .iter()
        .map(|s| SlabSeen { id: s.slab_id(), role: s.role(), partner: s.partner() })
        .collect()
}

/// Whether a drive carrying `other` is the partner of one carrying `own`:
/// one of `own` names a slab of `other`, or one of `other` names a slab of
/// `own`.
pub fn pairs(own: &[SlabSeen], other: &[SlabSeen]) -> bool {
    let names = |a: &[SlabSeen], b: &[SlabSeen]| a.iter().any(|s| s.partner.is_some_and(|p| b.iter().any(|t| t.id == p)));
    names(own, other) || names(other, own)
}

/// Among `candidates` (device paths; `own_path` and anything that does not
/// open are passed over), the drives paired with `own`.
pub async fn find_among(own: &[SlabSeen], candidates: &[String], own_path: &str) -> Vec<String> {
    let mut out = Vec::new();
    if own.is_empty() {
        return out;
    }
    for path in candidates {
        if path == own_path {
            continue;
        }
        let Ok(dev) = crate::drive::open_path(path, true).await else { continue };
        if pairs(own, &seen_on(&dev).await) {
            out.push(path.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::local::{lay_data_drive, lay_node_slabs, lay_system_drive, LocalLayout};

    fn seen(id: u128, role: SlabRole, partner: Option<u128>) -> SlabSeen {
        SlabSeen { id: SlabId(uuid::Uuid::from_u128(id)), role, partner: partner.map(|p| SlabId(uuid::Uuid::from_u128(p))) }
    }

    #[test]
    fn a_pair_is_named_from_either_side_and_a_stranger_is_not() {
        let ssd = [seen(1, SlabRole::System, Some(2))];
        let hdd = [seen(2, SlabRole::Data, None)];
        let stranger = [seen(3, SlabRole::Data, None)];
        assert!(pairs(&ssd, &hdd) && pairs(&hdd, &ssd));
        assert!(!pairs(&ssd, &stranger) && !pairs(&hdd, &stranger));
        assert!(!pairs(&[], &hdd));
    }

    /// #402: the system slab records its data slab; each drive finds the
    /// other among the machine's drives, and a drive with a data half of
    /// another node's (the same role, no pairing) is not taken.
    #[tokio::test]
    async fn each_half_finds_the_other_by_slab_id() {
        let uri = |n: &str, size: &str| format!("emulated://{n}-{}?size={size}", uuid::Uuid::new_v4().simple());
        let (ssd_p, hdd_p, other_p) = (uri("ssd", "64G"), uri("hdd", "128G"), uri("other", "64G"));
        let ssd = crate::drive::open_path(&ssd_p, false).await.unwrap();
        let hdd = crate::drive::open_path(&hdd_p, false).await.unwrap();
        let other = crate::drive::open_path(&other_p, false).await.unwrap();
        let layout = |d: &Arc<dyn BlockDevice>| {
            let mut l = LocalLayout::for_drive(d.capacity_bytes());
            l.slot_size = 1 << 20;
            l.bulk = false;
            l
        };
        let mut sys = lay_system_drive(ssd.clone(), &layout(&ssd)).await.unwrap().system;
        let data = lay_data_drive(hdd.clone(), &layout(&hdd)).await.unwrap().data;
        let _ = lay_node_slabs(other.clone(), &layout(&other)).await.unwrap();
        assert_eq!(sys.partner(), None, "nothing paired until the install says so");
        sys.set_partner(Some(data.slab_id())).await.unwrap();
        drop(sys);

        let paths = vec![ssd_p.clone(), hdd_p.clone(), other_p.clone()];
        let from_ssd = seen_on(&ssd).await;
        assert_eq!(from_ssd.len(), 1);
        assert_eq!(from_ssd[0].partner, Some(data.slab_id()), "the header keeps it across an open");
        assert_eq!(find_among(&from_ssd, &paths, &ssd_p).await, vec![hdd_p.clone()]);
        assert_eq!(find_among(&seen_on(&hdd).await, &paths, &hdd_p).await, vec![ssd_p.clone()]);
        assert!(find_among(&seen_on(&other).await, &paths, &other_p).await.is_empty(), "a node's own one drive pairs with nothing");
    }
}
