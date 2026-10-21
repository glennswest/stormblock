//! The image a boot target serves: one file, opened read-only.
//!
//! A golden's raw disk or an ISO. Its length need not be a whole number of
//! blocks (an ISO is 2048-byte sectors): the namespace is rounded up and what
//! lies past the end of the file reads as zeros.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

/// Read exactly `buf.len()` bytes at `off`, or fewer at the end of the file
/// (the rest of `buf` is zeroed). Positional, so many readers share one handle.
pub(crate) fn read_at(file: &File, buf: &mut [u8], off: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = pread(file, &mut buf[done..], off + done as u64)?;
        if n == 0 {
            buf[done..].fill(0);
            return Ok(());
        }
        done += n;
    }
    Ok(())
}

/// Write all of `buf` at `off`.
pub(crate) fn write_at(file: &File, buf: &[u8], off: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = pwrite(file, &buf[done..], off + done as u64)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
        }
        done += n;
    }
    Ok(())
}

#[cfg(unix)]
fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, off)
}
#[cfg(unix)]
fn pwrite(file: &File, buf: &[u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, buf, off)
}
// Windows' seek_read moves the handle's position too; nothing here relies on
// the position, so that is harmless.
#[cfg(windows)]
fn pread(file: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, off)
}
#[cfg(windows)]
fn pwrite(file: &File, buf: &[u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, buf, off)
}

/// A read-only image file.
#[derive(Debug)]
pub struct Image {
    file: File,
    path: PathBuf,
    len: u64,
    block_size: u32,
    nguid: [u8; 16],
}

impl Image {
    /// Open `path` read-only, presented in blocks of `block_size` (512 or
    /// 4096). Nothing is ever written to it.
    pub fn open(path: &Path, block_size: u32) -> io::Result<Image> {
        if block_size != 512 && block_size != 4096 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "block size is 512 or 4096"));
        }
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is empty", path.display())));
        }
        // The namespace's identity, the same for one file every time it is
        // served: a host that sees it again sees the same disk.
        let mut h = sha2::Sha256::new();
        use sha2::Digest;
        h.update(b"stormboot-serve image ");
        h.update(path.to_string_lossy().as_bytes());
        h.update(len.to_le_bytes());
        let d = h.finalize();
        let mut nguid = [0u8; 16];
        nguid.copy_from_slice(&d[..16]);
        Ok(Image { file, path: path.to_path_buf(), len, block_size, nguid })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The file's length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// The namespace's size in blocks: the file rounded up.
    pub fn blocks(&self) -> u64 {
        self.len.div_ceil(self.block_size as u64)
    }

    pub fn nguid(&self) -> [u8; 16] {
        self.nguid
    }

    /// Read at `off`; past the end of the file is zeros.
    pub fn read(&self, buf: &mut [u8], off: u64) -> io::Result<()> {
        if off >= self.len {
            buf.fill(0);
            return Ok(());
        }
        read_at(&self.file, buf, off)
    }
}
