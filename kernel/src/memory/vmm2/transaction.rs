use alloc::vec::Vec;
use super::{Vma, VmError, super::PageSize};
use crate::memory::{range_tree::RangeMap, vmm2::VmPermissions};

#[derive(Debug, Clone)]
pub struct VmaInsert {
    pub start: usize,
    pub end: usize,
    pub vma: Vma,
}

#[derive(Debug, Clone)]
pub struct VmaRemove {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AccountingDelta {
    pub reserved_add: usize,
    pub reserved_sub: usize,

    pub committed_add: usize,
    pub committed_sub: usize,

    pub resident_add: usize,
    pub resident_sub: usize,

    pub private_add: usize,
    pub private_sub: usize,

    pub shared_add: usize,
    pub shared_sub: usize,

    pub page_table_add: usize,
    pub page_table_sub: usize,
}

#[derive(Debug, Clone)]
pub enum PagerOp {
    Unmap { start: usize, end: usize, page_size: PageSize },
    Protect { start: usize, end: usize, page_size: PageSize, permissions: VmPermissions },
    Map { start: usize, end: usize, phys: usize, page_size: PageSize, permissions: VmPermissions },
}

#[derive(Debug, Clone, Default)]
pub struct VmaTransaction {
    pub removes: Vec<VmaRemove>,
    pub inserts: Vec<VmaInsert>,
    pub pager_ops: Vec<PagerOp>,
    pub accounting: AccountingDelta,
}

impl VmaTransaction {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn remove(&mut self, start: usize, end: usize) {
        self.removes.push(VmaRemove { start, end });
    }

    pub fn insert(&mut self, start: usize, end: usize, vma: Vma) {
        if start < end {
            self.inserts.push(VmaInsert { start, end, vma });
        }
    }

    pub fn push_pager_unmap(&mut self, start: usize, end: usize, page_size: PageSize) {
        if start < end { self.pager_ops.push(PagerOp::Unmap { start, end, page_size }); }
    }

    pub fn push_pager_protect(&mut self, start: usize, end: usize, page_size: PageSize, permissions: VmPermissions) {
        if start < end { self.pager_ops.push(PagerOp::Protect { start, end, page_size, permissions }); }
    }

    pub fn push_pager_map(&mut self, start: usize, end: usize, phys: usize, page_size: PageSize, permissions: VmPermissions) {
        if start < end {
            self.pager_ops.push(PagerOp::Map { start, end, phys, page_size, permissions });
        }
    }

    pub fn add_reserved(&mut self, bytes: usize) -> Result<(), VmError> {
        self.accounting.reserved_add = self.accounting.reserved_add.checked_add(bytes).ok_or(VmError::Overflow)?;
        Ok(())
    }
    
    pub fn sub_reserved(&mut self, bytes: usize) -> Result<(), VmError> {
        self.accounting.reserved_sub = self.accounting.reserved_sub.checked_add(bytes).ok_or(VmError::Overflow)?;
        Ok(())
    }
    
    pub fn add_committed(&mut self, bytes: usize) -> Result<(), VmError> {
        self.accounting.committed_add = self.accounting.committed_add.checked_add(bytes).ok_or(VmError::Overflow)?;
        Ok(())
    }
    
    pub fn sub_committed(&mut self, bytes: usize) -> Result<(), VmError> {
        self.accounting.committed_sub = self.accounting.committed_sub.checked_add(bytes).ok_or(VmError::Overflow)?;
        Ok(())
    }

    pub fn apply_to_map(&self, map: &mut RangeMap<Vma>) -> Result<(), VmError> {
        for remove in &self.removes {
            map.remove_exact(remove.start, remove.end)?;
        }

        for insert in &self.inserts {
            map.insert(insert.start, insert.end, insert.vma.clone())?;
        }

        Ok(())
    }
}
