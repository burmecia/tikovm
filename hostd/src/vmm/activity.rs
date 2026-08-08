//! Per-VM proxy activity tracking for auto-suspend.
//!
//! The proxy records every request it forwards to a VM here; the HTTP idle
//! detector (`FirecrackerVmm`'s background loop) reads `last_activity` to
//! decide when a VM has been quiet long enough to suspend, and the suspend
//! gate reads `in_flight` so a VM is never snapshotted mid-request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::vm::VmId;

#[derive(Debug, Default)]
struct Activity {
    in_flight: usize,
    last_activity: Option<Instant>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ActivityTracker {
    inner: Arc<Mutex<HashMap<VmId, Activity>>>,
}

impl ActivityTracker {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record the start of a proxied request to `vm_id`. The returned guard
    /// marks it finished on drop; holding it across `.await` is fine.
    pub(crate) fn track(&self, vm_id: &VmId) -> ActivityGuard {
        {
            let mut inner = self.inner.lock().unwrap();
            let activity = inner.entry(vm_id.clone()).or_default();
            activity.in_flight += 1;
            activity.last_activity = Some(Instant::now());
        }
        ActivityGuard {
            tracker: self.clone(),
            vm_id: vm_id.clone(),
        }
    }

    /// Requests currently being proxied to `vm_id`.
    pub(crate) fn in_flight(&self, vm_id: &VmId) -> usize {
        self.inner
            .lock()
            .unwrap()
            .get(vm_id)
            .map_or(0, |a| a.in_flight)
    }

    /// When the last proxied request to `vm_id` started, if any.
    pub(crate) fn last_activity(&self, vm_id: &VmId) -> Option<Instant> {
        self.inner
            .lock()
            .unwrap()
            .get(vm_id)
            .and_then(|a| a.last_activity)
    }

    /// Forget a VM entirely (on destroy).
    pub(crate) fn clear(&self, vm_id: &VmId) {
        self.inner.lock().unwrap().remove(vm_id);
    }
}

/// Marks one in-flight proxied request; dropping it releases the slot.
pub(crate) struct ActivityGuard {
    tracker: ActivityTracker,
    vm_id: VmId,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let mut inner = self.tracker.inner.lock().unwrap();
        if let Some(activity) = inner.get_mut(&self.vm_id) {
            activity.in_flight = activity.in_flight.saturating_sub(1);
            // A finishing request is traffic too: without this, a request
            // longer than the idle timeout would make the VM look idle the
            // moment it completes.
            activity.last_activity = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_tracks_in_flight() {
        let tracker = ActivityTracker::new();
        let vm = VmId::from("vm-1");
        assert_eq!(tracker.in_flight(&vm), 0);
        let guard = tracker.track(&vm);
        assert_eq!(tracker.in_flight(&vm), 1);
        let guard2 = tracker.track(&vm);
        assert_eq!(tracker.in_flight(&vm), 2);
        drop(guard);
        assert_eq!(tracker.in_flight(&vm), 1);
        drop(guard2);
        assert_eq!(tracker.in_flight(&vm), 0);
    }

    #[test]
    fn track_updates_last_activity() {
        let tracker = ActivityTracker::new();
        let vm = VmId::from("vm-1");
        assert!(tracker.last_activity(&vm).is_none());
        let before = Instant::now();
        let _guard = tracker.track(&vm);
        let last = tracker.last_activity(&vm).unwrap();
        assert!(last >= before);
    }

    #[test]
    fn vms_are_independent() {
        let tracker = ActivityTracker::new();
        let vm1 = VmId::from("vm-1");
        let vm2 = VmId::from("vm-2");
        let _guard = tracker.track(&vm1);
        assert_eq!(tracker.in_flight(&vm1), 1);
        assert_eq!(tracker.in_flight(&vm2), 0);
        assert!(tracker.last_activity(&vm2).is_none());
    }
}
