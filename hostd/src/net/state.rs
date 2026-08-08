//! Pure IPAM state: which project owns which subnet and which VM owns which
//! guest IP, plus computation of what an allocate/release must change on the
//! host. No host-side effects in this module, so the allocator is
//! unit-testable without root; `NetworkManager` applies the resulting plans
//! via the `host` module.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::Ipv4Addr;

use serde::{Deserialize, Serialize};

use super::cidr::Ipv4Net;
use super::types::{TapName, VmNet};
use super::{BRIDGE_PREFIX, IFNAMSIZ_MAX};
use crate::error::{Error, Result};
use crate::vmm::vm::VmId;

/// Persisted allocation state.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct NetState {
    pub(super) projects: HashMap<u64, ProjectState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ProjectState {
    /// Index of the project's subnet within the supernet.
    pub(super) subnet_index: u32,
    pub(super) bridge: String,
    pub(super) gateway: Ipv4Addr,
    /// guest IP -> vm id, ordered so the lowest free IP is easy to find.
    pub(super) vms: BTreeMap<Ipv4Addr, String>,
}

/// What `allocate` must create on the host.
pub(super) struct AllocPlan {
    /// Bridge for the VM's TAP (existing or to be created).
    pub(super) bridge: String,
    /// Set when this VM is the project's first: bridge + subnet + NAT to set up.
    pub(super) setup: Option<ProjectSetup>,
    pub(super) vm_net: VmNet,
}

pub(super) struct ProjectSetup {
    pub(super) bridge: String,
    pub(super) gateway: Ipv4Addr,
    pub(super) subnet: Ipv4Net,
    /// The supernet the subnet was carved from; the egress MASQUERADE must
    /// exclude it so intra-supernet (cross-project) traffic keeps its real
    /// source IP instead of being rewritten to the host's bridge IP.
    pub(super) supernet: Ipv4Net,
}

impl ProjectState {
    /// Whether this project's VM map contains `vm_id`.
    fn owns_vm(&self, vm_id: &VmId) -> bool {
        self.vms.values().any(|id| id.as_str() == &**vm_id)
    }

    /// Remove `vm_id` from the VM map, if present.
    fn remove_vm(&mut self, vm_id: &VmId) {
        self.vms.retain(|_, id| id.as_str() != &**vm_id);
    }
}

/// What `release` must tear down on the host.
pub(super) struct ReleasePlan {
    pub(super) tap: TapName,
    /// Set when the released VM was the project's last.
    pub(super) teardown: Option<ProjectSetup>,
}

impl NetState {
    /// Reserve a guest IP for `vm_id`, creating the project's subnet entry if
    /// this is its first VM. Mutates in memory; on host-effect failure the
    /// caller rolls back with `rollback_alloc`.
    pub(super) fn alloc(
        &mut self,
        project_id: u64,
        vm_id: &VmId,
        tap_name: &TapName,
        supernet: Ipv4Net,
        subnet_prefix: u8,
    ) -> Result<AllocPlan> {
        if let Some(project) = self.projects.get_mut(&project_id) {
            if project.owns_vm(vm_id) {
                return Err(Error::net(format!(
                    "vm {vm_id} already has an IP allocated"
                )));
            }
            let subnet = supernet.subnet(project.subnet_index, subnet_prefix);
            let guest_ip = lowest_free_ip(subnet, &project.vms).ok_or_else(|| {
                Error::net(format!(
                    "subnet {subnet} for project {project_id} is exhausted"
                ))
            })?;
            project.vms.insert(guest_ip, vm_id.to_string());
            return Ok(AllocPlan {
                bridge: project.bridge.clone(),
                setup: None,
                vm_net: VmNet::new(tap_name.clone(), guest_ip, project.gateway, subnet),
            });
        }

        let used: HashSet<u32> = self.projects.values().map(|p| p.subnet_index).collect();
        let subnet_index = (0..supernet.subnet_count(subnet_prefix))
            .find(|i| !used.contains(i))
            .ok_or_else(|| Error::net(format!("supernet {supernet} has no free subnets")))?;

        let bridge = format!("{BRIDGE_PREFIX}{project_id}");
        if bridge.len() > IFNAMSIZ_MAX {
            return Err(Error::net(format!(
                "bridge name {bridge:?} exceeds {IFNAMSIZ_MAX} bytes"
            )));
        }

        let subnet = supernet.subnet(subnet_index, subnet_prefix);
        let gateway = subnet.host(1);
        let guest_ip = subnet.host(2);

        let mut vms = BTreeMap::new();
        vms.insert(guest_ip, vm_id.to_string());
        self.projects.insert(
            project_id,
            ProjectState {
                subnet_index,
                bridge: bridge.clone(),
                gateway,
                vms,
            },
        );

        Ok(AllocPlan {
            setup: Some(ProjectSetup {
                bridge: bridge.clone(),
                gateway,
                subnet,
                supernet,
            }),
            bridge,
            vm_net: VmNet::new(tap_name.clone(), guest_ip, gateway, subnet),
        })
    }

    /// Undo a previously returned `alloc` after host-side setup failed.
    pub(super) fn rollback_alloc(&mut self, project_id: u64, vm_id: &VmId) {
        if let Some(project) = self.projects.get_mut(&project_id) {
            project.remove_vm(vm_id);
            if project.vms.is_empty() {
                self.projects.remove(&project_id);
            }
        }
    }

    /// Release `vm_id`'s allocation. Returns `None` when the VM is unknown
    /// (idempotent release).
    pub(super) fn release(
        &mut self,
        vm_id: &VmId,
        supernet: Ipv4Net,
        subnet_prefix: u8,
    ) -> Option<ReleasePlan> {
        let project_id = self
            .projects
            .iter()
            .find_map(|(pid, p)| p.owns_vm(vm_id).then_some(*pid))?;
        let project = self.projects.get_mut(&project_id)?;
        project.remove_vm(vm_id);

        let teardown = if project.vms.is_empty() {
            let project = self.projects.remove(&project_id)?;
            Some(ProjectSetup {
                bridge: project.bridge,
                gateway: project.gateway,
                subnet: supernet.subnet(project.subnet_index, subnet_prefix),
                supernet,
            })
        } else {
            None
        };

        Some(ReleasePlan {
            tap: TapName::from(vm_id),
            teardown,
        })
    }
}

/// Lowest usable host IP in `subnet` not present in `used`: skips the network
/// address (.0), the gateway (.1) and the broadcast address (last).
fn lowest_free_ip(subnet: Ipv4Net, used: &BTreeMap<Ipv4Addr, String>) -> Option<Ipv4Addr> {
    (2..subnet.size() - 1)
        .map(|idx| subnet.host(idx))
        .find(|ip| !used.contains_key(ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supernet() -> Ipv4Net {
        Ipv4Net::parse("172.16.0.0/12").unwrap()
    }

    fn alloc(state: &mut NetState, project_id: u64, vm_id: &str) -> AllocPlan {
        state
            .alloc(
                project_id,
                &VmId::from(vm_id),
                &TapName::from(vm_id),
                supernet(),
                24,
            )
            .unwrap()
    }

    #[test]
    fn first_vm_creates_project() {
        let mut state = NetState::default();
        let plan = alloc(&mut state, 7, "vm-7-aaaaaa");
        assert!(plan.setup.is_some());
        let setup = plan.setup.unwrap();
        assert_eq!(setup.bridge, "tbr-7");
        assert_eq!(setup.gateway, Ipv4Addr::new(172, 16, 0, 1));
        assert_eq!(setup.subnet.to_string(), "172.16.0.0/24");
        assert_eq!(
            plan.vm_net.guest_ip,
            "172.16.0.2".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(
            plan.vm_net.gateway_ip,
            "172.16.0.1".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(plan.vm_net.subnet.to_string(), "172.16.0.0/24");
        assert_eq!(plan.vm_net.guest_mac, "AA:FC:AC:10:00:02");
    }

    #[test]
    fn second_vm_shares_project_subnet() {
        let mut state = NetState::default();
        alloc(&mut state, 7, "vm-7-aaaaaa");
        let plan = alloc(&mut state, 7, "vm-7-bbbbbb");
        assert!(plan.setup.is_none());
        assert_eq!(plan.bridge, "tbr-7");
        assert_eq!(
            plan.vm_net.guest_ip,
            "172.16.0.3".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(plan.vm_net.subnet.to_string(), "172.16.0.0/24");
    }

    #[test]
    fn other_project_gets_next_subnet() {
        let mut state = NetState::default();
        alloc(&mut state, 7, "vm-7-aaaaaa");
        let plan = alloc(&mut state, 8, "vm-8-cccccc");
        assert!(plan.setup.is_some());
        assert_eq!(plan.vm_net.subnet.to_string(), "172.16.1.0/24");
        assert_eq!(
            plan.vm_net.guest_ip,
            "172.16.1.2".parse::<std::net::IpAddr>().unwrap()
        );
    }

    #[test]
    fn duplicate_vm_rejected() {
        let mut state = NetState::default();
        alloc(&mut state, 7, "vm-7-aaaaaa");
        let dup = state.alloc(
            7,
            &VmId::from("vm-7-aaaaaa"),
            &TapName::from("vm-7-aaaaaa"),
            supernet(),
            24,
        );
        assert!(dup.is_err());
    }

    #[test]
    fn last_release_tears_down_and_subnet_is_reused() {
        let mut state = NetState::default();
        alloc(&mut state, 7, "vm-7-aaaaaa");
        alloc(&mut state, 7, "vm-7-bbbbbb");

        // Non-last release keeps the project.
        let plan = state
            .release(&VmId::from("vm-7-aaaaaa"), supernet(), 24)
            .unwrap();
        assert!(plan.teardown.is_none());
        assert!(state.projects.contains_key(&7));

        // Last release tears the project down...
        let plan = state
            .release(&VmId::from("vm-7-bbbbbb"), supernet(), 24)
            .unwrap();
        let teardown = plan.teardown.unwrap();
        assert_eq!(teardown.bridge, "tbr-7");
        assert_eq!(teardown.subnet.to_string(), "172.16.0.0/24");
        assert!(state.projects.is_empty());

        // ...and the freed subnet is handed out again.
        let plan = alloc(&mut state, 9, "vm-9-dddddd");
        assert_eq!(plan.vm_net.subnet.to_string(), "172.16.0.0/24");
    }

    #[test]
    fn freed_ip_is_reused_within_project() {
        let mut state = NetState::default();
        alloc(&mut state, 7, "vm-7-aaaaaa");
        alloc(&mut state, 7, "vm-7-bbbbbb");
        state.release(&VmId::from("vm-7-aaaaaa"), supernet(), 24);
        let plan = alloc(&mut state, 7, "vm-7-cccccc");
        assert_eq!(
            plan.vm_net.guest_ip,
            "172.16.0.2".parse::<std::net::IpAddr>().unwrap()
        );
    }

    #[test]
    fn release_unknown_vm_is_none() {
        let mut state = NetState::default();
        assert!(
            state
                .release(&VmId::from("vm-1-zzzzzz"), supernet(), 24)
                .is_none()
        );
    }

    #[test]
    fn subnet_exhaustion_errors() {
        let mut state = NetState::default();
        let supernet = Ipv4Net::parse("172.16.0.0/28").unwrap();
        // /30 subnet: 4 addresses, only host index 2 usable (.0 net, .1 gw, .3 bcast).
        let first = state
            .alloc(
                1,
                &VmId::from("vm-1-aaaaaa"),
                &TapName::from("vm-1-aaaaaa"),
                supernet,
                30,
            )
            .unwrap();
        assert_eq!(first.vm_net.subnet.to_string(), "172.16.0.0/30");
        let second = state.alloc(
            1,
            &VmId::from("vm-1-bbbbbb"),
            &TapName::from("vm-1-bbbbbb"),
            supernet,
            30,
        );
        assert!(second.is_err());
    }

    #[test]
    fn supernet_exhaustion_errors() {
        let mut state = NetState::default();
        let supernet = Ipv4Net::parse("172.16.0.0/29").unwrap();
        // A /29 holds exactly two /30 subnets (one usable VM IP each).
        for project in 1..=2 {
            state
                .alloc(
                    project,
                    &VmId::from(format!("vm-{project}-aaaaaa")),
                    &TapName::from(format!("vm-{project}-aaaaaa").as_str()),
                    supernet,
                    30,
                )
                .unwrap();
        }
        let third = state.alloc(
            3,
            &VmId::from("vm-3-cccccc"),
            &TapName::from("vm-3-cccccc"),
            supernet,
            30,
        );
        assert!(third.is_err());
    }

    #[test]
    fn rollback_alloc_undoes_new_project() {
        let mut state = NetState::default();
        alloc(&mut state, 7, "vm-7-aaaaaa");
        state.rollback_alloc(7, &VmId::from("vm-7-aaaaaa"));
        assert!(state.projects.is_empty());
    }

    #[test]
    fn rollback_alloc_keeps_existing_project() {
        let mut state = NetState::default();
        alloc(&mut state, 7, "vm-7-aaaaaa");
        alloc(&mut state, 7, "vm-7-bbbbbb");
        state.rollback_alloc(7, &VmId::from("vm-7-bbbbbb"));
        let project = &state.projects[&7];
        assert_eq!(project.vms.len(), 1);
    }
}
