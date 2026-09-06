mod types;
mod accounting;
mod transaction;
use core::fmt::Debug;

use alloc::{sync::Arc, vec::Vec};
use crate::memory::{ALLOCATOR, FaultError, PageSize, shootdown::shootdown, vmm::transaction::PagerOp};
pub use types::*;
pub use accounting::*;

use crate::{memory::{PAGER, PCAllocator, range_tree::{RangeEntry, RangeMap}, vmm::transaction::{AccountingDelta, VmaTransaction}}, sync::TicketLock};
use hal::mmu::{DIRECT_MAP_OFFSET, PageFlags, PhysAddr, VirtAddr, pager::{PageTable, Pager, PagerError}};

pub const USER_NULL_GUARD_END:      usize = 0x0000_0000_0001_0000; // 64 KiB
pub const USER_IMAGE_BASE:          usize = 0x0000_0000_0040_0000; // normal ELF image area
pub const USER_DYNAMIC_BASE:        usize = 0x0000_0000_4000_0000; // mmap/reserve search base
pub const USER_INTERPRETER_BASE:    usize = 0x0000_0040_0000_0000; // current ld.so base
pub const USER_STACK_TOP:           usize = 0x0000_7000_0000_0000;
pub const USER_SPACE_LIMIT:         usize = 0x0000_8000_0000_0000; // exclusive


#[derive(Debug, Clone, Copy)]
struct NormalizedMap {
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
            VmaBacking::Vmo(_) => size,
        }
    }
}

pub struct VirtMemManager {
    vmas:               RangeMap<Vma>,
    pager:              TicketLock<Pager>,
    allocator:          &'static PCAllocator,
    accounting:         Arc<VmAccounting>,
}

impl Debug for VirtMemManager {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtMemManager").finish()
    }
}

impl VirtMemManager {
    pub fn new(allocator: &'static PCAllocator) -> Self {
        let root_frame = allocator.alloc(super::PageSize::Size4K);
        unsafe { PageTable::from_phys(PhysAddr(root_frame)).zero(); }

        let mut pager = Pager::new(PhysAddr(root_frame));
        let kernel_root = PAGER.lock().root_phys();
        pager.sync_kernel_mappings(kernel_root);

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
        let commited_bytes = vma.committed_bytes(size);

        let mut tx = VmaTransaction::new();
        tx.insert(start, start + size, vma);
        tx.add_reserved(size)?;
        tx.add_committed(commited_bytes)?;

        self.apply_transaction(tx)?;
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

                let commited_bytes = new_vma.committed_bytes(req.size);

                let mut tx = VmaTransaction::new();
                tx.insert(req.start, req.end, new_vma);
                tx.add_reserved(req.size)?;
                tx.add_committed(commited_bytes)?;

                self.apply_transaction(tx)?;
                Ok(req.start)
            },
            MapBehavior::ReplaceContained => { 
                let required = required_granularity_for_range(req.start, req.end);

                let mut working = self.vmas.clone();
                let demotion_txs = self.plan_demotions(&mut working, req.start, req.end, required)?;
                let tx = self.build_replace_contained_transaction(req.start, req.end, new_vma, &working)?;
                tx.apply_to_map(&mut working)?;

                if !working.validate() {
                    return Err(VmError::InvalidRange);
                }

                let mut all_txs = Vec::with_capacity(demotion_txs.len() + 1);
                for dtx in &demotion_txs { all_txs.push(dtx); }
                all_txs.push(&tx);
                self.execute_pager_ops(&all_txs)?;

                self.vmas = working;
                self.apply_accounting_delta(tx.accounting);

                Ok(req.start)
            },
        }
    }

    pub fn unmap_range(&mut self, addr: usize, size: usize) -> Result<(), VmError> {
        let range = normalize_range(addr, size, PageSize::Size4K)?;
        let required = required_granularity_for_range(range.start, range.end);

        // clone live map, plan and apply demotions sequentially to the clone 
        // then collect, build and apply the unmap tx to the clone then 
        // ensure integrity and commit to live

        let mut working = self.vmas.clone();
        let demotion_txs = self.plan_demotions(&mut working, range.start, range.end, required)?;
        let segments = collect_overlapping_vmas_from(&working, range.start, range.end);
        require_full_coverage(range.start, range.end, &segments)?;
        let tx = self.build_unmap_transaction(range.start, range.end, &segments)?;
        tx.apply_to_map(&mut working)?;
        
        if !working.validate() {
            return Err(VmError::InvalidRange);
        }

        let mut all_txs = Vec::with_capacity(demotion_txs.len() + 1);
        for dtx in &demotion_txs { all_txs.push(dtx); }
        all_txs.push(&tx);
        self.execute_pager_ops(&all_txs)?;

        self.vmas = working;
        self.apply_accounting_delta(tx.accounting);

        Ok(())
    }

    pub fn protect_range(&mut self, addr: usize, size: usize, permissions: VmPermissions) -> Result<(), VmError> {
        let range = normalize_range(addr, size, PageSize::Size4K)?;
        let required = required_granularity_for_range(range.start, range.end);
    
        let mut working = self.vmas.clone();
        let demotion_txs = self.plan_demotions(&mut working, range.start, range.end, required)?;
        let segments = collect_overlapping_vmas_from(&working, range.start, range.end);
        require_full_coverage(range.start, range.end, &segments)?;
        let tx = self.build_protect_transaction(range.start, range.end, permissions, &segments)?;
        tx.apply_to_map(&mut working)?;
    
        if !working.validate() {
            return Err(VmError::InvalidRange);
        }
    
        let mut all_txs = Vec::with_capacity(demotion_txs.len() + 1);
        for dtx in &demotion_txs { all_txs.push(dtx); }
        all_txs.push(&tx);
        self.execute_pager_ops(&all_txs)?;

        self.vmas = working;
        self.apply_accounting_delta(tx.accounting);
    
        Ok(())
    }


    fn build_replace_contained_transaction(&self, start: usize, end: usize, replacement: Vma, map: &RangeMap<Vma>) -> Result<VmaTransaction, VmError> {
        let old_entry = map.get(start).ok_or(VmError::NotFound)?;
        let old_start = old_entry.start;
        let old_end = old_entry.end;
        let old = old_entry.value.clone();
        
        if end > old_end {
            return Err(VmError::NotContained);
        }
        
        let replaced_size = end - start;
        let old_committed = old.committed_bytes(replaced_size);
        let new_committed = replacement.committed_bytes(replaced_size);
        
        let mut tx = VmaTransaction::new();
        tx.push_pager_unmap(start, end, old.page_size);
        tx.remove(old_start, old_end);
        
        if old_start < start {
            let left = old.clone();
            tx.insert(old_start, start, left);
        }
        
        tx.insert(start, end, replacement);
        
        if end < old_end {
            let mut right = old;
            right.backing_offset += end - old_start;
            tx.insert(end, old_end, right);
        }
        
        if new_committed > old_committed {
            tx.add_committed(new_committed - old_committed)?;
        } else {
            tx.sub_committed(old_committed - new_committed)?;
        }
        
        Ok(tx)
    }

    fn build_unmap_transaction(&self, start: usize, end: usize, segments: &[VmaSegment]) -> Result<VmaTransaction, VmError> {
        let mut tx = VmaTransaction::new();
        for segment in segments {
            let cut_start = segment.start.max(start);
            let cut_end = segment.end.min(end);
            let cut_size = cut_end - cut_start;

            tx.push_pager_unmap(cut_start, cut_end, segment.vma.page_size);

            tx.remove(segment.start, segment.end);

            if segment.start < cut_start {
                tx.insert(segment.start, cut_start, segment.vma.clone());
            }

            if cut_end < segment.end {
                let right = split_right_vma(&segment.vma, segment.start, cut_end);
                tx.insert(cut_end, segment.end, right);
            }

            tx.sub_reserved(cut_size)?;
            tx.sub_committed(segment.vma.committed_bytes(cut_size))?;
        }
        Ok(tx)
    }

    fn build_protect_transaction(&self, start: usize, end: usize, permissions: VmPermissions, segments: &[VmaSegment]) -> Result<VmaTransaction, VmError> {
        let mut tx = VmaTransaction::new();
        for segment in segments {
            let cut_start = segment.start.max(start);
            let cut_end = segment.end.min(end);

            tx.push_pager_protect(cut_start, cut_end, segment.vma.page_size, permissions);

            tx.remove(segment.start, segment.end);

            if segment.start < cut_start {
                tx.insert(segment.start, cut_start, segment.vma.clone());
            }

            let mut middle = segment.vma.clone();
            middle.permissions = permissions;
            middle.backing_offset += cut_start - segment.start;
            tx.insert(cut_start, cut_end, middle);

            if cut_end < segment.end {
                let right = split_right_vma(&segment.vma, segment.start, cut_end);
                tx.insert(cut_end, segment.end, right);
            }
        }
        Ok(tx)
    }

    fn validate_transaction(&self, tx: &VmaTransaction) -> Result<(), VmError> {
        for remove in &tx.removes {
            let existing = self.vmas.get_by_start(remove.start).ok_or(VmError::NotFound)?;
            if existing.end != remove.end { return Err(VmError::InvalidRange); }
        }

        for insert in &tx.inserts {
            if insert.start >= insert.end { return Err(VmError::InvalidRange); }
            if !range_is_aligned_to(insert.start, insert.end, insert.vma.page_size) {
                return Err(VmError::InvalidAlignment);
            }
        }

        let snap = self.accounting.snapshot();
        if tx.accounting.reserved_sub > snap.reserved_bytes + tx.accounting.reserved_add {
            return Err(VmError::Overflow);
        }
        if tx.accounting.committed_sub > snap.committed_bytes + tx.accounting.committed_add {
            return Err(VmError::Overflow);
        }

        Ok(())
    }

    fn apply_transaction(&mut self, tx: VmaTransaction) -> Result<(), VmError> {
        self.validate_transaction(&tx)?;

        // clone the live map and then apply tx to the clone. if the tx fails, the live is untouched. 
        // validate the clone before committing it to live. 
        let mut next = self.vmas.clone();
        tx.apply_to_map(&mut next)?;
        if !next.validate() { return Err(VmError::InvalidRange); }

        self.execute_pager_ops(&[&tx])?;

        self.vmas = next;
        self.apply_accounting_delta(tx.accounting);

        Ok(())
    }

    fn build_demote_segment_once_transaction(&self, segment: &VmaSegment, target_start: usize, target_end: usize) -> Result<Option<VmaTransaction>, VmError> {
        let Some(next_page_size) = segment.vma.page_size.demoted() else {
            return Ok(None);
        };

        let old_align = segment.vma.page_size.bytes();
        let demote_start = align_down(target_start, old_align).max(segment.start);
        let demote_end = align_up_checked(target_end, old_align)?.min(segment.end);

        if demote_start >= demote_end {
            return Ok(None);
        }

        let mut tx = VmaTransaction::new();
        tx.push_pager_demote(demote_start, demote_end, segment.vma.page_size);
        tx.remove(segment.start, segment.end);

        if segment.start < demote_start {
            tx.insert(segment.start, demote_start, segment.vma.clone());
        }

        let mut demoted = segment.vma.clone();
        demoted.page_size = next_page_size;
        demoted.backing_offset += demote_start - segment.start;
        tx.insert(demote_start, demote_end, demoted);

        if demote_end < segment.end {
            let right = split_right_vma(&segment.vma, segment.start, demote_end);
            tx.insert(demote_end, segment.end, right);
        }

        Ok(Some(tx))
    }

    fn plan_demotions(&self, working: &mut RangeMap<Vma>, start: usize, end: usize, required: PageSize) -> Result<Vec<VmaTransaction>, VmError> {
        let mut txs = Vec::new();

        loop {
            let segments = collect_overlapping_vmas_from(working, start, end);
            require_full_coverage(start, end, &segments)?;

            let mut changed = false;

            for segment in &segments {
                if segment.vma.page_size > required {
                    let Some(tx) = self.build_demote_segment_once_transaction(segment, start, end)? else {
                        continue;
                    };

                    tx.apply_to_map(working)?;
                    txs.push(tx);
                    changed = true;
                    break;
                }
            }

            if !changed {
                return Ok(txs);
            }
        }
    }

    pub fn handle_page_fault(&self, addr: usize, error_code: usize) -> Result<(), FaultError> {
        let _is_present = (error_code & 0x1) != 0;
        let is_write = (error_code & 0x2) != 0;
        let is_user = (error_code & 0x4) != 0;
        let is_instruction_fetch = (error_code & 0x10) != 0;

        let vma_entry = self.find_vma(addr).map_err(|_| FaultError::InvalidAddress)?;
        let vma = &vma_entry.value;

        if is_write && !vma.permissions.contains(VmPermissions::WRITE) {
            return Err(FaultError::AccessDenied);
        }
        if is_instruction_fetch && !vma.permissions.contains(VmPermissions::EXECUTE) {
            return Err(FaultError::AccessDenied);
        }
        if is_user && !vma.permissions.contains(VmPermissions::USER) {
            return Err(FaultError::AccessDenied);
        }
        if vma.permissions.contains(VmPermissions::GUARD) {
            return Err(FaultError::AccessDenied);
        }

        let page_aligned_addr = addr & !0xFFF;
        let offset_in_vma = page_aligned_addr - vma_entry.start;
        let backing_offset = vma.backing_offset + offset_in_vma;

        let phys_addr = match &vma.backing {
            VmaBacking::Reserved => return Err(FaultError::AccessDenied),
            VmaBacking::Vmo(vmo) => {
                vmo.request_page(backing_offset).map_err(|_| FaultError::OutOfMemory)?
            },
        };

        let mut alloc_wrapper = PCAllocator {};
        let mut pager = self.pager.lock();
        let mut hw_flags = vma.permissions.to_hardware_flags();
        hw_flags = hw_flags.insert(PageFlags::PRESENT);

        pager.map_page(VirtAddr(page_aligned_addr), PhysAddr(phys_addr), hw_flags, PageSize::Size4K, &mut alloc_wrapper)
            .map_err(|_| FaultError::OutOfMemory)?;

        Ok(())
    }

    fn execute_pager_ops(&self, txs: &[&VmaTransaction]) -> Result<(), VmError> {
        let mut ops_exist = false;
        for tx in txs {
            if !tx.pager_ops.is_empty() { ops_exist = true; break; }
        }
        if !ops_exist { return Ok(()); }
    
        let mut pager = self.pager.lock();
        let mut alloc_wrapper = PCAllocator {};
    
        let mut min_start = usize::MAX;
        let mut max_end = 0;
    
        for tx in txs {
            for op in &tx.pager_ops {
                match op {
                    PagerOp::Unmap { start, end, page_size } => {
                        let mut current = *start;
                        while current < *end {
                            if let Err(e) = pager.unmap_page_no_flush(VirtAddr(current), *page_size, &mut alloc_wrapper) {
                                if e != hal::mmu::pager::PagerError::NotMapped { return Err(VmError::MappingFailed); }
                            }
                            current += page_size.bytes();
                        }
                        min_start = min_start.min(*start);
                        max_end = max_end.max(*end);
                    },
                    PagerOp::Protect { start, end, page_size, permissions } => {
                        let mut current = *start;
                        let mut hw_flags = permissions.to_hardware_flags();
                        hw_flags = hw_flags.insert(PageFlags::PRESENT);
                        while current < *end {
                            if let Err(e) = pager.change_flags_no_flush(VirtAddr(current), hw_flags, *page_size) {
                                if e != hal::mmu::pager::PagerError::NotMapped { return Err(VmError::MappingFailed); }
                            }
                            current += page_size.bytes();
                        }
                        min_start = min_start.min(*start);
                        max_end = max_end.max(*end);
                    },
                    PagerOp::Demote { start, end, from } => {
                        let mut current = *start;
                        while current < *end {
                            if let Err(e) = pager.demote_page_no_flush(VirtAddr(current), *from, &mut alloc_wrapper) {
                                if e != hal::mmu::pager::PagerError::NotMapped { return Err(VmError::MappingFailed); }
                            }
                            current += from.bytes();
                        }
                        min_start = min_start.min(*start);
                        max_end = max_end.max(*end);
                    }
                    PagerOp::Map { start, end, phys, page_size, permissions } => {
                        let mut current_virt = *start;
                        let mut current_phys = *phys;
                        let mut hw_flags = permissions.to_hardware_flags();
                        hw_flags = hw_flags.insert(PageFlags::PRESENT);
                        while current_virt < *end {
                            if let Err(e) = pager.map_page(VirtAddr(current_virt), hal::mmu::PhysAddr(current_phys), hw_flags, *page_size, &mut alloc_wrapper) {
                                if e != hal::mmu::pager::PagerError::AlreadyMapped { return Err(VmError::MappingFailed); }
                            }
                            current_virt += page_size.bytes();
                            current_phys += page_size.bytes();
                        }
                        min_start = min_start.min(*start);
                        max_end = max_end.max(*end);
                    }
                }
            }
        }
    
        if max_end > min_start {
            shootdown(min_start, max_end - min_start);
        }
    
        Ok(())
    }

    fn apply_accounting_delta(&self, accounting: AccountingDelta) {
        self.accounting.add_reserved(accounting.reserved_add);
        self.accounting.sub_reserved(accounting.reserved_sub);
        self.accounting.add_committed(accounting.committed_add);
        self.accounting.sub_committed(accounting.committed_sub);
    }

    pub fn address_space_root(&self) -> usize { self.pager.lock().root_phys().0 }

    pub fn refresh_kernel_mappings(&self, kernel_root: usize) { self.pager.lock().sync_kernel_mappings(PhysAddr(kernel_root)); }

    pub fn validate(&self) -> bool { self.vmas.validate() }
}

impl Drop for VirtMemManager {
    fn drop(&mut self) {
        let mut pager = self.pager.lock();
        let mut alloc_wrapper = PCAllocator {};

        pager.free_user_page_tables(&mut alloc_wrapper);

        let root_phys = pager.root_phys().0;
        ALLOCATOR.free(root_phys, PageSize::Size4K);
    }
}

fn collect_overlapping_vmas_from(map: &RangeMap<Vma>, start: usize, end: usize) -> Vec<VmaSegment> {
    let mut segments = Vec::new();
    map.for_each(|vma| {
        if vma.end <= start || vma.start >= end { return; }
        segments.push(VmaSegment { start: vma.start, end: vma.end, vma: vma.value.clone() });
    });
    segments
}

fn require_full_coverage(start: usize, end: usize, segments: &[VmaSegment]) -> Result<(), VmError> {
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
    
    if addr & (align - 1) != 0 { return Err(VmError::InvalidArgument); }
    if backing_offset & (align - 1) != 0 { return Err(VmError::InvalidArgument); }

    let page_offset = addr & (align - 1);
    let start = align_down(addr, align);
    if start < USER_NULL_GUARD_END { return Err(VmError::InvalidRange); }

    let size_with_offset = size.checked_add(page_offset).ok_or(VmError::Overflow)?;
    let size = align_up_checked(size_with_offset, align)?;
    let end = start.checked_add(size).ok_or(VmError::Overflow)?;

    if end > USER_SPACE_LIMIT { return Err(VmError::InvalidRange); }

    let backing_offset = backing_offset.checked_sub(page_offset).ok_or(VmError::InvalidArgument)?;
    
    Ok(NormalizedMap { start, size, end, backing_offset })
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
