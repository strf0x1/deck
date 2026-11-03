//! The guest's AF_VSOCK connector.
//!
//! The design choice: only a Linux guest has AF_VSOCK, so every other host
//! builds this crate and gets an explicit "not available" error instead of a
//! silent fallback to TCP. The relay is only ever installed in the guest
//! image; the host build exists so the workspace checks cover it.

use std::io;
use std::os::unix::net::UnixStream;

/// The host's context id in a libkrun guest.
pub const HOST_CID: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub cid: u32,
    pub port: u32,
}

impl Address {
    /// `cid:port`, the shape the guest entry and the VMM config both use.
    pub fn parse(value: &str) -> Result<Self, String> {
        let (cid, port) = value.split_once(':').ok_or_else(|| format!("{value:?} is not cid:port"))?;
        let cid = cid.parse().map_err(|_| format!("{cid:?} is not a context id"))?;
        let port = port.parse().map_err(|_| format!("{port:?} is not a vsock port"))?;
        Ok(Self { cid, port })
    }
}

#[cfg(target_os = "linux")]
pub fn connect(address: Address) -> io::Result<UnixStream> {
    use std::os::fd::FromRawFd as _;

    // SAFETY: socket(2) with the AF_VSOCK family and a plain stream type.
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let name = libc::sockaddr_vm {
        svm_family: libc::AF_VSOCK as libc::sa_family_t,
        svm_reserved1: 0,
        svm_port: address.port,
        svm_cid: address.cid,
        svm_zero: [0; 4],
    };
    // SAFETY: `name` is an initialised sockaddr_vm and the length is its size.
    let rc = unsafe {
        libc::connect(fd, std::ptr::addr_of!(name).cast(), std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t)
    };
    if rc < 0 {
        let error = io::Error::last_os_error();
        // SAFETY: `fd` is owned by this function and not used after this.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    // SAFETY: `fd` is a connected, owned stream socket.
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

#[cfg(not(target_os = "linux"))]
pub fn connect(address: Address) -> io::Result<UnixStream> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("AF_VSOCK needs the Linux guest; this host cannot reach cid {} port {}", address.cid, address.port),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cid_and_port() {
        assert_eq!(Address::parse("2:1026").unwrap(), Address { cid: 2, port: 1026 });
        assert!(Address::parse("2").is_err());
        assert!(Address::parse("host:1026").is_err());
        assert!(Address::parse("2:port").is_err());
    }
}
