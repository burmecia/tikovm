//! Data structures shared across the `net` module and its consumers: the
//! per-VM network config, host TAP device names, and a VM's allocated
//! network identity.

use std::net::{IpAddr, Ipv4Addr};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::vmm::vm::VmId;

use super::cidr::Ipv4Net;

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl From<&VmId> for TapName {
    fn from(vm_id: &VmId) -> Self {
        Self::for_vm(vm_id)
    }
}

impl TapName {
    /// TAP name for a VM id: `vm-1-aaaaaa` becomes `tap-1-aaaaaa`.
    pub(crate) fn for_vm(vm_id: &str) -> Self {
        let suffix = vm_id.strip_prefix("vm-").unwrap_or(vm_id);
        Self(format!("tap-{suffix}"))
    }
}

/// A guest TCP port the VM exposes for its HTTP workloads, with a
/// human-readable `label` describing the port's purpose (e.g. "web", "api").
///
/// This is a VM-side registry: the JWT-authenticated reverse proxy reads it
/// on every connection to decide whether `<guest_ip>:<port>` is an allowed
/// target, so removing a port revokes access immediately.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExposedPort {
    pub port: u16,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct NetworkConfig {
    pub allow_internet: bool,
    /// Ports (with labels) the VM exposes for its HTTP workloads. Managed via
    /// the `/api/vms/{id}/ports` endpoints; see [`ExposedPort`].
    #[serde(default)]
    pub exposed_ports: Vec<ExposedPort>,
    #[serde(default)]
    pub egress: Vec<String>,
    #[serde(default)]
    pub public_access: bool,
}

impl NetworkConfig {
    /// Register an exposed port. The port number is the identity: adding the
    /// same port twice is a conflict, and port 0 is invalid.
    pub(crate) fn add_exposed_port(&mut self, vm_id: &VmId, port: ExposedPort) -> Result<()> {
        if port.port == 0 {
            return Err(Error::InvalidPort(port.port));
        }
        if self.exposed_ports.iter().any(|p| p.port == port.port) {
            return Err(Error::PortAlreadyExposed {
                vm_id: vm_id.to_string(),
                port: port.port,
            });
        }
        self.exposed_ports.push(port);
        Ok(())
    }

    /// Remove an exposed port by port number.
    pub(crate) fn remove_exposed_port(&mut self, vm_id: &VmId, port: u16) -> Result<()> {
        let Some(pos) = self.exposed_ports.iter().position(|p| p.port == port) else {
            return Err(Error::PortNotExposed {
                vm_id: vm_id.to_string(),
                port,
            });
        };
        self.exposed_ports.remove(pos);
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VmNet {
    pub tap_name: TapName,
    pub guest_ip: IpAddr,
    pub gateway_ip: IpAddr,
    /// NAT subnet of the VM's project, e.g. `172.16.5.0/24`.
    pub subnet: Ipv4Net,
    // Guest MAC, e.g. `AA:FC:00:00:00:07`.
    pub guest_mac: String,
}

impl VmNet {
    pub(crate) fn new(
        tap_name: TapName,
        guest_ip: Ipv4Addr,
        gateway_ip: Ipv4Addr,
        subnet: Ipv4Net,
    ) -> Self {
        Self {
            tap_name,
            guest_ip: IpAddr::V4(guest_ip),
            gateway_ip: IpAddr::V4(gateway_ip),
            subnet,
            guest_mac: guest_mac_from_ip(guest_ip),
        }
    }

    /// Dotted netmask of the subnet, e.g. `255.255.255.0` for a /24.
    pub(crate) fn netmask(&self) -> Ipv4Addr {
        self.subnet.netmask()
    }
}

/// Derive a locally administered guest MAC from the IPv4 address, e.g.
/// `AA:FC:AC:10:00:02` for `172.16.0.2`. Unique per IP, so unique per VM.
fn guest_mac_from_ip(ip: Ipv4Addr) -> String {
    let [a, b, c, d] = ip.octets();
    format!("AA:FC:{a:02X}:{b:02X}:{c:02X}:{d:02X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm_id() -> VmId {
        VmId::from("vm-1-test")
    }

    fn exposed(port: u16) -> ExposedPort {
        ExposedPort {
            port,
            label: format!("svc-{port}"),
        }
    }

    #[test]
    fn add_exposed_port_appends() {
        let mut cfg = NetworkConfig::default();
        cfg.add_exposed_port(&vm_id(), exposed(8080)).unwrap();
        cfg.add_exposed_port(&vm_id(), exposed(3000)).unwrap();
        assert_eq!(cfg.exposed_ports, vec![exposed(8080), exposed(3000)]);
    }

    #[test]
    fn add_exposed_port_rejects_zero() {
        let mut cfg = NetworkConfig::default();
        assert!(matches!(
            cfg.add_exposed_port(&vm_id(), exposed(0)),
            Err(Error::InvalidPort(0))
        ));
        assert!(cfg.exposed_ports.is_empty());
    }

    #[test]
    fn add_exposed_port_rejects_duplicate() {
        let mut cfg = NetworkConfig::default();
        cfg.add_exposed_port(&vm_id(), exposed(8080)).unwrap();
        assert!(matches!(
            cfg.add_exposed_port(&vm_id(), exposed(8080)),
            Err(Error::PortAlreadyExposed { port: 8080, .. })
        ));
        assert_eq!(cfg.exposed_ports.len(), 1);
    }

    #[test]
    fn remove_exposed_port_removes() {
        let mut cfg = NetworkConfig::default();
        cfg.add_exposed_port(&vm_id(), exposed(8080)).unwrap();
        cfg.add_exposed_port(&vm_id(), exposed(3000)).unwrap();
        cfg.remove_exposed_port(&vm_id(), 8080).unwrap();
        assert_eq!(cfg.exposed_ports, vec![exposed(3000)]);
    }

    #[test]
    fn remove_exposed_port_rejects_missing() {
        let mut cfg = NetworkConfig::default();
        assert!(matches!(
            cfg.remove_exposed_port(&vm_id(), 8080),
            Err(Error::PortNotExposed { port: 8080, .. })
        ));
    }
}
