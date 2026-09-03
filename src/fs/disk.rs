//! What is on a volume when it is not a filesystem the engine writes: a
//! partition table, or a read-only image.
//!
//! A VM disk golden is a whole disk — GPT or MBR, partitions, whatever is
//! inside them — and the engine stores it as raw bytes. Two things it can
//! still do for such a golden: say what it is (`detect`), and give every
//! clone its own **disk identity** (`stamp`): the GPT disk GUID, or the MBR
//! signature, is what a host uses to tell disks apart (`PARTUUID` is derived
//! from it), and two clones with one identity attached to one host collide
//! exactly the way two ext4 clones with one UUID do. What lives *inside* the
//! partitions — filesystem UUIDs, machine-id, SIDs — is the guest's to
//! change (cloud-init, sysprep); the engine does not parse partitions.

use std::sync::Arc;

use uuid::Uuid;

use crate::drive::{BlockDevice, DriveResult};
use crate::volume::FsInfo;

/// Recognise a partition table or a read-only image on the volume.
pub async fn detect(dev: &Arc<dyn BlockDevice>) -> DriveResult<Option<FsInfo>> {
    let cap = dev.capacity_bytes();
    // A pallet: `STORMPAL` at 0. A composed pallet is a volume, and sealing it
    // records what it is so a disk can be composed out of it by name.
    if cap >= 4096 {
        let mut sb = vec![0u8; 4096];
        dev.read(0, &mut sb).await?;
        if sb[..8] == crate::pallet::format::MAGIC {
            let name = String::from_utf8_lossy(&sb[52..92])
                .trim_end_matches('\0')
                .to_string();
            return Ok(Some(FsInfo {
                kind: "pallet".into(),
                journal: false,
                features: Some(format!("lba={}", u32::from_le_bytes(sb[16..20].try_into().unwrap()))),
                sixty_four_bit: false,
                metadata_csum: false,
                csum_seed: false,
                label: name,
                uuid: None,
            }));
        }
    }
    // GPT: "EFI PART" at LBA 1 for a 512-byte or a 4096-byte LBA.
    for lba in [512u64, 4096] {
        if cap < lba * 2 {
            continue;
        }
        let mut hdr = vec![0u8; lba as usize];
        dev.read(lba, &mut hdr).await?;
        if &hdr[..8] == b"EFI PART" {
            let guid = Uuid::from_bytes_le(hdr[56..72].try_into().unwrap());
            return Ok(Some(FsInfo {
                kind: "gpt".into(),
                journal: false,
                features: Some(format!("lba={lba}")),
                sixty_four_bit: false,
                metadata_csum: false,
                csum_seed: false,
                label: String::new(),
                uuid: Some(guid),
            }));
        }
    }
    if cap >= 512 {
        let mut mbr = vec![0u8; 512];
        dev.read(0, &mut mbr).await?;
        if mbr[510] == 0x55 && mbr[511] == 0xAA {
            // A partition entry with a type says this is a disk, not a boot
            // sector of a filesystem that happens to end in 55AA.
            let has_part = (0..4).any(|i| mbr[446 + i * 16 + 4] != 0);
            if has_part {
                let sig = u32::from_le_bytes(mbr[440..444].try_into().unwrap());
                return Ok(Some(FsInfo {
                    kind: "mbr".into(),
                    journal: false,
                    features: None,
                    sixty_four_bit: false,
                    metadata_csum: false,
                    csum_seed: false,
                    label: String::new(),
                    uuid: Some(Uuid::from_u128(sig as u128)),
                }));
            }
        }
    }
    // ISO 9660: primary volume descriptor at sector 16.
    if cap >= 0x8000 + 2048 {
        let mut pvd = vec![0u8; 2048];
        dev.read(0x8000, &mut pvd).await?;
        if &pvd[1..6] == b"CD001" {
            let label = String::from_utf8_lossy(&pvd[40..72]).trim_matches(|c| c == ' ' || c == '\0').to_string();
            return Ok(Some(FsInfo {
                kind: "iso9660".into(),
                journal: false,
                features: None,
                sixty_four_bit: false,
                metadata_csum: false,
                csum_seed: false,
                label,
                uuid: None,
            }));
        }
    }
    Ok(None)
}

/// What the engine can read off a volume: ext4 first, then a disk shape.
pub async fn probe(dev: &Arc<dyn BlockDevice>) -> Option<FsInfo> {
    if let Ok(l) = crate::fs::ext4::read_layout(dev).await {
        return Some(FsInfo {
            kind: "ext4".into(),
            journal: l.has_journal,
            features: None,
            sixty_four_bit: l.sixty_four_bit,
            metadata_csum: l.metadata_csum,
            csum_seed: l.csum_seed,
            label: l.label.clone(),
            uuid: Some(l.uuid),
        });
    }
    detect(dev).await.ok().flatten()
}

fn crc32_ieee(data: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(data);
    h.finalize()
}

/// Give a GPT disk a fresh disk GUID: both headers, both CRCs.
pub async fn stamp_gpt(dev: &Arc<dyn BlockDevice>, lba: u64, guid: Uuid) -> DriveResult<()> {
    let mut hdr = vec![0u8; lba as usize];
    dev.read(lba, &mut hdr).await?;
    if &hdr[..8] != b"EFI PART" {
        return Err(crate::drive::DriveError::Other(anyhow::anyhow!("no GPT header at LBA 1 ({lba}-byte LBA)")));
    }
    let hsize = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
    let backup_lba = u64::from_le_bytes(hdr[32..40].try_into().unwrap());
    let stamp = |h: &mut [u8]| {
        h[56..72].copy_from_slice(&guid.to_bytes_le());
        h[16..20].copy_from_slice(&[0, 0, 0, 0]);
        let crc = crc32_ieee(&h[..hsize.min(h.len())]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
    };
    stamp(&mut hdr);
    dev.write(lba, &hdr).await?;
    // The backup header, if the image carries one where it says it does.
    let backup_off = backup_lba * lba;
    if backup_lba != 0 && backup_off + lba <= dev.capacity_bytes() {
        let mut b = vec![0u8; lba as usize];
        dev.read(backup_off, &mut b).await?;
        if &b[..8] == b"EFI PART" {
            stamp(&mut b);
            dev.write(backup_off, &b).await?;
        }
    }
    dev.flush().await
}

/// Give an MBR disk a fresh 32-bit signature.
pub async fn stamp_mbr(dev: &Arc<dyn BlockDevice>, sig: u32) -> DriveResult<()> {
    let mut mbr = vec![0u8; 512];
    dev.read(0, &mut mbr).await?;
    mbr[440..444].copy_from_slice(&sig.to_le_bytes());
    dev.write(0, &mbr).await?;
    dev.flush().await
}

/// Stamp a fresh disk identity matching `fs.kind`; returns the new uuid, or
/// `None` when there is nothing to stamp (an ISO, a bare filesystem).
pub async fn stamp(dev: &Arc<dyn BlockDevice>, fs: &FsInfo) -> DriveResult<Option<Uuid>> {
    match fs.kind.as_str() {
        "gpt" => {
            let lba = fs
                .features
                .as_deref()
                .and_then(|f| f.strip_prefix("lba="))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(512);
            let guid = Uuid::new_v4();
            stamp_gpt(dev, lba, guid).await?;
            Ok(Some(guid))
        }
        "mbr" => {
            let sig: u32 = rand::random::<u32>() | 1; // never the "no signature" zero
            stamp_mbr(dev, sig).await?;
            Ok(Some(Uuid::from_u128(sig as u128)))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;
    use crate::pallet::gpt::Gpt;

    async fn dev(name: &str) -> (Arc<dyn BlockDevice>, String) {
        let p = std::env::temp_dir().join(format!("stormblock-disk-{}-{name}.bin", Uuid::new_v4().simple()));
        let s = p.to_str().unwrap().to_string();
        let d = FileDevice::open_with_capacity(&s, 16 * 1024 * 1024).await.unwrap();
        (Arc::new(d), s)
    }

    #[tokio::test]
    async fn gpt_is_detected_and_restamped_with_valid_crcs() {
        let (d, p) = dev("gpt").await;
        let g = Gpt::create_with_lba(&d, 512);
        g.write(&d).await.unwrap();
        let fs = detect(&d).await.unwrap().expect("a gpt");
        assert_eq!(fs.kind, "gpt");
        assert_eq!(fs.uuid, Some(Uuid::from_bytes_le(g.disk_guid)));
        let new = stamp(&d, &fs).await.unwrap().unwrap();
        assert_ne!(Some(new), fs.uuid);
        let again = detect(&d).await.unwrap().unwrap();
        assert_eq!(again.uuid, Some(new));
        // The pallet GPT reader validates CRCs: it must still accept both headers.
        let back = Gpt::read(&d).await.expect("headers still verify after the stamp");
        assert_eq!(back.disk_guid, new.to_bytes_le());
        let _ = std::fs::remove_file(p);
    }

    #[tokio::test]
    async fn mbr_and_iso_are_told_apart_from_a_blank() {
        let (d, p) = dev("mbr").await;
        assert!(detect(&d).await.unwrap().is_none(), "blank is nothing");
        let mut mbr = vec![0u8; 512];
        mbr[440..444].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        mbr[446 + 4] = 0x83;
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        d.write(0, &mbr).await.unwrap();
        let fs = detect(&d).await.unwrap().unwrap();
        assert_eq!(fs.kind, "mbr");
        assert_eq!(fs.uuid, Some(Uuid::from_u128(0x1234_5678)));
        let new = stamp(&d, &fs).await.unwrap().unwrap();
        assert_ne!(Some(new), fs.uuid);
        assert_eq!(detect(&d).await.unwrap().unwrap().uuid, Some(new));

        let (d2, p2) = dev("iso").await;
        let mut pvd = vec![0u8; 2048];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[40..46].copy_from_slice(b"STORM ");
        d2.write(0x8000, &pvd).await.unwrap();
        let fs = detect(&d2).await.unwrap().unwrap();
        assert_eq!(fs.kind, "iso9660");
        assert_eq!(fs.label, "STORM");
        assert!(stamp(&d2, &fs).await.unwrap().is_none(), "nothing to stamp on an iso");
        let _ = std::fs::remove_file(p);
        let _ = std::fs::remove_file(p2);
    }
}
