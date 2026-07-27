//! Host-side network effects: bridges, TAP devices, iptables NAT/FORWARD
//! rules and ip_forward. Everything here is idempotent (check before create,
//! ignore "not found" on delete) and needs root / CAP_NET_ADMIN.

use std::fs;
use std::process::Command;

use super::BRIDGE_PREFIX;
use super::state::ProjectSetup;
use super::types::TapName;
use crate::error::{Error, Result};

pub(super) fn run_cmd(cmd: &str, args: &[&str]) -> Result<()> {
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
pub(super) fn stray_bridges() -> Result<Vec<String>> {
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

pub(super) fn ensure_bridge(setup: &ProjectSetup) -> Result<()> {
    if !link_exists(&setup.bridge) {
        run_cmd(
            "ip",
            &["link", "add", "name", &setup.bridge, "type", "bridge"],
        )?;
    }
    run_cmd(
        "ip",
        &[
            "addr",
            "replace",
            &format!("{}/{}", setup.gateway, setup.subnet.prefix),
            "dev",
            &setup.bridge,
        ],
    )?;
    run_cmd("ip", &["link", "set", &setup.bridge, "up"])?;
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

pub(super) fn delete_bridge(setup: &ProjectSetup) -> Result<()> {
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
        run_cmd("ip", &["link", "set", &setup.bridge, "down"])?;
        run_cmd("ip", &["link", "del", &setup.bridge])?;
    }
    Ok(())
}

pub(super) fn ensure_tap(tap: &TapName, bridge: &str) -> Result<()> {
    let name = tap.0.as_str();
    if !link_exists(name) {
        run_cmd("ip", &["tuntap", "add", "dev", name, "mode", "tap"])?;
    }
    run_cmd("ip", &["link", "set", name, "master", bridge, "up"])?;
    Ok(())
}

pub(super) fn delete_tap(tap: &TapName) -> Result<()> {
    let name = tap.0.as_str();
    if link_exists(name) {
        run_cmd("ip", &["link", "del", name])?;
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
        run_cmd("iptables", args)?;
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
