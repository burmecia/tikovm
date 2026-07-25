pub(crate) mod firecracker;

use async_trait::async_trait;

use crate::common::vm::{VmConfig, VmId};
use crate::error::Result;

#[async_trait]
pub(crate) trait Vmm: Send + Sync {
    async fn create_vm(&self, config: &VmConfig) -> Result<VmId>;
}
