//! A flavour's composed disk read off a stormdist release ISO (#442): the
//! `stormdist://` drive is a block device like any other. A disk laid as a
//! node's system drive, with a volume written in its slab, goes into an ISO
//! made by stormdist's own `make`. Opened back by flavour, it reads byte for
//! byte as the disk, its slab is found in its GPT, and its volume adopted
//! reads what was written. Writes are kept in memory and read back, and the
//! ISO is untouched. A wrong key, a flavour with no disk, and a corrupt
//! chunk are each refused.

use std::path::Path;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};
use stormblock::drive::filedev::FileDevice;
use stormblock::drive::BlockDevice;
use stormblock::volume::{VolumeManager, DEFAULT_EXTENT_SIZE};
use stormdist::format::{Flavour, Kind, Range};
use stormdist::media::{Input, Recipe};
use tempfile::TempDir;

const MIB: u64 = 1 << 20;
const DISK: u64 = 256 * MIB;
const PATTERN: &[u8] = b"stormblock#442: a byte the flow-over reads off the ISO. ";

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn key_file(dir: &Path, k: &SigningKey, name: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, format!("{}\n", hex::encode(k.verifying_key().to_bytes()))).unwrap();
    p.display().to_string()
}

/// A node's system drive: its GPT, boot area and system slab, with
/// `pod-logs` written in the slab. The volume's bytes, for comparing.
async fn composed_disk(path: &Path) -> Vec<u8> {
    let dev: Arc<dyn BlockDevice> =
        Arc::new(FileDevice::open_with_capacity(path.to_str().unwrap(), DISK).await.unwrap());
    let mut layout = stormblock::image::local::LocalLayout::for_drive(DISK);
    layout.slot_size = DEFAULT_EXTENT_SIZE;
    let laid = stormblock::image::local::lay_system_drive(dev.clone(), &layout).await.unwrap();
    let sid = laid.system.slab_id();
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.add_slab(laid.system).await;
    vm.keep_metadata_in_first(&[sid]);
    let v = vm.create_volume_any("pod-logs", 16 * MIB).await.unwrap();
    let data: Vec<u8> = PATTERN.iter().cycle().take(MIB as usize).copied().collect();
    let d = vm.get_volume(&v).unwrap();
    d.write(0, &data).await.unwrap();
    d.flush().await.unwrap();
    vm.persist_checked().await.unwrap();
    data
}

/// The disk as a recipe input: its allocated MiB as ranges.
fn input(name: &str, component: &str, image: &Path) -> Input {
    let bytes = std::fs::read(image).unwrap();
    let mut ranges: Vec<Range> = Vec::new();
    for (i, c) in bytes.chunks(MIB as usize).enumerate() {
        if c.iter().any(|b| *b != 0) {
            let off = i as u64 * MIB;
            match ranges.last_mut() {
                Some(r) if r.offset + r.length == off => r.length += c.len() as u64,
                _ => ranges.push(Range { offset: off, length: c.len() as u64 }),
            }
        }
    }
    Input {
        name: name.into(),
        component: component.into(),
        commit: "0123456789ab".into(),
        image: image.to_path_buf(),
        size: bytes.len() as u64,
        device_sha256: hex::encode(Sha256::digest(&bytes)),
        ranges,
    }
}

fn sha_of(p: &Path) -> String {
    hex::encode(Sha256::digest(std::fs::read(p).unwrap()))
}

#[tokio::test]
async fn a_flavours_disk_reads_off_the_iso_as_a_drive() {
    let dir = TempDir::new().unwrap();
    let disk = dir.path().join("stormcos-sno.img");
    let written = composed_disk(&disk).await;
    let other = dir.path().join("golden-demo.img");
    std::fs::write(&other, vec![7u8; MIB as usize]).unwrap();

    let k = key(9);
    let iso = dir.path().join("12.99.iso");
    let recipe = Recipe {
        version: "12.99".into(),
        flavour: vec![
            Flavour { name: "cilium".into(), members: vec!["stormcos-sno-12.99".into(), "golden-demo-0123456789ab".into()] },
            Flavour { name: "flowsdn".into(), members: vec!["golden-demo-0123456789ab".into()] },
        ],
        kind: Kind::Full,
        volumes: vec![
            input("stormcos-sno-12.99", "stormcos-sno", &disk),
            input("golden-demo-0123456789ab", "demo", &other),
        ],
        forge: None,
        boot: None,
    };
    stormdist::media::make(&recipe, &iso, &k).unwrap();
    let iso_sha = sha_of(&iso);
    let kf = key_file(dir.path(), &k, "release-key.pub");
    let uri = format!("stormdist://{}?flavour=cilium&key={kf}", iso.display());

    // The flavour's disk, byte for byte.
    let dev = stormblock::drive::open_path(&uri, true).await.unwrap();
    assert_eq!(dev.capacity_bytes(), DISK);
    let mut all = vec![0u8; DISK as usize];
    let mut off = 0;
    while off < DISK {
        dev.read(off, &mut all[off as usize..(off + 4 * MIB) as usize]).await.unwrap();
        off += 4 * MIB;
    }
    assert_eq!(hex::encode(Sha256::digest(&all)), sha_of(&disk), "the disk as it went in");

    // An ordinary slab: found in its GPT, its volume adopted and read.
    let found = stormblock::drive::discover::slabs_in_partitions(&dev).await;
    assert_eq!(found.len(), 1, "the system slab");
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    vm.adopt_slabs(found).await.unwrap();
    let id = vm.find_volume("pod-logs").await.expect("the volume");
    let mut got = vec![0u8; MIB as usize];
    vm.get_volume(&id).unwrap().read(0, &mut got).await.unwrap();
    assert_eq!(got, written, "the volume reads what was written");

    // A write reads back, beside an unwritten neighbour; the ISO is untouched.
    let page = 200 * MIB + 4096;
    dev.write(page, &[0xa5u8; 4096]).await.unwrap();
    dev.write(page + 4096 + 512, &[0x5au8; 512]).await.unwrap();
    let mut back = vec![0u8; 8192 + 1024];
    dev.read(page, &mut back).await.unwrap();
    assert!(back[..4096].iter().all(|b| *b == 0xa5));
    assert!(back[4096..4096 + 512] == all[(page + 4096) as usize..(page + 4096 + 512) as usize], "the rest of a page is the ISO's");
    assert!(back[4096 + 512..4096 + 1024].iter().all(|b| *b == 0x5a));
    assert_eq!(sha_of(&iso), iso_sha, "nothing reaches the ISO");

    // By member, and refusals.
    let by_member = format!("stormdist://{}?member=stormcos-sno&key={kf}", iso.display());
    assert_eq!(stormblock::drive::open_path(&by_member, true).await.unwrap().capacity_bytes(), DISK);
    let no_disk = format!("stormdist://{}?flavour=flowsdn&key={kf}", iso.display());
    let e = stormblock::drive::open_path(&no_disk, true).await.err().expect("flowsdn carries no disk").to_string();
    assert!(e.contains("stormcos-sno-flowsdn"), "{e}");
    let wrong = key_file(dir.path(), &key(3), "other.pub");
    let e = stormblock::drive::open_path(&format!("stormdist://{}?flavour=cilium&key={wrong}", iso.display()), true)
        .await
        .err()
        .expect("another key")
        .to_string();
    assert!(e.contains("not a release ISO this key signs"), "{e}");
    let e = stormblock::drive::open_path(&format!("stormdist://{}?flavour=cilium", iso.display()), true)
        .await
        .err()
        .expect("no key")
        .to_string();
    assert!(e.contains("key="), "{e}");

    // A chunk changed on the medium: its read fails, never returns bytes.
    let mut bytes = std::fs::read(&iso).unwrap();
    let at = bytes.windows(PATTERN.len()).position(|w| w == PATTERN).expect("the volume's bytes are in a chunk");
    bytes[at] ^= 0xff;
    std::fs::write(&iso, &bytes).unwrap();
    let dev = stormblock::drive::open_path(&uri, true).await.unwrap();
    let found = stormblock::drive::discover::slabs_in_partitions(&dev).await;
    let mut vm = VolumeManager::new(DEFAULT_EXTENT_SIZE);
    let read = async {
        vm.adopt_slabs(found).await.map_err(|e| e.to_string())?;
        let id = vm.find_volume("pod-logs").await.ok_or("no volume")?;
        let mut got = vec![0u8; MIB as usize];
        vm.get_volume(&id).unwrap().read(0, &mut got).await.map_err(|e| e.to_string())?;
        Ok::<Vec<u8>, String>(got)
    };
    match read.await {
        Err(e) => assert!(e.contains("chunk") || e.contains("bad"), "{e}"),
        Ok(got) => panic!("a corrupt chunk was read ({} bytes)", got.len()),
    }
}

#[test]
fn a_stormdist_path_names_its_iso_key_and_disk() {
    use stormblock::drive::isodev::{disk_name, IsoSpec};
    let s = IsoSpec::parse("stormdist:///dev/sr0?flavour=cilium&key=/etc/stormblock/release-key.pub").unwrap().unwrap();
    assert_eq!((s.iso.as_str(), s.flavour.as_deref(), s.member), ("/dev/sr0", Some("cilium"), None));
    assert!(IsoSpec::parse("nvme-tcp://h:4420/n").is_none());
    assert!(IsoSpec::parse("stormdist:///dev/sr0?flavour=cilium").unwrap().is_err(), "no key");
    assert!(IsoSpec::parse("stormdist:///dev/sr0?key=k").unwrap().is_err(), "neither flavour nor member");
    assert!(IsoSpec::parse("stormdist:///dev/sr0?key=k&flavour=a&member=b").unwrap().is_err(), "both");
    assert_eq!(disk_name("cilium"), "stormcos-sno");
    assert_eq!(disk_name("flowsdn"), "stormcos-sno-flowsdn");
}
