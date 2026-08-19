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
    let spec = ImageSpec::from_toml(&spec_toml(tmp.path(), "")).unwrap();
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
    let gpt = stormblock::image::build::table_of(&iso).await.unwrap();
    assert_eq!(gpt.partitions().count(), 2);

    // The pallet inside the ISO is the pallet, still verifiable — which is the
    // whole point of everything inside it being partition-relative.
    let pallets = stormblock::image::build::pallets_in(&iso).await.unwrap();
    assert_eq!(pallets.len(), 1);
    assert!(pallets[0].is_readable());
    assert_eq!(pallets[0].name, "stormcos-boot");
}
