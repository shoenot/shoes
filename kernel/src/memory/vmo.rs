use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt::Debug;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::executor::syscall_bridge::block_on;
use crate::memory::pmm::PF_PINNED;
use crate::memory::{ALLOCATOR, GLOBAL_PMM, DIRECT_MAP_OFFSET, NORMAL_PAGE_SIZE};
use crate::storage::fs::VfsNode;
use crate::memory::page_tree::PageTree;
use hal::mmu::PageSize;

#[derive(Debug)]
pub struct Vmo {
    pub size: AtomicUsize,
    pub tree: PageTree,
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
    fn has_dirty_pages(&self) -> bool { false }
    fn get_node(&self) -> Option<Arc<dyn VfsNode>> { None }
    fn peek_page(&self, _offset: usize) -> Option<usize> { None }
}

impl PagedBackingStore for Vmo {
    fn request_page(&self, offset: usize) -> Result<usize, ()> {
        let current_size = self.size.load(Ordering::Relaxed);
        if offset >= current_size {
            return Err(());
        }

        if let Some(pfn) = self.tree.get_page(offset) {
            return Ok(pfn);
        }

        if self.is_physical {
            return Err(());
        }

        let pfn = self.tree.get_or_insert_page(offset, || {
            let page_phys = ALLOCATOR.alloc(PageSize::Size4K)?;
            unsafe {
                core::ptr::write_bytes((page_phys + *DIRECT_MAP_OFFSET) as *mut u8, 0, NORMAL_PAGE_SIZE);
            }
            Some(page_phys)
        });
        Ok(pfn)
    }

    fn resize_object(&self, new_size: usize) -> Result<(), ()> {
        if self.is_physical { return Err(()); }
        let old_size = self.size.load(Ordering::Relaxed);

        if new_size == old_size { return Ok(()); }

        if new_size < old_size {
            // Shrink: free pages beyond new size
            let mut to_remove = Vec::new();
            self.tree.for_each_page(|offset, _pfn| {
                if offset >= new_size {
                    to_remove.push(offset);
                }
            });

            for offset in to_remove {
                if let Some(pfn) = self.tree.remove_page(offset) {
                    ALLOCATOR.free(pfn, PageSize::Size4K);
                }
            }
        }

        self.size.store(new_size, Ordering::Relaxed);
        Ok(())
    }

    fn clone_range(&self, offset: usize, len: usize) -> Result<Arc<dyn PagedBackingStore>, ()> {
        if self.is_physical { return Err(()); }
        let current_size = self.size.load(Ordering::Relaxed);

        let child_vmo = Vmo::new(len);
        let num_pages = len.div_ceil(NORMAL_PAGE_SIZE);

        for i in 0..num_pages {
            let page_offset = i * NORMAL_PAGE_SIZE;
            let parent_offset = offset + page_offset;

            if parent_offset < current_size {
                if let Some(parent_pfn) = self.tree.get_page(parent_offset) {
                    child_vmo.tree.get_or_insert_page(page_offset, || {
                        let child_pfn = ALLOCATOR.alloc(PageSize::Size4K)?;
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                (parent_pfn + *DIRECT_MAP_OFFSET) as *mut u8,
                                (child_pfn + *DIRECT_MAP_OFFSET) as *mut u8,
                                NORMAL_PAGE_SIZE
                            );
                        }
                        Some(child_pfn)
                    });
                }
            }
        }
        Ok(child_vmo as Arc<dyn PagedBackingStore>)
    }

    fn pin(self: Arc<Self>, offset: usize, len: usize) -> Result<PinnedVmo, ()> {
        let current_size = self.size.load(Ordering::Relaxed);
        if offset + len > current_size { return Err(()); }

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
        if self.is_physical { return Err(()); }
        self.tree.set_dirty(offset, true)
    }

    fn is_dirty(&self, offset: usize) -> bool { self.tree.is_dirty(offset) }

    fn clear_dirty(&self, offset: usize) {
        let _ = self.tree.set_dirty(offset, false);
    }

    fn has_dirty_pages(&self) -> bool {
        let mut dirty = false;
        self.tree.for_each_page(|offset, _pfn| {
            if self.tree.is_dirty(offset) { dirty = true; }
        });
        dirty
    }

    fn peek_page(&self, offset: usize) -> Option<usize> { self.tree.get_page(offset) }
}

impl Vmo {
    pub fn new(size: usize) -> Arc<Self> {
        Arc::new(Self {
            size: AtomicUsize::new(size),
            tree: PageTree::new(),
            is_physical: false,
        })
    }

    pub fn new_phys(phys_addr: usize, size: usize) -> Arc<Self> {
        let tree = PageTree::new();
        let num_pages = size.div_ceil(NORMAL_PAGE_SIZE);
        for i in 0..num_pages {
            let offset = i * NORMAL_PAGE_SIZE;
            tree.get_or_insert_page(offset, || Some(phys_addr + offset));
        }

        Arc::new(Self {
            size: AtomicUsize::new(size),
            tree,
            is_physical: true,
        })
    }
}

// Drops all the physical pages, then the PageTree automatically drops the Radix metadata nodes!
impl Drop for Vmo {
    fn drop(&mut self) {
        if self.is_physical { return; }
        self.tree.for_each_page(|_offset, pfn| {
            ALLOCATOR.free(pfn, PageSize::Size4K);
        });
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
    pub fn new(size: usize, node: Weak<dyn VfsNode>) -> Arc<Self> {
        Arc::new(Self { anonymous_vmo: Vmo::new(size), node })
    }

    pub async fn flush_to_disk(&self) -> Result<(), ()> {
        let node = self.node.upgrade().ok_or(())?;
        let mut dirty_offsets = Vec::new();

        self.anonymous_vmo.tree.for_each_page(|offset, _pfn| {
            if self.anonymous_vmo.tree.is_dirty(offset) {
                dirty_offsets.push(offset);
            }
        });

        for offset in dirty_offsets {
            if let Some(phys_addr) = self.anonymous_vmo.tree.get_page(offset) {
                node.write_at_phys(offset, phys_addr, NORMAL_PAGE_SIZE).await?;
                let _ = self.anonymous_vmo.tree.set_dirty(offset, false);
            }
        }
        Ok(())
    }
}

impl PagedBackingStore for FileVmo {
    fn request_page(&self, offset: usize) -> Result<usize, ()> {
        let current_size = self.anonymous_vmo.size.load(Ordering::Relaxed);
        if offset >= current_size { return Err(()); }

        if let Some(pfn) = self.anonymous_vmo.tree.get_page(offset) {
            return Ok(pfn);
        }

        let page_phys = ALLOCATOR.alloc(PageSize::Size4K).ok_or(())?;
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
                core::ptr::write_bytes(dest_virt as *mut u8, 0, NORMAL_PAGE_SIZE - bytes_read);
            }
        }

        let mut consumed = false;
        let final_pfn = self.anonymous_vmo.tree.get_or_insert_page(offset, || {
            consumed = true;
            Some(page_phys)
        });

        // If another thread populated the tree before our closure was even called, we leak! So free it.
        if !consumed {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
        }

        Ok(final_pfn)
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
        if offset + len > current_size { return Err(()); }

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

    fn mark_dirty(&self, offset: usize) -> Result<(), ()> { self.anonymous_vmo.mark_dirty(offset) }

    fn is_dirty(&self, offset: usize) -> bool { self.anonymous_vmo.is_dirty(offset) }

    fn clear_dirty(&self, offset: usize) { self.anonymous_vmo.clear_dirty(offset); }

    fn has_dirty_pages(&self) -> bool {
        self.anonymous_vmo.has_dirty_pages()
    }

    fn get_node(&self) -> Option<Arc<dyn VfsNode>> { self.node.upgrade() }

    fn peek_page(&self, offset: usize) -> Option<usize> { self.anonymous_vmo.peek_page(offset) }
}
