mod process;
use alloc::sync::Arc;
use hal::mmu::get_cr3;
pub use process::*;
use vespertine_abi::{AccessRights, HandleID};
use vespertine_common::once::KernelOnceCell;

use crate::{object::{fs::directory::Directory, handle::HandleTable, namespace::DirLocation, vfs::ROOT_DIRECTORY}, security::credentials::Credentials};

pub mod procman;
pub mod thread_object;

pub static KERNEL_PROCESS: KernelOnceCell<Process> = KernelOnceCell::new();

pub fn current_process<'a>() -> Option<&'a Process> {
    let thread = crate::cpu::current_core_mut().scheduler.get_current_thread();

    if thread.is_null() {
        KERNEL_PROCESS.get()
    } else {
        unsafe { Some(&(*thread).process) }
    }
}

pub fn init_kernel_process() {
    KERNEL_PROCESS.get_or_init(|| {
        let mut proc = ProcessControlBlock::new(HandleTable::new(), "Vespertine".into(), Credentials::system());
        if let Some(p) = Arc::get_mut(&mut proc) {
            p.root_addr = get_cr3() as usize & 0x000F_FFFF_FFFF_F000;
        }
        let root = ROOT_DIRECTORY
            .get_or_init(|| {
                let root_mem = Arc::new(Directory::new());
                DirLocation::root(root_mem)
            })
            .clone();
        proc.handles.write().insert_at(HandleID(0), root, AccessRights::all());
        proc.handles.write().insert_at(HandleID(1), proc.clone(), AccessRights::all());
        proc
    });
}

