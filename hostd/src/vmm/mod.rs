pub(crate) mod firecracker;

use async_trait::async_trait;

use crate::common::vm::{VmConfig, VmId, VmInstanceRef, VmSnapshot};
use crate::error::Result;

#[async_trait]
pub(crate) trait Vmm: Send + Sync {
    async fn create_vm(&self, config: &VmConfig) -> Result<VmId>;
    async fn get_vm(&self, vm_id: &VmId) -> Result<Option<VmInstanceRef>>;
    async fn list_vms(&self) -> Result<Vec<VmInstanceRef>>;
    async fn start_vm(&self, vm_id: &VmId) -> Result<()>;
    async fn pause_vm(&self, vm_id: &VmId) -> Result<()>;
    async fn resume_vm(&self, vm_id: &VmId) -> Result<()>;
    async fn snapshot_vm(&self, vm_id: &VmId) -> Result<VmSnapshot>;
    async fn restore_vm(&self, vm_id: &VmId) -> Result<()>;
    async fn destroy_vm(&self, vm_id: &VmId) -> Result<()>;
}
