use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::executor::Executor;
use crate::executor::async_sleep::sleep_async;
use crate::sync::RwLock;
use crate::memory::vmo::{FileVmo, PagedBackingStore};
use crate::storage::blockdev::AsyncBlockDevice;
use crate::storage::fs::ext2::Ext2FileSystem;
use crate::storage::fs::ext2::obj::{
    Ext2Directory,
    Ext2File,
};
use crate::storage::partition::gpt::GptTable;

async fn ext2_writeback_daemon(fs: Arc<Ext2FileSystem>) {
    loop {
        // sleep 5 secs
        let _ = sleep_async(5000).await;

        let file_vmos: Vec<Arc<FileVmo>> = {
            let files = fs.active_files.lock();
            files.values().filter_map(|weak| weak.upgrade()).map(|file| file.file_vmo.clone()).collect()
        };
        let dirty_list: Vec<(u32, Arc<Ext2File>)> = {
            let dirty = fs.dirty_files.lock();
            dirty.iter().map(|(&num, file)| (num, Arc::clone(file))).collect()
        };

        for vmo in file_vmos {
            let _ = vmo.flush_to_disk().await;
        }
        for (inode_num, file) in dirty_list {
            if file.file_vmo.flush_to_disk().await.is_ok() {
                let is_still_dirty = file.file_vmo.has_dirty_pages();
                if !is_still_dirty {
                    fs.dirty_files.lock().remove(&inode_num);
                }
            }
        }

        let _ = fs.cache.flush().await;
    }
}

pub async fn mount_ext2_rootfs(raw_block_device: Arc<dyn AsyncBlockDevice>) -> Arc<Ext2Directory> {
    let mut gpt = GptTable::parse(raw_block_device).await.expect("Failed mounting GPT configuration table maps");

    let partition = Arc::new(gpt.partitions.remove(0));

    let ext2_fs = Arc::new(Ext2FileSystem::mount(partition).await.expect("Failed mounting Ext2 arch metadata"));

    // launch daemon after mounting
    let fs_clone = Arc::clone(&ext2_fs);
    Executor::new().spawn(ext2_writeback_daemon(fs_clone));

    let root_inode_data = ext2_fs.read_inode(2).await.expect("Failed parsing root inode structs");

    let root_dir_object = Arc::new(Ext2Directory { fs: ext2_fs, inode_num: 2, inode_data: RwLock::new(root_inode_data) });

    root_dir_object
}
