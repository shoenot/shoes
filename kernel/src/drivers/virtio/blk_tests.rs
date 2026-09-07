use alloc::sync::Arc;
use core::pin::Pin;
use core::ptr::write_bytes;
use core::sync::atomic::{
    AtomicBool,
    AtomicUsize,
    Ordering,
};
use core::task::{
    Context,
    Poll,
};

use super::{
    BlockCompletion,
    BlockRequest,
    BlockTransferFuture,
    RESULT_OK,
    Virtqueue,
    VqAvailableRing,
    VqDescriptor,
    VqUsedElem,
    VqUsedRing,
    alloc_request_descs,
    complete_request,
    poll_block_request,
    publish_available,
    store_request,
    take_request,
};
use crate::memory::{
    ALLOCATOR,
    PageSize,
    DIRECT_MAP_OFFSET,
};
use crate::storage::blockdev::DmaBuffer;

struct CountingWaker {
    wakes: AtomicUsize,
}

impl alloc::task::Wake for CountingWaker {
    fn wake(self: Arc<Self>) { self.wake_by_ref(); }

    fn wake_by_ref(self: &Arc<Self>) { self.wakes.fetch_add(1, Ordering::Relaxed); }
}

struct ResultObserver {
    request: Arc<BlockRequest>,
    saw_ready: AtomicBool,
    wakes: AtomicUsize,
}

impl alloc::task::Wake for ResultObserver {
    fn wake(self: Arc<Self>) { self.wake_by_ref(); }

    fn wake_by_ref(self: &Arc<Self>) {
        self.saw_ready.store(self.request.result.load(Ordering::Acquire) == RESULT_OK, Ordering::Release);
        self.wakes.fetch_add(1, Ordering::Relaxed);
    }
}

fn counting_context() -> (Arc<CountingWaker>, core::task::Waker) {
    let counter = Arc::new(CountingWaker { wakes: AtomicUsize::new(0) });
    let waker = counter.clone().into();
    (counter, waker)
}

fn observer_context(request: Arc<BlockRequest>) -> (Arc<ResultObserver>, core::task::Waker) {
    let observer = Arc::new(ResultObserver { request, saw_ready: AtomicBool::new(false), wakes: AtomicUsize::new(0) });
    let waker = observer.clone().into();
    (observer, waker)
}

fn poll_future(future: &mut BlockTransferFuture, context: &mut Context<'_>) -> Poll<Result<(), ()>> { Pin::new(future).poll(context) }

fn alloc_page() -> usize {
    let page = ALLOCATOR.alloc(PageSize::Size4K).unwrap();
    assert_ne!(page, 0, "failed to allocate test page");
    page
}

fn set_status(page_phys: usize, status: u8) {
    unsafe {
        *((page_phys + 512 + *DIRECT_MAP_OFFSET) as *mut u8) = status;
    }
}

fn mark_request_complete(request: &Arc<BlockRequest>, ok: bool) {
    request.result.store(if ok { RESULT_OK } else { super::RESULT_ERROR }, Ordering::Release);
    request.completed.store(true, Ordering::Release);
}

fn drain_descs(vq: &mut Virtqueue, count: usize) -> alloc::vec::Vec<u16> {
    let mut descs = alloc::vec::Vec::new();
    for _ in 0..count {
        descs.push(vq.alloc_desc().expect("descriptor missing") as u16);
    }
    descs
}

fn test_virtqueue(queue_size: u16, free_descs: u16) -> Virtqueue {
    let desc_phys = ALLOCATOR.alloc_order(0).expect("desc alloc failed");
    let av_phys = ALLOCATOR.alloc_order(0).expect("avail alloc failed");
    let used_phys = ALLOCATOR.alloc_order(0).expect("used alloc failed");

    unsafe {
        write_bytes((desc_phys + *DIRECT_MAP_OFFSET) as *mut u8, 0, 4096);
        write_bytes((av_phys + *DIRECT_MAP_OFFSET) as *mut u8, 0, 4096);
        write_bytes((used_phys + *DIRECT_MAP_OFFSET) as *mut u8, 0, 4096);
    }

    let desc = (desc_phys + *DIRECT_MAP_OFFSET) as *mut VqDescriptor;
    for i in 0..queue_size {
        let next = if i + 1 < free_descs { i + 1 } else { 0xFFFF };
        unsafe {
            *desc.add(i as usize) = VqDescriptor { addr: 0, len: 0, flags: 0, next };
        }
    }

    let mut requests = alloc::vec::Vec::new();
    requests.resize(queue_size as usize, None);

    Virtqueue {
        desc,
        available: VqAvailableRing {
            flags: (av_phys + *DIRECT_MAP_OFFSET) as *mut u16,
            idx: (av_phys + *DIRECT_MAP_OFFSET + 2) as *mut u16,
            ring: (av_phys + *DIRECT_MAP_OFFSET + 4) as *mut u16,
            _used_event: (av_phys + *DIRECT_MAP_OFFSET + 6) as *mut u16,
        },
        used: VqUsedRing {
            flags: (used_phys + *DIRECT_MAP_OFFSET) as *mut u16,
            idx: (used_phys + *DIRECT_MAP_OFFSET + 2) as *mut u16,
            ring: (used_phys + *DIRECT_MAP_OFFSET + 4) as *mut VqUsedElem,
            _avail_event: (used_phys + *DIRECT_MAP_OFFSET + 8) as *mut u16,
        },
        desc_phys,
        av_phys,
        used_phys,
        desc_order: 0,
        av_order: 0,
        used_order: 0,
        queue_size,
        free_head: if free_descs == 0 { 0xFFFF } else { 0 },
        last_seen_used: 0,
        queue_notify_off: 0,
        requests: crate::sync::TicketLock::new(requests),
    }
}

fn queued_request(vq: &mut Virtqueue) -> Arc<BlockRequest> {
    let page_phys = alloc_page();
    let (d0, d1, d2) = alloc_request_descs(vq, page_phys).expect("descriptor allocation failed");
    let request = BlockRequest::new(d0, d1, d2, page_phys, DmaBuffer::new(512).expect("dma buffer alloc failed"));
    store_request(vq, &request);
    request
}

fn test_dropped_future_leaves_request_queue_owned() {
    let mut vq = test_virtqueue(8, 8);
    let request = queued_request(&mut vq);
    let future = BlockTransferFuture { request: request.clone(), completion: None };
    drop(future);

    assert!(vq.requests.lock()[request.d0 as usize].is_some(), "queue lost ownership after future drop");

    let queued = take_request(&vq, request.d0 as usize).expect("request missing from queue");
    complete_request(&mut vq, &queued, 0);
}

fn test_completion_reclaims_descriptors_after_future_drop() {
    let mut vq = test_virtqueue(8, 8);
    let request = queued_request(&mut vq);
    drop(BlockTransferFuture { request: request.clone(), completion: None });

    let queued = take_request(&vq, request.d0 as usize).expect("request missing from queue");
    complete_request(&mut vq, &queued, 0);

    assert_eq!(drain_descs(&mut vq, 3), alloc::vec![request.d2, request.d1, request.d0]);
}

fn test_dropped_future_is_not_woken() {
    let mut vq = test_virtqueue(8, 8);
    let request = queued_request(&mut vq);
    let (counter, waker) = counting_context();
    let mut future = BlockTransferFuture { request: request.clone(), completion: None };
    assert!(poll_future(&mut future, &mut Context::from_waker(&waker)).is_pending());
    drop(future);

    let queued = take_request(&vq, request.d0 as usize).expect("request missing from queue");
    complete_request(&mut vq, &queued, 0);
    assert_eq!(counter.wakes.load(Ordering::Acquire), 0);
}

fn test_completion_before_registration_returns_ready() {
    let page_phys = alloc_page();
    let request = BlockRequest::new(0, 1, 2, page_phys, DmaBuffer::new(512).expect("dma buffer alloc failed"));
    mark_request_complete(&request, true);
    let (_, waker) = counting_context();
    let mut future = BlockTransferFuture { request: request.clone(), completion: None };
    assert_eq!(poll_future(&mut future, &mut Context::from_waker(&waker)), Poll::Ready(Ok(())));
    ALLOCATOR.free(page_phys, PageSize::Size4K);
}

fn test_completion_between_check_and_registration_returns_ready() {
    let page_phys = alloc_page();
    let request = BlockRequest::new(0, 1, 2, page_phys, DmaBuffer::new(512).expect("dma buffer alloc failed"));
    let (_, waker) = counting_context();
    let result = poll_block_request(&request, &mut Context::from_waker(&waker), || {
        mark_request_complete(&request, true);
    });

    assert_eq!(result, Poll::Ready(Ok(())));
    ALLOCATOR.free(page_phys, PageSize::Size4K);
}

fn test_request_status_is_published_before_waking() {
    let mut vq = test_virtqueue(8, 8);
    let request = queued_request(&mut vq);
    let (observer, waker) = observer_context(request.clone());
    request.waiter.register(&waker);

    let queued = take_request(&vq, request.d0 as usize).expect("request missing from queue");
    complete_request(&mut vq, &queued, 0);

    assert!(observer.saw_ready.load(Ordering::Acquire), "waker observed stale request status");
    assert_eq!(observer.wakes.load(Ordering::Acquire), 1);
}

fn test_request_exists_before_submission_publication() {
    let mut vq = test_virtqueue(8, 8);
    let page_phys = alloc_page();
    let (d0, d1, d2) = alloc_request_descs(&mut vq, page_phys).expect("descriptor allocation failed");
    let request = BlockRequest::new(d0, d1, d2, page_phys, DmaBuffer::new(512).expect("dma buffer alloc failed"));
    store_request(&vq, &request);

    unsafe {
        *vq.available.idx = 0;
    }
    publish_available(&vq, 0, core::ptr::null_mut(), |vq| {
        assert!(vq.requests.lock()[d0 as usize].is_some(), "request was not published before avail.idx");
    });

    let queued = take_request(&vq, d0 as usize).expect("request missing from queue");
    complete_request(&mut vq, &queued, 0);
}

fn test_descriptor_allocation_failure_cleans_up_resources() {
    let mut vq = test_virtqueue(2, 2);
    let page_phys = alloc_page();

    assert!(alloc_request_descs(&mut vq, page_phys).is_err(), "descriptor allocation unexpectedly succeeded");
    assert_eq!(drain_descs(&mut vq, 2), alloc::vec![0, 1], "descriptor cleanup did not restore the freelist");

    let recycled = ALLOCATOR.alloc(PageSize::Size4K).unwrap();
    assert_eq!(recycled, page_phys, "request page was not returned to the local allocator");
    ALLOCATOR.free(recycled, PageSize::Size4K);
}

fn test_completion_happens_exactly_once() {
    let mut vq = test_virtqueue(8, 8);
    let request = queued_request(&mut vq);
    let (_, waker) = counting_context();
    request.waiter.register(&waker);

    let queued = take_request(&vq, request.d0 as usize).expect("request missing from queue");
    assert!(take_request(&vq, request.d0 as usize).is_none(), "request was extracted twice");
    complete_request(&mut vq, &queued, 0);

    assert_eq!(request.result.load(Ordering::Acquire), RESULT_OK);
    assert!(request.completed.load(Ordering::Acquire));
}

fn test_read_completion_copies_dma_back_to_destination() {
    let page_phys = alloc_page();
    let dma = DmaBuffer::new(512).expect("dma buffer alloc failed");
    let src = [0x5Au8; 512];
    unsafe {
        core::ptr::copy_nonoverlapping(src.as_ptr(), (dma.phys() + *DIRECT_MAP_OFFSET) as *mut u8, src.len());
    }

    let request = BlockRequest::new(0, 1, 2, page_phys, dma);
    mark_request_complete(&request, true);
    let (_, waker) = counting_context();
    let mut future =
        BlockTransferFuture { request: request.clone(), completion: Some(BlockCompletion::CopyToPhys { dst_phys: page_phys }) };

    assert_eq!(poll_future(&mut future, &mut Context::from_waker(&waker)), Poll::Ready(Ok(())));
    unsafe {
        let dst = core::slice::from_raw_parts((page_phys + *DIRECT_MAP_OFFSET) as *const u8, src.len());
        assert_eq!(dst, src);
    }
    ALLOCATOR.free(page_phys, PageSize::Size4K);
}

pub(super) fn run() {
    crate::klogln!("[TEST] virtio blk dropped future keeps request");
    test_dropped_future_leaves_request_queue_owned();
    crate::klogln!("[TEST] virtio blk reclaim after drop");
    test_completion_reclaims_descriptors_after_future_drop();
    crate::klogln!("[TEST] virtio blk dropped future not woken");
    test_dropped_future_is_not_woken();
    crate::klogln!("[TEST] virtio blk completion before registration");
    test_completion_before_registration_returns_ready();
    crate::klogln!("[TEST] virtio blk completion during registration");
    test_completion_between_check_and_registration_returns_ready();
    crate::klogln!("[TEST] virtio blk status before wake");
    test_request_status_is_published_before_waking();
    crate::klogln!("[TEST] virtio blk request before publish");
    test_request_exists_before_submission_publication();
    crate::klogln!("[TEST] virtio blk allocation cleanup");
    test_descriptor_allocation_failure_cleans_up_resources();
    crate::klogln!("[TEST] virtio blk single completion");
    test_completion_happens_exactly_once();
    crate::klogln!("[TEST] virtio blk read copy-back");
    test_read_completion_copies_dma_back_to_destination();
}
