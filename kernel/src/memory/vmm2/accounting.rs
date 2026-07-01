use core::sync::atomic::{AtomicUsize, Ordering};

use super::VmaChargeKind;

#[derive(Debug, Clone, Copy, Default)]
pub struct VmAccountingSnapshot {
    pub reserved_bytes:     usize,
    pub committed_bytes:    usize,
    pub resident_bytes:     usize,
    pub private_bytes:      usize,
    pub shared_bytes:       usize,
    pub page_table_bytes:   usize,
}

#[derive(Debug)]
pub struct VmAccounting {
    pub reserved_bytes:     AtomicUsize,
    pub committed_bytes:    AtomicUsize,
    pub resident_bytes:     AtomicUsize,
    pub private_bytes:      AtomicUsize,
    pub shared_bytes:       AtomicUsize,
    pub page_table_bytes:   AtomicUsize,
}

impl VmAccounting {
    pub const fn new() -> Self {
        Self {
            reserved_bytes:     AtomicUsize::new(0),
            committed_bytes:    AtomicUsize::new(0),
            resident_bytes:     AtomicUsize::new(0),
            private_bytes:      AtomicUsize::new(0),
            shared_bytes:       AtomicUsize::new(0),
            page_table_bytes:   AtomicUsize::new(0),
        }
    }

    pub fn snapshot(&self) -> VmAccountingSnapshot {
        VmAccountingSnapshot {
            reserved_bytes:     self.reserved_bytes.load(Ordering::Relaxed),
            committed_bytes:    self.committed_bytes.load(Ordering::Relaxed),
            resident_bytes:     self.resident_bytes.load(Ordering::Relaxed),
            private_bytes:      self.private_bytes.load(Ordering::Relaxed),
            shared_bytes:       self.shared_bytes.load(Ordering::Relaxed),
            page_table_bytes:   self.page_table_bytes.load(Ordering::Relaxed),
        }
    }

    pub fn add_reserved(&self, bytes: usize) { atomic_add(&self.reserved_bytes, bytes); }
    pub fn sub_reserved(&self, bytes: usize) { atomic_sub_saturating(&self.reserved_bytes, bytes); }

    pub fn add_committed(&self, bytes: usize) { atomic_add(&self.committed_bytes, bytes); }
    pub fn sub_committed(&self, bytes: usize) { atomic_sub_saturating(&self.committed_bytes, bytes); }

    pub fn add_resident(&self, bytes: usize, charge: VmaChargeKind) { 
        atomic_add(&self.resident_bytes, bytes); 
        match charge {
            VmaChargeKind::Private => atomic_add(&self.private_bytes, bytes),
            VmaChargeKind::Shared => atomic_add(&self.shared_bytes, bytes),
            VmaChargeKind::ReservedOnly | VmaChargeKind::Device => {},
        }
    }
    pub fn sub_resident(&self, bytes: usize, charge: VmaChargeKind) { 
        atomic_sub_saturating(&self.resident_bytes, bytes); 
        match charge {
            VmaChargeKind::Private => atomic_sub_saturating(&self.private_bytes, bytes),
            VmaChargeKind::Shared => atomic_sub_saturating(&self.shared_bytes, bytes),
            VmaChargeKind::ReservedOnly | VmaChargeKind::Device => {},
        }
    }
}

fn atomic_sub_saturating(atomic: &AtomicUsize, val: usize) {
    let _ = atomic.try_update(Ordering::Relaxed, Ordering::Relaxed, |curr| Some(curr.saturating_sub(val)));
}

fn atomic_add(atomic: &AtomicUsize, val: usize) {
    atomic.fetch_add(val, Ordering::Relaxed);
}
