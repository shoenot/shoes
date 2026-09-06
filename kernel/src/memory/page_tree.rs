use core::{alloc::Layout, fmt::Debug, sync::atomic::{AtomicU64, Ordering}};

use alloc::{alloc::alloc_zeroed, boxed::Box};
use hal::mmu::PageSize;

use crate::memory::ALLOCATOR;


const FANOUT: usize = 512;
const PAGE_SHIFT: usize = 12;
const LEVEL_SHIFT: usize = 9;
const LEVEL_MASK: usize = 0x1FF;

pub struct PageSlot;

impl PageSlot {
    pub const PRESENT_BIT: u64 = 1 << 0;
    pub const DIRTY_BIT: u64 = 1 << 1;
    pub const PFN_MASK: u64 = 0x000F_FFFF_FFFF_F000;

    pub fn is_emtpy(val: u64) -> bool { val == 0 }

    pub fn is_internal_node(val: u64) -> bool { 
        val != 0 && (val & Self::PRESENT_BIT) == 0 
    }

    pub fn is_leaf(val: u64) -> bool {
        (val & Self::PRESENT_BIT) != 0
    }

    pub fn pack_leaf(phys: usize, dirty: bool) -> u64 {
        let mut val = (phys as u64) & Self::PFN_MASK;
        val |= Self::PRESENT_BIT;
        if dirty { val |= Self::DIRTY_BIT; }
        val
    }

    pub fn unpack_phys(val: u64) -> usize {
        (val & Self::PFN_MASK) as usize
    }

    pub fn pack_internal(ptr: *mut RadixNode) -> u64 { ptr as u64 }
    
    pub fn unpack_internal(val: u64) -> *mut RadixNode { val as *mut RadixNode }
}

pub struct RadixNode {
    pub entries: [AtomicU64; FANOUT],
}

impl RadixNode {
    pub fn new() -> Box<Self> {
        unsafe {
            let layout = Layout::new::<Self>();
            let ptr = alloc_zeroed(layout) as *mut Self;
            Box::from_raw(ptr)
        }
    }
}

pub struct PageTree {
    root: AtomicU64,
}

impl PageTree {
    pub fn new() -> Self {
        Self { root: AtomicU64::new(0) }
    }

    fn index_for(offset: usize, level: usize) -> usize {
        let shift = PAGE_SHIFT + (level * LEVEL_SHIFT);
        (offset >> shift) & LEVEL_MASK
    }

    pub fn get_leaf_entry(&self, offset: usize) -> Option<&AtomicU64> {
        let current_val = self.root.load(Ordering::Acquire);
        if current_val == 0 { return None; }

        let mut current_node = PageSlot::unpack_internal(current_val);

        for level in (1..=3).rev() {
            let idx = Self::index_for(offset, level);
            let node_ref = unsafe { &*current_node };
            let next_val = node_ref.entries[idx].load(Ordering::Acquire);

            if next_val == 0 { return None; }
            current_node = PageSlot::unpack_internal(next_val);
        }

        let idx = Self::index_for(offset, 0);
        let leaf_node = unsafe { &*current_node };
        Some(&leaf_node.entries[idx])
    }

    pub fn get_page(&self, offset: usize) -> Option<usize> {
        let leaf_entry = self.get_leaf_entry(offset)?;
        let val = leaf_entry.load(Ordering::Acquire);
        if PageSlot::is_leaf(val) {
            Some(PageSlot::unpack_phys(val))
        } else {
            None
        }
    }

    pub fn get_or_insert_page<F>(&self, offset: usize, mut alloc_pfn: F) -> usize 
    where 
        F: FnMut() -> Option<usize>,
    {
        let mut current_val = self.root.load(Ordering::Acquire);
        
        // root allocation (level 4)
        if current_val == 0 {
            let new_node = Box::into_raw(RadixNode::new());
            let new_packed = PageSlot::pack_internal(new_node);
            match self.root.compare_exchange(0, new_packed, Ordering::Release, Ordering::Acquire) {
                Ok(_) => { current_val = new_packed; },
                Err(existing) => {
                    unsafe { drop(Box::from_raw(new_node)); }
                    current_val = existing;
                },
            }
        }

        let mut current_node = PageSlot::unpack_internal(current_val);

        // internal branches (levels 3 down to 1)
        for level in (1..=3).rev() {
            let idx = Self::index_for(offset, level);
            let node_ref = unsafe { &*current_node };
            let mut next_val = node_ref.entries[idx].load(Ordering::Acquire);

            if next_val == 0 {
                let new_node = Box::into_raw(RadixNode::new());
                let new_packed = PageSlot::pack_internal(new_node);
                match node_ref.entries[idx].compare_exchange(0, new_packed, Ordering::Release, Ordering::Acquire) {
                    Ok(_) => { next_val = new_packed; },
                    Err(existing) => {
                        unsafe { drop(Box::from_raw(new_node)); } // Thread lost race, drop allocation
                        next_val = existing;
                    }
                }
            }
            current_node = PageSlot::unpack_internal(next_val);
        }

        let idx = Self::index_for(offset, 0);
        let leaf_node = unsafe { &*current_node };

        loop {
            let leaf_val = leaf_node.entries[idx].load(Ordering::Acquire);
            if PageSlot::is_leaf(leaf_val) {
                return PageSlot::unpack_phys(leaf_val);
            }

            // alloc actual phys memory chunk
            let new_pfn = alloc_pfn().expect("Out of memory during page fault!");
            let packed = PageSlot::pack_leaf(new_pfn, false);

            match leaf_node.entries[idx].compare_exchange(0, packed, Ordering::Release, Ordering::Acquire) {
                Ok(_) => return new_pfn,
                Err(_) => ALLOCATOR.free(new_pfn, PageSize::Size4K),
            }
        }
    }

    pub fn set_dirty(&self, offset: usize, dirty: bool) -> Result<(), ()> {
        let entry = self.get_leaf_entry(offset).ok_or(())?;
        if dirty {
            entry.fetch_or(PageSlot::DIRTY_BIT, Ordering::Release);
        } else {
            entry.fetch_and(!PageSlot::DIRTY_BIT, Ordering::Release);
        }
        Ok(())
    }
    
    pub fn is_dirty(&self, offset: usize) -> bool {
        if let Some(entry) = self.get_leaf_entry(offset) {
            (entry.load(Ordering::Acquire) & PageSlot::DIRTY_BIT) != 0
        } else {
            false
        }
    }
    
    pub fn for_each_page<F>(&self, mut callback: F)
    where
        F: FnMut(usize, usize), // offset, pfn
    {
        let root_val = self.root.load(Ordering::Acquire);
        if root_val != 0 {
            unsafe { self.visit_node(PageSlot::unpack_internal(root_val), 3, 0, &mut callback); }
        }
    }
    
    unsafe fn visit_node<F>(&self, ptr: *mut RadixNode, level: usize, base_offset: usize, callback: &mut F)
    where
        F: FnMut(usize, usize),
    {
        unsafe {
            let node = &*ptr;
            let shift = PAGE_SHIFT + (level * LEVEL_SHIFT);
        
            for (i, entry) in node.entries.iter().enumerate() {
                let val = entry.load(Ordering::Acquire);
                if val == 0 { continue; }
        
                let offset = base_offset + (i << shift);
                if level > 0 {
                    self.visit_node(PageSlot::unpack_internal(val), level - 1, offset, callback);
                } else {
                    if PageSlot::is_leaf(val) {
                        callback(offset, PageSlot::unpack_phys(val));
                    }
                }
            }
        }
    }

    pub fn remove_page(&self, offset: usize) -> Option<usize> {
        if let Some(entry) = self.get_leaf_entry(offset) {
            let old_val = entry.swap(0, Ordering::Release);
            if PageSlot::is_leaf(old_val) {
                return Some(PageSlot::unpack_phys(old_val));
            }
        }
        None
    }
}

// the pagetree auto drops all of its metadata radixnodes when it goes out of scope
// but it doesn't drop the actual physical pages, which allows the vmo to decide when and how
// the physical memory is freed

impl Debug for PageTree {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PageTree").finish()
    }
}

impl Drop for PageTree {
    fn drop(&mut self) {
        let root_val = self.root.load(Ordering::Acquire);
        if root_val != 0 {
            unsafe { self.free_nodes_only(PageSlot::unpack_internal(root_val), 3); }
        }
    }
}

impl PageTree {
    unsafe fn free_nodes_only(&self, ptr: *mut RadixNode, level: usize) {
        unsafe {
            let node = Box::from_raw(ptr);
            if level > 0 {
                for entry in &node.entries {
                    let val = entry.load(Ordering::Acquire);
                    if PageSlot::is_internal_node(val) {
                        self.free_nodes_only(PageSlot::unpack_internal(val), level - 1);
                    }
                }
            }
        }
    }
}
