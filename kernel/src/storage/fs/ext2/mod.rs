use alloc::collections::BTreeMap;
use alloc::slice;
use alloc::sync::{
    Arc,
    Weak,
};
use alloc::vec::Vec;
use core::{
    ptr,
    str,
};

use crate::executor::async_mutex::AsyncMutex;
use crate::sync::{
    Mutex,
    TicketLock,
};
use crate::memory::{
    ALLOCATOR,
    PageSize,
    DIRECT_MAP_OFFSET,
    calculate_order,
};
use crate::storage::blockdev::{
    AsyncBlockDevice,
    BlockCache,
};
use crate::storage::fs::ext2::obj::{
    Ext2Directory,
    Ext2File,
};
use crate::storage::fs::ext2::structs::{
    DiskDirHeader,
    DiskGroupDesc,
    DiskInode,
    DiskSuperblock,
};

pub mod init;
pub mod obj;
pub mod permissions;
pub mod structs;

#[derive(Debug)]
pub struct Ext2FileSystem {
    pub partition: Arc<dyn AsyncBlockDevice>,
    pub cache: BlockCache,

    pub block_size: u32,
    pub sectors_per_block: u32,
    pub inodes_per_group: u32,
    pub blocks_per_group: u32,
    pub inode_size: u32,

    pub bgdt: TicketLock<Vec<DiskGroupDesc>>,
    pub allocation_lock: AsyncMutex<()>,
    pub active_files: Mutex<BTreeMap<u32, Weak<Ext2File>>>,
    pub active_dirs: Mutex<BTreeMap<u32, Weak<Ext2Directory>>>,
    pub dirty_files: Mutex<BTreeMap<u32, Arc<Ext2File>>>,
}

impl Ext2FileSystem {
    pub async fn mount(partition: Arc<dyn AsyncBlockDevice>) -> Result<Self, ()> {
        let page_phys = ALLOCATOR.alloc(PageSize::Size4K);
        if page_phys == 0 {
            return Err(());
        }
        let page_virt = page_phys + *DIRECT_MAP_OFFSET;

        // read the ext2 superblock, which sits at 1024 bytes from part start
        let sb_future = partition.read_sectors(2, 2, page_phys as u64)?;
        sb_future.await?;

        let sb = unsafe { &*(page_virt as *const DiskSuperblock) };

        if sb.magic != 0xEF53 {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }

        let block_size = 1024 << sb.log_block_size;
        let cache = BlockCache::new(partition.clone(), block_size as usize, 512);
        let sectors_per_block = block_size / 512;

        let inode_size = if sb.rev_level >= 1 { sb.inode_size } else { 128 } as u32;

        let total_blocks = sb.blocks_count;
        let num_groups = (total_blocks + sb.blocks_per_group - 1) / sb.blocks_per_group;
        let bgdt_bytes = num_groups as usize * size_of::<DiskGroupDesc>();
        let bgdt_blocks = (bgdt_bytes as u32 * block_size - 1) / block_size;
        let bgdt_sectors = bgdt_blocks * sectors_per_block;

        let bgdt_start_block = if block_size == 1024 { 2 } else { 1 };
        let bgdt_start_sector = bgdt_start_block as u64 * sectors_per_block as u64;

        let bgdt_alloc_bytes = bgdt_sectors as usize * 512 + 4095;
        let bgdt_alloc_order = calculate_order(bgdt_alloc_bytes);
        let bgdt_buf_phys = match ALLOCATOR.alloc_order(bgdt_alloc_order) {
            Some(a) => a,
            None => {
                ALLOCATOR.free(page_phys, PageSize::Size4K);
                return Err(());
            }
        };
        let bgdt_buf_virt = bgdt_buf_phys + *DIRECT_MAP_OFFSET;

        let bgdt_future = partition.read_sectors(bgdt_start_sector, bgdt_sectors, bgdt_buf_phys as u64)?;
        bgdt_future.await?;

        let mut bgdt_vec = Vec::with_capacity(num_groups as usize);
        unsafe {
            let src_ptr = bgdt_buf_virt as *const DiskGroupDesc;
            for i in 0..num_groups as usize {
                bgdt_vec.push(ptr::read(src_ptr.add(i)));
            }
        }

        ALLOCATOR.free(page_phys, PageSize::Size4K);
        ALLOCATOR.free_order(bgdt_buf_phys, bgdt_alloc_order);

        Ok(Ext2FileSystem {
            partition,
            cache,
            block_size,
            sectors_per_block,
            inodes_per_group: sb.inodes_per_group,
            blocks_per_group: sb.blocks_per_group,
            inode_size,
            bgdt: TicketLock::new(bgdt_vec),
            allocation_lock: AsyncMutex::new(()),
            active_files: Mutex::new(BTreeMap::new()),
            active_dirs: Mutex::new(BTreeMap::new()),
            dirty_files: Mutex::new(BTreeMap::new()),
        })
    }

    pub async fn read_block(&self, block_id: u32, dest_phys: u64) -> Result<(), ()> {
        if block_id == 0 {
            unsafe {
                let dest_virt = dest_phys + *DIRECT_MAP_OFFSET as u64;
                ptr::write_bytes(dest_virt as *mut u8, 0, self.block_size as usize);
            }
            return Ok(());
        }

        self.cache.read_block(block_id as usize, dest_phys).await
    }

    pub async fn read_inode(&self, inode_num: u32) -> Result<DiskInode, ()> {
        if inode_num == 0 {
            return Err(());
        };
        let bg_index = (inode_num - 1) / self.inodes_per_group;
        let local_inode_idx = (inode_num - 1) % self.inodes_per_group;

        // unpack beginning block addr of target inode table from the bgdt
        let inode_table_start_block = {
            let bgdt = self.bgdt.lock();
            if bg_index as usize >= bgdt.len() {
                return Err(());
            };
            bgdt[bg_index as usize].inode_table
        };

        let byte_offset = local_inode_idx * self.inode_size;
        let target_logical_block = inode_table_start_block + (byte_offset / self.block_size);
        let block_internal_offset = byte_offset % self.block_size;

        let page_phys = ALLOCATOR.alloc(PageSize::Size4K);
        if page_phys == 0 {
            return Err(());
        };
        let page_virt = page_phys + *DIRECT_MAP_OFFSET;

        if self.read_block(target_logical_block, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }

        let inode = unsafe {
            let src_ptr = (page_virt as *const u8).add(block_internal_offset as usize) as *const DiskInode;
            ptr::read(src_ptr)
        };

        ALLOCATOR.free(page_phys, PageSize::Size4K);
        Ok(inode)
    }

    pub async fn lookup_in_dir(&self, inode: &DiskInode, name: &str) -> Result<Option<u32>, ()> {
        let page_phys = ALLOCATOR.alloc(PageSize::Size4K);
        if page_phys == 0 {
            return Err(());
        };
        let page_virt = page_phys + *DIRECT_MAP_OFFSET;

        // walk thru 12 direct blk pointers
        for direct_idx in 0..12 {
            let block_id = unsafe { inode.data.blocks.direct[direct_idx] };
            if block_id == 0 {
                continue;
            };

            if self.read_block(block_id, page_phys as u64).await.is_err() {
                ALLOCATOR.free(page_phys, PageSize::Size4K);
                return Err(());
            }

            let mut offset = 0;
            while offset < self.block_size as usize {
                unsafe {
                    let entry_ptr = (page_virt as *const u8).add(offset) as *const DiskDirHeader;
                    let inode_id = (*entry_ptr).inode;
                    let rec_len = (*entry_ptr).record_length as usize;
                    let name_len = (*entry_ptr).name_length as usize;

                    if rec_len == 0 {
                        break;
                    }

                    if inode_id != 0 && name_len > 0 && offset + 8 + name_len <= self.block_size as usize {
                        let name_ptr = (entry_ptr as *const u8).add(8);
                        let name_slice = slice::from_raw_parts(name_ptr, name_len);

                        if let Ok(entry_name) = str::from_utf8(name_slice) {
                            if entry_name == name {
                                ALLOCATOR.free(page_phys, PageSize::Size4K);
                                return Ok(Some(inode_id));
                            }
                        }
                    }
                    offset += rec_len;
                }
            }
        }
        ALLOCATOR.free(page_phys, PageSize::Size4K);
        Ok(None)
    }

    pub async fn resolve_file_block(&self, inode: &DiskInode, file_block_idx: usize) -> Result<u32, ()> {
        let pointers_per_block = (self.block_size / 4) as usize;

        // tier 1: direct blocks (indices 0 to 11)
        if file_block_idx < 12 {
            unsafe {
                return Ok(inode.data.blocks.direct[file_block_idx]);
            }
        }

        let mut remaining_idx = file_block_idx - 12;

        // tier 2: singly indirect blocks
        if remaining_idx < pointers_per_block {
            let single_indirect_id = unsafe { inode.data.blocks.single_indirect };
            if single_indirect_id == 0 {
                return Ok(0);
            } // data block hole configuration

            let page_phys = ALLOCATOR.alloc(PageSize::Size4K);
            if page_phys == 0 {
                return Err(());
            }
            self.read_block(single_indirect_id, page_phys as u64).await?;

            let physical_block_id = unsafe {
                let table_ptr = (page_phys + *DIRECT_MAP_OFFSET) as *const u32;
                ptr::read(table_ptr.add(remaining_idx))
            };

            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Ok(physical_block_id);
        }

        remaining_idx -= pointers_per_block;

        // tier 3: doubly indirect blocks
        let blocks_per_double = pointers_per_block * pointers_per_block;
        if remaining_idx < blocks_per_double {
            let double_indirect_id = unsafe { inode.data.blocks.double_indirect }; //
            if double_indirect_id == 0 {
                return Ok(0);
            }

            let page_phys = ALLOCATOR.alloc(PageSize::Size4K);
            if page_phys == 0 {
                return Err(());
            }
            let page_virt = page_phys + *DIRECT_MAP_OFFSET; //

            let level1_idx = remaining_idx / pointers_per_block;
            let level2_idx = remaining_idx % pointers_per_block;

            // load level 1 pointer map block
            self.read_block(double_indirect_id, page_phys as u64).await?;
            let single_indirect_id = unsafe { ptr::read((page_virt as *const u32).add(level1_idx)) };

            if single_indirect_id == 0 {
                ALLOCATOR.free(page_phys, PageSize::Size4K);
                return Ok(0);
            }

            // load level 2 ultimate target block location pointer
            self.read_block(single_indirect_id, page_phys as u64).await?;
            let physical_block_id = unsafe { ptr::read((page_virt as *const u32).add(level2_idx)) };

            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Ok(physical_block_id);
        }

        remaining_idx -= blocks_per_double;

        // tier 4: triply indirect blocks (will it end plz)
        let blocks_per_triple = blocks_per_double * pointers_per_block;
        if remaining_idx < blocks_per_triple {
            let triple_indirect_id = unsafe { inode.data.blocks.triple_indirect };
            if triple_indirect_id == 0 {
                return Ok(0);
            }

            let page_phys = ALLOCATOR.alloc(PageSize::Size4K);
            if page_phys == 0 {
                return Err(());
            }
            let page_virt = page_phys + *DIRECT_MAP_OFFSET;

            let level1_idx = remaining_idx / blocks_per_double;
            let level2_idx = (remaining_idx % blocks_per_double) / pointers_per_block;
            let level3_idx = (remaining_idx % blocks_per_double) % pointers_per_block;

            // read the triple indirect block
            self.read_block(triple_indirect_id, page_phys as u64).await?;
            let double_indirect_id = unsafe { core::ptr::read((page_virt as *const u32).add(level1_idx)) };
            if double_indirect_id == 0 {
                ALLOCATOR.free(page_phys, PageSize::Size4K);
                return Ok(0);
            }

            // read the doubly indirect block
            self.read_block(double_indirect_id, page_phys as u64).await?;
            let single_indirect_id = unsafe { core::ptr::read((page_virt as *const u32).add(level2_idx)) };
            if single_indirect_id == 0 {
                ALLOCATOR.free(page_phys, PageSize::Size4K);
                return Ok(0);
            }

            // read the singly indirect block
            self.read_block(single_indirect_id, page_phys as u64).await?;
            let physical_block_id = unsafe { core::ptr::read((page_virt as *const u32).add(level3_idx)) };

            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Ok(physical_block_id);
        }
        Err(())
    }

    fn add_inode_block(&self, inode: &mut DiskInode) {
        let blocks = inode.blocks;
        inode.blocks = blocks.saturating_add(self.sectors_per_block);
    }

    fn remove_inode_block(&self, inode: &mut DiskInode) {
        let blocks = inode.blocks;
        inode.blocks = blocks.saturating_sub(self.sectors_per_block);
    }

    async fn allocate_zeroed_block(&self) -> Result<u32, ()> {
        let block = self.allocate_block().await?;
        let page = ALLOCATOR.alloc(PageSize::Size4K);
        if page == 0 {
            self.free_block(block).await?;
            return Err(());
        }
        unsafe {
            ptr::write_bytes((page + *DIRECT_MAP_OFFSET) as *mut u8, 0, self.block_size as usize);
        }
        let result = self.cache.write_block(block as usize, page as u64).await;
        ALLOCATOR.free(page, PageSize::Size4K);
        if result.is_err() {
            self.free_block(block).await?;
            return Err(());
        }
        Ok(block)
    }

    async fn read_pointer(&self, table_block: u32, index: usize) -> Result<u32, ()> {
        let pointers = (self.block_size / 4) as usize;
        if table_block == 0 || index >= pointers {
            return Err(());
        }
        let page = ALLOCATOR.alloc(PageSize::Size4K);
        if page == 0 {
            return Err(());
        }
        if self.read_block(table_block, page as u64).await.is_err() {
            ALLOCATOR.free(page, PageSize::Size4K);
            return Err(());
        }
        let value = unsafe { ptr::read(((page + *DIRECT_MAP_OFFSET) as *const u32).add(index)) };
        ALLOCATOR.free(page, PageSize::Size4K);
        Ok(value)
    }

    async fn write_pointer(&self, table_block: u32, index: usize, value: u32) -> Result<bool, ()> {
        let pointers = (self.block_size / 4) as usize;

        if table_block == 0 || index >= pointers {
            return Err(());
        }

        let page = ALLOCATOR.alloc(PageSize::Size4K);
        if page == 0 {
            return Err(());
        }

        if self.read_block(table_block, page as u64).await.is_err() {
            ALLOCATOR.free(page, PageSize::Size4K);
            return Err(());
        }

        // Keep raw pointers scoped before the await.
        let empty = {
            let table = (page + *DIRECT_MAP_OFFSET) as *mut u32;

            unsafe {
                ptr::write(table.add(index), value);
                (0..pointers).all(|i| ptr::read(table.add(i)) == 0)
            }
        };

        let result = self.cache.write_block(table_block as usize, page as u64).await;

        ALLOCATOR.free(page, PageSize::Size4K);
        result.map(|_| empty)
    }

    fn indirect_path(&self, file_block_idx: usize) -> Result<(usize, [usize; 3]), ()> {
        let pointers = (self.block_size / 4) as usize;
        let double = pointers.checked_mul(pointers).ok_or(())?;
        let triple = double.checked_mul(pointers).ok_or(())?;
        if file_block_idx < 12 {
            return Ok((0, [file_block_idx, 0, 0]));
        }
        let mut remaining = file_block_idx - 12;
        if remaining < pointers {
            return Ok((1, [remaining, 0, 0]));
        }
        remaining -= pointers;
        if remaining < double {
            return Ok((2, [remaining / pointers, remaining % pointers, 0]));
        }
        remaining -= double;
        if remaining < triple {
            return Ok((3, [remaining / double, (remaining % double) / pointers, remaining % pointers]));
        }
        Err(())
    }

    fn get_indirect_root(inode: &DiskInode, depth: usize) -> u32 {
        unsafe {
            match depth {
                1 => inode.data.blocks.single_indirect,
                2 => inode.data.blocks.double_indirect,
                3 => inode.data.blocks.triple_indirect,
                _ => 0,
            }
        }
    }

    fn set_indirect_root(inode: &mut DiskInode, depth: usize, value: u32) {
        match depth {
            1 => inode.data.blocks.single_indirect = value,
            2 => inode.data.blocks.double_indirect = value,
            3 => inode.data.blocks.triple_indirect = value,
            _ => {}
        }
    }

    pub async fn allocate_file_block(&self, inode: &mut DiskInode, file_block_idx: usize) -> Result<u32, ()> {
        let (depth, indices) = self.indirect_path(file_block_idx)?;
        if depth == 0 {
            let existing = unsafe { inode.data.blocks.direct[file_block_idx] };
            if existing != 0 {
                return Ok(existing);
            }
            let data = self.allocate_block().await?;
            unsafe {
                inode.data.blocks.direct[file_block_idx] = data;
            }
            self.add_inode_block(inode);
            return Ok(data);
        }
        let mut root = Self::get_indirect_root(inode, depth);
        if root == 0 {
            root = self.allocate_zeroed_block().await?;
            Self::set_indirect_root(inode, depth, root);
            self.add_inode_block(inode);
        }
        let mut table = root;
        for level in 0..depth - 1 {
            let mut child = self.read_pointer(table, indices[level]).await?;
            if child == 0 {
                child = self.allocate_zeroed_block().await?;
                if self.write_pointer(table, indices[level], child).await.is_err() {
                    self.free_block(child).await?;
                    return Err(());
                }
                self.add_inode_block(inode);
            }
            table = child;
        }
        let leaf = indices[depth - 1];
        let existing = self.read_pointer(table, leaf).await?;
        if existing != 0 {
            return Ok(existing);
        }
        let data = self.allocate_block().await?;
        if self.write_pointer(table, leaf, data).await.is_err() {
            self.free_block(data).await?;
            return Err(());
        }
        self.add_inode_block(inode);
        Ok(data)
    }

    pub async fn clear_file_block(&self, inode: &mut DiskInode, file_block_idx: usize) -> Result<(), ()> {
        let (depth, indices) = self.indirect_path(file_block_idx)?;
        if depth == 0 {
            let data = unsafe { inode.data.blocks.direct[file_block_idx] };
            if data != 0 {
                unsafe {
                    inode.data.blocks.direct[file_block_idx] = 0;
                }
                self.free_block(data).await?;
                self.remove_inode_block(inode);
            }
            return Ok(());
        }
        let root = Self::get_indirect_root(inode, depth);
        if root == 0 {
            return Ok(());
        }
        let mut tables = [0u32; 3];
        tables[0] = root;
        for level in 0..depth - 1 {
            let child = self.read_pointer(tables[level], indices[level]).await?;
            if child == 0 {
                return Ok(());
            }
            tables[level + 1] = child;
        }
        let leaf_level = depth - 1;
        let data = self.read_pointer(tables[leaf_level], indices[leaf_level]).await?;
        if data == 0 {
            return Ok(());
        }
        let mut empty = self.write_pointer(tables[leaf_level], indices[leaf_level], 0).await?;
        self.free_block(data).await?;
        self.remove_inode_block(inode);
        let mut level = leaf_level;
        while empty {
            self.free_block(tables[level]).await?;
            self.remove_inode_block(inode);
            if level == 0 {
                Self::set_indirect_root(inode, depth, 0);
                break;
            }
            level -= 1;
            empty = self.write_pointer(tables[level], indices[level], 0).await?;
        }
        Ok(())
    }

    pub async fn write_inode(&self, inode_num: u32, inode: &DiskInode) -> Result<(), ()> {
        if inode_num == 0 {
            return Err(());
        };
        let bg_index = ((inode_num - 1) / self.inodes_per_group) as usize;
        let local_inode_idx = (inode_num - 1) % self.inodes_per_group;

        let inode_table_start_block = {
            let bgdt = self.bgdt.lock();
            if bg_index >= bgdt.len() {
                return Err(());
            };
            bgdt[bg_index].inode_table
        };

        let byte_offset = local_inode_idx * self.inode_size;
        let target_logical_block = inode_table_start_block + (byte_offset / self.block_size);
        let block_internal_offset = byte_offset % self.block_size;

        let page_phys = ALLOCATOR.alloc(PageSize::Size4K);
        if page_phys == 0 {
            return Err(());
        };
        let page_virt = page_phys + *DIRECT_MAP_OFFSET;

        self.read_block(target_logical_block, page_phys as u64).await?;

        unsafe {
            let dest_ptr = (page_virt as *mut u8).add(block_internal_offset as usize) as *mut DiskInode;
            ptr::write(dest_ptr, *inode);
        }

        self.cache.write_block(target_logical_block as usize, page_phys as u64).await?;
        ALLOCATOR.free(page_phys, PageSize::Size4K);
        Ok(())
    }

    pub async fn allocate_block(&self) -> Result<u32, ()> {
        let _guard = self.allocation_lock.lock().await;

        let page_phys = ALLOCATOR.alloc(PageSize::Size4K);
        if page_phys == 0 {
            return Err(());
        };
        let page_virt = page_phys + *DIRECT_MAP_OFFSET;

        let num_groups = self.bgdt.lock().len();

        for group in 0..num_groups {
            let mut bg = self.bgdt.lock()[group];
            if bg.free_blocks_count > 0 {
                self.read_block(bg.block_bitmap, page_phys as u64).await?;
                let bitmap_ptr = page_virt as *mut u8;
                let mut allocated_block_idx = None;

                // find first free block bit
                for byte_idx in 0..self.block_size as usize {
                    unsafe {
                        let val = *bitmap_ptr.add(byte_idx);
                        if val != 0xFF {
                            for bit in 0..8 {
                                if (val & (1 << bit)) == 0 {
                                    *bitmap_ptr.add(byte_idx) = val | (1 << bit);
                                    allocated_block_idx = Some(byte_idx * 8 + bit);
                                    break;
                                }
                            }
                        }
                    }
                    if allocated_block_idx.is_some() {
                        break;
                    }
                }

                if let Some(bit_idx) = allocated_block_idx {
                    // write bitmap block back
                    self.cache.write_block(bg.block_bitmap as usize, page_phys as u64).await?;

                    let first_data_block = if self.block_size == 1024 { 1 } else { 0 };
                    let block_id = group as u32 * self.blocks_per_group + bit_idx as u32 + first_data_block;

                    bg.free_blocks_count -= 1;
                    self.bgdt.lock()[group] = bg;

                    // write modified gd back to cache
                    let bgdt_start_block = if self.block_size == 1024 { 2 } else { 1 };
                    let descriptor_offset = group * size_of::<DiskGroupDesc>();
                    let target_logical_block = bgdt_start_block + (descriptor_offset as u32 / self.block_size);
                    let block_internal_offset = descriptor_offset % self.block_size as usize;

                    self.read_block(target_logical_block, page_phys as u64).await?;
                    unsafe {
                        let dest_ptr = (page_virt as *mut u8).add(block_internal_offset) as *mut DiskGroupDesc;
                        ptr::write(dest_ptr, bg);
                    }
                    self.cache.write_block(target_logical_block as usize, page_phys as u64).await?;

                    // update sb count
                    let sb_block = if self.block_size == 1024 { 1 } else { 0 };
                    let sb_internal_offset = if self.block_size == 1024 { 0 } else { 1024 };

                    self.read_block(sb_block, page_phys as u64).await?;
                    unsafe {
                        let sb_ptr = (page_virt as *mut u8).add(sb_internal_offset) as *mut DiskSuperblock;
                        if (*sb_ptr).free_blocks_count > 0 {
                            (*sb_ptr).free_blocks_count -= 1;
                        }
                    }
                    self.cache.write_block(sb_block as usize, page_phys as u64).await?;

                    ALLOCATOR.free(page_phys, PageSize::Size4K);
                    return Ok(block_id);
                }
            }
        }
        ALLOCATOR.free(page_phys, PageSize::Size4K);
        Err(())
    }

    pub async fn allocate_inode(&self, is_dir: bool) -> Result<u32, ()> {
        let _guard = self.allocation_lock.lock().await;

        let page_phys = ALLOCATOR.alloc(PageSize::Size4K);
        if page_phys == 0 {
            return Err(());
        };
        let page_virt = page_phys + *DIRECT_MAP_OFFSET;

        let num_groups = self.bgdt.lock().len();

        for group in 0..num_groups {
            let mut bg = self.bgdt.lock()[group];
            if bg.free_inodes_count > 0 {
                self.read_block(bg.inode_bitmap, page_phys as u64).await?;
                let bitmap_ptr = page_virt as *mut u8;
                let mut allocated_inode_idx = None;

                for byte_idx in 0..self.block_size as usize {
                    unsafe {
                        let val = *bitmap_ptr.add(byte_idx);
                        if val != 0xFF {
                            for bit in 0..8 {
                                if (val & (1 << bit)) == 0 {
                                    *bitmap_ptr.add(byte_idx) = val | (1 << bit);
                                    allocated_inode_idx = Some(byte_idx * 8 + bit);
                                    break;
                                }
                            }
                        }
                    }
                    if allocated_inode_idx.is_some() {
                        break;
                    }
                }

                if let Some(bit_idx) = allocated_inode_idx {
                    self.cache.write_block(bg.inode_bitmap as usize, page_phys as u64).await?;

                    let inode_num = group as u32 * self.inodes_per_group + bit_idx as u32 + 1;

                    bg.free_inodes_count -= 1;
                    if is_dir {
                        bg.used_dirs_count += 1;
                    }
                    self.bgdt.lock()[group] = bg;

                    let bgdt_start_block = if self.block_size == 1024 { 2 } else { 1 };
                    let descriptor_offset = group * size_of::<DiskGroupDesc>();
                    let target_logical_block = bgdt_start_block + (descriptor_offset as u32 / self.block_size);
                    let block_internal_offset = descriptor_offset % self.block_size as usize;

                    self.read_block(target_logical_block, page_phys as u64).await?;
                    unsafe {
                        let dest_ptr = (page_virt as *mut u8).add(block_internal_offset) as *mut DiskGroupDesc;
                        ptr::write(dest_ptr, bg);
                    }
                    self.cache.write_block(target_logical_block as usize, page_phys as u64).await?;

                    let sb_block = if self.block_size == 1024 { 1 } else { 0 };
                    let sb_internal_offset = if self.block_size == 1024 { 0 } else { 1024 };

                    self.read_block(sb_block, page_phys as u64).await?;
                    unsafe {
                        let sb_ptr = (page_virt as *mut u8).add(sb_internal_offset) as *mut DiskSuperblock;
                        if (*sb_ptr).free_inodes_count > 0 {
                            (*sb_ptr).free_inodes_count -= 1;
                        }
                    }
                    self.cache.write_block(sb_block as usize, page_phys as u64).await?;

                    ALLOCATOR.free(page_phys, PageSize::Size4K);
                    return Ok(inode_num);
                }
            }
        }

        ALLOCATOR.free(page_phys, PageSize::Size4K);
        Err(())
    }

    pub async fn free_inode(&self, inode_num: u32, is_dir: bool) -> Result<(), ()> {
        if inode_num == 0 {
            return Err(());
        }

        let _guard = self.allocation_lock.lock().await;

        let page_phys = ALLOCATOR.alloc(PageSize::Size4K);
        if page_phys == 0 {
            return Err(());
        };
        let page_virt = page_phys + *DIRECT_MAP_OFFSET;

        let group = ((inode_num - 1) / self.inodes_per_group) as usize;
        let local_idx = (inode_num - 1) % self.inodes_per_group;

        let mut bg = {
            let bgdt = self.bgdt.lock();
            if group >= bgdt.len() {
                ALLOCATOR.free(page_phys, PageSize::Size4K);
                return Err(());
            }
            bgdt[group]
        };

        if self.read_block(bg.inode_bitmap, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }

        let bitmap_ptr = page_virt as *mut u8;
        let byte_idx = (local_idx / 8) as usize;
        let bit_idx = (local_idx % 8) as usize;

        unsafe {
            let val = *bitmap_ptr.add(byte_idx);
            if (val & (1 << bit_idx)) == 0 {
                // prevent double freeing if alr free
                ALLOCATOR.free(page_phys, PageSize::Size4K);
                return Err(());
            }
            *bitmap_ptr.add(byte_idx) = val & !(1 << bit_idx);
        }

        if self.cache.write_block(bg.inode_bitmap as usize, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }

        bg.free_inodes_count += 1;
        if is_dir && bg.used_dirs_count > 0 {
            bg.used_dirs_count -= 1;
        }
        self.bgdt.lock()[group] = bg;

        let bgdt_start_block = if self.block_size == 1024 { 2 } else { 1 };
        let descriptor_offset = group * size_of::<DiskGroupDesc>();
        let target_logical_block = bgdt_start_block + (descriptor_offset as u32 / self.block_size);
        let block_internal_offset = descriptor_offset % self.block_size as usize;

        if self.read_block(target_logical_block, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }
        unsafe {
            let dest_ptr = (page_virt as *mut u8).add(block_internal_offset) as *mut DiskGroupDesc;
            ptr::write(dest_ptr, bg);
        }
        if self.cache.write_block(target_logical_block as usize, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }

        let sb_block = if self.block_size == 1024 { 1 } else { 0 };
        let sb_internal_offset = if self.block_size == 1024 { 0 } else { 1024 };

        if self.read_block(sb_block, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }
        unsafe {
            let sb_ptr = (page_virt as *mut u8).add(sb_internal_offset) as *mut DiskSuperblock;
            (*sb_ptr).free_inodes_count += 1;
        }
        if self.cache.write_block(sb_block as usize, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }

        ALLOCATOR.free(page_phys, PageSize::Size4K);
        Ok(())
    }

    pub async fn free_block(&self, block_id: u32) -> Result<(), ()> {
        if block_id == 0 {
            return Ok(()); // block hole, nothing to free
        }

        let _guard = self.allocation_lock.lock().await;

        let page_phys = ALLOCATOR.alloc(PageSize::Size4K);
        if page_phys == 0 {
            return Err(());
        };
        let page_virt = page_phys + *DIRECT_MAP_OFFSET;

        let first_data_block = if self.block_size == 1024 { 1 } else { 0 };
        if block_id < first_data_block {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }

        let relative_block = block_id - first_data_block;
        let group = (relative_block / self.blocks_per_group) as usize;
        let local_idx = relative_block % self.blocks_per_group;

        let mut bg = {
            let bgdt = self.bgdt.lock();
            if group >= bgdt.len() {
                ALLOCATOR.free(page_phys, PageSize::Size4K);
                return Err(());
            }
            bgdt[group]
        };

        if self.read_block(bg.block_bitmap, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }

        let bitmap_ptr = page_virt as *mut u8;
        let byte_idx = (local_idx / 8) as usize;
        let bit_idx = (local_idx % 8) as usize;

        unsafe {
            let val = *bitmap_ptr.add(byte_idx);
            if (val & (1 << bit_idx)) == 0 {
                // double free prevention like above
                ALLOCATOR.free(page_phys, PageSize::Size4K);
                return Err(());
            }
            *bitmap_ptr.add(byte_idx) = val & !(1 << bit_idx);
        }

        if self.cache.write_block(bg.block_bitmap as usize, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }

        bg.free_blocks_count += 1;
        self.bgdt.lock()[group] = bg;

        let bgdt_start_block = if self.block_size == 1024 { 2 } else { 1 };
        let descriptor_offset = group * size_of::<DiskGroupDesc>();
        let target_logical_block = bgdt_start_block + (descriptor_offset as u32 / self.block_size);
        let block_internal_offset = descriptor_offset % self.block_size as usize;

        if self.read_block(target_logical_block, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }
        unsafe {
            let dest_ptr = (page_virt as *mut u8).add(block_internal_offset) as *mut DiskGroupDesc;
            ptr::write(dest_ptr, bg);
        }
        if self.cache.write_block(target_logical_block as usize, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }

        let sb_block = if self.block_size == 1024 { 1 } else { 0 };
        let sb_internal_offset = if self.block_size == 1024 { 0 } else { 1024 };

        if self.read_block(sb_block, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }
        unsafe {
            let sb_ptr = (page_virt as *mut u8).add(sb_internal_offset) as *mut DiskSuperblock;
            (*sb_ptr).free_blocks_count += 1;
        }
        if self.cache.write_block(sb_block as usize, page_phys as u64).await.is_err() {
            ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }

        ALLOCATOR.free(page_phys, PageSize::Size4K);
        Ok(())
    }
}
