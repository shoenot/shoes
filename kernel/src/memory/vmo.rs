use alloc::boxed::Box;
use alloc::collections::btree_map::BTreeMap;
use alloc::sync::{
    Arc,
    Weak,
};
use alloc::vec::Vec;
use core::fmt::Debug;
use core::ptr::{
    copy_nonoverlapping,
    write_bytes,
};
use core::sync::atomic::{
    AtomicUsize,
    Ordering,
};

use crate::executor::syscall_bridge::block_on;
use crate::sync::TicketLock;
use crate::memory::pmm::{
    NORMAL_PAGE_SIZE,
    PF_PINNED,
};
use crate::memory::{
    ALLOCATOR,
    PageSize,
    GLOBAL_PMM,
    DIRECT_MAP_OFFSET,
};
use crate::storage::fs::VfsNode;

#[derive(Debug)]
pub struct Vmo {
    pub size: AtomicUsize,
    pub pages: TicketLock<BTreeMap<usize, usize>>,
    pub dirty_pages: TicketLock<BTreeMap<usize, bool>>,
    pub is_physical: bool,
}

pub trait PagedBackingStore: Send + Sync + Debug {
    fn request_page(&self, offset: usize) -> Result<usize, ()>;
    fn resize_object(&self, new_size: usize) -> Result<(), ()>;
    fn clone_range(&self, offset: usize, len: usize) -> Result<Arc<dyn PagedBackingStore>, ()>;
    fn pin(self: Arc<Self>, offset: usize, len: usize) -> Result<PinnedVmo, ()>;
    fn mark_dirty(&self, offset: usize) -> Result<(), ()>;
    fn is_dirty(&self, offset: usize) -> bool;
    fn clear_dirty(&self, offset: usize);
    fn get_node(&self) -> Option<Arc<dyn VfsNode>> { None }
    fn peek_page(&self, _offset: usize) -> Option<usize> { None }
}

impl PagedBackingStore for Vmo {
    fn request_page(&self, offset: usize) -> Result<usize, ()> {
        let mut pages = self.pages.lock();

        let current_size = self.size.load(Ordering::Relaxed);
        if offset >= current_size {
            return Err(());
        }

        if let Some(&pfn) = pages.get(&offset) {
            if pfn != 0 {
                return Ok(pfn);
            };
        }

        if self.is_physical {
            return Err(());
        }

        // allocate directly from the pmm
        let pfn = ALLOCATOR.alloc(PageSize::Size4K);
        if pfn != 0 {
            unsafe {
                core::ptr::write_bytes((pfn + *DIRECT_MAP_OFFSET) as *mut u8, 0, NORMAL_PAGE_SIZE);
            }
        }
        pages.insert(offset, pfn);
        Ok(pfn as usize)
    }

    fn resize_object(&self, new_size: usize) -> Result<(), ()> {
        if self.is_physical {
            return Err(());
        }
        let mut pages = self.pages.lock();
        let old_size = self.size.load(Ordering::Relaxed);

        if new_size == old_size {
            return Ok(());
        }

        if new_size < old_size {
            // shrink, free pages beyond new size
            let mut to_remove = Vec::new();
            for (&offset, &pfn) in pages.iter() {
                if offset >= new_size {
                    if pfn != 0 {
                        ALLOCATOR.free(pfn, PageSize::Size4K);
                    }
                    to_remove.push(offset);
                }
            }
            let mut dirty = self.dirty_pages.lock();
            for offset in to_remove {
                pages.remove(&offset);
                dirty.remove(&offset);
            }
        } else {
            // grow, pad map with 0s
            let num_pages = new_size.div_ceil(NORMAL_PAGE_SIZE);
            let mut dirty = self.dirty_pages.lock();
            for i in 0..num_pages {
                let offset = i * NORMAL_PAGE_SIZE;
                pages.entry(offset).or_insert(0);
                dirty.entry(offset).or_insert(false);
            }
        }
        self.size.store(new_size, Ordering::Relaxed);
        Ok(())
    }

    fn clone_range(&self, offset: usize, len: usize) -> Result<Arc<dyn PagedBackingStore>, ()> {
        if self.is_physical {
            return Err(());
        }
        let pages = self.pages.lock();
        let current_size = self.size.load(Ordering::Relaxed);

        let mut child_pages = BTreeMap::new();
        let mut child_dirty = BTreeMap::new();
        let num_pages = len.div_ceil(NORMAL_PAGE_SIZE);

        for i in 0..num_pages {
            let page_offset = i * NORMAL_PAGE_SIZE;
            let parent_offset = offset + page_offset;

            let child_pfn = ALLOCATOR.alloc(PageSize::Size4K);
            unsafe {
                write_bytes((child_pfn + *DIRECT_MAP_OFFSET) as *mut u8, 0, NORMAL_PAGE_SIZE);
            }

            // copy from parent to child if parent was alr allocated. can skip if no
            if parent_offset < current_size {
                if let Some(&parent_pfn) = pages.get(&parent_offset) {
                    if parent_pfn != 0 {
                        let parent_virt = parent_pfn + *DIRECT_MAP_OFFSET;
                        let child_virt = child_pfn + *DIRECT_MAP_OFFSET;
                        unsafe {
                            copy_nonoverlapping(parent_virt as *mut u8, child_virt as *mut u8, NORMAL_PAGE_SIZE);
                        }
                    }
                }
            }
            child_pages.insert(page_offset, child_pfn);
            child_dirty.insert(page_offset, false);
        }

        Ok(Arc::new(Vmo {
            size: AtomicUsize::new(len),
            pages: TicketLock::new(child_pages),
            dirty_pages: TicketLock::new(child_dirty),
            is_physical: false,
        }))
    }

    fn pin(self: Arc<Self>, offset: usize, len: usize) -> Result<PinnedVmo, ()> {
        let current_size = self.size.load(Ordering::Relaxed);
        if offset + len > current_size {
            return Err(());
        }

        let start_page = offset / NORMAL_PAGE_SIZE;
        let end_page = (offset + len).div_ceil(NORMAL_PAGE_SIZE);
        let mut phys_addrs = Vec::new();

        for i in start_page..end_page {
            let page_offset = i * NORMAL_PAGE_SIZE;
            let addr = self.request_page(page_offset)?;
            phys_addrs.push(addr);
        }

        let pmm = GLOBAL_PMM.lock();
        for &addr in &phys_addrs {
            let pfn = addr / NORMAL_PAGE_SIZE;
            if pfn < pmm.pfndb.len() {
                pmm.pfndb[pfn].flags.fetch_or(PF_PINNED, Ordering::SeqCst);
            }
        }
        Ok(PinnedVmo { vmo: self, phys_addrs })
    }

    fn mark_dirty(&self, offset: usize) -> Result<(), ()> {
        if self.is_physical {
            return Err(());
        }
        let mut dirty = self.dirty_pages.lock();
        if dirty.contains_key(&offset) {
            dirty.insert(offset, true);
            Ok(())
        } else {
            Err(())
        }
    }

    fn is_dirty(&self, offset: usize) -> bool {
        let dirty = self.dirty_pages.lock();
        *dirty.get(&offset).unwrap_or(&false)
    }

    fn clear_dirty(&self, offset: usize) {
        let mut dirty = self.dirty_pages.lock();
        if dirty.contains_key(&offset) {
            dirty.insert(offset, false);
        }
    }

    fn peek_page(&self, offset: usize) -> Option<usize> {
        let pages = self.pages.lock();
        pages.get(&offset).copied().filter(|&pfn| pfn != 0)
    }
}

impl Vmo {
    pub fn new(size: usize) -> Arc<Self> {
        let mut pages = BTreeMap::new();
        let mut dirty_pages = BTreeMap::new();
        let num_pages = size.div_ceil(NORMAL_PAGE_SIZE);
        for i in 0..num_pages {
            let offset = i * NORMAL_PAGE_SIZE;
            pages.insert(offset, 0);
            dirty_pages.insert(offset, false);
        }

        Arc::new(Self {
            size: AtomicUsize::new(size),
            pages: TicketLock::new(pages),
            dirty_pages: TicketLock::new(dirty_pages),
            is_physical: false,
        })
    }

    pub fn new_phys(phys_addr: usize, size: usize) -> Arc<Self> {
        let mut pages = BTreeMap::new();
        let mut dirty_pages = BTreeMap::new();
        let num_pages = size.div_ceil(NORMAL_PAGE_SIZE);
        for i in 0..num_pages {
            let offset = i * NORMAL_PAGE_SIZE;
            pages.insert(offset, phys_addr + offset);
            dirty_pages.insert(offset, false);
        }

        Arc::new(Self {
            size: AtomicUsize::new(size),
            pages: TicketLock::new(pages),
            dirty_pages: TicketLock::new(dirty_pages),
            is_physical: true,
        })
    }
}

impl Drop for Vmo {
    fn drop(&mut self) {
        if self.is_physical {
            return;
        }

        let pages = self.pages.lock();
        for (&_offset, &pfn) in pages.iter() {
            if pfn != 0 {
                ALLOCATOR.free(pfn, PageSize::Size4K);
            }
        }
    }
}

#[derive(Debug)]
pub struct PinnedVmo {
    vmo: Arc<dyn PagedBackingStore>,
    phys_addrs: Vec<usize>,
}

impl PinnedVmo {
    pub fn phys_addrs(&self) -> &[usize] { &self.phys_addrs }
}

impl Drop for PinnedVmo {
    fn drop(&mut self) {
        let pmm = GLOBAL_PMM.lock();

        for &addr in &self.phys_addrs {
            let pfn = addr / NORMAL_PAGE_SIZE;
            if pfn < pmm.pfndb.len() {
                // clear the pf pinned flag
                pmm.pfndb[pfn].flags.fetch_and(!PF_PINNED, Ordering::SeqCst);
            }
        }
    }
}

#[derive(Debug)]
pub struct FileVmo {
    pub anonymous_vmo: Arc<Vmo>,
    pub node: Weak<dyn VfsNode>,
}

impl FileVmo {
    pub fn new(size: usize, node: Weak<dyn VfsNode>) -> Arc<Self> { Arc::new(Self { anonymous_vmo: Vmo::new(size), node }) }

    pub async fn flush_to_disk(&self) -> Result<(), ()> {
        let node = self.node.upgrade().ok_or(())?;
        let mut dirty_offsets = Vec::new();

        {
            let dirty = self.anonymous_vmo.dirty_pages.lock();
            for (&offset, &is_dirty) in dirty.iter() {
                if is_dirty {
                    dirty_offsets.push(offset);
                }
            }
        }

        for offset in dirty_offsets {
            let phys_addr = {
                let pages = self.anonymous_vmo.pages.lock();
                *pages.get(&offset).ok_or(())?
            };

            if phys_addr != 0 {
                node.write_at_phys(offset, phys_addr, NORMAL_PAGE_SIZE).await?;
                self.anonymous_vmo.clear_dirty(offset);
            }
        }

        Ok(())
    }
}

impl PagedBackingStore for FileVmo {
    fn request_page(&self, offset: usize) -> Result<usize, ()> {
        // check if page alr loaded in ram
        {
            let pages = self.anonymous_vmo.pages.lock();

            let current_size = self.anonymous_vmo.size.load(Ordering::Relaxed);
            if offset >= current_size {
                return Err(());
            }

            if let Some(&pfn) = pages.get(&offset) {
                if pfn != 0 {
                    return Ok(pfn);
                }
            }
        }

        // cache miss
        let page_phys = ALLOCATOR.alloc(PageSize::Size4K) as usize;
        if page_phys == 0 {
            return Err(());
        }

        let node = match self.node.upgrade() {
            Some(n) => n,
            None => {
                ALLOCATOR.free(page_phys, PageSize::Size4K);
                return Err(());
            }
        };

        let read_fut = node.read_at_phys(offset, page_phys, NORMAL_PAGE_SIZE);
        let bytes_read = match block_on(Box::pin(read_fut)) {
            Ok(bytes) => bytes,
            Err(_) => {
                ALLOCATOR.free(page_phys, PageSize::Size4K);
                return Err(());
            }
        };

        if bytes_read < NORMAL_PAGE_SIZE {
            unsafe {
                let dest_virt = page_phys + bytes_read + *DIRECT_MAP_OFFSET;
                write_bytes(dest_virt as *mut u8, 0, NORMAL_PAGE_SIZE - bytes_read);
            }
        }

        {
            let mut pages = self.anonymous_vmo.pages.lock();
            if let Some(&existing_pfn) = pages.get(&offset) {
                if existing_pfn != 0 {
                    ALLOCATOR.free(page_phys, PageSize::Size4K);
                    return Ok(existing_pfn);
                }
            }
            pages.insert(offset, page_phys);
        }
        Ok(page_phys)
    }

    fn resize_object(&self, new_size: usize) -> Result<(), ()> {
        let node = self.node.upgrade().ok_or(())?;
        node.resize(new_size)?;
        self.anonymous_vmo.resize_object(new_size)
    }

    fn clone_range(&self, offset: usize, len: usize) -> Result<Arc<dyn PagedBackingStore>, ()> {
        let current_size = self.anonymous_vmo.size.load(Ordering::Relaxed);
        for page_offset in (offset..(offset + len)).step_by(NORMAL_PAGE_SIZE) {
            if page_offset < current_size {
                let _ = self.request_page(page_offset);
            }
        }
        self.anonymous_vmo.clone_range(offset, len)
    }

    fn pin(self: Arc<Self>, offset: usize, len: usize) -> Result<PinnedVmo, ()> {
        let current_size = self.anonymous_vmo.size.load(Ordering::Relaxed);
        if offset + len > current_size {
            return Err(());
        }

        let start_page = offset / NORMAL_PAGE_SIZE;
        let end_page = (offset + len).div_ceil(NORMAL_PAGE_SIZE);
        let mut phys_addrs = Vec::new();

        // ensure all pages are faulted/loaded
        for i in start_page..end_page {
            let page_offset = i * NORMAL_PAGE_SIZE;
            let addr = self.request_page(page_offset)?;
            phys_addrs.push(addr);
        }

        // pin pages in the pmm so they cant be reclaimed
        let pmm = GLOBAL_PMM.lock();
        for &addr in &phys_addrs {
            let pfn = addr / NORMAL_PAGE_SIZE;
            if pfn < pmm.pfndb.len() {
                pmm.pfndb[pfn].flags.fetch_or(PF_PINNED, Ordering::SeqCst);
            }
        }

        Ok(PinnedVmo { vmo: self, phys_addrs })
    }

    fn mark_dirty(&self, offset: usize) -> Result<(), ()> { self.anonymous_vmo.mark_dirty(offset) }

    fn is_dirty(&self, offset: usize) -> bool { self.anonymous_vmo.is_dirty(offset) }

    fn clear_dirty(&self, offset: usize) { self.anonymous_vmo.clear_dirty(offset); }

    fn get_node(&self) -> Option<Arc<dyn VfsNode>> { self.node.upgrade() }

    fn peek_page(&self, offset: usize) -> Option<usize> { self.anonymous_vmo.peek_page(offset) }
}
