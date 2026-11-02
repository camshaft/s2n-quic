// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(unix)]
use s2n_quic_core::ensure;
#[cfg(unix)]
use std::io;

pub mod udp;
#[cfg(windows)]
pub mod windows;

/// The OS handle of a socket: a file descriptor on Unix, a `SOCKET` on Windows.
#[cfg(unix)]
pub type Raw = std::os::fd::RawFd;

/// The OS handle of a socket: a file descriptor on Unix, a `SOCKET` on Windows.
#[cfg(windows)]
pub type Raw = std::os::windows::io::RawSocket;

/// A socket whose OS handle the send and receive calls of this module take.
///
/// On Unix every [`AsRawFd`](std::os::fd::AsRawFd) type is one. On Windows the socket types this
/// crate sends and receives on implement it.
pub trait AsRaw {
    fn as_raw(&self) -> Raw;
}

#[cfg(unix)]
impl<T: std::os::fd::AsRawFd + ?Sized> AsRaw for T {
    #[inline]
    fn as_raw(&self) -> Raw {
        std::os::fd::AsRawFd::as_raw_fd(self)
    }
}

#[cfg(windows)]
mod as_raw {
    use super::{AsRaw, Raw};
    use std::os::windows::io::AsRawSocket;

    impl AsRaw for std::net::UdpSocket {
        #[inline]
        fn as_raw(&self) -> Raw {
            self.as_raw_socket()
        }
    }

    impl AsRaw for tokio::net::UdpSocket {
        #[inline]
        fn as_raw(&self) -> Raw {
            self.as_raw_socket()
        }
    }

    impl<T: AsRaw + ?Sized> AsRaw for std::sync::Arc<T> {
        #[inline]
        fn as_raw(&self) -> Raw {
            (**self).as_raw()
        }
    }

    impl<T: AsRaw + ?Sized> AsRaw for Box<T> {
        #[inline]
        fn as_raw(&self) -> Raw {
            (**self).as_raw()
        }
    }
}

#[cfg(unix)]
pub type Flags = libc::c_int;

#[cfg(windows)]
pub type Flags = i32;

#[cfg(unix)]
#[inline]
pub fn peek<T>(fd: &T) -> io::Result<usize>
where
    T: AsRaw,
{
    libc_call(|| unsafe {
        let flags = libc::MSG_PEEK | libc::MSG_TRUNC;

        // macos doesn't seem to support MSG_TRUNC so we need to give it at least 1 byte
        if cfg!(target_os = "macos") {
            let mut buf = [0u8];
            libc::recv(fd.as_raw(), buf.as_mut_ptr() as *mut _, 1, flags) as _
        } else {
            libc::recv(fd.as_raw(), core::ptr::null_mut(), 0, flags) as _
        }
    })
}

#[cfg(windows)]
pub use windows::peek;

#[cfg(unix)]
#[inline]
pub fn libc_call(call: impl FnOnce() -> isize) -> io::Result<usize> {
    let res = call();

    ensure!(res >= 0, Err(io::Error::last_os_error()));

    Ok(res as _)
}
