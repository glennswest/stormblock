//! ISO9660 with El Torito, and the partitions appended behind it.
//!
//! A bootable ISO here is deliberately the *same image* seen two ways. The
//! ISO9660 filesystem at the front is what an optical reader and firmware's
//! El Torito path understand; the GPT written into the ISO's 32 KiB system
//! area describes the very same bytes as partitions, so writing the file to a
//! USB stick gives a disk whose ESP and pallets are exactly where the disk
//! build put them. That is what `xorriso -append_partition` does, and the
//! reason it works is the reason pallets are partition-relative in the first
//! place: nothing inside them refers to where they are.
//!
//! ```text
//! sector 0..15   system area — protective MBR + primary GPT (32 KiB)
//! sector 16      primary volume descriptor
//! sector 17      boot record volume descriptor (El Torito)
//! sector 18      terminator
//! sector 19      boot catalog
//! sector 20/21   path tables (L and M)
//! sector 22      root directory
//! …              each partition, 1 MiB aligned, as a file *and* a GPT entry
//! end            backup GPT
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;

use crate::drive::filedev::FileDevice;
use crate::drive::BlockDevice;
use crate::pallet::gpt::GptEntry;
use crate::pallet::{Gpt, PartitionView};

use super::{type_guid, ImageError, Result};

/// ISO9660 logical block size. Not negotiable: readers assume it.
const ISO_SECTOR: u64 = 2048;
const SYSTEM_AREA_SECTORS: u64 = 16;
const BOOT_CATALOG_SECTOR: u64 = 19;
const L_PATH_TABLE_SECTOR: u64 = 20;
const M_PATH_TABLE_SECTOR: u64 = 21;
const ROOT_DIR_SECTOR: u64 = 22;
const FIRST_FILE_SECTOR: u64 = 24;

/// Partitions inside the ISO are 1 MiB aligned, as on a disk.
const ALIGN: u64 = 1024 * 1024;
/// Room for the backup GPT at the end of the file.
const BACKUP_GPT_BYTES: u64 = 33 * 512 + 512;
const COPY_CHUNK: usize = 4 * 1024 * 1024;

/// El Torito platform id for UEFI.
const PLATFORM_EFI: u8 = 0xEF;

struct Placed {
    name: String,
    iso_name: String,
    iso_sector: u64,
    len: u64,
    src_start: u64,
    type_guid: [u8; 16],
    attributes: u64,
    is_esp: bool,
}

/// What to carry into the ISO.
#[derive(Debug, Clone, Copy, Default)]
pub struct IsoOptions {
    /// Carry the slab as well. Off by default: a slab is the *mutable* end of
    /// a disk and starts out empty, so including it turns a 35 MB image into a
    /// 320 MB one made mostly of zeros. An installer formats one on the target
    /// disk; a live image that genuinely ships content in its slab can ask.
    pub include_slab: bool,
}

/// Build an ISO from a finished raw image.
pub async fn from_image(raw: &Path, out: &Path) -> Result<PathBuf> {
    from_image_with(raw, out, IsoOptions::default()).await
}

/// Build an ISO, choosing what comes along.
pub async fn from_image_with(raw: &Path, out: &Path, opts: IsoOptions) -> Result<PathBuf> {
    let src: Arc<dyn BlockDevice> = Arc::new(
        FileDevice::open(
            raw.to_str()
                .ok_or_else(|| ImageError::Spec("image path is not UTF-8".into()))?,
        )
        .await?,
    );
    let gpt = Gpt::read(&src).await?;
    let lba = gpt.block_size as u64;

    let mut placed: Vec<Placed> = Vec::new();
    let mut cursor = FIRST_FILE_SECTOR * ISO_SECTOR;
    let mut n = 0;
    for (_, e) in gpt.partitions() {
        if (e.type_guid == type_guid::SLAB || e.type_guid == type_guid::SLAB_DATA)
            && !opts.include_slab
        {
            tracing::info!(
                "leaving the slab '{}' out of the ISO ({} bytes of empty space); \
                 pass include_slab to carry it",
                e.name,
                e.size_bytes(gpt.block_size)
            );
            continue;
        }
        let len = e.size_bytes(gpt.block_size);
        let start = e.first_lba * lba;
        let is_esp = e.type_guid == type_guid::ESP;
        let iso_name = iso_name_for(&e.name, is_esp, e.type_guid == crate::pallet::PALLET_TYPE_GUID, n);
        n += 1;
        cursor = cursor.div_ceil(ALIGN) * ALIGN;
        placed.push(Placed {
            name: e.name.clone(),
            iso_name,
            iso_sector: cursor / ISO_SECTOR,
            len,
            src_start: start,
            type_guid: e.type_guid,
            attributes: e.attributes,
            is_esp,
        });
        cursor += len.div_ceil(ISO_SECTOR) * ISO_SECTOR;
    }
    if placed.is_empty() {
        return Err(ImageError::Spec(
            "there is nothing in this image to put in an ISO".into(),
        ));
    }

    let content_end = cursor;
    let total = (content_end + BACKUP_GPT_BYTES).div_ceil(ISO_SECTOR) * ISO_SECTOR;
    let total_sectors = total / ISO_SECTOR;

    // --- the ISO structures ------------------------------------------------
    let root_records = root_directory(&placed);
    if root_records.len() as u64 > ISO_SECTOR * (FIRST_FILE_SECTOR - ROOT_DIR_SECTOR) {
        return Err(ImageError::Other(
            "too many partitions for the root directory of this ISO".into(),
        ));
    }

    let mut dst = tokio::fs::File::create(out).await?;
    // System area: zero now, GPT written over it at the end.
    dst.write_all(&vec![0u8; (SYSTEM_AREA_SECTORS * ISO_SECTOR) as usize])
        .await?;
    dst.write_all(&primary_volume_descriptor(
        total_sectors,
        root_records.len() as u32,
    ))
    .await?;
    dst.write_all(&boot_record_descriptor()).await?;
    dst.write_all(&terminator()).await?;
    dst.write_all(&boot_catalog(&placed)?).await?;
    dst.write_all(&path_table(true)).await?;
    dst.write_all(&path_table(false)).await?;
    let mut root_sector = vec![0u8; (ISO_SECTOR * (FIRST_FILE_SECTOR - ROOT_DIR_SECTOR)) as usize];
    root_sector[..root_records.len()].copy_from_slice(&root_records);
    dst.write_all(&root_sector).await?;

    // --- the partitions, byte for byte ------------------------------------
    let mut at = FIRST_FILE_SECTOR * ISO_SECTOR;
    for p in &placed {
        let want = p.iso_sector * ISO_SECTOR;
        if want > at {
            dst.write_all(&vec![0u8; (want - at) as usize]).await?;
            at = want;
        }
        let view = PartitionView::new(src.clone(), p.src_start, p.len);
        let mut buf = vec![0u8; COPY_CHUNK];
        let mut off = 0u64;
        while off < p.len {
            let take = ((p.len - off) as usize).min(COPY_CHUNK);
            view.read_at(off, &mut buf[..take]).await?;
            dst.write_all(&buf[..take]).await?;
            off += take as u64;
        }
        at += p.len;
        let padded = p.len.div_ceil(ISO_SECTOR) * ISO_SECTOR;
        if padded > p.len {
            dst.write_all(&vec![0u8; (padded - p.len) as usize]).await?;
            at += padded - p.len;
        }
    }
    if total > at {
        dst.write_all(&vec![0u8; (total - at) as usize]).await?;
    }
    dst.flush().await?;
    drop(dst);

    // --- the GPT over the system area -------------------------------------
    let iso: Arc<dyn BlockDevice> = Arc::new(
        FileDevice::open(
            out.to_str()
                .ok_or_else(|| ImageError::Spec("output path is not UTF-8".into()))?,
        )
        .await?,
    );
    let mut table = Gpt::create_with_lba(&iso, 512);
    for p in &placed {
        let first = p.iso_sector * ISO_SECTOR / 512;
        let last = first + p.len.div_ceil(512) - 1;
        table.insert(GptEntry {
            type_guid: p.type_guid,
            unique_guid: uuid::Uuid::new_v4().to_bytes_le(),
            first_lba: first,
            last_lba: last,
            // Pallet state travels with the pallet: an ISO's boot ladder is the
            // same ladder, priorities and tries and all.
            attributes: p.attributes,
            name: p.name.clone(),
        })?;
    }
    table.write(&iso).await?;
    Ok(out.to_path_buf())
}

fn iso_name_for(name: &str, is_esp: bool, is_pallet: bool, index: usize) -> String {
    if is_esp {
        return "ESP.IMG;1".to_string();
    }
    let stem: String = name
        .to_ascii_uppercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(6)
        .collect();
    let stem = if stem.is_empty() { "PART".to_string() } else { stem };
    let ext = if is_pallet { "PAL" } else { "IMG" };
    format!("{stem}{index:02};1", stem = stem, index = index)
        .replace(";1", &format!(".{ext};1"))
}

// -------------------------------------------------------- ISO structures

fn both_u16(v: u16) -> [u8; 4] {
    let mut b = [0u8; 4];
    b[0..2].copy_from_slice(&v.to_le_bytes());
    b[2..4].copy_from_slice(&v.to_be_bytes());
    b
}

fn both_u32(v: u32) -> [u8; 8] {
    let mut b = [0u8; 8];
    b[0..4].copy_from_slice(&v.to_le_bytes());
    b[4..8].copy_from_slice(&v.to_be_bytes());
    b
}

/// A fixed timestamp, so the same image always makes the same ISO.
const ISO_DATETIME: &[u8; 17] = b"2026010100000000\0";

fn ascii_field(s: &str, len: usize) -> Vec<u8> {
    let mut v = vec![b' '; len];
    for (i, c) in s.bytes().take(len).enumerate() {
        v[i] = c.to_ascii_uppercase();
    }
    v
}

fn directory_record(id: &[u8], extent: u32, len: u32, is_dir: bool) -> Vec<u8> {
    let base = 33 + id.len();
    let total = base + (base % 2);
    let mut r = vec![0u8; total];
    r[0] = total as u8;
    r[2..10].copy_from_slice(&both_u32(extent));
    r[10..18].copy_from_slice(&both_u32(len));
    // Recording date: years since 1900, month, day, hour, minute, second, tz.
    r[18..25].copy_from_slice(&[126, 1, 1, 0, 0, 0, 0]);
    r[25] = if is_dir { 0x02 } else { 0x00 };
    r[28..32].copy_from_slice(&both_u16(1));
    r[32] = id.len() as u8;
    r[33..33 + id.len()].copy_from_slice(id);
    r
}

fn root_directory(placed: &[Placed]) -> Vec<u8> {
    let dir_len = (ISO_SECTOR * (FIRST_FILE_SECTOR - ROOT_DIR_SECTOR)) as u32;
    let mut out = Vec::new();
    out.extend_from_slice(&directory_record(&[0x00], ROOT_DIR_SECTOR as u32, dir_len, true));
    out.extend_from_slice(&directory_record(&[0x01], ROOT_DIR_SECTOR as u32, dir_len, true));
    for p in placed {
        out.extend_from_slice(&directory_record(
            p.iso_name.as_bytes(),
            p.iso_sector as u32,
            // A file larger than 4 GiB cannot be described by one record; the
            // GPT entry still covers it, which is the path that matters.
            p.len.min(u32::MAX as u64) as u32,
            false,
        ));
    }
    out
}

fn primary_volume_descriptor(total_sectors: u64, root_len: u32) -> Vec<u8> {
    let _ = root_len;
    let mut v = vec![0u8; ISO_SECTOR as usize];
    v[0] = 1;
    v[1..6].copy_from_slice(b"CD001");
    v[6] = 1;
    v[8..40].copy_from_slice(&ascii_field("STORMBLOCK", 32));
    v[40..72].copy_from_slice(&ascii_field("STORMCOS", 32));
    v[80..88].copy_from_slice(&both_u32(total_sectors as u32));
    v[120..124].copy_from_slice(&both_u16(1));
    v[124..128].copy_from_slice(&both_u16(1));
    v[128..132].copy_from_slice(&both_u16(ISO_SECTOR as u16));
    v[132..140].copy_from_slice(&both_u32(10)); // path table size
    v[140..144].copy_from_slice(&(L_PATH_TABLE_SECTOR as u32).to_le_bytes());
    v[148..152].copy_from_slice(&(M_PATH_TABLE_SECTOR as u32).to_be_bytes());
    let dir_len = (ISO_SECTOR * (FIRST_FILE_SECTOR - ROOT_DIR_SECTOR)) as u32;
    let root = directory_record(&[0x00], ROOT_DIR_SECTOR as u32, dir_len, true);
    v[156..156 + root.len()].copy_from_slice(&root);
    v[190..318].copy_from_slice(&ascii_field("", 128));
    v[318..446].copy_from_slice(&ascii_field("", 128));
    v[446..574].copy_from_slice(&ascii_field("", 128));
    v[574..702].copy_from_slice(&ascii_field("STORMBLOCK IMAGE BUILDER", 128));
    v[702..739].copy_from_slice(&ascii_field("", 37));
    v[739..776].copy_from_slice(&ascii_field("", 37));
    v[776..813].copy_from_slice(&ascii_field("", 37));
    for (i, at) in [813usize, 830, 847, 864].iter().enumerate() {
        let _ = i;
        v[*at..*at + 17].copy_from_slice(ISO_DATETIME);
    }
    v[881] = 1;
    v
}

fn boot_record_descriptor() -> Vec<u8> {
    let mut v = vec![0u8; ISO_SECTOR as usize];
    v[0] = 0;
    v[1..6].copy_from_slice(b"CD001");
    v[6] = 1;
    let id = b"EL TORITO SPECIFICATION";
    v[7..7 + id.len()].copy_from_slice(id);
    v[71..75].copy_from_slice(&(BOOT_CATALOG_SECTOR as u32).to_le_bytes());
    v
}

fn terminator() -> Vec<u8> {
    let mut v = vec![0u8; ISO_SECTOR as usize];
    v[0] = 0xFF;
    v[1..6].copy_from_slice(b"CD001");
    v[6] = 1;
    v
}

/// The boot catalog: a UEFI validation entry and one default entry pointing at
/// the ESP, no emulation.
///
/// EFI-only on purpose. There is no BIOS loader in this image, so a bootable
/// x86 entry would send a BIOS machine into nothing, and a *non*-bootable
/// placeholder is an El Torito image with no size — which is exactly what
/// `xorriso` calls a hidden image and warns about. One honest entry is better
/// than two, one of which is a lie.
fn boot_catalog(placed: &[Placed]) -> Result<Vec<u8>> {
    let esp = placed
        .iter()
        .find(|p| p.is_esp)
        .ok_or_else(|| ImageError::Spec("a bootable ISO needs an ESP in the image".into()))?;

    let mut v = vec![0u8; ISO_SECTOR as usize];
    // Validation entry.
    v[0] = 0x01;
    v[1] = PLATFORM_EFI;
    let id = b"stormblock";
    v[4..4 + id.len()].copy_from_slice(id);
    v[30] = 0x55;
    v[31] = 0xAA;
    let mut sum: u16 = 0;
    for i in (0..32).step_by(2) {
        sum = sum.wrapping_add(u16::from_le_bytes([v[i], v[i + 1]]));
    }
    v[28..30].copy_from_slice(&(0u16.wrapping_sub(sum)).to_le_bytes());

    // Default entry: the ESP, no emulation.
    let sectors_512 = esp.len.div_ceil(512);
    if sectors_512 > 0xFFFF {
        // El Torito counts the boot image in 512-byte sectors in a 16-bit
        // field. Firmware that honours it would see only the first 32 MiB of
        // a larger ESP — so say so rather than let it be discovered at a boot
        // prompt. The GPT entry still covers the whole partition, which is the
        // path a USB stick takes.
        tracing::warn!(
            "ESP is {} bytes: El Torito can only describe {} of it, so optical boot may see a \
             truncated filesystem. Build the ISO with an ESP of 32M or less (FAT16) — booting \
             the same file from USB is unaffected.",
            esp.len,
            0xFFFFu64 * 512
        );
    }
    v[32] = 0x88; // bootable
    v[33] = 0x00; // no emulation
    v[38..40].copy_from_slice(&(sectors_512.min(0xFFFF) as u16).to_le_bytes());
    v[40..44].copy_from_slice(&(esp.iso_sector as u32).to_le_bytes());
    Ok(v)
}

/// One record, for the root. There are no directories in this ISO — the
/// partitions are its content, and each of them is one file.
fn path_table(little: bool) -> Vec<u8> {
    let mut v = vec![0u8; ISO_SECTOR as usize];
    v[0] = 1; // identifier length
    v[1] = 0; // extended attribute length
    if little {
        v[2..6].copy_from_slice(&(ROOT_DIR_SECTOR as u32).to_le_bytes());
        v[6..8].copy_from_slice(&1u16.to_le_bytes());
    } else {
        v[2..6].copy_from_slice(&(ROOT_DIR_SECTOR as u32).to_be_bytes());
        v[6..8].copy_from_slice(&1u16.to_be_bytes());
    }
    v[8] = 0x00;
    v
}
