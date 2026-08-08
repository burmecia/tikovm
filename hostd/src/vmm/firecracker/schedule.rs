//! The cron scheduler for `VmMode::Schedule` VMs.
//!
//! A schedule VM spends its life `Suspended` (snapshot on disk, no
//! Firecracker process). A background loop ticks every few seconds, finds
//! VMs whose `cron_schedule` fired since the last tick, and for each fire
//! runs the VM's configured `cmd` as a regular workload:
//!
//!   wake (start or restore) -> run cmd -> snapshot (back to Suspended)
//!
//! Run history and logs need no dedicated mechanism: every run is a
//! `Workload` with `origin == Schedule`, queryable through the existing
//! workloads API (`GET /api/vms/{id}/workloads[/{wid}/logs]`).
//!
//! Semantics:
//! - Cron expressions are interpreted in UTC. Standard 5-field cron and
//!   6/7-field expressions with a seconds field are both accepted.
//! - Overlapping runs never happen: a fire arriving while the previous run
//!   is still active is skipped with a warning (per-VM try-lock).
//! - An optional `timeout_secs` bounds a run: on expiry the workload is
//!   stopped (SIGTERM, then SIGKILL in the guest) and the VM is suspended
//!   anyway.
//! - Missed fires while hostd was down are not caught up.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use cron::Schedule;
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::vmm::Vmm;
use crate::vmm::vm::{VmId, VmMode, VmState};
use crate::vmm::workload::{WorkloadOrigin, WorkloadSpec};

use super::vmm::FirecrackerVmm;

/// How often the scheduler scans VMs for due cron fires. Fires are matched
/// against the (previous tick, now] window, so this only bounds detection
/// latency, not which fires are seen.
const SCHEDULE_POLL: Duration = Duration::from_secs(5);

/// How often a running scheduled workload is polled for its terminal state.
const RUN_POLL: Duration = Duration::from_secs(2);

/// Parse a cron expression into a [`Schedule`]. The `cron` crate requires a
/// seconds field (6/7 fields); a standard 5-field expression gets a `0`
/// seconds field prepended, so it fires at the top of the minute.
pub(super) fn parse_cron_schedule(expr: &str) -> Result<Schedule> {
    let trimmed = expr.trim();
    let normalized = match trimmed.split_whitespace().count() {
        5 => format!("0 {trimmed}"),
        _ => trimmed.to_string(),
    };
    Schedule::from_str(&normalized)
        .map_err(|e| Error::vmm(format!("invalid cron_schedule {expr:?}: {e}")))
}

/// Per-VM async locks deduplicating scheduled runs (a fire arriving while a
/// run is active is skipped, never queued).
#[derive(Default)]
pub(super) struct ScheduleRunLocks(Mutex<HashMap<VmId, Arc<tokio::sync::Mutex<()>>>>);

impl ScheduleRunLocks {
    pub(super) fn lock_for(&self, vm_id: &VmId) -> Result<Arc<tokio::sync::Mutex<()>>> {
        Ok(self
            .0
            .lock()?
            .entry(vm_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone())
    }

    pub(super) fn remove(&self, vm_id: &VmId) -> Result<()> {
        self.0.lock()?.remove(vm_id);
        Ok(())
    }
}

impl FirecrackerVmm {
    /// Spawn the cron scheduler loop. Called once from
    /// `start_background_tasks`.
    pub(super) fn spawn_scheduler(self: &Arc<Self>) {
        let vmm = Arc::clone(self);
        tokio::spawn(async move {
            // No retroactive fires: the window starts at loop start, so VMs
            // created before hostd booted only fire on future occurrences.
            let mut last_tick = Utc::now();
            let mut interval = tokio::time::interval(SCHEDULE_POLL);
            loop {
                interval.tick().await;
                let now = Utc::now();
                vmm.fire_due_schedules(last_tick, now).await;
                last_tick = now;
            }
        });
    }

    /// One scheduler tick: run every schedule VM whose cron fired inside
    /// `(last_tick, now]`.
    async fn fire_due_schedules(self: &Arc<Self>, last_tick: DateTime<Utc>, now: DateTime<Utc>) {
        // Collect (vm_id, parsed schedule) under the lock, then drop it
        // before spawning runs.
        let due: Vec<VmId> = {
            let Ok(vms) = self.vms.lock() else { return };
            vms.iter()
                .filter_map(|(vm_id, entry)| {
                    let instance = entry.instance.lock().ok()?;
                    if instance.vm_config.mode != VmMode::Schedule {
                        return None;
                    }
                    let expr = instance.vm_config.cron_schedule.as_deref()?;
                    let schedule = match parse_cron_schedule(expr) {
                        Ok(s) => s,
                        Err(e) => {
                            // create_vm validates, so this only fires if the
                            // VM was created before validation existed.
                            warn!(vm_id = %vm_id, error = %e, "ignoring schedule VM with invalid cron_schedule");
                            return None;
                        }
                    };
                    // A fire is due if the first occurrence after the
                    // previous tick is not in the future.
                    match schedule.after(&last_tick).next() {
                        Some(fire) if fire <= now => Some(vm_id.clone()),
                        _ => None,
                    }
                })
                .collect()
        };

        for vm_id in due {
            if let Err(e) = self.spawn_scheduled_run(&vm_id) {
                warn!(vm_id = %vm_id, error = %e, "failed to launch scheduled run");
            }
        }
    }

    /// Launch one scheduled run as a background task, unless this VM already
    /// has one in flight (overlap = skip with a warning).
    fn spawn_scheduled_run(self: &Arc<Self>, vm_id: &VmId) -> Result<()> {
        let run_lock = self.schedule_run_locks.lock_for(vm_id)?;
        let Ok(guard) = run_lock.try_lock_owned() else {
            warn!(vm_id = %vm_id, "previous scheduled run still active; skipping this fire");
            return Ok(());
        };
        info!(vm_id = %vm_id, "cron fired; starting scheduled run");
        let vmm = Arc::clone(self);
        let vm_id = vm_id.clone();
        tokio::spawn(async move {
            // Held until the run (wake -> cmd -> suspend) is done.
            let _guard = guard;
            vmm.run_scheduled(&vm_id).await;
        });
        Ok(())
    }

    /// One scheduled run: wake the VM, run its `cmd` as a workload, then
    /// snapshot it back to `Suspended` whatever the outcome.
    async fn run_scheduled(&self, vm_id: &VmId) {
        if let Err(e) = self.run_scheduled_inner(vm_id).await {
            warn!(vm_id = %vm_id, error = %e, "scheduled run failed");
        }

        // Back to Suspended so the VM consumes no resources until the next
        // fire. Best-effort on failures too: a VM left Started would idle
        // forever (schedule VMs have no auto-suspend).
        let state = self.vm_state(vm_id);
        if state == Some(VmState::Started)
            && let Err(e) = self.snapshot_vm(vm_id).await
        {
            warn!(vm_id = %vm_id, error = %e, "failed to suspend VM after scheduled run");
        }
    }

    async fn run_scheduled_inner(&self, vm_id: &VmId) -> Result<()> {
        // 1. Wake the VM. `Started` is accepted as-is (manual start, or a
        // leftover from a previously failed snapshot).
        match self.vm_state(vm_id) {
            Some(VmState::Suspended) => self.ensure_started(vm_id).await?,
            Some(VmState::Created) => self.start_vm(vm_id).await?,
            Some(VmState::Started) => {}
            other => {
                return Err(Error::vmm(format!(
                    "vm {vm_id} is {other:?}; cannot run scheduled cmd"
                )));
            }
        }

        // 2. Run the configured cmd as a workload tagged with the schedule
        // origin (so run history can tell cron runs from manual exec).
        let (cmd, env, timeout_secs) = {
            let vms = self.vms.lock()?;
            let entry = vms
                .get(vm_id)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?;
            let config = &entry.instance.lock()?.vm_config;
            (config.cmd.clone(), config.env.clone(), config.timeout_secs)
        };
        let workload = self
            .start_workload(
                vm_id,
                WorkloadSpec {
                    cmd,
                    env,
                    cwd: None,
                },
            )
            .await?;
        {
            let mut vms = self.vms.lock()?;
            if let Some(entry) = vms.get_mut(vm_id)
                && let Some(wl) = entry.workloads.get_mut(&workload.workload_id)
            {
                wl.origin = WorkloadOrigin::Schedule;
            }
        }
        info!(vm_id = %vm_id, workload_id = %workload.workload_id, "scheduled cmd started");

        // 3. Wait for the terminal state, honoring the optional timeout.
        self.await_workload(vm_id, &workload.workload_id, timeout_secs)
            .await
    }

    /// Poll a workload until it reaches a terminal state. With
    /// `timeout_secs` set, a still-active workload at the deadline is stopped
    /// (SIGTERM -> SIGKILL in the guest) and awaited to its terminal state.
    async fn await_workload(
        &self,
        vm_id: &VmId,
        workload_id: &crate::vmm::workload::WorkloadId,
        timeout_secs: Option<u64>,
    ) -> Result<()> {
        let started = std::time::Instant::now();
        loop {
            let workload = self.get_workload(vm_id, workload_id).await?;
            if !workload.is_active() {
                debug!(
                    vm_id = %vm_id,
                    workload_id = %workload_id,
                    state = ?workload.state,
                    exit_code = ?workload.exit_code,
                    "scheduled cmd finished"
                );
                return Ok(());
            }
            if let Some(timeout) = timeout_secs
                && started.elapsed() >= Duration::from_secs(timeout)
            {
                warn!(
                    vm_id = %vm_id,
                    workload_id = %workload_id,
                    timeout_secs = timeout,
                    "scheduled cmd timed out; stopping it"
                );
                self.stop_workload(vm_id, workload_id).await?;
                // The exit event lands asynchronously; keep polling until
                // the workload settles into a terminal state.
                while self.get_workload(vm_id, workload_id).await?.is_active() {
                    tokio::time::sleep(RUN_POLL).await;
                }
                return Ok(());
            }
            tokio::time::sleep(RUN_POLL).await;
        }
    }

    /// Current state of a VM, or `None` if it no longer exists.
    fn vm_state(&self, vm_id: &VmId) -> Option<VmState> {
        let vms = self.vms.lock().ok()?;
        let entry = vms.get(vm_id)?;
        entry.instance.lock().ok().map(|i| i.state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn parse_five_field_cron() {
        // Standard cron gets a `0` seconds field: fires at minute start.
        let schedule = parse_cron_schedule("*/5 * * * *").unwrap();
        let after = Utc::now();
        let next = schedule.after(&after).next().unwrap();
        assert_eq!(next.second(), 0);
        assert_eq!(next.minute() % 5, 0);
    }

    #[test]
    fn parse_six_field_cron_with_seconds() {
        let schedule = parse_cron_schedule("*/15 * * * * *").unwrap();
        let after = Utc::now();
        let next = schedule.after(&after).next().unwrap();
        assert_eq!(next.second() % 15, 0);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_cron_schedule("not a cron").is_err());
        assert!(parse_cron_schedule("").is_err());
        // 61 is not a valid minute.
        assert!(parse_cron_schedule("0 61 * * * *").is_err());
    }
}
