use crate::{
    drivers::timer::Instant,
    sched::{VCLOCK_EPSILON, VT_FIXED_SHIFT, sched_task::RunnableTask},
};

pub struct VClock {
    last_update: Option<Instant>,
    clk: u128,
}

impl VClock {
    pub fn new() -> Self {
        Self {
            last_update: None,
            clk: 0,
        }
    }

    pub fn is_task_eligible(&self, tsk: &RunnableTask) -> bool {
        tsk.v_eligible.saturating_sub(self.clk) <= VCLOCK_EPSILON
    }

    /// Fast forward the clk to the specified clock, `new_clk`.
    pub fn fast_forward(&mut self, new_clk: u128) {
        self.clk = new_clk;
    }

    /// Advance the virtual clock (`v_clock`) by converting the elapsed real time
    /// since the last update into 65.63-format fixed-point virtual-time units:
    /// v += (delta t << VT_FIXED_SHIFT) / sum w The caller must pass the
    /// current real time (`now_inst`).
    pub fn advance(&mut self, now_inst: Instant, weight: u64) {
        if let Some(prev) = self.last_update {
            let delta_real = now_inst - prev;

            if weight > 0 {
                let delta_vt = ((delta_real.as_nanos()) << VT_FIXED_SHIFT) / weight as u128;
                self.clk = self.clk.saturating_add(delta_vt);
            }
        }
        self.last_update = Some(now_inst);
    }
}
