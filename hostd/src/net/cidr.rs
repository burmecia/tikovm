//! Minimal IPv4 CIDR arithmetic used by the subnet allocator.

use std::net::Ipv4Addr;

use crate::error::{Error, Result};

/// Build the uniform parse error for a malformed CIDR string.
fn invalid_cidr(s: &str, why: impl std::fmt::Display) -> Error {
    Error::net(format!("invalid CIDR {s:?}: {why}"))
}

/// Minimal IPv4 CIDR, e.g. `172.16.0.0/12`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ipv4Net {
    addr: Ipv4Addr,
    pub(super) prefix: u8,
}

impl Ipv4Net {
    pub(super) fn parse(s: &str) -> Result<Self> {
        let (addr, prefix) = s
            .split_once('/')
            .ok_or_else(|| invalid_cidr(s, "missing prefix"))?;
        let addr: Ipv4Addr = addr.parse().map_err(|e| invalid_cidr(s, e))?;
        let prefix: u8 = prefix.parse().map_err(|e| invalid_cidr(s, e))?;
        if !(1..=32).contains(&prefix) {
            return Err(invalid_cidr(s, "bad prefix"));
        }
        let mask = u32::MAX << (32 - prefix);
        Ok(Self {
            addr: Ipv4Addr::from(u32::from(addr) & mask),
            prefix,
        })
    }

    /// Number of addresses in this network.
    pub(super) fn size(self) -> u32 {
        1u32 << (32 - self.prefix)
    }

    /// The address at host offset `idx` from the network address.
    pub(super) fn host(self, idx: u32) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.addr) + idx)
    }

    /// The `index`-th subnet of size /`prefix` within this network.
    pub(super) fn subnet(self, index: u32, prefix: u8) -> Ipv4Net {
        let subnet_size = 1u32 << (32 - prefix);
        Ipv4Net {
            addr: Ipv4Addr::from(u32::from(self.addr) + index * subnet_size),
            prefix,
        }
    }

    /// How many /`prefix` subnets fit inside this network.
    pub(super) fn subnet_count(self, prefix: u8) -> u32 {
        1u32 << (prefix - self.prefix)
    }

    /// Dotted netmask, e.g. `255.255.255.0` for a /24.
    pub(crate) fn netmask(self) -> Ipv4Addr {
        Ipv4Addr::from(u32::MAX << (32 - self.prefix))
    }
}

impl std::fmt::Display for Ipv4Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

// Serialized as its CIDR string ("172.16.0.0/24"), keeping `VmNet`'s JSON
// shape unchanged.
impl serde::Serialize for Ipv4Net {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Ipv4Net {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = <&str as serde::Deserialize>::deserialize(deserializer)?;
        Self::parse(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_parse_masks_host_bits() {
        let net = Ipv4Net::parse("172.16.3.77/12").unwrap();
        assert_eq!(net.to_string(), "172.16.0.0/12");
    }
}
