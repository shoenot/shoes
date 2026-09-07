use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::boot::direct_map_offset;
use core::pin::Pin;
use core::ptr::{
    addr_of,
    addr_of_mut,
    read_volatile,
    write_bytes,
    write_volatile,
};
use core::sync::atomic::{
    AtomicBool,
    AtomicPtr,
    AtomicU8,
    Ordering,
    fence,
};
use core::task::{
    Context,
    Poll,
};

use hal::interrupts::{
    disable_interrupts,
    enable_interrupts,
    interrupts_enabled,
};

use crate::cpu::{current_core, current_core_id};
use crate::cpu::current_core_mut;
use crate::executor::EXECUTOR_THREAD_PTR;
use crate::executor::waiter::AsyncWaiter;
use crate::sync::TicketLock;
use crate::sched::dispatch::{
    cancel_block_if_awoken,
    wake_thread,
};
use crate::sched::{
    Thread,
    ThreadState,
};
use crate::time;
use crate::drivers::virtio::mmio::{
    VirtioBlockDriver,
    init_virtio,
};
use crate::interrupts::alloc::MsiHandle;
use crate::memory::{
    ALLOCATOR, DIRECT_MAP_OFFSET, PageSize, PhysBuffer,
};
use crate::storage::blockdev::{
    AsyncBlockDevice,
    DmaBuffer,
};
use crate::util::bitwise::{
    set_bit,
    unset_bit,
};

#[repr(C, packed)]
pub struct VqDescriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

#[repr(C, packed)]
#[derive(Debug)]
pub struct VqAvailableRing {
    pub flags: *mut u16,
    pub idx: *mut u16,
    pub ring: *mut u16,
    pub _used_event: *mut u16,
}

#[repr(C, packed)]
#[derive(Debug)]
pub struct VqUsedRing {
    pub flags: *mut u16,
    pub idx: *mut u16,
    pub ring: *mut VqUsedElem,
    pub _avail_event: *mut u16,
}

#[repr(C, packed)]
#[derive(Debug)]
pub struct VqUsedElem {
    id: u32,
    len: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct Virtqueue {
    pub desc: *mut VqDescriptor,
    pub available: VqAvailableRing,
    pub used: VqUsedRing,

    // phys addrs for cleanup and tracking
    pub desc_phys: usize,
    pub av_phys: usize,
    pub used_phys: usize,

    // alloc orders for deallocation
    pub desc_order: usize,
    pub av_order: usize,
    pub used_order: usize,

    pub queue_size: u16,
    pub free_head: u16,
    pub last_seen_used: u16,
    pub queue_notify_off: u16,

    pub requests: TicketLock<Vec<Option<Arc<BlockRequest>>>>,
}

impl Drop for Virtqueue {
    fn drop(&mut self) {
        // auto release when dropped
        let allocator = &crate::memory::ALLOCATOR;
        allocator.free_order(self.desc_phys, self.desc_order);
        allocator.free_order(self.av_phys, self.av_order);
        allocator.free_order(self.used_phys, self.used_order);
    }
}

#[derive(Debug)]
pub struct VirtqueueState {
    pub vq: TicketLock<Virtqueue>,
    pub worker_tcb: AtomicPtr<Thread>,
    pub has_interrupts: AtomicBool,
    pub awoken: AtomicBool,
}

#[derive(Debug)]
pub struct VirtioBlockDevice {
    pub driver: VirtioBlockDriver,
    pub queues: Vec<VirtqueueState>,
    pub msi_handle: Option<MsiHandle>,
}

pub const RESULT_PENDING: u8 = 0;
pub const RESULT_OK: u8 = 1;
pub const RESULT_ERROR: u8 = 2;

#[derive(Debug)]
pub struct BlockRequest {
    d0: u16,
    d1: u16,
    d2: u16,
    page_phys: usize,
    dma_buffer: Arc<DmaBuffer>,

    completed: AtomicBool,
    result: AtomicU8,
    waiter: Arc<AsyncWaiter>,
}

fn calculate_order(bytes: usize) -> usize {
    let mut order = 0;
    while (1 << order) * 4096 < bytes {
        order += 1;
    }
    order
}

pub fn perform_handshake(drv: &VirtioBlockDriver) {
    unsafe {
        let cfg = &mut *drv.common_cfg;
        let status_ptr = addr_of_mut!(cfg.device_status) as *mut u8;
        write_volatile(status_ptr, 0);
        let mut scratch = read_volatile(status_ptr);
        write_volatile(status_ptr, scratch | 1); // acknowledgement
        scratch = read_volatile(status_ptr);
        write_volatile(status_ptr, scratch | 2); // driver
    }
}

pub fn negotiate_features(drv: &VirtioBlockDriver) {
    unsafe {
        let cfg = &mut *drv.common_cfg;
        let dev_feat_sel_ptr = addr_of_mut!(cfg.dev_feature_select) as *mut u32;
        let dev_feat_ptr = addr_of_mut!(cfg.dev_feature) as *mut u32;
        write_volatile(dev_feat_sel_ptr, 0);
        let mut lower = read_volatile(dev_feat_ptr);
        write_volatile(dev_feat_sel_ptr, 1);
        let mut upper = read_volatile(dev_feat_ptr);
        upper = set_bit(upper, 0); // VIRTIO_F_VERSION_1
        upper = unset_bit(upper, 2); // VIRTIO_F_RING_PACKED
        lower = set_bit(lower, 12); // VIRTIO_BLK_F_MQ
        lower = unset_bit(lower, 28); // VIRTIO_F_INDIRECT_DESC
        // enable event_idx only if device supports it
        if (lower & (1u32 << 29)) != 0 {
            lower = set_bit(lower, 29); // VIRTIO_F_EVENT_IDX
        } else {
            lower = unset_bit(lower, 29);
        }
        lower = set_bit(lower, 1); // VIRTIO_BLK_F_SIZE_MAX 
        lower = set_bit(lower, 2); // VIRTIO_BLK_F_SEG_MAX 
        lower = set_bit(lower, 5); // VIRTIO_BLK_F_RO 
        lower = set_bit(lower, 6); // VIRTIO_BLK_F_BLK_SIZE 
        lower = set_bit(lower, 9); // VIRTIO_BLK_F_FLUSH 
        let driv_feat_sel_ptr = addr_of_mut!(cfg.driv_feature_select) as *mut u32;
        let driv_feat_ptr = addr_of_mut!(cfg.driv_feature) as *mut u32;
        write_volatile(driv_feat_sel_ptr, 0);
        write_volatile(driv_feat_ptr, lower);
        write_volatile(driv_feat_sel_ptr, 1);
        write_volatile(driv_feat_ptr, upper);
    }
}

pub fn vq_setup(drv: &VirtioBlockDriver, q_idx: u16) -> Option<Virtqueue> {
    unsafe {
        let cfg = &mut *drv.common_cfg;
        let q_sel_ptr = addr_of_mut!(cfg.queue_select) as *mut u16;
        write_volatile(q_sel_ptr, q_idx);
        let q_size_ptr = addr_of_mut!(cfg.queue_size) as *mut u16;
        let q_size = read_volatile(q_size_ptr);
        if q_size == 0 {
            return None;
        }

        let desc_bytes = q_size as usize * 16;
        let av_bytes = 6 + (q_size as usize * 2);
        let used_bytes = 6 + (q_size as usize * 8);

        let desc_order = calculate_order(desc_bytes);
        let av_order = calculate_order(av_bytes);
        let used_order = calculate_order(used_bytes);

        let desc_phys = ALLOCATOR.alloc_order(desc_order)?;
        let av_phys = ALLOCATOR.alloc_order(av_order)?;
        let used_phys = ALLOCATOR.alloc_order(used_order)?;

        let direct_map_offset = *DIRECT_MAP_OFFSET;
        let desc_virt = (desc_phys + direct_map_offset) as *mut VqDescriptor;
        let av_virt_base = av_phys + direct_map_offset;
        let used_virt_base = used_phys + direct_map_offset;

        write_bytes(desc_virt as *mut u8, 0, (1 << desc_order) * 4096);
        write_bytes(av_virt_base as *mut u8, 0, (1 << av_order) * 4096);
        write_bytes(used_virt_base as *mut u8, 0, (1 << used_order) * 4096);

        // make a free list of descriptors
        for i in 0..(q_size - 1) {
            let desc_ptr = desc_virt.add(i as usize);
            let next_desc = VqDescriptor { addr: 0, len: 0, flags: 1, next: i + 1 };
            write_volatile(desc_ptr, next_desc);
        }

        // last descriptor needs to terminate chain with 0 flag and 0xffff next
        let last_desc_ptr = desc_virt.add((q_size - 1) as usize);
        let last_desc = VqDescriptor { addr: 0, len: 0, flags: 0, next: 0xFFFF };
        write_volatile(last_desc_ptr, last_desc);

        let av_flags = av_virt_base as *mut u16;
        let av_idx = (av_virt_base + 2) as *mut u16;
        let av_ring = (av_virt_base + 4) as *mut u16;
        let _used_event = (av_virt_base + 4 + (q_size as usize * 2)) as *mut u16;

        let available = VqAvailableRing { flags: av_flags, idx: av_idx, ring: av_ring, _used_event };

        let used_flags = used_virt_base as *mut u16;
        let used_idx = (used_virt_base + 2) as *mut u16;
        let used_ring = (used_virt_base + 4) as *mut VqUsedElem;
        let _avail_event = (used_virt_base + 4 + (q_size as usize * 8)) as *mut u16;

        let used = VqUsedRing { flags: used_flags, idx: used_idx, ring: used_ring, _avail_event };

        let queue_desc_ptr = addr_of_mut!(cfg.queue_desc) as *mut u64;
        let queue_driver_ptr = addr_of_mut!(cfg.queue_driver) as *mut u64;
        let queue_device_ptr = addr_of_mut!(cfg.queue_device) as *mut u64;

        write_volatile(queue_desc_ptr, desc_phys as u64);
        write_volatile(queue_driver_ptr, av_phys as u64);
        write_volatile(queue_device_ptr, used_phys as u64);

        // do not enable the queue here. the caller should enable the queue after any msi/x setup
        // so the device can be informed of msi-x table index prior to queue activation.
        let notify_off_ptr = addr_of_mut!(cfg.queue_notify_off) as *mut u16;
        let queue_notify_off = read_volatile(notify_off_ptr);

        let mut requests = Vec::new();
        requests.resize(q_size as usize, None);
        let requests = TicketLock::new(requests);

        Some(Virtqueue {
            desc: desc_virt,
            available,
            used,
            desc_phys,
            av_phys,
            used_phys,
            desc_order,
            av_order,
            used_order,
            queue_size: q_size,
            free_head: 0,
            last_seen_used: 0,
            queue_notify_off,
            requests,
        })
    }
}

pub fn init_block_device() -> Option<VirtioBlockDevice> {
    unsafe {
        let drv = init_virtio()?;
        let cfg = &mut *drv.common_cfg;
        let status_ptr = addr_of_mut!(cfg.device_status) as *mut u8;

        perform_handshake(&drv);
        negotiate_features(&drv);

        let dev_feat_ptr = addr_of_mut!((*cfg).dev_feature) as *mut u32;
        let lower = read_volatile(dev_feat_ptr);
        let num_queues = if (lower & (1u32 << 12)) != 0 { read_volatile(addr_of!((*cfg).num_queues)) } else { 1 };

        let status = read_volatile(status_ptr);
        write_volatile(status_ptr, status | 8); // write FEATURES_OK
        let verify = read_volatile(status_ptr);
        if (verify & 8) == 0 {
            return None;
        }

        let num_to_setup = core::cmp::min(num_queues as usize, *crate::cpu::NUM_CORES.get().unwrap());
        let mut queues = Vec::new();
        for i in 0..num_to_setup {
            let vq = vq_setup(&drv, i as u16)?;
            queues.push(VirtqueueState {
                vq: TicketLock::new(vq),
                worker_tcb: AtomicPtr::new(core::ptr::null_mut()),
                has_interrupts: AtomicBool::new(false),
                awoken: AtomicBool::new(false),
            });
        }

        let status = read_volatile(status_ptr);
        write_volatile(status_ptr, status | 4); // write DRIVER_OK

        Some(VirtioBlockDevice { driver: drv, queues, msi_handle: None })
    }
}

impl Virtqueue {
    pub fn alloc_desc(&mut self) -> Result<usize, ()> {
        let brw_idx = self.free_head;
        if brw_idx == 0xFFFF {
            return Err(());
        };
        unsafe {
            let brw = self.desc.add(brw_idx as usize);
            self.free_head = (*brw).next;
            Ok(brw_idx as usize)
        }
    }

    pub fn free_desc(&mut self, idx: u16) {
        unsafe {
            let brw = self.desc.add(idx as usize);
            (*brw).next = self.free_head;
            self.free_head = idx;
        }
    }
}

impl BlockRequest {
    fn new(d0: u16, d1: u16, d2: u16, page_phys: usize, dma_buffer: Arc<DmaBuffer>) -> Arc<Self> {
        Arc::new(Self {
            d0,
            d1,
            d2,
            page_phys,
            dma_buffer,
            completed: AtomicBool::new(false),
            result: AtomicU8::new(RESULT_PENDING),
            waiter: AsyncWaiter::new(),
        })
    }
}

fn free_submission_resources(vq: &mut Virtqueue, page_phys: usize, descs: &[u16]) {
    for &desc in descs.iter().rev() {
        vq.free_desc(desc);
    }
    crate::memory::ALLOCATOR.free(page_phys, PageSize::Size4K);
}

fn alloc_request_descs(vq: &mut Virtqueue, page_phys: usize) -> Result<(u16, u16, u16), ()> {
    let d0 = match vq.alloc_desc() {
        Ok(d) => d as u16,
        Err(_) => {
            crate::memory::ALLOCATOR.free(page_phys, PageSize::Size4K);
            return Err(());
        }
    };
    let d1 = match vq.alloc_desc() {
        Ok(d) => d as u16,
        Err(_) => {
            free_submission_resources(vq, page_phys, &[d0]);
            return Err(());
        }
    };
    let d2 = match vq.alloc_desc() {
        Ok(d) => d as u16,
        Err(_) => {
            free_submission_resources(vq, page_phys, &[d0, d1]);
            return Err(());
        }
    };

    Ok((d0, d1, d2))
}

fn take_request(vq: &Virtqueue, desc_id: usize) -> Option<Arc<BlockRequest>> {
    let mut requests = vq.requests.lock();
    if desc_id < requests.len() { requests[desc_id].take() } else { None }
}

fn store_request(vq: &Virtqueue, request: &Arc<BlockRequest>) { vq.requests.lock()[request.d0 as usize] = Some(request.clone()); }

fn publish_available(vq: &Virtqueue, idx: u16, doorbell_ptr: *mut u16, before_publish: impl FnOnce(&Virtqueue)) {
    before_publish(vq);
    fence(Ordering::Release);
    unsafe {
        write_volatile(vq.available.idx, idx.wrapping_add(1));
        if !doorbell_ptr.is_null() {
            fence(Ordering::SeqCst);
            write_volatile(doorbell_ptr, 0);
        }
    }
}

fn complete_request(vq: &mut Virtqueue, request: &Arc<BlockRequest>, status: u8) {
    let result = if status == 0 { RESULT_OK } else { RESULT_ERROR };
    request.result.store(result, Ordering::Release);
    request.completed.store(true, Ordering::Release);
    request.waiter.wake();

    vq.free_desc(request.d0);
    vq.free_desc(request.d1);
    vq.free_desc(request.d2);

    crate::memory::ALLOCATOR.free(request.page_phys, PageSize::Size4K);
}

pub const VIRTIO_BLK_T_IN: u32 = 0; // READ
pub const VIRTIO_BLK_T_OUT: u32 = 1; // WRITE
pub const VIRTIO_BLK_T_FLUSH: u32 = 4; // FLUSH

#[repr(C, packed)]
pub struct VirtioBlkReqHeader {
    pub req_type: u32,
    pub reserved: u32,
    pub sector: u64,
}

impl AsyncBlockDevice for VirtioBlockDevice {
    fn read_sectors(&self, sector: u64, sectors_count: u32, buf_phys: u64) -> Result<BlockTransferFuture, ()> {
        self.transfer_sectors(sector, sectors_count, buf_phys, false)
    }

    fn write_sectors(&self, sector: u64, sectors_count: u32, buf_phys: u64) -> Result<BlockTransferFuture, ()> {
        self.transfer_sectors(sector, sectors_count, buf_phys, true)
    }
}

unsafe impl Send for VirtioBlockDevice {}
unsafe impl Sync for VirtioBlockDevice {}

impl VirtioBlockDevice {
    pub fn setup_interrupts(self: &Arc<Self>) -> Result<(), ()> {
        use core::ptr::write_volatile;

        use crate::KERNEL_PROCESS;
        use crate::sched::dispatch::spawn_kernel_thread;
        use crate::sched::priority::ThreadPriority;
        use crate::drivers::pci::pci_has_msix;
        use crate::interrupts::alloc::{
            msi_allocate,
            msi_register,
        };

        let has_msix = pci_has_msix(self.driver.bus, self.driver.slot, self.driver.func);
        if !has_msix {
            unsafe {
                let cfg = &mut *self.driver.common_cfg;
                write_volatile(addr_of_mut!((*cfg).queue_enable), 1);
            }
            return Err(());
        }

        let num_queues = self.queues.len();
        let handle = msi_allocate(num_queues, 0).map_err(|_| {
            unsafe {
                let cfg = &mut *self.driver.common_cfg;
                write_volatile(addr_of_mut!((*cfg).queue_enable), 1);
            }
            ()
        })?;

        for i in 0..num_queues {
            let vq_state_ptr = &self.queues[i] as *const VirtqueueState as usize;

            // map queue i -> entry i -> core i
            let res = msi_register(
                &handle,
                i,
                self.driver.bus,
                self.driver.slot,
                self.driver.func,
                virtio_blk_irq_handler,
                vq_state_ptr,
                i, // hardware entry index i
                i, // target core i
            );

            match res {
                Ok(vector) => {
                    unsafe {
                        let vq_state_mut = &self.queues[i] as *const VirtqueueState as *mut VirtqueueState;

                        // spawn worker thread for this queue
                        let worker_tcb = spawn_kernel_thread(
                            virtio_blk_worker_thread as *const () as usize,
                            vq_state_ptr,
                            ThreadPriority::HIGH,
                            KERNEL_PROCESS.clone(),
                        );
                        (*vq_state_mut).worker_tcb.store(worker_tcb, Ordering::Release);

                        let cfg = &mut *self.driver.common_cfg;
                        write_volatile(addr_of_mut!((*cfg).queue_select), i as u16);
                        write_volatile(addr_of_mut!((*cfg).queue_msix_vector), i as u16);
                        write_volatile(addr_of_mut!((*cfg).queue_enable), 1);
                    }
                    crate::klogln!("[VIRTIO] Queue {} registered: vector={} entry={} core={}", i, vector, i, i);
                }
                Err(_) => unsafe {
                    let cfg = &mut *self.driver.common_cfg;
                    write_volatile(addr_of_mut!((*cfg).queue_select), i as u16);
                    write_volatile(addr_of_mut!((*cfg).queue_enable), 1);
                },
            }
        }

        unsafe {
            let dev_ptr = Arc::as_ptr(self) as *mut VirtioBlockDevice;
            (*dev_ptr).msi_handle = Some(handle);

            // setup config interrupt as well
            let cfg = &mut *self.driver.common_cfg;
            write_volatile(addr_of_mut!((*cfg).config_msix_vector), 0); // Reuse entry 0 for config
        }

        Ok(())
    }

    pub fn transfer_sectors(&self, sector: u64, sectors_count: u32, buf_phys: u64, is_write: bool) -> Result<BlockTransferFuture, ()> {
        let drv = &self.driver;
        let core_id = current_core_id();
        let vq_idx = core_id % self.queues.len();
        let vq_state = &self.queues[vq_idx];

        unsafe {
            let buffer = PhysBuffer::new().ok_or(())?;
            let dma_len = sectors_count as usize * 512;
            let dma_buffer = if is_write { DmaBuffer::from_phys(buf_phys as usize, dma_len)? } else { DmaBuffer::new(dma_len)? };

            write_bytes(buffer.virt() as *mut u8, 0, 4096);

            let req_hdr = VirtioBlkReqHeader { req_type: if is_write { VIRTIO_BLK_T_OUT } else { VIRTIO_BLK_T_IN }, reserved: 0, sector };
            let hdr_ptr = buffer.virt() as *mut VirtioBlkReqHeader;
            write_volatile(hdr_ptr, req_hdr);

            let status_ptr = (buffer.virt() + 512) as *mut u8;
            write_volatile(status_ptr, 0xFF);

            let request = {
                let mut vq = vq_state.vq.lock();
                let (d0, d1, d2) = alloc_request_descs(&mut vq, buffer.phys())?;

                // chain desc 0 - header
                let desc0 = vq.desc.add(d0 as usize);
                write_volatile(
                    desc0,
                    VqDescriptor {
                        addr: buffer.phys() as u64,
                        len: 16,
                        flags: 1, // next flag
                        next: d1,
                    },
                );

                // chain desc 1 - data buffer
                let desc1 = vq.desc.add(d1 as usize);
                write_volatile(
                    desc1,
                    VqDescriptor {
                        addr: dma_buffer.phys() as u64,
                        len: sectors_count * 512,
                        flags: if is_write { 1 } else { 3 },
                        next: d2,
                    },
                );

                // chain desc 2 - status byte
                let desc2 = vq.desc.add(d2 as usize);
                write_volatile(desc2, VqDescriptor { addr: (buffer.phys() + 512) as u64, len: 1, flags: 2, next: 0xFFFF });

                let avail_idx_ptr = vq.available.idx;
                let idx = read_volatile(avail_idx_ptr);
                let slot = (idx as usize) % (vq.queue_size as usize);
                let ring_slot_ptr = vq.available.ring.add(slot);
                write_volatile(ring_slot_ptr, d0);
                let request = BlockRequest::new(d0, d1, d2, buffer.phys(), dma_buffer.clone());
                store_request(&vq, &request);

                // if the device supports virtio_ring_f_event_idx, update used_event so the device
                // will generate an interrupt for the next completion. writing vq.last_seen_used
                // causes the device's vring_need_event check to evaluate true for any new used.idx.
                let used_event_ptr = vq.available._used_event;
                if !used_event_ptr.is_null() {
                    write_volatile(used_event_ptr, vq.last_seen_used);
                }

                let doorbell_offset = vq.queue_notify_off as usize * drv.notify_off_multiplier as usize;
                let doorbell_ptr = (drv.notify_base as usize + doorbell_offset) as *mut u16;
                publish_available(&vq, idx, doorbell_ptr, |_| {});

                request
            };

            core::mem::forget(buffer);

            Ok(BlockTransferFuture {
                request,
                completion: if is_write { None } else { Some(BlockCompletion::CopyToPhys { dst_phys: buf_phys as usize }) },
            })
        }
    }
}

enum BlockCompletion {
    CopyToPhys { dst_phys: usize },
}

pub struct BlockTransferFuture {
    request: Arc<BlockRequest>,
    completion: Option<BlockCompletion>,
}

unsafe impl Send for BlockTransferFuture {}

impl Future for BlockTransferFuture {
    type Output = Result<(), ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match poll_block_request(&self.request, cx, || {}) {
            Poll::Ready(Ok(())) => {
                if let Some(BlockCompletion::CopyToPhys { dst_phys }) = self.completion.take() {
                    self.request.dma_buffer.copy_to_phys(dst_phys);
                }
                Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

fn poll_block_request(request: &Arc<BlockRequest>, cx: &mut Context<'_>, after_register: impl FnOnce()) -> Poll<Result<(), ()>> {
    let result = request.result.load(Ordering::Acquire);
    let completed = request.completed.load(Ordering::Acquire);
    debug_assert!(!completed || result != RESULT_PENDING, "completed block request remained pending");

    match result {
        RESULT_OK => Poll::Ready(Ok(())),
        RESULT_ERROR => Poll::Ready(Err(())),
        RESULT_PENDING => {
            request.waiter.register(cx.waker());
            after_register();

            match request.result.load(Ordering::Acquire) {
                RESULT_OK => Poll::Ready(Ok(())),
                RESULT_ERROR => Poll::Ready(Err(())),
                RESULT_PENDING => Poll::Pending,
                _ => panic!("invalid block request result"),
            }
        }
        _ => panic!("invalid block request result"),
    }
}

impl Drop for BlockTransferFuture {
    fn drop(&mut self) { self.request.waiter.deactivate(); }
}

/// irq top half
pub extern "C" fn virtio_blk_irq_handler(arg: usize) {
    let vq_state = arg as *const VirtqueueState;
    unsafe {
        // wake worker thread if set
        let tcb = (*vq_state).worker_tcb.load(Ordering::Acquire);
        if !tcb.is_null() {
            (*vq_state).awoken.store(true, Ordering::Release);
            wake_thread(tcb);
            return;
        }

        // wake executor thread if worker not reg'd yet
        let exec = EXECUTOR_THREAD_PTR.load(Ordering::Acquire);
        if !exec.is_null() {
            wake_thread(exec);
        }
    }
}

pub extern "C" fn virtio_blk_worker_thread(arg: usize) -> ! {
    let vq_state = arg as *mut VirtqueueState;
    unsafe {
        let mut last_seen = {
            let vq = (*vq_state).vq.lock();
            vq.last_seen_used
        };

        loop {
            let (current_used, queue_size, used_ring_ptr) = {
                let vq = (*vq_state).vq.lock();
                (read_volatile(vq.used.idx), vq.queue_size as usize, vq.used.ring)
            };

            if current_used != last_seen {
                while last_seen != current_used {
                    let slot = (last_seen as usize) % (queue_size as usize);
                    let elem_ptr = used_ring_ptr.add(slot);
                    let desc_id = read_volatile(addr_of!((*elem_ptr).id)) as usize;

                    let request = {
                        let vq = (*vq_state).vq.lock();
                        take_request(&vq, desc_id)
                    };

                    if let Some(request) = request {
                        let status_ptr = (request.page_phys + 512 + *DIRECT_MAP_OFFSET) as *const u8;
                        let status = read_volatile(status_ptr);

                        let mut vq = (*vq_state).vq.lock();
                        complete_request(&mut vq, &request, status);
                    }

                    last_seen = last_seen.wrapping_add(1);
                }

                // persist last_seen back to virtqueue so new transfers start from correct base
                {
                    let mut vq = (*vq_state).vq.lock();
                    vq.last_seen_used = last_seen;
                }

                continue; // re-loop to check for more completions
            }

            if (*vq_state).has_interrupts.load(Ordering::Acquire) {
                let int_state = interrupts_enabled();
                disable_interrupts();

                let recheck = {
                    let vq = (*vq_state).vq.lock();
                    read_volatile(vq.used.idx) != last_seen
                };

                if !recheck {
                    let current_thread = current_core_mut().scheduler.get_current_thread();
                    (*current_thread).transition(ThreadState::Running, ThreadState::Blocked).expect("virtio worker was not running");

                    if !cancel_block_if_awoken(&*current_thread, &(*vq_state).awoken) {
                        current_core_mut().scheduler.schedule(crate::sched::scheduler::ScheduleReason::Blocked);
                    }
                }

                (*vq_state).awoken.store(false, Ordering::Release);

                if int_state {
                    enable_interrupts();
                }
            } else {
                time::sleep(1_000_000);
            }
        }
    }
}

#[path = "blk_tests.rs"]
mod tests;

pub(crate) fn run_diagnostic_tests() { tests::run(); }
