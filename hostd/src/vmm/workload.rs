use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    utils::random_id,
    vm::{EnvVar, VmId},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct WorkloadId(pub String);

impl WorkloadId {
    pub(crate) fn new_random() -> Self {
        Self(random_id("wl"))
    }
}

impl std::fmt::Display for WorkloadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<String> for WorkloadId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

/// What to run inside the guest: argv plus optional env and working dir.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkloadSpec {
    pub argv: Vec<String>,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkloadState {
    /// Sent to guestd, waiting for the guest to confirm the spawn.
    Starting,
    Running,
    /// The process finished on its own (any exit code).
    Exited,
    /// The process finished after a stop request.
    Stopped,
    /// The guest failed to spawn the process.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Workload {
    pub workload_id: WorkloadId,
    pub vm_id: VmId,
    pub spec: WorkloadSpec,
    pub state: WorkloadState,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,

    /// Set when a stop has been requested but the exit event has not arrived
    /// yet; the exit event then lands the workload in Stopped.
    #[serde(skip)]
    pub(crate) stop_requested: bool,
}

impl Workload {
    pub(crate) fn new(vm_id: &VmId, spec: WorkloadSpec) -> Self {
        Self {
            workload_id: WorkloadId::new_random(),
            vm_id: vm_id.clone(),
            spec,
            state: WorkloadState::Starting,
            exit_code: None,
            signal: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
            stop_requested: false,
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        matches!(self.state, WorkloadState::Starting | WorkloadState::Running)
    }

    /// guestd confirmed the process spawned.
    pub(crate) fn mark_running(&mut self) {
        if self.state == WorkloadState::Starting {
            self.state = WorkloadState::Running;
            self.started_at = Some(Utc::now());
        }
    }

    /// guestd reported the process exit.
    pub(crate) fn mark_finished(&mut self, exit_code: Option<i32>, signal: Option<i32>) {
        if !self.is_active() {
            return;
        }
        self.state = if self.stop_requested {
            WorkloadState::Stopped
        } else {
            WorkloadState::Exited
        };
        self.exit_code = exit_code;
        self.signal = signal;
        self.finished_at = Some(Utc::now());
    }

    /// guestd failed to spawn the process.
    pub(crate) fn mark_failed(&mut self) {
        if self.is_active() {
            self.state = WorkloadState::Failed;
            self.finished_at = Some(Utc::now());
        }
    }
}

/// One captured output chunk, stored as a JSON line in the workload log file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkloadLogEntry {
    pub ts: DateTime<Utc>,
    pub stream: String,
    pub data: String,
}
