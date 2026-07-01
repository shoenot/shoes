use alloc::sync::Arc;
use vespertine_abi::define_bitflags;
use crate::memory::{HUGE_PAGE_SIZE, NORMAL_PAGE_SIZE, range_tree::RangeMapError, vmo::PagedBackingStore};

define_bitflags! {
    pub struct VmPermissions(u16) {
        WRITE       = 1 << 0;
        EXECUTE     = 1 << 1;
        USER        = 1 << 2;
        GLOBAL      = 1 << 3;
        GUARD       = 1 << 4;
    }
}

#[derive(Debug, Clone)]
pub enum VmaBacking {
    Reserved,
    Anonymous,
    Vmo(Arc<dyn PagedBackingStore>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapBehavior {
    RequireVacant,
    ReplaceContained,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePolicy {
    Normal,
    WriteThrough,
    Uncached,
    Device,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSize {
    Normal,
    Huge,
}

impl PageSize {
    pub fn bytes(&self) -> usize {
        match self {
            Self::Normal => NORMAL_PAGE_SIZE,
            Self::Huge   => HUGE_PAGE_SIZE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmaChargeKind {
    ReservedOnly,
    Private,
    Shared,
    Device,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmaAccounting {
    pub charge: VmaChargeKind,
}

#[derive(Debug, Clone, Copy)]
pub struct VmOptions {
    pub permissions:    VmPermissions,
    pub cache:          CachePolicy,
    pub page_size:      PageSize,
    pub charge:         VmaChargeKind,
}

impl VmOptions {
    pub const fn user_ro() -> Self {
        Self { 
            permissions: VmPermissions::USER,
            cache: CachePolicy::Normal, 
            page_size: PageSize::Normal, 
            charge: VmaChargeKind::Private,
        }
    }

    pub const fn user_rw() -> Self {
        Self { 
            permissions: VmPermissions::USER.union(VmPermissions::WRITE),
            cache: CachePolicy::Normal, 
            page_size: PageSize::Normal, 
            charge: VmaChargeKind::Private,
        }
    }

    pub const fn user_rx() -> Self {
        Self { 
            permissions: VmPermissions::USER.union(VmPermissions::EXECUTE), 
            cache: CachePolicy::Normal, 
            page_size: PageSize::Normal, 
            charge: VmaChargeKind::Private,
        }
    }

    pub const fn kernel_rw() -> Self {
        Self { 
            permissions: VmPermissions::WRITE, 
            cache: CachePolicy::Normal, 
            page_size: PageSize::Normal, 
            charge: VmaChargeKind::Private,
        }
    }

    pub const fn guard() -> Self {
        Self { 
            permissions: VmPermissions::GUARD, 
            cache: CachePolicy::Normal, 
            page_size: PageSize::Normal, 
            charge: VmaChargeKind::ReservedOnly,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum VmError {
    InvalidRange,
    InvalidArgument,
    Overflow,
    Overlap,
    NotFound,
    NotContained,
    OutOfMemory,
    MappingFailed,
    AccessDenied,
}

impl From<RangeMapError> for VmError {
    fn from(value: RangeMapError) -> Self {
        match value {
            RangeMapError::Overflow => VmError::Overflow,
            RangeMapError::Overlap => VmError::Overlap,
            RangeMapError::NotFound => VmError::NotFound,
            RangeMapError::Mismatch => VmError::InvalidArgument,
            RangeMapError::InvalidAlignment => VmError::InvalidArgument,
            RangeMapError::Empty => VmError::NotFound,
        }
    }
}

