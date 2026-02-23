//! NVMe userspace driver via VFIO — maps PCIe BAR0, programs admin + I/O queue pairs directly.
//!
//! This module defines the data structures for the NVMe userspace driver.
//! The actual VFIO initialization requires bare-metal hardware with PCIe passthrough
//! and is not yet implemented. See docs/stormblock-spec.md section 4.2.

#![allow(dead_code)]

use std::os::unix::io::RawFd;

use uuid::Uuid;

use super::{DeviceId, DriveError, DriveResult};

// --- NVMe register structures (NVMe spec) ---

/// NVMe controller registers (BAR0 memory-mapped).
#[repr(C)]
pub struct NvmeRegisters {
    pub cap: u64,      // Controller Capabilities
    pub vs: u32,       // Version
    pub intms: u32,    // Interrupt Mask Set
    pub intmc: u32,    // Interrupt Mask Clear
    pub cc: u32,       // Controller Configuration
    pub _rsvd: u32,
    pub csts: u32,     // Controller Status
    pub nssr: u32,     // NVM Subsystem Reset
    pub aqa: u32,      // Admin Queue Attributes
    pub asq: u64,      // Admin Submission Queue Base Address
    pub acq: u64,      // Admin Completion Queue Base Address
}

/// Submission queue entry (64 bytes, NVMe spec).
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct NvmeSubmissionEntry {
    pub opcode: u8,
    pub flags: u8,
    pub command_id: u16,
    pub nsid: u32,
    pub reserved: [u64; 2],
    pub metadata_ptr: u64,
    pub prp1: u64,
    pub prp2: u64,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

/// Completion queue entry (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct NvmeCompletionEntry {
    pub result: u32,
    pub reserved: u32,
    pub sq_head: u16,
    pub sq_id: u16,
    pub command_id: u16,
    pub status: u16, // Status field + phase bit
}

// NVMe opcodes
pub const NVME_OPC_READ: u8 = 0x02;
pub const NVME_OPC_WRITE: u8 = 0x01;
pub const NVME_OPC_FLUSH: u8 = 0x00;
pub const NVME_OPC_DSM: u8 = 0x09; // Dataset Management (TRIM)
pub const NVME_ADMIN_IDENTIFY: u8 = 0x06;
pub const NVME_ADMIN_CREATE_IO_CQ: u8 = 0x05;
pub const NVME_ADMIN_CREATE_IO_SQ: u8 = 0x01;

// --- Device structures ---

/// An NVMe device accessed via VFIO userspace driver.
pub struct NvmeDevice {
    /// VFIO container and device file descriptors.
    pub(crate) vfio_fd: RawFd,
    pub(crate) device_fd: RawFd,

    /// Memory-mapped NVMe registers (BAR0).
    pub(crate) regs: *mut NvmeRegisters,

    /// Admin queue pair.
    pub(crate) admin_sq_depth: u32,
    pub(crate) admin_cq_depth: u32,

    /// I/O queue pairs (one per core for lock-free operation).
    pub(crate) io_queues: Vec<IoQueuePair>,

    /// Device identity.
    pub(crate) id: DeviceId,
    pub(crate) serial: String,
    pub(crate) model: String,
    pub(crate) firmware: String,
    pub(crate) namespaces: Vec<NvmeNamespace>,
    pub(crate) max_transfer_size: u32,
}

/// Per-core I/O queue pair.
pub struct IoQueuePair {
    pub sq_dma_addr: u64,
    pub cq_dma_addr: u64,
    pub sq_doorbell: *mut u32,
    pub cq_doorbell: *mut u32,
    pub sq_tail: u32,
    pub cq_head: u32,
    pub cq_phase: bool,
    pub depth: u32,
    pub core_id: usize,
}

/// NVMe namespace info.
#[derive(Debug, Clone)]
pub struct NvmeNamespace {
    pub nsid: u32,
    pub capacity_blocks: u64,
    pub block_size: u32,
}

// Safety: NvmeDevice pointers are used only by the owning thread.
unsafe impl Send for NvmeDevice {}
unsafe impl Sync for NvmeDevice {}
unsafe impl Send for IoQueuePair {}
unsafe impl Sync for IoQueuePair {}

/// VFIO container and group file descriptors.
struct VfioFds {
    container_fd: RawFd,
    group_fd: RawFd,
    device_fd: RawFd,
}

impl NvmeDevice {
    /// Initialize an NVMe device via VFIO.
    ///
    /// `pci_addr` — PCI address in BDF notation (e.g., "0000:03:00.0").
    ///
    /// Steps:
    /// 1. Open VFIO container (/dev/vfio/vfio)
    /// 2. Open VFIO group (/dev/vfio/<group>)
    /// 3. Set IOMMU type (TYPE1v2)
    /// 4. Get device fd from group
    /// 5. Map BAR0 for register access
    /// 6. Create admin queue pair
    /// 7. Issue Identify Controller
    /// 8. Create I/O queue pairs
    pub fn init(pci_addr: &str) -> DriveResult<Self> {
        let fds = Self::open_vfio(pci_addr)?;
        let regs = Self::map_bar0(fds.device_fd)?;

        // Read controller version from mapped registers
        let vs = unsafe { (*regs).vs };
        tracing::info!("NVMe controller version: {}.{}", vs >> 16, (vs >> 8) & 0xFF);

        // Disable controller (CC.EN = 0)
        unsafe {
            (*regs).cc = 0;
        }

        // Wait for CSTS.RDY to clear
        Self::wait_csts_rdy(regs, false)?;

        // Create admin queues
        let admin_depth: u32 = 32;
        let admin_sq = super::dma::DmaBuf::alloc(admin_depth as usize * 64);
        let admin_cq = super::dma::DmaBuf::alloc(admin_depth as usize * 16);

        // Configure AQA (Admin Queue Attributes)
        unsafe {
            (*regs).aqa = ((admin_depth - 1) << 16) | (admin_depth - 1);
            (*regs).asq = admin_sq.as_ptr() as u64;
            (*regs).acq = admin_cq.as_ptr() as u64;
        }

        // Enable controller (CC.EN = 1, IOSQES=6, IOCQES=4, MPS=0, CSS=0)
        let cc = (6 << 16) | (4 << 20) | 1;
        unsafe { (*regs).cc = cc; }

        // Wait for CSTS.RDY
        Self::wait_csts_rdy(regs, true)?;

        let id = DeviceId {
            uuid: Uuid::new_v4(),
            serial: "vfio".to_string(),
            model: "NVMe-VFIO".to_string(),
            path: pci_addr.to_string(),
        };

        Ok(NvmeDevice {
            vfio_fd: fds.container_fd,
            device_fd: fds.device_fd,
            regs,
            admin_sq_depth: admin_depth,
            admin_cq_depth: admin_depth,
            io_queues: Vec::new(),
            id,
            serial: "vfio".to_string(),
            model: "NVMe-VFIO".to_string(),
            firmware: String::new(),
            namespaces: Vec::new(),
            max_transfer_size: 131072, // 128KB default, updated after Identify
        })
    }

    /// Open VFIO container, group, and device.
    fn open_vfio(pci_addr: &str) -> DriveResult<VfioFds> {
        // Find IOMMU group for this PCI device
        let group_link = format!("/sys/bus/pci/devices/{pci_addr}/iommu_group");
        let group_path = std::fs::read_link(&group_link)
            .map_err(|e| DriveError::Other(anyhow::anyhow!(
                "cannot find IOMMU group for {pci_addr}: {e}. Is the device bound to vfio-pci?"
            )))?;
        let group_num: u32 = group_path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| s.parse().ok())
            .ok_or(DriveError::VfioNotAvailable)?;

        // Open VFIO container
        let container_fd = unsafe {
            libc::open(b"/dev/vfio/vfio\0".as_ptr() as *const libc::c_char, libc::O_RDWR)
        };
        if container_fd < 0 {
            return Err(DriveError::Io(std::io::Error::last_os_error()));
        }

        // Open VFIO group
        let group_dev = format!("/dev/vfio/{group_num}\0");
        let group_fd = unsafe {
            libc::open(group_dev.as_ptr() as *const libc::c_char, libc::O_RDWR)
        };
        if group_fd < 0 {
            unsafe { libc::close(container_fd); }
            return Err(DriveError::Io(std::io::Error::last_os_error()));
        }

        // Set IOMMU type on container (TYPE1v2 = 3)
        let ret = unsafe { libc::ioctl(container_fd, 0x3B01u64 as libc::Ioctl, 3i32) };
        if ret < 0 {
            unsafe { libc::close(group_fd); libc::close(container_fd); }
            return Err(DriveError::Other(anyhow::anyhow!("VFIO SET_IOMMU failed")));
        }

        // Get device fd from group
        let pci_addr_c = format!("{pci_addr}\0");
        let device_fd = unsafe {
            libc::ioctl(group_fd, 0x3B06u64 as libc::Ioctl, pci_addr_c.as_ptr())
        };
        if device_fd < 0 {
            unsafe { libc::close(group_fd); libc::close(container_fd); }
            return Err(DriveError::Io(std::io::Error::last_os_error()));
        }

        Ok(VfioFds { container_fd, group_fd, device_fd })
    }

    /// Memory-map BAR0 for register access.
    fn map_bar0(device_fd: RawFd) -> DriveResult<*mut NvmeRegisters> {
        // BAR0 region info: VFIO_DEVICE_GET_REGION_INFO
        // Region 0 = BAR0, offset for mmap comes from VFIO region info
        // For simplicity, use the standard BAR0 mmap offset
        let bar0_offset: libc::off_t = 0; // VFIO region 0 offset
        let bar0_size: usize = 0x4000; // 16KB typical NVMe register space

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bar0_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                device_fd,
                bar0_offset,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(DriveError::Io(std::io::Error::last_os_error()));
        }

        Ok(ptr as *mut NvmeRegisters)
    }

    /// Wait for CSTS.RDY to reach expected value.
    fn wait_csts_rdy(regs: *mut NvmeRegisters, expected: bool) -> DriveResult<()> {
        let target = if expected { 1u32 } else { 0u32 };
        for _ in 0..1000 {
            let csts = unsafe { (*regs).csts };
            if (csts & 1) == target {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        Err(DriveError::DeviceNotReady)
    }
}
