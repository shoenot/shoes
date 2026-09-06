#![no_std]
#![no_main]
extern crate alloc;
mod cpu;
mod drivers;
mod executor;
mod interrupts;
mod process;
mod memory;
mod object;
mod panic;
mod program;
mod storage;
mod sched;
mod security;
mod syscall;
mod sync;
mod init;
mod time;
mod tests;
mod util;

use alloc::sync::Arc;
use hal::platform::PlatformInit;

use ::core::sync::atomic::Ordering;
use crate::cpu::{KernelCoreData, current_core_mut, hal_boot_alloc, init_bootstrap_core};
use crate::process::KERNEL_PROCESS;
use crate::time::callout::init_timer_daemon;
use cpu::ap_entry::BSP_CR3;
use drivers::logger::LOGGER;
use hal::interrupts::enable_interrupts;
use hal::mmu::get_cr3;
use memory::{
    BOOTSTRAP_ALLOC,
    PageSize,
};
pub use vespertine_common::define_bitflags;

use crate::cpu::init_smp;
use crate::sched::dispatch::spawn_kernel_thread;
use crate::sched::priority::ThreadPriority;
use crate::time::datetime::epoch_to_datetime;
use crate::drivers::keyboard::init_keyboard_irq;
use crate::drivers::pci::{
    PCI_DEVICES,
    enumerate_pci_devices,
};
use crate::drivers::virtio::blk::init_block_device;
use crate::drivers::virtio::mmio::init_virtio;
use crate::memory::{
    GLOBAL_PMM, PAGER, hal_map_mmio
};
use crate::storage::blockdev::AsyncBlockDevice;
use crate::init::vfs_init::BLOCK_DEVICE;

#[unsafe(no_mangle)]
pub extern "C" fn kmain() -> ! {
    LOGGER.lock().init();

    memory::init();
    let bootstrap_page = GLOBAL_PMM.lock().alloc(PageSize::Size2M).unwrap() as usize;
    BOOTSTRAP_ALLOC.lock().init(bootstrap_page);

    interrupts::init();
    let platform_hooks = PlatformInit { map_mmio: hal_map_mmio };
    hal::platform::init_early(platform_hooks);
    init_bootstrap_core();
    hal::platform::init();

    klogln!("[INFO] GS Base initialized. Starting FPU...");
    hal::cpu::init_bsp_state(hal_boot_alloc);

    klogln!("[INFO] FPU initialized. Starting IOAPIC...");
    hal::interrupts::init_platform_interrupts();

    process::init_kernel_process();

    current_core_mut().scheduler.init_threads(0);

    time::init();
    let data_ptr = current_core_mut() as *mut KernelCoreData;
    init_timer_daemon(data_ptr);

    let cr3 = get_cr3();
    BSP_CR3.store(cr3, Ordering::Release);

    init_smp();

    enumerate_pci_devices();
    for dev in &*PCI_DEVICES.lock() {
        klogln!("{}", dev);
    }

    init_virtio();

    let blk = init_block_device().expect("Failed to init block device");
    let blk_arc = Arc::new(blk);

    // setup_interrupts now handles spawning per-core worker threads and MSI-X steering
    blk_arc.setup_interrupts().ok();

    {
        let kernel_pml4 = PAGER.lock().pml4_phys().0;
        KERNEL_PROCESS.vmm.write().refresh_kernel_mappings(kernel_pml4 as usize);
    }

    let blk_dyn: Arc<dyn AsyncBlockDevice> = blk_arc.clone();
    BLOCK_DEVICE.get_or_init(|| blk_dyn);

    time::init_realtime();
    klogln!("[SUCCESS] Initialized Real Time Clock.");
    klogln!("[INFO] Current date and time: {}", epoch_to_datetime(time::get_realtime().0));

    init_keyboard_irq();
    enable_interrupts();

    spawn_kernel_thread(init::initializer as *const () as usize, 0, ThreadPriority::MAXIMUM, KERNEL_PROCESS.clone());

    terminate_thread!();
}
