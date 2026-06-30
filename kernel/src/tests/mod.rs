use crate::klogln;

pub mod concurrency_tests;
pub mod memory_tests;
pub mod object_tests;
pub mod smp_tests;

pub const RUN_TESTS: bool = true;

pub fn run_pre_vfs_tests() {
    if !RUN_TESTS {
        return;
    }
    klogln!("========== RUNNING SYSTEM DIAGNOSITC UNIT TESTS (PHASE 1) ==========");
    concurrency_tests::run_concurrency_tests();
    memory_tests::run_pmm_tests();
    memory_tests::run_range_tree_tests();
    klogln!("================= ALL DIAGNOSTIC UNIT TESTS PASSED =================");
}

pub async fn run_post_vfs_tests() {
    if !RUN_TESTS {
        return;
    }
    klogln!("========== RUNNING SYSTEM DIAGNOSITC UNIT TESTS (PHASE 2) ==========");
    object_tests::run_object_tests().await;
    klogln!("================= ALL DIAGNOSTIC UNIT TESTS PASSED =================");
}

#[macro_export]
macro_rules! vklog {
    ($verbose:expr, $($arg:tt)*) => {
        if $verbose {
            klog!($($arg)*);
        }
    };
}

#[macro_export]
macro_rules! vklogln {
    ($verbose:expr, $($arg:tt)*) => {
        if $verbose {
            klogln!($($arg)*);
        }
    };
}
