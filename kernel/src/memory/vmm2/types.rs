use alloc::sync::Arc;
use vespertine_abi::define_bitflags;
use crate::memory::{range_tree::RangeMapError, vmo::PagedBackingStore, PageSize};

define_bitflags! {
    pub struct VmPermissions(u16) {
        WRITE       = 1 << 0;
        EXECUTE     = 1 << 1;
        USER        = 1 << 2;
        GLOBAL      = 1 << 3;
        GUARD       = 1 << 4;
    }
}

impl VmPermissions {
    pub fn to_hardware_flags(&self) -> hal::mmu::PageFlags {
        let mut flags = hal::mmu::PageFlags(0);
        if self.contains(VmPermissions::WRITE) { flags = flags.insert(hal::mmu::PageFlags::WRITABLE); }
        if self.contains(VmPermissions::USER) { flags = flags.insert(hal::mmu::PageFlags::USER_ACCESSIBLE); }
        if !self.contains(VmPermissions::EXECUTE) { flags = flags.insert(hal::mmu::PageFlags::NO_EXECUTE); }
        flags
    }
}

#[derive(Debug, Clone)]
pub enum VmaBacking {
    Reserved,
    Anonymous,
    Vmo(Arc<dyn PagedBackingStore>),
}

impl PartialEq for VmaBacking {
    fn eq(&self, other: &Self) -> bool {
        match self {
            Self::Reserved => matches!(other, VmaBacking::Reserved),
            Self::Anonymous => matches!(other, VmaBacking::Anonymous),
            Self::Vmo(_) => matches!(other, VmaBacking::Vmo(_)),
        }
    }
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
            page_size: PageSize::Size4K, 
            charge: VmaChargeKind::Private,
        }
    }

    pub const fn user_rw() -> Self {
        Self { 
            permissions: VmPermissions::USER.union(VmPermissions::WRITE),
            cache: CachePolicy::Normal, 
            page_size: PageSize::Size4K, 
            charge: VmaChargeKind::Private,
        }
    }

    pub const fn user_rx() -> Self {
        Self { 
            permissions: VmPermissions::USER.union(VmPermissions::EXECUTE), 
            cache: CachePolicy::Normal, 
            page_size: PageSize::Size4K, 
            charge: VmaChargeKind::Private,
        }
    }

    pub const fn kernel_rw() -> Self {
        Self { 
            permissions: VmPermissions::WRITE, 
            cache: CachePolicy::Normal, 
            page_size: PageSize::Size4K, 
            charge: VmaChargeKind::Private,
        }
    }

    pub const fn guard() -> Self {
        Self { 
            permissions: VmPermissions::GUARD, 
            cache: CachePolicy::Normal, 
            page_size: PageSize::Size4K, 
            charge: VmaChargeKind::ReservedOnly,
        }
    }

    pub const fn with_page_size(mut self, page_size: PageSize) -> Self {
        self.page_size = page_size;
        self
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum VmError {
    InvalidRange,
    InvalidArgument,
    InvalidAlignment,
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
            RangeMapError::InvalidAlignment => VmError::InvalidAlignment,
            RangeMapError::Empty => VmError::NotFound,
        }
    }
}

