//! A host's copy-on-write overlay over a read-only [`Image`].
//!
//! Every write a host makes lands here, never in the image: a scratch file
//! that holds only the blocks written, appended in the order they were first
//! written, with an index from the namespace's block to its place in the file.
//! Append-only rather than a sparse file the size of the image, because a
//! sparse file is not sparse on every filesystem a laptop has (NTFS needs a
//! flag to be), and a 48 GiB image would be written out to its end by the
//! first write near it. Blocks zeroed or deallocated take no room at all.
//!
//! The scratch file is removed when the overlay is dropped: a new claim
//! starts from the image again, as a fresh clone does on forge.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::image::{read_at, write_at, Image};

/// What a written block holds.
#[derive(Clone, Copy)]
enum Block {
    /// Its bytes, at this block's index in the scratch file.
    At(u64),
    /// All zeros (written zeros, or deallocated).
    Zero,
}

/// The overlay's unit: one namespace block.
pub struct Overlay {
    image: Arc<Image>,
    scratch: File,
    path: PathBuf,
    state: Mutex<State>,
}

struct State {
    blocks: HashMap<u64, Block>,
    /// The next free block in the scratch file.
    next: u64,
}

impl Overlay {
    /// A fresh overlay over `image`, its scratch file created in `dir`.
    ///
    /// The file's name is opaque (`<16 hex digits>.overlay`, random) and never
    /// the host's spelling, and it is created with `create_new`: two hosts
    /// whose names differ only in punctuation or case (`node_a`, `node+a`,
    /// `Node-A` on a case-insensitive filesystem) can never share, or
    /// truncate, each other's file (finding 95472f7cca9a). The path kept is
    /// the one created, and only it is removed on drop.
    pub fn create(image: Arc<Image>, dir: &Path) -> io::Result<Overlay> {
        Self::create_named(image, dir, rand::random::<u64>)
    }

    /// [`Overlay::create`] with the candidate names drawn from `pick`; a name
    /// already taken is skipped, never opened.
    fn create_named(image: Arc<Image>, dir: &Path, mut pick: impl FnMut() -> u64) -> io::Result<Overlay> {
        std::fs::create_dir_all(dir)?;
        const TRIES: usize = 64;
        for _ in 0..TRIES {
            let path = dir.join(format!("{:016x}.overlay", pick()));
            match OpenOptions::new().read(true).write(true).create_new(true).open(&path) {
                Ok(scratch) => {
                    return Ok(Overlay {
                        image,
                        scratch,
                        path,
                        state: Mutex::new(State { blocks: HashMap::new(), next: 0 }),
                    })
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("no free overlay name in {} after {TRIES} tries", dir.display()),
        ))
    }

    pub fn image(&self) -> &Arc<Image> {
        &self.image
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn bs(&self) -> u64 {
        self.image.block_size() as u64
    }

    /// Blocks this host has written (zeroed ones included).
    pub fn written_blocks(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).blocks.len()
    }

    /// Bytes the scratch file holds.
    pub fn scratch_bytes(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).next * self.bs()
    }

    fn check(&self, off: u64, len: usize) -> io::Result<()> {
        let end = off.checked_add(len as u64).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "overflow"))?;
        if end > self.image.blocks() * self.bs() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "past the end of the namespace"));
        }
        Ok(())
    }

    /// One block as this host sees it.
    fn read_block(&self, st: &State, lba: u64, out: &mut [u8]) -> io::Result<()> {
        match st.blocks.get(&lba) {
            Some(Block::At(i)) => read_at(&self.scratch, out, i * self.bs()),
            Some(Block::Zero) => {
                out.fill(0);
                Ok(())
            }
            None => self.image.read(out, lba * self.bs()),
        }
    }

    /// Read `buf.len()` bytes at `off`: the host's own blocks where it wrote
    /// them, the image's elsewhere.
    pub fn read(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        self.check(off, buf.len())?;
        let bs = self.bs();
        let st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        // Nothing written in the range: one read of the image.
        let first = off / bs;
        let last = (off + buf.len() as u64).div_ceil(bs);
        if (first..last).all(|b| !st.blocks.contains_key(&b)) {
            return self.image.read(buf, off);
        }
        let mut block = vec![0u8; bs as usize];
        let mut done = 0usize;
        while done < buf.len() {
            let pos = off + done as u64;
            let lba = pos / bs;
            let inner = (pos % bs) as usize;
            let n = (bs as usize - inner).min(buf.len() - done);
            self.read_block(&st, lba, &mut block)?;
            buf[done..done + n].copy_from_slice(&block[inner..inner + n]);
            done += n;
        }
        Ok(())
    }

    /// Write `data` at `off` into the host's own blocks. A block written in
    /// part is the image's (or the host's) with this part over it.
    pub fn write(&self, data: &[u8], off: u64) -> io::Result<()> {
        self.check(off, data.len())?;
        let bs = self.bs();
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut block = vec![0u8; bs as usize];
        let mut done = 0usize;
        while done < data.len() {
            let pos = off + done as u64;
            let lba = pos / bs;
            let inner = (pos % bs) as usize;
            let n = (bs as usize - inner).min(data.len() - done);
            if n < bs as usize {
                self.read_block(&st, lba, &mut block)?;
            }
            block[inner..inner + n].copy_from_slice(&data[done..done + n]);
            let at = match st.blocks.get(&lba) {
                Some(Block::At(i)) => *i,
                _ => {
                    let i = st.next;
                    st.next += 1;
                    i
                }
            };
            write_at(&self.scratch, &block, at * bs)?;
            st.blocks.insert(lba, Block::At(at));
            done += n;
        }
        Ok(())
    }

    /// Zero `blocks` blocks from `lba` (Write Zeroes, Deallocate): recorded,
    /// nothing written. A block's old scratch space is not reused.
    pub fn zero(&self, lba: u64, blocks: u64) -> io::Result<()> {
        self.check(lba * self.bs(), (blocks * self.bs()) as usize)?;
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for b in lba..lba + blocks {
            st.blocks.insert(b, Block::Zero);
        }
        Ok(())
    }

    /// Make what was written durable in the scratch file. A host's writes
    /// live as long as this overlay; this is what a Flush asks for.
    pub fn flush(&self) -> io::Result<()> {
        self.scratch.sync_data()
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(dir: &Path, len: usize, bs: u32) -> Arc<Image> {
        let p = dir.join("image.raw");
        let bytes: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &bytes).unwrap();
        Arc::new(Image::open(&p, bs).unwrap())
    }

    fn scratch() -> PathBuf {
        let d = std::env::temp_dir().join(format!("stormboot-overlay-{}", rand::random::<u64>()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn writes_land_in_the_overlay_and_never_in_the_image() {
        let dir = scratch();
        let img = image(&dir, 64 * 1024 + 100, 4096);
        assert_eq!(img.blocks(), 17, "an image's tail rounds up");
        let before = std::fs::read(img.path()).unwrap();
        let a = Overlay::create(img.clone(), &dir.join("s")).unwrap();
        let b = Overlay::create(img.clone(), &dir.join("s")).unwrap();

        // A partial write in the middle of a block keeps the rest of it.
        a.write(&[0xEE; 10], 4096 + 5).unwrap();
        let mut got = vec![0u8; 4096];
        a.read(&mut got, 4096).unwrap();
        let mut want = before[4096..8192].to_vec();
        want[5..15].fill(0xEE);
        assert_eq!(got, want);

        // Across blocks, and read back unaligned.
        a.write(&vec![0x11; 9000], 10_000).unwrap();
        let mut got = vec![0u8; 9100];
        a.read(&mut got, 9_950).unwrap();
        assert_eq!(&got[..50], &before[9_950..10_000]);
        assert!(got[50..9050].iter().all(|&x| x == 0x11));
        assert_eq!(&got[9050..], &before[19_000..19_050]);

        // The other host and the file itself see the image.
        let mut other = vec![0u8; before.len()];
        b.read(&mut other, 0).unwrap();
        assert_eq!(other, before);
        assert_eq!(std::fs::read(img.path()).unwrap(), before, "the image is never written");

        // Past the end of the file: zeros, inside the namespace.
        let mut tail = vec![1u8; 4096];
        b.read(&mut tail, 16 * 4096).unwrap();
        assert_eq!(&tail[..100], &before[16 * 4096..]);
        assert!(tail[100..].iter().all(|&x| x == 0));
        assert!(b.read(&mut tail, 17 * 4096).is_err(), "past the namespace");

        // Zeroed blocks take no room.
        let used = a.scratch_bytes();
        a.zero(10, 4).unwrap();
        assert_eq!(a.scratch_bytes(), used);
        let mut z = vec![1u8; 4 * 4096];
        a.read(&mut z, 10 * 4096).unwrap();
        assert!(z.iter().all(|&x| x == 0));

        let p = a.path().to_path_buf();
        assert!(p.exists());
        drop(a);
        assert!(!p.exists(), "the scratch file goes with the overlay");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Finding 95472f7cca9a: hosts whose names fold to the same spelling
    /// (punctuation, or case on a case-insensitive filesystem) each get a
    /// file of their own, and neither sees nor removes the other's writes.
    #[test]
    fn colliding_host_spellings_never_share_an_overlay() {
        let dir = scratch();
        let img = image(&dir, 16 * 4096, 4096);
        let s = dir.join("s");
        let hosts = ["node_a", "node+a", "node a", "Node_A", "NODE_A"];
        let overlays: Vec<Overlay> = hosts.iter().map(|_| Overlay::create(img.clone(), &s).unwrap()).collect();

        let mut paths: Vec<String> = Vec::new();
        for o in &overlays {
            let name = o.path().file_name().unwrap().to_str().unwrap().to_string();
            assert!(name.ends_with(".overlay"));
            let stem = name.trim_end_matches(".overlay");
            assert_eq!(stem.len(), 16);
            assert!(
                stem.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "lowercase hex only, so case folding cannot make two names one: {name}"
            );
            for h in hosts {
                assert!(!name.to_ascii_lowercase().contains(&h.to_ascii_lowercase()), "no host spelling in {name}");
            }
            // Distinct even case-folded.
            assert!(!paths.contains(&name.to_ascii_lowercase()), "{name} shared");
            paths.push(name.to_ascii_lowercase());
        }

        // Each host writes its own byte to the same block.
        for (i, o) in overlays.iter().enumerate() {
            o.write(&[i as u8 + 1; 4096], 3 * 4096).unwrap();
        }
        for (i, o) in overlays.iter().enumerate() {
            let mut got = vec![0u8; 4096];
            o.read(&mut got, 3 * 4096).unwrap();
            assert!(got.iter().all(|&x| x == i as u8 + 1), "host {} reads its own block", hosts[i]);
        }

        // Releasing one leaves the others' files and data.
        let mut overlays = overlays;
        let gone = overlays.remove(1);
        let gone_path = gone.path().to_path_buf();
        drop(gone);
        assert!(!gone_path.exists());
        for (i, o) in overlays.iter().enumerate() {
            let n = if i == 0 { 1u8 } else { i as u8 + 2 };
            assert!(o.path().exists());
            let mut got = vec![0u8; 4096];
            o.read(&mut got, 3 * 4096).unwrap();
            assert!(got.iter().all(|&x| x == n));
        }
        drop(overlays);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A name already on disk (a live overlay's, or anything else) is never
    /// opened or truncated: the next one is taken.
    #[test]
    fn a_taken_name_is_skipped_never_truncated() {
        let dir = scratch();
        let img = image(&dir, 4 * 4096, 4096);
        let s = dir.join("s");
        let first = Overlay::create_named(img.clone(), &s, || 7).unwrap();
        first.write(&[0xAB; 4096], 0).unwrap();
        first.flush().unwrap();
        let len = std::fs::metadata(first.path()).unwrap().len();
        assert_eq!(len, 4096);

        let mut picks = [7u64, 7, 8].into_iter();
        let second = Overlay::create_named(img.clone(), &s, || picks.next().unwrap()).unwrap();
        assert_ne!(first.path(), second.path());
        assert!(second.path().ends_with("0000000000000008.overlay"));
        assert_eq!(std::fs::metadata(first.path()).unwrap().len(), len, "the first file is not truncated");
        let mut got = vec![0u8; 4096];
        first.read(&mut got, 0).unwrap();
        assert!(got.iter().all(|&x| x == 0xAB));

        // Every candidate taken: an error, nothing opened.
        assert_eq!(
            Overlay::create_named(img.clone(), &s, || 7).err().map(|e| e.kind()),
            Some(io::ErrorKind::AlreadyExists)
        );
        drop(second);
        assert!(first.path().exists(), "dropping one overlay removes only its own file");
        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
