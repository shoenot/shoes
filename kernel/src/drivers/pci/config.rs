use hal::interrupts::compose_msi_message;

use hal::io::{
    inl,
    outl,
};

use crate::memory::hal_map_mmio;

pub fn pci_build_addr(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    (bus as u32) << 16 | (slot as u32) << 11 | (func as u32) << 8 | (offset & 0xFC) as u32 | 0x8000_0000
}

pub fn pci_config_read_16(bus: u8, slot: u8, func: u8, offset: u8) -> u16 {
    let addr = pci_build_addr(bus, slot, func, offset);
    unsafe { outl(0xCF8, addr) };
    let res = unsafe { inl(0xCFC) };
    ((res >> ((offset & 2) * 8)) & 0xFFFF) as u16
}

pub fn pci_config_read_32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let addr = pci_build_addr(bus, slot, func, offset);
    unsafe { outl(0xCF8, addr) };
    unsafe { inl(0xCFC) }
}

pub fn pci_config_write_32(bus: u8, slot: u8, func: u8, offset: u8, value: u32) {
    let addr = pci_build_addr(bus, slot, func, offset);
    unsafe { outl(0xCF8, addr) };
    unsafe { outl(0xCFC, value) };
}

pub fn pci_config_write_16(bus: u8, slot: u8, func: u8, offset: u8, value: u16) {
    // write a 16-bit value into the PCI config space by preserving the surrounding 32-bit dword
    let aligned = offset & !0x3;
    let mut dw = pci_config_read_32(bus, slot, func, aligned);
    let shift = ((offset & 0x2) as u32) * 8;
    dw = (dw & !(0xFFFFu32 << shift)) | ((value as u32) << shift);
    pci_config_write_32(bus, slot, func, aligned, dw);
}

pub fn pci_has_msix(bus: u8, slot: u8, func: u8) -> bool { pci_find_msix_cap(bus, slot, func).is_some() }

/// (cap_offset, bir, table_offset, table_size_entries)
pub fn pci_find_msix_cap(bus: u8, slot: u8, func: u8) -> Option<(u8, u8, u32, usize)> {
    let status = pci_config_read_32(bus, slot, func, 0x4);
    if ((status >> 16) & 0x10) == 0 {
        return None;
    }

    let mut cap_ptr = (pci_config_read_32(bus, slot, func, 0x34) & 0xFF) as u8;
    while cap_ptr != 0 {
        let cap = pci_config_read_32(bus, slot, func, cap_ptr);
        let cap_id = (cap & 0xFF) as u8;
        if cap_id == 0x11 {
            // table bir and offset are at cap_ptr + 4
            let table_dw = pci_config_read_32(bus, slot, func, cap_ptr + 4);
            let bir = (table_dw & 0x7) as u8;
            let table_offset = table_dw & !0x7u32;

            // message control is at cap_ptr + 2 (u16)
            let msg_ctrl = pci_config_read_32(bus, slot, func, cap_ptr + 2) as u16;
            let table_size_field = (msg_ctrl & 0x07FF) as usize; // lower 11 bits
            let entries = table_size_field + 1;

            return Some((cap_ptr, bir, table_offset, entries));
        }
        cap_ptr = ((cap >> 8) & 0xFF) as u8;
    }
    None
}

pub fn pci_setup_msix_entry(bus: u8, slot: u8, func: u8, vector: u8, target_core: usize, entry_idx: usize) -> Result<(), ()> {
    use core::ptr::{
        read_volatile,
        write_volatile,
    };

    use crate::drivers::pci::{
        PCIBar,
        PCIDevice,
        get_bar,
    };
    use crate::memory::{
        DIRECT_MAP_OFFSET,
        PAGER,
    };

    let (cap_off, bir, table_offset, entries) = pci_find_msix_cap(bus, slot, func).ok_or(())?;
    if entry_idx >= entries {
        return Err(());
    }

    let reg0 = pci_config_read_32(bus, slot, func, 0x0);
    let dev = PCIDevice {
        bus,
        slot,
        func,
        vendor_id: (reg0 & 0xFFFF) as u16,
        device_id: (reg0 >> 16) as u16,
        class: (pci_config_read_32(bus, slot, func, 0x8) >> 24) as u8,
        subclass: (pci_config_read_32(bus, slot, func, 0x8) >> 16) as u8,
        header_type: (pci_config_read_32(bus, slot, func, 0xC) >> 16) as u8,
    };

    let bar = get_bar(dev, bir);
    let bar_base = match bar {
        PCIBar::Memory { addr, .. } => addr,
        _ => return Err(()),
    };

    let table_phys = bar_base + table_offset as u64;
    let page_size = 4096u64;
    let total_bytes = (entries * 16) as u64;

    // map the msi-x table if not already mapped
    for p in (table_phys / page_size)..=((table_phys + total_bytes - 1) / page_size) {
        hal_map_mmio(p * page_size, 4096);
    }

    let table_virt = table_phys + *DIRECT_MAP_OFFSET as u64;

    let entry_ptr = (table_virt + (entry_idx * 16) as u64) as *mut u32;
    let (msg_addr_low, msg_addr_high, msg_data) = compose_msi_message(target_core, vector);

    unsafe {
        write_volatile(entry_ptr, msg_addr_low);
        write_volatile(entry_ptr.add(1), msg_addr_high);
        write_volatile(entry_ptr.add(2), msg_data);
        let vec_ctrl = entry_ptr.add(3);
        write_volatile(vec_ctrl, read_volatile(vec_ctrl) & !1); // unmask
    }

    // enable msi-x in message control
    let ctrl_off = cap_off + 2;
    let mut msg_ctrl = (pci_config_read_32(bus, slot, func, ctrl_off & !3) >> ((ctrl_off & 2) * 8)) as u16;
    msg_ctrl |= 1 << 15; // enable
    msg_ctrl &= !(1 << 14); // function mask clear
    pci_config_write_16(bus, slot, func, ctrl_off, msg_ctrl);

    Ok(())
}
