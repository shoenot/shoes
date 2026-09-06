use alloc::boxed::Box;
use alloc::sync::Arc;

use async_trait::async_trait;
use hal::usercopy::{
    safe_copy_from,
    safe_copy_to,
};
use vespertine_abi::{
    AccessRights,
    FileOp,
    FileStat,
    Invocation,
    ObjectType,
};

use crate::executor::async_mutex::AsyncMutex;
use crate::object::invoke::InvocationError;
use crate::object::vmo::VmoObject;
use crate::object::obj::KernelObject;
use crate::security::permissions::FilePermissions;
use crate::sync::RwLock;
use crate::process::current_process;
use crate::memory::vmo::{
    FileVmo,
    PagedBackingStore,
};
use crate::memory::{
    ALLOCATOR,
    PageSize,
    DIRECT_MAP_OFFSET,
};
use crate::storage::fs::ext2::Ext2FileSystem;
use crate::storage::fs::ext2::permissions::file_permissions;
use crate::storage::fs::ext2::structs::DiskInode;
use crate::storage::fs::{
    VfsNode,
    VfsNodeType,
};

#[derive(Debug)]
pub struct Ext2File {
    pub fs: Arc<Ext2FileSystem>,
    pub inode_num: u32,
    pub inode_data: RwLock<DiskInode>,
    pub file_vmo: Arc<FileVmo>,
    pub write_lock: AsyncMutex<()>,
}

unsafe impl Send for Ext2File {}
unsafe impl Sync for Ext2File {}

#[async_trait]
impl KernelObject for Ext2File {
    fn type_name(&self) -> &'static str { "File" }

    fn object_type(&self) -> ObjectType { ObjectType::File }

    async fn invoke(&self, invocation: Invocation, _rights: AccessRights) -> Result<usize, InvocationError> {
        match invocation {
            Invocation::File(FileOp::Read { offset, buffer_ptr, len }) => {
                let bytes_read = self.read_bytes_async(offset, buffer_ptr, len).await?;
                Ok(bytes_read)
            }
            Invocation::File(FileOp::Stat { stat_ptr }) => {
                let inode = self.inode_data.read();

                let stat = FileStat {
                    object_type: ObjectType::File as u32,
                    mode: inode.mode as u32,
                    user: inode.uid as u32,
                    _group: 0,
                    inode: self.inode_num as u64,
                    device: 1,
                    size: inode.size as u64,
                    block_size: self.fs.block_size as u32,
                    blocks: inode.blocks as u64,
                    nlink: inode.links_count as u32,
                    atime_sec: inode.atime as i64,
                    atime_nsec: 0,
                    mtime_sec: inode.mtime as i64,
                    mtime_nsec: 0,
                    ctime_sec: inode.ctime as i64,
                    ctime_nsec: 0,
                };

                if !safe_copy_to(stat_ptr as *mut u8, &stat as *const _ as *const u8, size_of::<FileStat>()) {
                    return Err(InvocationError::InvalidPointer);
                }

                Ok(0)
            }
            Invocation::File(FileOp::GetVmo) => {
                let vmo_obj = Arc::new(VmoObject::new(self.file_vmo.clone()));
                let current_proc = current_process().ok_or(InvocationError::UnsupportedOperation)?;
                let handle_id = current_proc.handles.write().insert(vmo_obj, AccessRights::all());

                Ok(handle_id.0 as usize)
            }
            Invocation::File(FileOp::Write { offset, buffer_ptr, len }) => {
                let bytes_written = self.write_bytes_async(offset, buffer_ptr, len).await?;
                Ok(bytes_written)
            }
            Invocation::File(FileOp::Seek { .. }) => Err(InvocationError::UnsupportedOperation),
            Invocation::File(FileOp::Truncate { size }) => {
                self.truncate(size).await?;
                Ok(0)
            }
            _ => Err(InvocationError::UnsupportedOperation),
        }
    }

    fn permissions(&self) -> Option<FilePermissions> {
        let inode = self.inode_data.read();
        Some(file_permissions(inode.uid, inode.mode))
    }
}

impl Ext2File {
    async fn read_bytes_async(&self, offset: usize, buffer_ptr: usize, req_len: usize) -> Result<usize, InvocationError> {
        let file_size = self.inode_data.read().size as usize;
        if offset >= file_size {
            return Ok(0);
        };

        let bytes_available = file_size - offset;
        let read_len = core::cmp::min(bytes_available, req_len);
        if read_len == 0 {
            return Ok(0);
        }

        let mut bytes_copied = 0;

        while bytes_copied < read_len {
            let current_file_offset = offset + bytes_copied;
            let page_offset = (current_file_offset / 4096) * 4096;
            let block_internal_offset = current_file_offset % 4096;

            let phys_addr = self.file_vmo.request_page(page_offset).map_err(|_| InvocationError::InvalidPointer)?;

            let page_virt = phys_addr + *DIRECT_MAP_OFFSET;
            let chunk_size = core::cmp::min(4096 - block_internal_offset, read_len - bytes_copied);

            unsafe {
                let src_ptr = (page_virt as *const u8).add(block_internal_offset);
                let dst_ptr = (buffer_ptr as *mut u8).add(bytes_copied);

                if !safe_copy_to(dst_ptr, src_ptr, chunk_size) {
                    return Err(InvocationError::InvalidPointer);
                }
            }
            bytes_copied += chunk_size;
        }
        Ok(bytes_copied)
    }

    pub async fn write_bytes_async(&self, offset: usize, buffer_ptr: usize, req_len: usize) -> Result<usize, InvocationError> {
        let _guard = self.write_lock.lock().await;

        if req_len == 0 {
            return Ok(0);
        }

        let file_size = self.inode_data.read().size as usize;

        // resize vmo if writing past eof
        if offset + req_len > file_size {
            self.file_vmo.resize_object(offset + req_len).map_err(|_| InvocationError::OutOfMemory)?;

            let mut inode_write = self.inode_data.write();
            inode_write.size = (offset + req_len) as u32;
        }

        let mut bytes_copied = 0;

        while bytes_copied < req_len {
            let current_offset = offset + bytes_copied;
            let page_offset = (current_offset / 4096) * 4096;
            let block_internal_offset = current_offset % 4096;

            let phys_addr = self.file_vmo.request_page(page_offset).map_err(|_| InvocationError::InvalidPointer)?;
            let page_virt = phys_addr + *DIRECT_MAP_OFFSET;
            let chunk_size = core::cmp::min(4096 - block_internal_offset, req_len - bytes_copied);

            unsafe {
                let dst_ptr = (page_virt as *mut u8).add(block_internal_offset);
                let src_ptr = (buffer_ptr as *const u8).add(bytes_copied);
                if !safe_copy_from(dst_ptr, src_ptr, chunk_size) {
                    return Err(InvocationError::InvalidPointer);
                }
            }

            self.file_vmo.mark_dirty(page_offset).map_err(|_| InvocationError::InvalidPointer)?;

            bytes_copied += chunk_size;
        }
        let inode = *self.inode_data.read();
        self.fs.write_inode(self.inode_num, &inode).await.map_err(|_| InvocationError::InvalidPointer)?;

        let self_arc = {
            let active = self.fs.active_files.lock();
            active.get(&self.inode_num).and_then(|weak| weak.upgrade())
        };
        if let Some(arc) = self_arc {
            self.fs.dirty_files.lock().insert(self.inode_num, arc);
        }

        Ok(bytes_copied)
    }

    async fn truncate(&self, new_size: usize) -> Result<(), InvocationError> {
        if new_size > u32::MAX as usize {
            return Err(InvocationError::InvalidArgument);
        }

        let _guard = self.write_lock.lock().await;

        let old_size = self.inode_data.read().size as usize;
        if new_size == old_size {
            return Ok(());
        }

        let block_size = self.fs.block_size as usize;

        if new_size < old_size {
            // Zero bytes after the new EOF in the final retained block.
            if new_size % block_size != 0 {
                let block_index = new_size / block_size;
                let block_offset = new_size % block_size;

                let inode = *self.inode_data.read();
                let block_id = self.fs.resolve_file_block(&inode, block_index).await.map_err(|_| InvocationError::UnsupportedOperation)?;

                if block_id != 0 {
                    let page = ALLOCATOR.alloc(PageSize::Size4K);
                    if page == 0 {
                        return Err(InvocationError::OutOfMemory);
                    }

                    if self.fs.read_block(block_id, page as u64).await.is_err() {
                        ALLOCATOR.free(page, PageSize::Size4K);
                        return Err(InvocationError::UnsupportedOperation);
                    }

                    unsafe {
                        core::ptr::write_bytes((page + *DIRECT_MAP_OFFSET + block_offset) as *mut u8, 0, block_size - block_offset);
                    }

                    if self.fs.cache.write_block(block_id as usize, page as u64).await.is_err() {
                        ALLOCATOR.free(page, PageSize::Size4K);
                        return Err(InvocationError::UnsupportedOperation);
                    }

                    ALLOCATOR.free(page, PageSize::Size4K);
                }
            }

            let old_blocks = old_size.div_ceil(block_size);
            let new_blocks = new_size.div_ceil(block_size);

            let mut inode = *self.inode_data.read();

            for block_index in new_blocks..old_blocks {
                self.fs.clear_file_block(&mut inode, block_index).await.map_err(|_| InvocationError::UnsupportedOperation)?;
            }

            inode.size = new_size as u32;

            self.fs.write_inode(self.inode_num, &inode).await.map_err(|_| InvocationError::UnsupportedOperation)?;
            *self.inode_data.write() = inode;
        } else {
            let mut inode = *self.inode_data.read();
            inode.size = new_size as u32;

            self.fs.write_inode(self.inode_num, &inode).await.map_err(|_| InvocationError::UnsupportedOperation)?;
            *self.inode_data.write() = inode;
        }

        self.file_vmo.resize_object(new_size).map_err(|_| InvocationError::OutOfMemory)?;

        Ok(())
    }
}

#[async_trait]
impl VfsNode for Ext2File {
    async fn read_at_phys(&self, offset: usize, dest_phys: usize, len: usize) -> Result<usize, ()> {
        let file_size = self.inode_data.read().size as usize;
        if offset >= file_size {
            return Ok(0);
        }

        let bytes_available = file_size - offset;
        let read_len = core::cmp::min(bytes_available, len);
        if read_len == 0 {
            return Ok(0);
        }

        let block_size = self.fs.block_size as usize;
        let blocks_per_page = 4096 / block_size;
        let start_file_block = offset / block_size;

        let mut block_ids = [0u32; 4];
        {
            let inode = *self.inode_data.read();
            for i in 0..blocks_per_page {
                let file_block_idx = start_file_block + i;
                block_ids[i] = self.fs.resolve_file_block(&inode, file_block_idx).await.map_err(|_| ())?;
            }
        }

        // block fusion
        let is_contiguous =
            (1..blocks_per_page).all(|i| block_ids[i] != 0 && block_ids[i] == (block_ids[0] + i as u32)) && block_ids[0] != 0;

        if is_contiguous {
            let sectors_to_read = blocks_per_page as u32 * self.fs.sectors_per_block;
            let start_sector = block_ids[0] as u64 * self.fs.sectors_per_block as u64;

            let read_fut = self.fs.partition.read_sectors(start_sector, sectors_to_read, dest_phys as u64).map_err(|_| ())?;
            read_fut.await.map_err(|_| ())?;
        } else {
            for i in 0..blocks_per_page {
                let dest_blocks_phys = dest_phys + (i * block_size);
                if block_ids[i] == 0 {
                    unsafe {
                        let dest_virt = dest_blocks_phys + *DIRECT_MAP_OFFSET;
                        core::ptr::write_bytes(dest_virt as *mut u8, 0, block_size);
                    }
                } else {
                    let sector = block_ids[i] as u64 * self.fs.sectors_per_block as u64;
                    let read_fut =
                        self.fs.partition.read_sectors(sector, self.fs.sectors_per_block, dest_blocks_phys as u64).map_err(|_| ())?;

                    read_fut.await.map_err(|_| ())?;
                }
            }
        }

        Ok(read_len)
    }

    async fn write_at_phys(&self, offset: usize, src_phys: usize, len: usize) -> Result<usize, ()> {
        let _guard = self.write_lock.lock().await;

        let block_size = self.fs.block_size as usize;
        let blocks_per_page = len / block_size;
        let start_file_block = offset / block_size;

        for i in 0..blocks_per_page {
            let file_block_idx = start_file_block + i;
            let src_block_phys = src_phys + (i * block_size);

            let inode = *self.inode_data.read();
            let mut disk_block_id = self.fs.resolve_file_block(&inode, file_block_idx).await.unwrap_or(0);

            if disk_block_id == 0 {
                let mut inode = *self.inode_data.read();
                disk_block_id = self.fs.allocate_file_block(&mut inode, file_block_idx).await?;
                *self.inode_data.write() = inode;
            }

            // write direct from vmo frame to part (bypass cache)
            let sector = disk_block_id as u64 * self.fs.sectors_per_block as u64;
            let write_fut = self.fs.partition.write_sectors(sector, self.fs.sectors_per_block, src_block_phys as u64).map_err(|_| ())?;

            write_fut.await.map_err(|_| ())?;
        }

        // save metadata
        let inode = *self.inode_data.read();
        self.fs.write_inode(self.inode_num, &inode).await.map_err(|_| ())?;

        Ok(len)
    }

    fn size(&self) -> usize { self.inode_data.read().size as usize }

    fn resize(&self, new_size: usize) -> Result<(), ()> { self.file_vmo.anonymous_vmo.resize_object(new_size) }

    fn node_type(&self) -> VfsNodeType { VfsNodeType::File }
}
