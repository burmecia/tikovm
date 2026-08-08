//! Auto-suspend idle detector: periodically run a hostd-supplied check
//! command and emit an `idle` event each time it exits 0.
//!
//! The detector program is how non-HTTP workloads (e.g. a Postgres server)
//! signal idleness: the VM image ships a check script (conntrack/ss
//! inspection, application-level metrics, ...) and hostd names it in the
//! VM's `auto_suspend.idle_check_cmd` config. Exit status 0 means "idle";
//! anything else (non-zero exit, spawn failure, timeout) means "not idle".
//! hostd applies its own gates to the event before suspending, so reporting
//! idle on every interval is safe and idempotent.

use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::agent::Agent;

/// A check command running longer than this is killed and counts as
/// "not idle" — a hung detector must not suspend (or wedge) anything.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// How often the monitor thread re-reads its config while disabled.
const DISABLED_POLL: Duration = Duration::from_secs(1);
/// Poll granularity while waiting for a check command to exit.
const WAIT_POLL: Duration = Duration::from_millis(100);

/// Active detector configuration; `None` in the shared cell disables the
/// detector.
struct IdleCheckConfig {
    cmd: Vec<String>,
    interval: Duration,
}

/// The auto-suspend monitor: holds the current detector config and spawns
/// the monitor thread on first use. Cheap to construct; the `Agent` owns it.
pub(crate) struct IdleMonitor {
    /// Shared behind an `Arc` so the monitor loop's per-cycle re-read clones
    /// one pointer instead of the whole command vector.
    config: Arc<Mutex<Option<Arc<IdleCheckConfig>>>>,
    started: AtomicBool,
}

impl IdleMonitor {
    pub(crate) fn new() -> Self {
        Self {
            config: Arc::new(Mutex::new(None)),
            started: AtomicBool::new(false),
        }
    }

    /// Install (or, with an empty `cmd`, remove) the detector configuration,
    /// spawning the monitor thread the first time a real config arrives.
    pub(crate) fn configure(
        self: &Arc<Self>,
        agent: &Arc<Agent>,
        cmd: Vec<String>,
        interval_secs: u64,
    ) {
        if cmd.is_empty() {
            *self.config.lock().unwrap() = None;
            debug!("auto-suspend idle detector disabled");
            return;
        }
        *self.config.lock().unwrap() = Some(Arc::new(IdleCheckConfig {
            cmd,
            interval: Duration::from_secs(interval_secs.max(1)),
        }));
        if self.started.swap(true, Ordering::SeqCst) {
            return; // thread already running; it picks up the new config
        }
        let monitor = Arc::clone(self);
        let agent = Arc::clone(agent);
        thread::spawn(move || monitor.run(&agent));
    }

    /// Monitor loop: run the configured check every interval, emit `idle`
    /// when it reports idle. Re-reads the config each cycle so reconfigures
    /// (e.g. after a hostd reconnect) take effect without a restart.
    fn run(&self, agent: &Arc<Agent>) {
        loop {
            let current = self.config.lock().unwrap().clone();
            let Some(config) = current else {
                thread::sleep(DISABLED_POLL);
                continue;
            };
            thread::sleep(config.interval);
            if is_idle(&config.cmd) {
                agent.send_idle_event();
            }
        }
    }
}

/// Run the check command and interpret its result: exit status 0 = idle.
/// Failures to spawn or timeouts count as "not idle".
fn is_idle(cmd: &[String]) -> bool {
    // configure() rejects an empty cmd, so this split never fails.
    let (prog, args) = cmd.split_first().expect("configure rejects empty cmd");
    let mut child = match Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            warn!(error = %e, cmd = ?cmd, "auto-suspend idle check failed to spawn");
            return false;
        }
    };
    if let Some(status) = wait_with_timeout(&mut child, CHECK_TIMEOUT) {
        let idle = status.success();
        debug!(cmd = ?cmd, %status, idle, "auto-suspend idle check finished");
        idle
    } else {
        warn!(cmd = ?cmd, "auto-suspend idle check timed out");
        false
    }
}

/// Wait for `child` up to `timeout`; on timeout kill it and return `None`.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait(); // reap
                    return None;
                }
                thread::sleep(WAIT_POLL);
            }
            Err(e) => {
                warn!(error = %e, "idle check wait failed");
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(args: &[&str]) -> Child {
        Command::new("sh")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn exit_zero_is_idle() {
        assert!(is_idle(&["sh".into(), "-c".into(), "exit 0".into()]));
    }

    #[test]
    fn non_zero_exit_is_not_idle() {
        assert!(!is_idle(&["sh".into(), "-c".into(), "exit 1".into()]));
    }

    #[test]
    fn unspawnable_command_is_not_idle() {
        assert!(!is_idle(&["/nonexistent/tikovm-check".into()]));
    }

    #[test]
    fn timeout_kills_and_reports_none() {
        let mut child = sh(&["-c", "sleep 30"]);
        assert!(wait_with_timeout(&mut child, Duration::from_millis(300)).is_none());
        assert!(child.try_wait().unwrap().is_some(), "child must be reaped");
    }

    #[test]
    fn fast_command_returns_status() {
        let mut child = sh(&["-c", "exit 0"]);
        let status = wait_with_timeout(&mut child, Duration::from_secs(5)).unwrap();
        assert!(status.success());
    }
}
