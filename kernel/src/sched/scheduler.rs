#![allow(dead_code)]

use alloc::sync::Arc;
use core::mem::size_of;
use core::ptr::{
    null_mut,
    write_bytes,
};
use core::sync::atomic::{
    AtomicUsize,
    Ordering,
};

use hal::cpu::{halt_loop, set_kernel_stack, set_user_thread_pointer};
use hal::interrupts::disable_interrupts;
use hal::ipi::send_reschedule_ipi;

use crate::cpu::{
    NO_STEAL_REQUEST, NUM_CORES, current_core_mut, get_core_data_for, hal_boot_alloc
};
use crate::sync::TicketLock;
use crate::sched::idle::*;
use crate::sched::priority::ThreadPriority;
use crate::sched::{
    Thread,
    ThreadState,
};
use crate::time::{
    get_time,
    ns_to_ticks,
    update_hardware_timer,
};
use hal::context::{
    allocate_bootstrap_extended_context, switch_from_bootstrap, switch_threads
};
use hal::mmu::load_cr3;
use crate::{
    BOOTSTRAP_ALLOC,
    KERNEL_PROCESS,
    impl_queue_methods,
};

pub static GLOBAL_TID: AtomicUsize = AtomicUsize::new(0);

pub const RFLAGS_IF: u64 = 0x202; // bit 9 is interrupt enable and bit 1 is always 1 (reserved)

pub const DEFAULT_QUANTUM: usize = 10_000_000;

pub static GRAVEYARD: TicketLock<TCBQueue> =
    TicketLock::new(TCBQueue { queue_length: AtomicUsize::new(0), head: null_mut(), tail: null_mut() });

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ScheduleReason {
    /// Running thread consumed its complete quantum
    QuantumExpired,
    /// Running thread voluntarily yielded
    Yield,
    /// Running thread transitioned to blocked
    Blocked,
    /// Running thread transitioned to terminated
    Terminated,
    /// Timer event interrupt before quantum expired
    TimerEvent,
    /// Another CPU requested that this CPU reconsider scheduling
    RescheduleIpi,
}

pub(crate) fn account_running_thread(thread: &mut Thread, reason: ScheduleReason, now: usize) {
    if thread.last_started != 0 {
        thread.total_runtime = thread.total_runtime.saturating_add(now.saturating_sub(thread.last_started));
    }

    if reason == ScheduleReason::QuantumExpired {
        thread.effective_priority = thread.effective_priority.decay_toward(thread.base_priority);
    }
}

pub(crate) fn should_refresh_quantum(
    next_thread: *mut Thread, prev_thread: *mut Thread, reason: ScheduleReason, quantum_expiry: usize, now: usize,
) -> bool {
    next_thread != prev_thread || reason == ScheduleReason::QuantumExpired || quantum_expiry <= now
}

pub struct TCBQueue {
    pub queue_length: AtomicUsize,
    head: *mut Thread,
    tail: *mut Thread,
}

unsafe impl Send for TCBQueue {}

impl_queue_methods!(TCBQueue, Thread, head, tail);

pub struct SchedulerState {
    pub core_logical_id: usize,

    pub queue_length: AtomicUsize,
    pub ready_queue_heads: [*mut Thread; 32],
    pub ready_queue_tails: [*mut Thread; 32],
    pub active_priorities: u32,

    pub sleep_queue_head: *mut Thread,
    pub mailbox: TicketLock<TCBQueue>,
    pub idle_thread: *mut Thread,

    pub current_thread: *mut Thread,
}

unsafe impl Send for SchedulerState {}
unsafe impl Sync for SchedulerState {}

pub fn get_new_tid() -> usize { GLOBAL_TID.fetch_add(1, Ordering::Relaxed) }

impl SchedulerState {
    pub const fn new() -> Self {
        SchedulerState {
            core_logical_id: 0,

            queue_length: AtomicUsize::new(0),
            ready_queue_heads: [null_mut(); 32],
            ready_queue_tails: [null_mut(); 32],
            active_priorities: 0,

            sleep_queue_head: null_mut(),
            mailbox: TicketLock::new(TCBQueue { queue_length: AtomicUsize::new(0), head: null_mut(), tail: null_mut() }),
            idle_thread: null_mut(),

            current_thread: null_mut(),
        }
    }

    pub fn init_basic(&mut self, logical_id: usize) { self.core_logical_id = logical_id; }

    pub fn init_threads(&mut self, logical_id: usize) {
        self.idle_thread = init_idle_thread(logical_id);

        let tcb_ptr = BOOTSTRAP_ALLOC.lock().alloc(size_of::<Thread>(), 8) as *mut Thread;

        unsafe { write_bytes(tcb_ptr as *mut u8, 0, size_of::<Thread>()) };

        let fpu_ptr = allocate_bootstrap_extended_context(hal_boot_alloc);
        let kernel_proc = KERNEL_PROCESS.get().expect("[FATAL] Kernel process was not initialized before scheduler threads").clone();

        unsafe {
            (*tcb_ptr).init(0, 0, 0, fpu_ptr, logical_id, ThreadPriority::MAXIMUM, kernel_proc.clone());
            (*tcb_ptr).set_state(ThreadState::Running);
        }

        kernel_proc.register_thread(tcb_ptr);
        kernel_proc.active_threads.fetch_add(1, Ordering::SeqCst);

        self.current_thread = tcb_ptr;
    }

    pub fn schedule(&mut self, reason: ScheduleReason) {
        disable_interrupts();

        self.process_steal_request();

        let now = get_time();
        let prev_thread = self.current_thread;
        let mut prev_retired = false;

        if !prev_thread.is_null() {
            unsafe {
                account_running_thread(&mut *prev_thread, reason, now);

                if (*prev_thread).state() == ThreadState::Running && thread_should_exit(prev_thread) {
                    prev_retired = retire_current_thread(prev_thread, 0)
                }
            }
        }

        loop {
            let item = { self.mailbox.lock().pop() };
            if item.is_null() {
                break;
            }
            self.push(item);
        }

        let mut next_thread = loop {
            let candidate = self.pop();
            if candidate.is_null() {
                break null_mut();
            }
            if thread_should_exit(candidate) {
                retire_queued_thread(candidate, 0);
                continue;
            }
            if unsafe { (*candidate).transition(ThreadState::Ready, ThreadState::Running) }.is_ok() {
                break candidate;
            }
        };

        if next_thread.is_null() {
            if !prev_retired && !prev_thread.is_null() && unsafe { (*prev_thread).state() == ThreadState::Running } {
                next_thread = prev_thread;
                if prev_thread == self.idle_thread {
                    self.request_stolen_work();
                }
            } else {
                self.request_stolen_work();
                next_thread = self.idle_thread;
                unsafe {
                    if (*next_thread).state() != ThreadState::Running {
                        let _ = (*next_thread).transition(ThreadState::Ready, ThreadState::Running);
                    }
                }
            }
        }

        if !prev_retired && !prev_thread.is_null() && prev_thread != next_thread {
            unsafe {
                if (*prev_thread).state() == ThreadState::Running {
                    if (*prev_thread).transition(ThreadState::Running, ThreadState::Ready).is_ok() && prev_thread != self.idle_thread {
                        self.push(prev_thread);
                    }
                }
            }
        }

        unsafe {
            let next = &mut *next_thread;
            next.last_started = now;

            if next.effective_priority == ThreadPriority::IDLE {
                next.quantum_expiry = usize::MAX;
            } else if should_refresh_quantum(next_thread, prev_thread, reason, next.quantum_expiry, now) {
                next.quantum_expiry = now + ns_to_ticks(DEFAULT_QUANTUM);
            }
        }

        self.current_thread = next_thread;

        let next_stack_top = unsafe { (*next_thread).stack_base + (*next_thread).stack_size };
        unsafe { set_kernel_stack(next_stack_top); }
        update_hardware_timer();

        if prev_retired {
            GRAVEYARD.lock().push(prev_thread);
        }

        if prev_thread == next_thread {
            return;
        }

        unsafe {
            let should_switch_addr_space = {
                if prev_thread.is_null() || prev_retired { true } else { !Arc::ptr_eq(&(*prev_thread).process, &(*next_thread).process) }
            };

            if should_switch_addr_space {
                load_cr3((&*next_thread).process.root_addr as u64);
            }

            let base_target = (*next_thread).fs_base;
            set_user_thread_pointer(base_target);
        }

        if !prev_thread.is_null() {
            unsafe {
                switch_threads(
                    &mut (*prev_thread).stack_ptr as *mut usize,
                    (*next_thread).stack_ptr,
                    (*prev_thread).extended_context,
                    (*next_thread).extended_context,
                );
            }
        } else {
            unsafe {
                switch_from_bootstrap(
                    (*next_thread).stack_ptr,
                    (*next_thread).extended_context,
                )
                .expect("failed to switch from bootstrap context");
            }
        }
    }

    pub fn push(&mut self, item: *mut Thread) {
        if item.is_null() {
            return;
        }

        let now = get_time();
        let priority = unsafe {
            (*item).ready_since = now;
            (*item).effective_priority.as_usize()
        };

        unsafe {
            (*item).next = null_mut();

            if self.ready_queue_tails[priority].is_null() {
                self.ready_queue_heads[priority] = item;
                self.ready_queue_tails[priority] = item;
            } else {
                (*self.ready_queue_tails[priority]).next = item;
                self.ready_queue_tails[priority] = item;
            }

            self.queue_length.fetch_add(1, Ordering::Relaxed);
        }

        self.active_priorities |= 1 << priority;
    }

    pub fn pop(&mut self) -> *mut Thread {
        if self.active_priorities == 0 {
            return null_mut();
        }

        let highest_priority = self.active_priorities.trailing_zeros() as usize;
        let ret = self.ready_queue_heads[highest_priority];

        unsafe {
            if ret.is_null() {
                return null_mut();
            }

            self.ready_queue_heads[highest_priority] = (*ret).next;

            if self.ready_queue_heads[highest_priority].is_null() {
                self.ready_queue_tails[highest_priority] = null_mut();
            }

            if self.ready_queue_heads[highest_priority].is_null() {
                self.active_priorities &= !(1 << highest_priority);
            }

            (*ret).next = null_mut();
            self.queue_length.fetch_sub(1, Ordering::Relaxed);
            ret
        }
    }

    pub fn get_current_thread(&self) -> *mut Thread { self.current_thread }

    pub fn terminate_current_thread(&mut self, exit_code: u32) -> ! {
        disable_interrupts();

        let thread = self.current_thread;
        if !thread.is_null() {
            retire_current_thread(thread, exit_code);
        }

        self.schedule(ScheduleReason::Terminated);
        halt_loop();
    }

    pub fn terminate(&mut self, exit_code: u32) -> ! { self.terminate_current_thread(exit_code); }

    pub(crate) fn pop_lowest_priority_migratable(&mut self) -> *mut Thread {
        for priority in (0..32).rev() {
            let mut previous: *mut Thread = null_mut();
            let mut current = self.ready_queue_heads[priority];

            while !current.is_null() {
                unsafe {
                    let next = (*current).next;

                    if (*current).is_migratable() {
                        if previous.is_null() {
                            self.ready_queue_heads[priority] = next;
                        } else {
                            (*previous).next = next;
                        }

                        if self.ready_queue_tails[priority] == current {
                            self.ready_queue_tails[priority] = previous;
                        }

                        if self.ready_queue_heads[priority].is_null() {
                            self.active_priorities &= !(1 << priority);
                        }

                        (*current).next = null_mut();
                        self.queue_length.fetch_sub(1, Ordering::Relaxed);
                        return current;
                    }

                    previous = current;
                    current = next;
                }
            }
        }

        null_mut()
    }

    fn process_steal_request(&mut self) {
        let requester = current_core_mut().steal_requester.swap(NO_STEAL_REQUEST, Ordering::AcqRel);

        if requester == NO_STEAL_REQUEST || requester == self.core_logical_id {
            return;
        }

        if self.queue_length.load(Ordering::Acquire) < 2 {
            return;
        }

        let donated = self.pop_lowest_priority_migratable();
        if donated.is_null() {
            return;
        }

        unsafe {
            (*donated).set_assigned_core(requester);
        }

        let target = get_core_data_for(requester);
        target.scheduler.mailbox.lock().push(donated);

        send_reschedule_ipi(requester);
    }

    fn request_stolen_work(&self) {
        let this_core = self.core_logical_id;
        let mut victim = None;
        let mut victim_load = 1;
        let Some(&num_cores) = NUM_CORES.get() else {
            return;
        };

        for logical_id in 0..num_cores {
            if logical_id == this_core {
                continue;
            }

            let candidate = get_core_data_for(logical_id);
            let load = candidate.scheduler.queue_length.load(Ordering::Acquire);

            if load > victim_load {
                victim = Some(candidate);
                victim_load = load;
            }
        }

        let Some(victim) = victim else {
            return;
        };

        if victim.steal_requester.compare_exchange(NO_STEAL_REQUEST, this_core, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            send_reschedule_ipi(victim.logical_id);
        }
    }
}

fn thread_should_exit(thread: *mut Thread) -> bool {
    if thread.is_null() {
        return false;
    }
    unsafe { (*thread).cancel_requested() || (*thread).process.is_terminating() }
}

fn retire_queued_thread(thread: *mut Thread, exit_code: u32) -> bool {
    if thread.is_null() {
        return false;
    }
    unsafe {
        if (*thread).state() == ThreadState::Terminated {
            return false;
        }

        let proc = (*thread).process.clone();

        if !proc.finish_thread_exit(thread, exit_code) {
            return false;
        }

        (*thread).set_state(ThreadState::Terminated);
        GRAVEYARD.lock().push(thread);

        true
    }
}

fn retire_current_thread(thread: *mut Thread, exit_code: u32) -> bool {
    if thread.is_null() {
        return false;
    }
    unsafe {
        if (*thread).state() == ThreadState::Terminated {
            return false;
        }

        let proc = (*thread).process.clone();

        if !proc.finish_thread_exit(thread, exit_code) {
            return false;
        }

        (*thread).set_state(ThreadState::Terminated);

        true
    }
}
