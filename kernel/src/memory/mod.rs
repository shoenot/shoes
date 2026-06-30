mod bootalloc;
pub mod heap;
mod init_pmm;
pub mod magazine;
pub mod paging;
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
use heap::*;
use paging::*;
use pmm::*;
pub use pmm::{
    BlockSize,
    HUGE_PAGE_SIZE,
    NORMAL_PAGE_SIZE,
};
use vespertine_common::slab::SlabAllocator;
use vmm::*;

use crate::cpu::current_core_mut;
use crate::sync::{
    KernelOnceCell,
    TicketLock,
};
use crate::process::current_process;
use crate::{
    klogln,
};

pub static DIRECT_MAP_OFFSET: KernelOnceCell<usize> = KernelOnceCell::new();

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
pub static PAGER: TicketLock<Pager> = TicketLock::new(Pager::new(&ALLOCATOR));

pub fn handle_page_fault(addr: usize, error_code: usize) -> Result<(), FaultError> {
    if let Some(proc) = current_process() {
        proc.vmm.read().handle_page_fault(addr, error_code)
    } else {
        Err(FaultError::InvalidAddress)
    }
}

#[derive(Debug)]
pub struct PCAllocator {}

impl PCAllocator {
    pub fn alloc(&self, size: BlockSize) -> usize {
        match size {
            BlockSize::Huge => GLOBAL_PMM.lock().alloc(size).expect("[FATAL] Global PMM Exhausted"),
            BlockSize::Normal => {
                let int_state = interrupts_enabled();
                disable_interrupts();
                let ret = current_core_mut().magazine.alloc();
                if int_state {
                    enable_interrupts();
                }
                ret
            }
        }
    }

    pub fn alloc_order(&self, order: usize) -> Option<usize> { GLOBAL_PMM.lock().alloc_order(order) }

    pub fn free(&self, addr: usize, size: BlockSize) {
        match size {
            BlockSize::Huge => {
                GLOBAL_PMM.lock().free(addr, size);
            }
            BlockSize::Normal => {
                let int_state = interrupts_enabled();
                disable_interrupts();
                current_core_mut().magazine.free(addr);
                if int_state {
                    enable_interrupts();
                }
            }
        }
    }

    pub fn free_order(&self, addr: usize, order: usize) { GLOBAL_PMM.lock().free_order(addr, order) }
}

pub fn init() {
    klogln!("[INFO] Initiating memory management system...");
    DIRECT_MAP_OFFSET.get_or_init(|| direct_map_offset());
    // Inititate PMM
    {
        let mut global_pmm = GLOBAL_PMM.lock();
        global_pmm.init();
    }
    klogln!("[SUCCESS] Physical memory manager operational.");
    // Inititate Pager
    {
        let mut pager = PAGER.lock();
        pager.init();
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
    PAGER.lock().map_mmio_addr(page)?;
    Some(phys as usize + *DIRECT_MAP_OFFSET)
}
