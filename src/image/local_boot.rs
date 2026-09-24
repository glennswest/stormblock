//! Making an installed disk boot on its own (#123).
//!
//! A flow-over lays two slabs on a node's drive — the goldens and the data —
//! and nothing that firmware can start. So every cold boot of an installed
//! node still goes through the network claim, and with the appliance down it
//! falls through to a disk that has nothing bootable on it.
//!
//! The loader is already running on every boot: stormbootx attaches the image
//! and starts `\EFI\BOOT\BOOTX64.EFI` from it, which is **stormuefi**. It scans
//! every block device for `kind = boot` pallets, verifies them and starts the
//! best one. So a disk that boots is a disk carrying the same two things the
//! image does:
//!
//! - an **ESP** holding stormuefi, and
//! - the release's **boot pallet(s)** — kernel, initramfs, command line.
//!
//! Both are copied off the image this node netbooted, into the boot area
//! [`super::local::lay_node_slabs`] leaves at the front of the drive. The
//! command line already names the local disk first
//! (`rd.stormblock.slab=/dev/sda`), so a kernel started from here finds its
//! slabs where the flow-over put them.
//!
//! **One ladder, not two.** A/B is the running-upgrade path (#122): the next
//! release is staged beside the current one and the previous one stays as
//! the fallback. What is laid here is exactly that ladder — pallets whose GPT
//! attributes carry priority, tries and the successful bit — so an upgrade
//! adds to it rather than replacing a second selection mechanism.
//!
//! **The network outranks the disk.** stormuefi scans every device, so a node
//! netbooting a new release also sees the local copy of the old one. Local
//! boot pallets are therefore ranked from [`LOCAL_TOP`], one below the top an
//! image publishes at: when the image is attached, its own boot pallet wins;
//! when it is not, the newest local one does.
//!
//! **Nothing here can make a node unbootable.** A pallet is invisible to the
//! ladder until it has verified at its destination (`copy_pallet`). The ESP is
//! typed as a plain data partition while it is written and becomes an ESP only
//! once its contents read back identical to the source's — so an interrupted
//! copy is a disk with no ESP, which firmware skips on its way to the network,
//! and the next boot does it again.

use std::collections::HashSet;
use std::sync::Arc;

use crate::drive::partition::PartitionDevice;
use crate::drive::BlockDevice;
use crate::image::type_guid;
use crate::pallet::gpt::Gpt;
use crate::pallet::manager::{PalletManager, DEFAULT_TRIES};
use crate::pallet::{PalletKind, PalletLocation, PalletStore};

use super::local::node_layout;
use super::{fat, ImageError, Result};

/// Where the local ladder starts: one below the top priority a published
/// pallet takes, so an attached image's own boot pallet always outranks a
/// local copy (see the module docs).
pub const LOCAL_TOP: u8 = 14;
/// How many boot pallets a disk keeps: the one it boots and the one it falls
/// back to.
pub const LOCAL_KEEP: usize = 2;
/// The ESP's partition name while it is being written. Typed BASIC under this
/// name, it is not an ESP to anything; renamed `EFI` and typed ESP when done.
const ESP_PENDING: &str = "EFI-pending";
const ESP_NAME: &str = "EFI";
const COPY_CHUNK: usize = 1024 * 1024;

/// What happened to the ESP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EspOutcome {
    /// None of the sources carries one.
    NoSource,
    /// The disk's ESP already holds exactly what the source's does.
    Unchanged,
    /// Copied byte for byte: the two media share a sector size.
    Copied { bytes: u64 },
    /// Read out and laid down again at the disk's sector size.
    Rebuilt { from_sector: u32, to_sector: u32, files: usize },
}

/// What [`lay_local_boot`] did.
#[derive(Debug, Clone)]
pub struct LocalBootReport {
    pub esp: EspOutcome,
    /// Boot pallets copied onto the disk, as `name vN (label)`.
    pub copied: Vec<String>,
    /// Boot pallets the source carries that the disk already had.
    pub already: usize,
    /// Boot pallets removed from the disk, oldest first, to keep
    /// [`LOCAL_KEEP`] or to make room.
    pub removed: Vec<String>,
    /// The disk's boot ladder afterwards: name, version, priority.
    pub ladder: Vec<(String, u64, u8)>,
    /// Source boot pallets that could not be copied, and why.
    pub failed: Vec<String>,
}

impl LocalBootReport {
    /// Whether the disk can start a kernel on its own now.
    pub fn bootable(&self) -> bool {
        !matches!(self.esp, EspOutcome::NoSource) && !self.ladder.is_empty()
    }
}

fn describe(p: &PalletLocation) -> String {
    if p.version_label.is_empty() {
        format!("{} v{}", p.name, p.version)
    } else {
        format!("{} v{} ({})", p.name, p.version, p.version_label)
    }
}

/// Lay the ESP and the boot pallets of `sources` onto `disk`, a drive that
/// already carries a node layout with a boot area.
///
/// `sources` are the drives this node booted from — the attached image — as
/// `(path, device)`. Idempotent: a disk that already holds the sources' boot
/// pallets and ESP is left exactly as it is.
pub async fn lay_local_boot(
    disk_path: &str,
    disk: Arc<dyn BlockDevice>,
    sources: Vec<(String, Arc<dyn BlockDevice>)>,
) -> Result<LocalBootReport> {
    if node_layout(&disk).await?.is_none() {
        return Err(ImageError::Spec(format!("{disk_path} does not carry a node layout")));
    }

    // 1. The source's ESP, and room for ours. The room is taken first so the
    //    ESP is the first thing in the boot area, ahead of the pallets.
    let src_esp = find_esp(&sources).await;
    let mut esp_index = None;
    if let Some((_, _, len)) = &src_esp {
        esp_index = Some(reserve_esp(&disk, *len).await?);
    }

    // 2. The boot pallets.
    let mut report = LocalBootReport {
        esp: EspOutcome::NoSource,
        copied: Vec::new(),
        already: 0,
        removed: Vec::new(),
        ladder: Vec::new(),
        failed: Vec::new(),
    };
    let mut store = PalletStore::new(Vec::new());
    store.add_drive(disk_path, disk.clone());
    for (path, dev) in &sources {
        store.add_drive(path.clone(), dev.clone());
    }
    let mgr = PalletManager::new(store);

    let mut wanted: Vec<PalletLocation> = boot_pallets(&mgr, |d| d != 0).await;
    wanted.sort_by_key(|p| std::cmp::Reverse(p.order_key()));
    let mut wanted_digests: Vec<[u8; 32]> = Vec::new();
    for p in &wanted {
        wanted_digests.push(digest(&mgr, p).await?);
    }

    for (src, want) in wanted.iter().zip(wanted_digests.iter()) {
        let mut have = HashSet::new();
        for p in boot_pallets(&mgr, |d| d == 0).await {
            have.insert(digest(&mgr, &p).await?);
        }
        if have.contains(want) {
            report.already += 1;
            continue;
        }
        loop {
            match mgr.copy_pallet(src.id, 0).await {
                Ok(_) => {
                    report.copied.push(describe(src));
                    break;
                }
                Err(crate::pallet::PalletError::NoSpace { .. }) => {
                    // Make room by dropping the lowest-ranked local pallet the
                    // sources do not carry — never one they do, and never the
                    // last one standing.
                    match evictable(&mgr, &wanted_digests).await? {
                        Some(victim) => {
                            remove(&disk, &victim).await?;
                            report.removed.push(describe(&victim));
                        }
                        None => {
                            report.failed.push(format!(
                                "{}: no room in the boot area",
                                describe(src)
                            ));
                            break;
                        }
                    }
                }
                Err(e) => {
                    report.failed.push(format!("{}: {e}", describe(src)));
                    break;
                }
            }
        }
    }

    // 3. The ladder: the sources' pallets on top in their own order, then
    //    whatever the disk already carried, then nothing past LOCAL_KEEP.
    let mut local = boot_pallets(&mgr, |d| d == 0).await;
    let mut ranked: Vec<(usize, PalletLocation)> = Vec::new();
    for p in local.drain(..) {
        let d = digest(&mgr, &p).await?;
        let rank = wanted_digests.iter().position(|w| *w == d).unwrap_or(usize::MAX);
        ranked.push((rank, p));
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.order_key().cmp(&a.1.order_key())));
    let mut gpt = Gpt::read(&disk).await?;
    let mut keep = Vec::new();
    for (i, (rank, p)) in ranked.iter().enumerate() {
        if i >= LOCAL_KEEP && *rank == usize::MAX {
            gpt.remove(p.entry_index)?;
            report.removed.push(describe(p));
            continue;
        }
        let mut a = p.attributes;
        a.priority = LOCAL_TOP.saturating_sub(keep.len() as u8).max(1);
        if !a.successful && a.tries_left == 0 {
            a.tries_left = DEFAULT_TRIES;
        }
        gpt.entries[p.entry_index].attributes = a.to_u64();
        keep.push((p.name.clone(), p.version, a.priority));
    }
    gpt.write(&disk).await?;
    report.ladder = keep;

    // 4. The ESP last, so the disk becomes bootable only once there is
    //    something on it to boot.
    if let (Some((src_dev, start, len)), Some(i)) = (src_esp, esp_index) {
        report.esp = fill_esp(&disk, i, src_dev, start, len).await?;
    }
    Ok(report)
}

/// The first ESP among the sources: `(device, start, len)` in bytes.
async fn find_esp(
    sources: &[(String, Arc<dyn BlockDevice>)],
) -> Option<(Arc<dyn BlockDevice>, u64, u64)> {
    for (_, dev) in sources {
        let Ok(gpt) = Gpt::read(dev).await else { continue };
        if let Some((_, e)) = gpt.partitions().find(|(_, e)| e.type_guid == type_guid::ESP) {
            let bs = gpt.block_size;
            return Some((dev.clone(), e.start_bytes(bs), e.size_bytes(bs)));
        }
    }
    None
}

/// The disk's ESP entry: the finished one, a pending one a previous run left,
/// or a new pending one of `len` bytes in the first free run.
async fn reserve_esp(disk: &Arc<dyn BlockDevice>, len: u64) -> Result<usize> {
    let mut gpt = Gpt::read(disk).await?;
    if let Some((i, _)) = gpt.partitions().find(|(_, e)| e.type_guid == type_guid::ESP) {
        return Ok(i);
    }
    if let Some((i, _)) = gpt
        .partitions()
        .find(|(_, e)| e.type_guid == type_guid::BASIC && e.name == ESP_PENDING)
    {
        return Ok(i);
    }
    let i = gpt.allocate(ESP_PENDING, type_guid::BASIC, len, 0).map_err(|e| {
        ImageError::Other(format!("no room for a {len}-byte ESP in the boot area: {e}"))
    })?;
    gpt.write(disk).await?;
    Ok(i)
}

async fn boot_pallets(mgr: &PalletManager, on: impl Fn(usize) -> bool) -> Vec<PalletLocation> {
    mgr.store()
        .scan()
        .await
        .into_iter()
        .filter(|p| on(p.drive_index) && p.kind == PalletKind::Boot && p.is_readable())
        .filter(|p| !p.is_whole_drive())
        .collect()
}

async fn digest(mgr: &PalletManager, p: &PalletLocation) -> Result<[u8; 32]> {
    Ok(mgr.store().open(p).await?.sb.manifest_digest)
}

/// The lowest-ranked local boot pallet that is not one of `wanted`, if there
/// is more than one local boot pallet to choose from.
async fn evictable(mgr: &PalletManager, wanted: &[[u8; 32]]) -> Result<Option<PalletLocation>> {
    let mut local = boot_pallets(mgr, |d| d == 0).await;
    if local.is_empty() {
        return Ok(None);
    }
    local.sort_by_key(|p| p.order_key());
    for p in local {
        if !wanted.contains(&digest(mgr, &p).await?) {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

async fn remove(disk: &Arc<dyn BlockDevice>, p: &PalletLocation) -> Result<()> {
    let mut gpt = Gpt::read(disk).await?;
    gpt.remove(p.entry_index)?;
    gpt.write(disk).await?;
    Ok(())
}

/// Fill the disk's ESP (entry `i`) from the source's, unless it already holds
/// the same files.
async fn fill_esp(
    disk: &Arc<dyn BlockDevice>,
    i: usize,
    src: Arc<dyn BlockDevice>,
    src_start: u64,
    src_len: u64,
) -> Result<EspOutcome> {
    let src_part: Arc<dyn BlockDevice> = Arc::new(PartitionDevice::new(src, src_start, src_len)?);
    let want = fat::read_tree(src_part.clone()).await?;

    let mut gpt = Gpt::read(disk).await?;
    let lba = gpt.block_size;
    let e = gpt.entries[i].clone();
    let (start, len) = (e.start_bytes(lba), e.size_bytes(lba));
    let dest = || -> Result<Arc<dyn BlockDevice>> {
        Ok(Arc::new(PartitionDevice::with_block_size(disk.clone(), start, len, lba)?))
    };

    if e.type_guid == type_guid::ESP {
        if let Ok(have) = fat::read_tree(dest()?).await {
            if same_tree(&have, &want) {
                return Ok(EspOutcome::Unchanged);
            }
        }
        // Not an ESP while it is rewritten: firmware skips it, and boots the
        // network instead of half a loader.
        gpt.entries[i].type_guid = type_guid::BASIC;
        gpt.entries[i].name = ESP_PENDING.into();
        gpt.write(disk).await?;
    }

    let src_sector = {
        let mut b = vec![0u8; 512];
        crate::pallet::PartitionView::whole(src_part.clone()).read_at(0, &mut b).await?;
        u16::from_le_bytes([b[11], b[12]]) as u32
    };
    let outcome = if src_sector == lba && len >= src_len {
        let view_src = crate::pallet::PartitionView::whole(src_part.clone());
        let view_dst = crate::pallet::PartitionView::whole(dest()?);
        let mut buf = vec![0u8; COPY_CHUNK];
        let mut off = 0u64;
        while off < src_len {
            let take = ((src_len - off) as usize).min(COPY_CHUNK);
            view_src.read_at(off, &mut buf[..take]).await?;
            view_dst.write_at(off, &buf[..take]).await?;
            off += take as u64;
        }
        EspOutcome::Copied { bytes: src_len }
    } else {
        // A FAT declares its medium's sector size, and firmware will not
        // mount one that disagrees with the disk it is on — so an ESP served
        // at 4096 is read out and laid down again at this disk's size.
        let label = if want.label.is_empty() { ESP_NAME.to_string() } else { want.label.clone() };
        fat::format_from_tree(dest()?, &want.files, &want.dirs, &label).await?;
        EspOutcome::Rebuilt { from_sector: src_sector, to_sector: lba, files: want.files.len() }
    };
    disk.flush().await?;

    let have = fat::read_tree(dest()?).await?;
    if !same_tree(&have, &want) {
        return Err(ImageError::Other(
            "the ESP written to the disk does not read back as the source's; left typed as \
             a plain partition, so firmware will not try it"
                .into(),
        ));
    }

    let mut gpt = Gpt::read(disk).await?;
    gpt.entries[i].type_guid = type_guid::ESP;
    gpt.entries[i].name = ESP_NAME.into();
    gpt.write(disk).await?;
    Ok(outcome)
}

/// Same files with the same bytes, and the same directories — the label and
/// the order entries happen to be in do not decide whether a loader boots.
fn same_tree(a: &fat::FatTree, b: &fat::FatTree) -> bool {
    let norm = |t: &fat::FatTree| {
        let mut f: Vec<(String, Vec<u8>)> =
            t.files.iter().map(|(p, d)| (p.to_ascii_uppercase(), d.clone())).collect();
        f.sort();
        let mut d: Vec<String> = t.dirs.iter().map(|p| p.to_ascii_uppercase()).collect();
        d.sort();
        (f, d)
    };
    norm(a) == norm(b)
}
