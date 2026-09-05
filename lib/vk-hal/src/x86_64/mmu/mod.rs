pub mod pager;

use core::arch::asm;

use common::{define_bitflags, once::KernelOnceCell};

pub static DIRECT_MAP_OFFSET: KernelOnceCell<usize> = KernelOnceCell::new();
pub fn init_direct_map_offset(offset: usize) {
    DIRECT_MAP_OFFSET.get_or_init(|| offset);
}

#[inline(always)]
pub fn get_cr3() -> u64 {
    let cr3: u64;
    unsafe {
        asm!("mov {0}, cr3", 
            out(reg) cr3,
            options(nostack, preserves_flags));
    };
    cr3
}

#[inline(always)]
pub fn load_cr3(addr: u64) {
    unsafe {
        asm!("mov cr3, {0}",
            in(reg) addr,
            options(nostack, preserves_flags));
    };
}

#[inline(always)]
pub fn flush_tlb(virt: u64) {
    unsafe {
        asm!("invlpg [{0}]", 
            in(reg) virt,
            options(nostack, preserves_flags))
    }
}

#[inline(always)]
pub fn flush_tlb_range(start: usize, size: usize) {
    let end = start.saturating_add(size);
    let mut current = start & !0xFFF;

    while current < end {
        flush_tlb(current as u64);
        current = current.saturating_add(4096);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct PhysAddr(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct VirtAddr(pub usize);

impl VirtAddr {
    pub const fn p4_index(&self) -> usize { (self.0 >> 39) & 0o777 }
    pub const fn p3_index(&self) -> usize { (self.0 >> 30) & 0o777 }
    pub const fn p2_index(&self) -> usize { (self.0 >> 21) & 0o777 }
    pub const fn p1_index(&self) -> usize { (self.0 >> 12) & 0o777 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PageSize {
    Size4K,
    Size2M,
    Size1G,
}

impl PageSize {
    pub const fn bytes(&self) -> usize {
        match self {
            Self::Size4K => 4096,
            Self::Size2M => 512 * 4096,
            Self::Size1G => 512 * 512 * 4096,
        }
    }

    pub const fn demoted(&self) -> Option<Self> {
        match self {
            Self::Size1G => Some(Self::Size2M),
            Self::Size2M => Some(Self::Size4K),
            Self::Size4K => None,
        }
    }

    pub const fn is_base(&self) -> bool {
        matches!(self, Self::Size4K)
    }
}

pub trait FrameAllocator {
    fn allocate_frame(&mut self) -> Option<usize>;
    fn deallocate_frame(&mut self, phys_addr: usize);
}

define_bitflags! {
    pub struct PageFlags(u64) {
        PRESENT         = 1 << 0;
        WRITABLE        = 1 << 1;
        USER_ACCESSIBLE = 1 << 2; 
        WRITE_THROUGH   = 1 << 3;
        NO_CACHE        = 1 << 4;
        ACCESSED        = 1 << 5;
        DIRTY           = 1 << 6;
        HUGE_PAGE       = 1 << 7;
        GLOBAL          = 1 << 8;
        NO_EXECUTE      = 1 << 63;
    }
}

