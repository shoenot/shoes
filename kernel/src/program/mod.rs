pub mod env;
pub mod parser;
use alloc::alloc::{
    Layout,
    alloc,
    dealloc,
};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use hal::mmu::PageSize;
use core::mem::MaybeUninit;
use core::ptr::{
    copy_nonoverlapping,
    write_bytes,
};
use core::{
    cmp,
    fmt,
};

use parser::*;
use vespertine_abi::{
    AccessRights,
    FileOp,
    FileStat,
    HandleID,
    Invocation,
};

use crate::process::Process;
use crate::object::vmo::VmoObject;
use crate::object::obj::KernelObject;
use crate::object::vfs::kernel_walk;
use crate::process::current_process;
use crate::klogln;
use crate::memory::vmm::{
    CachePolicy, MapBehavior, VmOptions, VmPermissions, VmaBacking, VmaChargeKind, 
};
use crate::memory::vmo::{
    PagedBackingStore,
    Vmo,
};
use crate::memory::{
    DIRECT_MAP_OFFSET,
    NORMAL_PAGE_SIZE,
};

#[derive(Debug)]
pub enum LoaderError {
    InvalidBuffer,
    InvalidMagicNumbers,
    NotAWashingMachine,
    Not64BitElf,
    UnsupportedElfType(u16),
    UnsupportedArch(u16),
    UnsupportedABI(u8),
    FileReadError,
}

impl fmt::Display for LoaderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoaderError::InvalidBuffer => write!(f, "InvalidBuffer"),
            LoaderError::InvalidMagicNumbers => write!(f, "Invalid ELF Magic numbers"),
            LoaderError::NotAWashingMachine => write!(f, "Big endian not supported"),
            LoaderError::Not64BitElf => write!(f, "32 bit programs not supported"),
            LoaderError::UnsupportedElfType(t) => write!(f, "Unsupported ELF type: 0x{:X}", t),
            LoaderError::UnsupportedArch(t) => write!(f, "Unsupported architechture: 0x{:X}", t),
            LoaderError::UnsupportedABI(t) => write!(f, "Unsupported ABI: 0x{:X}", t),
            LoaderError::FileReadError => write!(f, "File read or map error"),
        }
    }
}

pub struct ElfLoadResult {
    pub entry_point: usize,
    pub phdr_addr: usize,
    pub phnum: usize,
    pub base_addr: usize,
    pub interpreter_entry: Option<usize>,
}

async fn read_elf_header(file_obj: &Arc<dyn KernelObject>) -> Result<(Vec<u8>, Elf64_Ehdr), LoaderError> {
    // KernelObject file reads accept kernel destination pointers directly, so
    // loader I/O must not change the calling thread's process identity.
    let mut stat = MaybeUninit::<FileStat>::uninit();
    file_obj.invoke(Invocation::File(FileOp::Stat { stat_ptr: stat.as_mut_ptr() as usize }), AccessRights::READ).await.map_err(|e| {
        klogln!("[ERROR] read_elf_header: Stat failed: {:?}", e);
        LoaderError::FileReadError
    })?;
    let file_size = unsafe { stat.assume_init().size as usize };
    let header_read_size = cmp::min(file_size, 4096);

    let (buf_addr, file_layout) = {
        let file_layout = Layout::from_size_align(header_read_size, 8).map_err(|_| LoaderError::FileReadError)?;
        let buffer_ptr = unsafe { alloc(file_layout) };
        (buffer_ptr as usize, file_layout)
    };

    file_obj
        .invoke(Invocation::File(FileOp::Read { offset: 0, buffer_ptr: buf_addr, len: header_read_size }), AccessRights::READ)
        .await
        .map_err(|e| {
            klogln!("[ERROR] load_elf: Read failed: {:?}", e);
            LoaderError::FileReadError
        })?;

    let mut header_vec = vec![0u8; header_read_size];
    unsafe {
        copy_nonoverlapping(buf_addr as *const u8, header_vec.as_mut_ptr(), header_read_size);
        dealloc(buf_addr as *mut u8, file_layout);
    }

    let ehdr = *Elf64_Ehdr::from_bytes(&header_vec)?;
    Ok((header_vec, ehdr))
}

async fn map_elf_segments(
    file_obj: &Arc<dyn KernelObject>, header: &Elf64_Ehdr, file_bytes: &[u8], proc: &Process, load_base: usize,
) -> Result<usize, LoaderError> {
    let ph_iter = header.prog_headers(file_bytes).ok_or(LoaderError::InvalidBuffer)?;

    let vmo_handle_id = file_obj.invoke(Invocation::File(FileOp::GetVmo), AccessRights::READ).await.map_err(|e| {
        klogln!("[ERROR] load_elf: GetVmo failed: {:?}", e);
        LoaderError::FileReadError
    })?;
    let vmo_handle = HandleID(vmo_handle_id);
    let current_proc = current_process().ok_or(LoaderError::FileReadError)?;
    let vmo_obj_dyn = current_proc.handles.read().resolve(vmo_handle, AccessRights::READ).map_err(|e| {
        klogln!("[ERROR] load_elf: Resolve VmoObject handle failed: {:?}", e);
        LoaderError::FileReadError
    })?;
    let vmo_obj = vmo_obj_dyn.as_any().downcast_ref::<VmoObject>().ok_or_else(|| {
        klogln!("[ERROR] load_elf: Downcast to VmoObject failed");
        LoaderError::FileReadError
    })?;
    let file_vmo = vmo_obj.vmo.clone();
    let _ = current_proc.handles.write().close(vmo_handle);

    let mut phdr_addr = 0;
    for ph in ph_iter {
        if ph.p_type == 6 {
            // PT_PHDR
            phdr_addr = ph.p_vaddr as usize;
        }

        if ph.p_type == P_Type::PT_LOAD as u32 {
            let aligned_vaddr = (load_base + ph.p_vaddr as usize) & !0xFFF;
            let aligned_offset = (ph.p_offset & !0xFFF) as usize;
            let offset_in_page = (load_base + ph.p_vaddr as usize) & 0xFFF;
            let total_map_size = (offset_in_page + ph.p_memsz as usize + 4095) & !4095;

            let mut perms = VmPermissions::USER;
            if (ph.p_flags & PF_W) != 0 { perms = perms | VmPermissions::WRITE; }
            if (ph.p_flags & PF_X) != 0 { perms = perms | VmPermissions::EXECUTE; }

            let options = VmOptions {
                permissions: perms,
                cache: CachePolicy::Normal,
                page_size: PageSize::Size4K,
                charge: VmaChargeKind::Private,
            };

            let (segment_vmo, map_offset) = if ph.p_filesz == 0 {
                (Vmo::new(total_map_size) as Arc<dyn PagedBackingStore>, 0)
            } else if ph.p_memsz as usize > ph.p_filesz as usize || (ph.p_flags & PF_W) != 0 {
                if (ph.p_vaddr % NORMAL_PAGE_SIZE as u64) != (ph.p_offset % NORMAL_PAGE_SIZE as u64) {
                    klogln!("[ERROR] load_elf: Misaligned segment virtual address and file offset");
                    return Err(LoaderError::FileReadError);
                }

                let anon_vmo = Vmo::new(total_map_size);

                let mut progress = 0;
                let filesz = ph.p_filesz as usize;

                while progress < offset_in_page + filesz {
                    let file_offset = aligned_offset + progress;
                    let target_offset = progress;

                    let file_pfn = file_vmo.request_page(file_offset).map_err(|_| LoaderError::FileReadError)?;

                    let anon_pfn = anon_vmo.request_page(target_offset).map_err(|_| LoaderError::FileReadError)?;

                    let src_virt = file_pfn + *DIRECT_MAP_OFFSET;
                    let dest_virt = anon_pfn + *DIRECT_MAP_OFFSET;

                    unsafe {
                        copy_nonoverlapping(src_virt as *const u8, dest_virt as *mut u8, NORMAL_PAGE_SIZE);
                    }
                    progress += NORMAL_PAGE_SIZE;
                }

                // zero out any trailing bytes in the shared data/bss page
                let total_copied_bytes = offset_in_page + filesz;
                if total_copied_bytes % NORMAL_PAGE_SIZE != 0 {
                    let last_page_offset = total_copied_bytes & !(NORMAL_PAGE_SIZE - 1);
                    let last_page_pfn = anon_vmo.request_page(last_page_offset).map_err(|_| LoaderError::FileReadError)?;

                    let zero_start_offset = total_copied_bytes % NORMAL_PAGE_SIZE;
                    let zero_len = NORMAL_PAGE_SIZE - zero_start_offset;
                    let dest_virt = last_page_pfn + *DIRECT_MAP_OFFSET;

                    unsafe {
                        write_bytes((dest_virt + zero_start_offset) as *mut u8, 0, zero_len);
                    }
                }

                (anon_vmo as Arc<dyn PagedBackingStore>, 0)
            } else {
                (file_vmo.clone(), aligned_offset)
            };

            proc.vmm.write().map_at(aligned_vaddr, total_map_size, options, VmaBacking::Vmo(segment_vmo), map_offset, MapBehavior::RequireVacant).map_err(|_| {
                klogln!("[ERROR] load_elf: mmap_vmo_at failed for segment at 0x{:X}", aligned_vaddr);
                LoaderError::FileReadError
            })?;
        }
    }
    if phdr_addr == 0 {
        for ph in header.prog_headers(file_bytes).unwrap() {
            if ph.p_type == 1 && ph.p_offset == 0 {
                // PT_LOAD
                phdr_addr = (ph.p_vaddr + header.e_phoff) as usize;
                break;
            }
        }
    }
    Ok(phdr_addr)
}

pub async fn load_elf(file_handle: HandleID, proc: &Process) -> Result<ElfLoadResult, LoaderError> {
    let file_obj =
        current_process().ok_or(LoaderError::FileReadError)?.handles.read().resolve(file_handle, AccessRights::READ).map_err(|e| {
            klogln!("[ERROR] load_elf: Failed to resolve file_handle: {:?}", e);
            LoaderError::FileReadError
        })?;

    let (file_bytes, header) = read_elf_header(&file_obj).await?;

    // check if there is an interpreter (PT_INTERP) segment
    let ph_iter = header.prog_headers(&file_bytes).ok_or(LoaderError::InvalidBuffer)?;
    let mut interpreter_path: Option<String> = None;
    for ph in ph_iter {
        if ph.p_type == P_Type::PT_INTERP as u32 {
            let interp_offset = ph.p_offset as usize;
            let interp_len = ph.p_filesz as usize;
            if interp_offset + interp_len <= file_bytes.len() {
                let raw_path = &file_bytes[interp_offset..interp_offset + interp_len];
                let len = raw_path.iter().position(|&b| b == 0).unwrap_or(raw_path.len());
                if let Ok(path_str) = str::from_utf8(&raw_path[..len]) {
                    interpreter_path = Some(String::from(path_str));
                }
            }
        }
    }

    let main_phdr_addr = map_elf_segments(&file_obj, &header, &file_bytes, proc, 0).await?;

    let mut load_result = ElfLoadResult {
        entry_point: header.e_entry as usize,
        phdr_addr: main_phdr_addr,
        phnum: header.e_phnum as usize,
        base_addr: 0,
        interpreter_entry: None,
    };

    if let Some(path) = interpreter_path {
        let interp_handle = kernel_walk(&path, HandleID(0), AccessRights::READ).await.map_err(|e| {
            klogln!("[ERROR] load_elf: Failed to resolve interpreter path {}: {:?}", path, e);
            LoaderError::FileReadError
        })?;

        let interp_obj = {
            let proc = current_process().ok_or(LoaderError::FileReadError)?;
            let obj = proc.handles.read().resolve(interp_handle, AccessRights::READ).map_err(|_| LoaderError::FileReadError)?;
            let _ = proc.handles.write().close(interp_handle);
            obj
        };

        // KEEP INTERPRETER FILE ALIVE FOR PAGE FAULTS:
        proc.handles.write().insert(interp_obj.clone(), AccessRights::READ);

        let (interp_bytes, interp_header) = read_elf_header(&interp_obj).await?;

        let interp_base = 0x40_0000_0000;
        let _interp_phdr_addr = map_elf_segments(&interp_obj, &interp_header, &interp_bytes, proc, interp_base).await?;

        load_result.base_addr = interp_base;
        load_result.interpreter_entry = Some(interp_base + interp_header.e_entry as usize);
    }
    Ok(load_result)
}
