//! The screen's virtual terminals (#618): the screen on tty1 in graphics
//! mode (no console text drawn over it), the full text log on tty2, and the
//! two keys between them: F2 shows the log, F1 the screen. The keys are read
//! raw from both terminals, so no key reaches a shell (there is none to
//! reach: principle 5); the serial console is not touched.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;

const VT_ACTIVATE: libc::c_ulong = 0x5606;
const VT_WAITACTIVE: libc::c_ulong = 0x5607;
const KDSETMODE: libc::c_ulong = 0x4B3A;
const KD_TEXT: libc::c_int = 0;
const KD_GRAPHICS: libc::c_int = 1;
const KDGKBMODE: libc::c_ulong = 0x4B44;
const KDSKBMODE: libc::c_ulong = 0x4B45;
const K_MEDIUMRAW: libc::c_int = 2;

/// Keycodes in medium-raw mode (a release has bit 7 set).
pub const KEY_F1: u8 = 59;
pub const KEY_F2: u8 = 60;

/// What a byte read from a terminal in medium-raw mode asks for.
pub fn key_of(b: u8) -> Option<u8> {
    match b {
        KEY_F1 | KEY_F2 => Some(b),
        _ => None,
    }
}

pub struct Term {
    file: std::fs::File,
    kb_was: libc::c_int,
    termios_was: Option<libc::termios>,
}

impl Term {
    /// `/dev/ttyN`, its keyboard raw and its input unbuffered.
    pub fn open(n: u32) -> std::io::Result<Term> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
            .open(format!("/dev/tty{n}"))?;
        let fd = file.as_raw_fd();
        let mut kb_was: libc::c_int = 0;
        // SAFETY: the console ioctls, on a terminal this process opened.
        unsafe { libc::ioctl(fd, KDGKBMODE as _, &mut kb_was) };
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        let termios_was = if unsafe { libc::tcgetattr(fd, &mut t) } == 0 {
            let was = t;
            unsafe { libc::cfmakeraw(&mut t) };
            // Output is the log's: keep its newlines turned into CR LF.
            t.c_oflag |= libc::OPOST | libc::ONLCR;
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
            Some(was)
        } else {
            None
        };
        unsafe { libc::ioctl(fd, KDSKBMODE as _, K_MEDIUMRAW) };
        Ok(Term { file, kb_was, termios_was })
    }

    pub fn graphics(&self, on: bool) {
        // SAFETY: as above.
        unsafe { libc::ioctl(self.file.as_raw_fd(), KDSETMODE as _, if on { KD_GRAPHICS } else { KD_TEXT }) };
    }

    /// Show this terminal's VT (`n`), and wait until it is shown.
    pub fn activate(&self, n: u32) {
        let fd = self.file.as_raw_fd();
        // SAFETY: as above.
        unsafe {
            libc::ioctl(fd, VT_ACTIVATE as _, n as libc::c_int);
            libc::ioctl(fd, VT_WAITACTIVE as _, n as libc::c_int);
        }
    }

    /// The keys pressed since the last call.
    pub fn keys(&mut self) -> Vec<u8> {
        let mut buf = [0u8; 64];
        match self.file.read(&mut buf) {
            Ok(n) => buf[..n].iter().filter_map(|b| key_of(*b)).collect(),
            Err(_) => vec![],
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.file.write_all(bytes);
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let fd = self.file.as_raw_fd();
        // SAFETY: as above; the modes it had when opened.
        unsafe {
            libc::ioctl(fd, KDSKBMODE as _, self.kb_was);
            libc::ioctl(fd, KDSETMODE as _, KD_TEXT);
            if let Some(t) = &self.termios_was {
                libc::tcsetattr(fd, libc::TCSANOW, t);
            }
        }
    }
}

/// Follows a log file from where it was, for tty2: the new bytes since the
/// last call (a file that shrank is read from its start again).
#[derive(Default)]
pub struct Follow {
    pos: u64,
}

impl Follow {
    pub fn new_bytes(&mut self, path: &std::path::Path) -> Vec<u8> {
        use std::io::{Seek, SeekFrom};
        let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.pos {
            self.pos = 0;
        }
        if len == self.pos || f.seek(SeekFrom::Start(self.pos)).is_err() {
            return vec![];
        }
        let mut out = Vec::new();
        let _ = f.take(1 << 20).read_to_end(&mut out);
        self.pos += out.len() as u64;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_the_log_followed() {
        assert_eq!(key_of(59), Some(KEY_F1));
        assert_eq!(key_of(60), Some(KEY_F2));
        assert_eq!(key_of(60 | 0x80), None, "a release");
        assert_eq!(key_of(30), None, "any other key does nothing");
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("init.log");
        let mut f = Follow::default();
        assert!(f.new_bytes(&p).is_empty());
        std::fs::write(&p, "one\n").unwrap();
        assert_eq!(f.new_bytes(&p), b"one\n");
        std::fs::write(&p, "one\ntwo\n").unwrap();
        assert_eq!(f.new_bytes(&p), b"two\n");
        std::fs::write(&p, "new\n").unwrap();
        assert_eq!(f.new_bytes(&p), b"new\n", "a file written again is read from its start");
    }
}
