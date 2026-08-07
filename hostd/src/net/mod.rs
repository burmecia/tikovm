//! Host networking: per-project bridges and lifecycle-following IPAM.
//!
//! Each project gets one Linux bridge (`tbr-<project_id>`) carrying a /24 (by
//! default) subnet lazily carved out of a supernet. The host side of the
//! bridge is the gateway (.1); each VM gets a TAP device enslaved to the
//! bridge and a guest IP from .2 up. Same-project VMs reach each other at L2
//! through the bridge; internet egress is a per-subnet MASQUERADE rule that
//! excludes supernet destinations, so cross-project traffic is routed with
//! its real source IP (per-project access rules in guests can rely on it).
//!
//! Allocation follows the VM lifecycle: the bridge/subnet/NAT rule are
//! created when a project's first VM is allocated and torn down when its last
//! VM is released. State is persisted to `network_state.json` and reconciled
//! on startup so a hostd restart never leaks devices or subnets.
//!
//! Layout: `cidr` has the CIDR arithmetic, `types` the shared data
//! structures, `state` the pure (unit-testable) allocator, `host` the
//! host-side effects, and `manager` the `NetworkManager` tying them together.

mod cidr;
mod host;
mod manager;
mod state;
mod types;

pub(crate) use manager::NetworkManager;
pub(crate) use types::{ExposedPort, NetworkConfig, TapName, VmNet};

/// Host interface names are limited to 15 bytes (IFNAMSIZ - 1).
const IFNAMSIZ_MAX: usize = 15;

const BRIDGE_PREFIX: &str = "tbr-";
