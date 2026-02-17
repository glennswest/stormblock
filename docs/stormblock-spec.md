# StormBlock — Pure Rust Enterprise Block Storage Engine

## Specification Document v0.1

**Project:** StormBlock — Bare-metal block storage engine  
**Language:** Pure Rust, single static binary (x86_64 + aarch64)  
**Runtime:** Minimal Linux kernel (buildroot) + Rust userspace  
**Targets:** NVMe-oF/TCP, iSCSI (RFC 7143)  
**Architecture:** Tiered: NVMe E1.S/E3.S (200GbE) → SAS SSD → SAS HDD JBOD (ARM64 25GbE)

**Companion Documents:**
- **StormFS v0.4** (`stormfs-spec.md`) — Distributed filesystem that consumes StormBlock volumes via NVMe-oF/TCP for its data path.
- **StormForce v0.1** (`stormforce-spec.md`) — Event streaming platform. Can optionally use StormBlock for tiered log segment storage.
- **StormOS v0.1** (`stormos-spec.md`) — Infrastructure OS that manages StormBlock as a native service on bare-metal nodes.

---

## 1. Design Philosophy

StormBlock is the block-layer engine underneath StormFS. It turns raw physical drives — NVMe SSDs, SAS SSDs, SAS HDDs — into network-accessible logical volumes over NVMe-oF/TCP and iSCSI. Every I/O request from a StormFS client or a bare iSCSI/NVMe-oF initiator terminates here.

**Core principles:**

- **Pure Rust data path.** No SPDK, no FFI to C libraries. The NVMe driver, RAID engine, volume manager, and target protocols are all Rust. Unsafe code is confined to VFIO/MMIO boundary crossings and DMA buffer management.
- **Single static binary.** One `stormblock` binary compiled with musl, runs on both x86_64 (Tier 0/1 NVMe/SAS nodes) and aarch64 (Tier 2 ARM JBOD head units). No dynamic linking, no runtime dependencies beyond the kernel.
- **Userspace I/O for NVMe.** NVMe drives are accessed via VFIO — bypassing the kernel block layer entirely. The kernel is only used for hardware init, PCIe enumeration, TCP stack, and SAS HBA drivers.
- **Kernel-assisted I/O for SAS.** SAS drives are behind LSI/Broadcom HBA controllers that require kernel drivers (mpt3sas/megaraid). StormBlock accesses these via io_uring on /dev/sdX with O_DIRECT. This is the pragmatic path — writing a userspace SAS initiator would be a multi-year detour.
- **io_uring everywhere.** Network I/O (target protocols), SAS disk I/O, and management plane all use io_uring for async submission/completion. The NVMe userspace driver uses its own polling loop (MMIO doorbells) but integrates with the io_uring event loop via eventfd bridging.

---

## 2. System Architecture

```
┌──────────────────────────────────────────────────────────────────────┐
│                        StormBlock Binary (Rust)                         │
│                                                                      │
│  ┌─────────────────────────────────────────────────────────────────┐ │
│  │                    Management Plane                              │ │
│  │  REST API (axum)  │  Prometheus metrics  │  Web UI  │  CLI      │ │
│  └───────────────────────────────┬─────────────────────────────────┘ │
│                                  │                                    │
│  ┌───────────────────────────────▼─────────────────────────────────┐ │
│  │                    Target Protocol Layer                         │ │
│  │  ┌──────────────────┐  ┌──────────────────┐                     │ │
│  │  │  NVMe-oF/TCP     │  │  iSCSI Target    │                     │ │
│  │  │  Target           │  │  (RFC 7143)      │                     │ │
│  │  │  Port 4420        │  │  Port 3260       │                     │ │
│  │  └────────┬─────────┘  └────────┬─────────┘                     │ │
│  └───────────┼─────────────────────┼───────────────────────────────┘ │
│              │                     │                                  │
│  ┌───────────▼─────────────────────▼───────────────────────────────┐ │
│  │                    Volume Manager                                │ │
│  │  ┌──────────┐  ┌───────────┐  ┌──────────┐  ┌───────────────┐  │ │
│  │  │ Thin     │  │ COW       │  │ Extent   │  │ Volume → LUN  │  │ │
│  │  │ Provision│  │ Snapshots │  │ Allocator│  │ Mapping       │  │ │
│  │  └────┬─────┘  └─────┬─────┘  └────┬─────┘  └──────┬────────┘  │ │
│  └───────┼───────────────┼─────────────┼───────────────┼───────────┘ │
│          │               │             │               │              │
│  ┌───────▼───────────────▼─────────────▼───────────────▼───────────┐ │
│  │                    RAID Engine                                    │ │
│  │  RAID 0 │ RAID 1 │ RAID 5 │ RAID 6 │ RAID 10                    │ │
│  │  SIMD parity (AVX2/AVX-512/NEON)  │  Background rebuild          │ │
│  │  Journal (write-intent bitmap)     │  Scrub / verify              │ │
│  └───────────────────────┬─────────────────────────────────────────┘ │
│                          │                                            │
│  ┌───────────────────────▼─────────────────────────────────────────┐ │
│  │                    Drive Layer                                    │ │
│  │  ┌───────────────────────┐  ┌──────────────────────────────┐    │ │
│  │  │  NVMe Userspace Driver │  │  SAS/SATA via io_uring       │    │ │
│  │  │  (VFIO + MMIO polling) │  │  (O_DIRECT + kernel drivers) │    │ │
│  │  │  PCIe BAR mapping      │  │  mpt3sas / megaraid_sas      │    │ │
│  │  │  Admin + I/O queues    │  │  /dev/sdX block devices       │    │ │
│  │  │  DMA via hugepages     │  │  Registered buffers           │    │ │
│  │  └───────────────────────┘  └──────────────────────────────┘    │ │
│  └─────────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────────┘
         │                              │
    VFIO passthrough              Kernel block layer
         │                              │
    ┌────▼────┐                    ┌────▼────┐
    │ NVMe    │                    │ SAS/SATA│
    │ SSDs    │                    │ HDDs/SSDs│
    │ (E1.S,  │                    │ (via HBA)│
    │  E3.S,  │                    │          │
    │  U.2)   │                    │          │
    └─────────┘                    └──────────┘
```

---

## 3. Boot & Runtime Environment

### 3.1 Minimal Linux Image (Buildroot)

StormBlock runs on a stripped Linux kernel built with buildroot. The kernel provides hardware initialization, PCIe enumeration, SAS HBA drivers, TCP/IP stack, and VFIO infrastructure. Everything else runs in userspace.

```
stormblock-image/
├── bzImage                    # ~8MB kernel (stripped, NVMe/SAS/network/VFIO only)
├── initramfs.cpio.zst         # ~15MB (busybox + stormblock binary + config)
└── stormblock.toml               # Configuration (drive map, network, topology)
```

**Kernel configuration (key options):**

```
# VFIO for NVMe userspace driver
CONFIG_VFIO=y
CONFIG_VFIO_PCI=y
CONFIG_VFIO_IOMMU_TYPE1=y

# IOMMU (Intel VT-d / AMD-Vi)
CONFIG_INTEL_IOMMU=y
CONFIG_AMD_IOMMU=y
CONFIG_IOMMU_DEFAULT_DMA_STRICT=n    # passthrough mode for performance

# io_uring (target protocols + SAS I/O)
CONFIG_IO_URING=y

# SAS HBA drivers (for Tier 1/2 SAS drives)
CONFIG_SCSI_MPT3SAS=y                # LSI SAS 3008/3108 (HBA330)
CONFIG_SCSI_MEGARAID_SAS=y           # Broadcom MegaRAID (if present)

# Networking
CONFIG_MLX5_CORE=y                   # Mellanox ConnectX-4/5/6/7
CONFIG_MLX4_EN=y                     # Mellanox ConnectX-3
CONFIG_NET_VENDOR_INTEL=y            # Intel X710/E810

# Hugepages for DMA buffers
CONFIG_HUGETLBFS=y
CONFIG_TRANSPARENT_HUGEPAGE=y

# Disable everything we don't need
CONFIG_SOUND=n
CONFIG_USB=n
CONFIG_DRM=n
CONFIG_WIRELESS=n
CONFIG_BLUETOOTH=n
CONFIG_INPUT=n
```

**Boot parameters:**

```
intel_iommu=on iommu=pt hugepagesz=1G hugepages=4 default_hugepagesz=2M
hugepages=2048 isolcpus=2-N nohz_full=2-N rcu_nocbs=2-N
```

- `iommu=pt` — passthrough mode, only devices explicitly bound to VFIO go through IOMMU translation
- `hugepages` — pre-allocate 4×1GB + 2048×2MB pages for DMA buffers
- `isolcpus` / `nohz_full` — isolate I/O cores from scheduler interference (core 0-1 for management, rest for I/O)

### 3.2 Boot Sequence

```
1. BIOS/UEFI → PXE boot or local SSD boot
2. Linux kernel boots (~2 seconds)
3. initramfs loads:
   a. Mount hugetlbfs at /dev/hugepages
   b. Bind NVMe devices to vfio-pci driver
   c. Configure network interfaces (DHCP or static from stormblock.toml)
   d. Start stormblock binary
4. stormblock init sequence:
   a. Parse stormblock.toml configuration
   b. Initialize NVMe userspace driver (VFIO BAR mapping, queue pair setup)
   c. Enumerate SAS drives via sysfs (/sys/class/scsi_disk/*)
   d. Load or create RAID superblocks
   e. Load or create volume manager metadata
   f. Start target protocol listeners (NVMe-oF/TCP :4420, iSCSI :3260)
   g. Start management API (:8443)
   h. Register with StormFS metadata cluster (if configured)
5. Ready to serve I/O (~5-8 seconds total boot)
```

### 3.3 VFIO Device Binding

On boot, NVMe devices are unbound from the kernel nvme driver and rebound to vfio-pci:

```bash
#!/bin/sh
# initramfs script: bind NVMe devices to VFIO

for dev in /sys/bus/pci/devices/*/class; do
    class=$(cat "$dev")
    # NVMe class code: 0x010802
    if [ "$class" = "0x010802" ]; then
        pci_addr=$(dirname "$dev" | xargs basename)
        vendor=$(cat "$(dirname "$dev")/vendor")
        device=$(cat "$(dirname "$dev")/device")
        
        # Unbind from kernel nvme driver
        echo "$pci_addr" > /sys/bus/pci/devices/$pci_addr/driver/unbind 2>/dev/null
        
        # Bind to vfio-pci
        echo "$vendor $device" > /sys/bus/pci/drivers/vfio-pci/new_id
        echo "$pci_addr" > /sys/bus/pci/drivers/vfio-pci/bind
    fi
done
```

SAS/SATA devices remain under kernel control — they're accessed through standard block device paths via io_uring.

---

## 4. Drive Layer

The drive layer abstracts the physical I/O interface. It presents a uniform `BlockDevice` trait to the RAID engine regardless of whether the underlying drive is NVMe (userspace VFIO) or SAS (kernel io_uring).

### 4.1 BlockDevice Trait

```rust
/// Unified interface for all physical drive types
#[async_trait]
pub trait BlockDevice: Send + Sync {
    /// Device identity
    fn id(&self) -> &DeviceId;
    fn capacity_bytes(&self) -> u64;
    fn block_size(&self) -> u32;           // 512 or 4096
    fn optimal_io_size(&self) -> u32;      // For alignment
    fn device_type(&self) -> DriveType;    // NVMe, SasSsd, SasHdd
    
    /// I/O operations
    async fn read(&self, offset: u64, buf: &mut DmaBuf) -> Result<()>;
    async fn write(&self, offset: u64, buf: &DmaBuf) -> Result<()>;
    async fn flush(&self) -> Result<()>;
    async fn discard(&self, offset: u64, len: u64) -> Result<()>;  // TRIM/UNMAP
    
    /// Batch I/O (submit multiple ops, poll completions)
    fn submit_batch(&self, ops: &[IoOp]) -> Result<u32>;  // returns submitted count
    fn poll_completions(&self, completions: &mut Vec<IoCompletion>) -> Result<u32>;
    
    /// Health
    fn smart_status(&self) -> Result<SmartData>;
    fn media_errors(&self) -> u64;
}

/// DMA-safe buffer (hugepage-backed for NVMe, page-aligned for SAS)
pub struct DmaBuf {
    ptr: *mut u8,
    len: usize,
    iova: u64,            // IOMMU virtual address (NVMe only)
    source: BufSource,    // Hugepage or PageAligned
}

#[derive(Clone)]
pub enum IoOp {
    Read { offset: u64, buf_idx: u32, len: u32 },
    Write { offset: u64, buf_idx: u32, len: u32 },
    Flush,
    Discard { offset: u64, len: u64 },
}

pub struct IoCompletion {
    pub op_idx: u32,
    pub result: Result<u32>,    // bytes transferred or error
    pub latency_ns: u64,
}
```

### 4.2 NVMe Userspace Driver (VFIO)

The NVMe driver maps the device's PCIe BAR0 registers into userspace via VFIO, then directly programs the NVMe controller's admin and I/O queues. Based on the approach proven by TU Munich's `vroom` driver, which achieved SPDK-equivalent throughput in pure Rust.

```rust
pub struct NvmeDevice {
    /// VFIO container and device file descriptors
    vfio_fd: RawFd,
    device_fd: RawFd,
    
    /// Memory-mapped NVMe registers (BAR0)
    regs: *mut NvmeRegisters,
    
    /// Admin queue pair (for identify, create I/O queues, etc.)
    admin_sq: SubmissionQueue,
    admin_cq: CompletionQueue,
    
    /// I/O queue pairs (one per core for lock-free operation)
    io_queues: Vec<IoQueuePair>,
    
    /// DMA buffer pool (hugepage-backed)
    dma_pool: DmaPool,
    
    /// Device identity
    serial: String,
    model: String,
    firmware: String,
    namespaces: Vec<NvmeNamespace>,
    max_transfer_size: u32,
}

struct IoQueuePair {
    sq: SubmissionQueue,         // Submission queue (in DMA memory)
    cq: CompletionQueue,         // Completion queue (in DMA memory)
    sq_doorbell: *mut u32,       // MMIO doorbell register
    cq_doorbell: *mut u32,
    sq_tail: u32,                // Software-tracked tail pointer
    cq_head: u32,
    cq_phase: bool,              // Phase bit for completion detection
    inflight: Vec<Option<IoRequest>>,  // Indexed by command ID
    core_id: usize,              // Pinned to this CPU core
}

/// Submission queue entry (64 bytes, NVMe spec)
#[repr(C)]
struct NvmeSubmissionEntry {
    opcode: u8,
    flags: u8,
    command_id: u16,
    nsid: u32,
    reserved: [u64; 2],
    metadata_ptr: u64,
    prp1: u64,                   // Physical Region Page 1 (DMA address)
    prp2: u64,                   // PRP2 or PRP list pointer
    cdw10: u32,                  // Command-specific
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
}

/// Completion queue entry (16 bytes)
#[repr(C)]
struct NvmeCompletionEntry {
    result: u32,
    reserved: u32,
    sq_head: u16,
    sq_id: u16,
    command_id: u16,
    status: u16,                 // Status field + phase bit
}
```

**Initialization sequence:**

```rust
impl NvmeDevice {
    pub fn init(pci_addr: &str) -> Result<Self> {
        // 1. Open VFIO container and group
        let container_fd = open("/dev/vfio/vfio", O_RDWR)?;
        let group_fd = open(&format!("/dev/vfio/{}", iommu_group), O_RDWR)?;
        
        // 2. Set IOMMU type
        ioctl(container_fd, VFIO_SET_IOMMU, VFIO_TYPE1v2_IOMMU)?;
        
        // 3. Get device fd
        let device_fd = ioctl(group_fd, VFIO_GROUP_GET_DEVICE_FD, pci_addr)?;
        
        // 4. Map BAR0 (NVMe registers)
        let bar0_info = vfio_get_region_info(device_fd, VFIO_PCI_BAR0_REGION_INDEX)?;
        let regs = mmap(
            null_mut(), bar0_info.size,
            PROT_READ | PROT_WRITE, MAP_SHARED,
            device_fd, bar0_info.offset
        )? as *mut NvmeRegisters;
        
        // 5. Allocate DMA memory (hugepages)
        let dma_pool = DmaPool::new(container_fd, 1 << 30)?;  // 1GB
        
        // 6. Reset controller
        write_volatile(&mut (*regs).cc, 0);  // CC.EN = 0
        while read_volatile(&(*regs).csts) & 0x1 != 0 { /* wait for RDY=0 */ }
        
        // 7. Create admin queue pair in DMA memory
        let admin_sq = dma_pool.alloc_queue(64)?;   // 64 entries
        let admin_cq = dma_pool.alloc_queue(64)?;
        write_volatile(&mut (*regs).aqa, (63 << 16) | 63);  // Admin Queue Attributes
        write_volatile(&mut (*regs).asq, admin_sq.dma_addr());
        write_volatile(&mut (*regs).acq, admin_cq.dma_addr());
        
        // 8. Enable controller
        write_volatile(&mut (*regs).cc, 
            (0 << 20) |     // IOSQES = 6 (64 bytes)
            (0 << 16) |     // IOCQES = 4 (16 bytes)
            (0 << 14) |     // SHN = 00 (no shutdown)
            (0 << 11) |     // AMS = round robin
            (0 << 7)  |     // MPS = 0 (4KB pages)
            (0 << 4)  |     // CSS = NVM command set
            1               // EN = 1
        );
        while read_volatile(&(*regs).csts) & 0x1 == 0 { /* wait for RDY=1 */ }
        
        // 9. Identify controller + namespaces
        let identify = Self::admin_identify(&admin_sq, &admin_cq, &dma_pool)?;
        
        // 10. Create I/O queue pairs (one per core)
        let num_cores = num_cpus::get_physical();
        let io_queues = (0..num_cores).map(|core| {
            Self::create_io_queue_pair(core, &admin_sq, &admin_cq, &dma_pool, regs)
        }).collect::<Result<Vec<_>>>()?;
        
        Ok(Self { /* ... */ })
    }
}
```

**I/O submission (lock-free, per-core):**

```rust
impl NvmeDevice {
    /// Submit a read command on the calling core's queue pair
    pub fn submit_read(&self, nsid: u32, lba: u64, blocks: u16, buf: &DmaBuf) -> u16 {
        let core = current_core_id();
        let qp = &self.io_queues[core];
        let cmd_id = qp.next_cmd_id();
        
        let sqe = NvmeSubmissionEntry {
            opcode: NVME_OPC_READ,  // 0x02
            flags: 0,
            command_id: cmd_id,
            nsid,
            reserved: [0; 2],
            metadata_ptr: 0,
            prp1: buf.iova,
            prp2: if buf.len > 4096 { buf.prp_list_iova() } else { 0 },
            cdw10: (lba & 0xFFFFFFFF) as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: blocks as u32 - 1,  // 0-based count
            ..Default::default()
        };
        
        // Write SQE to submission queue (already in DMA memory)
        unsafe {
            qp.sq.write_entry(qp.sq_tail, &sqe);
            qp.sq_tail = (qp.sq_tail + 1) % qp.sq.depth;
            
            // Ring doorbell (single MMIO write)
            write_volatile(qp.sq_doorbell, qp.sq_tail);
        }
        
        cmd_id
    }
    
    /// Poll completion queue for finished commands
    pub fn poll_completions(&self, core: usize) -> Vec<IoCompletion> {
        let qp = &mut self.io_queues[core];
        let mut completions = Vec::new();
        
        loop {
            let cqe = unsafe { qp.cq.read_entry(qp.cq_head) };
            
            // Check phase bit — toggles each time CQ wraps
            if ((cqe.status & 1) != 0) != qp.cq_phase {
                break;  // No more completions
            }
            
            let status = (cqe.status >> 1) & 0x7FFF;
            completions.push(IoCompletion {
                op_idx: cqe.command_id as u32,
                result: if status == 0 { Ok(0) } else { Err(nvme_error(status)) },
                latency_ns: qp.inflight[cqe.command_id as usize].elapsed_ns(),
            });
            
            qp.cq_head = (qp.cq_head + 1) % qp.cq.depth;
            if qp.cq_head == 0 { qp.cq_phase = !qp.cq_phase; }
        }
        
        if !completions.is_empty() {
            // Ring CQ doorbell
            unsafe { write_volatile(qp.cq_doorbell, qp.cq_head); }
        }
        
        completions
    }
}
```

### 4.3 SAS/SATA via io_uring

SAS and SATA drives remain under kernel control (mpt3sas, megaraid_sas drivers). StormBlock accesses them as block devices through io_uring with O_DIRECT for zero-copy alignment:

```rust
pub struct SasDevice {
    fd: RawFd,                          // /dev/sdX opened with O_DIRECT
    ring: IoUring,                      // Per-device io_uring instance
    capacity: u64,
    block_size: u32,
    serial: String,
    model: String,
    device_type: DriveType,             // SasSsd or SasHdd
    
    /// Registered buffer pool for io_uring fixed buffers
    buffers: RegisteredBufferPool,
}

impl SasDevice {
    pub fn open(path: &str) -> Result<Self> {
        let fd = open(path, O_RDWR | O_DIRECT | O_NONBLOCK)?;
        
        // Get device geometry
        let capacity = ioctl_blkgetsize64(fd)?;
        let block_size = ioctl_blksszget(fd)?;
        
        // Create io_uring instance with registered buffers
        let ring = IoUring::builder()
            .setup_sqpoll(2000)          // Kernel-side SQ polling (2ms idle timeout)
            .setup_single_issuer()       // Single thread submits
            .build(256)?;                // 256 SQ entries
        
        // Register fixed buffers for zero-copy
        let buffers = RegisteredBufferPool::new(256, 1 << 20)?;  // 256 × 1MB
        ring.register_buffers(buffers.iovecs())?;
        
        Ok(Self { fd, ring, capacity, block_size, /* ... */ })
    }
}

#[async_trait]
impl BlockDevice for SasDevice {
    async fn read(&self, offset: u64, buf: &mut DmaBuf) -> Result<()> {
        let sqe = io_uring::opcode::ReadFixed::new(
            types::Fd(self.fd),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
        .offset(offset)
        .buf_index(buf.registered_index() as u16)
        .build()
        .user_data(self.next_tag());
        
        unsafe { self.ring.submission().push(&sqe)?; }
        self.ring.submit_and_wait(1)?;
        
        let cqe = self.ring.completion().next().unwrap();
        if cqe.result() < 0 {
            return Err(io::Error::from_raw_os_error(-cqe.result()));
        }
        Ok(())
    }
    
    async fn write(&self, offset: u64, buf: &DmaBuf) -> Result<()> {
        let sqe = io_uring::opcode::WriteFixed::new(
            types::Fd(self.fd),
            buf.as_ptr(),
            buf.len() as u32,
        )
        .offset(offset)
        .buf_index(buf.registered_index() as u16)
        .build()
        .user_data(self.next_tag());
        
        unsafe { self.ring.submission().push(&sqe)?; }
        self.ring.submit_and_wait(1)?;
        
        let cqe = self.ring.completion().next().unwrap();
        if cqe.result() < 0 {
            return Err(io::Error::from_raw_os_error(-cqe.result()));
        }
        Ok(())
    }
    
    async fn discard(&self, offset: u64, len: u64) -> Result<()> {
        // BLKDISCARD ioctl for SAS SSDs, no-op for HDDs
        if self.device_type == DriveType::SasSsd {
            ioctl_blkdiscard(self.fd, offset, len)?;
        }
        Ok(())
    }
}
```

### 4.4 DMA Buffer Management

NVMe userspace I/O requires DMA-capable memory that the IOMMU can translate. StormBlock allocates a large pool of hugepage-backed memory at startup and manages it with a slab allocator:

```rust
pub struct DmaPool {
    vfio_container_fd: RawFd,
    
    /// 1GB hugepage regions for large buffers
    large_regions: Vec<DmaRegion>,
    
    /// 2MB hugepage regions for small buffers
    small_regions: Vec<DmaRegion>,
    
    /// Slab allocator for fixed-size DMA buffers
    slabs: HashMap<usize, SlabAllocator>,  // key: buffer size (4K, 64K, 1M, 4M)
}

struct DmaRegion {
    virt_addr: *mut u8,
    phys_addr: u64,     // IOVA assigned by VFIO
    size: usize,
    hugepage_size: usize,
}

impl DmaPool {
    pub fn new(container_fd: RawFd, total_size: usize) -> Result<Self> {
        // Map hugepages
        let virt = mmap(
            null_mut(), total_size,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS | MAP_HUGETLB | MAP_HUGE_1GB,
            -1, 0
        )?;
        
        // Register with VFIO IOMMU for DMA
        let dma_map = VfioDmaMap {
            vaddr: virt as u64,
            iova: virt as u64,  // Identity mapping for simplicity
            size: total_size as u64,
            flags: VFIO_DMA_MAP_FLAG_READ | VFIO_DMA_MAP_FLAG_WRITE,
        };
        ioctl(container_fd, VFIO_IOMMU_MAP_DMA, &dma_map)?;
        
        // Create slab allocators for common sizes
        let mut slabs = HashMap::new();
        slabs.insert(4096, SlabAllocator::new(virt, 4096, total_size / 4));
        slabs.insert(65536, SlabAllocator::new(/* ... */));
        slabs.insert(1 << 20, SlabAllocator::new(/* ... */));
        slabs.insert(4 << 20, SlabAllocator::new(/* ... */));
        
        Ok(Self { vfio_container_fd: container_fd, large_regions: vec![], small_regions: vec![], slabs })
    }
    
    /// Allocate a DMA buffer of the given size
    pub fn alloc(&self, size: usize) -> Result<DmaBuf> {
        let slab_size = size.next_power_of_two().max(4096);
        let slab = self.slabs.get(&slab_size)
            .ok_or(Error::BufferSizeTooLarge)?;
        let (ptr, iova) = slab.alloc()?;
        Ok(DmaBuf { ptr, len: size, iova, source: BufSource::Hugepage })
    }
}
```

---

## 5. RAID Engine

The RAID engine presents virtual devices (RAID arrays) to the volume manager. Each RAID array maps I/O across multiple physical `BlockDevice` instances with redundancy.

### 5.1 RAID Levels

```rust
pub enum RaidLevel {
    Raid0 { stripe_size: u32 },                      // Striping only (no redundancy)
    Raid1,                                            // Mirror (2+ copies)
    Raid5 { stripe_size: u32 },                      // Single parity, distributed
    Raid6 { stripe_size: u32 },                      // Dual parity (Reed-Solomon)
    Raid10 { mirror_count: u8, stripe_size: u32 },   // Striped mirrors
}

pub struct RaidArray {
    id: RaidArrayId,
    level: RaidLevel,
    members: Vec<Arc<dyn BlockDevice>>,
    
    /// On-disk superblock (sector 0 of each member)
    superblock: RaidSuperblock,
    
    /// Write-intent bitmap (tracks dirty stripes during writes)
    /// Stored on a small NVMe SSD for fast journal access
    journal: WriteIntentJournal,
    
    /// Parity computation engine (SIMD-accelerated)
    parity_engine: ParityEngine,
    
    /// Background rebuild state
    rebuild_state: Option<RebuildState>,
    
    /// Scrub state (background verification)
    scrub_state: Option<ScrubState>,
    
    // Derived geometry
    stripe_size: u32,
    data_disks: usize,
    usable_capacity: u64,
}

/// On-disk superblock (stored at sector 0 of each member drive)
#[repr(C)]
#[derive(Serialize, Deserialize)]
struct RaidSuperblock {
    magic: [u8; 8],              // "STRMBLK\0"
    version: u32,
    array_uuid: Uuid,
    member_index: u32,           // Position in array
    member_uuid: Uuid,           // Unique ID of this drive
    level: u8,
    member_count: u8,
    stripe_size: u32,
    data_offset: u64,            // Where data starts (after superblock + bitmap)
    data_size: u64,              // Usable data area
    create_time: u64,
    update_time: u64,
    state: RaidMemberState,      // Active, Degraded, Spare, Failed
    checksum: u32,               // CRC32C of superblock
}
```

### 5.2 SIMD Parity Computation

RAID 5 and RAID 6 parity calculations use SIMD instructions for throughput. On x86_64, this means AVX2 (256-bit) or AVX-512 (512-bit). On aarch64, NEON (128-bit).

```rust
pub struct ParityEngine {
    /// Runtime-detected SIMD capability
    simd_level: SimdLevel,
}

enum SimdLevel {
    Avx512,     // 512-bit, ~64 GB/s parity throughput per core
    Avx2,       // 256-bit, ~32 GB/s per core
    Neon,       // 128-bit, ~16 GB/s per core (ARM)
    Generic,    // Scalar fallback
}

impl ParityEngine {
    /// RAID 5: XOR across all data strips to produce parity
    pub fn compute_xor_parity(&self, data_strips: &[&[u8]], parity: &mut [u8]) {
        match self.simd_level {
            SimdLevel::Avx512 => unsafe { self.xor_avx512(data_strips, parity) },
            SimdLevel::Avx2 => unsafe { self.xor_avx2(data_strips, parity) },
            SimdLevel::Neon => unsafe { self.xor_neon(data_strips, parity) },
            SimdLevel::Generic => self.xor_generic(data_strips, parity),
        }
    }
    
    /// RAID 6: Galois field multiplication for Q syndrome
    /// P = XOR of all data strips
    /// Q = Σ (g^i × D_i) in GF(2^8)
    pub fn compute_raid6_parity(
        &self, data_strips: &[&[u8]], p_parity: &mut [u8], q_parity: &mut [u8]
    ) {
        // P parity: simple XOR
        self.compute_xor_parity(data_strips, p_parity);
        
        // Q parity: GF(2^8) multiplication with generator coefficients
        match self.simd_level {
            SimdLevel::Avx512 => unsafe {
                self.gf_mul_avx512(data_strips, q_parity)
            },
            SimdLevel::Avx2 => unsafe {
                // VPSHUFB-based GF multiplication (split lookup table approach)
                self.gf_mul_avx2(data_strips, q_parity)
            },
            SimdLevel::Neon => unsafe {
                self.gf_mul_neon(data_strips, q_parity)
            },
            SimdLevel::Generic => {
                // Lookup table fallback
                self.gf_mul_generic(data_strips, q_parity)
            },
        }
    }
    
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn xor_avx2(&self, data_strips: &[&[u8]], parity: &mut [u8]) {
        use std::arch::x86_64::*;
        let len = parity.len();
        let mut offset = 0;
        
        while offset + 32 <= len {
            let mut acc = _mm256_loadu_si256(
                data_strips[0][offset..].as_ptr() as *const __m256i
            );
            for strip in &data_strips[1..] {
                let d = _mm256_loadu_si256(strip[offset..].as_ptr() as *const __m256i);
                acc = _mm256_xor_si256(acc, d);
            }
            _mm256_storeu_si256(parity[offset..].as_mut_ptr() as *mut __m256i, acc);
            offset += 32;
        }
        // Handle remainder with scalar XOR
    }
    
    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    unsafe fn xor_neon(&self, data_strips: &[&[u8]], parity: &mut [u8]) {
        use std::arch::aarch64::*;
        let len = parity.len();
        let mut offset = 0;
        
        while offset + 16 <= len {
            let mut acc = vld1q_u8(data_strips[0][offset..].as_ptr());
            for strip in &data_strips[1..] {
                let d = vld1q_u8(strip[offset..].as_ptr());
                acc = veorq_u8(acc, d);
            }
            vst1q_u8(parity[offset..].as_mut_ptr(), acc);
            offset += 16;
        }
    }
}
```

### 5.3 Write Path (Full-Stripe and Partial-Stripe)

```rust
impl RaidArray {
    /// RAID 5/6 write — handles both full-stripe and partial-stripe (read-modify-write)
    pub async fn write(&self, offset: u64, data: &[u8]) -> Result<()> {
        let stripe = self.offset_to_stripe(offset);
        let strip_offset = offset % self.stripe_size as u64;
        
        if data.len() == self.stripe_data_size() && strip_offset == 0 {
            // Full-stripe write: compute parity from new data, write everything
            self.full_stripe_write(stripe, data).await
        } else {
            // Partial-stripe write: read-modify-write
            self.partial_stripe_write(stripe, strip_offset, data).await
        }
    }
    
    async fn full_stripe_write(&self, stripe: u64, data: &[u8]) -> Result<()> {
        // 1. Mark stripe dirty in write-intent journal
        self.journal.mark_dirty(stripe).await?;
        
        // 2. Split data into per-disk strips
        let strips: Vec<&[u8]> = data.chunks(self.stripe_size as usize).collect();
        
        // 3. Compute parity (SIMD)
        let mut parity = vec![0u8; self.stripe_size as usize];
        self.parity_engine.compute_xor_parity(&strips, &mut parity);
        
        // 4. Write all strips + parity in parallel
        let mut futures = Vec::new();
        let parity_disk = self.parity_disk_for_stripe(stripe);
        
        for (i, strip) in strips.iter().enumerate() {
            let disk_idx = self.data_disk_index(stripe, i);
            let disk_offset = self.stripe_to_disk_offset(stripe);
            futures.push(self.members[disk_idx].write(disk_offset, strip));
        }
        futures.push(self.members[parity_disk].write(
            self.stripe_to_disk_offset(stripe), &parity
        ));
        
        // Wait for all writes
        futures::future::try_join_all(futures).await?;
        
        // 5. Clear dirty bit
        self.journal.mark_clean(stripe).await?;
        
        Ok(())
    }
    
    async fn partial_stripe_write(
        &self, stripe: u64, offset_in_stripe: u64, data: &[u8]
    ) -> Result<()> {
        // Read-Modify-Write:
        // 1. Read old data strip + old parity
        // 2. New parity = old_parity XOR old_data XOR new_data
        // 3. Write new data + new parity
        
        self.journal.mark_dirty(stripe).await?;
        
        let strip_idx = (offset_in_stripe / self.stripe_size as u64) as usize;
        let disk_idx = self.data_disk_index(stripe, strip_idx);
        let parity_disk = self.parity_disk_for_stripe(stripe);
        let disk_offset = self.stripe_to_disk_offset(stripe);
        
        // Read old data and old parity in parallel
        let mut old_data = self.dma_pool.alloc(self.stripe_size as usize)?;
        let mut old_parity = self.dma_pool.alloc(self.stripe_size as usize)?;
        
        futures::try_join!(
            self.members[disk_idx].read(disk_offset, &mut old_data),
            self.members[parity_disk].read(disk_offset, &mut old_parity),
        )?;
        
        // Compute new parity: P_new = P_old XOR D_old XOR D_new
        let mut new_parity = old_parity.clone();
        self.parity_engine.xor_in_place(&mut new_parity, &old_data);
        
        // Overlay new data onto old data strip
        old_data.copy_from_slice_at(offset_in_stripe as usize % self.stripe_size as usize, data);
        self.parity_engine.xor_in_place(&mut new_parity, &old_data);
        
        // Write new data + new parity in parallel
        futures::try_join!(
            self.members[disk_idx].write(disk_offset, &old_data),
            self.members[parity_disk].write(disk_offset, &new_parity),
        )?;
        
        self.journal.mark_clean(stripe).await?;
        Ok(())
    }
}
```

### 5.4 Write-Intent Journal

A small journal (write-intent bitmap) tracks which stripes have in-flight writes. On crash recovery, only dirty stripes need parity verification. Stored on the fastest available media (NVMe SSD) for minimal write latency:

```rust
pub struct WriteIntentJournal {
    /// Bitmap: 1 bit per stripe. 1 = dirty (write in progress)
    bitmap: MmapMut,           // Memory-mapped file on fast SSD
    bitmap_path: PathBuf,
    stripe_count: u64,
    
    /// Persistent log for crash recovery
    log_fd: RawFd,
}

impl WriteIntentJournal {
    pub async fn mark_dirty(&self, stripe: u64) -> Result<()> {
        let byte_idx = (stripe / 8) as usize;
        let bit_idx = (stripe % 8) as u8;
        self.bitmap[byte_idx] |= 1 << bit_idx;
        // fsync the bitmap page — we need this to survive crash
        // Only sync the single page containing this bit
        msync(&self.bitmap[byte_idx..byte_idx+1], MS_SYNC)?;
        Ok(())
    }
    
    pub async fn mark_clean(&self, stripe: u64) -> Result<()> {
        let byte_idx = (stripe / 8) as usize;
        let bit_idx = (stripe % 8) as u8;
        self.bitmap[byte_idx] &= !(1 << bit_idx);
        // Clean marks don't need immediate sync — worst case we re-verify a clean stripe
        Ok(())
    }
    
    /// On startup: return list of dirty stripes that need parity verification
    pub fn dirty_stripes(&self) -> Vec<u64> {
        let mut dirty = Vec::new();
        for (byte_idx, &byte) in self.bitmap.iter().enumerate() {
            if byte != 0 {
                for bit in 0..8 {
                    if byte & (1 << bit) != 0 {
                        dirty.push(byte_idx as u64 * 8 + bit as u64);
                    }
                }
            }
        }
        dirty
    }
}
```

### 5.5 Background Rebuild & Scrub

```rust
pub struct RebuildState {
    source_disks: Vec<usize>,      // Healthy member indices
    target_disk: usize,            // New/replacement disk index
    progress_stripe: u64,          // Current position
    total_stripes: u64,
    rate_limit: RateLimiter,       // Don't starve foreground I/O
    started_at: Instant,
}

impl RaidArray {
    /// Rebuild a failed/replaced disk from remaining members
    pub async fn rebuild(&self, target_disk: usize) -> Result<()> {
        let state = RebuildState {
            source_disks: (0..self.members.len())
                .filter(|&i| i != target_disk)
                .collect(),
            target_disk,
            progress_stripe: 0,
            total_stripes: self.total_stripes(),
            rate_limit: RateLimiter::new(200 << 20),  // 200 MB/s max
            started_at: Instant::now(),
        };
        
        for stripe in 0..state.total_stripes {
            state.rate_limit.wait().await;
            
            // Read all surviving strips for this stripe
            let mut strips = Vec::new();
            for &disk in &state.source_disks {
                let mut buf = self.dma_pool.alloc(self.stripe_size as usize)?;
                self.members[disk].read(
                    self.stripe_to_disk_offset(stripe), &mut buf
                ).await?;
                strips.push(buf);
            }
            
            // Reconstruct missing strip via XOR (RAID 5) or GF math (RAID 6)
            let mut rebuilt = self.dma_pool.alloc(self.stripe_size as usize)?;
            self.parity_engine.reconstruct(&strips, &mut rebuilt)?;
            
            // Write to replacement disk
            self.members[target_disk].write(
                self.stripe_to_disk_offset(stripe), &rebuilt
            ).await?;
            
            // Update progress
            if stripe % 1000 == 0 {
                metrics::gauge!("raid.rebuild.progress_pct",
                    (stripe as f64 / state.total_stripes as f64) * 100.0);
            }
        }
        
        Ok(())
    }
    
    /// Background scrub: verify parity matches data for all stripes
    pub async fn scrub(&self) -> Result<ScrubReport> {
        let mut mismatches = 0u64;
        let mut repaired = 0u64;
        
        for stripe in 0..self.total_stripes() {
            // Read all strips including parity
            let mut all_strips = Vec::new();
            for disk in 0..self.members.len() {
                let mut buf = self.dma_pool.alloc(self.stripe_size as usize)?;
                self.members[disk].read(
                    self.stripe_to_disk_offset(stripe), &mut buf
                ).await?;
                all_strips.push(buf);
            }
            
            // Verify parity
            if !self.parity_engine.verify(&all_strips)? {
                mismatches += 1;
                // Attempt repair using redundant data
                if self.attempt_repair(stripe, &all_strips).await? {
                    repaired += 1;
                }
            }
            
            // Rate limit to avoid impacting foreground I/O
            tokio::time::sleep(Duration::from_micros(100)).await;
        }
        
        Ok(ScrubReport { total_stripes: self.total_stripes(), mismatches, repaired })
    }
}
```

---

## 6. Volume Manager

The volume manager carves RAID arrays into logical volumes with thin provisioning, copy-on-write snapshots, and extent-based allocation.

### 6.1 Extent Allocator

```rust
/// Manages free space across one or more RAID arrays
pub struct ExtentAllocator {
    /// Free extent bitmap per RAID array
    bitmaps: HashMap<RaidArrayId, ExtentBitmap>,
    
    /// Extent size: 4MB (tunable)
    extent_size: u64,
}

pub struct ExtentBitmap {
    array_id: RaidArrayId,
    total_extents: u64,
    free_extents: u64,
    
    /// Bitmap: 1 = free, 0 = allocated
    /// Stored on-disk as part of volume manager metadata
    bitmap: BitVec,
    
    /// Free extent hint (start searching here)
    hint: u64,
}

impl ExtentAllocator {
    /// Allocate N contiguous extents on the given array (best-effort contiguity)
    pub fn allocate(
        &mut self, array_id: &RaidArrayId, count: u64
    ) -> Result<Vec<Extent>> {
        let bitmap = self.bitmaps.get_mut(array_id)
            .ok_or(Error::ArrayNotFound)?;
        
        let mut allocated = Vec::new();
        let mut search_pos = bitmap.hint;
        let mut remaining = count;
        
        while remaining > 0 {
            // Find next free extent
            match bitmap.find_free_run(search_pos, remaining) {
                Some((start, run_len)) => {
                    let alloc_len = run_len.min(remaining);
                    for i in 0..alloc_len {
                        bitmap.bitmap.set((start + i) as usize, false);  // Mark used
                    }
                    bitmap.free_extents -= alloc_len;
                    allocated.push(Extent {
                        array_id: *array_id,
                        start_extent: start,
                        count: alloc_len,
                        offset_bytes: start * self.extent_size,
                        size_bytes: alloc_len * self.extent_size,
                    });
                    remaining -= alloc_len;
                    search_pos = start + alloc_len;
                },
                None => {
                    // Wrap around or fail
                    if search_pos > 0 {
                        search_pos = 0;
                    } else {
                        // Free what we allocated and fail
                        for ext in &allocated {
                            self.free_extent(ext);
                        }
                        return Err(Error::OutOfSpace);
                    }
                }
            }
        }
        
        bitmap.hint = search_pos;
        Ok(allocated)
    }
    
    pub fn free_extent(&mut self, extent: &Extent) {
        if let Some(bitmap) = self.bitmaps.get_mut(&extent.array_id) {
            for i in 0..extent.count {
                bitmap.bitmap.set((extent.start_extent + i) as usize, true);
            }
            bitmap.free_extents += extent.count;
        }
    }
}
```

### 6.2 Thin Volumes & Snapshots

```rust
/// Logical volume with thin provisioning and COW snapshots
pub struct ThinVolume {
    id: VolumeId,
    name: String,
    
    /// Virtual size (what the initiator sees)
    virtual_size: u64,
    
    /// Actual allocated extents (grows on write)
    allocated: u64,
    
    /// Extent map: virtual block → physical extent
    /// B-tree stored in volume manager metadata area
    extent_map: BTreeMap<u64, PhysicalExtent>,
    
    /// Parent snapshot (for COW)
    parent: Option<VolumeId>,
    
    /// Child snapshots
    children: Vec<VolumeId>,
    
    /// NVMe-oF subsystem NQN or iSCSI target IQN
    export: Option<ExportConfig>,
    
    /// Access control
    acl: Vec<InitiatorAcl>,
}

#[derive(Clone)]
struct PhysicalExtent {
    array_id: RaidArrayId,
    offset: u64,
    length: u64,
    ref_count: u32,     // >1 means shared with snapshot
}

impl ThinVolume {
    /// Read: follow extent map, fall through to parent for unmapped regions
    pub async fn read(&self, offset: u64, buf: &mut DmaBuf) -> Result<()> {
        match self.extent_map.get_floor(&(offset / self.extent_size)) {
            Some((_vblock, phys)) => {
                // Mapped — read from physical extent
                let raid = self.get_raid_array(&phys.array_id)?;
                raid.read(phys.offset + (offset % self.extent_size), buf).await
            }
            None => {
                // Unmapped — check parent snapshot
                if let Some(parent_id) = &self.parent {
                    let parent = self.volume_mgr.get_volume(parent_id)?;
                    parent.read(offset, buf).await
                } else {
                    // No parent, unallocated region — return zeros
                    buf.fill(0);
                    Ok(())
                }
            }
        }
    }
    
    /// Write: COW — if extent is shared with snapshot, copy first
    pub async fn write(&self, offset: u64, data: &DmaBuf) -> Result<()> {
        let vblock = offset / self.extent_size;
        
        match self.extent_map.get(&vblock) {
            Some(phys) if phys.ref_count > 1 => {
                // Shared with snapshot — Copy-on-Write
                let new_extent = self.allocator.allocate(&phys.array_id, 1)?;
                
                // Copy old data to new extent
                let mut tmp = self.dma_pool.alloc(self.extent_size as usize)?;
                let raid = self.get_raid_array(&phys.array_id)?;
                raid.read(phys.offset, &mut tmp).await?;
                raid.write(new_extent[0].offset_bytes, &tmp).await?;
                
                // Write new data to new extent
                let write_offset = offset % self.extent_size;
                raid.write(new_extent[0].offset_bytes + write_offset, data).await?;
                
                // Update extent map (atomic via metadata transaction)
                self.extent_map.insert(vblock, PhysicalExtent {
                    array_id: phys.array_id,
                    offset: new_extent[0].offset_bytes,
                    length: self.extent_size,
                    ref_count: 1,
                });
                
                // Decrement ref_count on old extent
                // (freed when ref_count reaches 0)
                self.decrement_refcount(&phys)?;
                
                Ok(())
            }
            Some(phys) => {
                // Exclusively owned — write in place
                let raid = self.get_raid_array(&phys.array_id)?;
                raid.write(phys.offset + (offset % self.extent_size), data).await
            }
            None => {
                // Unallocated — allocate new extent
                let array_id = self.select_array_for_write()?;
                let new_extent = self.allocator.allocate(&array_id, 1)?;
                let raid = self.get_raid_array(&array_id)?;
                
                // Zero-fill the extent first (thin provisioning)
                let zeros = self.dma_pool.alloc_zeroed(self.extent_size as usize)?;
                raid.write(new_extent[0].offset_bytes, &zeros).await?;
                
                // Write actual data
                raid.write(new_extent[0].offset_bytes + (offset % self.extent_size), data).await?;
                
                self.extent_map.insert(vblock, PhysicalExtent {
                    array_id,
                    offset: new_extent[0].offset_bytes,
                    length: self.extent_size,
                    ref_count: 1,
                });
                
                self.allocated += self.extent_size;
                Ok(())
            }
        }
    }
    
    /// Create snapshot: clone extent map, increment all ref_counts
    pub fn snapshot(&self, snap_name: &str) -> Result<ThinVolume> {
        let mut snap_map = self.extent_map.clone();
        
        // Increment ref_count on all extents (now shared)
        for (_, phys) in snap_map.iter_mut() {
            phys.ref_count += 1;
        }
        // Also increment in our own map
        for (_, phys) in self.extent_map.iter_mut() {
            phys.ref_count += 1;
        }
        
        let snap = ThinVolume {
            id: VolumeId::new(),
            name: snap_name.to_string(),
            virtual_size: self.virtual_size,
            allocated: self.allocated,  // Same physical space (shared)
            extent_map: snap_map,
            parent: None,               // Snapshot is independent
            children: vec![],
            export: None,
            acl: vec![],
        };
        
        Ok(snap)
    }
}
```

### 6.3 Volume Manager Metadata Persistence

Volume manager metadata (extent maps, RAID superblocks, volume definitions) is stored on a dedicated metadata partition on the fastest available drive. Double-buffered for crash safety:

```rust
pub struct VolumeManagerMeta {
    /// Primary metadata location (NVMe SSD partition)
    primary_path: PathBuf,
    
    /// Secondary copy (different drive for redundancy)
    secondary_path: PathBuf,
    
    /// In-memory state
    volumes: HashMap<VolumeId, ThinVolume>,
    raid_arrays: HashMap<RaidArrayId, RaidArray>,
    allocator: ExtentAllocator,
    
    /// Transaction log for atomic metadata updates
    txn_log: TransactionLog,
}

impl VolumeManagerMeta {
    /// Atomic metadata update: write to log first, then apply
    pub async fn commit(&mut self, ops: Vec<MetaOp>) -> Result<()> {
        // 1. Write to transaction log (append-only, O_DSYNC)
        let txn_id = self.txn_log.append(&ops).await?;
        
        // 2. Apply operations to in-memory state
        for op in &ops {
            self.apply_op(op)?;
        }
        
        // 3. Periodically checkpoint full state to primary + secondary
        if self.txn_log.should_checkpoint() {
            self.checkpoint().await?;
        }
        
        Ok(())
    }
    
    async fn checkpoint(&self) -> Result<()> {
        let state = bincode::serialize(&self.volumes)?;
        
        // Write primary
        atomic_write(&self.primary_path, &state).await?;
        // Write secondary
        atomic_write(&self.secondary_path, &state).await?;
        // Truncate transaction log
        self.txn_log.truncate().await?;
        
        Ok(())
    }
}
```

---

## 7. Target Protocol Layer

### 7.1 NVMe-oF/TCP Target

The primary high-performance target protocol. Implements the NVMe-oF TCP transport binding per NVMe specification.

```rust
pub struct NvmeofTcpTarget {
    /// Listening socket on port 4420
    listener: TcpListener,
    
    /// io_uring instance for network I/O
    ring: IoUring,
    
    /// Registered subsystems (NQNs)
    subsystems: HashMap<String, NvmeSubsystem>,
    
    /// Active connections (controllers)
    controllers: Vec<NvmeofController>,
}

struct NvmeSubsystem {
    nqn: String,                        // e.g., "nqn.2026-01.com.stormblock:vol-001"
    namespaces: Vec<NvmeNamespaceExport>,
    allowed_hosts: Vec<String>,         // Host NQNs (ACL)
}

struct NvmeNamespaceExport {
    nsid: u32,
    volume: Arc<ThinVolume>,
    block_size: u32,                    // 512 or 4096
}

struct NvmeofController {
    /// TCP connection fd
    conn_fd: RawFd,
    
    /// Controller ID
    cntlid: u16,
    
    /// I/O queues (one per host-side queue)
    queues: Vec<NvmeofQueue>,
    
    /// Host NQN
    host_nqn: String,
    
    /// Capsule alignment data buffer
    recv_buf: DmaBuf,
}

/// NVMe-oF/TCP PDU types
#[repr(u8)]
enum PduType {
    ICReq = 0x00,       // Initialize Connection Request
    ICResp = 0x01,      // Initialize Connection Response
    H2CTermReq = 0x02,  // Host to Controller Terminate
    C2HTermReq = 0x03,  // Controller to Host Terminate
    CapsuleCmd = 0x04,  // NVMe command capsule
    CapsuleResp = 0x05, // NVMe response capsule
    H2CData = 0x06,     // Host to Controller data
    C2HData = 0x07,     // Controller to Host data
    R2T = 0x09,         // Ready to Transfer
}

impl NvmeofTcpTarget {
    pub async fn run(&mut self) -> Result<()> {
        loop {
            // Accept new connections via io_uring multishot accept
            let sqe = io_uring::opcode::AcceptMulti::new(
                types::Fd(self.listener.as_raw_fd())
            ).build();
            
            unsafe { self.ring.submission().push(&sqe)?; }
            self.ring.submit()?;
            
            // Process completions (new connections + data from existing)
            while let Some(cqe) = self.ring.completion().next() {
                match cqe.user_data() {
                    ACCEPT_TAG => self.handle_new_connection(cqe.result()).await?,
                    tag => self.handle_io(tag, &cqe).await?,
                }
            }
        }
    }
    
    async fn handle_capsule_cmd(
        &mut self, ctrl: &mut NvmeofController, pdu: &CapsuleCmdPdu
    ) -> Result<()> {
        let cmd = &pdu.nvme_cmd;
        
        match cmd.opcode {
            NVME_OPC_READ => {
                let nsid = cmd.nsid;
                let lba = ((cmd.cdw11 as u64) << 32) | cmd.cdw10 as u64;
                let block_count = (cmd.cdw12 & 0xFFFF) as u64 + 1;
                let ns = ctrl.get_namespace(nsid)?;
                let byte_offset = lba * ns.block_size as u64;
                let byte_count = block_count * ns.block_size as u64;
                
                // Read from volume
                let mut buf = self.dma_pool.alloc(byte_count as usize)?;
                ns.volume.read(byte_offset, &mut buf).await?;
                
                // Send C2H Data PDU followed by Capsule Response
                self.send_c2h_data(ctrl, cmd.command_id, &buf).await?;
                self.send_capsule_resp(ctrl, cmd.command_id, NvmeStatus::Success).await?;
            }
            
            NVME_OPC_WRITE => {
                let nsid = cmd.nsid;
                let lba = ((cmd.cdw11 as u64) << 32) | cmd.cdw10 as u64;
                let block_count = (cmd.cdw12 & 0xFFFF) as u64 + 1;
                let ns = ctrl.get_namespace(nsid)?;
                let byte_offset = lba * ns.block_size as u64;
                let byte_count = block_count * ns.block_size as u64;
                
                if pdu.has_inline_data() {
                    // Data included in capsule — small writes
                    ns.volume.write(byte_offset, &pdu.inline_data).await?;
                } else {
                    // Large write: send R2T, receive H2C Data
                    self.send_r2t(ctrl, cmd.command_id, byte_count).await?;
                    let data = self.recv_h2c_data(ctrl, byte_count).await?;
                    ns.volume.write(byte_offset, &data).await?;
                }
                
                self.send_capsule_resp(ctrl, cmd.command_id, NvmeStatus::Success).await?;
            }
            
            NVME_OPC_FLUSH => {
                let ns = ctrl.get_namespace(cmd.nsid)?;
                ns.volume.flush().await?;
                self.send_capsule_resp(ctrl, cmd.command_id, NvmeStatus::Success).await?;
            }
            
            NVME_OPC_DATASET_MGMT => {
                // TRIM/UNMAP (DSM deallocate)
                self.handle_dsm(ctrl, cmd).await?;
            }
            
            _ => {
                self.send_capsule_resp(
                    ctrl, cmd.command_id, NvmeStatus::InvalidOpcode
                ).await?;
            }
        }
        Ok(())
    }
    
    /// Zero-copy send via io_uring SEND_ZC
    async fn send_c2h_data(
        &self, ctrl: &NvmeofController, cmd_id: u16, data: &DmaBuf
    ) -> Result<()> {
        // Build C2H Data PDU header
        let header = C2HDataPdu {
            pdu_type: PduType::C2HData,
            flags: C2H_DATA_FLAG_LAST_PDU,
            pdu_length: (size_of::<C2HDataPdu>() + data.len()) as u32,
            command_id: cmd_id,
            data_offset: size_of::<C2HDataPdu>() as u32,
            data_length: data.len() as u32,
        };
        
        // Send header + data with zero-copy
        let sqe = io_uring::opcode::SendZc::new(
            types::Fd(ctrl.conn_fd),
            header.as_bytes(),
        ).build();
        unsafe { self.ring.submission().push(&sqe)?; }
        
        let sqe = io_uring::opcode::SendZc::new(
            types::Fd(ctrl.conn_fd),
            data.as_slice(),
        ).build();
        unsafe { self.ring.submission().push(&sqe)?; }
        
        self.ring.submit_and_wait(2)?;
        Ok(())
    }
}
```

### 7.2 iSCSI Target (RFC 7143)

Full iSCSI target implementation for legacy initiator compatibility. More complex protocol than NVMe-oF but essential for broad device support.

```rust
pub struct IscsiTarget {
    listener: TcpListener,
    ring: IoUring,
    
    /// iSCSI target configuration
    target_iqns: HashMap<String, IscsiTargetConfig>,
    
    /// Active sessions
    sessions: HashMap<SessionId, IscsiSession>,
}

struct IscsiTargetConfig {
    iqn: String,                      // e.g., "iqn.2026-01.com.stormblock:vol-001"
    luns: Vec<IscsiLun>,
    chap_credentials: Option<ChapAuth>,
    allowed_initiators: Vec<String>,   // Initiator IQNs
}

struct IscsiLun {
    lun_id: u64,
    volume: Arc<ThinVolume>,
    block_size: u32,
    read_only: bool,
}

struct IscsiSession {
    conn_fd: RawFd,
    isid: [u8; 6],                    // Initiator Session ID
    tsih: u16,                        // Target Session Handle
    initiator_iqn: String,
    
    // Negotiated parameters
    max_recv_data_segment: u32,       // Default: 8192
    max_burst_length: u32,            // Default: 262144
    first_burst_length: u32,
    immediate_data: bool,
    initial_r2t: bool,
    
    // Sequence numbers
    cmd_sn: u32,
    exp_stat_sn: u32,
    max_cmd_sn: u32,
    
    // Error recovery level (0, 1, or 2)
    error_recovery_level: u8,
}

impl IscsiTarget {
    /// iSCSI login phase: negotiate parameters, authenticate
    async fn handle_login(
        &mut self, conn_fd: RawFd, pdu: &LoginRequestPdu
    ) -> Result<SessionId> {
        // Phase 1: Security negotiation (CHAP if configured)
        if let Some(chap) = &self.get_target(&pdu.target_iqn)?.chap_credentials {
            self.chap_authenticate(conn_fd, chap, pdu).await?;
        }
        
        // Phase 2: Operational parameter negotiation
        let params = self.negotiate_params(conn_fd, pdu).await?;
        
        let session = IscsiSession {
            conn_fd,
            isid: pdu.isid,
            tsih: self.allocate_tsih(),
            initiator_iqn: pdu.initiator_iqn.clone(),
            max_recv_data_segment: params.max_recv_data_segment,
            max_burst_length: params.max_burst_length,
            first_burst_length: params.first_burst_length,
            immediate_data: params.immediate_data,
            initial_r2t: params.initial_r2t,
            cmd_sn: pdu.cmd_sn,
            exp_stat_sn: 0,
            max_cmd_sn: pdu.cmd_sn + 32,  // Command window of 32
            error_recovery_level: params.error_recovery_level,
        };
        
        let session_id = SessionId::new();
        self.sessions.insert(session_id, session);
        
        // Send Login Response (Final)
        self.send_login_response(conn_fd, &session, LoginStatus::Success).await?;
        
        Ok(session_id)
    }
    
    /// Handle SCSI command (wrapped in iSCSI PDU)
    async fn handle_scsi_cmd(
        &mut self, session: &mut IscsiSession, pdu: &ScsiCmdPdu
    ) -> Result<()> {
        let lun = self.resolve_lun(session, pdu.lun)?;
        
        match pdu.cdb[0] {
            SCSI_READ_10 | SCSI_READ_16 => {
                let (lba, blocks) = parse_read_cdb(&pdu.cdb);
                let byte_offset = lba * lun.block_size as u64;
                let byte_count = blocks as u64 * lun.block_size as u64;
                
                let mut buf = self.dma_pool.alloc(byte_count as usize)?;
                lun.volume.read(byte_offset, &mut buf).await?;
                
                self.send_data_in(session, pdu.initiator_task_tag, &buf).await?;
                self.send_scsi_response(
                    session, pdu.initiator_task_tag, ScsiStatus::Good
                ).await?;
            }
            
            SCSI_WRITE_10 | SCSI_WRITE_16 => {
                let (lba, blocks) = parse_write_cdb(&pdu.cdb);
                let byte_offset = lba * lun.block_size as u64;
                let byte_count = blocks as u64 * lun.block_size as u64;
                
                // Receive data (immediate data in PDU + solicited via R2T)
                let data = if pdu.has_data() && session.immediate_data {
                    pdu.data_segment.clone()
                } else {
                    self.solicit_data(session, pdu.initiator_task_tag, byte_count).await?
                };
                
                lun.volume.write(byte_offset, &data).await?;
                self.send_scsi_response(
                    session, pdu.initiator_task_tag, ScsiStatus::Good
                ).await?;
            }
            
            SCSI_INQUIRY => self.handle_inquiry(session, pdu).await?,
            SCSI_READ_CAPACITY_16 => self.handle_read_capacity(session, pdu, lun).await?,
            SCSI_REPORT_LUNS => self.handle_report_luns(session, pdu).await?,
            SCSI_MODE_SENSE_6 => self.handle_mode_sense(session, pdu).await?,
            SCSI_UNMAP => self.handle_unmap(session, pdu, lun).await?,
            SCSI_WRITE_SAME_16 => self.handle_write_same(session, pdu, lun).await?,
            
            // VAAI offloads
            SCSI_XCOPY => self.handle_xcopy(session, pdu).await?,
            SCSI_ATS => self.handle_compare_and_write(session, pdu, lun).await?,
            
            _ => {
                self.send_scsi_response(
                    session, pdu.initiator_task_tag,
                    ScsiStatus::CheckCondition,
                ).await?;
            }
        }
        
        Ok(())
    }
}
```

---

## 8. I/O Pipeline & Threading Model

### 8.1 Reactor Architecture

StormBlock uses a per-core reactor model. Each CPU core runs an independent event loop that handles both NVMe completions (via MMIO polling) and network I/O (via io_uring). No cross-core locking on the hot path.

```
Core 0-1: Management (REST API, metrics, background tasks)

Core 2-N: I/O reactors (one per core)
  ┌─────────────────────────────────────────┐
  │  Reactor Loop (pinned to core)           │
  │                                          │
  │  1. Poll NVMe CQ doorbells (MMIO read)   │
  │     → Process completions                │
  │     → Complete pending target responses   │
  │                                          │
  │  2. io_uring_enter() (non-blocking)      │
  │     → New TCP connections                │
  │     → Incoming NVMe-oF/iSCSI PDUs        │
  │     → SAS I/O completions                │
  │                                          │
  │  3. Process incoming commands             │
  │     → Parse PDU                          │
  │     → Volume lookup                       │
  │     → RAID stripe calculation             │
  │     → Submit NVMe/SAS I/O                │
  │                                          │
  │  4. Send responses for completed I/O     │
  │     → Build response PDU                 │
  │     → io_uring send (zero-copy)          │
  │                                          │
  │  Loop with no syscalls in steady state    │
  │  (NVMe: MMIO polling, SAS: SQPOLL)      │
  └─────────────────────────────────────────┘
```

```rust
pub struct IoReactor {
    core_id: usize,
    
    /// io_uring for network + SAS I/O
    ring: IoUring,
    
    /// NVMe queue pairs assigned to this core
    nvme_queues: Vec<IoQueuePairRef>,
    
    /// Active target connections handled by this core
    connections: Vec<TargetConnection>,
    
    /// Pending I/O operations (waiting for drive completion)
    pending: HashMap<u64, PendingIo>,
}

impl IoReactor {
    pub fn run(&mut self) -> ! {
        // Pin to core
        core_affinity::set_for_current(CoreId(self.core_id));
        
        loop {
            // 1. Poll NVMe completions (no syscall — MMIO read)
            for qp in &self.nvme_queues {
                let completions = qp.poll_completions();
                for comp in completions {
                    self.handle_nvme_completion(comp);
                }
            }
            
            // 2. Submit/complete io_uring (network + SAS)
            // SQPOLL mode: no syscall needed if kernel thread is active
            let _ = self.ring.submit();
            
            while let Some(cqe) = self.ring.completion().next() {
                self.handle_uring_completion(cqe);
            }
            
            // 3. Process new commands from connections
            for conn in &mut self.connections {
                while let Some(cmd) = conn.try_recv_command() {
                    self.dispatch_command(conn, cmd);
                }
            }
        }
    }
}
```

### 8.2 Connection Affinity

New TCP connections are distributed across I/O cores using SO_INCOMING_CPU or explicit steering. Once assigned, a connection stays on its core for its lifetime — no cross-core migration:

```rust
impl NvmeofTcpTarget {
    fn assign_connection_to_core(&self, conn_fd: RawFd) -> usize {
        // Use SO_INCOMING_CPU to match the NIC's RSS steering
        let cpu = getsockopt(conn_fd, SOL_SOCKET, SO_INCOMING_CPU)?;
        
        // Map to our I/O reactor cores (cores 2+)
        let io_core = (cpu as usize).max(2);
        if io_core < self.reactors.len() + 2 {
            io_core
        } else {
            // Round-robin fallback
            self.next_core.fetch_add(1, Ordering::Relaxed) % self.reactors.len() + 2
        }
    }
}
```

---

## 9. Management Plane

### 9.1 REST API

```rust
pub fn management_routes() -> Router {
    Router::new()
        // Health & discovery
        .route("/api/v1/health", get(health_handler))
        .route("/api/v1/metrics", get(prometheus_metrics))
        .route("/api/v1/info", get(system_info))
        
        // Drive management
        .route("/api/v1/drives", get(list_drives))
        .route("/api/v1/drives/:id", get(drive_detail))
        .route("/api/v1/drives/:id/smart", get(drive_smart))
        .route("/api/v1/drives/:id/identify", post(drive_identify))  // Blink LED
        
        // RAID arrays
        .route("/api/v1/arrays", get(list_arrays).post(create_array))
        .route("/api/v1/arrays/:id", get(array_detail).delete(destroy_array))
        .route("/api/v1/arrays/:id/rebuild", post(start_rebuild))
        .route("/api/v1/arrays/:id/scrub", post(start_scrub))
        .route("/api/v1/arrays/:id/add-spare", post(add_spare))
        
        // Volumes
        .route("/api/v1/volumes", get(list_volumes).post(create_volume))
        .route("/api/v1/volumes/:id", get(volume_detail).delete(destroy_volume))
        .route("/api/v1/volumes/:id/resize", post(resize_volume))
        .route("/api/v1/volumes/:id/snapshot", post(create_snapshot))
        .route("/api/v1/volumes/:id/clone", post(clone_volume))
        .route("/api/v1/volumes/:id/stats", get(volume_stats))
        
        // Target exports
        .route("/api/v1/exports/nvmeof", get(list_nvmeof).post(create_nvmeof))
        .route("/api/v1/exports/nvmeof/:nqn", delete(remove_nvmeof))
        .route("/api/v1/exports/iscsi", get(list_iscsi).post(create_iscsi))
        .route("/api/v1/exports/iscsi/:iqn", delete(remove_iscsi))
        
        // StormFS integration
        .route("/api/v1/stormfs/allocate", post(stormfs_allocate))
        .route("/api/v1/stormfs/deallocate", post(stormfs_deallocate))
        .route("/api/v1/stormfs/extent-map/:vol", get(stormfs_extent_map))
        .route("/api/v1/stormfs/trim", post(stormfs_trim))
        
        // Topology (for StormFS tiering)
        .route("/api/v1/topology/register", post(topology_register))
        .route("/api/v1/topology/map", get(topology_map))
        .route("/api/v1/replication/copy", post(replication_copy))
        .route("/api/v1/replication/status/:id", get(replication_status))
        .route("/api/v1/scrub/verify", post(scrub_verify))
        
        // Cluster
        .route("/api/v1/cluster/peers", get(list_peers))
        .route("/api/v1/cluster/join", post(join_cluster))
}
```

### 9.2 Configuration

```toml
# stormblock.toml

[system]
hostname = "stormblock-nvme-1"
management_port = 8443
management_tls = true

[topology]
site = "nashville"
rack = "rack-a"
tier = "tier0"                    # tier0, tier1, or tier2
architecture = "x86_64"          # auto-detected, override if needed
latitude = 36.1627
longitude = -86.7816

[network]
# Target protocol listeners
nvmeof_bind = "0.0.0.0:4420"
iscsi_bind = "0.0.0.0:3260"

# Management interface
mgmt_bind = "0.0.0.0:8443"

[drives]
# NVMe drives are auto-discovered via VFIO
# SAS drives are auto-discovered via /sys/class/scsi_disk

# Manual overrides / exclusions
exclude = ["nvme0"]              # Don't use this device
boot_device = "/dev/sda"         # Exclude boot drive

[io]
# Per-core I/O reactors
io_cores = "2-15"                # Cores 2-15 for I/O (auto if omitted)
mgmt_cores = "0-1"               # Cores 0-1 for management

# NVMe queue depth per core
nvme_queue_depth = 256

# io_uring settings
uring_sq_entries = 4096
uring_cq_entries = 8192
uring_sqpoll = true
uring_sqpoll_idle_ms = 2000

# DMA buffer pool
dma_hugepage_1g = 4              # Number of 1GB hugepages
dma_hugepage_2m = 2048           # Number of 2MB hugepages

[raid]
# Default RAID configuration
default_stripe_size = "256K"
journal_device = "auto"          # Fastest NVMe SSD for write-intent journal
scrub_interval = "7d"            # Background scrub every 7 days
rebuild_rate_limit = "200MB/s"

[cluster]
# Multi-node configuration
peers = []                        # Empty = standalone mode
replication = "async"             # sync or async
raft_data_dir = "/var/lib/stormblock/raft"
```

---

## 10. Cluster & Replication

For multi-node deployments, StormBlock uses Raft (via `openraft`) for cluster configuration consensus and provides synchronous or asynchronous volume replication.

```rust
pub struct StormCluster {
    raft: Raft<ClusterConfig>,
    peers: Vec<PeerNode>,
    
    /// Replication streams per volume
    replication_tasks: HashMap<VolumeId, ReplicationTask>,
}

struct ReplicationTask {
    source_volume: Arc<ThinVolume>,
    target_peer: PeerNode,
    target_volume_id: VolumeId,
    mode: ReplicationMode,
    
    /// Write journal for async replication
    /// Tracks which extents need to be sent to the remote
    dirty_bitmap: BitVec,
    
    /// Current lag (async mode)
    lag_bytes: AtomicU64,
}

enum ReplicationMode {
    /// Write completes only after confirmed on both nodes
    Synchronous,
    /// Write completes locally, replicated in background
    Asynchronous { max_lag: Duration },
}

impl ReplicationTask {
    /// For synchronous replication: intercept writes and fan out
    pub async fn replicate_write(&self, offset: u64, data: &DmaBuf) -> Result<()> {
        match self.mode {
            ReplicationMode::Synchronous => {
                // Write to both local and remote in parallel
                let local = self.source_volume.write(offset, data);
                let remote = self.target_peer.remote_write(
                    &self.target_volume_id, offset, data
                );
                futures::try_join!(local, remote)?;
                Ok(())
            }
            ReplicationMode::Asynchronous { .. } => {
                // Write locally, mark dirty for background replication
                self.source_volume.write(offset, data).await?;
                let extent_idx = offset / self.source_volume.extent_size();
                self.dirty_bitmap.set(extent_idx as usize, true);
                self.lag_bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                Ok(())
            }
        }
    }
    
    /// Background replication loop (async mode)
    pub async fn replication_loop(&self) -> Result<()> {
        loop {
            // Find dirty extents
            for idx in 0..self.dirty_bitmap.len() {
                if self.dirty_bitmap[idx] {
                    let offset = idx as u64 * self.source_volume.extent_size();
                    let mut buf = DmaBuf::alloc(self.source_volume.extent_size() as usize)?;
                    
                    self.source_volume.read(offset, &mut buf).await?;
                    self.target_peer.remote_write(
                        &self.target_volume_id, offset, &buf
                    ).await?;
                    
                    self.dirty_bitmap.set(idx, false);
                    self.lag_bytes.fetch_sub(buf.len() as u64, Ordering::Relaxed);
                }
            }
            
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
```

---

## 11. Build Plan

### Phase 1: Foundation (Months 1-3)
- [ ] Buildroot Linux image (kernel config, VFIO, hugepages, mpt3sas)
- [ ] NVMe userspace driver: VFIO BAR mapping, admin queue, identify controller
- [ ] NVMe I/O queue pairs: per-core queues, DMA buffer pool, submission/completion
- [ ] SAS device enumeration and io_uring O_DIRECT I/O
- [ ] BlockDevice trait + NvmeDevice + SasDevice implementations
- [ ] Single-threaded reactor: poll NVMe CQ + io_uring in one loop
- [ ] NVMe-oF/TCP target: ICReq/ICResp, single namespace, read/write/flush
- [ ] End-to-end test: `nvme connect` from Linux initiator, `fio` benchmark

### Phase 2: Block Layer (Months 3-6)
- [ ] RAID engine: RAID 1 (mirror) first, then RAID 5, RAID 6, RAID 10
- [ ] SIMD parity: AVX2 + NEON implementations, runtime detection
- [ ] Write-intent journal (bitmap on fast SSD)
- [ ] Extent allocator with free-space bitmap
- [ ] Thin volume manager: create, delete, resize
- [ ] COW snapshots and clones
- [ ] Volume manager metadata persistence (double-buffered)
- [ ] Multi-queue I/O: per-core reactors with connection affinity

### Phase 3: iSCSI Target (Months 6-9)
- [ ] iSCSI login phase: security + operational negotiation
- [ ] CHAP authentication
- [ ] SCSI command processing: READ/WRITE (10/16), INQUIRY, READ_CAPACITY
- [ ] REPORT_LUNS, MODE_SENSE, UNMAP, WRITE_SAME
- [ ] Multi-connection sessions + error recovery (level 0, then 1)
- [ ] Task management (ABORT_TASK, LUN_RESET)
- [ ] MPIO with ALUA (Active/Optimized + Active/Non-Optimized paths)

### Phase 4: Clustering & Replication (Months 9-12)
- [ ] Raft cluster for configuration consensus (openraft)
- [ ] Synchronous replication (2-node mirror)
- [ ] Asynchronous replication with dirty bitmap journal
- [ ] Peer discovery and health monitoring
- [ ] Automatic failover (volume takeover on node failure)
- [ ] Online volume migration (move volume between nodes)

### Phase 5: Production Hardening (Months 12-15)
- [ ] VAAI offloads: XCOPY (full copy offload), ATS (compare-and-write), WRITE_SAME (zero fill)
- [ ] Background scrub with auto-repair
- [ ] RAID rebuild with rate limiting
- [ ] SMART monitoring and predictive failure alerting
- [ ] ARM64 cross-compilation and JBOD head unit testing
- [ ] Performance tuning: NUMA awareness, interrupt coalescing, buffer sizing
- [ ] TLS for NVMe-oF/TCP and iSCSI (in-kernel kTLS via io_uring)
- [ ] Web UI for management

---

## 12. Crate Dependencies

```toml
[dependencies]
# Async runtime
tokio = { version = "1", features = ["full"] }
io-uring = "0.7"

# Consensus (cluster mode)
openraft = { version = "0.10", features = ["serde"], optional = true }

# Management API
axum = "0.8"
hyper = "1"
tower = "0.5"

# Serialization
serde = { version = "1", features = ["derive"] }
bincode = "2"
toml = "0.8"

# CLI
clap = { version = "4", features = ["derive"] }

# Observability
tracing = "0.1"
tracing-subscriber = "0.3"
metrics = "0.24"
metrics-exporter-prometheus = "0.16"

# Data integrity
crc32c = "0.6"
xxhash-rust = { version = "0.8", features = ["xxh3"] }

# System
nix = "0.29"               # POSIX/libc wrappers (ioctl, mmap, etc.)
libc = "0.2"                # Raw syscall access for VFIO
bitvec = "1"                # Extent bitmaps, write-intent bitmaps
uuid = { version = "1", features = ["v4"] }

# Async utilities
futures = "0.3"
pin-project-lite = "0.2"

[target.'cfg(target_arch = "x86_64")'.dependencies]
# No extra deps — SIMD intrinsics via std::arch::x86_64

[target.'cfg(target_arch = "aarch64")'.dependencies]
# No extra deps — NEON intrinsics via std::arch::aarch64

[features]
default = ["nvmeof", "iscsi", "cluster"]
nvmeof = []
iscsi = []
cluster = ["dep:openraft"]
arm64 = []                  # Disable VFIO NVMe driver (SAS-only for JBOD heads)
```

**Total C dependencies:** Zero. The only non-Rust code is the Linux kernel itself.

---

## 13. ARM64 JBOD Head Unit Notes

When compiled for aarch64 with `--features arm64`, StormBlock operates in SAS-only mode:

- NVMe userspace driver is disabled (no VFIO on many ARM boards)
- All drive I/O goes through kernel SAS drivers + io_uring
- 2-4 NVMe SSDs (if available) used for metadata journal + read cache
- RAID engine uses NEON SIMD for parity
- NVMe-oF/TCP and iSCSI targets function identically
- Lower core count (32-128 on Ampere Altra) but sufficient for HDD-bound throughput
- The 25GbE NIC is the ceiling: ~3 GB/s, well above aggregate HDD throughput

```bash
# Cross-compile for ARM64 JBOD head unit
cargo build --release --target aarch64-unknown-linux-musl --features "arm64,iscsi,nvmeof"

# Produces: target/aarch64-unknown-linux-musl/release/stormblock (~12MB static binary)
```

The same REST API, the same topology registration, the same volume manager — just running on ARM silicon serving SAS drives instead of NVMe.
