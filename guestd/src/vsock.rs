//! Minimal `AF_VSOCK` listener over libc (neither std nor tokio wraps vsock).

use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd};

/// Listen for connections from any CID (in practice always the host, CID 2).
const VMADDR_CID_ANY: u32 = u32::MAX;

/// Backlog passed to `listen`: one pending host connection is the norm; a
/// few spare slots cover reconnect bursts after a guest restore.
const LISTEN_BACKLOG: i32 = 4;

/// A bound vsock listening socket; closes the fd on drop.
pub(crate) struct VsockListener {
    /// The `File` owns the socket fd, so every error path after `socket()`
    /// closes it via `Drop`.
    socket: File,
}

impl VsockListener {
    #[allow(clippy::cast_possible_truncation)]
    // AF_VSOCK and size_of::<sockaddr_vm>() are small constants that always
    // fit sa_family_t (u16) and socklen_t (u32).
    pub(crate) fn bind(port: u32) -> io::Result<Self> {
        // SAFETY: all libc calls below use valid pointers and checked return
        // values; the fd is owned by `socket` from the moment it exists.
        unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let socket = File::from_raw_fd(fd);
            let addr = libc::sockaddr_vm {
                svm_family: libc::AF_VSOCK as libc::sa_family_t,
                svm_reserved1: 0,
                svm_port: port,
                svm_cid: VMADDR_CID_ANY,
                svm_zero: [0; 4],
            };
            let ret = libc::bind(
                socket.as_raw_fd(),
                (&raw const addr).cast::<libc::sockaddr>(),
                size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            );
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::listen(socket.as_raw_fd(), LISTEN_BACKLOG) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { socket })
        }
    }

    /// Accept one host connection, retrying on EINTR.
    pub(crate) fn accept(&self) -> io::Result<File> {
        loop {
            // SAFETY: the listener fd is a valid socket; null addr/ptr is
            // allowed.
            let fd = unsafe {
                libc::accept(
                    self.socket.as_raw_fd(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if fd >= 0 {
                // SAFETY: fd is a fresh, owned socket fd from accept().
                return Ok(unsafe { File::from_raw_fd(fd) });
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}
