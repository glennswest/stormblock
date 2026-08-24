//! How small a golden can actually be.
//!
//! The question this answers is not "does ext4 have a documented minimum" but
//! "what does *our* formatter produce, on a volume whose logical sector is
//! 4096, and does `fsck` accept it". A settings container or a cmdline
//! container is a few MB at most, and the floor is set by filesystem metadata
//! rather than by anything in the block layer — so it has to be measured, not
//! reasoned about.

use std::sync::Arc;

use stormblock::drive::filedev::FileDevice;
use stormblock::drive::BlockDevice;
use stormblock::fs::ext4::{self, Ext4Params};

async fn volume(bytes: u64, name: &str) -> (Arc<dyn BlockDevice>, String) {
    let dir = std::env::temp_dir().join("stormblock-small-vol");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}-{}.img", uuid::Uuid::new_v4().simple()));
    let p = path.to_str().unwrap().to_string();
    let _ = std::fs::remove_file(&p);
    let dev = FileDevice::open_with_capacity(&p, bytes).await.unwrap();
    (Arc::new(dev), p)
}

/// The whole small end, in one table, with `fsck` as the arbiter.
#[tokio::test]
async fn the_small_end_formats_and_checks() {
    // (size, journal) — None lets the profile and size class decide.
    let cases: [(u64, Option<bool>); 7] = [
        (1024 * 1024, None),
        (2 * 1024 * 1024, None),
        (4 * 1024 * 1024, None),
        (8 * 1024 * 1024, None),
        (8 * 1024 * 1024, Some(false)),
        (16 * 1024 * 1024, None),
        (64 * 1024 * 1024, None),
    ];

    println!(
        "{:>8}  {:>5}  {:>7}  {:>7}  {:>8}  {:>8}  {:>6}",
        "size", "bs", "blocks", "journal", "usable", "overhead", "inodes"
    );

    for (bytes, journal) in cases {
        let (dev, path) = volume(bytes, "small").await;
        let params = Ext4Params { journal, ..Default::default() };
        let report = match ext4::format(&dev, &params).await {
            Ok(r) => r,
            Err(e) => {
                println!("{:>8}  REFUSED: {e}", human(bytes));
                let _ = std::fs::remove_file(&path);
                continue;
            }
        };
        let usable = report.free_blocks * report.block_size as u64;
        println!(
            "{:>8}  {:>5}  {:>7}  {:>7}  {:>8}  {:>8}  {:>6}",
            human(bytes),
            report.block_size,
            report.blocks,
            report.journal_blocks,
            human(usable),
            human(bytes - usable),
            report.inodes,
        );

        // The number only means something if the filesystem is sound.
        let verdict = ext4::check(&dev).await.expect("fsck runs");
        assert!(
            verdict.problems.is_empty(),
            "{} (journal {journal:?}) did not check out: {verdict:?}",
            human(bytes)
        );
        let _ = std::fs::remove_file(&path);
    }
}

/// A 1 MB golden is the smallest thing worth having — a rendered config, a
/// kernel command line. It must format on a 4 KiB-sector volume, check clean,
/// and leave enough room to be worth the trouble.
#[tokio::test]
async fn a_one_megabyte_golden_is_usable() {
    let (dev, path) = volume(1024 * 1024, "onemeg").await;
    assert_eq!(dev.block_size(), 4096, "the floor this has to work against");

    let report = ext4::format(&dev, &Ext4Params::default())
        .await
        .expect("1 MB must format");
    assert_eq!(report.block_size, 4096, "never below the volume's sector (#40)");
    assert_eq!(report.journal_blocks, 0, "a 4 MB journal cannot fit in 1 MB");

    let usable = report.free_blocks * report.block_size as u64;
    assert!(
        usable >= 512 * 1024,
        "1 MB volume left only {usable} bytes usable — not worth having"
    );

    let verdict = ext4::check(&dev).await.expect("fsck runs");
    assert!(verdict.problems.is_empty(), "1 MB filesystem did not check out: {verdict:?}");

    let _ = std::fs::remove_file(&path);
}

fn human(b: u64) -> String {
    if b >= 1024 * 1024 {
        format!("{} MB", b / (1024 * 1024))
    } else {
        format!("{} KB", b / 1024)
    }
}
