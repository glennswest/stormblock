//! A slab's slot table, read from the device through a bounded cache (#155).
//!
//! The table on the device has always been the record of who owns each slot:
//! every entry is written when it changes, and a slab is opened by reading it.
//! The engine used to keep a second copy of all of it in memory, 40 bytes for
//! every slot whether used or not — ~10 GB for an empty 256 TB drive at
//! 1 MiB slots — plus an index of the allocated ones.
//!
//! Now the slab keeps only its free map (1 bit a slot) and the entries that
//! differ from the device (`Pending::entries` in `slab.rs`: allocated and not
//! yet published, share counts waiting for a sync, #171). An entry is read
//! from a cache of 4 KiB table pages, bounded per slab, and an entry written
//! is written into its page, read first if the cache does not have it, and
//! the page kept. Restore and the collector read the whole table in one pass
//! ([`SlotTable::scan`]) instead of asking a slot at a time.
//!
//! Reads of a page that is not cached and writes of pages are one at a time
//! (`page_io`), so a read can never put a page older than one just written
//! into the cache.
//!
//! The rule from #269 holds here too: no lock the node's I/O waits for is
//! held across a device read that can be avoided. Hot paths call
//! [`SlotTable::prefetch`] with no lock held before they take the registry
//! to change entries.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use super::slab::{Slot, SLOT_ENTRY_BYTES};
use super::{BlockDevice, DriveResult};

/// Bytes in a cached page of the table.
const PAGE: u64 = 4096;
const ENTRIES_PER_PAGE: u64 = PAGE / SLOT_ENTRY_BYTES;

/// Default bound of a slab's page cache: 16 MiB = 4096 pages = 262,144
/// entries. `STORMBLOCK_SLOT_CACHE_MB` overrides it (0 = no cache).
const DEFAULT_CACHE_BYTES: u64 = 16 << 20;

fn cache_pages() -> usize {
    let bytes = std::env::var("STORMBLOCK_SLOT_CACHE_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|mb| mb << 20)
        .unwrap_or(DEFAULT_CACHE_BYTES);
    (bytes / PAGE) as usize
}

struct PageCache {
    pages: HashMap<u64, Arc<[u8]>>,
    /// Insertion order, for eviction (oldest first).
    order: VecDeque<u64>,
    cap: usize,
}

impl PageCache {
    fn put(&mut self, page: u64, bytes: Arc<[u8]>) {
        if self.cap == 0 {
            return;
        }
        if self.pages.insert(page, bytes).is_none() {
            self.order.push_back(page);
        }
        while self.pages.len() > self.cap {
            match self.order.pop_front() {
                Some(old) => {
                    self.pages.remove(&old);
                }
                None => break,
            }
        }
    }
}

/// See the module documentation.
pub struct SlotTable {
    device: Arc<dyn BlockDevice>,
    table_offset: u64,
    /// Bytes from `table_offset` the table may occupy: its pages never
    /// reach into the data region.
    limit: u64,
    cache: std::sync::Mutex<PageCache>,
    page_io: tokio::sync::Mutex<()>,
}

impl SlotTable {
    pub(crate) fn new(device: Arc<dyn BlockDevice>, table_offset: u64, data_offset: u64) -> SlotTable {
        SlotTable {
            device,
            table_offset,
            limit: data_offset.saturating_sub(table_offset),
            cache: std::sync::Mutex::new(PageCache { pages: HashMap::new(), order: VecDeque::new(), cap: cache_pages() }),
            page_io: tokio::sync::Mutex::new(()),
        }
    }

    fn page_of(idx: u64) -> (u64, usize) {
        let idx = idx as u64;
        (idx / ENTRIES_PER_PAGE, ((idx % ENTRIES_PER_PAGE) * SLOT_ENTRY_BYTES) as usize)
    }

    fn page_len(&self, page: u64) -> usize {
        (self.limit.saturating_sub(page.saturating_mul(PAGE))).min(PAGE) as usize
    }

    fn cached(&self, page: u64) -> Option<Arc<[u8]>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).pages.get(&page).cloned()
    }

    /// A page, from the cache or the device. The caller holds `page_io`.
    async fn load(&self, page: u64) -> DriveResult<Arc<[u8]>> {
        if let Some(p) = self.cached(page) {
            return Ok(p);
        }
        let mut buf = vec![0u8; self.page_len(page)];
        self.device.read(self.table_offset + page * PAGE, &mut buf).await?;
        let bytes: Arc<[u8]> = buf.into();
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).put(page, bytes.clone());
        Ok(bytes)
    }

    /// The entry of slot `idx` as the device has it. An entry that does not
    /// check is free, as at open.
    pub async fn read(&self, idx: u64) -> DriveResult<Slot> {
        let (page, off) = Self::page_of(idx);
        let bytes = match self.cached(page) {
            Some(b) => b,
            None => {
                let _io = self.page_io.lock().await;
                self.load(page).await?
            }
        };
        let end = off + SLOT_ENTRY_BYTES as usize;
        if end > bytes.len() {
            return Ok(Slot::free());
        }
        Ok(Slot::from_bytes(&bytes[off..end]).unwrap_or_else(Slot::free))
    }

    /// Put the pages of `idxs` in the cache, with no lock of the caller's
    /// held: what a change under the registry lock will read.
    pub async fn prefetch(&self, idxs: impl IntoIterator<Item = u64>) {
        let mut pages: Vec<u64> = idxs.into_iter().map(|i| Self::page_of(i).0).collect();
        pages.sort_unstable();
        pages.dedup();
        for page in pages {
            // An index past the table (a caller's bad slot) has no page.
            if self.page_len(page) == 0 || self.cached(page).is_some() {
                continue;
            }
            let _io = self.page_io.lock().await;
            if let Err(e) = self.load(page).await {
                tracing::debug!("slot table prefetch of page {page}: {e}");
                return;
            }
        }
    }

    /// Write entries, each into its page: the page as the device has it,
    /// with these entries replaced.
    pub async fn write(&self, entries: &[(u64, [u8; SLOT_ENTRY_BYTES as usize])]) -> DriveResult<()> {
        self.write_checked(entries, || true).await.map(|_| ())
    }

    /// [`write`](Self::write), if `still` says so once the page lock is held
    /// (#364): a caller that read its entries with no slab lock held checks
    /// them against memory here, where no other table write can come between
    /// the check and its own. `Ok(false)`: nothing written.
    pub async fn write_checked(
        &self,
        entries: &[(u64, [u8; SLOT_ENTRY_BYTES as usize])],
        still: impl FnOnce() -> bool,
    ) -> DriveResult<bool> {
        let mut by_page: std::collections::BTreeMap<u64, Vec<(usize, &[u8; SLOT_ENTRY_BYTES as usize])>> =
            Default::default();
        for (idx, bytes) in entries {
            let (page, off) = Self::page_of(*idx);
            by_page.entry(page).or_default().push((off, bytes));
        }
        let _io = self.page_io.lock().await;
        if !still() {
            return Ok(false);
        }
        for (page, changes) in by_page {
            let mut buf = self.load(page).await?.to_vec();
            for (off, bytes) in changes {
                let end = off + SLOT_ENTRY_BYTES as usize;
                if end <= buf.len() {
                    buf[off..end].copy_from_slice(bytes);
                }
            }
            self.device.write(self.table_offset + page * PAGE, &buf).await?;
            self.cache.lock().unwrap_or_else(|e| e.into_inner()).put(page, buf.into());
        }
        Ok(true)
    }

    /// Every entry of the first `total` slots that is not free, read from the
    /// device in large pieces (not through the cache). For restore and the
    /// collector, which want the whole table once.
    pub async fn scan(&self, total: u64) -> DriveResult<Vec<(u64, Slot)>> {
        const PIECE: u64 = 4 << 20;
        let want = (total * SLOT_ENTRY_BYTES).min(self.limit);
        let mut out = Vec::new();
        let mut at = 0u64;
        while at < want {
            let n = PIECE.min(want - at);
            let mut buf = vec![0u8; n as usize];
            self.device.read(self.table_offset + at, &mut buf).await?;
            for (i, e) in buf.chunks_exact(SLOT_ENTRY_BYTES as usize).enumerate() {
                if e[0] == 0 {
                    continue; // free (state byte 0), the common case
                }
                if let Some(slot) = Slot::from_bytes(e) {
                    if slot.state != super::slab::SlotState::Free {
                        out.push(((at / SLOT_ENTRY_BYTES) + i as u64, slot));
                    }
                }
            }
            at += n;
        }
        Ok(out)
    }

    /// Pages cached, for tests and the footprint example.
    pub fn cached_pages(&self) -> usize {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).pages.len()
    }
}
