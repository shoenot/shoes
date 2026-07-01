mod types;
mod accounting;
use alloc::{sync::Arc, vec::Vec};
pub use types::*;
pub use accounting::*;

use crate::{memory::{PAGER, PCAllocator, paging::Pager, range_tree::RangeMap}, storage::fs::VfsNode, sync::TicketLock};

pub const USER_NULL_GUARD_END:      usize = 0x0000_0000_0001_0000; // 64 KiB
pub const USER_IMAGE_BASE:          usize = 0x0000_0000_0040_0000; // normal ELF image area
pub const USER_DYNAMIC_BASE:        usize = 0x0000_0000_4000_0000; // mmap/reserve search base
pub const USER_INTERPRETER_BASE:    usize = 0x0000_0040_0000_0000; // current ld.so base
pub const USER_STACK_TOP:           usize = 0x0000_7000_0000_0000;
pub const USER_SPACE_LIMIT:         usize = 0x0000_8000_0000_0000; // exclusive


struct NormalizedMap {
    returned_addr:  usize,
    start:          usize,
    size:           usize,
    end:            usize,
    backing_offset: usize,
}

#[derive(Debug, Clone)]
pub struct Vma {
    pub permissions:    VmPermissions,
    pub cache:          CachePolicy,
    pub page_size:      PageSize,
    pub backing:        VmaBacking,
    pub backing_offset: usize,
    pub accounting:     VmaAccounting,
}

impl Vma {
    pub fn new(options: VmOptions, backing: VmaBacking, backing_offset: usize) -> Self {
        Self {
            permissions: options.permissions,
            cache: options.cache,
            page_size: options.page_size,
            backing,
            backing_offset,
            accounting: VmaAccounting { charge: options.charge },
        }
    }

    fn committed_bytes(&self, size: usize) -> usize {
        match self.backing {
            VmaBacking::Reserved => 0,
            VmaBacking::Anonymous | VmaBacking::Vmo(_) => size,
        }
    }
}

pub struct VirtMemManager {
    vmas:               RangeMap<Vma>,
    pager:              TicketLock<Pager>,
    allocator:          &'static PCAllocator,
    accounting:         Arc<VmAccounting>,
    pub backing_nodes:  Vec<Arc<dyn VfsNode>>,
}

impl VirtMemManager {
    pub fn new(allocator: &'static PCAllocator) -> Self {
        let mut pager = Pager::new(allocator);
        let kernel_addr_root = PAGER.lock().get_l4_addr();
        pager.init_process_pager_from_kernel(kernel_addr_root)
            .expect("Failed to initialize process pager");

        Self {
            vmas: RangeMap::new(),
            pager: TicketLock::new(pager),
            allocator,
            accounting: Arc::new(VmAccounting::new()),
            backing_nodes: Vec::new(),
        }
    }

    pub fn accounting(&self) -> Arc<VmAccounting> {
        self.accounting.clone()
    }

    pub fn reserve(&mut self, size: usize, options: VmOptions, backing: VmaBacking) -> Result<usize, VmError> {
        let align = options.page_size.bytes();
        let size = align_up_checked(size, align)?;
        let start = self.vmas.find_gap(size, align, USER_DYNAMIC_BASE, USER_SPACE_LIMIT)?.ok_or(VmError::OutOfMemory)?;

        let vma = Vma::new(options, backing, 0);
        self.accounting.add_reserved(size);
        self.accounting.add_committed(vma.committed_bytes(size));
        
        self.vmas.insert_size(start, size, vma)?;
        Ok(start)
    }

    pub fn map_at(
        &mut self, addr: usize, size: usize, options: VmOptions, 
        backing: VmaBacking, backing_offset: usize, behavior: MapBehavior
    ) -> Result<usize, VmError> {
        let req = normalize_map_request(addr, size, backing_offset, options.page_size)?;
        let new_vma = Vma::new(options, backing, req.backing_offset);
        match behavior {
            MapBehavior::RequireVacant => {
                if self.vmas.find_overlap(req.start, req.end).is_some() {
                    return Err(VmError::Overlap);
                }
                self.accounting.add_reserved(req.size);
                self.accounting.add_committed(new_vma.committed_bytes(req.size));

                self.vmas.insert(req.start, req.end, new_vma)?;
                Ok(req.returned_addr)
            },
            MapBehavior::ReplaceContained => { 
                self.replace_contained_range(req.start, req.end, new_vma)?;
                Ok(req.returned_addr)
            },
        }
    }

    fn replace_contained_range(&mut self, start: usize, end: usize, replacement: Vma) -> Result<(), VmError> {
        if start >= end { return Err(VmError::InvalidRange); }

        let old_entry = self.vmas.get(start).ok_or(VmError::NotFound)?;
        let old_start = old_entry.start;
        let old_end = old_entry.end;
        let old = old_entry.value.clone();

        if end > old_end { return  Err(VmError::NotContained); }

        let replaced_size = end - start;
        let old_committed = old.committed_bytes(replaced_size);
        let new_committed = replacement.committed_bytes(replaced_size);

        self.vmas.remove_exact(old_start, old_end)?;

        if old_start < start {
            let left = old.clone();
            self.vmas.insert(old_start, start, left)?;
        }

        self.vmas.insert(start, end, replacement)?;

        if end < old_end {
            let mut right = old;
            right.backing_offset += end - old_start;
            self.vmas.insert(end, old_end, right)?;
        }

        if new_committed > old_committed {
            self.accounting.add_committed(new_committed - old_committed);
        } else {
            self.accounting.sub_committed(old_committed - new_committed);
        }

        Ok(())
    }

    pub fn address_space_root(&self) -> usize { self.pager.lock().get_l4_addr() as usize }

    pub fn refresh_kernel_mappings(&self, kernel_root: usize) { self.pager.lock().refresh_kernel_half_from(kernel_root as u64); }
}

fn align_down(addr: usize, align: usize) -> usize {
    addr & !(align - 1)
}

fn align_up_checked(value: usize, align: usize) -> Result<usize, VmError> {
    if align == 0 || !align.is_power_of_two() {
        return Err(VmError::InvalidArgument);
    }
    value.checked_add(align - 1).map(|v| v & !(align - 1)).ok_or(VmError::Overflow)
}

fn normalize_map_request(addr: usize, size: usize, backing_offset: usize, page_size: PageSize) -> Result<NormalizedMap, VmError> {
    if size == 0 { return Err(VmError::InvalidRange); }
    let align = page_size.bytes();
    let page_offset = addr & (align - 1);
    let start = align_down(addr, align);

    let size_with_offset = size.checked_add(page_offset).ok_or(VmError::Overflow)?;
    let size = align_up_checked(size_with_offset, align)?;
    let end = start.checked_add(size).ok_or(VmError::Overflow)?;

    if end > USER_SPACE_LIMIT { return Err(VmError::InvalidRange); }

    let backing_offset = backing_offset.checked_sub(page_offset).unwrap_or(0);
    
    Ok(NormalizedMap { returned_addr: addr, start, size, end, backing_offset })
}
