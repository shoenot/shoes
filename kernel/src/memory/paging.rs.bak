#![allow(dead_code)]

use core::arch::asm;

use hal::mmu::get_cr3;
use hal::mmu::flush_tlb;
use hal::mmu::load_cr3;
use super::pmm::*;
use crate::memory::{
    GLOBAL_PMM,
    PCAllocator,
};

type PhysAlloc = PCAllocator;

// structs

#[repr(C, align(4096))]
pub struct PageTable {
    pub entries: [PageTableEntry; 512],
}

#[repr(transparent)]
#[derive(Copy, Clone)]
pub struct PageTableEntry(u64);

#[repr(transparent)]
#[derive(Copy, Clone)]
pub struct VirtAddress(pub u64);

#[derive(Debug)]
pub struct Pager {
    active_l4_addr: u64,
    allocator: &'static PhysAlloc,
}

// helper functions

fn next_table(entry: &PageTableEntry, phys_offset: u64) -> Option<*const PageTable> {
    if !entry.is_present() {
        return None;
    }
    Some((entry.get_addr() + phys_offset) as *const PageTable)
}

fn get_or_create_next(entry: &mut PageTableEntry, phys_offset: u64, allocator: &'static PhysAlloc) -> Option<*mut PageTable> {
    if !entry.is_present() {
        let new_frame_phys = { allocator.alloc(BlockSize::Normal) as u64 };

        let new_table_virt = (new_frame_phys + phys_offset) as *mut PageTable;
        unsafe {
            (*new_table_virt).zero();
        }

        // flags: most_accessible. we change it for the last table manually.
        *entry = PageTableEntry::new(new_frame_phys, 0x7);
    }
    Some((entry.get_addr() + phys_offset) as *mut PageTable)
}

pub fn copy_kernel_half(dst: &mut PageTable, src_pml4_phys: u64) {
    let src = unsafe { &*((src_pml4_phys + *DIRECT_MAP_OFFSET as u64) as *const PageTable) };
    for idx in 256..512 {
        dst.entries[idx] = src.entries[idx];
    }
}

pub fn get_flags(
    present: bool, writable: bool, user_access: bool, writethru: bool, no_cache: bool, accessed: bool, dirty: bool, huge: bool,
    global: bool, no_execute: bool,
) -> u64 {
    let mut flags: u64 = 0;
    if present {
        flags |= 1 << 0
    }
    if writable {
        flags |= 1 << 1
    }
    if user_access {
        flags |= 1 << 2
    }
    if writethru {
        flags |= 1 << 3
    }
    if no_cache {
        flags |= 1 << 4
    }
    if accessed {
        flags |= 1 << 5
    }
    if dirty {
        flags |= 1 << 6
    }
    if huge {
        flags |= 1 << 7
    }
    if global {
        flags |= 1 << 8
    }
    if no_execute {
        flags |= 1 << 63
    }
    flags
}

// struct methods

impl PageTableEntry {
    pub fn new(phys_addr: u64, flags: u64) -> Self { Self((phys_addr & 0x000F_FFFF_FFFF_F000) | flags) }

    pub fn is_unused(&self) -> bool { self.0 == 0 }

    pub fn set_unused(&mut self) { self.0 = 0; }

    pub fn get_addr(&self) -> u64 { self.0 & 0x000F_FFFF_FFFF_F000 }

    pub fn is_present(&self) -> bool { self.0 & 1 == 1 }

    pub fn is_huge(&self) -> bool { self.0 & (1 << 7) != 0 }

    pub fn set_flags(&mut self, phys_addr: u64, flags: u64) { self.0 = (phys_addr & 0x000F_FFFF_FFFF_F000) | flags; }
}

impl PageTable {
    pub fn zero(&mut self) {
        for entry in self.entries.iter_mut() {
            entry.set_unused();
        }
    }

    pub unsafe fn translate(&self, addr: VirtAddress, physical_offset: u64) -> Option<u64> {
        let (l4, l3, l2, l1, offset) = addr.get_idxs();

        unsafe {
            let l3_table = next_table(&self.entries[l4 as usize], physical_offset)?;
            let l2_table = next_table(&(*l3_table).entries[l3 as usize], physical_offset)?;
            let l2_entry = &(*l2_table).entries[l2 as usize];

            if !l2_entry.is_present() {
                return None;
            };

            if l2_entry.is_huge() {
                return Some(l2_entry.get_addr() + addr.get_huge_offset());
            }

            let l1_table = next_table(l2_entry, physical_offset)?;
            let final_entry = &(*l1_table).entries[l1 as usize];
            if !final_entry.is_present() {
                return None;
            }

            Some(final_entry.get_addr() + offset as u64)
        }
    }

    pub unsafe fn map_page(
        &mut self, virt: VirtAddress, phys: u64, flags: u64, allocator: &'static PhysAlloc, phys_offset: u64, size: BlockSize,
    ) -> Option<()> {
        let (l4, l3, l2, l1, _) = virt.get_idxs();

        unsafe {
            let l3_table = get_or_create_next(&mut self.entries[l4 as usize], phys_offset, allocator)?;
            let l2_table = get_or_create_next(&mut (*l3_table).entries[l3 as usize], phys_offset, allocator)?;
            if size == BlockSize::Huge {
                let huge_flags = flags | (1 << 7);
                (*l2_table).entries[l2 as usize] = PageTableEntry::new(phys, huge_flags);
            } else {
                let l1_table = get_or_create_next(&mut (*l2_table).entries[l2 as usize], phys_offset, allocator)?;
                (*l1_table).entries[l1 as usize] = PageTableEntry::new(phys, flags);
            }
        }
        Some(())
    }

    pub fn unmap_page(&mut self, virt: VirtAddress, phys_offset: u64, size: BlockSize) {
        let (l4, l3, l2, l1, _) = virt.get_idxs();
        unsafe {
            let l3_table = match next_table(&self.entries[l4 as usize], phys_offset) {
                Some(t) => t as *mut PageTable,
                None => return,
            };

            let l2_table = match next_table(&(*l3_table).entries[l3 as usize], phys_offset) {
                Some(t) => t as *mut PageTable,
                None => return,
            };

            if size == BlockSize::Huge {
                (*l2_table).entries[l2 as usize].set_unused();
            } else {
                let l1_table = match next_table(&(*l2_table).entries[l2 as usize], phys_offset) {
                    Some(t) => t as *mut PageTable,
                    None => return,
                };
                (*l1_table).entries[l1 as usize].set_unused();
            }
        }
    }

    pub fn change_flags(&mut self, virt: VirtAddress, new_flags: u64, phys_offset: u64, size: BlockSize) {
        let (l4, l3, l2, l1, _) = virt.get_idxs();
        unsafe {
            let l3_table = match next_table(&self.entries[l4 as usize], phys_offset) {
                Some(t) => t as *mut PageTable,
                None => return,
            };

            let l2_table = match next_table(&(*l3_table).entries[l3 as usize], phys_offset) {
                Some(t) => t as *mut PageTable,
                None => return,
            };

            if size == BlockSize::Huge {
                let entry = &mut (*l2_table).entries[l2 as usize];
                let huge_flags = new_flags | (1 << 7);
                if entry.is_present() {
                    let phys_addr = entry.get_addr();
                    entry.set_flags(phys_addr, huge_flags);
                }
            } else {
                let l1_table = match next_table(&(*l2_table).entries[l2 as usize], phys_offset) {
                    Some(t) => t as *mut PageTable,
                    None => return,
                };

                let entry = &mut (*l1_table).entries[l1 as usize];
                if entry.is_present() {
                    let phys_addr = entry.get_addr();
                    entry.set_flags(phys_addr, new_flags);
                }
            }
        }
    }
}

impl VirtAddress {
    pub fn new(l4: u64, l3: u64, l2: u64, l1: u64, offset: u64) -> Self {
        let mut addr: u64 = 0;
        addr |= (l4 & 0o777) << 39;
        addr |= (l3 & 0o777) << 30;
        addr |= (l2 & 0o777) << 21;
        addr |= (l1 & 0o777) << 12;
        addr |= offset & 0xFFF;
        if (addr & (1 << 47)) != 0 {
            addr |= 0xffff_0000_0000_0000;
        }
        VirtAddress(addr)
    }

    pub fn get_idxs(&self) -> (u64, u64, u64, u64, u64) {
        let l4 = (self.0 >> 39) & 0o777;
        let l3 = (self.0 >> 30) & 0o777;
        let l2 = (self.0 >> 21) & 0o777;
        let l1 = (self.0 >> 12) & 0o777;
        let offset = self.0 & 0xFFF;
        (l4, l3, l2, l1, offset)
    }

    pub fn get_huge_offset(&self) -> u64 { self.0 & 0x1F_FFFF }

    pub fn get_offset(&self) -> u64 { self.0 & 0xFFF }
}

impl Pager {
    pub const fn new(allocator: &'static PhysAlloc) -> Self { Self { active_l4_addr: 0, allocator } }

    pub fn init(&mut self) -> Option<()> {
        let pml4_table_frame = { GLOBAL_PMM.lock().alloc(BlockSize::Normal)? as u64 };

        let new_pml4_table = unsafe { &mut *((pml4_table_frame + *DIRECT_MAP_OFFSET as u64) as *mut PageTable) };
        new_pml4_table.zero();

        let old_pml4_table_addr = get_cr3() & 0x000F_FFFF_FFFF_F000;
        copy_kernel_half(new_pml4_table, old_pml4_table_addr);

        load_cr3(pml4_table_frame);

        self.active_l4_addr = pml4_table_frame;
        Some(())
    }

    pub fn init_process_pager_from_kernel(&mut self, kernel_pml4_phys: u64) -> Option<()> {
        let pml4_table_frame = { GLOBAL_PMM.lock().alloc(BlockSize::Normal)? as u64 };

        let new_pml4_table = unsafe { &mut *((pml4_table_frame + *DIRECT_MAP_OFFSET as u64) as *mut PageTable) };
        new_pml4_table.zero();

        copy_kernel_half(new_pml4_table, kernel_pml4_phys);

        self.active_l4_addr = pml4_table_frame;
        Some(())
    }

    // get pml4 frame addr
    pub fn get_l4_addr(&self) -> u64 { self.active_l4_addr }

    pub fn refresh_kernel_half_from(&mut self, kernel_pml4_phys: u64) {
        let table = unsafe { &mut *((self.active_l4_addr + *DIRECT_MAP_OFFSET as u64) as *mut PageTable) };
        copy_kernel_half(table, kernel_pml4_phys);
    }

    pub fn map_page(&mut self, virt: VirtAddress, phys: u64, flags: u64, phys_offset: u64, size: BlockSize) -> Option<()> {
        if size == BlockSize::Normal {
            assert!(phys & 0xFFF == 0, "Phys address not 4k aligned");
            assert!(virt.get_offset() == 0, "Virt address not 4k aligned");
        } else {
            assert!(phys & 0x1F_FFFF == 0, "Phys address not 2M aligned");
            assert!(virt.get_huge_offset() == 0, "Virt address not 2M aligned");
        }
        unsafe {
            let active_table = &mut *((self.active_l4_addr + phys_offset) as *mut PageTable);
            active_table.map_page(virt, phys, flags, self.allocator, phys_offset, size)
        }
    }

    pub fn unmap_page(&mut self, virt: VirtAddress, phys_offset: u64, size: BlockSize) {
        if size == BlockSize::Normal {
            assert!(virt.get_offset() == 0, "Virt address not 4k aligned");
        } else {
            assert!(virt.get_huge_offset() == 0, "Virt address not 2M aligned");
        }
        unsafe {
            let active_table = &mut *((self.active_l4_addr + phys_offset) as *mut PageTable);
            active_table.unmap_page(virt, phys_offset, size);
        }
        flush_tlb(virt.0);
    }

    pub fn translate(&mut self, virt: VirtAddress, phys_offset: u64) -> Option<u64> {
        unsafe {
            let active_table = &mut *((self.active_l4_addr + phys_offset) as *mut PageTable);
            active_table.translate(virt, phys_offset)
        }
    }

    pub fn change_flags(&mut self, virt: VirtAddress, new_flags: u64, phys_offset: u64, size: BlockSize) {
        if size == BlockSize::Normal {
            assert!(virt.get_offset() == 0, "Virt address not 4k aligned");
        } else {
            assert!(virt.get_huge_offset() == 0, "Virt address not 2M aligned");
        }
        unsafe {
            let active_table = &mut *((self.active_l4_addr + phys_offset) as *mut PageTable);
            active_table.change_flags(virt, new_flags, phys_offset, size);
        }
        flush_tlb(virt.0);
    }

    pub fn map_mmio_addr(&mut self, phys: u64) -> Option<()> {
        let virt = VirtAddress(phys + *DIRECT_MAP_OFFSET as u64);
        let flags = get_flags(true, true, false, true, true, false, false, false, true, true);
        self.map_page(virt, phys, flags, *DIRECT_MAP_OFFSET as u64, BlockSize::Normal)?;
        flush_tlb(virt.0);
        Some(())
    }
}
