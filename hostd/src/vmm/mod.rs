pub(crate) mod firecracker;

use std::sync::Arc;

use async_trait::async_trait;

use crate::common::vm::{VmConfig, VmId, VmInstanceRef};
use crate::error::Result;

#[async_trait]
pub(crate) trait Vmm: Send + Sync {
    async fn create_vm(&self, config: &VmConfig) -> Result<VmId>;
    async fn get_vm(&self, vm_id: &VmId) -> Result<Option<VmInstanceRef>>;
    async fn list_vms(&self) -> Result<Vec<VmInstanceRef>>;
    async fn destroy_vm(&self, vm_id: &VmId) -> Result<()>;
}
