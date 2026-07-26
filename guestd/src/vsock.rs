//! Minimal AF_VSOCK listener over libc (neither std nor tokio wraps vsock).

use std::fs::File;
use std::io;
use std::os::unix::io::{FromRawFd, RawFd};

/// Listen for connections from any CID (in practice always the host, CID 2).
const VMADDR_CID_ANY: u32 = u32::MAX;

/// A bound vsock listening socket; closes the fd on drop.
pub(crate) struct VsockListener {
    fd: RawFd,
}

impl VsockListener {
    pub(crate) fn bind(port: u32) -> io::Result<Self> {
        // SAFETY: all libc calls below use valid pointers and checked return
        // values; on any error the fd is closed before returning.
        unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let addr = libc::sockaddr_vm {
                svm_family: libc::AF_VSOCK as libc::sa_family_t,
                svm_reserved1: 0,
                svm_port: port,
                svm_cid: VMADDR_CID_ANY,
                svm_zero: [0; 4],
            };
            let ret = libc::bind(
                fd,
                &addr as *const libc::sockaddr_vm as *const libc::sockaddr,
                size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            );
            if ret < 0 {
                let e = io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            if libc::listen(fd, 4) < 0 {
                let e = io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            Ok(Self { fd })
        }
    }

    /// Accept one host connection, retrying on EINTR.
    pub(crate) fn accept(&self) -> io::Result<File> {
        loop {
            // SAFETY: self.fd is a valid socket fd; null addr/ptr is allowed.
            let fd = unsafe { libc::accept(self.fd, std::ptr::null_mut(), std::ptr::null_mut()) };
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

impl Drop for VsockListener {
    fn drop(&mut self) {
        // SAFETY: self.fd is owned by this listener and closed exactly once.
        unsafe { libc::close(self.fd) };
    }
}
