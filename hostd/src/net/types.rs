//! Data structures shared across the `net` module and its consumers: the
//! per-VM network config, host TAP device names, and a VM's allocated
//! network identity.

use std::net::{IpAddr, Ipv4Addr};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::vmm::vm::VmId;

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
        let suffix = vm_id.strip_prefix("vm-").unwrap_or(vm_id.as_ref());
        Self(format!("tap-{suffix}"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct NetworkConfig {
    pub allow_internet: bool,
    #[serde(default)]
    pub ingress_ports: Vec<u16>,
    #[serde(default)]
    pub egress: Vec<String>,
    #[serde(default)]
    pub public_access: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct VmNet {
    pub tap_name: TapName,
    pub guest_ip: IpAddr,
    pub gateway_ip: IpAddr,
    // NAT subnet in CIDR notation, e.g. `172.16.5.0/24`.
    pub subnet: String,
    // Guest MAC, e.g. `AA:FC:00:00:00:07`.
    pub guest_mac: String,
}

impl VmNet {
    pub(crate) fn new(
        tap_name: TapName,
        guest_ip: Ipv4Addr,
        gateway_ip: Ipv4Addr,
        subnet: String,
    ) -> Self {
        Self {
            tap_name,
            guest_ip: IpAddr::V4(guest_ip),
            gateway_ip: IpAddr::V4(gateway_ip),
            subnet,
            guest_mac: guest_mac_from_ip(guest_ip),
        }
    }

    /// Prefix length of the subnet CIDR, e.g. 24 for `172.16.5.0/24`.
    pub(crate) fn prefix_len(&self) -> Result<u8> {
        self.subnet
            .split_once('/')
            .and_then(|(_, prefix)| prefix.parse().ok())
            .ok_or_else(|| Error::net(format!("invalid subnet CIDR {:?}", self.subnet)))
    }

    /// Dotted netmask of the subnet, e.g. `255.255.255.0` for a /24.
    pub(crate) fn netmask(&self) -> Result<Ipv4Addr> {
        let prefix = self.prefix_len()?;
        if prefix > 32 {
            return Err(Error::net(format!("invalid subnet CIDR {:?}", self.subnet)));
        }
        let mask = if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - prefix)
        };
        Ok(Ipv4Addr::from(mask))
    }
}

/// Derive a locally administered guest MAC from the IPv4 address, e.g.
/// `AA:FC:AC:10:00:02` for `172.16.0.2`. Unique per IP, so unique per VM.
fn guest_mac_from_ip(ip: Ipv4Addr) -> String {
    let [a, b, c, d] = ip.octets();
    format!("AA:FC:{a:02X}:{b:02X}:{c:02X}:{d:02X}")
}
