mod bootalloc;
pub mod heap;
mod init_pmm;
pub mod magazine;
mod pmm;
pub mod range_tree;
pub mod vmm;
pub mod vmo;
pub mod shootdown;

use core::alloc::GlobalAlloc;
use core::sync::atomic::{
    AtomicUsize,
    Ordering,
};

pub use bootalloc::*;
use hal::boot::direct_map_offset;
use hal::interrupts::{
    disable_interrupts,
    enable_interrupts,
    interrupts_enabled,
};
use hal::mmu::{FrameAllocator, PageFlags, get_cr3, load_cr3};
use hal::mmu::pager::{PageTable, Pager};
use heap::*;
use pmm::*;
pub use pmm::{
    HUGE_PAGE_SIZE,
    NORMAL_PAGE_SIZE,
};
use vespertine_common::slab::SlabAllocator;

use crate::cpu::current_core_mut;
use crate::sync::TicketLock;
use crate::process::current_process;
use crate::{
    klogln,
};
pub use hal::mmu::{PhysAddr, VirtAddr, PageSize, DIRECT_MAP_OFFSET, pager::{PagerError}};

// wrapper that disables interrupts and reenables them (needed bc the slab code was moved to common
pub struct KernelAllocatorWrapper(SlabAllocator<KernelPageProvider>);

pub static KERNEL_HEAP_ALLOCATED: AtomicUsize = AtomicUsize::new(0);
pub static KERNEL_HEAP_ALLOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for KernelAllocatorWrapper {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        let int_state = interrupts_enabled();
        disable_interrupts();

        let ptr = unsafe { self.0.alloc(layout) };
        if !ptr.is_null() {
            KERNEL_HEAP_ALLOCATED.fetch_add(layout.size(), Ordering::Relaxed);
            KERNEL_HEAP_ALLOCATION_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        if int_state {
            enable_interrupts();
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        let int_state = interrupts_enabled();
        disable_interrupts();

        unsafe { self.0.dealloc(ptr, layout) };
        KERNEL_HEAP_ALLOCATED.fetch_sub(layout.size(), Ordering::Relaxed);
        KERNEL_HEAP_ALLOCATION_COUNT.fetch_sub(1, Ordering::Relaxed);

        if int_state {
            enable_interrupts();
        }
    }
}

pub fn kernel_heap_allocated() -> usize { KERNEL_HEAP_ALLOCATED.load(Ordering::Relaxed) }

#[global_allocator]
pub static KERNEL_ALLOCATOR: KernelAllocatorWrapper = KernelAllocatorWrapper(SlabAllocator::new(KernelPageProvider));

pub static GLOBAL_PMM: TicketLock<Allocator> = TicketLock::new(Allocator::new());
pub static ALLOCATOR: PCAllocator = PCAllocator {};
pub static PAGER: TicketLock<Pager> = TicketLock::new(Pager::new(PhysAddr(0)));

pub fn handle_page_fault(addr: usize, error_code: usize) -> Result<(), FaultError> {
    if let Some(proc) = current_process() {
        proc.vmm.read().handle_page_fault(addr, error_code)
    } else {
        Err(FaultError::InvalidAddress)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultError {
    InvalidAddress,
    AccessDenied,
    OutOfMemory,
}

#[derive(Debug)]
pub struct PCAllocator {}

impl PCAllocator {
    pub fn alloc(&self, size: PageSize) -> usize {
        match size {
            PageSize::Size2M => GLOBAL_PMM.lock().alloc(size).expect("[FATAL] Global PMM Exhausted"),
            PageSize::Size4K => {
                let int_state = interrupts_enabled();
                disable_interrupts();
                let ret = current_core_mut().magazine.alloc();
                if int_state {
                    enable_interrupts();
                }
                ret
            },
            PageSize::Size1G => unimplemented!(),
        }
    }

    pub fn alloc_order(&self, order: usize) -> Option<usize> { GLOBAL_PMM.lock().alloc_order(order) }

    pub fn free(&self, addr: usize, size: PageSize) {
        match size {
            PageSize::Size2M => GLOBAL_PMM.lock().free(addr, size),
            PageSize::Size4K => {
                let int_state = interrupts_enabled();
                disable_interrupts();
                current_core_mut().magazine.free(addr);
                if int_state {
                    enable_interrupts();
                }
            },
            PageSize::Size1G => unimplemented!(),
        }
    }

    pub fn free_order(&self, addr: usize, order: usize) { GLOBAL_PMM.lock().free_order(addr, order) }
}

impl FrameAllocator for PCAllocator {
    fn allocate_frame(&mut self) -> Option<usize> {
        Some(self.alloc(PageSize::Size4K))
    }

    fn deallocate_frame(&mut self, phys_addr: usize) {
        self.free(phys_addr, PageSize::Size4K)
    }
}

pub fn init() {
    klogln!("[INFO] Initiating memory management system...");
    hal::mmu::init_direct_map_offset(direct_map_offset());
    // Inititate PMM
    {
        let mut global_pmm = GLOBAL_PMM.lock();
        global_pmm.init();
    }
    klogln!("[SUCCESS] Physical memory manager operational.");
    // Inititate Pager
    {
        let boot_cr3 = get_cr3() & 0x000F_FFFF_FFFF_F000;

        let new_pml4 = GLOBAL_PMM.lock().alloc(PageSize::Size4K).expect("Failed to allocate kernel PML4");

        unsafe { PageTable::from_phys(PhysAddr(new_pml4)).zero(); }
        let mut kernel_pager = Pager::new(PhysAddr(new_pml4));

        kernel_pager.sync_kernel_mappings(PhysAddr(boot_cr3 as usize));

        load_cr3(new_pml4 as u64);

        *PAGER.lock() = kernel_pager;
    }
    klogln!("[SUCCESS] Switched CR3. Paging handover complete.");
}

pub fn calculate_order(bytes: usize) -> usize {
    let mut order = 0;
    while (1 << order) * 4096 < bytes {
        order += 1;
    }
    order
}

pub fn hal_map_mmio(phys: u64, _size: usize) -> Option<usize> {
    let page = phys & !0xFFF;
    let flags = hal::mmu::PageFlags::PRESENT | hal::mmu::PageFlags::WRITABLE | hal::mmu::PageFlags::NO_EXECUTE | hal::mmu::PageFlags::NO_CACHE | hal::mmu::PageFlags::WRITE_THROUGH;
    let mut alloc_wrapper = PCAllocator {};

    let result = PAGER.lock().map_page(
        hal::mmu::VirtAddr(page as usize + *DIRECT_MAP_OFFSET),
        hal::mmu::PhysAddr(page as usize),
        flags,
        hal::mmu::PageSize::Size4K,
        &mut alloc_wrapper
    );

    if let Err(e) = result {
        if e != hal::mmu::pager::PagerError::AlreadyMapped {
            return None;
        }
    }

    Some(phys as usize + *DIRECT_MAP_OFFSET)
}
