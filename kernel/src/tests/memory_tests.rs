use alloc::alloc::{
    Layout,
    alloc,
    dealloc,
};
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::hint::black_box;
use core::ptr::{
    read_volatile,
    write_volatile,
};

use crate::memory::{ALLOCATOR, NORMAL_PAGE_SIZE};
use crate::memory::vmm2::*;
use crate::memory::{
    BlockSize,
    GLOBAL_PMM,
    HUGE_PAGE_SIZE,
    range_tree::{
        RangeMap,
        RangeMapError,
    }
};

use crate::{
    klog,
    klogln,
    vklog,
    vklogln,
};

pub fn test_kmalloc(print: bool) {
    unsafe {
        vklogln!(print, "");
        vklog!(print, "Running kmalloc tests... ");
        let layout = Layout::new::<u64>();
        let p1 = black_box(alloc(layout) as *mut u64);
        vklog!(print, "Allocation OK... ");

        if p1.is_null() {
            vklogln!(print, "[FAIL] p1 is null");
            panic!("MEMORY TEST FAILED");
        }

        write_volatile(p1, 0x12345678_ABCDEF01);
        if read_volatile(p1) != 0x12345678_ABCDEF01 {
            vklogln!(print, "[FAIL] Memory corruption at {:p}", p1);
            panic!("MEMORY TEST FAILED");
        }
        vklog!(print, "Write test OK... ");

        let original_addr = p1 as usize;
        dealloc(black_box(p1 as *mut u8), layout);

        let p2 = black_box(alloc(layout) as *mut u64);
        if p2 as usize != original_addr {
            vklogln!(print, "[FAIL] SLUB did not recycle pointer");
            panic!("MEMORY TEST FAILED");
        } else {
            vklogln!(print, "Recycling test OK");
        }

        dealloc(black_box(p2 as *mut u8), layout);
        vklogln!(print, "All kmalloc tests passed!");
    }
}

pub fn test_vmalloc(print: bool) {
    unsafe {
        vklogln!(print, "");
        vklog!(print, "Running vmalloc tests... ");

        let size = 8192; // 2 pages
        let layout = Layout::from_size_align(size, 4096).unwrap();
        let p_large = black_box(alloc(layout));

        if p_large.is_null() {
            vklogln!(print, "[FAIL] vmalloc failed for 8KB");
            panic!("MEMORY TEST FAILED");
        }

        if (p_large as usize) < 0x4000_0000 {
            vklog!(print, "[FAIL] vmalloc returned direct-map address instead of VMM address\n");
            panic!("MEMORY TEST FAILED");
        }
        vklog!(print, "Allocation OK... ");

        write_volatile(p_large as *mut u64, 0xAAAA_BBBB);
        if read_volatile(p_large as *mut u64) != 0xAAAA_BBBB {
            vklog!(print, "[FAIL] Demand paging failed");
            panic!("MEMORY TEST FAILED");
        }
        vklogln!(print, "Demand paging OK");

        black_box(dealloc(p_large, layout));
        vklogln!(print, "All vmalloc tests passed!");
    }
}

pub fn test_collections(print: bool) {
    vklogln!(print, "");
    vklogln!(print, "Testing rust high-level collections... ");

    vklog!(print, "    Testing boxes... ");
    let b = Box::new(42u32);
    if *b != 42 {
        vklogln!(print, "[FAIL] Box value mismatch");
        panic!("MEMORY TEST FAILED");
    }
    vklogln!(print, "Box test OK");

    vklog!(print, "    Testing vectors... ");
    let mut v = Vec::new();
    for i in 0..100 {
        v.push(i);
    }

    if v.len() != 100 || v[99] != 99 {
        vklogln!(print, "[FAIL] Vector corruption");
        panic!("MEMORY TEST FAILED");
    }
    vklogln!(print, "Vector test OK");

    vklogln!(print, "Collections tests passed!");
}

pub fn run_pmm_tests() {
    klogln!("RUNNING PMM BUDDY TESTS...");
    test_buddy_merge();
    test_huge_alignment();
    test_freelist_isolation();
    klogln!("ALL PMM TESTS PASSED!");
}

fn test_buddy_merge() {
    klog!("  Testing buddy split and merge... ");
    let mut pmm = GLOBAL_PMM.lock();

    // get order 1 block, and store the address, then free it
    let target_block = pmm.alloc_order(1).expect("Failed to alloc Order 1");
    pmm.free_order(target_block, 1);

    // get 2 order 0 blocks so they split the order 1 above
    let left_child = pmm.alloc_order(0).expect("Failed to alloc left Order 0");
    let right_child = pmm.alloc_order(0).expect("Failed to alloc right Order 0");

    // free the blocks, which should result in them merging back into the order 1 block
    pmm.free_order(right_child, 0);
    pmm.free_order(left_child, 0);

    // get the merged block addr
    let merged_block = pmm.alloc_order(1).expect("Failed to re-alloc Order 1");

    assert_eq!(target_block, merged_block, "Buddy merge failed! Expected base {:#X}, got {:#X}", target_block, merged_block);

    pmm.free_order(merged_block, 1);
    klogln!("OK");
}

fn test_huge_alignment() {
    klog!("  Testing huge page alignment... ");
    let mut pmm = GLOBAL_PMM.lock();

    let huge_frame = pmm.alloc(BlockSize::Huge).expect("Failed to allocate Huge Page");

    assert_eq!(huge_frame % HUGE_PAGE_SIZE, 0, "Alignment fault! {:#X} is not 2MB aligned.", huge_frame);

    pmm.free(huge_frame, BlockSize::Huge);
    klogln!("OK");
}

fn test_freelist_isolation() {
    klog!("  Testing freelist isolation... ");
    let mut pmm = GLOBAL_PMM.lock();

    // Allocate two blocks, ensure the allocator doesn't hand out the same frame twice.
    let block1 = pmm.alloc(BlockSize::Normal).unwrap();
    let block2 = pmm.alloc(BlockSize::Normal).unwrap();

    assert!(block1 != block2, "Allocator handed out the same frame twice! {:#X}", block1);

    pmm.free(block1, BlockSize::Normal);
    pmm.free(block2, BlockSize::Normal);
    klogln!("OK");
}

pub fn run_range_tree_tests() {
    klogln!("RUNNING RANGE TREE TESTS...");
    test_range_tree_insert_lookup();
    test_range_tree_overlap_rejection();
    test_range_tree_gap_search();
    test_range_tree_remove();
    test_range_tree_remove_stress();
    run_vmm2_tests();
    klogln!("ALL RANGE TREE TESTS PASSED!");
}

fn test_range_tree_insert_lookup() {
    klog!("  Testing range tree insert/lookup... ");
    let mut tree = RangeMap::new();

    assert_eq!(tree.insert(0x3000, 0x4000, 3), Ok(()));
    assert_eq!(tree.insert(0x1000, 0x2000, 1), Ok(()));
    assert_eq!(tree.insert(0x5000, 0x6000, 5), Ok(()));

    assert_eq!(*tree.get(0x1000).expect("missing first range").value, 1);
    assert_eq!(*tree.get(0x3FFF).expect("missing middle range").value, 3);
    assert_eq!(*tree.get(0x5001).expect("missing last range").value, 5);

    assert!(tree.get(0x2000).is_none());
    assert!(tree.get(0x4000).is_none());
    assert!(tree.validate());

    klogln!("OK");
}

fn test_range_tree_overlap_rejection() {
    klog!("  Testing range tree overlap rejection... ");
    let mut tree = RangeMap::new();

    assert_eq!(tree.insert(0x1000, 0x2000, 1), Ok(()));
    assert_eq!(tree.insert(0x2000, 0x3000, 2), Ok(()));

    assert_eq!(tree.insert(0x0800, 0x1001, 3), Err(RangeMapError::Overlap));
    assert_eq!(tree.insert(0x1800, 0x2800, 4), Err(RangeMapError::Overlap));
    assert_eq!(tree.insert(0x2FFF, 0x4000, 5), Err(RangeMapError::Overlap));
    assert_eq!(tree.insert(0x4000, 0x4000, 6), Err(RangeMapError::Empty));

    assert!(tree.validate());

    klogln!("OK");
}

fn test_range_tree_gap_search() {
    klog!("  Testing range tree gap search... ");
    let mut tree = RangeMap::new();

    assert_eq!(tree.insert(0x1000, 0x2000, 1), Ok(()));
    assert_eq!(tree.insert(0x4000, 0x5000, 2), Ok(()));
    assert_eq!(tree.insert(0x8000, 0x9000, 3), Ok(()));

    assert_eq!(tree.find_gap(0x1000, 0x1000, 0x1000, 0xA000), Ok(Some(0x2000)));
    assert_eq!(tree.find_gap(0x2000, 0x1000, 0x1000, 0xA000), Ok(Some(0x2000)));
    assert_eq!(tree.find_gap(0x3000, 0x1000, 0x1000, 0xA000), Ok(Some(0x5000)));
    assert_eq!(tree.find_gap(0x2000, 0x2000, 0x1000, 0xA000), Ok(Some(0x2000)));
    assert_eq!(tree.find_gap(0x2000, 0x2000, 0x5000, 0x8000), Ok(Some(0x6000)));
    assert_eq!(tree.find_gap(0x3000, 0x2000, 0x1000, 0xA000), Ok(None));
    assert_eq!(tree.find_gap(0x2000, 0x1000, 0x9000, 0xA000), Ok(None));

    assert!(tree.validate());

    klogln!("OK");
}

fn test_range_tree_remove() {
    klog!("  Testing range tree remove... ");
    let mut tree = RangeMap::new();

    for i in 0..32 {
        let start = 0x1000 + i * 0x2000;
        let end = start + 0x1000;
        assert_eq!(tree.insert(start, end, i), Ok(()));
    }

    assert_eq!(tree.remove(0x1000), Some(0));
    assert_eq!(tree.remove(0x1000 + 15 * 0x2000), Some(15));
    assert_eq!(tree.remove(0x1000 + 31 * 0x2000), Some(31));
    assert_eq!(tree.remove(0xDEAD), None);

    assert!(tree.get(0x1000).is_none());
    assert!(tree.get(0x1000 + 15 * 0x2000).is_none());
    assert!(tree.get(0x1000 + 31 * 0x2000).is_none());

    assert!(tree.validate());

    klogln!("OK");
}

fn test_range_tree_remove_stress() {
    klog!("  Testing range tree repeated remove/rebalance... ");
    let mut tree = RangeMap::new();

    for i in 0..128 {
        let start = 0x1000 + i * 0x3000;
        assert_eq!(tree.insert(start, start + 0x1000, i), Ok(()));
        assert!(tree.validate());
    }

    for i in (0..128).step_by(2) {
        let start = 0x1000 + i * 0x3000;
        assert_eq!(tree.remove(start), Some(i));
        assert!(tree.validate());
    }

    for i in (1..128).step_by(2) {
        let start = 0x1000 + i * 0x3000;
        assert_eq!(*tree.get(start).expect("odd range disappeared").value, i);
    }

    for i in (1..128).rev().step_by(2) {
        let start = 0x1000 + i * 0x3000;
        assert_eq!(tree.remove(start), Some(i));
        assert!(tree.validate());
    }

    assert!(tree.is_empty());
    assert!(tree.validate());

    klogln!("OK");
}

fn test_range_tree_helpers() {
    klog!("  Testing range tree helpers... ");
    let mut tree = RangeMap::new();

    assert_eq!(tree.insert_size(0x1000, 0x1000, 1), Ok(()));
    assert_eq!(tree.insert_size(usize::MAX - 0x800, 0x1000, 2), Err(RangeMapError::Overflow));
    assert_eq!(tree.insert_size(0x3000, 0, 3), Err(RangeMapError::Empty));

    {
        let (_, _, value) = tree.get_by_start_mut(0x1000).expect("missing mutable range");
        *value = 42;
    }

    assert_eq!(*tree.get(0x1000).expect("missing updated range").value, 42);

    assert_eq!(tree.remove_exact(0x1000, 0x1800), Err(RangeMapError::Mismatch));
    assert_eq!(tree.remove_exact(0x2000, 0x3000), Err(RangeMapError::NotFound));
    assert_eq!(tree.remove_exact(0x1000, 0x2000), Ok(42));

    assert!(tree.is_empty());
    assert!(tree.validate());

    klogln!("OK");
}

pub fn run_vmm2_tests() {
    klogln!("RUNNING VMM2 TESTS...");
    test_vmm2_map_and_find();
    test_vmm2_unmap_middle_splits_vma();
    test_vmm2_unmap_across_multiple_vmas();
    test_vmm2_unmap_rejects_holes_without_mutating();
    test_vmm2_protect_middle_splits_vma();
    test_vmm2_protect_rejects_holes_without_mutating();
    test_vmm2_preserves_2m_vma_for_aligned_unmap();
    test_vmm2_precise_2m_demotion_only_demotes_touched_chunk();
    test_vmm2_precise_2m_protect_only_demotes_touched_chunk();
    test_vmm2_precise_1g_to_4k_unmap_keeps_untouched_2m_chunks();
    test_vmm2_precise_1g_to_4k_protect_keeps_untouched_2m_chunks();

    klogln!("ALL VMM2 TESTS PASSED!");
}

fn test_vmm2_map_and_find() {
    klog!("  Testing vmm2 map/find... ");
    let mut vmm = VirtMemManager::new(&ALLOCATOR);
    let base = 0x4000_0000;
    let size = NORMAL_PAGE_SIZE * 4;

    assert_eq!(
        vmm.map_at(base, size, VmOptions::user_rw(), VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base),
    );

    let vma = vmm.find_vma(base).expect("mapped base missing");
    assert_eq!(vma.start, base);
    assert_eq!(vma.end, base + size);
    assert_eq!(vma.size(), size);
    assert_eq!(vma.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(vma.value.page_size, PageSize::Size4K);
    assert_eq!(vma.value.backing_offset, 0);

    assert!(vmm.find_vma(base + size - 1).is_ok());
    assert_eq!(vmm.find_vma(base + size), Err(VmError::NotFound));
    assert!(vmm.validate());

    klogln!("OK");
}

fn test_vmm2_unmap_middle_splits_vma() {
    klog!("  Testing vmm2 middle unmap split... ");
    let mut vmm = VirtMemManager::new(&ALLOCATOR);
    let base = 0x4000_0000;
    let page = NORMAL_PAGE_SIZE;
    let size = page * 4;

    assert_eq!(
        vmm.map_at(base, size, VmOptions::user_rw(), VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base),
    );

    assert_eq!(vmm.unmap_range(base + page, page * 2), Ok(()));

    let left = vmm.find_vma(base).expect("left fragment missing");
    assert_eq!(left.start, base);
    assert_eq!(left.end, base + page);
    assert_eq!(left.value.backing_offset, 0);

    assert_eq!(vmm.find_vma(base + page), Err(VmError::NotFound));
    assert_eq!(vmm.find_vma(base + page * 2), Err(VmError::NotFound));

    let right = vmm.find_vma(base + page * 3).expect("right fragment missing");
    assert_eq!(right.start, base + page * 3);
    assert_eq!(right.end, base + page * 4);
    assert_eq!(right.value.backing_offset, page * 3);

    assert!(vmm.validate());

    let snapshot = vmm.accounting().snapshot();
    assert_eq!(snapshot.reserved_bytes, page * 2);
    assert_eq!(snapshot.committed_bytes, page * 2);

    klogln!("OK");
}

fn test_vmm2_unmap_across_multiple_vmas() {
    klog!("  Testing vmm2 multi-vma unmap... ");
    let mut vmm = VirtMemManager::new(&ALLOCATOR);
    let base = 0x4000_0000;
    let page = NORMAL_PAGE_SIZE;

    assert_eq!(
        vmm.map_at(base, page * 2, VmOptions::user_rw(), VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base),
    );
    assert_eq!(
        vmm.map_at(base + page * 2, page * 2, VmOptions::user_ro(), VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base + page * 2),
    );

    assert_eq!(vmm.unmap_range(base + page, page * 2), Ok(()));

    let left = vmm.find_vma(base).expect("left remainder missing");
    assert_eq!(left.start, base);
    assert_eq!(left.end, base + page);
    assert_eq!(left.value.permissions, VmOptions::user_rw().permissions);

    assert_eq!(vmm.find_vma(base + page), Err(VmError::NotFound));
    assert_eq!(vmm.find_vma(base + page * 2), Err(VmError::NotFound));

    let right = vmm.find_vma(base + page * 3).expect("right remainder missing");
    assert_eq!(right.start, base + page * 3);
    assert_eq!(right.end, base + page * 4);
    assert_eq!(right.value.permissions, VmOptions::user_ro().permissions);
    assert_eq!(right.value.backing_offset, page);

    assert!(vmm.validate());

    let snapshot = vmm.accounting().snapshot();
    assert_eq!(snapshot.reserved_bytes, page * 2);
    assert_eq!(snapshot.committed_bytes, page * 2);

    klogln!("OK");
}

fn test_vmm2_unmap_rejects_holes_without_mutating() {
    klog!("  Testing vmm2 unmap hole rejection... ");
    let mut vmm = VirtMemManager::new(&ALLOCATOR);
    let base = 0x4000_0000;
    let page = NORMAL_PAGE_SIZE;

    assert_eq!(
        vmm.map_at(base, page, VmOptions::user_rw(), VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base),
    );
    assert_eq!(
        vmm.map_at(base + page * 2, page, VmOptions::user_ro(), VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base + page * 2),
    );

    let before = vmm.accounting().snapshot();

    assert_eq!(vmm.unmap_range(base, page * 3), Err(VmError::NotFound));

    let first = vmm.find_vma(base).expect("first mapping was mutated");
    assert_eq!(first.start, base);
    assert_eq!(first.end, base + page);
    assert_eq!(first.value.permissions, VmOptions::user_rw().permissions);

    assert_eq!(vmm.find_vma(base + page), Err(VmError::NotFound));

    let second = vmm.find_vma(base + page * 2).expect("second mapping was mutated");
    assert_eq!(second.start, base + page * 2);
    assert_eq!(second.end, base + page * 3);
    assert_eq!(second.value.permissions, VmOptions::user_ro().permissions);

    let after = vmm.accounting().snapshot();
    assert_eq!(after.reserved_bytes, before.reserved_bytes);
    assert_eq!(after.committed_bytes, before.committed_bytes);
    assert!(vmm.validate());

    klogln!("OK");
}

fn test_vmm2_protect_middle_splits_vma() {
    klog!("  Testing vmm2 middle protect split... ");
    let mut vmm = VirtMemManager::new(&ALLOCATOR);
    let base = 0x4000_0000;
    let page = NORMAL_PAGE_SIZE;
    let size = page * 4;

    assert_eq!(
        vmm.map_at(base, size, VmOptions::user_rw(), VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base),
    );

    let new_permissions = VmOptions::user_ro().permissions;
    assert_eq!(vmm.protect_range(base + page, page * 2, new_permissions), Ok(()));

    let left = vmm.find_vma(base).expect("left fragment missing");
    assert_eq!(left.start, base);
    assert_eq!(left.end, base + page);
    assert_eq!(left.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(left.value.backing_offset, 0);

    let middle = vmm.find_vma(base + page).expect("middle fragment missing");
    assert_eq!(middle.start, base + page);
    assert_eq!(middle.end, base + page * 3);
    assert_eq!(middle.value.permissions, new_permissions);
    assert_eq!(middle.value.backing_offset, page);

    let right = vmm.find_vma(base + page * 3).expect("right fragment missing");
    assert_eq!(right.start, base + page * 3);
    assert_eq!(right.end, base + page * 4);
    assert_eq!(right.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(right.value.backing_offset, page * 3);

    assert!(vmm.validate());

    let snapshot = vmm.accounting().snapshot();
    assert_eq!(snapshot.reserved_bytes, size);
    assert_eq!(snapshot.committed_bytes, size);

    klogln!("OK");
}

fn test_vmm2_protect_rejects_holes_without_mutating() {
    klog!("  Testing vmm2 protect hole rejection... ");
    let mut vmm = VirtMemManager::new(&ALLOCATOR);
    let base = 0x4000_0000;
    let page = NORMAL_PAGE_SIZE;

    assert_eq!(
        vmm.map_at(base, page, VmOptions::user_rw(), VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base),
    );
    assert_eq!(
        vmm.map_at(base + page * 2, page, VmOptions::user_rx(), VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base + page * 2),
    );

    let before = vmm.accounting().snapshot();

    assert_eq!(
        vmm.protect_range(base, page * 3, VmOptions::user_ro().permissions),
        Err(VmError::NotFound),
    );

    let first = vmm.find_vma(base).expect("first mapping was mutated");
    assert_eq!(first.value.permissions, VmOptions::user_rw().permissions);

    assert_eq!(vmm.find_vma(base + page), Err(VmError::NotFound));

    let second = vmm.find_vma(base + page * 2).expect("second mapping was mutated");
    assert_eq!(second.value.permissions, VmOptions::user_rx().permissions);

    let after = vmm.accounting().snapshot();
    assert_eq!(after.reserved_bytes, before.reserved_bytes);
    assert_eq!(after.committed_bytes, before.committed_bytes);
    assert!(vmm.validate());

    klogln!("OK");
}

fn test_vmm2_preserves_2m_vma_for_aligned_unmap() {
    klog!("  Testing vmm2 2M preservation for aligned unmap... ");
    let mut vmm = VirtMemManager::new(&ALLOCATOR);
    let base = 0x4000_0000;
    let huge = HUGE_PAGE_SIZE;

    let options = VmOptions::user_rw().with_page_size(PageSize::Size2M);

    assert_eq!(
        vmm.map_at(base, huge * 2, options, VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base),
    );

    assert_eq!(vmm.unmap_range(base, huge), Ok(()));
    assert_eq!(vmm.find_vma(base), Err(VmError::NotFound));

    let remaining = vmm.find_vma(base + huge).expect("remaining huge VMA missing");
    assert_eq!(remaining.start, base + huge);
    assert_eq!(remaining.end, base + huge * 2);
    assert_eq!(remaining.value.page_size, PageSize::Size2M);
    assert_eq!(remaining.value.backing_offset, huge);

    assert!(vmm.validate());

    let snapshot = vmm.accounting().snapshot();
    assert_eq!(snapshot.reserved_bytes, huge);
    assert_eq!(snapshot.committed_bytes, huge);

    klogln!("OK");
}

fn test_vmm2_precise_2m_demotion_only_demotes_touched_chunk() {
    klog!("  Testing vmm2 precise 2M metadata demotion for unmap... ");

    let mut vmm = VirtMemManager::new(&ALLOCATOR);
    let base = 0x4000_0000;
    let page = NORMAL_PAGE_SIZE;
    let huge = HUGE_PAGE_SIZE;

    let options = VmOptions::user_rw().with_page_size(PageSize::Size2M);

    assert_eq!(
        vmm.map_at(base, huge * 2, options, VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base),
    );

    assert_eq!(vmm.unmap_range(base + page, page), Ok(()));

    let left = vmm.find_vma(base).expect("left 4K fragment missing");
    assert_eq!(left.start, base);
    assert_eq!(left.end, base + page);
    assert_eq!(left.value.page_size, PageSize::Size4K);
    assert_eq!(left.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(left.value.backing_offset, 0);

    assert_eq!(vmm.find_vma(base + page), Err(VmError::NotFound));

    let right_4k = vmm.find_vma(base + page * 2).expect("right 4K fragment missing");
    assert_eq!(right_4k.start, base + page * 2);
    assert_eq!(right_4k.end, base + huge);
    assert_eq!(right_4k.value.page_size, PageSize::Size4K);
    assert_eq!(right_4k.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(right_4k.value.backing_offset, page * 2);

    let untouched = vmm.find_vma(base + huge).expect("untouched 2M fragment missing");
    assert_eq!(untouched.start, base + huge);
    assert_eq!(untouched.end, base + huge * 2);
    assert_eq!(untouched.value.page_size, PageSize::Size2M);
    assert_eq!(untouched.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(untouched.value.backing_offset, huge);

    assert!(vmm.validate());

    let snapshot = vmm.accounting().snapshot();
    assert_eq!(snapshot.reserved_bytes, huge * 2 - page);
    assert_eq!(snapshot.committed_bytes, huge * 2 - page);

    klogln!("OK");
}

fn test_vmm2_precise_2m_protect_only_demotes_touched_chunk() {
    klog!("  Testing vmm2 precise 2M metadata demotion for protect... ");

    let mut vmm = VirtMemManager::new(&ALLOCATOR);
    let base = 0x4000_0000;
    let page = NORMAL_PAGE_SIZE;
    let huge = HUGE_PAGE_SIZE;

    let options = VmOptions::user_rw().with_page_size(PageSize::Size2M);
    let ro = VmOptions::user_ro().permissions;

    assert_eq!(
        vmm.map_at(base, huge * 2, options, VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base),
    );

    assert_eq!(vmm.protect_range(base + page, page, ro), Ok(()));

    let left = vmm.find_vma(base).expect("left 4K fragment missing");
    assert_eq!(left.start, base);
    assert_eq!(left.end, base + page);
    assert_eq!(left.value.page_size, PageSize::Size4K);
    assert_eq!(left.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(left.value.backing_offset, 0);

    let middle = vmm.find_vma(base + page).expect("protected 4K fragment missing");
    assert_eq!(middle.start, base + page);
    assert_eq!(middle.end, base + page * 2);
    assert_eq!(middle.value.page_size, PageSize::Size4K);
    assert_eq!(middle.value.permissions, ro);
    assert_eq!(middle.value.backing_offset, page);

    let right_4k = vmm.find_vma(base + page * 2).expect("right 4K fragment missing");
    assert_eq!(right_4k.start, base + page * 2);
    assert_eq!(right_4k.end, base + huge);
    assert_eq!(right_4k.value.page_size, PageSize::Size4K);
    assert_eq!(right_4k.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(right_4k.value.backing_offset, page * 2);

    let untouched = vmm.find_vma(base + huge).expect("untouched 2M fragment missing");
    assert_eq!(untouched.start, base + huge);
    assert_eq!(untouched.end, base + huge * 2);
    assert_eq!(untouched.value.page_size, PageSize::Size2M);
    assert_eq!(untouched.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(untouched.value.backing_offset, huge);

    assert!(vmm.validate());

    let snapshot = vmm.accounting().snapshot();
    assert_eq!(snapshot.reserved_bytes, huge * 2);
    assert_eq!(snapshot.committed_bytes, huge * 2);

    klogln!("OK");
}

fn test_vmm2_precise_1g_to_4k_unmap_keeps_untouched_2m_chunks() {
    klog!("  Testing vmm2 precise 1G->4K metadata demotion for unmap... ");

    let mut vmm = VirtMemManager::new(&ALLOCATOR);
    let base = 0x4000_0000_0000;
    let page = NORMAL_PAGE_SIZE;
    let huge = HUGE_PAGE_SIZE;
    let one_gib = PageSize::Size1G.bytes();

    let options = VmOptions::user_rw().with_page_size(PageSize::Size1G);

    assert_eq!(
        vmm.map_at(base, one_gib, options, VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base),
    );

    assert_eq!(vmm.unmap_range(base + page, page), Ok(()));

    let left = vmm.find_vma(base).expect("left 4K fragment missing");
    assert_eq!(left.start, base);
    assert_eq!(left.end, base + page);
    assert_eq!(left.value.page_size, PageSize::Size4K);
    assert_eq!(left.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(left.value.backing_offset, 0);

    assert_eq!(vmm.find_vma(base + page), Err(VmError::NotFound));

    let right_4k = vmm.find_vma(base + page * 2).expect("right 4K fragment missing");
    assert_eq!(right_4k.start, base + page * 2);
    assert_eq!(right_4k.end, base + huge);
    assert_eq!(right_4k.value.page_size, PageSize::Size4K);
    assert_eq!(right_4k.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(right_4k.value.backing_offset, page * 2);

    let untouched_2m = vmm.find_vma(base + huge).expect("untouched 2M fragment missing");
    assert_eq!(untouched_2m.start, base + huge);
    assert_eq!(untouched_2m.end, base + one_gib);
    assert_eq!(untouched_2m.value.page_size, PageSize::Size2M);
    assert_eq!(untouched_2m.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(untouched_2m.value.backing_offset, huge);

    assert!(vmm.validate());

    let snapshot = vmm.accounting().snapshot();
    assert_eq!(snapshot.reserved_bytes, one_gib - page);
    assert_eq!(snapshot.committed_bytes, one_gib - page);

    klogln!("OK");
}

fn test_vmm2_precise_1g_to_4k_protect_keeps_untouched_2m_chunks() {
    klog!("  Testing vmm2 precise 1G->4K metadata demotion for protect... ");

    let mut vmm = VirtMemManager::new(&ALLOCATOR);
    let base = 0x4000_0000_0000;
    let page = NORMAL_PAGE_SIZE;
    let huge = HUGE_PAGE_SIZE;
    let one_gib = PageSize::Size1G.bytes();

    let options = VmOptions::user_rw().with_page_size(PageSize::Size1G);
    let ro = VmOptions::user_ro().permissions;

    assert_eq!(
        vmm.map_at(base, one_gib, options, VmaBacking::Anonymous, 0, MapBehavior::RequireVacant),
        Ok(base),
    );

    assert_eq!(vmm.protect_range(base + page, page, ro), Ok(()));

    let left = vmm.find_vma(base).expect("left 4K fragment missing");
    assert_eq!(left.start, base);
    assert_eq!(left.end, base + page);
    assert_eq!(left.value.page_size, PageSize::Size4K);
    assert_eq!(left.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(left.value.backing_offset, 0);

    let middle = vmm.find_vma(base + page).expect("protected 4K fragment missing");
    assert_eq!(middle.start, base + page);
    assert_eq!(middle.end, base + page * 2);
    assert_eq!(middle.value.page_size, PageSize::Size4K);
    assert_eq!(middle.value.permissions, ro);
    assert_eq!(middle.value.backing_offset, page);

    let right_4k = vmm.find_vma(base + page * 2).expect("right 4K fragment missing");
    assert_eq!(right_4k.start, base + page * 2);
    assert_eq!(right_4k.end, base + huge);
    assert_eq!(right_4k.value.page_size, PageSize::Size4K);
    assert_eq!(right_4k.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(right_4k.value.backing_offset, page * 2);

    let untouched_2m = vmm.find_vma(base + huge).expect("untouched 2M fragment missing");
    assert_eq!(untouched_2m.start, base + huge);
    assert_eq!(untouched_2m.end, base + one_gib);
    assert_eq!(untouched_2m.value.page_size, PageSize::Size2M);
    assert_eq!(untouched_2m.value.permissions, VmOptions::user_rw().permissions);
    assert_eq!(untouched_2m.value.backing_offset, huge);

    assert!(vmm.validate());

    let snapshot = vmm.accounting().snapshot();
    assert_eq!(snapshot.reserved_bytes, one_gib);
    assert_eq!(snapshot.committed_bytes, one_gib);

    klogln!("OK");
}
