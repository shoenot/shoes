use core::slice::from_raw_parts_mut;
use core::sync::atomic::{
    AtomicU16,
    AtomicU32,
    Ordering,
};

use hal::mmu::PageSize;

pub use crate::memory::DIRECT_MAP_OFFSET;
use crate::memory::init_pmm::*;

static ORDER_MAX: usize = 11;
pub static HUGE_PAGE_SIZE: usize = 0x20_0000;
pub static NORMAL_PAGE_SIZE: usize = 0x1000;

pub const PF_FREE: u16 = 1 << 0;
pub const PF_KERNEL: u16 = 1 << 1;
pub const PF_PAGE_TABLE: u16 = 1 << 2;
pub const PF_VMO: u16 = 1 << 3;
pub const PF_PINNED: u16 = 1 << 4;
pub const PF_BUDDY_HEAD: u16 = 1 << 5;

#[repr(C)]
struct FreeBlock {
    prev: usize,
    next: usize,
}

pub struct PageFrame {
    pub refcount: AtomicU32,
    pub flags: AtomicU16,
    pub buddy_order: AtomicU16,
}

pub struct Allocator {
    freelist: [usize; ORDER_MAX + 1],
    pub pfndb: &'static mut [PageFrame],
    pub pfndb_phys_addr: usize,
}

impl Allocator {
    pub const fn new() -> Self { Self { freelist: [0; ORDER_MAX + 1], pfndb: &mut [], pfndb_phys_addr: 0 } }

    pub fn init(&mut self) {
        let mut init_allocator = BitmapPMM::init();
        let max_addr = init_allocator.max_addr;
        let total_pages = max_addr / NORMAL_PAGE_SIZE;
        let pfndb_size_bytes = total_pages * size_of::<PageFrame>();

        let meta_frame_idx = init_allocator.alloc(pfndb_size_bytes).unwrap();
        let meta_phys = meta_frame_idx * NORMAL_PAGE_SIZE;
        let meta_virt = meta_phys + *DIRECT_MAP_OFFSET;

        let pfndb_slice = unsafe { from_raw_parts_mut(meta_virt as *mut PageFrame, total_pages) };

        for frame in pfndb_slice.iter_mut() {
            frame.refcount = AtomicU32::new(1);
            frame.flags = AtomicU16::new(PF_KERNEL);
            frame.buddy_order = AtomicU16::new(0);
        }

        self.pfndb = pfndb_slice;
        self.freelist = [0; ORDER_MAX + 1];
        self.pfndb_phys_addr = meta_phys;

        let mut cursor = 0;
        while cursor < init_allocator.total_frames {
            while cursor < init_allocator.total_frames && !init_allocator.is_free(cursor) {
                cursor += 1; // skip used frames
            }
            if cursor >= init_allocator.total_frames {
                break;
            }
            let mut start_frame = cursor;
            while cursor < init_allocator.total_frames && init_allocator.is_free(cursor) {
                cursor += 1;
            }
            let end_frame = cursor;

            loop {
                let mut max_order = start_frame.trailing_zeros() as usize; // max order allowed based on alignment constraints
                max_order = if max_order > ORDER_MAX { ORDER_MAX } else { max_order };

                let mut order = max_order;
                loop {
                    let frames_needed = 1 << order;
                    if frames_needed > (end_frame - start_frame) {
                        order -= 1;
                        continue;
                    } else {
                        self.free_order(start_frame * NORMAL_PAGE_SIZE, order);
                        start_frame += 1 << order;
                        break;
                    }
                }
                if start_frame >= end_frame {
                    break;
                }
            }
        }
    }

    pub fn alloc(&mut self, size: PageSize) -> Option<usize> {
        let order = match size {
            PageSize::Size2M => 9,
            PageSize::Size4K => 0,
            PageSize::Size1G => unimplemented!(),
        };
        self.alloc_order(order)
    }

    pub fn alloc_order(&mut self, mut order: usize) -> Option<usize> {
        let target_order = order;
        let block_addr = {
            loop {
                let list_addr = self.freelist[order];

                if list_addr == 0 {
                    order += 1;
                    if order > ORDER_MAX { return None } else { continue }
                }

                let list_ptr = (list_addr + *DIRECT_MAP_OFFSET) as *mut FreeBlock;
                let block = unsafe { &mut *list_ptr };

                if block.next != 0 {
                    let next_block = unsafe {
                        let blk = (block.next + *DIRECT_MAP_OFFSET) as *mut FreeBlock;
                        &mut *blk
                    };
                    next_block.prev = 0;
                }

                self.freelist[order] = block.next;

                break list_addr;
            }
        };

        let block_pfn = block_addr / NORMAL_PAGE_SIZE;
        self.pfndb[block_pfn].refcount.store(0, Ordering::Relaxed);
        let flags = self.pfndb[block_pfn].flags.load(Ordering::Acquire);
        let new_flags = flags & !(PF_FREE | PF_BUDDY_HEAD);
        self.pfndb[block_pfn].flags.store(new_flags, Ordering::Release);

        self.split_block(block_addr, order, target_order);

        self.pfndb[block_pfn].refcount.store(1, Ordering::Relaxed);
        self.pfndb[block_pfn].flags.store(PF_KERNEL, Ordering::Relaxed);

        Some(block_addr)
    }

    fn split_block(&mut self, addr: usize, mut order: usize, target_order: usize) {
        while order > target_order {
            order -= 1;

            let buddy_addr = addr + (NORMAL_PAGE_SIZE << order);
            let buddy_ptr = (buddy_addr + *DIRECT_MAP_OFFSET) as *mut FreeBlock;

            let order_head_addr = self.freelist[order];
            if order_head_addr != 0 {
                let order_head_ptr = unsafe {
                    let blk = (self.freelist[order] + *DIRECT_MAP_OFFSET) as *mut FreeBlock;
                    &mut *blk
                };
                order_head_ptr.prev = buddy_addr;
            }

            self.freelist[order] = buddy_addr;

            unsafe {
                *buddy_ptr = FreeBlock { prev: 0, next: order_head_addr };
            }

            // pfndb shit
            let buddy_pfn = buddy_addr / NORMAL_PAGE_SIZE;
            self.pfndb[buddy_pfn].refcount.store(0, Ordering::Relaxed);
            self.pfndb[buddy_pfn].flags.store(PF_FREE | PF_BUDDY_HEAD, Ordering::Relaxed);
            self.pfndb[buddy_pfn].buddy_order.store(order as u16, Ordering::Relaxed);
        }
    }

    pub fn free(&mut self, block_addr: usize, size: PageSize) {
        let order = match size {
            PageSize::Size2M => 9,
            PageSize::Size4K => 0,
            PageSize::Size1G => unimplemented!(),
        };
        self.free_order(block_addr, order);
    }

    pub fn free_order(&mut self, mut block_addr: usize, mut order: usize) {
        let buddy_pfn = block_addr / NORMAL_PAGE_SIZE;
        let flags = self.pfndb[buddy_pfn].flags.load(Ordering::Acquire);
        if flags & PF_FREE != 0 {
            panic!("PMM DOUBLE-FREE DETECTED AT ADDR: {:#X}", block_addr);
        }
        while order < ORDER_MAX {
            let block_size = NORMAL_PAGE_SIZE << order;
            let buddy_addr = block_addr ^ block_size;
            let buddy_pfn = buddy_addr / NORMAL_PAGE_SIZE;
            let flags = self.pfndb[buddy_pfn].flags.load(Ordering::Acquire);
            let buddy_order = self.pfndb[buddy_pfn].buddy_order.load(Ordering::Relaxed);
            let is_free_and_head = (flags & (PF_FREE | PF_BUDDY_HEAD)) == (PF_FREE | PF_BUDDY_HEAD);
            let order_match = order as u16 == buddy_order;
            if is_free_and_head && order_match {
                self.pfndb[buddy_pfn].flags.fetch_and(!PF_BUDDY_HEAD, Ordering::Release);
                block_addr = self.merge_block(block_addr, buddy_addr, order);
                order += 1;
            } else {
                break;
            }
        }

        let final_pfn = block_addr / NORMAL_PAGE_SIZE;
        self.pfndb[final_pfn].refcount.store(0, Ordering::Relaxed);
        self.pfndb[final_pfn].flags.store(PF_FREE | PF_BUDDY_HEAD, Ordering::Relaxed);
        self.pfndb[final_pfn].buddy_order.store(order as u16, Ordering::Relaxed);

        let new_block_ptr = (block_addr + *DIRECT_MAP_OFFSET) as *mut FreeBlock;
        unsafe {
            let old_head = self.freelist[order];
            *new_block_ptr = FreeBlock { prev: 0, next: self.freelist[order] };

            if old_head != 0 {
                let old_head_ptr = (old_head + *DIRECT_MAP_OFFSET) as *mut FreeBlock;
                (*old_head_ptr).prev = block_addr;
            }
        }
        self.freelist[order] = block_addr;
    }

    fn merge_block(&mut self, block_addr: usize, buddy_addr: usize, order: usize) -> usize {
        let buddy_ptr = (buddy_addr + *DIRECT_MAP_OFFSET) as *mut FreeBlock;
        unsafe {
            let prev = (*buddy_ptr).prev;
            let next = (*buddy_ptr).next;

            if prev != 0 {
                let prev_ptr = (prev + *DIRECT_MAP_OFFSET) as *mut FreeBlock;
                (*prev_ptr).next = next;
            } else {
                self.freelist[order] = next;
            }

            if next != 0 {
                let next_ptr = (next + *DIRECT_MAP_OFFSET) as *mut FreeBlock;
                (*next_ptr).prev = prev;
            }

            (*buddy_ptr).prev = 0;
            (*buddy_ptr).next = 0;
        }

        // return left addr
        if block_addr < buddy_addr { block_addr } else { buddy_addr }
    }
}
