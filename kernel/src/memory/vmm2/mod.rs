mod types;
mod accounting;
use alloc::{sync::Arc, vec::Vec};
pub use types::*;
pub use accounting::*;

use crate::{memory::{PAGER, PCAllocator, paging::Pager, range_tree::{RangeEntry, RangeMap}}, sync::TicketLock};

pub const USER_NULL_GUARD_END:      usize = 0x0000_0000_0001_0000; // 64 KiB
pub const USER_IMAGE_BASE:          usize = 0x0000_0000_0040_0000; // normal ELF image area
pub const USER_DYNAMIC_BASE:        usize = 0x0000_0000_4000_0000; // mmap/reserve search base
pub const USER_INTERPRETER_BASE:    usize = 0x0000_0040_0000_0000; // current ld.so base
pub const USER_STACK_TOP:           usize = 0x0000_7000_0000_0000;
pub const USER_SPACE_LIMIT:         usize = 0x0000_8000_0000_0000; // exclusive


#[derive(Debug, Clone, Copy)]
struct NormalizedMap {
    returned_addr:  usize,
    start:          usize,
    size:           usize,
    end:            usize,
    backing_offset: usize,
}

#[derive(Debug, Clone, Copy)]
struct NormalizedRange {
    start:  usize,
    size:   usize,
    end:    usize,
}

#[derive(Debug, Clone)]
struct VmaSegment {
    start:  usize,
    end:    usize,
    vma:    Vma,
}

#[derive(Debug, Clone, PartialEq)]
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
        }
    }

    pub fn accounting(&self) -> Arc<VmAccounting> {
        self.accounting.clone()
    }

    pub fn find_vma(&self, addr: usize) -> Result<RangeEntry<'_, Vma>, VmError> {
        self.vmas.get(addr).ok_or(VmError::NotFound)
    }

    pub fn find_vma_by_start(&self, start: usize) -> Result<RangeEntry<'_, Vma>, VmError> {
        self.vmas.get_by_start(start).ok_or(VmError::NotFound)
    }

    pub fn find_vma_mut(&mut self, addr: usize) -> Result<(usize, usize, &mut Vma), VmError> {
        self.vmas.get_mut(addr).ok_or(VmError::NotFound)
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
                let required = required_granularity_for_range(req.start, req.end);
                self.ensure_range_granularity(req.start, req.end, required)?;
                self.replace_contained_range(req.start, req.end, new_vma)?;
                Ok(req.returned_addr)
            },
        }
    }

    pub fn unmap_range(&mut self, addr: usize, size: usize) -> Result<(), VmError> {
        let range = normalize_range(addr, size, PageSize::Size4K)?;
        let required = required_granularity_for_range(range.start, range.end);

        self.ensure_range_granularity(range.start, range.end, required)?;

        let segments = self.collect_overlapping_vmas(range.start, range.end);
        self.require_full_coverage(range.start, range.end, &segments)?;

        for segment in segments {
            self.vmas.remove_exact(segment.start, segment.end)?;
            let cut_start = segment.start.max(range.start);
            let cut_end = segment.end.min(range.end);
            let cut_size = cut_end - cut_start;

            if segment.start < cut_start { self.vmas.insert(segment.start, cut_start, segment.vma.clone())?; }

            // TODO: add actual page mods here

            if cut_end < segment.end {
                let right = split_right_vma(&segment.vma, segment.start, cut_end);
                self.vmas.insert(cut_end, segment.end, right)?;
            }

            self.accounting.sub_reserved(cut_size);
            self.accounting.sub_committed(segment.vma.committed_bytes(cut_size));
        }
        Ok(())
    }

    pub fn protect_range(&mut self, addr: usize, size: usize, permissions: VmPermissions) -> Result<(), VmError> {
        let range = normalize_range(addr, size, PageSize::Size4K)?;
        let required = required_granularity_for_range(range.start, range.end);

        self.ensure_range_granularity(range.start, range.end, required)?;

        let segments = self.collect_overlapping_vmas(range.start, range.end);
        self.require_full_coverage(range.start, range.end, &segments)?;

        for segment in segments {
            self.vmas.remove_exact(segment.start, segment.end)?;
            let cut_start = segment.start.max(range.start);
            let cut_end = segment.end.min(range.end);

            if segment.start < cut_start { self.vmas.insert(segment.start, cut_start, segment.vma.clone())?; }

            let mut middle = segment.vma.clone();
            middle.permissions = permissions;
            middle.backing_offset += cut_start - segment.start;
            self.vmas.insert(cut_start, cut_end, middle)?;

            if cut_end < segment.end {
                let right = split_right_vma(&segment.vma, segment.start, cut_end);
                self.vmas.insert(cut_end, segment.end, right)?;
            }
        }
        Ok(())
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


    fn demote_segment_once( &mut self, segment: &VmaSegment, target_start: usize, target_end: usize) -> Result<bool, VmError> {
        // only demotes the affected page, not the whole vma. so a big vma gets split into smaller
        // vmas if the vma size > vma page size.
        let Some(next_page_size) = segment.vma.page_size.demoted() else { return Ok(false); };
    
        let old_page_size = segment.vma.page_size;
        let old_align = old_page_size.bytes();
        let demote_start = align_down(target_start, old_align).max(segment.start);
        let demote_end = align_up_checked(target_end, old_align)?.min(segment.end);
        if demote_start >= demote_end { return Ok(false); }
    
        self.vmas.remove_exact(segment.start, segment.end)?;
    
        if segment.start < demote_start {
            self.vmas.insert(segment.start, demote_start, segment.vma.clone())?;
        }
    
        let mut demoted = segment.vma.clone();
        demoted.page_size = next_page_size;
        demoted.backing_offset += demote_start - segment.start;
        self.vmas.insert(demote_start, demote_end, demoted)?;
    
        if demote_end < segment.end {
            let right = split_right_vma(&segment.vma, segment.start, demote_end);
            self.vmas.insert(demote_end, segment.end, right)?;
        }
        Ok(true)
    }

    fn ensure_range_granularity(&mut self, start: usize, end: usize, required: PageSize) -> Result<(), VmError> {
        loop {
            let segments = self.collect_overlapping_vmas(start, end);
            self.require_full_coverage(start, end, &segments)?;
            let mut changed = false;
            for segment in &segments {
                if segment.vma.page_size > required {
                    self.demote_segment_once(segment, start, end)?;
                    changed = true;
                    break;
                }
            }
            if !changed { return Ok(()); }
        }
    }

    fn collect_overlapping_vmas(&self, start: usize, end: usize) -> Vec<VmaSegment> {
        let mut segments = Vec::new();
        self.vmas.for_each(|vma| {
            if vma.end <= start || vma.start >= end { return; }
            segments.push(VmaSegment { start: vma.start, end: vma.end, vma: vma.value.clone() });
        });
        segments
    }

    fn require_full_coverage(&self, start: usize, end: usize, segments: &[VmaSegment]) -> Result<(), VmError> {
        let mut cursor = start;
        for segment in segments {
            if segment.start > cursor {
                return Err(VmError::NotFound);
            }
            if segment.end > cursor {
                cursor = segment.end;
            }
            if cursor >= end {
                return Ok(())
            }
        }
        Err(VmError::NotFound)
    }

    pub fn address_space_root(&self) -> usize { self.pager.lock().get_l4_addr() as usize }

    pub fn refresh_kernel_mappings(&self, kernel_root: usize) { self.pager.lock().refresh_kernel_half_from(kernel_root as u64); }


    pub fn validate(&self) -> bool { self.vmas.validate() }
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

fn align_down_to(addr: usize, align: usize) -> usize {
    addr & !(align - 1)
}

fn align_up_to(addr: usize, align: usize) -> Result<usize, VmError> {
    align_up_checked(addr, align)
}

fn clamp_range(start: usize, end: usize, min: usize, max: usize) -> (usize, usize) {
    (start.max(min), end.min(max))
}

fn is_aligned_to(addr: usize, page_size: PageSize) -> bool {
    addr & (page_size.bytes() - 1) == 0
}

fn range_is_aligned_to(start: usize, end: usize, page_size: PageSize) -> bool {
    is_aligned_to(start, page_size) && is_aligned_to(end, page_size)
}

fn split_right_vma(old: &Vma, old_start: usize, new_start: usize) -> Vma {
    let mut right = old.clone();
    right.backing_offset += new_start - old_start;
    right
}

fn required_granularity_for_range(start: usize, end: usize) -> PageSize {
    if range_is_aligned_to(start, end, PageSize::Size1G) {
        PageSize::Size1G
    } else if range_is_aligned_to(start, end, PageSize::Size2M) {
        PageSize::Size2M
    } else {
        PageSize::Size4K
    }
}

fn normalize_map_request(addr: usize, size: usize, backing_offset: usize, page_size: PageSize) -> Result<NormalizedMap, VmError> {
    if size == 0 { return Err(VmError::InvalidRange); }
    let align = page_size.bytes();
    let page_offset = addr & (align - 1);
    let start = align_down(addr, align);
    if start < USER_NULL_GUARD_END { return Err(VmError::InvalidRange); }

    let size_with_offset = size.checked_add(page_offset).ok_or(VmError::Overflow)?;
    let size = align_up_checked(size_with_offset, align)?;
    let end = start.checked_add(size).ok_or(VmError::Overflow)?;

    if end > USER_SPACE_LIMIT { return Err(VmError::InvalidRange); }

    let backing_offset = backing_offset.checked_sub(page_offset).ok_or(VmError::InvalidArgument)?;
    
    Ok(NormalizedMap { returned_addr: addr, start, size, end, backing_offset })
}

fn normalize_range(addr: usize, size: usize, page_size: PageSize) -> Result<NormalizedRange, VmError> {
    if size == 0 { return Err(VmError::InvalidRange); }
    let align = page_size.bytes();
    let page_offset = addr & (align - 1);
    let start = align_down(addr, align);
    if start < USER_NULL_GUARD_END { return Err(VmError::InvalidRange); }

    let size_with_offset = size.checked_add(page_offset).ok_or(VmError::Overflow)?;
    let size = align_up_checked(size_with_offset, align)?;
    let end = start.checked_add(size).ok_or(VmError::Overflow)?;

    if end > USER_SPACE_LIMIT { return Err(VmError::InvalidRange); }
    
    Ok(NormalizedRange { start, size, end })
}
