//! Image assembly end to end.
//!
//! An image is a GPT plus a concatenation of pallets, so the assertions are
//! about exactly that: that the partitions land where the spec says, that
//! every pallet verifies *inside the image*, and that a pallet copied in from
//! another image arrives byte for byte.

use std::path::Path;

use stormblock::image::{ImageBuilder, ImageFormat, ImageSpec};
use tempfile::TempDir;

fn write(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

fn spec_toml(dir: &Path, extra: &str) -> String {
    format!(
        r#"
name = "stormcos"
size = "192M"

[esp]
size = "48M"
label = "EFI"
from_dir = "{esp}"

[[pallet]]
name = "stormcos-boot"
kind = "boot"
version_label = "6.12.0"
priority = 15
members = [
  {{ name = "kernel", role = "kernel", kind = "kernel", file = "{kernel}" }},
  {{ name = "cmdline", role = "cmdline", kind = "bootconfig", text = "root=ublk0 ro" }},
]
{extra}
"#,
        esp = dir.join("esp").display(),
        kernel = dir.join("vmlinuz").display(),
    )
}

fn make_sources(dir: &Path) {
    std::fs::create_dir_all(dir.join("esp/EFI/BOOT")).unwrap();
    write(&dir.join("esp/EFI/BOOT"), "BOOTX64.EFI", &vec![0xE1; 40_000]);
    std::fs::create_dir_all(dir.join("esp/loader/entries")).unwrap();
    write(
        &dir.join("esp/loader/entries"),
        "stormcos-6.12.0.conf",
        b"title StormCOS\nlinux /vmlinuz\n",
    );
    write(dir, "vmlinuz", &vec![0x5A; 3_000_000]);
}

#[tokio::test]
async fn an_image_is_a_gpt_plus_a_concatenation_of_pallets() {
    let tmp = TempDir::new().unwrap();
    make_sources(tmp.path());
    let spec = ImageSpec::from_toml(&spec_toml(tmp.path(), "[slab]\nsize = \"rest\"\ntier = \"hot\"")).unwrap();
    let out = tmp.path().join("disk.img");
    let report = ImageBuilder::new(spec).build(&out).await.expect("build");

    // ESP, pallet, slab — in the order the spec declares them.
    let kinds: Vec<&str> = report.partitions.iter().map(|p| p.kind.as_str()).collect();
    assert_eq!(kinds, vec!["esp", "pallet/boot", "slab"], "{report:#?}");
    assert!(report.partitions.iter().all(|p| p.start_bytes % (1024 * 1024) == 0));
    assert!(report.partitions[1].verified.unwrap());

    // And it is a real image: the table reads back, and the pallet in it
    // verifies against the manifest's digests.
    let gpt = stormblock::image::build::table_of(&out).await.unwrap();
    assert_eq!(gpt.block_size, 512, "an image is read as 512-byte sectors");
    assert_eq!(gpt.partitions().count(), 3);
    let pallets = stormblock::image::build::pallets_in(&out).await.unwrap();
    assert_eq!(pallets.len(), 1);
    assert_eq!(pallets[0].name, "stormcos-boot");
    assert_eq!(pallets[0].version_label, "6.12.0");
    assert_eq!(pallets[0].attributes.priority, 15);
    assert!(pallets[0].is_readable());
}

#[tokio::test]
async fn a_pallet_can_be_taken_from_another_image_byte_for_byte() {
    let tmp = TempDir::new().unwrap();
    make_sources(tmp.path());

    let first = tmp.path().join("first.img");
    let spec = ImageSpec::from_toml(&spec_toml(tmp.path(), "")).unwrap();
    ImageBuilder::new(spec).build(&first).await.unwrap();
    let source_pallet = stormblock::image::build::pallets_in(&first).await.unwrap()[0].clone();

    // A second image that composes a new boot pallet and imports the old one
    // as the fallback — the shape every upgrade image has.
    let toml = format!(
        r#"
name = "stormcos"
size = "256M"

[esp]
size = "48M"
from_dir = "{esp}"

[[pallet]]
name = "stormcos-boot"
kind = "boot"
version = 2
priority = 15
members = [ {{ name = "kernel", role = "kernel", kind = "kernel", file = "{kernel}" }} ]

[[pallet]]
from_image = "{first}"
id = "{id}"
"#,
        esp = tmp.path().join("esp").display(),
        kernel = tmp.path().join("vmlinuz").display(),
        first = first.display(),
        id = source_pallet.id,
    );
    let out = tmp.path().join("second.img");
    let report = ImageBuilder::new(ImageSpec::from_toml(&toml).unwrap())
        .build(&out)
        .await
        .expect("build");
    assert_eq!(report.partitions.len(), 3);

    let pallets = stormblock::image::build::pallets_in(&out).await.unwrap();
    assert_eq!(pallets.len(), 2);
    let imported = pallets.iter().find(|p| p.version == 1).expect("the fallback came along");
    assert_eq!(imported.name, "stormcos-boot");
    assert!(imported.is_readable());
    // Both verified during the build; verifying the import again here is the
    // claim that the copy is the same pallet, not a rebuild of one.
    assert!(report.partitions.iter().filter(|p| p.verified == Some(true)).count() == 2);
}

#[tokio::test]
async fn the_image_is_refused_rather_than_truncated_when_it_is_too_small() {
    let tmp = TempDir::new().unwrap();
    make_sources(tmp.path());
    let mut toml = spec_toml(tmp.path(), "");
    toml = toml.replace("size = \"192M\"", "size = \"8M\"");
    let out = tmp.path().join("tiny.img");
    let err = ImageBuilder::new(ImageSpec::from_toml(&toml).unwrap())
        .build(&out)
        .await
        .unwrap_err();
    assert!(matches!(err, stormblock::image::ImageError::TooSmall { .. }), "{err}");
}

#[tokio::test]
async fn every_format_carries_the_same_image() {
    let tmp = TempDir::new().unwrap();
    make_sources(tmp.path());
    let spec = ImageSpec::from_toml(&spec_toml(tmp.path(), "")).unwrap();
    let raw = tmp.path().join("disk.img");
    ImageBuilder::new(spec).build(&raw).await.unwrap();
    let raw_len = std::fs::metadata(&raw).unwrap().len();

    for format in [ImageFormat::Qcow2, ImageFormat::Vhd, ImageFormat::Vmdk] {
        let out = tmp.path().join(format!("disk.{}", format.extension()));
        stormblock::image::formats::convert(&raw, &out, format).await.unwrap();
        let len = std::fs::metadata(&out).unwrap().len();
        assert!(len > 0, "{format} is empty");
        match format {
            // Fixed VHD is the raw image plus a footer, exactly.
            ImageFormat::Vhd => assert_eq!(len, raw_len + 512),
            // The sparse formats skip what is still zero, and a fresh image is
            // mostly zero.
            _ => assert!(len < raw_len, "{format} should be sparser than raw ({len} vs {raw_len})"),
        }
    }
}

#[tokio::test]
async fn an_iso_carries_the_partitions_and_a_gpt_over_them() {
    let tmp = TempDir::new().unwrap();
    make_sources(tmp.path());
    let spec =
        ImageSpec::from_toml(&spec_toml(tmp.path(), "[slab]\nsize = \"rest\"\ntier = \"hot\"")).unwrap();
    let raw = tmp.path().join("disk.img");
    ImageBuilder::new(spec).build(&raw).await.unwrap();

    let iso = tmp.path().join("disk.iso");
    stormblock::image::formats::convert(&raw, &iso, ImageFormat::Iso).await.unwrap();

    let bytes = std::fs::read(&iso).unwrap();
    // It is an ISO9660…
    assert_eq!(&bytes[16 * 2048..16 * 2048 + 6], b"\x01CD001");
    // …with an El Torito boot record…
    assert_eq!(&bytes[17 * 2048 + 7..17 * 2048 + 30], b"EL TORITO SPECIFICATION");
    // …and a GPT in its system area describing the same bytes.
    assert_eq!(&bytes[512..520], b"EFI PART");
    // The ESP and the pallet come along; the slab does not — it is the mutable
    // end of a disk and empty, and carrying it would be 200 MB of zeros.
    let gpt = stormblock::image::build::table_of(&iso).await.unwrap();
    assert_eq!(gpt.partitions().count(), 2);
    assert!(
        std::fs::metadata(&iso).unwrap().len() < std::fs::metadata(&raw).unwrap().len() / 2,
        "an ISO without the slab should be far smaller than the disk"
    );

    // The pallet inside the ISO is the pallet, still verifiable — which is the
    // whole point of everything inside it being partition-relative.
    let pallets = stormblock::image::build::pallets_in(&iso).await.unwrap();
    assert_eq!(pallets.len(), 1);
    assert!(pallets[0].is_readable());
    assert_eq!(pallets[0].name, "stormcos-boot");

    // …and a live image that really does ship a slab can ask for it.
    let with_slab = tmp.path().join("live.iso");
    stormblock::image::iso::from_image_with(
        &raw,
        &with_slab,
        stormblock::image::iso::IsoOptions { include_slab: true },
    )
    .await
    .unwrap();
    let gpt = stormblock::image::build::table_of(&with_slab).await.unwrap();
    assert_eq!(gpt.partitions().count(), 3);
}

// ------------------------------------------------------- goldens in the slab

/// The image that *is* a node rather than one that merely contains it (#62):
/// the goldens land in the slab, each has a first clone, and the slab says so
/// itself — there is no filesystem in an image to keep `volumes.dat` in.
#[tokio::test]
async fn the_slab_ships_with_goldens_and_a_first_clone_of_each() {
    use std::sync::Arc;
    use stormblock::drive::filedev::FileDevice;
    use stormblock::drive::partition::PartitionDevice;
    use stormblock::drive::slab::Slab;
    use stormblock::drive::BlockDevice;
    use stormblock::raid::RaidArrayId;
    use stormblock::volume::VolumeManager;

    let tmp = TempDir::new().unwrap();
    make_sources(tmp.path());

    // A golden with a hole in the middle: a filesystem image is mostly holes,
    // and writing them would cost the slab a slot per hole.
    let mut rootfs = vec![0u8; 6 * 1024 * 1024];
    rootfs[..1024].fill(0xAB);
    rootfs[5 * 1024 * 1024..5 * 1024 * 1024 + 1024].fill(0xCD);
    write(tmp.path(), "rootfs.img", &rootfs);
    write(tmp.path(), "config.img", &vec![0x42; 512 * 1024]);

    let extra = format!(
        r#"
[[pallet]]
name = "system1"
kind = "system"
version = 1
members = [
  {{ name = "stormpump", role = "rootfs", file = "{rootfs}" }},
]

[slab]
size = "rest"
tier = "hot"

  [[slab.golden]]
  name = "stormpump"
  from = "pallet:system1/stormpump"

  [[slab.golden]]
  name = "stormpump-config"
  file = "{config}"
"#,
        rootfs = tmp.path().join("rootfs.img").display(),
        config = tmp.path().join("config.img").display(),
    );
    let mut toml = spec_toml(tmp.path(), &extra);
    toml = toml.replace("size = \"192M\"", "size = \"320M\"");
    let spec = ImageSpec::from_toml(&toml).unwrap();
    let out = tmp.path().join("node.img");
    let report = ImageBuilder::new(spec).build(&out).await.expect("build");

    let slab_part = report
        .partitions
        .iter()
        .find(|p| p.kind == "slab")
        .expect("a slab partition");

    // Two goldens, two clones, named so a kernel cmdline can ask for the clone.
    let names: Vec<&str> = slab_part.volumes.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "stormpump.golden",
            "stormpump",
            "stormpump-config.golden",
            "stormpump-config"
        ],
        "{:#?}",
        slab_part.volumes
    );

    let golden = &slab_part.volumes[0];
    let clone = &slab_part.volumes[1];
    assert_eq!(clone.clone_of, Some(golden.id));
    assert!(golden.sealed, "a golden arrives sealed (#77)");
    assert!(!clone.sealed, "the clone is what the node writes to");
    // The hole is not stored: 6 MiB of content, two 1 MiB slots of data.
    assert_eq!(golden.size_bytes, 6 * 1024 * 1024);
    assert_eq!(golden.allocated_bytes, 2 * 1024 * 1024, "holes were written");
    // The clone maps the same extents and shares every slot with the golden,
    // so it costs the slab nothing until it writes.
    assert_eq!(clone.allocated_bytes, golden.allocated_bytes);

    // Now read the image back the way a boot does: open the slab partition,
    // take the volume metadata *out of the slab*, and read the clone.
    let dev: Arc<dyn BlockDevice> =
        Arc::new(FileDevice::open(out.to_str().unwrap()).await.unwrap());
    let part = Arc::new(
        PartitionDevice::new(dev, slab_part.start_bytes, slab_part.size_bytes).unwrap(),
    );
    let slab = Slab::open(part).await.unwrap();
    assert!(slab.has_metadata_region());
    let slab_id = slab.slab_id();
    let slot_size = slab.slot_size();

    let mut mgr = VolumeManager::new(slot_size);
    mgr.persist_to_slab(slab_id);
    mgr.attach_slab(RaidArrayId(uuid::Uuid::new_v4()), slab)
        .await
        .unwrap();
    mgr.restore().await.expect("metadata restores from the slab itself");
    assert!(mgr.is_sealed(&stormblock::volume::VolumeId(golden.id)), "sealed survives in the slab's own record");
    assert!(!mgr.is_sealed(&stormblock::volume::VolumeId(clone.id)));
    assert_eq!(mgr.parent(&stormblock::volume::VolumeId(clone.id)), Some(stormblock::volume::VolumeId(golden.id)), "lineage shipped");

    let restored = mgr.list_volumes().await;
    assert_eq!(restored.len(), 4, "{restored:#?}");
    let (clone_id, _, size, _) = restored
        .iter()
        .find(|(_, n, _, _)| n == "stormpump")
        .expect("the clone the cmdline names")
        .clone();
    assert_eq!(size, 6 * 1024 * 1024);

    // The clone reads the golden's content, holes and all.
    let vol = mgr.get_volume(&clone_id).unwrap();
    let mut buf = vec![0u8; 4096];
    vol.read(0, &mut buf).await.unwrap();
    assert!(buf[..1024].iter().all(|&b| b == 0xAB), "clone lost the head");
    assert!(buf[1024..].iter().all(|&b| b == 0), "clone invented data");
    vol.read(5 * 1024 * 1024, &mut buf).await.unwrap();
    assert!(buf[..1024].iter().all(|&b| b == 0xCD), "clone lost the tail");
    vol.read(3 * 1024 * 1024, &mut buf).await.unwrap();
    assert!(buf.iter().all(|&b| b == 0), "the hole came back as data");

    // …and it is writable, diverging from the golden rather than editing it.
    vol.write(0, &vec![0x11; 4096]).await.unwrap();
    vol.flush().await.unwrap();
    let golden_id = restored
        .iter()
        .find(|(_, n, _, _)| n == "stormpump.golden")
        .map(|(id, ..)| *id)
        .unwrap();
    let gvol = mgr.get_volume(&golden_id).unwrap();
    gvol.read(0, &mut buf).await.unwrap();
    assert!(buf[..1024].iter().all(|&b| b == 0xAB), "the golden was written through");
}

/// The same image, through the binary the initramfs actually runs: a slab
/// path and a volume name, no metadata directory anywhere.
#[tokio::test]
async fn boot_local_resolves_a_shipped_clone_with_no_meta_directory() {
    use std::io::{Read, Seek, SeekFrom};
    use std::process::Command;

    let tmp = TempDir::new().unwrap();
    make_sources(tmp.path());
    write(tmp.path(), "rootfs.img", &vec![0x7E; 2 * 1024 * 1024]);

    let extra = format!(
        r#"
[slab]
size = "rest"
tier = "hot"

  [[slab.golden]]
  name = "stormpump"
  file = "{rootfs}"
  clone = "stormpump"
"#,
        rootfs = tmp.path().join("rootfs.img").display(),
    );
    let spec = ImageSpec::from_toml(&spec_toml(tmp.path(), &extra)).unwrap();
    let out = tmp.path().join("node.img");
    let report = ImageBuilder::new(spec).build(&out).await.expect("build");
    let slab_part = report.partitions.iter().find(|p| p.kind == "slab").unwrap();

    // Carve the slab partition out, the way the kernel presents /dev/sda4.
    let slab_file = tmp.path().join("sda4");
    let mut src = std::fs::File::open(&out).unwrap();
    src.seek(SeekFrom::Start(slab_part.start_bytes)).unwrap();
    let mut bytes = vec![0u8; slab_part.size_bytes as usize];
    src.read_exact(&mut bytes).unwrap();
    std::fs::write(&slab_file, &bytes).unwrap();

    let cmd = Command::new(env!("CARGO_BIN_EXE_stormblock"))
        .args([
            "boot-local",
            "--slab",
            slab_file.to_str().unwrap(),
            "--volume",
            "stormpump",
            "--check",
        ])
        .output()
        .expect("spawn stormblock boot-local");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&cmd.stdout),
        String::from_utf8_lossy(&cmd.stderr)
    );
    assert!(cmd.status.success(), "boot-local must resolve the clone:\n{text}");
    assert!(text.contains("Volume metadata from slab"), "{text}");
    assert!(text.contains("Boot volume: stormpump"), "{text}");
    assert!(text.contains("/dev/ublkb0"), "{text}");
}

/// A golden whose blocks are smaller than the volume's sectors is refused at
/// build time, not at boot.
///
/// Found on real hardware: the host's `mkfs.ext4` picks 1024-byte blocks for a
/// 64 MB *file* — its size class, and a file reads as 512-byte sectors — and
/// the volume that golden lands in has 4096-byte sectors. Everything downstream
/// succeeds: the image builds, both pallets verify, `boot-local` resolves the
/// clone and exports it. Then the kernel says `EXT4-fs (ublkb0): bad block size
/// 1024` and the node drops to a shell (#40).
///
/// The engine knows both numbers at build time, so this must fail there.
#[tokio::test]
async fn a_golden_with_blocks_smaller_than_the_volume_sector_is_refused() {
    let tmp = TempDir::new().unwrap();
    make_sources(tmp.path());

    // An ext4 superblock claiming 1024-byte blocks: magic 0xEF53 at 1024+0x38,
    // s_log_block_size at 1024+0x18, block size = 1024 << it.
    let mut golden = vec![0u8; 8 * 1024 * 1024];
    golden[1024 + 0x38] = 0x53;
    golden[1024 + 0x39] = 0xEF;
    golden[1024 + 0x18..1024 + 0x1C].copy_from_slice(&0u32.to_le_bytes());
    write(tmp.path(), "small-blocks.img", &golden);

    let extra = format!(
        r#"
[slab]
size = "rest"
tier = "hot"

  [[slab.golden]]
  name = "rootfs"
  file = "{img}"
"#,
        img = tmp.path().join("small-blocks.img").display(),
    );
    let spec = ImageSpec::from_toml(&spec_toml(tmp.path(), &extra)).unwrap();
    let err = ImageBuilder::new(spec)
        .build(&tmp.path().join("bad.img"))
        .await
        .expect_err("a 1024-block golden on a 4096-byte-sector volume must not build");
    let text = err.to_string();
    assert!(text.contains("1024-byte blocks"), "{text}");
    assert!(text.contains("4096-byte sectors"), "{text}");
    assert!(text.contains("mkfs.ext4 -b 4096"), "the error must say how to fix it: {text}");

    // …and the same golden at 4096-byte blocks builds.
    let mut ok_golden = vec![0u8; 8 * 1024 * 1024];
    ok_golden[1024 + 0x38] = 0x53;
    ok_golden[1024 + 0x39] = 0xEF;
    ok_golden[1024 + 0x18..1024 + 0x1C].copy_from_slice(&2u32.to_le_bytes());
    write(tmp.path(), "right-blocks.img", &ok_golden);
    let extra = extra.replace("small-blocks.img", "right-blocks.img");
    let spec = ImageSpec::from_toml(&spec_toml(tmp.path(), &extra)).unwrap();
    let report = ImageBuilder::new(spec)
        .build(&tmp.path().join("good.img"))
        .await
        .expect("4096-byte blocks match the volume's sectors");
    let slab = report.partitions.iter().find(|p| p.kind == "slab").unwrap();
    assert_eq!(slab.volumes.len(), 2, "{:#?}", slab.volumes);
}

/// #88 — an image that carries a data slab survives being reimaged.
///
/// The failure this guards is silent: tier-0 holds the node's CA private key
/// and its ServiceAccount token signing key, and a reinstall that reformats
/// the slab holding them mints new ones. Every token in the cluster becomes
/// invalid and the node comes up looking healthy.
///
/// So the assertions are the two halves of the split. The system slab and the
/// data slab are separate partitions with separate type GUIDs and separate
/// records of themselves; and after the system slab has been reformatted and
/// refilled — which is what an install is — the data volume is still there
/// with its contents intact.
#[tokio::test]
async fn a_reimage_replaces_the_system_slab_and_leaves_the_data_slab_alone() {
    use std::sync::Arc;
    use stormblock::drive::filedev::FileDevice;
    use stormblock::drive::partition::PartitionDevice;
    use stormblock::drive::slab::{Slab, SlabFormat, SlabRole};
    use stormblock::drive::BlockDevice;
    use stormblock::image::type_guid;
    use stormblock::raid::RaidArrayId;
    use stormblock::volume::VolumeManager;

    let tmp = TempDir::new().unwrap();
    make_sources(tmp.path());
    write(tmp.path(), "rootfs.img", &vec![0xAB; 2 * 1024 * 1024]);
    // The blank every `-data` volume is a clone of. It lives in the data
    // slab, because a clone shares its golden's slots: a blank in the system
    // slab would make the clone's unwritten extents point at the half an
    // install replaces.
    write(tmp.path(), "blank.img", &vec![0u8; 2 * 1024 * 1024]);

    let extra = format!(
        r#"
[[pallet]]
name = "system1"
kind = "system"
version = 1
members = [
  {{ name = "stormpump", role = "rootfs", file = "{rootfs}" }},
]

[data_slab]
size = "24M"
tier = "hot"

  [[data_slab.golden]]
  name = "stormcert"
  file = "{blank}"
  clone = "stormcert-data"

[slab]
size = "rest"
tier = "hot"

  [[slab.golden]]
  name = "stormpump"
  from = "pallet:system1/stormpump"
"#,
        rootfs = tmp.path().join("rootfs.img").display(),
        blank = tmp.path().join("blank.img").display(),
    );
    let mut toml = spec_toml(tmp.path(), &extra);
    toml = toml.replace("size = \"192M\"", "size = \"320M\"");
    let spec = ImageSpec::from_toml(&toml).unwrap();
    let out = tmp.path().join("node.img");
    let report = ImageBuilder::new(spec).build(&out).await.expect("build");

    let system = report.partitions.iter().find(|p| p.kind == "slab").expect("system slab");
    let data = report.partitions.iter().find(|p| p.kind == "data-slab").expect("data slab");
    assert_ne!(system.index, data.index, "two partitions, not one");
    // The data slab is allocated first, so growing the system slab across a
    // release does not move the partition holding the node's identity.
    assert!(data.start_bytes < system.start_bytes, "data slab comes first");

    // Each carries only its own volumes.
    let sys_names: Vec<&str> = system.volumes.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(sys_names, vec!["stormpump.golden", "stormpump"]);
    let data_names: Vec<&str> = data.volumes.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(data_names, vec!["stormcert.golden", "stormcert-data"]);

    // The partition table says which is which, without opening either.
    let table = stormblock::image::build::table_of(&out).await.unwrap();
    assert_eq!(table.entries[system.index].type_guid, type_guid::SLAB);
    assert_eq!(table.entries[data.index].type_guid, type_guid::SLAB_DATA);

    // And so does each slab's own header, which is what a whole-drive slab
    // with no partition table has instead.
    let listed = stormblock::image::build::slabs_in(&out).await.unwrap();
    let mut roles: Vec<&str> = listed.iter().map(|s| s.role.as_str()).collect();
    roles.sort_unstable();
    assert_eq!(roles, vec!["data", "system"]);

    // Write the node's identity into the clone in the data slab — the thing
    // nothing can mint again.
    let dev: Arc<dyn BlockDevice> =
        Arc::new(FileDevice::open(out.to_str().unwrap()).await.unwrap());
    let secret = vec![0x5Eu8; 4096];
    let open_both = |dev: Arc<dyn BlockDevice>| async move {
        let sys_part = Arc::new(
            PartitionDevice::new(dev.clone(), system.start_bytes, system.size_bytes).unwrap(),
        );
        let data_part =
            Arc::new(PartitionDevice::new(dev, data.start_bytes, data.size_bytes).unwrap());
        let sys_slab = Slab::open(sys_part).await.unwrap();
        let data_slab = Slab::open(data_part).await.unwrap();
        assert_eq!(data_slab.role(), SlabRole::Data);
        assert_eq!(sys_slab.role(), SlabRole::System);
        let slot_size = sys_slab.slot_size();
        let ids = vec![sys_slab.slab_id(), data_slab.slab_id()];
        let mut mgr = VolumeManager::new(slot_size);
        mgr.attach_slab(RaidArrayId(uuid::Uuid::new_v4()), sys_slab).await.unwrap();
        mgr.attach_slab(RaidArrayId(uuid::Uuid::new_v4()), data_slab).await.unwrap();
        mgr.persist_to_slabs(ids);
        mgr.restore().await.unwrap();
        mgr
    };

    {
        let mut mgr = open_both(dev.clone()).await;
        let names: Vec<String> = mgr.list_volumes().await.into_iter().map(|(_, n, ..)| n).collect();
        assert_eq!(names.len(), 4, "both slabs' records were read: {names:?}");
        let id = mgr.find_volume("stormcert-data").await.expect("the tier-0 clone");
        let vol = mgr.get_volume(&id).unwrap();
        vol.write(0, &secret).await.unwrap();
        vol.flush().await.unwrap();
        drop(vol);
        mgr.persist_checked().await.unwrap();
    }

    // Now reimage: reformat the system partition and lay a new golden and
    // clone into it, exactly as an install does. The data partition is not
    // touched, because nothing here has any reason to touch it.
    {
        let sys_part = Arc::new(
            PartitionDevice::new(dev.clone(), system.start_bytes, system.size_bytes).unwrap(),
        );
        let meta = stormblock::drive::slab::auto_metadata_bytes(system.size_bytes, 1024 * 1024);
        let fresh = Slab::format_with(
            sys_part,
            SlabFormat::new(1024 * 1024, stormblock::placement::topology::StorageTier::Hot)
                .with_metadata(meta),
        )
        .await
        .unwrap();
        let slab_id = fresh.slab_id();
        let mut mgr = VolumeManager::new(1024 * 1024);
        mgr.attach_slab(RaidArrayId(uuid::Uuid::new_v4()), fresh).await.unwrap();
        mgr.persist_to_slab(slab_id);
        let new_golden = mgr.create_volume_any("stormpump.golden", 2 * 1024 * 1024).await.unwrap();
        let v = mgr.get_volume(&new_golden).unwrap();
        v.write(0, &vec![0xF0; 4096]).await.unwrap();
        v.flush().await.unwrap();
        drop(v);
        mgr.seal_volume(new_golden, None).await.unwrap();
        mgr.create_snapshot(new_golden, "stormpump").await.unwrap();
        mgr.persist_checked().await.unwrap();
    }

    // The node boots again on both slabs. The new system volumes are there,
    // and so is the identity — which is the whole point.
    let mgr = open_both(dev.clone()).await;
    let names: Vec<String> = mgr.list_volumes().await.into_iter().map(|(_, n, ..)| n).collect();
    assert!(names.contains(&"stormpump".to_string()), "{names:?}");
    assert!(names.contains(&"stormcert-data".to_string()), "identity survived: {names:?}");

    let id = mgr.find_volume("stormcert-data").await.expect("tier-0 after the reimage");
    let vol = mgr.get_volume(&id).unwrap();
    let mut buf = vec![0u8; 4096];
    vol.read(0, &mut buf).await.unwrap();
    assert_eq!(buf, secret, "the reimage re-minted the node's identity");

    // The new system clone is the new one, not a ghost of the old.
    let sys = mgr.find_volume("stormpump").await.unwrap();
    let sv = mgr.get_volume(&sys).unwrap();
    sv.read(0, &mut buf).await.unwrap();
    assert!(buf.iter().all(|&b| b == 0xF0), "the system slab was replaced");
}

/// The boundary is hard, not a preference: a data volume that grows never
/// takes a slot in the system slab, and a system volume never takes one in
/// the data slab. Without this the split leaks one copy-on-write extent at a
/// time — the same loss as #88, just slower.
#[tokio::test]
async fn a_volume_allocates_only_in_slabs_of_its_own_role() {
    use std::sync::Arc;
    use stormblock::drive::filedev::FileDevice;
    use stormblock::drive::slab::{Slab, SlabFormat, SlabRole};
    use stormblock::drive::BlockDevice;
    use stormblock::placement::topology::StorageTier;
    use stormblock::raid::RaidArrayId;
    use stormblock::volume::{CreateOptions, VolumeManager};

    let tmp = TempDir::new().unwrap();
    let slot = 64 * 1024u64;
    let mut ids = Vec::new();
    let mut mgr = VolumeManager::new(slot);
    for (name, role) in [("system.slab", SlabRole::System), ("data.slab", SlabRole::Data)] {
        let p = tmp.path().join(name);
        std::fs::write(&p, vec![0u8; 8 * 1024 * 1024]).unwrap();
        let dev: Arc<dyn BlockDevice> =
            Arc::new(FileDevice::open(p.to_str().unwrap()).await.unwrap());
        let slab = Slab::format_with(dev, SlabFormat::new(slot, StorageTier::Hot).with_role(role))
            .await
            .unwrap();
        ids.push((role, slab.slab_id()));
        mgr.attach_slab(RaidArrayId(uuid::Uuid::new_v4()), slab).await.unwrap();
    }
    let system_slab = ids.iter().find(|(r, _)| *r == SlabRole::System).unwrap().1;
    let data_slab = ids.iter().find(|(r, _)| *r == SlabRole::Data).unwrap().1;

    let sys = mgr.create_volume_any("sys", 1024 * 1024).await.unwrap();
    let dat = mgr
        .create_volume_with(
            "identity",
            1024 * 1024,
            CreateOptions::default().in_role(SlabRole::Data),
        )
        .await
        .unwrap();
    for id in [sys, dat] {
        let v = mgr.get_volume(&id).unwrap();
        v.write(0, &vec![0x7Eu8; slot as usize]).await.unwrap();
        v.flush().await.unwrap();
    }

    async fn where_is(mgr: &VolumeManager, id: stormblock::volume::VolumeId) -> Vec<stormblock::drive::slab::SlabId> {
        mgr.gem()
            .read()
            .await
            .get_volume_map(&id)
            .map(|m| m.all_legs().map(|l| l.slab_id).collect())
            .unwrap_or_default()
    }
    assert_eq!(where_is(&mgr, sys).await, vec![system_slab], "a system volume stayed system-side");
    assert_eq!(where_is(&mgr, dat).await, vec![data_slab], "a data volume stayed data-side");

    // A clone inherits the role, so its copy-on-write lands data-side too.
    let clone = mgr.create_snapshot(dat, "identity-clone").await.unwrap();
    let cv = mgr.get_volume(&clone).unwrap();
    cv.write(0, &vec![0x11u8; slot as usize]).await.unwrap();
    cv.flush().await.unwrap();
    drop(cv);
    assert_eq!(where_is(&mgr, clone).await, vec![data_slab], "a clone's COW left the data slab");
}

/// #81 — a mistyped section is a parse error, not silence.
///
/// `[[slab.clone]]` where `[[slab.golden]]` was meant built cleanly, reported
/// success, and produced an image with two volumes missing. The symptom
/// arrived one image, one copy and one boot later, as a root device that
/// never appeared — and it pointed at the mount list rather than at the spec
/// that failed to create the volume.
#[tokio::test]
async fn a_spec_that_names_something_the_builder_does_not_know_is_refused() {
    let cases = [
        // The section that started it.
        (
            r#"
name = "x"
[slab]
size = "rest"
  [[slab.clone]]
  name = "cilium"
"#,
            "clone",
        ),
        // A misspelt data slab is the same class, and the cost is higher:
        // silence there means the node's identity shares a partition with
        // the goldens again (#88).
        (
            r#"
name = "x"
[data-slab]
size = "64M"
"#,
            "data-slab",
        ),
        (
            r#"
name = "x"
[[pallet]]
name = "system1"
verison = 1
"#,
            "verison",
        ),
        (
            r#"
name = "x"
[esp]
size = "24M"
from_directory = "esp"
"#,
            "from_directory",
        ),
    ];
    for (toml, offender) in cases {
        let err = ImageSpec::from_toml(toml).expect_err("must not parse: {toml}");
        let msg = err.to_string();
        assert!(msg.contains(offender), "did not name '{offender}': {msg}");
    }

    // And the spec that is right still parses.
    ImageSpec::from_toml(
        r#"
name = "x"
size = "320M"
[data_slab]
size = "64M"
  [[data_slab.golden]]
  name = "stormcert"
  clone = "stormcert-data"
[slab]
size = "rest"
  [[slab.golden]]
  name = "stormpump"
"#,
    )
    .expect("a correct spec still parses");
}
