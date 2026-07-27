pub(crate) mod firecracker;
mod utils;
pub(crate) mod vm;
pub(crate) mod workload;

use async_trait::async_trait;

use self::vm::{VmConfig, VmId, VmInstanceRef, VmSnapshot};
use self::workload::{Workload, WorkloadId, WorkloadLogEntry, WorkloadSpec};
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

    async fn start_workload(&self, vm_id: &VmId, spec: WorkloadSpec) -> Result<Workload>;
    async fn stop_workload(&self, vm_id: &VmId, workload_id: &WorkloadId) -> Result<Workload>;
    async fn list_workloads(&self, vm_id: &VmId) -> Result<Vec<Workload>>;
    async fn get_workload(&self, vm_id: &VmId, workload_id: &WorkloadId) -> Result<Workload>;
    async fn workload_logs(
        &self,
        vm_id: &VmId,
        workload_id: &WorkloadId,
    ) -> Result<Vec<WorkloadLogEntry>>;
}
