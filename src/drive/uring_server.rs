//! Shared ring IPC server — accepts StormFS clients via Unix socket, serves
//! block I/O through shared-memory ring buffers.
//!
//! Each client gets its own memfd + eventfd pair. A worker thread per client
//! polls the submission queue and dispatches reads/writes to the underlying
//! `BlockDevice`.

use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;
#[allow(unused_imports)]
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Wrapper around a raw pointer to make it `Send`.
/// Safety: the mmap'd shared memory is valid for the lifetime of the worker
/// thread and is not accessed from other client workers.
#[cfg(target_os = "linux")]
struct SendShmPtr(*mut u8);
#[cfg(target_os = "linux")]
unsafe impl Send for SendShmPtr {}

#[cfg(target_os = "linux")]
use super::uring_channel::*;
use super::BlockDevice;

/// Server listening on a Unix socket, dispatching I/O for connected clients.
#[allow(dead_code)]
pub struct UringServer {
    socket_path: String,
    volumes: Arc<Mutex<HashMap<String, Arc<dyn BlockDevice>>>>,
    running: Arc<AtomicBool>,
}

impl UringServer {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            volumes: Arc::new(Mutex::new(HashMap::new())),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Register a named volume for client access.
    pub fn add_volume(&self, name: String, device: Arc<dyn BlockDevice>) {
        self.volumes.lock().unwrap().insert(name, device);
    }

    /// Run the server until `running` is set to false.
    ///
    /// This is Linux-only; on other platforms it returns an error immediately.
    #[cfg(target_os = "linux")]
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use std::os::unix::io::AsRawFd;

        // Remove stale socket
        let _ = std::fs::remove_file(&self.socket_path);

        let listener = std::os::unix::net::UnixListener::bind(&self.socket_path)?;
        listener.set_nonblocking(true)?;
        self.running.store(true, Ordering::SeqCst);

        tracing::info!("UringServer listening on {}", self.socket_path);

        let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();

        while self.running.load(Ordering::Relaxed) {
            // Non-blocking accept with a short sleep to check running flag
            match listener.accept() {
                Ok((stream, _addr)) => {
                    tracing::info!("UringServer: new client connection");
                    match self.handle_client(stream) {
                        Ok(handle) => workers.push(handle),
                        Err(e) => tracing::error!("UringServer: client setup failed: {e}"),
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
                Err(e) => {
                    tracing::error!("UringServer: accept error: {e}");
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
            }
        }

        tracing::info!("UringServer shutting down, waiting for {} workers", workers.len());
        for w in workers {
            let _ = w.join();
        }

        let _ = std::fs::remove_file(&self.socket_path);
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub async fn run(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Err("UringServer is Linux-only".into())
    }

    /// Stop the server (can be called from any thread).
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Handle a single client connection: read volume name, set up shared memory,
    /// send fds, spawn worker thread.
    #[cfg(target_os = "linux")]
    fn handle_client(
        &self,
        stream: std::os::unix::net::UnixStream,
    ) -> Result<std::thread::JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>> {
        use std::io::Read;

        // Set blocking for handshake
        stream.set_nonblocking(false)?;
        let mut stream_clone = stream.try_clone()?;

        // Read volume request (JSON: {"volume": "name"})
        let mut buf = [0u8; 4096];
        let n = stream_clone.read(&mut buf)?;
        let request: serde_json::Value = serde_json::from_slice(&buf[..n])?;
        let vol_name = request
            .get("volume")
            .and_then(|v| v.as_str())
            .ok_or("missing 'volume' field")?
            .to_string();

        tracing::info!("UringServer: client requested volume '{}'", vol_name);

        let device = {
            let vols = self.volumes.lock().unwrap();
            vols.get(&vol_name)
                .cloned()
                .ok_or_else(|| format!("volume '{}' not found", vol_name))?
        };

        // Create shared memory
        let qd = DEFAULT_QUEUE_DEPTH;
        let bs = DEFAULT_BUF_SIZE;
        let (memfd, shm_ptr, shm_size) = shm::create_shm(qd, bs)?;

        // Initialize ring header
        unsafe {
            ring_header_init(
                shm_ptr as *mut RingHeader,
                qd,
                bs,
                device.capacity_bytes(),
                device.block_size(),
            );
        }

        // Create eventfds
        let submit_efd = shm::create_eventfd()?;
        let complete_efd = shm::create_eventfd()?;

        // Send fds to client via SCM_RIGHTS
        send_fds(stream_clone.as_raw_fd(), &[memfd, submit_efd, complete_efd])?;

        tracing::info!(
            "UringServer: sent fds to client (memfd={}, submit_efd={}, complete_efd={}, shm_size={})",
            memfd, submit_efd, complete_efd, shm_size
        );

        // Spawn worker thread — wrap raw pointer in Send newtype before the
        // move closure so the compiler doesn't capture the non-Send *mut u8.
        let running = self.running.clone();
        let rt = tokio::runtime::Handle::current();
        let shm_send = SendShmPtr(shm_ptr);
        #[allow(unused_variables)]
        let shm_ptr = ();  // shadow the raw pointer so it can't be captured

        let handle = std::thread::Builder::new()
            .name(format!("uring-{}", vol_name))
            .spawn(move || {
                let _ = shm_ptr; // capture the () shadow, not the raw pointer
                let ptr = shm_send.0;
                client_worker(
                    ptr, shm_size, submit_efd, complete_efd,
                    device, running, rt,
                );
                // Cleanup: close our copies of the fds; client has its own via SCM_RIGHTS
                unsafe {
                    libc::close(submit_efd);
                    libc::close(complete_efd);
                    libc::close(memfd);
                    shm::unmap_shm(ptr, shm_size);
                }
            })?;

        Ok(handle)
    }

}

/// Send file descriptors over a Unix socket using SCM_RIGHTS.
#[cfg(target_os = "linux")]
fn send_fds(sock_fd: RawFd, fds: &[RawFd]) -> std::io::Result<()> {
    use std::mem;

    let iov_data: [u8; 1] = [0];
    let iov = libc::iovec {
        iov_base: iov_data.as_ptr() as *mut libc::c_void,
        iov_len: 1,
    };

    // Build cmsg buffer
    let fd_bytes = fds.len() * mem::size_of::<RawFd>();
    let cmsg_len = unsafe { libc::CMSG_SPACE(fd_bytes as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_len];

    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &iov as *const libc::iovec as *mut libc::iovec;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_len;

    let cmsg: *mut libc::cmsghdr = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    unsafe {
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as usize;
        let data_ptr = libc::CMSG_DATA(cmsg) as *mut RawFd;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), data_ptr, fds.len());
    }

    let ret = unsafe { libc::sendmsg(sock_fd, &msg, 0) };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Receive file descriptors over a Unix socket using SCM_RIGHTS.
#[cfg(target_os = "linux")]
pub fn recv_fds(sock_fd: RawFd, max_fds: usize) -> std::io::Result<Vec<RawFd>> {
    use std::mem;

    let mut iov_data: [u8; 1] = [0];
    let mut iov = libc::iovec {
        iov_base: iov_data.as_mut_ptr() as *mut libc::c_void,
        iov_len: 1,
    };

    let fd_bytes = max_fds * mem::size_of::<RawFd>();
    let cmsg_len = unsafe { libc::CMSG_SPACE(fd_bytes as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_len];

    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = cmsg_len;

    let ret = unsafe { libc::recvmsg(sock_fd, &mut msg, 0) };
    if ret < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut fds = Vec::new();
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg.is_null() {
        unsafe {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                let payload_len = (*cmsg).cmsg_len - libc::CMSG_LEN(0) as usize;
                let num_fds = payload_len / mem::size_of::<RawFd>();
                for i in 0..num_fds {
                    fds.push(*data_ptr.add(i));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }

    Ok(fds)
}

#[cfg(not(target_os = "linux"))]
pub fn recv_fds(_sock_fd: RawFd, _max_fds: usize) -> std::io::Result<Vec<RawFd>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "SCM_RIGHTS is Linux-only in this implementation",
    ))
}

// ---------------------------------------------------------------------------
// Per-client worker
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn client_worker(
    shm_base: *mut u8,
    _shm_size: usize,
    submit_efd: RawFd,
    complete_efd: RawFd,
    device: Arc<dyn BlockDevice>,
    running: Arc<AtomicBool>,
    rt: tokio::runtime::Handle,
) {
    let header = shm_base as *const RingHeader;
    let sq_base = unsafe {
        shm_base.add((*header).sq_offset as usize) as *const RingCommand
    };
    let cq_base = unsafe {
        shm_base.add((*header).cq_offset as usize) as *mut RingCompletion
    };

    tracing::info!("UringServer worker started");

    while running.load(Ordering::Relaxed) {
        // Wait for submit notification (1 second timeout to recheck running flag)
        match shm::eventfd_wait(submit_efd, 1000) {
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                tracing::error!("UringServer worker: eventfd_wait error: {e}");
                break;
            }
        }

        // Drain submission queue
        let mut completed = 0u32;
        loop {
            let cmd = unsafe { sq_pop(header, sq_base) };
            let cmd = match cmd {
                Some(c) => c,
                None => break,
            };

            let buf_ptr = unsafe { data_buf_ptr(shm_base as *mut u8, header, cmd.buf_idx) };

            let (status, result) = match cmd.op {
                OP_READ => {
                    let buf = unsafe {
                        std::slice::from_raw_parts_mut(buf_ptr, cmd.length as usize)
                    };
                    match rt.block_on(device.read(cmd.offset, buf)) {
                        Ok(n) => (0i16, n as u32),
                        Err(e) => {
                            tracing::error!("ring read @{}+{}: {e}", cmd.offset, cmd.length);
                            (-(libc::EIO as i16), 0)
                        }
                    }
                }
                OP_WRITE => {
                    let buf = unsafe {
                        std::slice::from_raw_parts(buf_ptr, cmd.length as usize)
                    };
                    match rt.block_on(device.write(cmd.offset, buf)) {
                        Ok(n) => (0i16, n as u32),
                        Err(e) => {
                            tracing::error!("ring write @{}+{}: {e}", cmd.offset, cmd.length);
                            (-(libc::EIO as i16), 0)
                        }
                    }
                }
                OP_FLUSH => {
                    match rt.block_on(device.flush()) {
                        Ok(()) => (0i16, 0),
                        Err(e) => {
                            tracing::error!("ring flush: {e}");
                            (-(libc::EIO as i16), 0)
                        }
                    }
                }
                OP_DISCARD => {
                    match rt.block_on(device.discard(cmd.offset, cmd.length as u64)) {
                        Ok(()) => (0i16, 0),
                        Err(e) => {
                            tracing::error!("ring discard @{}+{}: {e}", cmd.offset, cmd.length);
                            (-(libc::EIO as i16), 0)
                        }
                    }
                }
                _ => {
                    tracing::warn!("ring: unknown op {}", cmd.op);
                    (-(libc::ENOTSUP as i16), 0)
                }
            };

            let comp = RingCompletion {
                tag: cmd.tag,
                status,
                result,
                _pad: [0; 24],
            };
            unsafe {
                if !cq_push(header, cq_base, &comp) {
                    tracing::error!("ring: CQ full, dropping completion for tag {}", cmd.tag);
                }
            }
            completed += 1;
        }

        // Signal completions available
        if completed > 0 {
            let _ = shm::eventfd_write(complete_efd, completed as u64);
        }
    }

    tracing::info!("UringServer worker exiting");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_add_volume() {
        let server = UringServer::new("/tmp/test-uring.sock");

        // Use a FileDevice for testing
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let dev: Arc<dyn BlockDevice> = rt.block_on(async {
            let dir = std::env::temp_dir();
            let path = dir.join("uring-server-test.bin");
            let ps = path.to_str().unwrap().to_string();
            let dev = super::super::filedev::FileDevice::open_with_capacity(&ps, 1024 * 1024)
                .await
                .unwrap();
            let _ = std::fs::remove_file(&path);
            Arc::new(dev) as Arc<dyn BlockDevice>
        });

        server.add_volume("test-vol".into(), dev);
        assert!(server.volumes.lock().unwrap().contains_key("test-vol"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn end_to_end_ring_io() {
        use std::os::unix::net::UnixStream;
        use std::io::Write;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();

        let sock_path = format!(
            "/tmp/uring-test-{}.sock",
            std::process::id()
        );

        // Create a backing file device
        let dev: Arc<dyn BlockDevice> = rt.block_on(async {
            let path = std::env::temp_dir().join(format!("uring-e2e-{}.bin", std::process::id()));
            let ps = path.to_str().unwrap().to_string();
            let dev = super::super::filedev::FileDevice::open_with_capacity(&ps, 4 * 1024 * 1024)
                .await
                .unwrap();
            Arc::new(dev) as Arc<dyn BlockDevice>
        });

        let server = Arc::new(UringServer::new(&sock_path));
        server.add_volume("test".into(), dev);

        // Start server in background
        let server_clone = server.clone();
        let sock_clone = sock_path.clone();
        let server_handle = rt.spawn(async move {
            server_clone.run().await.unwrap();
        });

        // Give server time to bind
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Connect as client
        let stream = UnixStream::connect(&sock_path).unwrap();
        stream.set_nonblocking(false).unwrap();
        let mut stream_w = stream.try_clone().unwrap();

        // Send volume request
        let req = br#"{"volume":"test"}"#;
        stream_w.write_all(req).unwrap();

        // Receive fds
        let fds = recv_fds(stream.as_raw_fd(), 3).unwrap();
        assert_eq!(fds.len(), 3);
        let memfd = fds[0];
        let submit_efd = fds[1];
        let complete_efd = fds[2];

        // Attach shared memory
        let (shm_ptr, _shm_size) = shm::attach_shm(memfd).unwrap();
        let header = shm_ptr as *const RingHeader;
        unsafe {
            assert_eq!((*header).magic, RING_MAGIC);
            assert!((*header).capacity > 0);
        }

        let sq_base = unsafe {
            shm_ptr.add((*header).sq_offset as usize) as *mut RingCommand
        };
        let cq_base = unsafe {
            shm_ptr.add((*header).cq_offset as usize) as *const RingCompletion
        };

        // Write test data into buffer 0
        let test_data = b"Hello from ring IPC!";
        unsafe {
            let buf = data_buf_ptr(shm_ptr, header, 0);
            std::ptr::copy_nonoverlapping(test_data.as_ptr(), buf, test_data.len());
        }

        // Submit write command
        let write_cmd = RingCommand {
            tag: 1,
            op: OP_WRITE,
            flags: 0,
            buf_idx: 0,
            _pad: 0,
            offset: 0,
            length: test_data.len() as u32,
            _pad2: [0; 36],
        };
        unsafe { assert!(sq_push(header, sq_base, &write_cmd)); }
        shm::eventfd_write(submit_efd, 1).unwrap();

        // Wait for completion
        shm::eventfd_wait(complete_efd, 5000).unwrap();
        let comp = unsafe { cq_pop(header, cq_base) }.unwrap();
        assert_eq!(comp.tag, 1);
        assert_eq!(comp.status, 0);

        // Submit read command
        let read_cmd = RingCommand {
            tag: 2,
            op: OP_READ,
            flags: 0,
            buf_idx: 1,
            _pad: 0,
            offset: 0,
            length: test_data.len() as u32,
            _pad2: [0; 36],
        };
        unsafe { assert!(sq_push(header, sq_base, &read_cmd)); }
        shm::eventfd_write(submit_efd, 1).unwrap();

        // Wait for read completion
        shm::eventfd_wait(complete_efd, 5000).unwrap();
        let comp = unsafe { cq_pop(header, cq_base) }.unwrap();
        assert_eq!(comp.tag, 2);
        assert_eq!(comp.status, 0);

        // Verify data
        unsafe {
            let buf = data_buf_ptr(shm_ptr, header, 1);
            let read_data = std::slice::from_raw_parts(buf, test_data.len());
            assert_eq!(read_data, test_data);
        }

        // Cleanup
        server.stop();
        rt.block_on(async { let _ = server_handle.await; });
        unsafe {
            shm::unmap_shm(shm_ptr, _shm_size);
            libc::close(memfd);
            libc::close(submit_efd);
            libc::close(complete_efd);
        }
        let _ = std::fs::remove_file(&sock_path);
        let _ = std::fs::remove_file(
            std::env::temp_dir().join(format!("uring-e2e-{}.bin", std::process::id()))
        );
    }
}
