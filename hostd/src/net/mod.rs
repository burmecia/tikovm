//! Host networking: per-project bridges and lifecycle-following IPAM.
//!
//! Each project gets one Linux bridge (`tbr-<project_id>`) carrying a /24 (by
//! default) subnet lazily carved out of a supernet. The host side of the
//! bridge is the gateway (.1); each VM gets a TAP device enslaved to the
//! bridge and a guest IP from .2 up. Same-project VMs reach each other at L2
//! through the bridge; internet egress is a per-subnet MASQUERADE rule.
//!
//! Allocation follows the VM lifecycle: the bridge/subnet/NAT rule are
//! created when a project's first VM is allocated and torn down when its last
//! VM is released. State is persisted to `network_state.json` and reconciled
//! on startup so a hostd restart never leaks devices or subnets.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::common::vm::{TapName, VmId, VmNet};
use crate::error::{Error, Result};

/// Host interface names are limited to 15 bytes (IFNAMSIZ - 1).
const IFNAMSIZ_MAX: usize = 15;

const BRIDGE_PREFIX: &str = "tbr-";

/// Minimal IPv4 CIDR, e.g. `172.16.0.0/12`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ipv4Net {
    addr: Ipv4Addr,
    prefix: u8,
}

impl Ipv4Net {
    fn parse(s: &str) -> Result<Self> {
        let (addr, prefix) = s
            .split_once('/')
            .ok_or_else(|| Error::net(format!("invalid CIDR {s:?}: missing prefix")))?;
        let addr: Ipv4Addr = addr
            .parse()
            .map_err(|e| Error::net(format!("invalid CIDR {s:?}: {e}")))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|e| Error::net(format!("invalid CIDR {s:?}: {e}")))?;
        if prefix == 0 || prefix > 32 {
            return Err(Error::net(format!("invalid CIDR {s:?}: bad prefix")));
        }
        let mask = u32::MAX << (32 - prefix);
        Ok(Self {
            addr: Ipv4Addr::from(u32::from(addr) & mask),
            prefix,
        })
    }

    /// Number of addresses in this network.
    fn size(&self) -> u32 {
        1u32 << (32 - self.prefix)
    }

    /// The address at host offset `idx` from the network address.
    fn host(&self, idx: u32) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.addr) + idx)
    }

    /// The `index`-th subnet of size /`prefix` within this network.
    fn subnet(&self, index: u32, prefix: u8) -> Ipv4Net {
        let subnet_size = 1u32 << (32 - prefix);
        Ipv4Net {
            addr: Ipv4Addr::from(u32::from(self.addr) + index * subnet_size),
            prefix,
        }
    }

    /// How many /`prefix` subnets fit inside this network.
    fn subnet_count(&self, prefix: u8) -> u32 {
        1u32 << (prefix - self.prefix)
    }
}

impl std::fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// Persisted allocation state. Pure data + pure plan computation, so the
/// allocator is unit-testable without root; `NetworkManager` applies the
/// resulting plans as host-side effects.
#[derive(Debug, Default, Serialize, Deserialize)]
struct NetState {
    projects: HashMap<u64, ProjectState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectState {
    /// Index of the project's subnet within the supernet.
    subnet_index: u32,
    bridge: String,
    gateway: Ipv4Addr,
    /// guest IP -> vm id, ordered so the lowest free IP is easy to find.
    vms: BTreeMap<Ipv4Addr, String>,
}

/// What `allocate` must create on the host.
struct AllocPlan {
    /// Bridge for the VM's TAP (existing or to be created).
    bridge: String,
    /// Set when this VM is the project's first: bridge + subnet + NAT to set up.
    setup: Option<ProjectSetup>,
    vm_net: VmNet,
}

struct ProjectSetup {
    bridge: String,
    gateway: Ipv4Addr,
    subnet: Ipv4Net,
}

/// What `release` must tear down on the host.
struct ReleasePlan {
    tap: TapName,
    /// Set when the released VM was the project's last.
    teardown: Option<ProjectSetup>,
}

impl NetState {
    /// Reserve a guest IP for `vm_id`, creating the project's subnet entry if
    /// this is its first VM. Mutates in memory; on host-effect failure the
    /// caller rolls back with `rollback_alloc`.
    fn alloc(
        &mut self,
        project_id: u64,
        vm_id: &VmId,
        tap_name: &TapName,
        supernet: &Ipv4Net,
        subnet_prefix: u8,
    ) -> Result<AllocPlan> {
        if let Some(project) = self.projects.get_mut(&project_id) {
            if project.vms.values().any(|id| id.as_str() == &**vm_id) {
                return Err(Error::net(format!(
                    "vm {vm_id} already has an IP allocated"
                )));
            }
            let subnet = supernet.subnet(project.subnet_index, subnet_prefix);
            let guest_ip = lowest_free_ip(&subnet, &project.vms).ok_or_else(|| {
                Error::net(format!(
                    "subnet {subnet} for project {project_id} is exhausted"
                ))
            })?;
            project.vms.insert(guest_ip, vm_id.to_string());
            return Ok(AllocPlan {
                bridge: project.bridge.clone(),
                setup: None,
                vm_net: VmNet::new(
                    tap_name.clone(),
                    guest_ip,
                    project.gateway,
                    subnet.to_string(),
                ),
            });
        }

        let used: std::collections::HashSet<u32> =
            self.projects.values().map(|p| p.subnet_index).collect();
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
            }),
            bridge,
            vm_net: VmNet::new(tap_name.clone(), guest_ip, gateway, subnet.to_string()),
        })
    }

    /// Undo a previously returned `alloc` after host-side setup failed.
    fn rollback_alloc(&mut self, project_id: u64, vm_id: &VmId) {
        if let Some(project) = self.projects.get_mut(&project_id) {
            project.vms.retain(|_, id| id.as_str() != &**vm_id);
            if project.vms.is_empty() {
                self.projects.remove(&project_id);
            }
        }
    }

    /// Release `vm_id`'s allocation. Returns `None` when the VM is unknown
    /// (idempotent release).
    fn release(
        &mut self,
        vm_id: &VmId,
        supernet: &Ipv4Net,
        subnet_prefix: u8,
    ) -> Option<ReleasePlan> {
        let project_id = self.projects.iter().find_map(|(pid, p)| {
            p.vms
                .values()
                .any(|id| id.as_str() == &**vm_id)
                .then_some(*pid)
        })?;
        let project = self.projects.get_mut(&project_id)?;
        project.vms.retain(|_, id| id.as_str() != &**vm_id);

        let teardown = if project.vms.is_empty() {
            let project = self.projects.remove(&project_id)?;
            Some(ProjectSetup {
                bridge: project.bridge,
                gateway: project.gateway,
                subnet: supernet.subnet(project.subnet_index, subnet_prefix),
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
fn lowest_free_ip(subnet: &Ipv4Net, used: &BTreeMap<Ipv4Addr, String>) -> Option<Ipv4Addr> {
    (2..subnet.size() - 1)
        .map(|idx| subnet.host(idx))
        .find(|ip| !used.contains_key(ip))
}

pub(crate) struct NetworkManager {
    state: Mutex<NetState>,
    state_path: PathBuf,
    supernet: Ipv4Net,
    subnet_prefix: u8,
}

impl NetworkManager {
    pub(crate) fn new(
        work_dir: impl AsRef<Path>,
        supernet: &str,
        subnet_prefix: u8,
    ) -> Result<Self> {
        let supernet = Ipv4Net::parse(supernet)?;
        if subnet_prefix <= supernet.prefix || subnet_prefix > 30 {
            return Err(Error::net(format!(
                "subnet prefix /{subnet_prefix} must be between /{} and /30 for supernet {supernet}",
                supernet.prefix + 1
            )));
        }

        let state_path = work_dir.as_ref().join("network_state.json");
        let state = match fs::read_to_string(&state_path) {
            Ok(contents) => serde_json::from_str(&contents)
                .map_err(|e| Error::net(format!("parse {}: {e}", state_path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => NetState::default(),
            Err(e) => return Err(e.into()),
        };

        Ok(Self {
            state: Mutex::new(state),
            state_path,
            supernet,
            subnet_prefix,
        })
    }

    /// Allocate a guest IP (+ TAP, and on first VM of the project the bridge,
    /// gateway, subnet and NAT rule) for `vm_id`.
    pub(crate) fn allocate(
        &self,
        project_id: u64,
        vm_id: &VmId,
        tap_name: &TapName,
    ) -> Result<VmNet> {
        let mut state = self.state.lock()?;
        let plan = state.alloc(
            project_id,
            vm_id,
            tap_name,
            &self.supernet,
            self.subnet_prefix,
        )?;

        let applied = (|| -> Result<()> {
            if let Some(setup) = &plan.setup {
                ensure_bridge(setup)?;
            }
            ensure_tap(&plan.vm_net.tap_name, &plan.bridge)
        })();

        if let Err(e) = applied {
            let _ = delete_tap(&plan.vm_net.tap_name);
            if let Some(setup) = &plan.setup {
                let _ = delete_bridge(setup);
            }
            state.rollback_alloc(project_id, vm_id);
            return Err(e);
        }

        persist(&self.state_path, &state)?;
        Ok(plan.vm_net)
    }

    /// Release `vm_id`'s allocation: delete its TAP, free its IP, and when it
    /// was the project's last VM, tear down the bridge + NAT rule and return
    /// the subnet to the pool. Idempotent and best-effort: host-side
    /// failures are logged, not propagated, so `destroy_vm` can always
    /// proceed.
    pub(crate) fn release(&self, vm_id: &VmId) -> Result<()> {
        let mut state = self.state.lock()?;
        let Some(plan) = state.release(vm_id, &self.supernet, self.subnet_prefix) else {
            return Ok(());
        };

        if let Err(e) = delete_tap(&plan.tap) {
            warn!(tap = %plan.tap, error = %e, "failed to delete TAP device");
        }
        if let Some(setup) = &plan.teardown
            && let Err(e) = delete_bridge(setup)
        {
            warn!(bridge = %setup.bridge, error = %e, "failed to delete bridge");
        }

        persist(&self.state_path, &state)?;
        Ok(())
    }

    /// Delete every device the persisted state claims, plus any stray `tbr-*`
    /// bridge, and reset the state to empty. Safe because no VM (and
    /// therefore no network consumer) survives a hostd restart: Firecracker
    /// children are killed when hostd exits.
    pub(crate) fn reconcile_on_startup(&self) -> Result<()> {
        let mut state = self.state.lock()?;

        for project in state.projects.values() {
            for vm_id in project.vms.values() {
                let tap = TapName::from(&VmId::from(vm_id.as_str()));
                if let Err(e) = delete_tap(&tap) {
                    warn!(tap = %tap, error = %e, "reconcile: failed to delete TAP");
                }
            }
            let setup = ProjectSetup {
                bridge: project.bridge.clone(),
                gateway: project.gateway,
                subnet: self
                    .supernet
                    .subnet(project.subnet_index, self.subnet_prefix),
            };
            if let Err(e) = delete_bridge(&setup) {
                warn!(bridge = %setup.bridge, error = %e, "reconcile: failed to delete bridge");
            }
        }

        // Stray bridges from a previous run that crashed before persisting.
        for name in stray_bridges()? {
            warn!(bridge = %name, "reconcile: deleting stray bridge");
            if let Err(e) = run("ip", &["link", "del", &name]) {
                warn!(bridge = %name, error = %e, "reconcile: failed to delete stray bridge");
            }
        }

        *state = NetState::default();
        persist(&self.state_path, &state)?;
        Ok(())
    }
}

fn persist(path: &Path, state: &NetState) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(state)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

// --- host-side effect helpers (need root / CAP_NET_ADMIN) ---

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| Error::net(format!("spawn {cmd}: {e}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::net(format!(
            "{cmd} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn link_exists(name: &str) -> bool {
    Command::new("ip")
        .args(["link", "show", "dev", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Names of existing bridges with our prefix (for startup reconciliation).
fn stray_bridges() -> Result<Vec<String>> {
    let output = Command::new("ip")
        .args(["-o", "link", "show", "type", "bridge"])
        .output()
        .map_err(|e| Error::net(format!("spawn ip: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .filter_map(|line| line.split(": ").nth(1))
        .map(|name| name.split('@').next().unwrap_or(name).to_string())
        .filter(|name| name.starts_with(BRIDGE_PREFIX))
        .collect())
}

fn ensure_bridge(setup: &ProjectSetup) -> Result<()> {
    if !link_exists(&setup.bridge) {
        run(
            "ip",
            &["link", "add", "name", &setup.bridge, "type", "bridge"],
        )?;
    }
    run(
        "ip",
        &[
            "addr",
            "replace",
            &format!("{}/{}", setup.gateway, setup.subnet.prefix),
            "dev",
            &setup.bridge,
        ],
    )?;
    run("ip", &["link", "set", &setup.bridge, "up"])?;
    ensure_ip_forward()?;
    // Egress NAT for the whole project subnet, plus explicit FORWARD accepts
    // (FORWARD policy is DROP on e.g. Docker-enabled hosts).
    iptables_ensure(&[
        "-t",
        "nat",
        "-A",
        "POSTROUTING",
        "-s",
        &setup.subnet.to_string(),
        "-j",
        "MASQUERADE",
    ])?;
    iptables_ensure(&[
        "-A",
        "FORWARD",
        "-s",
        &setup.subnet.to_string(),
        "-j",
        "ACCEPT",
    ])?;
    iptables_ensure(&[
        "-A",
        "FORWARD",
        "-d",
        &setup.subnet.to_string(),
        "-m",
        "conntrack",
        "--ctstate",
        "RELATED,ESTABLISHED",
        "-j",
        "ACCEPT",
    ])?;
    Ok(())
}

fn delete_bridge(setup: &ProjectSetup) -> Result<()> {
    iptables_remove(&[
        "-t",
        "nat",
        "-D",
        "POSTROUTING",
        "-s",
        &setup.subnet.to_string(),
        "-j",
        "MASQUERADE",
    ]);
    iptables_remove(&[
        "-D",
        "FORWARD",
        "-s",
        &setup.subnet.to_string(),
        "-j",
        "ACCEPT",
    ]);
    iptables_remove(&[
        "-D",
        "FORWARD",
        "-d",
        &setup.subnet.to_string(),
        "-m",
        "conntrack",
        "--ctstate",
        "RELATED,ESTABLISHED",
        "-j",
        "ACCEPT",
    ]);
    if link_exists(&setup.bridge) {
        run("ip", &["link", "set", &setup.bridge, "down"])?;
        run("ip", &["link", "del", &setup.bridge])?;
    }
    Ok(())
}

fn ensure_tap(tap: &TapName, bridge: &str) -> Result<()> {
    let name = tap.0.as_str();
    if !link_exists(name) {
        run("ip", &["tuntap", "add", "dev", name, "mode", "tap"])?;
    }
    run("ip", &["link", "set", name, "master", bridge, "up"])?;
    Ok(())
}

fn delete_tap(tap: &TapName) -> Result<()> {
    let name = tap.0.as_str();
    if link_exists(name) {
        run("ip", &["link", "del", name])?;
    }
    Ok(())
}

/// Idempotently append an iptables rule (`-C` check first, then apply args,
/// which must use `-A`).
fn iptables_ensure(args: &[&str]) -> Result<()> {
    let check: Vec<&str> = args
        .iter()
        .map(|a| if *a == "-A" { "-C" } else { a })
        .collect();
    let exists = Command::new("iptables")
        .args(&check)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        run("iptables", args)?;
    }
    Ok(())
}

/// Best-effort rule deletion; a missing rule is fine.
fn iptables_remove(args: &[&str]) {
    let _ = Command::new("iptables").args(args).output();
}

fn ensure_ip_forward() -> Result<()> {
    const PATH: &str = "/proc/sys/net/ipv4/ip_forward";
    if fs::read_to_string(PATH)
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
    {
        return Ok(());
    }
    fs::write(PATH, "1")?;
    Ok(())
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
                &supernet(),
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
        assert_eq!(plan.vm_net.subnet, "172.16.0.0/24");
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
        assert_eq!(plan.vm_net.subnet, "172.16.0.0/24");
    }

    #[test]
    fn other_project_gets_next_subnet() {
        let mut state = NetState::default();
        alloc(&mut state, 7, "vm-7-aaaaaa");
        let plan = alloc(&mut state, 8, "vm-8-cccccc");
        assert!(plan.setup.is_some());
        assert_eq!(plan.vm_net.subnet, "172.16.1.0/24");
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
            &supernet(),
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
            .release(&VmId::from("vm-7-aaaaaa"), &supernet(), 24)
            .unwrap();
        assert!(plan.teardown.is_none());
        assert!(state.projects.contains_key(&7));

        // Last release tears the project down...
        let plan = state
            .release(&VmId::from("vm-7-bbbbbb"), &supernet(), 24)
            .unwrap();
        let teardown = plan.teardown.unwrap();
        assert_eq!(teardown.bridge, "tbr-7");
        assert_eq!(teardown.subnet.to_string(), "172.16.0.0/24");
        assert!(state.projects.is_empty());

        // ...and the freed subnet is handed out again.
        let plan = alloc(&mut state, 9, "vm-9-dddddd");
        assert_eq!(plan.vm_net.subnet, "172.16.0.0/24");
    }

    #[test]
    fn freed_ip_is_reused_within_project() {
        let mut state = NetState::default();
        alloc(&mut state, 7, "vm-7-aaaaaa");
        alloc(&mut state, 7, "vm-7-bbbbbb");
        state.release(&VmId::from("vm-7-aaaaaa"), &supernet(), 24);
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
                .release(&VmId::from("vm-1-zzzzzz"), &supernet(), 24)
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
                &supernet,
                30,
            )
            .unwrap();
        assert_eq!(first.vm_net.subnet, "172.16.0.0/30");
        let second = state.alloc(
            1,
            &VmId::from("vm-1-bbbbbb"),
            &TapName::from("vm-1-bbbbbb"),
            &supernet,
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
                    &supernet,
                    30,
                )
                .unwrap();
        }
        let third = state.alloc(
            3,
            &VmId::from("vm-3-cccccc"),
            &TapName::from("vm-3-cccccc"),
            &supernet,
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

    #[test]
    fn cidr_parse_masks_host_bits() {
        let net = Ipv4Net::parse("172.16.3.77/12").unwrap();
        assert_eq!(net.to_string(), "172.16.0.0/12");
    }
}
