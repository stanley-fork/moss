use crate::drivers::timer::{Instant, now};
#[cfg(feature = "smp")]
use crate::interrupts::cpu_messenger::{Message, message_cpu};
use crate::kernel::cpu_id::CpuId;
use crate::process::owned::OwnedTask;
use crate::{per_cpu_private, per_cpu_shared, process::TASK_LIST};
use alloc::{boxed::Box, sync::Arc};
use core::fmt::Debug;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use core::task::Waker;
use core::time::Duration;
use current::{CUR_TASK_PTR, current_task};
use libkernel::error::Result;
use log::warn;
use runqueue::RunQueue;
use sched_task::Work;
use waker::create_waker;

pub mod current;
mod runqueue;
pub mod sched_task;
pub mod uspc_ret;
pub mod waker;

pub static NUM_CONTEXT_SWITCHES: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Default)]
pub struct CpuStat<T>
where
    T: Debug + Default,
{
    pub user: T,
    pub nice: T,
    pub system: T,
    pub idle: T,
    pub iowait: T,
    pub irq: T,
    pub softirq: T,
    pub steal: T,
    pub guest: T,
    pub guest_nice: T,
}

impl CpuStat<AtomicUsize> {
    pub fn to_usize(&self) -> CpuStat<usize> {
        CpuStat {
            user: self.user.load(Ordering::Relaxed),
            nice: self.nice.load(Ordering::Relaxed),
            system: self.system.load(Ordering::Relaxed),
            idle: self.idle.load(Ordering::Relaxed),
            iowait: self.iowait.load(Ordering::Relaxed),
            irq: self.irq.load(Ordering::Relaxed),
            softirq: self.softirq.load(Ordering::Relaxed),
            steal: self.steal.load(Ordering::Relaxed),
            guest: self.guest.load(Ordering::Relaxed),
            guest_nice: self.guest_nice.load(Ordering::Relaxed),
        }
    }
}

per_cpu_shared! {
    pub static CPU_STAT: CpuStat<AtomicUsize> = CpuStat::default;
}

pub fn get_cpu_stat(cpu_id: CpuId) -> CpuStat<usize> {
    CPU_STAT.get_by_cpu(cpu_id.value()).to_usize()
}

per_cpu_private! {
    static SCHED_STATE: SchedState = SchedState::new;
}

/// Default time-slice assigned to runnable tasks.
const DEFAULT_TIME_SLICE: Duration = Duration::from_millis(4);

/// Fixed-point configuration for virtual-time accounting.
/// We now use a 65.63 format (65 integer bits, 63 fractional bits) as
/// recommended by the EEVDF paper to minimise rounding error accumulation.
pub const VT_FIXED_SHIFT: u32 = 63;
pub const VT_ONE: u128 = 1u128 << VT_FIXED_SHIFT;
/// Tolerance used when comparing virtual-time values (see EEVDF, Fixed-Point Arithmetic).
/// Two virtual-time instants whose integer parts differ by no more than this constant are considered equal.
pub const VCLOCK_EPSILON: u128 = VT_ONE;

/// Scheduler base weight to ensure tasks always have a strictly positive
/// scheduling weight. The value is added to a task's priority to obtain its
/// effective weight (`w_i` in EEVDF paper).
pub const SCHED_WEIGHT_BASE: i32 = 1024;

/// Schedule a new task.
///
/// This function is the core of the kernel's scheduler. It is responsible for
/// deciding which process to run next.
///
/// # Logic:
/// 1. Finds the highest-priority, `Runnable` process in the system.
/// 2. The idle task (PID 0, lowest priority) serves as a fallback if no other
///    process is runnable.
/// 3. If the selected process is the same as the currently running one, no
///    switch occurs.
/// 4. If a new process is selected, it handles the state transitions (`Running`
///   > `Runnable` for the old task, `Runnable` > `Running` for the new task)
///   > and performs the architecture-specific context switch.
///
/// # Returns
///
/// Nothing, but the CPU context will be set to the next runnable task. See
/// `userspace_return` for how this is invoked.
fn schedule() {
    // Reentrancy Check
    if SCHED_STATE.try_borrow_mut().is_none() {
        warn!(
            "Scheduler reentrancy detected on CPU {}",
            CpuId::this().value()
        );
        return;
    }

    SCHED_STATE.borrow_mut().do_schedule();
}

pub fn spawn_kernel_work(fut: impl Future<Output = ()> + 'static + Send) {
    current_task().ctx.put_kernel_work(Box::pin(fut));
}

/// Global atomic storing info about the least-tasked CPU.
/// First 16 bits: CPU ID
/// Remaining 48 bits: Run-queue weight
#[cfg(feature = "smp")]
static LEAST_TASKED_CPU_INFO: AtomicU64 = AtomicU64::new(0);
const WEIGHT_SHIFT: u32 = 16;

#[cfg(feature = "smp")]
fn get_best_cpu() -> CpuId {
    // Get the CPU with the least number of tasks.
    let least_tasked_cpu_info = LEAST_TASKED_CPU_INFO.load(Ordering::Acquire);
    CpuId::from_value((least_tasked_cpu_info & 0xffff) as usize)
}

/// Insert the given task onto a CPU's run queue.
pub fn insert_work(work: Arc<Work>) {
    SCHED_STATE.borrow_mut().run_q.add_work(work);
}

#[cfg(feature = "smp")]
pub fn insert_task_cross_cpu(task: Arc<Work>) {
    let cpu = get_best_cpu();
    if cpu == CpuId::this() {
        SCHED_STATE.borrow_mut().run_q.add_work(task);
    } else {
        message_cpu(cpu, Message::EnqueueWork(task)).expect("Failed to send task to CPU");
    }
}

#[cfg(not(feature = "smp"))]
pub fn insert_task_cross_cpu(task: Arc<Work>) {
    insert_work(task);
}

pub struct SchedState {
    run_q: RunQueue,
}

unsafe impl Send for SchedState {}

impl SchedState {
    pub fn new() -> Self {
        Self {
            run_q: RunQueue::new(),
        }
    }

    /// Update the global least-tasked CPU info atomically.
    #[cfg(feature = "smp")]
    fn update_global_least_tasked_cpu_info(&self) {
        fn none<T>() -> Option<T> {
            None
        }

        per_cpu_private! {
            static LAST_UPDATE: Option<Instant> = none;
        }

        // Try and throttle contention on the atomic variable.
        const MIN_COOLDOWN: Duration = Duration::from_millis(16);
        if let Some(last) = LAST_UPDATE.borrow().as_ref()
            && let Some(now) = now()
            && now - *last < MIN_COOLDOWN
        {
            return;
        }
        *LAST_UPDATE.borrow_mut() = now();

        let weight = self.run_q.weight();
        let cpu_id = CpuId::this().value() as u64;
        let new_info = (cpu_id & 0xffff) | ((weight & 0xffffffffffff) << WEIGHT_SHIFT);
        let mut old_info = LEAST_TASKED_CPU_INFO.load(Ordering::Acquire);
        // Ensure we don't spin forever (possible with a larger number of CPUs)
        const MAX_RETRIES: usize = 8;
        // Ensure consistency
        for _ in 0..MAX_RETRIES {
            let old_cpu_id = old_info & 0xffff;
            let old_weight = old_info >> WEIGHT_SHIFT;
            if (cpu_id == old_cpu_id && old_info != new_info) || (weight < old_weight) {
                match LEAST_TASKED_CPU_INFO.compare_exchange(
                    old_info,
                    new_info,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(x) => old_info = x,
                }
            }
        }
    }

    #[cfg(not(feature = "smp"))]
    fn update_global_least_tasked_cpu_info(&self) {
        // No-op on single-core systems.
    }

    pub fn do_schedule(&mut self) {
        self.update_global_least_tasked_cpu_info();

        let now_inst = now().expect("System timer not initialised");

        {
            let current = self.run_q.current_mut();

            current.task.update_accounting(Some(now_inst));

            // Reset accounting baseline after updating stats to avoid double-counting
            // the same time interval on the next scheduler tick.
            current.task.reset_last_account(now_inst);
        }

        self.run_q.schedule(now_inst);
    }
}

pub fn sched_init() {
    let init_task = OwnedTask::create_init_task();

    let init_work = Work::new(Box::new(init_task));

    {
        let mut task_list = TASK_LIST.lock_save_irq();

        task_list.insert(init_work.task.descriptor(), Arc::downgrade(&init_work));
    }

    insert_work(init_work);

    schedule();
}

pub fn sched_init_secondary() {
    // Force update_global_least_tasked_cpu_info
    SCHED_STATE.borrow().update_global_least_tasked_cpu_info();

    schedule();
}

pub fn sys_sched_yield() -> Result<usize> {
    schedule();
    Ok(0)
}

pub fn current_work() -> Arc<Work> {
    SCHED_STATE.borrow().run_q.current().task.clone()
}

pub fn current_work_waker() -> Waker {
    create_waker(current_work())
}
