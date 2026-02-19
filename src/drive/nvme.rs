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

impl NvmeDevice {
    /// Initialize an NVMe device via VFIO.
    ///
    /// Not yet implemented — requires bare-metal hardware with PCIe passthrough.
    pub fn init(_pci_addr: &str) -> DriveResult<Self> {
        Err(DriveError::VfioNotAvailable)
    }
}
