//! Workload execution core: spawn, stop, track, and report guest processes.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use tracing::warn;

use crate::monitor::IdleMonitor;
use crate::proto::{Event, Request, WorkloadInfo};

/// Grace period between SIGTERM and SIGKILL when stopping a workload.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// Chunk size for forwarding child stdout/stderr.
const OUTPUT_BUF_SIZE: usize = 8 * 1024;

/// Write half of the current host connection. A reconnect replaces it, and
/// subsequent events flow to the new connection.
pub(crate) type ConnWriter = Arc<Mutex<File>>;

struct WorkloadEntry {
    pid: u32,
    /// (exit_code, signal) once the reap thread has collected the child.
    result: Option<(Option<i32>, Option<i32>)>,
}

/// The guestd core: executes requests from the host connection, tracks
/// workloads, and emits events back over the current connection.
pub(crate) struct Agent {
    workloads: Mutex<HashMap<String, WorkloadEntry>>,
    conn: Mutex<Option<ConnWriter>>,
    idle_monitor: Arc<IdleMonitor>,
}

impl Agent {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            workloads: Mutex::new(HashMap::new()),
            conn: Mutex::new(None),
            idle_monitor: Arc::new(IdleMonitor::new()),
        })
    }

    /// Dispatch one request from the host.
    pub(crate) fn handle_request(self: &Arc<Self>, request: Request) {
        match request {
            Request::Start {
                workload_id,
                cmd,
                env,
                cwd,
            } => self.start(workload_id, cmd, env, cwd),
            Request::Stop { workload_id } => self.stop(workload_id),
            Request::List => self.list(),
            Request::ConfigureAutoSuspend {
                idle_check_cmd,
                check_interval_secs,
            } => self
                .idle_monitor
                .configure(self, idle_check_cmd, check_interval_secs),
        }
    }

    /// Make `writer` the current host connection for outgoing events.
    pub(crate) fn set_conn(&self, writer: ConnWriter) {
        *self.conn.lock().unwrap() = Some(writer);
    }

    /// Clear the current connection, but only if it is still `writer`'s.
    pub(crate) fn clear_conn(&self, writer: &ConnWriter) {
        let mut conn = self.conn.lock().unwrap();
        if conn.as_ref().is_some_and(|c| Arc::ptr_eq(c, writer)) {
            *conn = None;
        }
    }

    /// Emit an `idle` event from the auto-suspend monitor (see monitor.rs).
    pub(crate) fn send_idle_event(&self) {
        self.send_event(&Event::Idle);
    }

    /// Serialize an event to the current host connection. Errors are
    /// swallowed: a dead connection is replaced by the accept loop, and
    /// hostd resyncs workload state with a `list` request after reconnecting.
    fn send_event(&self, event: &Event) {
        let mut line = match serde_json::to_string(event) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to serialize event");
                return;
            }
        };
        line.push('\n');
        let conn = self.conn.lock().unwrap().clone();
        if let Some(conn) = conn {
            let _ = conn.lock().unwrap().write_all(line.as_bytes());
        }
    }

    fn start(
        self: &Arc<Self>,
        workload_id: String,
        cmd: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<String>,
    ) {
        if cmd.is_empty() {
            self.send_event(&Event::Error {
                workload_id: Some(workload_id),
                message: "cmd must not be empty".to_string(),
            });
            return;
        }

        let mut command = Command::new(&cmd[0]);
        command
            .args(&cmd[1..])
            .envs(env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        // Put the child in its own process group so stop() can signal the
        // whole tree (e.g. `sh -c` plus its children) at once.
        // SAFETY: setpgid is async-signal-safe and runs in the child post-fork.
        unsafe {
            command.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.send_event(&Event::Error {
                    workload_id: Some(workload_id),
                    message: format!("spawn {}: {e}", cmd[0]),
                });
                return;
            }
        };

        let pid = child.id();
        self.workloads
            .lock()
            .unwrap()
            .insert(workload_id.clone(), WorkloadEntry { pid, result: None });
        self.send_event(&Event::Started {
            workload_id: workload_id.clone(),
            pid,
        });

        // Forward stdout/stderr chunks as output events.
        let pipes: [(Option<Box<dyn Read + Send>>, &'static str); 2] = [
            (child.stdout.take().map(|p| Box::new(p) as _), "stdout"),
            (child.stderr.take().map(|p| Box::new(p) as _), "stderr"),
        ];
        for (pipe, stream_name) in pipes {
            let Some(pipe) = pipe else { continue };
            let agent = Arc::clone(self);
            let workload_id = workload_id.clone();
            thread::spawn(move || agent.forward_output(pipe, workload_id, stream_name));
        }

        // Reap the child and report its exit status.
        let agent = Arc::clone(self);
        thread::spawn(move || agent.reap(child, workload_id));
    }

    fn stop(self: &Arc<Self>, workload_id: String) {
        let pid = {
            let workloads = self.workloads.lock().unwrap();
            match workloads.get(&workload_id) {
                Some(entry) if entry.result.is_none() => entry.pid,
                Some(_) => return, // already finished
                None => {
                    drop(workloads);
                    self.send_event(&Event::Error {
                        workload_id: Some(workload_id),
                        message: "unknown workload".to_string(),
                    });
                    return;
                }
            }
        };

        // Negative pid = the child's whole process group (see start()).
        let pgid = -(pid as i32);
        // SAFETY: kill with a valid signal number.
        unsafe { libc::kill(pgid, libc::SIGTERM) };

        // Escalate to SIGKILL if the workload is still running after the
        // grace period. The reap thread reports the exit either way.
        let agent = Arc::clone(self);
        thread::spawn(move || agent.escalate_kill(pgid, workload_id));
    }

    fn list(&self) {
        let workloads = self
            .workloads
            .lock()
            .unwrap()
            .iter()
            .map(|(workload_id, entry)| WorkloadInfo {
                workload_id: workload_id.clone(),
                state: if entry.result.is_some() {
                    "exited"
                } else {
                    "running"
                },
                exit_code: entry.result.and_then(|(code, _)| code),
                signal: entry.result.and_then(|(_, sig)| sig),
            })
            .collect();
        self.send_event(&Event::ListResult { workloads });
    }

    /// Read a child output pipe to EOF, forwarding chunks as output events.
    fn forward_output(
        &self,
        mut pipe: Box<dyn Read + Send>,
        workload_id: String,
        stream: &'static str,
    ) {
        let mut buf = [0u8; OUTPUT_BUF_SIZE];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.send_event(&Event::Output {
                    workload_id: workload_id.clone(),
                    stream,
                    data: String::from_utf8_lossy(&buf[..n]).into_owned(),
                }),
                Err(_) => break,
            }
        }
    }

    /// Wait for the child, record its result, and emit the exit event.
    fn reap(&self, mut child: Child, workload_id: String) {
        let (exit_code, signal) = match child.wait() {
            Ok(status) => (status.code(), status.signal()),
            Err(e) => {
                warn!(error = %e, workload_id, "wait failed");
                (None, None)
            }
        };
        if let Some(entry) = self.workloads.lock().unwrap().get_mut(&workload_id) {
            entry.result = Some((exit_code, signal));
        }
        self.send_event(&Event::Exited {
            workload_id,
            exit_code,
            signal,
        });
    }

    /// SIGKILL a workload's process group if it ignored the earlier SIGTERM.
    fn escalate_kill(&self, pgid: i32, workload_id: String) {
        thread::sleep(STOP_GRACE);
        let still_running = self
            .workloads
            .lock()
            .unwrap()
            .get(&workload_id)
            .is_some_and(|entry| entry.result.is_none());
        if still_running {
            // SAFETY: kill with a valid signal number.
            unsafe { libc::kill(pgid, libc::SIGKILL) };
        }
    }
}
