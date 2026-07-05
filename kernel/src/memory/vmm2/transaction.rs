use alloc::vec::Vec;
use super::{Vma, VmError};

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
    pub reserved_sub: usize,
    pub committed_sub: usize,
    pub resident_sub: usize,
}

#[derive(Debug, Clone, Default)]
pub struct VmaTransaction {
    pub removes: Vec<VmaRemove>,
    pub inserts: Vec<VmaInsert>,
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

    pub fn sub_reserved(&mut self, bytes: usize) -> Result<(), VmError> {
        self.accounting.reserved_sub = self.accounting.reserved_sub.checked_add(bytes).ok_or(VmError::Overflow)?;
        Ok(())
    }

    pub fn sub_committed(&mut self, bytes: usize) -> Result<(), VmError> {
        self.accounting.committed_sub = self.accounting.committed_sub.checked_add(bytes).ok_or(VmError::Overflow)?;
        Ok(())
    }
}
