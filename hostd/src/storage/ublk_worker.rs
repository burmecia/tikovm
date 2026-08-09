//! The `hostd ublk-worker` subprocess: serves one chunk-backed ublk block
//! device (`/dev/ublkbN`) for one VM volume. hostd spawns one worker per
//! volume (`storage::worker`), so a storage fault is isolated to a single
//! VM, and so a crashed worker can be respawned against the same device
//! (`UBLK_F_USER_RECOVERY` + `RECOVER_DEV`) without the VM noticing — both
//! properties validated by the ublk spike.
//!
//! IO model (spike-measured): one thread per ublk queue, a smol
//! LocalExecutor with one task per tag, and *blocking chunk IO inline* in
//! the task. Parking the task on `smol::unblock` starves the queue's
//! executor (only polled when the ublk ring wakes) and measured ~400 ms
//! per 4 KiB IO; io_uring per-chunk sqes (loop.rs-style) are the documented
//! future optimization if per-queue serialization becomes the bottleneck.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::helpers::IoBuf;
use libublk::io::{UblkDev, UblkQueue};
use libublk::{BufDesc, UblkError, UblkFlags};
use tracing::{error, info};

use super::volume::{DEFAULT_CHUNK_KB, Volume};
use crate::error::{Error, Result};

const IO_BUF_BYTES: u32 = 512 * 1024;

pub(crate) struct WorkerArgs {
    pub dir: PathBuf,
    /// Fresh volume: size. Mutually exclusive with `recover`.
    pub size_mb: Option<u64>,
    pub chunk_kb: Option<u32>,
    /// Reattach to an existing device left behind by a dead worker.
    pub recover: bool,
    /// Device id: -1 = kernel auto-allocation (add only); recover needs a
    /// concrete id.
    pub dev_id: i32,
    pub queues: u16,
    pub depth: u16,
}

async fn io_task(
    q: &UblkQueue<'_>,
    tag: u16,
    vol: Arc<Volume>,
) -> std::result::Result<(), UblkError> {
    let mut buf = IoBuf::<u8>::new(q.dev.dev_info.max_io_buf_bytes as usize);
    q.submit_io_prep_cmd(tag, BufDesc::Slice(buf.as_slice()), 0, Some(&buf))
        .await?;

    loop {
        let iod = q.get_iod(tag);
        let op = iod.op_flags & 0xff;
        let off = iod.start_sector << 9;
        let bytes = (iod.nr_sectors << 9) as usize;

        let res = match op {
            libublk::sys::UBLK_IO_OP_READ | libublk::sys::UBLK_IO_OP_WRITE => {
                let write = op == libublk::sys::UBLK_IO_OP_WRITE;
                match vol.do_io(write, off, &mut buf.as_mut_slice()[..bytes]) {
                    Ok(n) => n as i32,
                    Err(e) => {
                        error!(error = %e, off, bytes, write, "chunk io failed");
                        -libc::EIO
                    }
                }
            }
            libublk::sys::UBLK_IO_OP_FLUSH => match vol.flush() {
                Ok(()) => 0,
                Err(e) => {
                    error!(error = %e, "flush failed");
                    -libc::EIO
                }
            },
            // TRIM is a no-op: chunks are already sparse (missing = zero).
            libublk::sys::UBLK_IO_OP_DISCARD => 0,
            _ => -libc::EOPNOTSUPP,
        };

        q.submit_io_commit_cmd(tag, BufDesc::Slice(buf.as_slice()), res)
            .await?;
    }
}

fn queue_fn(qid: u16, dev: &UblkDev, depth: u16, vol: Arc<Volume>) {
    let q_rc = Rc::new(UblkQueue::new(qid, dev).expect("UblkQueue::new"));
    let exe_rc = Rc::new(smol::LocalExecutor::new());
    let exe = exe_rc.clone();
    let mut tasks = Vec::new();

    for tag in 0..depth {
        let q = q_rc.clone();
        let vol = vol.clone();
        tasks.push(exe.spawn(async move {
            match io_task(&q, tag, vol).await {
                Err(UblkError::QueueIsDown) | Ok(_) => {}
                Err(e) => error!(tag, error = %e, "io task failed"),
            }
        }));
    }

    smol::block_on(exe_rc.run(async move {
        let run_ops = || while exe.try_tick() {};
        let done = || tasks.iter().all(|task| task.is_finished());
        if let Err(e) = libublk::wait_and_handle_io_events(&q_rc, Some(20), run_ops, done).await {
            error!(error = %e, "wait_and_handle_io_events failed");
        }
    }));
}

/// Serve the device until it is deleted (or the process is killed).
/// Blocks forever; the parent hostd manages the process lifetime.
pub(crate) fn run(args: WorkerArgs) -> Result<()> {
    // Die with the parent hostd (its VMs die with it too, so a worker
    // must not outlive it). The getppid check covers the race where the
    // parent already exited before we got to install the death signal.
    // SAFETY: plain prctl/getppid calls with no pointers involved.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0);
    }
    if unsafe { libc::getppid() } == 1 {
        return Err(Error::storage("ublk-worker: parent hostd already gone"));
    }

    let vol = if args.recover {
        Volume::open(&args.dir)?
    } else {
        let size_mb = args
            .size_mb
            .ok_or_else(|| Error::storage("ublk-worker: --size-mb required without --recover"))?;
        Volume::create(
            &args.dir,
            size_mb << 20,
            u64::from(args.chunk_kb.unwrap_or(DEFAULT_CHUNK_KB)) << 10,
        )?
    };
    let size = vol.size_bytes();
    let vol = Arc::new(vol);

    let dev_flags = if args.recover {
        UblkFlags::UBLK_DEV_F_RECOVER_DEV
    } else {
        UblkFlags::UBLK_DEV_F_ADD_DEV
    };
    let ctrl = UblkCtrlBuilder::default()
        .name("tikovm_data")
        .id(args.dev_id)
        .nr_queues(args.queues)
        .depth(args.depth)
        .io_buf_bytes(IO_BUF_BYTES)
        .dev_flags(dev_flags)
        .ctrl_flags(libublk::sys::UBLK_F_USER_RECOVERY as u64)
        .build()
        .map_err(|e| Error::storage(format!("build ublk device: {e}")))?;
    let dev_id = ctrl.dev_info().dev_id;

    // Parsed by storage::worker::spawn — keep this exact format.
    println!("device id: {dev_id}");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    info!(dev_id, dir = %args.dir.display(), "ublk worker serving device");

    let tgt_init = move |dev: &mut UblkDev| {
        dev.set_default_params(size);
        Ok(())
    };
    let vol2 = vol.clone();
    ctrl.run_target(
        tgt_init,
        move |qid, dev: &_| queue_fn(qid, dev, args.depth, vol2.clone()),
        move |c: &UblkCtrl| {
            info!(dev_id = c.dev_info().dev_id, "ublk worker: device stopped");
        },
    )
    .map_err(|e| Error::storage(format!("run ublk target: {e}")))?;
    Ok(())
}
