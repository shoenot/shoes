use alloc::boxed::Box;
use alloc::sync::{
    Arc,
    Weak,
};
use core::ptr::{
    self,
    copy_nonoverlapping,
};

use async_trait::async_trait;
use hal::usercopy::safe_copy_to;
use vespertine_abi::protocol::{
    AbiDirEntry,
    DirEntryType,
    PacketFlags,
    PacketHeader,
    VESPER_MAGIC,
};
use vespertine_abi::{
    AccessRights,
    DirectoryOp,
    FileOp,
    FileStat,
    Invocation,
    ObjectType,
    UserID,
};

use super::file::Ext2File;
use crate::executor::async_mutex::AsyncMutex;
use crate::object::invoke::InvocationError;
use crate::object::fs::directory::{
    FILENAME_LEN_MAX,
    Filename,
    validate_child_name,
};
use crate::object::obj::{
    KernelDirectory,
    KernelObject,
};
use crate::object::vfs::FileDescription;
use crate::security::permissions::{
    FilePermissions,
    allowed_rights,
};
use crate::sync::RwLock;
use crate::process::current_process;
use crate::time::get_realtime;
use crate::memory::vmo::FileVmo;
use crate::memory::{
    ALLOCATOR, DIRECT_MAP_OFFSET, PageSize, PhysBuffer
};
use crate::storage::fs::VfsNode;
use crate::storage::fs::ext2::Ext2FileSystem;
use crate::storage::fs::ext2::permissions::directory_permissions;
use crate::storage::fs::ext2::structs::{
    DiskDirHeader,
    DiskInode,
};

#[derive(Debug)]
pub struct Ext2Directory {
    pub fs: Arc<Ext2FileSystem>,
    pub inode_num: u32,
    pub inode_data: RwLock<DiskInode>,
}

#[async_trait]
impl KernelObject for Ext2Directory {
    fn type_name(&self) -> &'static str { "Directory" }

    fn object_type(&self) -> ObjectType { ObjectType::Directory }

    fn as_directory(&self) -> Option<&dyn KernelDirectory> { Some(self) }

    async fn invoke(&self, invocation: Invocation, _calling_rights: AccessRights) -> Result<usize, InvocationError> {
        match invocation {
            Invocation::Directory(DirectoryOp::Lookup { name, name_len }) => {
                let filename = Filename::new(name as *const u8, name_len)?;
                let object = KernelDirectory::lookup_child(self, &filename.name).await?;

                let proc = current_process().ok_or(InvocationError::InvalidHandle)?;
                let handle = proc.handles.write().insert(object, AccessRights::all());

                Ok(handle.0)
            }
            Invocation::File(FileOp::Stat { stat_ptr }) => {
                let inode = self.inode_data.read();

                let stat = FileStat {
                    object_type: ObjectType::Directory as u32,
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
            Invocation::Directory(DirectoryOp::List { offset: _, sink }) => {
                let mut entries = alloc::vec::Vec::new();

                let buffer = PhysBuffer::new().ok_or(InvocationError::OutOfMemory)?;

                for direct_idx in 0..12 {
                    let block_id = unsafe { self.inode_data.read().data.blocks.direct[direct_idx] };
                    if block_id == 0 {
                        continue;
                    };

                    if self.fs.read_block(block_id, buffer.phys() as u64).await.is_err() {
                        return Err(InvocationError::InvalidPointer);
                    }

                    let mut offset = 0;
                    while offset < self.fs.block_size as usize {
                        unsafe {
                            let entry_ptr = (buffer.virt() as *const u8).add(offset) as *const DiskDirHeader;
                            let inode_id = (*entry_ptr).inode;
                            let rec_len = (*entry_ptr).record_length as usize;
                            let name_len = (*entry_ptr).name_length as usize;

                            if rec_len == 0 {
                                break;
                            }

                            if inode_id != 0 && name_len > 0 && offset + 8 + name_len <= self.fs.block_size as usize {
                                let name_ptr = (entry_ptr as *const u8).add(8);
                                let name_slice = core::slice::from_raw_parts(name_ptr, name_len);

                                if let Ok(entry_name) = core::str::from_utf8(name_slice) {
                                    if entry_name != "." && entry_name != ".." {
                                        entries.push((alloc::string::ToString::to_string(entry_name), (*entry_ptr).file_type));
                                    }
                                }
                            }
                            offset += rec_len;
                        }
                    }
                }

                // resolve sink socket
                let proc = current_process().ok_or(InvocationError::InvalidHandle)?;
                let sink_obj = proc.handles.read().resolve(sink, AccessRights::WRITE)?;

                crate::executor::Executor::new().spawn(async move {
                    let mut iter = entries.iter().peekable();
                    while let Some((name_str, file_type)) = iter.next() {
                        let mut entry = AbiDirEntry {
                            entry_type: match *file_type {
                                2 => DirEntryType::Directory as u8,
                                1 => DirEntryType::File as u8,
                                _ => DirEntryType::Object as u8,
                            },
                            name_len: core::cmp::min(name_str.len(), 254) as u8,
                            name: [0u8; 254],
                        };
                        let len = entry.name_len as usize;
                        entry.name[..len].copy_from_slice(&name_str.as_bytes()[..len]);

                        let mut flags = PacketFlags::IS_STREAM;
                        if iter.peek().is_some() {
                            flags = flags.insert(PacketFlags::HAS_NEXT);
                        }

                        let header = PacketHeader {
                            magic: VESPER_MAGIC,
                            version: 1,
                            packet_flags: flags,
                            packet_type: 1,
                            payload_len: core::mem::size_of::<AbiDirEntry>() as u32,
                            reserved: 0,
                        };

                        let mut buffer = [0u8; core::mem::size_of::<PacketHeader>() + core::mem::size_of::<AbiDirEntry>()];
                        let header_size = core::mem::size_of::<PacketHeader>();
                        let entry_size = core::mem::size_of::<AbiDirEntry>();
                        unsafe {
                            let header_ptr = &header as *const _ as *const u8;
                            let entry_ptr = &entry as *const _ as *const u8;
                            copy_nonoverlapping(header_ptr, buffer.as_mut_ptr(), header_size);
                            copy_nonoverlapping(entry_ptr, buffer.as_mut_ptr().add(header_size), entry_size);
                        }

                        let op = FileOp::Write { offset: 0, buffer_ptr: buffer.as_mut_ptr() as usize, len: buffer.len() };
                        if sink_obj.invoke(Invocation::File(op), AccessRights::WRITE).await.is_err() {
                            break;
                        }
                    }
                });

                Ok(0)
            }

            Invocation::Directory(DirectoryOp::Link { .. }) => Err(InvocationError::UnsupportedOperation),

            Invocation::Directory(DirectoryOp::Unlink { name, name_len }) => {
                let filename = Filename::new(name as *const u8, name_len)?;
                KernelDirectory::unlink_child(self, &filename.name).await?;
                Ok(0)
            }
            Invocation::Directory(DirectoryOp::CreateFile { name, name_len }) => {
                let filename = Filename::new(name as *const u8, name_len)?;
                let owner = current_process().ok_or(InvocationError::InvalidHandle)?.credentials.user();
                let object = KernelDirectory::create_child_file(self, &filename.name, owner).await?;
                register_created_object(object)
            }

            Invocation::Directory(DirectoryOp::CreateDir { name, name_len }) => {
                let filename = Filename::new(name as *const u8, name_len)?;
                let owner = current_process().ok_or(InvocationError::InvalidHandle)?.credentials.user();
                let object = KernelDirectory::create_child_dir(self, &filename.name, owner).await?;
                register_created_object(object)
            }
            _ => Err(InvocationError::UnsupportedOperation),
        }
    }

    fn permissions(&self) -> Option<FilePermissions> {
        let inode = self.inode_data.read();
        Some(directory_permissions(inode.uid, inode.mode))
    }
}

#[async_trait]
impl KernelDirectory for Ext2Directory {
    async fn lookup_child(&self, name: &str) -> Result<Arc<dyn KernelObject>, InvocationError> {
        if name.len() > FILENAME_LEN_MAX {
            return Err(InvocationError::NameTooLong);
        }

        let child_inode_id = self
            .fs
            .lookup_in_dir(&self.inode_data.read(), name)
            .await
            .map_err(|_| InvocationError::PathNotFound)?
            .ok_or(InvocationError::PathNotFound)?;

        let child_inode_data = self.fs.read_inode(child_inode_id).await.map_err(|_| InvocationError::PathNotFound)?;

        let is_directory = (child_inode_data.mode & 0xF000) == 0x4000;

        let target_object: Arc<dyn KernelObject> = if is_directory {
            let mut dirs = self.fs.active_dirs.lock();
            let mut cached = None;
            if let Some(weak_dir) = dirs.get(&child_inode_id) {
                cached = weak_dir.upgrade();
            }
            if let Some(arc_dir) = cached {
                arc_dir
            } else {
                let new_dir = Arc::new(Ext2Directory {
                    fs: Arc::clone(&self.fs),
                    inode_num: child_inode_id,
                    inode_data: RwLock::new(child_inode_data),
                });
                dirs.insert(child_inode_id, Arc::downgrade(&new_dir));
                new_dir
            }
        } else {
            let mut files = self.fs.active_files.lock();
            let mut cached = None;
            if let Some(weak_file) = files.get(&child_inode_id) {
                cached = weak_file.upgrade();
            }

            let base_file = if let Some(arc_file) = cached {
                arc_file
            } else {
                // new_cyclic passes a weak ptr to the ext2file being built
                let new_file = Arc::new_cyclic(|me| {
                    let weak_node = me.clone() as Weak<dyn VfsNode>;

                    Ext2File {
                        fs: Arc::clone(&self.fs),
                        inode_num: child_inode_id,
                        inode_data: RwLock::new(child_inode_data.clone()),
                        file_vmo: FileVmo::new(child_inode_data.size as usize, weak_node),
                        write_lock: AsyncMutex::new(()),
                    }
                });
                files.insert(child_inode_id, Arc::downgrade(&new_file));
                new_file
            };

            Arc::new(FileDescription::new(base_file as Arc<dyn KernelObject>)) as Arc<dyn KernelObject>
        };

        Ok(target_object)
    }

    async fn unlink_child(&self, name: &str) -> Result<(), InvocationError> {
        validate_child_name(name)?;

        let child_inode_num = self
            .fs
            .lookup_in_dir(&self.inode_data.read(), name)
            .await
            .map_err(|_| InvocationError::PathNotFound)?
            .ok_or(InvocationError::PathNotFound)?;

        let mut child_inode = self.fs.read_inode(child_inode_num).await.map_err(|_| InvocationError::UnsupportedOperation)?;
        let is_dir = (child_inode.mode & 0xF000) == 0x4000;

        self.remove_dir_entry(name).await.map_err(|_| InvocationError::UnsupportedOperation)?;

        if child_inode.links_count > 0 {
            child_inode.links_count -= 1;
        }

        if child_inode.links_count == 0 {
            let block_size = self.fs.block_size as usize;
            let total_blocks = child_inode.size as usize / block_size;
            for block_idx in 0..total_blocks {
                let block_id = self.fs.resolve_file_block(&child_inode, block_idx).await.unwrap_or(0);
                if block_id != 0 {
                    self.fs.free_block(block_id).await.map_err(|_| InvocationError::UnsupportedOperation)?;
                }
            }

            let single_indirect = unsafe { child_inode.data.blocks.single_indirect };
            if single_indirect != 0 {
                self.fs.free_block(single_indirect).await.map_err(|_| InvocationError::UnsupportedOperation)?;
            }

            let double_indirect = unsafe { child_inode.data.blocks.double_indirect };
            if double_indirect != 0 {
                let mut sub_blocks = alloc::vec::Vec::new();
                let buffer = PhysBuffer::new().ok_or(InvocationError::OutOfMemory)?;
                if buffer.phys() != 0 {
                    if self.fs.read_block(double_indirect, buffer.phys() as u64).await.is_ok() {
                        let pointers_per_block = (self.fs.block_size / 4) as usize;
                        unsafe {
                            let table_ptr = buffer.virt() as *const u32;
                            for i in 0..pointers_per_block {
                                let sub_block = core::ptr::read(table_ptr.add(i));
                                if sub_block != 0 {
                                    sub_blocks.push(sub_block);
                                }
                            }
                        }
                    }
                }
                for sub_block in sub_blocks {
                    let _ = self.fs.free_block(sub_block).await;
                }
                self.fs.free_block(double_indirect).await.map_err(|_| InvocationError::UnsupportedOperation)?;
            }

            let triple_indirect = unsafe { child_inode.data.blocks.triple_indirect };
            if triple_indirect != 0 {
                let mut d_blocks = alloc::vec::Vec::new();
                let mut s_blocks = alloc::vec::Vec::new();

                let buffer = PhysBuffer::new().ok_or(InvocationError::OutOfMemory)?;
                if buffer.phys() != 0 {
                    if self.fs.read_block(triple_indirect, buffer.phys() as u64).await.is_ok() {
                        let pointers_per_block = (self.fs.block_size / 4) as usize;
                        unsafe {
                            let table_ptr = buffer.virt() as *const u32;
                            for i in 0..pointers_per_block {
                                let d_block = core::ptr::read(table_ptr.add(i));
                                if d_block != 0 {
                                    d_blocks.push(d_block);
                                }
                            }
                        }
                    }
                }

                for &d_block in &d_blocks {
                    let buffer_sub = PhysBuffer::new().ok_or(InvocationError::OutOfMemory)?;
                    if buffer_sub.phys() != 0 {
                        if self.fs.read_block(d_block, buffer_sub.phys() as u64).await.is_ok() {
                            let pointers_per_block = (self.fs.block_size / 4) as usize;
                            unsafe {
                                let sub_table_ptr = buffer_sub.virt() as *const u32;
                                for j in 0..pointers_per_block {
                                    let s_block = core::ptr::read(sub_table_ptr.add(j));
                                    if s_block != 0 {
                                        s_blocks.push(s_block);
                                    }
                                }
                            }
                        }
                    }
                }

                for s_block in s_blocks {
                    let _ = self.fs.free_block(s_block).await;
                }
                for d_block in d_blocks {
                    let _ = self.fs.free_block(d_block).await;
                }
                self.fs.free_block(triple_indirect).await.map_err(|_| InvocationError::UnsupportedOperation)?;
            }

            self.fs.free_inode(child_inode_num, is_dir).await.map_err(|_| InvocationError::UnsupportedOperation)?;
        } else {
            self.fs.write_inode(child_inode_num, &child_inode).await.map_err(|_| InvocationError::UnsupportedOperation)?;
        }

        Ok(())
    }

    async fn create_child_file(&self, name: &str, owner: UserID) -> Result<Arc<dyn KernelObject>, InvocationError> {
        validate_child_name(name)?;
        let creator_uid = u16::try_from(owner.0).map_err(|_| InvocationError::InvalidArgument)?;

        if self.fs.lookup_in_dir(&self.inode_data.read(), name).await.map_err(|_| InvocationError::UnsupportedOperation)?.is_some() {
            return Err(InvocationError::InvalidArgument);
        }

        let new_inode_num = self.fs.allocate_inode(false).await.map_err(|_| InvocationError::OutOfMemory)?;

        let current_time = get_realtime().0 as u32;

        // populate diskinode for regular file
        let child_inode = DiskInode {
            mode: 0x81A4, // regular file (0x8000) + permissions (0o644)
            uid: creator_uid,
            size: 0,
            atime: current_time,
            ctime: current_time,
            mtime: current_time,
            dtime: 0,
            gid: 0,
            links_count: 1,
            blocks: 0,
            flags: 0,
            osdl1: 0,
            data: crate::storage::fs::ext2::structs::FileData {
                blocks: crate::storage::fs::ext2::structs::DiskBlockPointers {
                    direct: [0; 12],
                    single_indirect: 0,
                    double_indirect: 0,
                    triple_indirect: 0,
                },
            },
            generation: 0,
            file_acl: 0,
            dir_acl: 0,
            faddr: 0,
            osd2: [0; 12],
        };

        self.fs.write_inode(new_inode_num, &child_inode).await.map_err(|_| InvocationError::UnsupportedOperation)?;

        // link child inode under name in the parent directory
        self.add_dir_entry(name, new_inode_num, 1u8).await.map_err(|_| InvocationError::UnsupportedOperation)?;

        let new_file = Arc::new_cyclic(|me| {
            let weak_node = me.clone() as Weak<dyn VfsNode>;
            Ext2File {
                fs: Arc::clone(&self.fs),
                inode_num: new_inode_num,
                inode_data: RwLock::new(child_inode),
                file_vmo: FileVmo::new(0, weak_node),
                write_lock: AsyncMutex::new(()),
            }
        });

        // cache file in active node cache
        self.fs.active_files.lock().insert(new_inode_num, Arc::downgrade(&new_file));

        let file_desc = Arc::new(FileDescription::new(new_file as Arc<dyn KernelObject>));

        Ok(file_desc)
    }

    async fn create_child_dir(&self, name: &str, owner: UserID) -> Result<Arc<dyn KernelObject>, InvocationError> {
        validate_child_name(name)?;
        let creator_uid = u16::try_from(owner.0).map_err(|_| InvocationError::InvalidArgument)?;

        if self.fs.lookup_in_dir(&self.inode_data.read(), name).await.map_err(|_| InvocationError::UnsupportedOperation)?.is_some() {
            return Err(InvocationError::InvalidArgument);
        }

        let new_inode_num = self.fs.allocate_inode(true).await.map_err(|_| InvocationError::OutOfMemory)?;

        let block_id = self.fs.allocate_block().await.map_err(|_| InvocationError::OutOfMemory)?;

        let buffer = PhysBuffer::new().ok_or(InvocationError::OutOfMemory)?;
        let block_size = self.fs.block_size as usize;

        // initialize "." and ".." headers in the directory's data block
        unsafe {
            ptr::write_bytes(buffer.virt() as *mut u8, 0, block_size);

            // entry 1: "." pointing to itself
            let entry1_ptr = buffer.virt() as *mut DiskDirHeader;
            ptr::write(
                entry1_ptr,
                DiskDirHeader {
                    inode: new_inode_num,
                    record_length: 12,
                    name_length: 1,
                    file_type: 2, // directory type
                },
            );
            let name1_ptr = (entry1_ptr as *mut u8).add(8);
            *name1_ptr = b'.';

            // entry 2: ".." pointing to the parent directory
            let entry2_ptr = (buffer.virt() as *mut u8).add(12) as *mut DiskDirHeader;
            ptr::write(
                entry2_ptr,
                DiskDirHeader {
                    inode: self.inode_num,
                    record_length: (block_size - 12) as u16,
                    name_length: 2,
                    file_type: 2, // directory type
                },
            );
            let name2_ptr = (entry2_ptr as *mut u8).add(8);
            *name2_ptr = b'.';
            *name2_ptr.add(1) = b'.';
        }

        let sector = block_id as u64 * self.fs.sectors_per_block as u64;
        let write_fut = self.fs.partition.write_sectors(sector, self.fs.sectors_per_block, buffer.phys() as u64);
        let write_result = match write_fut {
            Ok(fut) => fut.await,
            Err(_) => Err(()),
        };
        if write_result.is_err() {
            return Err(InvocationError::UnsupportedOperation);
        }

        let current_time = get_realtime().0 as u32;

        let child_inode = DiskInode {
            mode: 0x41ED, // directory (0x4000) + permissions (0o755)
            uid: creator_uid,
            size: self.fs.block_size,
            atime: current_time,
            ctime: current_time,
            mtime: current_time,
            dtime: 0,
            gid: 0,
            links_count: 2, // "." plus parent's link
            blocks: self.fs.sectors_per_block,
            flags: 0,
            osdl1: 0,
            data: crate::storage::fs::ext2::structs::FileData {
                blocks: crate::storage::fs::ext2::structs::DiskBlockPointers {
                    direct: {
                        let mut d = [0; 12];
                        d[0] = block_id;
                        d
                    },
                    single_indirect: 0,
                    double_indirect: 0,
                    triple_indirect: 0,
                },
            },
            generation: 0,
            file_acl: 0,
            dir_acl: 0,
            faddr: 0,
            osd2: [0; 12],
        };

        self.fs.write_inode(new_inode_num, &child_inode).await.map_err(|_| InvocationError::UnsupportedOperation)?;

        // link directory into the parent directory entry table
        self.add_dir_entry(name, new_inode_num, 2u8).await.map_err(|_| InvocationError::UnsupportedOperation)?;

        // increment parent directory link count (internal ".." points to parent)
        let mut parent_inode = self.inode_data.read().clone();
        parent_inode.links_count += 1;
        self.fs.write_inode(self.inode_num, &parent_inode).await.map_err(|_| InvocationError::UnsupportedOperation)?;
        *self.inode_data.write() = parent_inode;

        let new_dir = Arc::new(Ext2Directory { fs: Arc::clone(&self.fs), inode_num: new_inode_num, inode_data: RwLock::new(child_inode) });

        self.fs.active_dirs.lock().insert(new_inode_num, Arc::downgrade(&new_dir));

        Ok(new_dir)
    }
}

fn register_created_object(object: Arc<dyn KernelObject>) -> Result<usize, InvocationError> {
    let rights = allowed_rights(&object)?;
    let proc = current_process().ok_or(InvocationError::InvalidHandle)?;
    Ok(proc.handles.write().insert(object, rights).0)
}

impl Ext2Directory {
    pub async fn add_dir_entry(&self, name: &str, child_inode_num: u32, file_type: u8) -> Result<(), ()> {
        let name_bytes = name.as_bytes();
        if name_bytes.len() > 254 || name_bytes.is_empty() {
            return Err(());
        }

        let needed_len = (8 + name_bytes.len() + 3) & !3;
        let block_size = self.fs.block_size as usize;

        let buffer = PhysBuffer::new().ok_or(())?;

        let total_blocks = self.inode_data.read().size as usize / block_size;
        for block_idx in 0..total_blocks {
            let block_id = {
                let inode_ref = self.inode_data.read();
                self.fs.resolve_file_block(&*inode_ref, block_idx).await.unwrap_or(0)
            };
            if block_id == 0 {
                continue;
            }

            self.fs.read_block(block_id, buffer.phys() as u64).await?;

            let mut offset = 0;
            while offset < block_size {
                unsafe {
                    let entry_ptr = (buffer.virt() as *mut u8).add(offset) as *mut DiskDirHeader;
                    let rec_len = (*entry_ptr).record_length as usize;
                    let name_len = (*entry_ptr).name_length as usize;

                    if rec_len == 0 {
                        break;
                    }

                    if offset + rec_len == block_size {
                        let last_used_len = (8 + name_len + 3) & !3;
                        let padding = rec_len - last_used_len;

                        if padding >= needed_len {
                            (*entry_ptr).record_length = last_used_len as u16;

                            let new_entry_ptr = (buffer.virt() as *mut u8).add(offset + last_used_len) as *mut DiskDirHeader;
                            ptr::write(
                                new_entry_ptr,
                                DiskDirHeader {
                                    inode: child_inode_num,
                                    record_length: padding as u16,
                                    name_length: name_bytes.len() as u8,
                                    file_type,
                                },
                            );

                            let new_name_ptr = (new_entry_ptr as *mut u8).add(8);
                            copy_nonoverlapping(name_bytes.as_ptr(), new_name_ptr, name_bytes.len());

                            self.fs.cache.write_block(block_id as usize, buffer.phys() as u64).await?;
                            return Ok(());
                        }
                    }
                    offset += rec_len;
                }
            }
        }

        let new_block_id = match self.fs.allocate_block().await {
            Ok(id) => id,
            Err(_) => return Err(()),
        };

        unsafe {
            ptr::write_bytes(buffer.virt() as *mut u8, 0, block_size);
            let new_entry_ptr = buffer.virt() as *mut DiskDirHeader;
            ptr::write(
                new_entry_ptr,
                DiskDirHeader { inode: child_inode_num, record_length: block_size as u16, name_length: name_bytes.len() as u8, file_type },
            );

            let new_name_ptr = (new_entry_ptr as *mut u8).add(8);
            copy_nonoverlapping(name_bytes.as_ptr(), new_name_ptr, name_bytes.len());
        }

        let sector = new_block_id as u64 * self.fs.sectors_per_block as u64;
        let write_result = {
            let write_fut = self.fs.partition.write_sectors(sector, self.fs.sectors_per_block, buffer.phys() as u64);
            match write_fut {
                Ok(fut) => fut.await,
                Err(_) => Err(()),
            }
        };
        if write_result.is_err() { return Err(()); }

        let new_logical_idx = total_blocks;
        let mut inode_write = self.inode_data.write();

        let map_result = {
            if new_logical_idx < 12 {
                unsafe {
                    inode_write.data.blocks.direct[new_logical_idx] = new_block_id;
                }
                Ok(())
            } else {
                let pointers_per_block = (self.fs.block_size / 4) as usize;
                let blocks_per_double = pointers_per_block * pointers_per_block;
                let blocks_per_triple = blocks_per_double * pointers_per_block;
                let remaining = new_logical_idx - 12;

                if remaining < pointers_per_block {
                    // single indirection
                    let mut single_indirect = unsafe { inode_write.data.blocks.single_indirect };
                    if single_indirect == 0 {
                        single_indirect = match self.fs.allocate_block().await {
                            Ok(id) => id,
                            Err(_) => return Err(()),
                        };
                        inode_write.data.blocks.single_indirect = single_indirect;

                        unsafe {
                            ptr::write_bytes(buffer.virt() as *mut u8, 0, block_size);
                        }
                        let s = single_indirect as u64 * self.fs.sectors_per_block as u64;
                        self.fs.partition.write_sectors(s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    }

                    let s = single_indirect as u64 * self.fs.sectors_per_block as u64;
                    self.fs.partition.read_sectors(s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    unsafe {
                        let table_ptr = (buffer.virt() as *mut u8) as *mut u32;
                        ptr::write(table_ptr.add(remaining), new_block_id);
                    }
                    self.fs.partition.write_sectors(s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    Ok(())
                } else if remaining < pointers_per_block + blocks_per_double {
                    // double indirection
                    let doubly_idx = remaining - pointers_per_block;
                    let mut double_indirect = unsafe { inode_write.data.blocks.double_indirect };
                    if double_indirect == 0 {
                        double_indirect = match self.fs.allocate_block().await {
                            Ok(id) => id,
                            Err(_) => return Err(()),
                        };
                        inode_write.data.blocks.double_indirect = double_indirect;

                        unsafe {
                            ptr::write_bytes(buffer.virt() as *mut u8, 0, block_size);
                        }
                        let s = double_indirect as u64 * self.fs.sectors_per_block as u64;
                        self.fs.partition.write_sectors(s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    }

                    let s = double_indirect as u64 * self.fs.sectors_per_block as u64;
                    self.fs.partition.read_sectors(s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    let level1_idx = doubly_idx / pointers_per_block;
                    let level2_idx = doubly_idx % pointers_per_block;

                    let mut single_indirect = unsafe {
                        let table_ptr = (buffer.virt() as *mut u8) as *mut u32;
                        ptr::read(table_ptr.add(level1_idx))
                    };

                    if single_indirect == 0 {
                        let new_sub_block = match self.fs.allocate_block().await {
                            Ok(id) => id,
                            Err(_) => return Err(()),
                        };
                        single_indirect = new_sub_block;

                        unsafe {
                            let table_ptr = (buffer.virt() as *mut u8) as *mut u32;
                            ptr::write(table_ptr.add(level1_idx), single_indirect);
                        }
                        self.fs.partition.write_sectors(s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;

                        let buffer_sub = PhysBuffer::new().ok_or(())?;
                        unsafe {
                            ptr::write_bytes(buffer_sub.virt() as *mut u8, 0, block_size);
                        }
                        let sub_s = single_indirect as u64 * self.fs.sectors_per_block as u64;
                        self.fs.partition.write_sectors(sub_s, self.fs.sectors_per_block, buffer_sub.phys() as u64)?.await?;
                    }

                    let sub_s = single_indirect as u64 * self.fs.sectors_per_block as u64;
                    self.fs.partition.read_sectors(sub_s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    unsafe {
                        let table2_ptr = (buffer.virt() as *mut u8) as *mut u32;
                        ptr::write(table2_ptr.add(level2_idx), new_block_id);
                    }
                    self.fs.partition.write_sectors(sub_s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    Ok(())
                } else if remaining < pointers_per_block + blocks_per_double + blocks_per_triple {
                    // triple indirection
                    let triply_idx = remaining - pointers_per_block - blocks_per_double;
                    let mut triple_indirect = unsafe { inode_write.data.blocks.triple_indirect };
                    if triple_indirect == 0 {
                        triple_indirect = match self.fs.allocate_block().await {
                            Ok(id) => id,
                            Err(_) => return Err(()),
                        };
                        inode_write.data.blocks.triple_indirect = triple_indirect;

                        unsafe {
                            ptr::write_bytes(buffer.virt() as *mut u8, 0, block_size);
                        }
                        let s = triple_indirect as u64 * self.fs.sectors_per_block as u64;
                        self.fs.partition.write_sectors(s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    }

                    let s = triple_indirect as u64 * self.fs.sectors_per_block as u64;
                    self.fs.partition.read_sectors(s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    let level1_idx = triply_idx / blocks_per_double;
                    let level2_idx = (triply_idx % blocks_per_double) / pointers_per_block;
                    let level3_idx = (triply_idx % blocks_per_double) % pointers_per_block;

                    let mut double_indirect = unsafe {
                        let table_ptr = (buffer.virt() as *mut u8) as *mut u32;
                        ptr::read(table_ptr.add(level1_idx))
                    };

                    if double_indirect == 0 {
                        let new_sub_block = match self.fs.allocate_block().await {
                            Ok(id) => id,
                            Err(_) => return Err(()),
                        };
                        double_indirect = new_sub_block;

                        unsafe {
                            let table_ptr = (buffer.virt() as *mut u8) as *mut u32;
                            ptr::write(table_ptr.add(level1_idx), double_indirect);
                        }
                        self.fs.partition.write_sectors(s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;

                        let buffer_sub = PhysBuffer::new().ok_or(())?;
                        unsafe {
                            ptr::write_bytes(buffer_sub.virt() as *mut u8, 0, block_size);
                        }
                        let sub_s = double_indirect as u64 * self.fs.sectors_per_block as u64;
                        self.fs.partition.write_sectors(sub_s, self.fs.sectors_per_block, buffer_sub.phys() as u64)?.await?;
                    }

                    let sub_s = double_indirect as u64 * self.fs.sectors_per_block as u64;
                    self.fs.partition.read_sectors(sub_s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    let mut single_indirect = unsafe {
                        let table2_ptr = (buffer.virt() as *mut u8) as *mut u32;
                        ptr::read(table2_ptr.add(level2_idx))
                    };

                    if single_indirect == 0 {
                        let new_sub_block = match self.fs.allocate_block().await {
                            Ok(id) => id,
                            Err(_) => return Err(()),
                        };
                        single_indirect = new_sub_block;

                        unsafe {
                            let table2_ptr = (buffer.virt() as *mut u8) as *mut u32;
                            ptr::write(table2_ptr.add(level2_idx), single_indirect);
                        }
                        self.fs.partition.write_sectors(sub_s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;

                        let buffer_sub = PhysBuffer::new().ok_or(())?;
                        unsafe {
                            ptr::write_bytes(buffer_sub.virt() as *mut u8, 0, block_size);
                        }
                        let sub2_s = single_indirect as u64 * self.fs.sectors_per_block as u64;
                        self.fs.partition.write_sectors(sub2_s, self.fs.sectors_per_block, buffer_sub.phys() as u64)?.await?;
                    }

                    let sub2_s = single_indirect as u64 * self.fs.sectors_per_block as u64;
                    self.fs.partition.read_sectors(sub2_s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    unsafe {
                        let table3_ptr = (buffer.virt() as *mut u8) as *mut u32;
                        ptr::write(table3_ptr.add(level3_idx), new_block_id);
                    }
                    self.fs.partition.write_sectors(sub2_s, self.fs.sectors_per_block, buffer.phys() as u64)?.await?;
                    Ok(())
                } else {
                    Err(())
                }
            }
        };

        if map_result.is_err() { return Err(()); }

        inode_write.size += block_size as u32;
        inode_write.blocks += self.fs.sectors_per_block;

        let num = self.inode_num;
        let save_result = self.fs.write_inode(num, &*inode_write).await;

        save_result
    }

    pub async fn remove_dir_entry(&self, name: &str) -> Result<(), ()> {
        let name_bytes = name.as_bytes();
        if name_bytes.is_empty() || name_bytes.len() > 254 {
            return Err(());
        }

        let block_size = self.fs.block_size as usize;
        let buffer = PhysBuffer::new().ok_or(())?;

        let total_blocks = self.inode_data.read().size as usize / block_size;
        for block_idx in 0..total_blocks {
            let block_id = {
                let inode_ref = self.inode_data.read();
                self.fs.resolve_file_block(&*inode_ref, block_idx).await.unwrap_or(0)
            };
            if block_id == 0 {
                continue;
            }

            if self.fs.read_block(block_id, buffer.phys() as u64).await.is_err() {
                return Err(());
            }

            let mut offset = 0;
            let mut prev_entry_ptr: Option<*mut DiskDirHeader> = None;

            while offset < block_size {
                unsafe {
                    let entry_ptr = (buffer.virt() as *mut u8).add(offset) as *mut DiskDirHeader;
                    let rec_len = (*entry_ptr).record_length as usize;
                    let name_len = (*entry_ptr).name_length as usize;
                    let inode_id = (*entry_ptr).inode;

                    if rec_len == 0 {
                        break;
                    }

                    if inode_id != 0 && name_len == name_bytes.len() {
                        let name_ptr = (entry_ptr as *const u8).add(8);
                        let name_slice = core::slice::from_raw_parts(name_ptr, name_len);
                        if name_slice == name_bytes {
                            if let Some(prev) = prev_entry_ptr {
                                (*prev).record_length += rec_len as u16;
                            } else {
                                (*entry_ptr).inode = 0;
                            }

                            if self.fs.cache.write_block(block_id as usize, buffer.phys() as u64).await.is_err() {
                                return Err(());
                            }

                            return Ok(());
                        }
                    }

                    prev_entry_ptr = Some(entry_ptr);
                    offset += rec_len;
                }
            }
        }

        Err(())
    }
}
