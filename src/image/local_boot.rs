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
        let bs = gpt.block_size;
        let found = gpt
            .partitions()
            .find(|(_, e)| e.type_guid == type_guid::ESP)
            .map(|(_, e)| (e.start_bytes(bs), e.size_bytes(bs)));
        if let Some((start, len)) = found {
            return Some((dev.clone(), start, len));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::image::local::{has_boot_area, lay_node_slabs, LocalLayout};
    use crate::pallet::manager::PublishSpec;
    use crate::pallet::{BytesContent, MemberKind, MemberSpec, PalletManager, PalletStore};

    const MIB: u64 = 1024 * 1024;
    const DISK: u64 = 512 * MIB;

    fn esp_files(tag: &str) -> Vec<(String, Vec<u8>)> {
        vec![
            (
                "EFI/BOOT/BOOTX64.EFI".into(),
                format!("stormuefi {tag} ").into_bytes().repeat(5000),
            ),
            ("loader/entries/stormcos-long-name.conf".into(), format!("title {tag}\n").into_bytes()),
        ]
    }

    /// An image the way `image build` lays one for a node to attach: a GPT and
    /// an ESP at 4096-byte sectors, which is what NVMe/TCP presents, and a
    /// boot pallet at the top of the ladder.
    async fn image(dir: &tempfile::TempDir, name: &str, tag: &str) -> (String, Arc<dyn BlockDevice>) {
        let path = dir.path().join(name).to_string_lossy().to_string();
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(&path, 256 * MIB).await.unwrap());
        let mut gpt = Gpt::create_with_lba(&dev, 4096);
        let i = gpt.allocate("EFI", type_guid::ESP, 32 * MIB, 0).unwrap();
        gpt.write(&dev).await.unwrap();
        let e = gpt.entries[i].clone();
        let part = Arc::new(
            PartitionDevice::new(dev.clone(), e.start_bytes(4096), e.size_bytes(4096)).unwrap(),
        );
        fat::format_from_tree(part, &esp_files(tag), &[], "EFI").await.unwrap();

        let mut store = PalletStore::new(Vec::new());
        store.add_drive(path.clone(), dev.clone());
        let mgr = PalletManager::new(store);
        let mut spec = PublishSpec::new("kernel1", PalletKind::Boot)
            .member(MemberSpec::new(
                "kernel",
                "kernel",
                MemberKind::Kernel,
                Arc::new(BytesContent(format!("vmlinuz {tag} ").into_bytes().repeat(20_000))),
            ))
            .member(MemberSpec::new(
                "cmdline",
                "cmdline",
                MemberKind::BootConfig,
                Arc::new(BytesContent(b"root=/dev/ublkb0 rd.stormblock.slab=/dev/sda".to_vec())),
            ));
        spec.version_label = tag.into();
        spec.priority = Some(15);
        mgr.publish(spec).await.unwrap();
        (path, dev)
    }

    /// A node's disk as a flow-over lays it: at 512-byte LBAs, the way a real
    /// drive is read by firmware, with a boot area in front.
    async fn node_disk(dir: &tempfile::TempDir) -> (String, Arc<dyn BlockDevice>) {
        let path = dir.path().join("node.disk").to_string_lossy().to_string();
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(&path, DISK).await.unwrap());
        let mut layout = LocalLayout::for_drive(DISK);
        layout.slot_size = MIB;
        layout.lba = Some(512);
        layout.boot_bytes = 128 * MIB;
        lay_node_slabs(dev.clone(), &layout).await.unwrap();
        assert!(has_boot_area(&dev, layout.boot_bytes).await);
        (path, dev)
    }

    async fn esp_of(dev: &Arc<dyn BlockDevice>) -> Option<(fat::FatTree, u16)> {
        let gpt = Gpt::read(dev).await.unwrap();
        let bs = gpt.block_size;
        let (s, l) = gpt
            .partitions()
            .find(|(_, e)| e.type_guid == type_guid::ESP)
            .map(|(_, e)| (e.start_bytes(bs), e.size_bytes(bs)))?;
        let part: Arc<dyn BlockDevice> =
            Arc::new(PartitionDevice::with_block_size(dev.clone(), s, l, bs).unwrap());
        let mut boot = vec![0u8; 512];
        part.read(0, &mut boot).await.unwrap();
        Some((fat::read_tree(part).await.unwrap(), u16::from_le_bytes([boot[11], boot[12]])))
    }

    /// The whole point: after this, the disk carries an ESP firmware can read
    /// and a boot pallet stormuefi will select — and the slabs are where they
    /// were.
    #[tokio::test]
    async fn a_laid_disk_boots_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let (ipath, idev) = image(&dir, "a.img", "A").await;
        let (dpath, ddev) = node_disk(&dir).await;
        let before = node_layout(&ddev).await.unwrap().unwrap();

        let r = lay_local_boot(&dpath, ddev.clone(), vec![(ipath, idev)]).await.unwrap();
        assert!(r.bootable(), "{r:?}");
        assert_eq!(
            r.esp,
            EspOutcome::Rebuilt { from_sector: 4096, to_sector: 512, files: 2 },
            "the image's ESP is at 4096 and the disk is read at 512"
        );
        assert_eq!(r.copied.len(), 1);
        assert_eq!(r.ladder, vec![("kernel1".to_string(), 1, LOCAL_TOP)]);

        // The ESP: typed, named, and declaring the disk's own sector size.
        let (tree, sector) = esp_of(&ddev).await.expect("an ESP");
        assert_eq!(sector, 512, "a FAT must declare its medium's sector size");
        let upper = |v: &[(String, Vec<u8>)]| {
            let mut v: Vec<(String, Vec<u8>)> =
                v.iter().map(|(p, b)| (p.to_ascii_uppercase(), b.clone())).collect();
            v.sort();
            v
        };
        assert_eq!(upper(&tree.files), upper(&esp_files("A")));

        // The pallet verifies where it landed, and it is in the boot area:
        // in front of the system slab.
        let mut store = PalletStore::new(Vec::new());
        store.add_drive(dpath.clone(), ddev.clone());
        let mgr = PalletManager::new(store);
        let local = mgr.list().await;
        assert_eq!(local.len(), 1);
        assert!(mgr.verify(local[0].id).await.unwrap().ok);
        let gpt = Gpt::read(&ddev).await.unwrap();
        assert_eq!(gpt.block_size, 512);
        let (d, s) = node_layout(&ddev).await.unwrap().unwrap();
        assert_eq!((d, s), before, "the slabs' entries did not move");
        assert!(local[0].start_bytes < gpt.entries[s].start_bytes(512));
        assert!(
            gpt.partitions().all(|(_, e)| e.last_lba <= gpt.entries[d].last_lba),
            "the data half is still last, so it can still grow"
        );
    }

    #[tokio::test]
    async fn a_second_run_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (ipath, idev) = image(&dir, "a.img", "A").await;
        let (dpath, ddev) = node_disk(&dir).await;
        lay_local_boot(&dpath, ddev.clone(), vec![(ipath.clone(), idev.clone())]).await.unwrap();
        let r = lay_local_boot(&dpath, ddev.clone(), vec![(ipath, idev)]).await.unwrap();
        assert_eq!(r.esp, EspOutcome::Unchanged);
        assert!(r.copied.is_empty());
        assert_eq!(r.already, 1);
        assert!(r.removed.is_empty());
        assert_eq!(r.ladder.len(), 1);
    }

    /// The next release goes on top, the previous one stays as its fallback,
    /// and the one before that is dropped — the A/B ladder an upgrade writes.
    #[tokio::test]
    async fn a_new_release_goes_on_top_and_the_old_one_stays_as_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let (dpath, ddev) = node_disk(&dir).await;
        for tag in ["A", "B", "C"] {
            let (ipath, idev) = image(&dir, &format!("{tag}.img"), tag).await;
            let r = lay_local_boot(&dpath, ddev.clone(), vec![(ipath, idev)]).await.unwrap();
            assert_eq!(r.copied.len(), 1, "{tag}: {r:?}");
            assert!(r.failed.is_empty(), "{tag}: {r:?}");
            let (tree, _) = esp_of(&ddev).await.unwrap();
            assert!(
                tree.files.iter().any(|(_, d)| d.starts_with(format!("stormuefi {tag}").as_bytes())),
                "{tag}: the ESP is the newest release's"
            );
        }
        let mut store = PalletStore::new(Vec::new());
        store.add_drive(dpath, ddev.clone());
        let mgr = PalletManager::new(store);
        let mut local = mgr.list().await;
        local.sort_by_key(|p| std::cmp::Reverse(p.attributes.priority));
        let labels: Vec<(String, u8)> =
            local.iter().map(|p| (p.version_label.clone(), p.attributes.priority)).collect();
        assert_eq!(labels, vec![("C".to_string(), LOCAL_TOP), ("B".to_string(), LOCAL_TOP - 1)]);
    }

    /// stormuefi scans every device. When a node netboots, the image it
    /// attached has to win over whatever the disk carries — otherwise a
    /// reinstall onto a new release could start the old kernel.
    #[tokio::test]
    async fn the_attached_image_outranks_the_disk() {
        let dir = tempfile::tempdir().unwrap();
        let (ipath, idev) = image(&dir, "a.img", "A").await;
        let (dpath, ddev) = node_disk(&dir).await;
        lay_local_boot(&dpath, ddev.clone(), vec![(ipath.clone(), idev.clone())]).await.unwrap();

        let mut store = PalletStore::new(Vec::new());
        store.add_drive(dpath, ddev);
        store.add_drive(ipath, idev);
        let mut all = PalletManager::new(store).list().await;
        all.sort_by_key(|p| std::cmp::Reverse(p.order_key()));
        assert_eq!(all[0].drive_index, 1, "the image's boot pallet is selected first");
        assert_eq!(all[1].drive_index, 0);
    }

    /// A copy interrupted before its ESP was finished leaves a partition that
    /// is not typed ESP — firmware passes over it — and the next run finishes
    /// the job in the same place.
    #[tokio::test]
    async fn an_interrupted_esp_is_not_an_esp_and_is_finished_next_time() {
        let dir = tempfile::tempdir().unwrap();
        let (ipath, idev) = image(&dir, "a.img", "A").await;
        let (dpath, ddev) = node_disk(&dir).await;

        let i = reserve_esp(&ddev, 32 * MIB).await.unwrap();
        assert!(esp_of(&ddev).await.is_none(), "a pending ESP is not an ESP");

        lay_local_boot(&dpath, ddev.clone(), vec![(ipath, idev)]).await.unwrap();
        let gpt = Gpt::read(&ddev).await.unwrap();
        assert_eq!(gpt.entries[i].type_guid, type_guid::ESP, "finished in the same entry");
        assert_eq!(gpt.entries[i].name, ESP_NAME);
        assert_eq!(
            gpt.partitions().filter(|(_, e)| e.name == ESP_NAME || e.name == ESP_PENDING).count(),
            1
        );
    }

    /// Same sector size on both sides: the ESP is copied as it is.
    #[tokio::test]
    async fn a_matching_sector_size_is_a_byte_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (ipath, idev) = image(&dir, "a.img", "A").await;
        let path = dir.path().join("4kn.disk").to_string_lossy().to_string();
        let ddev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open_with_capacity(&path, DISK).await.unwrap());
        let mut layout = LocalLayout::for_drive(DISK);
        layout.slot_size = MIB;
        layout.lba = Some(4096);
        layout.boot_bytes = 128 * MIB;
        lay_node_slabs(ddev.clone(), &layout).await.unwrap();

        let r = lay_local_boot(&path, ddev.clone(), vec![(ipath, idev)]).await.unwrap();
        assert_eq!(r.esp, EspOutcome::Copied { bytes: 32 * MIB });
        let (_, sector) = esp_of(&ddev).await.unwrap();
        assert_eq!(sector, 4096);
    }
}
