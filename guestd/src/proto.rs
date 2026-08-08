//! The guestd control protocol: newline-delimited JSON, both directions.
//!
//!   host -> guest: {"`type":"start","workload_id":..,"cmd"`:[..],"env":{..},"cwd":..}
//!                  {"`type":"stop","workload_id"`:..}
//!                  {"type":"list"}
//!                  {"`type":"configure_auto_suspend","idle_check_cmd"`:[..],"`check_interval_secs"`:..}
//!   guest -> host: {"`type":"started","workload_id":..,"pid"`:..}
//!                  {"`type":"output","workload_id":..,"stream":"stdout|stderr","data"`:..}
//!                  {"`type":"exited","workload_id":..,"exit_code":..,"signal"`:..}
//!                  {"`type":"error","workload_id":..,"message"`:..}
//!                  {"`type":"list_result","workloads"`:[..]}
//!                  {"type":"idle"}

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Request {
    Start {
        workload_id: String,
        cmd: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
        cwd: Option<String>,
    },
    Stop {
        workload_id: String,
    },
    List,
    /// Configure the guest-side auto-suspend idle detector: run
    /// `idle_check_cmd` every `check_interval_secs` and report `idle` when it
    /// exits 0. An empty `idle_check_cmd` disables the detector.
    ConfigureAutoSuspend {
        idle_check_cmd: Vec<String>,
        check_interval_secs: u64,
    },
}

/// Output stream of a workload process (wire format: "stdout" | "stderr").
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Stream {
    Stdout,
    Stderr,
}

/// Lifecycle state reported in `WorkloadInfo`: `Running` while the process
/// lives, `Exited` once reaped (wire format: "running" | "exited").
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkloadState {
    Running,
    Exited,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum Event {
    Started {
        workload_id: String,
        pid: u32,
    },
    Output {
        workload_id: String,
        stream: Stream,
        data: String,
    },
    Exited {
        workload_id: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    Error {
        workload_id: Option<String>,
        message: String,
    },
    ListResult {
        workloads: Vec<WorkloadInfo>,
    },
    /// The auto-suspend idle detector reported the guest as idle. hostd
    /// decides whether to actually suspend the VM.
    Idle,
}

#[derive(Debug, Serialize)]
pub(crate) struct WorkloadInfo {
    pub(crate) workload_id: String,
    pub(crate) state: WorkloadState,
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<i32>,
}
