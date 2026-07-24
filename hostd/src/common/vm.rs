use serde::{Deserialize, Serialize};

pub(crate) type VmId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    // --- transitional ---
    Creating,
    Starting,
    Pausing,
    Resuming,
    Suspending,
    Restoring,
    Destroying,
    // --- stable ---
    Created,
    Started,
    Paused,
    Suspended,
    Destroyed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VmConfig {
    pub vm_id: VmId,
}
