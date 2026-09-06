use alloc::vec::Vec;
use core::ptr::null_mut;

use crate::drivers::pci::{
    PCIBar,
    PCIDevice,
    get_bar,
    pci_config_read_32,
    pci_get_dev,
};
use crate::memory::{
    DIRECT_MAP_OFFSET, PAGER, hal_map_mmio,
};

#[derive(Debug)]
pub struct VirtioCapability {
    pub cap_id: u8,
    pub next_cap: u8,
    pub len: u8,
    pub cfg_type: u8,
    pub bar_idx: u8,
    pub bar_offset: u32,
    pub bar_len: u32,
    pub notify_off_multiplier: u32,
}

#[derive(Debug)]
pub struct VirtioBlockDriver {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub common_cfg: *mut VirtioCommonCfg,
    pub notify_base: *mut u8,
    pub notify_off_multiplier: u32,
    pub isr_cfg: *mut u8,
    pub device_cfg: *mut u8,
}

#[repr(C, packed)]
pub struct VirtioCommonCfg {
    pub dev_feature_select: u32,
    pub dev_feature: u32,
    pub driv_feature_select: u32,
    pub driv_feature: u32,
    pub config_msix_vector: u16,
    pub num_queues: u16,
    pub device_status: u8,
    pub config_gen: u8,
    pub queue_select: u16,
    pub queue_size: u16,
    pub queue_msix_vector: u16,
    pub queue_enable: u16,
    pub queue_notify_off: u16,
    pub queue_desc: u64,
    pub queue_driver: u64,
    pub queue_device: u64,
}

pub fn init_virtio() -> Option<VirtioBlockDriver> {
    let dev = pci_get_dev(0x1AF4, 0x1042)?;
    let caps = walk_cap(dev);

    let mut common_cfg = null_mut();
    let mut notify_base = null_mut();
    let mut notify_off_multiplier = 0;
    let mut isr_cfg = null_mut();
    let mut device_cfg = null_mut();

    let mut mapped_bars = [0; 6];
    for cap in caps {
        let bar_idx = cap.bar_idx as usize;
        if bar_idx >= 6 {
            continue;
        }

        let bar_addr = match get_bar(dev, cap.bar_idx) {
            PCIBar::Memory { addr, .. } => addr,
            PCIBar::IOSpace { .. } => continue,
        };

        let cap_start = bar_addr + cap.bar_offset as u64;
        let cap_len = core::cmp::max(cap.bar_len as u64, 1);
        let cap_end = cap_start + cap_len;

        let start_phys = cap_start & !0xFFF;
        let end_phys = cap_end.div_ceil(4096) * 4096;

        {
            let mut page_phys = start_phys;
            while page_phys < end_phys {
                hal_map_mmio(page_phys, 4096).unwrap();
                page_phys += 4096;
            }
        }

        if mapped_bars[bar_idx] == 0 {
            mapped_bars[bar_idx] = bar_addr + *DIRECT_MAP_OFFSET as u64;
        }

        let block_virt = cap_start + *DIRECT_MAP_OFFSET as u64;

        match cap.cfg_type {
            1 => common_cfg = block_virt as *mut VirtioCommonCfg,
            2 => {
                notify_base = block_virt as *mut u8;
                notify_off_multiplier = cap.notify_off_multiplier;
            }
            3 => isr_cfg = block_virt as *mut u8,
            4 => device_cfg = block_virt as *mut u8,
            _ => {}
        }
    }

    if common_cfg.is_null() || notify_base.is_null() || isr_cfg.is_null() || device_cfg.is_null() {
        return None;
    }

    Some(VirtioBlockDriver {
        bus: dev.bus,
        slot: dev.slot,
        func: dev.func,
        common_cfg,
        notify_base,
        notify_off_multiplier,
        isr_cfg,
        device_cfg,
    })
}

fn read_byte_slice(val: u32, slice_idx: u8) -> u8 {
    let start_bit = slice_idx * 8;
    ((val >> start_bit) & 0xFF) as u8
}

pub fn walk_cap(dev: PCIDevice) -> Vec<VirtioCapability> {
    let cap_ptr = (pci_config_read_32(dev.bus, dev.slot, dev.func, 0x34) & 0xFF) as u8;
    let mut current_ptr = cap_ptr;
    let mut caps = Vec::new();
    if current_ptr == 0 {
        return caps;
    }
    while current_ptr != 0 {
        let cap = pci_config_read_32(dev.bus, dev.slot, dev.func, current_ptr);
        let cap_id = read_byte_slice(cap, 0);
        let next_cap = read_byte_slice(cap, 1);
        let len = read_byte_slice(cap, 2);
        let cfg_type = read_byte_slice(cap, 3);
        if cap_id == 0x9 {
            let bar_idx = read_byte_slice(pci_config_read_32(dev.bus, dev.slot, dev.func, current_ptr + 4), 0);
            let bar_offset = pci_config_read_32(dev.bus, dev.slot, dev.func, current_ptr + 8);
            let bar_len = pci_config_read_32(dev.bus, dev.slot, dev.func, current_ptr + 12);
            let notify_off_multiplier = if cfg_type == 2 { pci_config_read_32(dev.bus, dev.slot, dev.func, current_ptr + 16) } else { 0 };
            let capability = VirtioCapability { cap_id, next_cap, len, cfg_type, bar_idx, bar_offset, bar_len, notify_off_multiplier };
            caps.push(capability)
        }
        current_ptr = next_cap;
    }
    caps
}
