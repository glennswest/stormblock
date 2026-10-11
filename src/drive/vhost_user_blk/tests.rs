//! A minimal vhost-user frontend — what a VMM does — driving the backend
//! over its socket: memfd guest memory, a split ring, kick and call
//! eventfds. Real protocol, real fds, a file-backed device behind it.

use super::*;
use crate::drive::filedev::FileDevice;
use std::os::unix::net::UnixStream;

const RING: u16 = 16;
const MEM: usize = 4 << 20;
const DESC: u64 = 0x0;
const AVAIL: u64 = 0x1000;
const USED: u64 = 0x2000;
const BUFS: u64 = 0x10000;

struct Vmm {
    sock: UnixStream,
    mem: *mut u8,
    memfd: OwnedFd,
    kick: OwnedFd,
    call: OwnedFd,
    avail_idx: u16,
    next_buf: u64,
}

fn eventfd() -> OwnedFd {
    // SAFETY: a new eventfd, owned by the result.
    unsafe { OwnedFd::from_raw_fd(libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC)) }
}

fn send(sock: &UnixStream, req: u32, flags: u32, body: &[u8], fds: &[RawFd]) {
    let mut m = Vec::new();
    m.extend_from_slice(&req.to_le_bytes());
    m.extend_from_slice(&(FLAG_VERSION | flags).to_le_bytes());
    m.extend_from_slice(&(body.len() as u32).to_le_bytes());
    m.extend_from_slice(body);
    let mut iov = libc::iovec { iov_base: m.as_mut_ptr() as *mut libc::c_void, iov_len: m.len() };
    let mut cmsg = [0u8; 256];
    // SAFETY: a msghdr over buffers that outlive the call; SCM_RIGHTS built
    // with the kernel's macros.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            let space = libc::CMSG_SPACE((fds.len() * 4) as u32) as usize;
            msg.msg_control = cmsg.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = space as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((fds.len() * 4) as u32) as _;
            std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c) as *mut RawFd, fds.len());
        }
        assert_eq!(libc::sendmsg(sock.as_raw_fd(), &msg, 0), m.len() as isize);
    }
}

fn reply(sock: &UnixStream) -> Vec<u8> {
    let mut s = sock;
    let mut hdr = [0u8; 12];
    s.read_exact(&mut hdr).unwrap();
    assert_ne!(u32::from_le_bytes(hdr[4..8].try_into().unwrap()) & FLAG_REPLY, 0);
    let mut body = vec![0u8; u32::from_le_bytes(hdr[8..12].try_into().unwrap()) as usize];
    s.read_exact(&mut body).unwrap();
    body
}

fn le64(v: u64) -> Vec<u8> {
    v.to_le_bytes().to_vec()
}

fn two(a: u32, b: u32) -> Vec<u8> {
    let mut v = a.to_le_bytes().to_vec();
    v.extend_from_slice(&b.to_le_bytes());
    v
}

impl Vmm {
    /// Connect and negotiate the way QEMU's vhost-user-blk does, then set up
    /// queue 0.
    fn connect(path: &Path) -> (Vmm, u64, Vec<u8>) {
        let sock = UnixStream::connect(path).unwrap();
        sock.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
        send(&sock, GET_FEATURES, 0, &[], &[]);
        let features = u64::from_le_bytes(reply(&sock).try_into().unwrap());
        send(&sock, SET_FEATURES, 0, &le64(features), &[]);
        send(&sock, GET_PROTOCOL_FEATURES, 0, &[], &[]);
        let proto = u64::from_le_bytes(reply(&sock).try_into().unwrap());
        assert_ne!(proto & PROTOCOL_F_CONFIG, 0, "CONFIG: QEMU's vhost-user-blk needs it");
        send(&sock, SET_PROTOCOL_FEATURES, 0, &le64(proto), &[]);
        send(&sock, SET_OWNER, FLAG_NEED_REPLY, &[], &[]);
        assert_eq!(reply(&sock), le64(0), "REPLY_ACK");
        let mut cfg_req = two(0, 60);
        cfg_req.extend_from_slice(&0u32.to_le_bytes());
        cfg_req.extend_from_slice(&[0u8; 60]);
        send(&sock, GET_CONFIG, 0, &cfg_req, &[]);
        let config = reply(&sock)[12..].to_vec();

        // SAFETY: a new memfd sized and mapped shared here.
        let (memfd, mem) = unsafe {
            let fd = libc::memfd_create(b"guest\0".as_ptr() as *const libc::c_char, libc::MFD_CLOEXEC);
            assert!(fd >= 0);
            assert_eq!(libc::ftruncate(fd, MEM as i64), 0);
            let p = libc::mmap(std::ptr::null_mut(), MEM, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            assert_ne!(p, libc::MAP_FAILED);
            (OwnedFd::from_raw_fd(fd), p as *mut u8)
        };
        // One region: guest-physical 0 and the VMM's address = the offset
        // (a VMM's own addresses are just numbers to the backend).
        let uva = 0x7f00_0000_0000u64;
        let mut table = two(1, 0);
        for v in [0u64, MEM as u64, uva, 0] {
            table.extend_from_slice(&v.to_le_bytes());
        }
        send(&sock, SET_MEM_TABLE, FLAG_NEED_REPLY, &table, &[memfd.as_raw_fd()]);
        assert_eq!(reply(&sock), le64(0));
        send(&sock, SET_VRING_NUM, 0, &two(0, RING as u32), &[]);
        send(&sock, SET_VRING_BASE, 0, &two(0, 0), &[]);
        let mut addr = two(0, 0);
        for v in [uva + DESC, uva + USED, uva + AVAIL, 0] {
            addr.extend_from_slice(&v.to_le_bytes());
        }
        send(&sock, SET_VRING_ADDR, 0, &addr, &[]);
        let (kick, call) = (eventfd(), eventfd());
        send(&sock, SET_VRING_CALL, 0, &le64(0), &[call.as_raw_fd()]);
        send(&sock, SET_VRING_KICK, 0, &le64(0), &[kick.as_raw_fd()]);
        send(&sock, SET_VRING_ENABLE, FLAG_NEED_REPLY, &two(0, 1), &[]);
        assert_eq!(reply(&sock), le64(0));
        (Vmm { sock, mem, memfd, kick, call, avail_idx: 0, next_buf: BUFS }, features, config)
    }

    fn at(&self, gpa: u64) -> *mut u8 {
        // SAFETY: inside the guest memory this test mapped.
        unsafe { self.mem.add(gpa as usize) }
    }

    fn buf(&mut self, data: &[u8]) -> u64 {
        let g = self.next_buf;
        // SAFETY: inside guest memory.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.at(g), data.len()) };
        self.next_buf += (data.len() as u64).div_ceil(64) * 64;
        g
    }

    fn set_desc(&self, i: u16, addr: u64, len: u32, flags: u16, next: u16) {
        // SAFETY: inside the descriptor table.
        unsafe {
            let p = self.at(DESC + 16 * i as u64);
            std::ptr::write(p as *mut u64, addr);
            std::ptr::write(p.add(8) as *mut u32, len);
            std::ptr::write(p.add(12) as *mut u16, flags);
            std::ptr::write(p.add(14) as *mut u16, next);
        }
    }

    /// Submit one request (header, then `segs` as (gpa, len, writable), then
    /// a status byte); wait for its completion; the status and used length.
    fn submit(&mut self, kind: u32, sector: u64, segs: &[(u64, u32, bool)]) -> (u8, u32) {
        self.next_buf = BUFS;
        let mut h = kind.to_le_bytes().to_vec();
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(&sector.to_le_bytes());
        let hg = self.buf(&h);
        let sg = self.buf(&[0xEE]);
        let mut all = vec![(hg, 16u32, false)];
        all.extend_from_slice(segs);
        all.push((sg, 1, true));
        for (i, &(g, l, w)) in all.iter().enumerate() {
            let last = i + 1 == all.len();
            let flags = if w { DESC_F_WRITE } else { 0 } | if last { 0 } else { DESC_F_NEXT };
            self.set_desc(i as u16, g, l, flags, i as u16 + 1);
        }
        // SAFETY: the avail ring: head into the slot, then publish the index.
        unsafe {
            let slot = (self.avail_idx % RING) as u64;
            std::ptr::write_volatile(self.at(AVAIL + 4 + 2 * slot) as *mut u16, 0);
            fence(Ordering::Release);
            self.avail_idx = self.avail_idx.wrapping_add(1);
            std::ptr::write_volatile(self.at(AVAIL + 2) as *mut u16, self.avail_idx);
        }
        signal(self.kick.as_raw_fd());
        // Wait for the call, then for the used index.
        let mut pfd = libc::pollfd { fd: self.call.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: polling one fd for up to 10 s.
        assert_eq!(unsafe { libc::poll(&mut pfd, 1, 10_000) }, 1, "no completion signalled");
        let mut v = [0u8; 8];
        // SAFETY: draining the call eventfd.
        unsafe { libc::read(self.call.as_raw_fd(), v.as_mut_ptr() as *mut libc::c_void, 8) };
        // SAFETY: reading the used ring.
        unsafe {
            assert_eq!(std::ptr::read_volatile(self.at(USED + 2) as *const u16), self.avail_idx);
            let slot = ((self.avail_idx.wrapping_sub(1)) % RING) as u64;
            let len = std::ptr::read_volatile(self.at(USED + 4 + 8 * slot + 4) as *const u32);
            (std::ptr::read_volatile(self.at(sg)), len)
        }
    }

    fn read_guest(&self, gpa: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        // SAFETY: inside guest memory.
        unsafe { std::ptr::copy_nonoverlapping(self.at(gpa), v.as_mut_ptr(), len) };
        v
    }
}

impl Drop for Vmm {
    fn drop(&mut self) {
        // SAFETY: unmapping what connect mapped.
        unsafe { libc::munmap(self.mem as *mut libc::c_void, MEM) };
        let _ = &self.memfd;
    }
}

async fn device(name: &str, size: u64) -> (Arc<dyn BlockDevice>, PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("stormblock-vhost-{name}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("disk.img");
    let dev = FileDevice::open_with_capacity(img.to_str().unwrap(), size).await.unwrap();
    (Arc::new(dev), dir.join("blk.sock"), dir)
}

/// The whole path: negotiate as QEMU does, write through the ring, read it
/// back through the ring and from the device, flush, GET_ID, discard and
/// write-zeroes, a bad address failed without harm, GET_VRING_BASE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guest_writes_and_reads_through_the_virtqueue() {
    let (dev, sock, dir) = device("rw", 8 << 20).await;
    let blk = VhostUserBlk::serve(&sock, dev.clone(), BlkSpec { read_only: false, serial: "vol-1234".into() }, tokio::runtime::Handle::current()).unwrap();

    let dev2 = dev.clone();
    tokio::task::spawn_blocking(move || {
        let (mut vmm, features, config) = Vmm::connect(&sock);
        assert_ne!(features & BLK_F_FLUSH, 0);
        assert_eq!(features & BLK_F_RO, 0, "read-write");
        assert_eq!(u64::from_le_bytes(config[0..8].try_into().unwrap()), (8 << 20) / 512, "capacity in sectors");

        // A write of 8 KiB at sector 16, split over two segments.
        let data: Vec<u8> = (0..8192u32).map(|i| (i * 13 % 251) as u8).collect();
        vmm.next_buf = BUFS + 0x1000;
        let a = BUFS + 0x4000;
        let b = BUFS + 0x6000;
        // SAFETY: placing the payload in guest memory.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), vmm.at(a), 4096);
            std::ptr::copy_nonoverlapping(data.as_ptr().add(4096), vmm.at(b), 4096);
        }
        assert_eq!(vmm.submit(T_OUT, 16, &[(a, 4096, false), (b, 4096, false)]).0, S_OK);

        // Read back through the ring into one buffer.
        let r = BUFS + 0x10000;
        let (st, len) = vmm.submit(T_IN, 16, &[(r, 8192, true)]);
        assert_eq!((st, len), (S_OK, 8193), "8 KiB and the status byte");
        assert!(vmm.read_guest(r, 8192) == data);

        assert_eq!(vmm.submit(T_FLUSH, 0, &[]).0, S_OK);
        let (st, _) = vmm.submit(T_GET_ID, 0, &[(r, 20, true)]);
        assert_eq!(st, S_OK);
        assert_eq!(&vmm.read_guest(r, 8), b"vol-1234");

        // Write-zeroes over the first 4 KiB of what was written.
        let mut seg = 16u64.to_le_bytes().to_vec();
        seg.extend_from_slice(&8u32.to_le_bytes());
        seg.extend_from_slice(&0u32.to_le_bytes());
        let sg = BUFS + 0x20000;
        // SAFETY: placing the segment in guest memory.
        unsafe { std::ptr::copy_nonoverlapping(seg.as_ptr(), vmm.at(sg), 16) };
        assert_eq!(vmm.submit(T_WRITE_ZEROES, 0, &[(sg, 16, false)]).0, S_OK);
        assert_eq!(vmm.submit(T_DISCARD, 0, &[(sg, 16, false)]).0, S_OK);

        // A buffer outside guest memory: that request fails, nothing else.
        assert_eq!(vmm.submit(T_IN, 0, &[(MEM as u64 + 4096, 512, true)]).0, 0xEE, "never written: the chain is refused");
        // Past the end of the disk.
        assert_eq!(vmm.submit(T_IN, (8 << 20) / 512, &[(r, 512, true)]).0, S_IOERR);
        assert_eq!(vmm.submit(99, 0, &[]).0, S_UNSUPP);

        // GET_VRING_BASE stops the ring and says where to resume.
        send(&vmm.sock, GET_VRING_BASE, 0, &two(0, 0), &[]);
        assert_eq!(reply(&vmm.sock), two(0, vmm.avail_idx as u32));
        (data, dev2)
    })
    .await
    .map(|(data, dev)| async move {
        let mut got = vec![0u8; 8192];
        dev.read(16 * 512, &mut got).await.unwrap();
        assert!(got[..4096].iter().all(|&b| b == 0), "write-zeroes reached the device");
        assert!(got[4096..] == data[4096..], "the write reached the device");
    })
    .unwrap()
    .await;
    assert!(blk.counters.requests.load(Ordering::Relaxed) >= 9);
    blk.stop(std::time::Duration::from_secs(5));
    assert!(!blk.path().exists(), "the socket goes with the device");
    let _ = std::fs::remove_dir_all(dir);
}

/// Read-only at the device: RO offered, a write fails whatever the guest
/// does, reads work. A VMM that goes away and comes back (QEMU
/// `reconnect=`) negotiates afresh and is served; a second VMM while one is
/// connected is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_is_enforced_and_a_vmm_can_reconnect_but_not_double_up() {
    let (dev, sock, dir) = device("ro", 4 << 20).await;
    dev.write(0, &[0x5A; 4096]).await.unwrap();
    let blk = VhostUserBlk::serve(&sock, dev, BlkSpec { read_only: true, serial: "ro".into() }, tokio::runtime::Handle::current()).unwrap();
    let path = sock.clone();
    tokio::task::spawn_blocking(move || {
        let (mut vmm, features, _) = Vmm::connect(&path);
        assert_ne!(features & BLK_F_RO, 0);
        let w = BUFS + 0x4000;
        assert_eq!(vmm.submit(T_OUT, 0, &[(w, 4096, false)]).0, S_IOERR);
        let mut seg = 0u64.to_le_bytes().to_vec();
        seg.extend_from_slice(&8u32.to_le_bytes());
        seg.extend_from_slice(&0u32.to_le_bytes());
        // SAFETY: placing the segment in guest memory.
        unsafe { std::ptr::copy_nonoverlapping(seg.as_ptr(), vmm.at(w), 16) };
        assert_eq!(vmm.submit(T_DISCARD, 0, &[(w, 16, false)]).0, S_IOERR);
        let r = BUFS + 0x8000;
        assert_eq!(vmm.submit(T_IN, 0, &[(r, 4096, true)]).0, S_OK);
        assert!(vmm.read_guest(r, 4096).iter().all(|&b| b == 0x5A), "untouched by the refused writes");

        // A second VMM is turned away while this one holds the disk.
        let extra = UnixStream::connect(&path).unwrap();
        extra.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut b = [0u8; 1];
        assert_eq!((&extra).read(&mut b).unwrap_or(0), 0, "closed by the backend");

        drop(vmm);
        // The backend notices, resets, and serves the next connection.
        let (mut again, _, _) = Vmm::connect(&path);
        assert_eq!(again.submit(T_IN, 0, &[(r, 4096, true)]).0, S_OK);
    })
    .await
    .unwrap();
    assert_eq!(blk.counters.connections.load(Ordering::Relaxed), 2);
    blk.stop(std::time::Duration::from_secs(5));
    let _ = std::fs::remove_dir_all(dir);
}
