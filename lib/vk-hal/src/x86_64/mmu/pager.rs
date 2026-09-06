use crate::mmu::FrameAllocator;
use crate::mmu::PageFlags;

use super::{DIRECT_MAP_OFFSET, PageSize, PhysAddr, VirtAddr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagerError {
    OutOfMemory,
    AlreadyMapped,
    NotMapped,
    InvalidAlignment,
    HugePageClash,
}

#[repr(C, align(4096))]
pub struct PageTable {
    entries: [PageTableEntry; 512],
}

impl PageTable {
    pub unsafe fn from_phys<'a>(phys: PhysAddr) -> &'a mut Self {
        unsafe {
            let virt = phys.0 + *DIRECT_MAP_OFFSET;
            &mut *(virt as *mut PageTable)
        }
    }

    pub fn zero(&mut self) {
        for entry in self.entries.iter_mut() {
            entry.clear();
        }
    }

    // check if all 512 entries are empty
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(|e| !e.is_present())
    }
}

#[repr(transparent)]
#[derive(Copy, Clone)]
pub struct PageTableEntry(u64);

impl PageTableEntry {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn clear(&mut self) {
        self.0 = 0;
    }

    pub fn is_present(&self) -> bool {
        (self.0 & PageFlags::PRESENT.bits()) != 0
    }

    pub fn is_huge(&self) -> bool {
        (self.0 & PageFlags::HUGE_PAGE.bits()) != 0
    }

    pub fn get_frame(&self) -> PhysAddr {
        // mask out the flags and the nx bit to get the raw phys frame
        PhysAddr((self.0 & 0x000F_FFFF_FFFF_F000) as usize)
    }

    pub fn set(&mut self, frame: PhysAddr, flags: PageFlags) {
        self.0 = (frame.0 as u64) | flags.bits();
    }

    pub fn flags(&self) -> PageFlags {
        PageFlags(self.0 & 0xFFF0_0000_0000_0FFF)
    }
}

pub struct Pager {
    pml4_phys: PhysAddr,
}

impl Pager {
    pub const fn new(pml4_phys: PhysAddr) -> Self {
        Self { pml4_phys }
    }

    pub fn root_phys(&self) -> PhysAddr {
        self.pml4_phys
    }

    pub fn map_page(&mut self, virt: VirtAddr, phys: PhysAddr, flags: PageFlags, size: PageSize, alloc: &mut impl FrameAllocator) -> Result<(), PagerError> {
        let indices = [virt.p4_index(), virt.p3_index(), virt.p2_index(), virt.p1_index()];
        let depth = match size {
            PageSize::Size1G => 1,
            PageSize::Size2M => 2,
            PageSize::Size4K => 3,
        };
        let mut table = unsafe { PageTable::from_phys(self.pml4_phys) };

        for i in 0..depth {
            let idx = indices[i];
            let entry = &mut table.entries[idx];

            if !entry.is_present() {
                let new_frame = alloc.allocate_frame().ok_or(PagerError::OutOfMemory)?;
                let phys_frame = PhysAddr(new_frame);

                unsafe { PageTable::from_phys(phys_frame).zero() };

                let int_flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER_ACCESSIBLE;
                entry.set(phys_frame, int_flags);
            } else if entry.is_huge() {
                return Err(PagerError::HugePageClash);
            }

            table = unsafe { PageTable::from_phys(entry.get_frame()) };
        }

        let leaf_idx = indices[depth];
        let leaf_entry = &mut table.entries[leaf_idx];
        
        if leaf_entry.is_present() {
            return Err(PagerError::AlreadyMapped);
        }

        let mut final_flags = flags | PageFlags::PRESENT;
        if size != PageSize::Size4K {
            final_flags = final_flags.insert(PageFlags::HUGE_PAGE);
        }

        leaf_entry.set(phys, final_flags);
        Ok(())
    }

    pub fn unmap_page_no_flush(&mut self, virt: VirtAddr, size: PageSize, alloc: &mut impl FrameAllocator) -> Result<(), PagerError> {
        let indices = [virt.p4_index(), virt.p3_index(), virt.p2_index(), virt.p1_index()];
        let depth = match size {
            PageSize::Size1G => 1,
            PageSize::Size2M => 2,
            PageSize::Size4K => 3,
        };
        self.unmap_recursive(self.pml4_phys, &indices, 0, depth, alloc)
    }

    pub fn unmap_page(&mut self, virt: VirtAddr, size: PageSize, alloc: &mut impl FrameAllocator) -> Result<(), PagerError> {
        self.unmap_page_no_flush(virt, size, alloc)?;
        super::flush_tlb(virt.0 as u64);
        Ok(())
    }

    pub fn unmap_recursive(
        &self, table_phys: PhysAddr, indices: &[usize; 4], current_level: usize, target_level: usize, alloc: &mut impl FrameAllocator) 
        -> Result<(), PagerError> {
        let table = unsafe { PageTable::from_phys(table_phys) };
        let idx = indices[current_level];
        let entry = &mut table.entries[idx];

        if !entry.is_present() { return Err(PagerError::NotMapped); }

        if current_level == target_level {
            if entry.is_huge() != (target_level < 3) {
                return Err(PagerError::HugePageClash);
            }
            entry.clear();
            return Ok(());
        }

        if entry.is_huge() {
            return Err(PagerError::HugePageClash);
        }

        let child_phys = entry.get_frame();

        self.unmap_recursive(child_phys, indices, current_level + 1, target_level, alloc)?;

        let child_table = unsafe { PageTable::from_phys(child_phys) };
        if child_table.is_empty() {
            entry.clear();
            alloc.deallocate_frame(child_phys.0);
        }

        Ok(())
    }

    pub fn demote_page_no_flush(&mut self, virt: VirtAddr, from_size: PageSize, alloc: &mut impl FrameAllocator) -> Result<(), PagerError> {
        if from_size == PageSize::Size4K { return Ok(()); }
        
        let indices = [virt.p4_index(), virt.p3_index(), virt.p2_index(), virt.p1_index()];
        let depth = match from_size {
            PageSize::Size1G => 1,
            PageSize::Size2M => 2,
            PageSize::Size4K => unreachable!(),
        };
        
        let mut table = unsafe { PageTable::from_phys(self.pml4_phys) };
        
        for i in 0..depth {
            let idx = indices[i];
            let entry = &mut table.entries[idx];
        
            if !entry.is_present() {
                return Ok(());
            }
        
            if i == depth - 1 {
                if !entry.is_huge() {
                    return Ok(());
                }
        
                let phys_frame = entry.get_frame();
                let flags = entry.flags();
        
                let new_pt_frame = alloc.allocate_frame().ok_or(PagerError::OutOfMemory)?;
                let new_pt = unsafe { PageTable::from_phys(PhysAddr(new_pt_frame)) };
                new_pt.zero();
        
                let sub_page_size = if from_size == PageSize::Size1G { 0x20_0000 } else { 0x1000 };
                let child_flags = flags.remove(PageFlags::HUGE_PAGE);
        
                for j in 0..512 {
                    let child_phys = PhysAddr(phys_frame.0 + (j * sub_page_size));
                    let mut final_child_flags = child_flags;
                    if from_size == PageSize::Size1G {
                        final_child_flags = final_child_flags.insert(PageFlags::HUGE_PAGE);
                    }
                    new_pt.entries[j].set(child_phys, final_child_flags);
                }
        
                let table_flags = PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER_ACCESSIBLE;
                entry.set(PhysAddr(new_pt_frame), table_flags);
                return Ok(());
            }
        
            if entry.is_huge() {
                return Err(PagerError::HugePageClash);
            }
        
            table = unsafe { PageTable::from_phys(entry.get_frame()) };
        }
        
        Ok(())
    }

    pub fn change_flags_no_flush(&mut self, virt: VirtAddr, flags: PageFlags, size: PageSize) -> Result<(), PagerError> {
        let indices = [virt.p4_index(), virt.p3_index(), virt.p2_index(), virt.p1_index()];
        let depth = match size {
            PageSize::Size1G => 1,
            PageSize::Size2M => 2,
            PageSize::Size4K => 3,
        };
    
        let mut table = unsafe { PageTable::from_phys(self.pml4_phys) };
    
        for i in 0..depth {
            let idx = indices[i];
            let entry = &mut table.entries[idx];
    
            if !entry.is_present() || entry.is_huge() {
                return Err(PagerError::NotMapped);
            }
    
            table = unsafe { PageTable::from_phys(entry.get_frame()) };
        }
    
        let leaf_idx = indices[depth];
        let leaf_entry = &mut table.entries[leaf_idx];
    
        if !leaf_entry.is_present() {
            return Err(PagerError::NotMapped);
        }
    
        let mut final_flags = flags | PageFlags::PRESENT;
        if size != PageSize::Size4K {
            final_flags = final_flags.insert(PageFlags::HUGE_PAGE);
        }
    
        let phys = leaf_entry.get_frame();
        leaf_entry.set(phys, final_flags);
        Ok(())
    }

    pub fn free_user_page_tables(&mut self, alloc: &mut impl FrameAllocator) {
        let pml4 = unsafe { PageTable::from_phys(self.pml4_phys) };
        for i in 0..256 {
            let entry = &mut pml4.entries[i];
            if entry.is_present() && !entry.is_huge() {
                let pdpt_phys = entry.get_frame();
                self.free_table_recursive(pdpt_phys, 1, alloc);
                entry.clear();
                alloc.deallocate_frame(pdpt_phys.0);
            }
        }
    }

    pub fn free_table_recursive(&self, table_phys: PhysAddr, level: usize, alloc: &mut impl FrameAllocator) {
        if level == 3 {
            // vmo handles l3 data frames
            return;
        }

        let table = unsafe { PageTable::from_phys(table_phys) };
        for i in 0..512 {
            let entry = &mut table.entries[i];
            // huge means children are data, not tables, so don't descend 
            if entry.is_present() && !entry.is_huge() {
                let child_phys = entry.get_frame();
                self.free_table_recursive(child_phys, level + 1, alloc);
                entry.clear();
                alloc.deallocate_frame(child_phys.0);
            }
        }

    }

    pub fn change_flags(&mut self, virt: VirtAddr, flags: PageFlags, size: PageSize) -> Result<(), PagerError> {
        self.change_flags_no_flush(virt, flags, size)?;
        super::flush_tlb(virt.0 as u64);
        Ok(())
    }

    pub fn sync_kernel_mappings(&mut self, master_pml4: PhysAddr) {
        let current_table = unsafe { PageTable::from_phys(self.pml4_phys) };
        let master_table = unsafe { PageTable::from_phys(master_pml4) };
        
        // copy higher half (top 256 entries)
        for i in 256..512 {
            current_table.entries[i] = master_table.entries[i];
        }
    }

    pub fn translate(&self, virt: VirtAddr) -> Option<(PhysAddr, PageSize, PageFlags)> {
        let indices = [virt.p4_index(), virt.p3_index(), virt.p2_index(), virt.p1_index()];
        let mut table_phys = self.pml4_phys;

        for level in 0..4 {
            let table = unsafe { PageTable::from_phys(table_phys) };
            let entry = &table.entries[indices[level]];

            if !entry.is_present() { return None; }

            if entry.is_huge() || level == 3 {
                let page_size = match level {
                    1 => PageSize::Size1G,
                    2 => PageSize::Size2M,
                    3 => PageSize::Size4K,
                    _ => return None,
                };
                let frame_base = entry.get_frame().0;
                let offset = virt.0 & (page_size.bytes() - 1);

                return Some((PhysAddr(frame_base + offset), page_size, entry.flags()));
            }

            table_phys = entry.get_frame();
        }

        None
    }
}
