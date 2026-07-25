use rand::Rng;
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::ops::Deref;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct VmId(pub String);

impl VmId {
    pub(crate) fn new_random(project_id: u64) -> Self {
        let mut rng = rand::rng();
        const ID_CHARSET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
        let short_id: String = (0..6)
            .map(|_| {
                let idx = rng.random_range(0..ID_CHARSET.len());
                ID_CHARSET[idx] as char
            })
            .collect();
        Self(format!("vm-{project_id}-{short_id}"))
    }
}

impl std::fmt::Display for VmId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl Deref for VmId {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<str> for VmId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<OsStr> for VmId {
    fn as_ref(&self) -> &OsStr {
        self.0.as_ref()
    }
}

impl From<String> for VmId {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for VmId {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct TapName(pub String);

impl std::fmt::Display for TapName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<String> for TapName {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for TapName {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

impl From<VmId> for TapName {
    fn from(vm_id: VmId) -> Self {
        let suffix = vm_id.0.strip_prefix("vm-").unwrap_or(&vm_id.0);
        Self(format!("tap-{suffix}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VmState {
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
    pub name: String,
    pub project_id: u64,
}
