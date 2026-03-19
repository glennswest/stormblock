//! NBD (Network Block Device) server — exports a BlockDevice to the local kernel.
//!
//! Implements the NBD protocol (newstyle fixed negotiation) over a TCP socket.
//! The kernel's nbd-client connects and gets /dev/nbdN.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{BlockDevice, DriveResult};

// NBD protocol constants
const NBD_MAGIC: u64 = 0x4e42444d41474943; // "NBDMAGIC"
const NBD_OPTS_MAGIC: u64 = 0x49484156454F5054; // "IHAVEOPT"
const NBD_REPLY_MAGIC: u32 = 0x67446698;
const NBD_REQUEST_MAGIC: u32 = 0x25609513;
const NBD_SIMPLE_REPLY_MAGIC: u32 = 0x67446698;

// NBD option types
const NBD_OPT_EXPORT_NAME: u32 = 1;
const NBD_OPT_ABORT: u32 = 2;
const NBD_OPT_GO: u32 = 7;

// NBD option reply types
const NBD_REP_ACK: u32 = 1;
const NBD_REP_INFO: u32 = 3;
const NBD_REP_ERR_UNSUP: u32 = 0x80000001;

// NBD info types
const NBD_INFO_EXPORT: u16 = 0;

// NBD command types
const NBD_CMD_READ: u16 = 0;
const NBD_CMD_WRITE: u16 = 1;
const NBD_CMD_DISC: u16 = 2;
const NBD_CMD_FLUSH: u16 = 3;
const NBD_CMD_TRIM: u16 = 4;

// NBD error codes
const NBD_OK: u32 = 0;
const NBD_EIO: u32 = 5;
const NBD_EINVAL: u32 = 22;

// NBD transmission flags
const NBD_FLAG_HAS_FLAGS: u16 = 1;
const NBD_FLAG_SEND_FLUSH: u16 = 4;
const NBD_FLAG_SEND_TRIM: u16 = 32;

// NBD handshake flags
const NBD_FLAG_FIXED_NEWSTYLE: u16 = 1;

/// NBD server that exports a BlockDevice over TCP.
pub struct NbdServer {
    device: Arc<dyn BlockDevice>,
    listen_addr: String,
}

impl NbdServer {
    /// Create a new NBD server for the given device.
    pub fn new(device: Arc<dyn BlockDevice>, listen_addr: &str) -> Self {
        NbdServer {
            device,
            listen_addr: listen_addr.to_string(),
        }
    }

    /// Run the NBD server, accepting connections until shutdown.
    pub async fn run(&self, shutdown: tokio::sync::watch::Receiver<bool>) -> DriveResult<()> {
        let listener = TcpListener::bind(&self.listen_addr).await
            .map_err(|e| super::DriveError::Other(anyhow::anyhow!("NBD bind failed: {e}")))?;
        tracing::info!("NBD server listening on {}", self.listen_addr);

        let mut shutdown_local = shutdown.clone();
        loop {
            tokio::select! {
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, peer)) => {
                            tracing::info!("NBD client connected from {peer}");
                            let device = self.device.clone();
                            let mut shutdown_rx = shutdown.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_nbd_connection(stream, device, &mut shutdown_rx).await {
                                    tracing::error!("NBD session error: {e}");
                                }
                            });
                        }
                        Err(e) => {
                            tracing::error!("NBD accept error: {e}");
                        }
                    }
                }
                _ = shutdown_local.changed() => {
                    tracing::info!("NBD server shutting down");
                    break;
                }
            }
        }

        Ok(())
    }

    /// Run serving a single connection (for nbd-client or testing).
    pub async fn serve_one(&self, stream: TcpStream) -> anyhow::Result<()> {
        let (tx, rx) = tokio::sync::watch::channel(false);
        handle_nbd_connection(stream, self.device.clone(), &mut rx.clone()).await?;
        drop(tx);
        Ok(())
    }
}

async fn handle_nbd_connection(
    mut stream: TcpStream,
    device: Arc<dyn BlockDevice>,
    _shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let size = device.capacity_bytes();

    // --- Phase 1: Newstyle negotiation ---

    // Send initial handshake
    stream.write_u64(NBD_MAGIC).await?;
    stream.write_u64(NBD_OPTS_MAGIC).await?;
    // Handshake flags: fixed newstyle
    stream.write_u16(NBD_FLAG_FIXED_NEWSTYLE).await?;
    stream.flush().await?;

    // Read client flags
    let _client_flags = stream.read_u32().await?;

    // --- Phase 2: Option haggling ---
    loop {
        let opt_magic = stream.read_u64().await?;
        if opt_magic != NBD_OPTS_MAGIC {
            anyhow::bail!("bad option magic: {opt_magic:#x}");
        }

        let opt_type = stream.read_u32().await?;
        let opt_len = stream.read_u32().await?;

        // Read option data
        let mut opt_data = vec![0u8; opt_len as usize];
        if opt_len > 0 {
            stream.read_exact(&mut opt_data).await?;
        }

        match opt_type {
            NBD_OPT_EXPORT_NAME => {
                // Old-style: send export info directly, no reply
                let flags: u16 = NBD_FLAG_HAS_FLAGS | NBD_FLAG_SEND_FLUSH | NBD_FLAG_SEND_TRIM;
                stream.write_u64(size).await?;
                stream.write_u16(flags).await?;
                // 124 bytes of zeroed padding
                stream.write_all(&[0u8; 124]).await?;
                stream.flush().await?;
                break;
            }
            NBD_OPT_GO => {
                // Newstyle: send NBD_REP_INFO then NBD_REP_ACK
                let flags: u16 = NBD_FLAG_HAS_FLAGS | NBD_FLAG_SEND_FLUSH | NBD_FLAG_SEND_TRIM;

                // Send export info
                let info_len = 12u32; // 2 (info type) + 8 (size) + 2 (flags)
                stream.write_u64(NBD_REPLY_MAGIC as u64).await?;
                stream.write_u32(opt_type).await?;
                stream.write_u32(NBD_REP_INFO).await?;
                stream.write_u32(info_len).await?;
                stream.write_u16(NBD_INFO_EXPORT).await?;
                stream.write_u64(size).await?;
                stream.write_u16(flags).await?;

                // Send ACK
                stream.write_u64(NBD_REPLY_MAGIC as u64).await?;
                stream.write_u32(opt_type).await?;
                stream.write_u32(NBD_REP_ACK).await?;
                stream.write_u32(0).await?;
                stream.flush().await?;
                break;
            }
            NBD_OPT_ABORT => {
                // Client aborted
                stream.write_u64(NBD_REPLY_MAGIC as u64).await?;
                stream.write_u32(opt_type).await?;
                stream.write_u32(NBD_REP_ACK).await?;
                stream.write_u32(0).await?;
                stream.flush().await?;
                return Ok(());
            }
            _ => {
                // Unknown option — send ERR_UNSUP
                stream.write_u64(NBD_REPLY_MAGIC as u64).await?;
                stream.write_u32(opt_type).await?;
                stream.write_u32(NBD_REP_ERR_UNSUP).await?;
                stream.write_u32(0).await?;
                stream.flush().await?;
            }
        }
    }

    // --- Phase 3: Transmission ---
    loop {
        let magic = stream.read_u32().await?;
        if magic != NBD_REQUEST_MAGIC {
            anyhow::bail!("bad request magic: {magic:#x}");
        }

        let _flags = stream.read_u16().await?;
        let cmd_type = stream.read_u16().await?;
        let handle = stream.read_u64().await?;
        let offset = stream.read_u64().await?;
        let length = stream.read_u32().await?;

        match cmd_type {
            NBD_CMD_READ => {
                let mut buf = vec![0u8; length as usize];
                let error = match device.read(offset, &mut buf).await {
                    Ok(_) => NBD_OK,
                    Err(e) => {
                        tracing::error!("NBD read error at {offset}+{length}: {e}");
                        NBD_EIO
                    }
                };
                // Send reply
                stream.write_u32(NBD_SIMPLE_REPLY_MAGIC).await?;
                stream.write_u32(error).await?;
                stream.write_u64(handle).await?;
                if error == NBD_OK {
                    stream.write_all(&buf).await?;
                }
                stream.flush().await?;
            }
            NBD_CMD_WRITE => {
                let mut buf = vec![0u8; length as usize];
                stream.read_exact(&mut buf).await?;
                let error = match device.write(offset, &buf).await {
                    Ok(_) => NBD_OK,
                    Err(e) => {
                        tracing::error!("NBD write error at {offset}+{length}: {e}");
                        NBD_EIO
                    }
                };
                stream.write_u32(NBD_SIMPLE_REPLY_MAGIC).await?;
                stream.write_u32(error).await?;
                stream.write_u64(handle).await?;
                stream.flush().await?;
            }
            NBD_CMD_DISC => {
                tracing::info!("NBD client disconnected");
                return Ok(());
            }
            NBD_CMD_FLUSH => {
                let error = match device.flush().await {
                    Ok(()) => NBD_OK,
                    Err(e) => {
                        tracing::error!("NBD flush error: {e}");
                        NBD_EIO
                    }
                };
                stream.write_u32(NBD_SIMPLE_REPLY_MAGIC).await?;
                stream.write_u32(error).await?;
                stream.write_u64(handle).await?;
                stream.flush().await?;
            }
            NBD_CMD_TRIM => {
                let error = match device.discard(offset, length as u64).await {
                    Ok(()) => NBD_OK,
                    Err(e) => {
                        tracing::error!("NBD trim error at {offset}+{length}: {e}");
                        NBD_EIO
                    }
                };
                stream.write_u32(NBD_SIMPLE_REPLY_MAGIC).await?;
                stream.write_u32(error).await?;
                stream.write_u64(handle).await?;
                stream.flush().await?;
            }
            _ => {
                // Unknown command
                stream.write_u32(NBD_SIMPLE_REPLY_MAGIC).await?;
                stream.write_u32(NBD_EINVAL).await?;
                stream.write_u64(handle).await?;
                stream.flush().await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drive::filedev::FileDevice;

    #[tokio::test]
    async fn nbd_handshake_and_io() {
        let dir = std::env::temp_dir().join("stormblock-nbd-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nbd-dev.bin");
        let path_str = path.to_str().unwrap();
        let _ = std::fs::remove_file(&path);

        let dev = Arc::new(
            FileDevice::open_with_capacity(path_str, 4 * 1024 * 1024).await.unwrap()
        ) as Arc<dyn BlockDevice>;

        let server = NbdServer::new(dev.clone(), "127.0.0.1:0");

        // Bind to a random port for testing
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let dev_clone = dev.clone();
        let server_handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (tx, rx) = tokio::sync::watch::channel(false);
            let _ = handle_nbd_connection(stream, dev_clone, &mut rx.clone()).await;
            drop(tx);
        });

        // Client side
        let mut client = TcpStream::connect(addr).await.unwrap();

        // Read handshake
        let magic = client.read_u64().await.unwrap();
        assert_eq!(magic, NBD_MAGIC);
        let opts_magic = client.read_u64().await.unwrap();
        assert_eq!(opts_magic, NBD_OPTS_MAGIC);
        let _flags = client.read_u16().await.unwrap();

        // Send client flags
        client.write_u32(0).await.unwrap();

        // Send OPT_EXPORT_NAME
        client.write_u64(NBD_OPTS_MAGIC).await.unwrap();
        client.write_u32(NBD_OPT_EXPORT_NAME).await.unwrap();
        client.write_u32(0).await.unwrap(); // empty name
        client.flush().await.unwrap();

        // Read export info
        let size = client.read_u64().await.unwrap();
        assert_eq!(size, 4 * 1024 * 1024);
        let _flags = client.read_u16().await.unwrap();
        let mut pad = [0u8; 124];
        client.read_exact(&mut pad).await.unwrap();

        // Write 4KB at offset 0
        let data = vec![0xAB_u8; 4096];
        client.write_u32(NBD_REQUEST_MAGIC).await.unwrap();
        client.write_u16(0).await.unwrap(); // flags
        client.write_u16(NBD_CMD_WRITE).await.unwrap();
        client.write_u64(1).await.unwrap(); // handle
        client.write_u64(0).await.unwrap(); // offset
        client.write_u32(4096).await.unwrap(); // length
        client.write_all(&data).await.unwrap();
        client.flush().await.unwrap();

        // Read write reply
        let reply_magic = client.read_u32().await.unwrap();
        assert_eq!(reply_magic, NBD_SIMPLE_REPLY_MAGIC);
        let error = client.read_u32().await.unwrap();
        assert_eq!(error, NBD_OK);
        let handle = client.read_u64().await.unwrap();
        assert_eq!(handle, 1);

        // Read 4KB at offset 0
        client.write_u32(NBD_REQUEST_MAGIC).await.unwrap();
        client.write_u16(0).await.unwrap();
        client.write_u16(NBD_CMD_READ).await.unwrap();
        client.write_u64(2).await.unwrap(); // handle
        client.write_u64(0).await.unwrap(); // offset
        client.write_u32(4096).await.unwrap();
        client.flush().await.unwrap();

        // Read reply
        let reply_magic = client.read_u32().await.unwrap();
        assert_eq!(reply_magic, NBD_SIMPLE_REPLY_MAGIC);
        let error = client.read_u32().await.unwrap();
        assert_eq!(error, NBD_OK);
        let handle = client.read_u64().await.unwrap();
        assert_eq!(handle, 2);
        let mut read_buf = vec![0u8; 4096];
        client.read_exact(&mut read_buf).await.unwrap();
        assert_eq!(read_buf, data);

        // Disconnect
        client.write_u32(NBD_REQUEST_MAGIC).await.unwrap();
        client.write_u16(0).await.unwrap();
        client.write_u16(NBD_CMD_DISC).await.unwrap();
        client.write_u64(3).await.unwrap();
        client.write_u64(0).await.unwrap();
        client.write_u32(0).await.unwrap();
        client.flush().await.unwrap();

        let _ = server_handle.await;
        let _ = std::fs::remove_file(&path);
    }
}
