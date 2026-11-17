//! The firmware's display through DRM (#618): the first connected connector,
//! its preferred mode, a dumb buffer scanned out on a CRTC that can drive
//! it. Works on what the kernel hands over at boot (`simpledrm` over the
//! UEFI GOP, `efifb`'s DRM successor) and on a real driver alike, with no
//! library: the KMS ioctls of `<drm/drm.h>` and `<drm/drm_mode.h>`.

use std::os::fd::{AsRawFd, OwnedFd};

const fn iowr(nr: u32, size: usize) -> libc::c_ulong {
    ((3u32 << 30) | ((size as u32) << 16) | ((b'd' as u32) << 8) | nr) as libc::c_ulong
}
const fn io(nr: u32) -> libc::c_ulong {
    (((b'd' as u32) << 8) | nr) as libc::c_ulong
}

#[allow(dead_code)] // the kernel's struct: some fields are only its to fill
#[repr(C)]
#[derive(Default)]
struct CardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[allow(dead_code)] // the kernel's struct: some fields are only its to fill
#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct ModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub type_: u32,
    pub name: [u8; 32],
}

#[allow(dead_code)] // the kernel's struct: some fields are only its to fill
#[repr(C)]
#[derive(Default)]
struct GetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[allow(dead_code)] // the kernel's struct: some fields are only its to fill
#[repr(C)]
#[derive(Default)]
struct GetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[allow(dead_code)] // the kernel's struct: some fields are only its to fill
#[repr(C)]
#[derive(Default)]
struct Crtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: ModeInfo,
}

#[allow(dead_code)] // the kernel's struct: some fields are only its to fill
#[repr(C)]
#[derive(Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[allow(dead_code)] // the kernel's struct: some fields are only its to fill
#[repr(C)]
#[derive(Default)]
struct MapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[allow(dead_code)] // the kernel's struct: some fields are only its to fill
#[repr(C)]
#[derive(Default)]
struct FbCmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

#[allow(dead_code)] // the kernel's struct: some fields are only its to fill
#[repr(C)]
#[derive(Default)]
struct FbDirty {
    fb_id: u32,
    flags: u32,
    color: u32,
    num_clips: u32,
    clips_ptr: u64,
}

const SET_MASTER: libc::c_ulong = io(0x1e);
const DROP_MASTER: libc::c_ulong = io(0x1f);
const GETRESOURCES: libc::c_ulong = iowr(0xA0, std::mem::size_of::<CardRes>());
const SETCRTC: libc::c_ulong = iowr(0xA2, std::mem::size_of::<Crtc>());
const GETENCODER: libc::c_ulong = iowr(0xA6, std::mem::size_of::<GetEncoder>());
const GETCONNECTOR: libc::c_ulong = iowr(0xA7, std::mem::size_of::<GetConnector>());
const ADDFB: libc::c_ulong = iowr(0xAE, std::mem::size_of::<FbCmd>());
const DIRTYFB: libc::c_ulong = iowr(0xB1, std::mem::size_of::<FbDirty>());
const CREATE_DUMB: libc::c_ulong = iowr(0xB2, std::mem::size_of::<CreateDumb>());
const MAP_DUMB: libc::c_ulong = iowr(0xB3, std::mem::size_of::<MapDumb>());
const MODE_TYPE_PREFERRED: u32 = 1 << 3;

fn ioctl<T>(fd: i32, req: libc::c_ulong, arg: &mut T) -> std::io::Result<()> {
    loop {
        // SAFETY: `arg` is the kernel's struct for `req`, sized as its
        // number says; every pointer inside it names a live buffer.
        let rc = unsafe { libc::ioctl(fd, req as _, arg as *mut T) };
        if rc == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if !matches!(e.raw_os_error(), Some(libc::EINTR) | Some(libc::EAGAIN)) {
            return Err(e);
        }
    }
}

/// A display, set to its mode, with a buffer the screen draws into.
pub struct Display {
    fd: OwnedFd,
    pub width: u32,
    pub height: u32,
    pitch: u32,
    map: *mut u8,
    size: usize,
    fb_id: u32,
    crtc_id: u32,
    connector_id: u32,
    mode: ModeInfo,
    /// Which card, and the connector, for the console.
    pub said: String,
}

// The mapping is this process's own and used from one thread at a time.
unsafe impl Send for Display {}

impl Display {
    /// The first card under `/dev/dri` with a connected display, its buffer
    /// ready but not scanned out until [`Display::acquire`];
    /// `None` when there is none (headless: nothing drawn, nothing blocked).
    pub fn open(card: Option<&std::path::Path>) -> std::io::Result<Option<Display>> {
        let cards: Vec<std::path::PathBuf> = match card {
            Some(c) => vec![c.to_path_buf()],
            None => {
                let mut v: Vec<_> = std::fs::read_dir("/dev/dri")
                    .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("card"))).collect())
                    .unwrap_or_default();
                v.sort();
                v
            }
        };
        let mut last = None;
        for c in cards {
            match Self::open_card(&c) {
                Ok(Some(d)) => return Ok(Some(d)),
                Ok(None) => continue,
                Err(e) => last = Some(e),
            }
        }
        match last {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }

    fn open_card(path: &std::path::Path) -> std::io::Result<Option<Display>> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
        let fd: OwnedFd = f.into();
        let raw = fd.as_raw_fd();
        let _ = ioctl(raw, SET_MASTER, &mut 0u8);
        let mut res = CardRes::default();
        ioctl(raw, GETRESOURCES, &mut res)?;
        let mut crtcs = vec![0u32; res.count_crtcs as usize];
        let mut conns = vec![0u32; res.count_connectors as usize];
        let mut encs = vec![0u32; res.count_encoders as usize];
        let mut fbs = vec![0u32; res.count_fbs as usize];
        let mut res2 = CardRes {
            fb_id_ptr: fbs.as_mut_ptr() as u64,
            crtc_id_ptr: crtcs.as_mut_ptr() as u64,
            connector_id_ptr: conns.as_mut_ptr() as u64,
            encoder_id_ptr: encs.as_mut_ptr() as u64,
            count_fbs: res.count_fbs,
            count_crtcs: res.count_crtcs,
            count_connectors: res.count_connectors,
            count_encoders: res.count_encoders,
            ..Default::default()
        };
        ioctl(raw, GETRESOURCES, &mut res2)?;
        for &cid in conns.iter().take(res2.count_connectors as usize) {
            let mut c = GetConnector { connector_id: cid, ..Default::default() };
            ioctl(raw, GETCONNECTOR, &mut c)?;
            // 1 = connected; 3 (unknown) is what simpledrm says of the GOP.
            if !(c.connection == 1 || c.connection == 3) || c.count_modes == 0 {
                continue;
            }
            let mut modes = vec![ModeInfo::default(); c.count_modes as usize];
            let mut cencs = vec![0u32; c.count_encoders as usize];
            let mut c2 = GetConnector {
                connector_id: cid,
                modes_ptr: modes.as_mut_ptr() as u64,
                count_modes: c.count_modes,
                encoders_ptr: cencs.as_mut_ptr() as u64,
                count_encoders: c.count_encoders,
                ..Default::default()
            };
            ioctl(raw, GETCONNECTOR, &mut c2)?;
            modes.truncate(c2.count_modes.min(c.count_modes) as usize);
            let Some(mode) = modes.iter().find(|m| m.type_ & MODE_TYPE_PREFERRED != 0).or(modes.first()).copied() else { continue };
            // The CRTC: the one the connector's encoder drives, else the
            // first any of its encoders can.
            let mut crtc_id = 0;
            if c2.encoder_id != 0 {
                let mut e = GetEncoder { encoder_id: c2.encoder_id, ..Default::default() };
                if ioctl(raw, GETENCODER, &mut e).is_ok() {
                    crtc_id = e.crtc_id;
                }
            }
            if crtc_id == 0 {
                'enc: for &eid in cencs.iter().take(c2.count_encoders as usize) {
                    let mut e = GetEncoder { encoder_id: eid, ..Default::default() };
                    if ioctl(raw, GETENCODER, &mut e).is_err() {
                        continue;
                    }
                    for (i, &cr) in crtcs.iter().enumerate() {
                        if e.possible_crtcs & (1 << i) != 0 {
                            crtc_id = cr;
                            break 'enc;
                        }
                    }
                }
            }
            if crtc_id == 0 {
                continue;
            }
            let (w, h) = (mode.hdisplay as u32, mode.vdisplay as u32);
            let mut cd = CreateDumb { width: w, height: h, bpp: 32, ..Default::default() };
            ioctl(raw, CREATE_DUMB, &mut cd)?;
            let mut fb = FbCmd { width: w, height: h, pitch: cd.pitch, bpp: 32, depth: 24, handle: cd.handle, ..Default::default() };
            ioctl(raw, ADDFB, &mut fb)?;
            let mut md = MapDumb { handle: cd.handle, ..Default::default() };
            ioctl(raw, MAP_DUMB, &mut md)?;
            // SAFETY: the offset is the kernel's for this buffer, mapped for
            // its whole size, shared, read and write.
            let map = unsafe {
                libc::mmap(std::ptr::null_mut(), cd.size as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, raw, md.offset as libc::off_t)
            };
            if map == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
            let name = String::from_utf8_lossy(&mode.name).trim_end_matches('\0').to_string();
            let d = Display {
                fd,
                width: w,
                height: h,
                pitch: cd.pitch,
                map: map as *mut u8,
                size: cd.size as usize,
                fb_id: fb.fb_id,
                crtc_id,
                connector_id: cid,
                mode,
                said: format!("{} connector {cid} crtc {crtc_id} {name} ({w}x{h})", path.display()),
            };
            // Not scanned out yet (#637): the caller draws its first frame
            // into the buffer, then `acquire`s, so the display goes from the
            // firmware's last picture to the screen's first with no empty
            // buffer between.
            return Ok(Some(d));
        }
        Ok(None)
    }

    /// Scan this buffer out (again): at the start, and after the log was
    /// shown on another VT.
    pub fn show(&self) -> std::io::Result<()> {
        let raw = self.fd.as_raw_fd();
        let mut conn = self.connector_id;
        let mut c = Crtc {
            set_connectors_ptr: &mut conn as *mut u32 as u64,
            count_connectors: 1,
            crtc_id: self.crtc_id,
            fb_id: self.fb_id,
            mode_valid: 1,
            mode: self.mode,
            ..Default::default()
        };
        ioctl(raw, SETCRTC, &mut c)
    }

    /// Take the display back (F1): master, then the mode.
    pub fn acquire(&self) -> std::io::Result<()> {
        ioctl(self.fd.as_raw_fd(), SET_MASTER, &mut 0u8)?;
        self.show()
    }

    /// Let it go (F2, or the end), so the console's own framebuffer returns.
    pub fn release(&self) {
        let _ = ioctl(self.fd.as_raw_fd(), DROP_MASTER, &mut 0u8);
    }

    /// Copy XRGB8888 pixels in (rows of `width`), then tell the driver.
    pub fn present(&self, px: &[u32]) {
        let (w, h) = (self.width as usize, self.height as usize);
        for y in 0..h {
            let off = y * self.pitch as usize;
            if off + w * 4 > self.size {
                break;
            }
            // SAFETY: inside the mapping, which is at least pitch × height.
            let row = unsafe { std::slice::from_raw_parts_mut(self.map.add(off) as *mut u32, w) };
            row.copy_from_slice(&px[y * w..y * w + w]);
        }
        let mut d = FbDirty { fb_id: self.fb_id, ..Default::default() };
        // ENOSYS: a driver that scans the buffer out directly needs none.
        let _ = ioctl(self.fd.as_raw_fd(), DIRTYFB, &mut d);
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `open_card`, of this size.
        unsafe { libc::munmap(self.map as *mut libc::c_void, self.size) };
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The structs are the kernel's sizes (`<drm/drm_mode.h>`), so the
    /// ioctl numbers are too.
    #[test]
    fn the_kernels_sizes() {
        assert_eq!(std::mem::size_of::<CardRes>(), 64);
        assert_eq!(std::mem::size_of::<ModeInfo>(), 68);
        assert_eq!(std::mem::size_of::<GetConnector>(), 80);
        assert_eq!(std::mem::size_of::<GetEncoder>(), 20);
        assert_eq!(std::mem::size_of::<Crtc>(), 104);
        assert_eq!(std::mem::size_of::<CreateDumb>(), 32);
        assert_eq!(std::mem::size_of::<MapDumb>(), 16);
        assert_eq!(std::mem::size_of::<FbCmd>(), 28);
        assert_eq!(std::mem::size_of::<FbDirty>(), 24);
        // DRM_IOCTL_MODE_GETRESOURCES is 0xC04064A0 on every Linux.
        assert_eq!(GETRESOURCES, 0xC040_64A0);
        assert_eq!(CREATE_DUMB, 0xC020_64B2);
        assert_eq!(SET_MASTER, 0x641E);
    }
}
