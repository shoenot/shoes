use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::drivers::virtio::blk::BlockTransferFuture;
use crate::klogln;
use crate::memory::{
    ALLOCATOR, DIRECT_MAP_OFFSET, PageSize, PhysBuffer, calculate_order,
};
use crate::storage::blockdev::AsyncBlockDevice;

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GptGuid {
    pub a: u32,
    pub b: u16,
    pub c: u16,
    pub d: [u8; 2],
    pub e: [u8; 6],
}

#[derive(Debug)]
#[repr(C, packed)]
pub struct GptHeader {
    signature: u64, // "EFI PART"
    revision: u32,
    header_size: u32,
    header_checksum: u32,
    reserved_zero: u32,
    current_lba: u64,
    backup_lba: u64,
    first_lba: u64,
    last_lba: u64,
    disk_guid: GptGuid,
    entry_table_lba: u64,
    num_entries: u32,
    entry_size: u32,
    table_checksum: u32,
    padding: [u8; 420],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub struct GptEntry {
    pub part_type_guid: GptGuid,
    pub uniq_part_guid: GptGuid,
    pub starting_lba: u64,
    pub ending_lba: u64,
    pub attrs: u64,
    pub partition_name: [u16; 36], // UTF-16LE
}

#[derive(Debug)]
pub struct GptPartition {
    pub raw_device: Arc<dyn AsyncBlockDevice>,
    pub start_sector: u64,
    pub total_sectors: u64,
}

impl AsyncBlockDevice for GptPartition {
    fn read_sectors(&self, sector: u64, sectors_count: u32, buf_phys: u64) -> Result<BlockTransferFuture, ()> {
        if sector + sectors_count as u64 > self.total_sectors {
            return Err(());
        }

        let absolute_sector = self.start_sector + sector;
        // forward to the hardware driver
        self.raw_device.read_sectors(absolute_sector, sectors_count, buf_phys)
    }

    fn write_sectors(&self, sector: u64, sectors_count: u32, buf_phys: u64) -> Result<BlockTransferFuture, ()> {
        if sector + sectors_count as u64 > self.total_sectors {
            return Err(());
        }

        let absolute_sector = self.start_sector + sector;
        // forward to the hardware driver
        self.raw_device.write_sectors(absolute_sector, sectors_count, buf_phys)
    }
}

pub struct GptTable {
    pub partitions: Vec<GptPartition>,
}

impl GptTable {
    pub async fn parse(raw_device: Arc<dyn AsyncBlockDevice>) -> Result<Self, ()> {
        let mut partitions = Vec::new();

        let header_buffer = PhysBuffer::new().ok_or(())?;

        // fetch lba 1 (primary gpt header block)
        klogln!("[GPT] reading GPT header lba=1 into phys=0x{:x}", header_buffer.phys());
        let header_future = raw_device.read_sectors(1, 1, header_buffer.phys() as u64)?;
        header_future.await?;
        klogln!("[GPT] header read complete");

        let header = unsafe { &*(header_buffer.virt() as *const GptHeader) };
        if header.signature != 0x5452415020494645 {
            return Err(());
        }

        let entry_lba = header.entry_table_lba;
        let num_entries = header.num_entries as usize;
        let entry_size = header.entry_size as usize;
        klogln!("[GPT] entry_lba={} num_entries={} entry_size={}", entry_lba, num_entries, entry_size);
        let total_table_bytes = num_entries * entry_size;
        let table_sectors = (total_table_bytes + 511) / 512; // round up to sector units

        if num_entries > 128 || entry_size != 128 {
            return Err(());
        }

        let table_bytes_needed = total_table_bytes + 4095;
        let table_order = calculate_order(table_bytes_needed);

        let table_buf_phys = match ALLOCATOR.alloc_order(table_order) {
            Some(a) => a,
            None => {
                return Err(());
            }
        } as u64;
        let table_buf_virt = table_buf_phys + *DIRECT_MAP_OFFSET as u64;

        let table_future = raw_device.read_sectors(entry_lba, table_sectors as u32, table_buf_phys)?;
        table_future.await?;

        for i in 0..num_entries {
            unsafe {
                let entry_offset = i * entry_size;
                let entry_ptr = (table_buf_virt as *const u8).add(entry_offset) as *const GptEntry;

                if (*entry_ptr).part_type_guid.a == 0 && (*entry_ptr).part_type_guid.b == 0 {
                    continue; //slot empty
                }

                let starting_lba = (*entry_ptr).starting_lba;
                let ending_lba = (*entry_ptr).ending_lba;
                let total_sectors = (ending_lba - starting_lba) + 1;

                partitions.push(GptPartition { raw_device: Arc::clone(&raw_device), start_sector: starting_lba, total_sectors });
            }
        }

        ALLOCATOR.free_order(table_buf_phys as usize, table_order);
        Ok(GptTable { partitions })
    }
}
