//! Host-side network effects: bridges, TAP devices, iptables NAT/FORWARD
//! rules and `ip_forward`. Everything here is idempotent (check before create,
//! ignore "not found" on delete) and needs root / `CAP_NET_ADMIN`.

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
        .is_ok_and(|o| o.status.success())
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
    // (FORWARD policy is DROP on e.g. Docker-enabled hosts). The MASQUERADE
    // matches on the egress interface, not the destination: traffic leaving
    // via a tikovm bridge (cross-project) keeps its real source IP — both so
    // per-project access rules in guests (e.g. a seeded PostgreSQL pg_hba
    // subnet rule) see the true origin, and so one project's VMs can't
    // impersonate the host's bridge IP on another project's subnet — while
    // everything heading to the real world is NATed. Matching on `-d
    // <supernet>` instead would break when real infrastructure overlaps the
    // supernet (e.g. this host's AWS VPC is 172.31.0.0/16 ⊂ 172.16.0.0/12:
    // guest traffic to an S3 Files mount target would leave unmasqueraded
    // and be dropped).
    //
    // Remove any legacy rule forms first (older hostd versions installed
    // `-s <subnet> -j MASQUERADE`, then `-s <subnet> ! -d <supernet> -j
    // MASQUERADE`); a stale form would otherwise shadow or leak at teardown.
    for rule in legacy_masq_rules(setup) {
        iptables_remove(&str_refs(&rule));
    }
    for rule in nat_rules(setup, "-A") {
        iptables_ensure(&str_refs(&rule))?;
    }
    Ok(())
}

pub(super) fn delete_bridge(setup: &ProjectSetup) -> Result<()> {
    for rule in nat_rules(setup, "-D") {
        iptables_remove(&str_refs(&rule));
    }
    // Also try the legacy forms, in case a bridge created by an older hostd
    // is being torn down.
    for rule in legacy_masq_rules(setup) {
        iptables_remove(&str_refs(&rule));
    }
    if link_exists(&setup.bridge) {
        run_cmd("ip", &["link", "set", &setup.bridge, "down"])?;
        run_cmd("ip", &["link", "del", &setup.bridge])?;
    }
    Ok(())
}

/// Borrowed view of a built rule, for the `&[&str]`-taking runners.
fn str_refs(rule: &[String]) -> Vec<&str> {
    rule.iter().map(String::as_str).collect()
}

/// The iptables rules wiring up a project bridge, with `verb` (`-A` or
/// `-D`) spliced in: egress NAT for the subnet for anything not heading back
/// into a tikovm bridge, plus the two FORWARD accepts. Shared by
/// `ensure_bridge` and `delete_bridge` so the two can never drift apart.
fn nat_rules(setup: &ProjectSetup, verb: &str) -> [Vec<String>; 3] {
    let subnet = setup.subnet.to_string();
    [
        vec![
            "-t".into(),
            "nat".into(),
            verb.into(),
            "POSTROUTING".into(),
            "-s".into(),
            subnet.clone(),
            "!".into(),
            "-o".into(),
            format!("{BRIDGE_PREFIX}+"),
            "-j".into(),
            "MASQUERADE".into(),
        ],
        vec![
            verb.into(),
            "FORWARD".into(),
            "-s".into(),
            subnet.clone(),
            "-j".into(),
            "ACCEPT".into(),
        ],
        vec![
            verb.into(),
            "FORWARD".into(),
            "-d".into(),
            subnet,
            "-m".into(),
            "conntrack".into(),
            "--ctstate".into(),
            "RELATED,ESTABLISHED".into(),
            "-j".into(),
            "ACCEPT".into(),
        ],
    ]
}

/// Legacy MASQUERADE rule forms, in delete form: only ever removed (by both
/// `ensure_bridge` and `delete_bridge`), never added. Hostd first installed
/// an unrestricted `-s <subnet> -j MASQUERADE`, then one excluding supernet
/// destinations (`! -d <supernet>`); both predate the egress-interface match.
fn legacy_masq_rules(setup: &ProjectSetup) -> [Vec<String>; 2] {
    let subnet = setup.subnet.to_string();
    [
        vec![
            "-t".into(),
            "nat".into(),
            "-D".into(),
            "POSTROUTING".into(),
            "-s".into(),
            subnet.clone(),
            "-j".into(),
            "MASQUERADE".into(),
        ],
        vec![
            "-t".into(),
            "nat".into(),
            "-D".into(),
            "POSTROUTING".into(),
            "-s".into(),
            subnet,
            "!".into(),
            "-d".into(),
            setup.supernet.to_string(),
            "-j".into(),
            "MASQUERADE".into(),
        ],
    ]
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
        .is_ok_and(|o| o.status.success());
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
    if fs::read_to_string(PATH).is_ok_and(|s| s.trim() == "1") {
        return Ok(());
    }
    fs::write(PATH, "1")?;
    Ok(())
}
