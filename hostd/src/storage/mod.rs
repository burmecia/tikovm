//! Per-VM block storage: optional dedicated block volumes backed by
//! chunk files on the storage root (AWS S3 Files, NFS-mounted, in
//! production; any directory in tests).
//!
//! Architecture: hostd spawns one `ublk-worker` subprocess per volume
//! (`ublk_worker` — hostd re-executing itself with a hidden subcommand),
//! which serves a `/dev/ublkbN` block device via the kernel ublk driver.
//! All IO lands in `volume`, which maps byte ranges onto fixed-size chunk
//! files (sparse: missing chunk = zeros). The device is attached to the VM
//! as an extra virtio-block drive (`/dev/vdc`), formatted ext4 by hostd at
//! volume creation, and mounted in the guest by a systemd unit seeded into
//! the overlay disk (`vmm::firecracker::setup::seed_overlay_disk`).
//!
//! Lifecycle: volume and worker are created in `create_vm` and destroyed
//! in `destroy_vm` (`manager`); a volume dies with its VM. Snapshot /
//! restore need no storage action: the worker (and device) are independent
//! of the Firecracker process, and a restored VM re-opens the same device
//! path.
//!
//! Durability contract (spike-validated): a completed FLUSH (guest fsync)
//! means all dirty chunks were fdatasynced — on S3 Files, one NFS COMMIT
//! per dirty chunk, ~9 ms p50. A worker crash fails in-flight IOs with
//! EIO (guest ext4 replays its journal, as after a disk power blip) and
//! the device is transparently recovered by a respawned worker.

pub(crate) mod manager;
pub(crate) mod ublk_worker;
pub(crate) mod volume;
pub(crate) mod worker;

pub(crate) use manager::StorageManager;
pub(crate) use volume::ALLOWED_CHUNK_KB;
