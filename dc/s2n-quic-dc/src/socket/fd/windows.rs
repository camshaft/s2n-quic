// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The Windows socket layer: the `recv`, `send` and `peek` calls of [`super::udp`] on Winsock's
//! `WSARecvMsg` and `WSASendMsg`.
//!
//! - Addresses: a receive writes each datagram's source address into the message's name, as
//!   `recvmsg` does on Unix, and a send takes its destination from the name.
//! - ECN: [`configure`] asks Winsock for each datagram's ECN bits (`IP_RECVECN` and
//!   `IPV6_RECVECN`), which a receive reads from the `IP_ECN` or `IPV6_ECN` control message. A send
//!   marks its datagram with the same control message.
//! - Batching: [`configure`] turns on receive coalescing (`UDP_RECV_MAX_COALESCED_SIZE`), the
//!   counterpart of Linux's GRO: a receive reads the segment size of a coalesced message from its
//!   `UDP_COALESCED_INFO` control message. A send of more than one segment sets its segment size
//!   with a `UDP_SEND_MSG_SIZE` control message, the counterpart of Linux's GSO.
//!
//! A Windows build that doesn't support one of these options refuses it: [`configure`] returns a
//! [`Support`] value for each option, and a send whose ECN marking the build refuses goes out
//! unmarked.

use super::{AsRaw, Flags};
use crate::msg::{addr::Addr, cmsg};
use core::{
    mem, ptr,
    sync::atomic::{AtomicBool, Ordering},
};
use s2n_quic_core::inet::{ExplicitCongestionNotification, SocketAddress};
use std::{
    cell::RefCell,
    io::{self, IoSlice, IoSliceMut},
    sync::OnceLock,
};
use windows_sys::Win32::Networking::WinSock;

// The levels and names of the socket options and control messages this module reads and writes.
const IPPROTO_IP: i32 = WinSock::IPPROTO_IP as i32;
const IPPROTO_IPV6: i32 = WinSock::IPPROTO_IPV6 as i32;
const IPPROTO_UDP: i32 = WinSock::IPPROTO_UDP as i32;
const IP_ECN: i32 = WinSock::IP_ECN as i32;
const IPV6_ECN: i32 = WinSock::IPV6_ECN as i32;
const IP_RECVECN: i32 = WinSock::IP_RECVECN as i32;
const IPV6_RECVECN: i32 = WinSock::IPV6_RECVECN as i32;
const UDP_SEND_MSG_SIZE: i32 = WinSock::UDP_SEND_MSG_SIZE as i32;
const UDP_RECV_MAX_COALESCED_SIZE: i32 = WinSock::UDP_RECV_MAX_COALESCED_SIZE as i32;
const UDP_COALESCED_INFO: i32 = WinSock::UDP_COALESCED_INFO as i32;

/// `SIO_UDP_CONNRESET` of `mswsock.h`: `_WSAIOW(IOC_VENDOR, 12)`.
const SIO_UDP_CONNRESET: u32 = 0x9800_000C;

/// Whether a socket option is on, as [`configure`] set it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// The option is on.
    Enabled,
    /// The Windows build refused the option, with this Winsock error code.
    Unsupported(i32),
}

impl Support {
    #[inline]
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Enabled)
    }
}

/// The options [`configure`] set on a socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Features {
    /// Each received datagram's ECN bits come back with it.
    pub ecn: Support,
    /// Receive coalescing, which hands a receive several datagrams of one size at once.
    pub recv_coalescing: Support,
    /// An ICMP port unreachable for an earlier send doesn't fail the next receive with
    /// `WSAECONNRESET`, as it does by default on Windows.
    pub ignore_connreset: Support,
}

/// Sets the options this module's receives and sends read on `socket`, and returns which of them
/// the Windows build took.
///
/// `recv_coalesced_size` is the largest message receive coalescing may hand a receive: each receive
/// buffer must hold at least that many bytes, as each buffer must hold a GRO message on Linux.
pub fn configure<T: AsRaw + ?Sized>(socket: &T, recv_coalesced_size: u32) -> Features {
    let socket = socket.as_raw() as WinSock::SOCKET;

    // A socket takes the option of its own family: an IPv4 socket refuses the IPv6 one, and a
    // dual-stack IPv6 socket takes both.
    let ipv4 = set_option(socket, IPPROTO_IP, IP_RECVECN, 1);
    let ipv6 = set_option(socket, IPPROTO_IPV6, IPV6_RECVECN, 1);
    let ecn = if ipv6.is_enabled() { ipv6 } else { ipv4 };

    let recv_coalescing = set_option(
        socket,
        IPPROTO_UDP,
        UDP_RECV_MAX_COALESCED_SIZE,
        recv_coalesced_size,
    );

    let ignore_connreset = disable_connreset(socket);

    Features {
        ecn,
        recv_coalescing,
        ignore_connreset,
    }
}

fn set_option(socket: WinSock::SOCKET, level: i32, name: i32, value: u32) -> Support {
    // SAFETY: `value` is a readable `u32` for the length the call states.
    let res = unsafe {
        WinSock::setsockopt(
            socket,
            level,
            name,
            ptr::addr_of!(value).cast(),
            mem::size_of::<u32>() as i32,
        )
    };

    if res == WinSock::SOCKET_ERROR {
        Support::Unsupported(last_error_code())
    } else {
        Support::Enabled
    }
}

fn disable_connreset(socket: WinSock::SOCKET) -> Support {
    // `FALSE`: don't report an ICMP port unreachable on a later receive
    let value: u32 = 0;
    let mut returned = 0u32;
    // SAFETY: `value` is a readable `u32` for the length the call states, the call doesn't write an
    // output buffer, and `returned` is a writable `u32`.
    let res = unsafe {
        WinSock::WSAIoctl(
            socket,
            SIO_UDP_CONNRESET,
            ptr::addr_of!(value).cast(),
            mem::size_of::<u32>() as u32,
            ptr::null_mut(),
            0,
            &mut returned,
            ptr::null_mut(),
            None,
        )
    };

    if res == WinSock::SOCKET_ERROR {
        Support::Unsupported(last_error_code())
    } else {
        Support::Enabled
    }
}

/// Receives one message into `buffer`, and writes its source address into `addr` and its ECN bits
/// and coalesced segment size into `cmsg`.
#[inline]
pub fn recv<T>(
    socket: &T,
    addr: &mut Addr,
    cmsg: &mut cmsg::Receiver,
    buffer: &mut [IoSliceMut],
    flags: Flags,
) -> io::Result<usize>
where
    T: AsRaw + ?Sized,
{
    debug_assert!(!buffer.is_empty());

    let socket = socket.as_raw() as WinSock::SOCKET;
    let Some(recvmsg) = wsarecvmsg(socket)? else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the socket's provider doesn't offer WSARecvMsg",
        ));
    };

    let mut control = Control::<{ cmsg::DECODER_LEN }>::default();

    // SAFETY: an all-zero `WSAMSG` is valid: null pointers with zero lengths.
    let mut msg: WinSock::WSAMSG = unsafe { mem::zeroed() };
    msg.name = addr.name_mut_ptr().cast();
    msg.namelen = addr.name_capacity() as i32;
    // `IoSliceMut` is ABI compatible with `WSABUF` on Windows.
    msg.lpBuffers = buffer.as_mut_ptr().cast();
    msg.dwBufferCount = buffer.len() as u32;
    msg.Control = WinSock::WSABUF {
        len: control.0.len() as u32,
        buf: control.0.as_mut_ptr(),
    };
    msg.dwFlags = flags as u32;

    let mut len = 0u32;
    // SAFETY: each pointer in `msg` points into a live, writable buffer of the length it states,
    // and `len` is a writable `u32`. The call is synchronous, since it doesn't pass an `OVERLAPPED`.
    let res = unsafe { recvmsg(socket, &mut msg, &mut len, ptr::null_mut(), None) };

    if res == WinSock::SOCKET_ERROR {
        return Err(last_error());
    }

    addr.update_with_len(msg.namelen as usize);
    decode_control(&msg, cmsg);

    Ok(len as usize)
}

/// Sends `buffer` to `addr`, marked with `ecn`, as one datagram, or as datagrams of
/// `segment_size` bytes each where the send holds more than one segment.
#[inline]
pub fn send<T>(
    socket: &T,
    addr: &Addr,
    ecn: ExplicitCongestionNotification,
    buffer: &[IoSlice],
    segment_size: Option<u16>,
    flags: Flags,
) -> io::Result<usize>
where
    T: AsRaw + ?Sized,
{
    debug_assert!(!buffer.is_empty());

    // make sure we constructed a valid iovec
    #[cfg(debug_assertions)]
    super::udp::check_send_iovec(buffer);

    let socket = socket.as_raw() as WinSock::SOCKET;

    // As the Unix layer reads it: an explicit segment size of 0 means a single segment, and with
    // no explicit size a send of several segments takes the first segment's length.
    let segment_size = match segment_size {
        Some(size) => size,
        None if buffer.len() > 1 => buffer[0].len() as u16,
        None => 0,
    };

    let mut mark = ecn != ExplicitCongestionNotification::NotEct
        && !SEND_ECN_REFUSED.load(Ordering::Relaxed);

    loop {
        let mut control = Control::<{ cmsg::ENCODER_LEN }>::default();

        let control_len = {
            let mut encoder = Encoder::new(&mut control);

            if mark {
                let (level, ty) = if is_ipv4(addr) {
                    (IPPROTO_IP, IP_ECN)
                } else {
                    (IPPROTO_IPV6, IPV6_ECN)
                };
                encoder.push(level, ty, ecn as u8 as u32);
            }

            if segment_size > 0 {
                encoder.push(IPPROTO_UDP, UDP_SEND_MSG_SIZE, segment_size as u32);
            }

            encoder.len
        };

        // SAFETY: an all-zero `WSAMSG` is valid: null pointers with zero lengths.
        let mut msg: WinSock::WSAMSG = unsafe { mem::zeroed() };
        // Winsock reads the name and the buffers of a send and doesn't write them.
        msg.name = addr.name_ptr() as *mut WinSock::SOCKADDR;
        msg.namelen = addr.name_len() as i32;
        // `IoSlice` is ABI compatible with `WSABUF` on Windows.
        msg.lpBuffers = buffer.as_ptr() as *mut WinSock::WSABUF;
        msg.dwBufferCount = buffer.len() as u32;
        if control_len > 0 {
            msg.Control = WinSock::WSABUF {
                len: control_len as u32,
                buf: control.0.as_mut_ptr(),
            };
        }

        let mut sent = 0u32;
        // SAFETY: each pointer in `msg` points into a live buffer of the length it states, and
        // `sent` is a writable `u32`. The call is synchronous, since it doesn't pass an `OVERLAPPED`.
        let res = unsafe {
            WinSock::WSASendMsg(
                socket,
                &msg,
                flags as u32,
                &mut sent,
                ptr::null_mut(),
                None,
            )
        };

        if res != WinSock::SOCKET_ERROR {
            return Ok(sent as usize);
        }

        let code = last_error_code();

        // A Windows build that doesn't support ECN on sends refuses the control message with
        // `WSAEINVAL`: send this datagram, and every later one, unmarked.
        if mark && code == WinSock::WSAEINVAL as i32 {
            SEND_ECN_REFUSED.store(true, Ordering::Relaxed);
            mark = false;
            continue;
        }

        return Err(io::Error::from_raw_os_error(code));
    }
}

/// Returns the length of the next datagram without taking it off the socket.
#[inline]
pub fn peek<T>(socket: &T) -> io::Result<usize>
where
    T: AsRaw + ?Sized,
{
    // Winsock's `MSG_PEEK` doesn't take `MSG_TRUNC`, and a peek into a buffer smaller than the
    // datagram fails without its length, so peek into a buffer that holds the largest datagram.
    thread_local! {
        static BUFFER: RefCell<Box<[u8]>> =
            RefCell::new(vec![0u8; u16::MAX as usize].into_boxed_slice());
    }

    let socket = socket.as_raw() as WinSock::SOCKET;

    BUFFER.with(|buffer| {
        let mut buffer = buffer.borrow_mut();
        // SAFETY: `buffer` is a writable buffer of the length the call states.
        let res = unsafe {
            WinSock::recv(
                socket,
                buffer.as_mut_ptr(),
                buffer.len() as i32,
                WinSock::MSG_PEEK as _,
            )
        };

        if res == WinSock::SOCKET_ERROR {
            Err(last_error())
        } else {
            Ok(res as usize)
        }
    })
}

/// Set once a send's ECN control message is refused: later sends go out unmarked.
static SEND_ECN_REFUSED: AtomicBool = AtomicBool::new(false);

/// `WSARecvMsg`, which Winsock hands out as an extension function of the socket's provider.
static WSARECVMSG: OnceLock<WinSock::LPFN_WSARECVMSG> = OnceLock::new();

fn wsarecvmsg(socket: WinSock::SOCKET) -> io::Result<WinSock::LPFN_WSARECVMSG> {
    if let Some(function) = WSARECVMSG.get() {
        return Ok(*function);
    }

    let guid = WinSock::WSAID_WSARECVMSG;
    let mut function: WinSock::LPFN_WSARECVMSG = None;
    let mut returned = 0u32;

    // SAFETY: `guid` is a readable GUID and `function` a writable function pointer, each of the
    // length the call states, and `returned` is a writable `u32`.
    let res = unsafe {
        WinSock::WSAIoctl(
            socket,
            WinSock::SIO_GET_EXTENSION_FUNCTION_POINTER,
            ptr::addr_of!(guid).cast(),
            mem::size_of_val(&guid) as u32,
            ptr::addr_of_mut!(function).cast(),
            mem::size_of_val(&function) as u32,
            &mut returned,
            ptr::null_mut(),
            None,
        )
    };

    if res == WinSock::SOCKET_ERROR {
        return Err(last_error());
    }

    if function.is_some() {
        let _ = WSARECVMSG.set(function);
    }

    Ok(function)
}

/// Whether a send to `addr` goes out as IPv4, which takes the `IPPROTO_IP` level for its control
/// messages: an IPv4 address, or an IPv4-mapped IPv6 address on a dual-stack socket.
#[inline]
fn is_ipv4(addr: &Addr) -> bool {
    match addr.get() {
        SocketAddress::IpV4(_) => true,
        SocketAddress::IpV6(addr) => {
            let octets: [u8; 16] = (*addr.ip()).into();
            octets[..12] == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff]
        }
    }
}

#[inline]
fn last_error_code() -> i32 {
    // SAFETY: `WSAGetLastError` only reads the calling thread's last error.
    unsafe { WinSock::WSAGetLastError() as i32 }
}

#[inline]
fn last_error() -> io::Error {
    io::Error::from_raw_os_error(last_error_code())
}

// The layout of a control message, as the `WSA_CMSG_*` macros of `ws2def.h` lay it out: a
// `CMSGHDR`, its data at the header's length rounded up to the natural alignment, and the next
// message at the message's length rounded up to the header's alignment.

const HEADER_LEN: usize = mem::size_of::<WinSock::CMSGHDR>();
const HEADER_ALIGN: usize = mem::align_of::<WinSock::CMSGHDR>();
/// `MAX_NATURAL_ALIGNMENT`: the size of a pointer.
const NATURAL_ALIGN: usize = mem::size_of::<usize>();

/// `WSA_CMSGHDR_ALIGN`
#[inline]
const fn align_header(len: usize) -> usize {
    (len + HEADER_ALIGN - 1) & !(HEADER_ALIGN - 1)
}

/// `WSA_CMSGDATA_ALIGN`
#[inline]
const fn align_data(len: usize) -> usize {
    (len + NATURAL_ALIGN - 1) & !(NATURAL_ALIGN - 1)
}

/// The offset of a message's data from its header: `WSA_CMSG_DATA`.
const DATA_OFFSET: usize = align_data(HEADER_LEN);

/// The bytes a message of `len` data bytes takes, with its padding: `WSA_CMSG_SPACE`.
#[inline]
const fn space(len: usize) -> usize {
    align_data(HEADER_LEN + align_header(len))
}

/// A control message buffer, aligned for its headers.
#[repr(C, align(8))]
struct Control<const N: usize>([u8; N]);

impl<const N: usize> Default for Control<N> {
    #[inline]
    fn default() -> Self {
        Self([0; N])
    }
}

/// Writes control messages of 4-byte values into a [`Control`] buffer.
struct Encoder<'a, const N: usize> {
    control: &'a mut Control<N>,
    len: usize,
}

impl<'a, const N: usize> Encoder<'a, N> {
    #[inline]
    fn new(control: &'a mut Control<N>) -> Self {
        Self { control, len: 0 }
    }

    /// Appends a message of `value`; returns false, and appends nothing, where it doesn't fit.
    #[inline]
    fn push(&mut self, level: i32, ty: i32, value: u32) -> bool {
        let space = space(mem::size_of::<u32>());

        if self.len + space > N {
            return false;
        }

        let header = WinSock::CMSGHDR {
            cmsg_len: DATA_OFFSET + mem::size_of::<u32>(),
            cmsg_level: level,
            cmsg_type: ty,
        };

        // SAFETY: the message's `space` bytes from `self.len` lie within the buffer, as checked
        // above, and each write may be unaligned.
        unsafe {
            let message = self.control.0.as_mut_ptr().add(self.len);
            ptr::write_unaligned(message.cast::<WinSock::CMSGHDR>(), header);
            ptr::write_unaligned(message.add(DATA_OFFSET).cast::<u32>(), value);
        }

        self.len += space;

        true
    }
}

/// Reads the ECN bits and the coalesced segment size of a received message from its control
/// messages.
#[inline]
fn decode_control(msg: &WinSock::WSAMSG, cmsg: &mut cmsg::Receiver) {
    // assume we didn't get a coalesced message initially
    cmsg.set_segment_len(0);

    let base = msg.Control.buf as *const u8;
    // a receive sets the length to the bytes of control messages it wrote
    let len = msg.Control.len as usize;

    if base.is_null() {
        return;
    }

    let mut offset = 0;

    while offset + HEADER_LEN <= len {
        // SAFETY: the header's bytes lie within the control buffer, as the loop's condition checks,
        // and the read may be unaligned.
        let header = unsafe { ptr::read_unaligned(base.add(offset).cast::<WinSock::CMSGHDR>()) };

        let message_len = header.cmsg_len;

        if message_len < DATA_OFFSET || offset + message_len > len {
            break;
        }

        // SAFETY: the message's data lies within the control buffer, as checked above.
        let data = unsafe {
            core::slice::from_raw_parts(base.add(offset + DATA_OFFSET), message_len - DATA_OFFSET)
        };

        match (header.cmsg_level, header.cmsg_type) {
            (IPPROTO_IP, IP_ECN) | (IPPROTO_IPV6, IPV6_ECN) => {
                if let Some(value) = decode_value(data) {
                    cmsg.set_ecn(ExplicitCongestionNotification::new(value as u8));
                }
            }
            (IPPROTO_UDP, UDP_COALESCED_INFO) => {
                if let Some(value) = decode_value(data) {
                    cmsg.set_segment_len(value.min(u16::MAX as u32) as u16);
                }
            }
            _ => {}
        }

        offset += align_header(message_len);
    }
}

/// Reads a control message's value: a `DWORD` or an `INT`, or a single byte.
#[inline]
fn decode_value(data: &[u8]) -> Option<u32> {
    match data.len() {
        1 => Some(data[0] as u32),
        4 => Some(u32::from_ne_bytes(data.try_into().ok()?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{SocketAddr, UdpSocket};

    fn pair(addr: &str) -> (UdpSocket, UdpSocket) {
        let a = UdpSocket::bind(addr).unwrap();
        let b = UdpSocket::bind(addr).unwrap();
        (a, b)
    }

    fn recv_one(socket: &UdpSocket, len: usize) -> (usize, Addr, cmsg::Receiver, Vec<u8>) {
        let mut payload = vec![0u8; len];
        let mut addr = Addr::default();
        let mut cmsg = cmsg::Receiver::default();
        let len = {
            let mut iov = [IoSliceMut::new(&mut payload)];
            recv(socket, &mut addr, &mut cmsg, &mut iov, 0).unwrap()
        };
        (len, addr, cmsg, payload)
    }

    /// A datagram sent with `send` arrives whole at `recv`, which reports the sender's address.
    #[test]
    fn send_and_recv_report_the_peer_address() {
        for local in ["127.0.0.1:0", "[::1]:0"] {
            let (a, b) = pair(local);
            configure(&b, u16::MAX as u32);

            let to: SocketAddr = b.local_addr().unwrap();
            let to = Addr::new(to.into());
            let payload = b"hello from windows";
            let iov = [IoSlice::new(payload)];
            let sent = send(
                &a,
                &to,
                ExplicitCongestionNotification::NotEct,
                &iov,
                None,
                0,
            )
            .unwrap();
            assert_eq!(sent, payload.len());

            let (len, from, cmsg, received) = recv_one(&b, 1500);
            assert_eq!(len, payload.len());
            assert_eq!(&received[..len], payload);
            let from: SocketAddr = from.get().into();
            assert_eq!(from, a.local_addr().unwrap());
            assert_eq!(cmsg.segment_len(), 0);
        }
    }

    /// Where the build takes ECN on both ends, a send's ECN marking comes back with the datagram;
    /// where it doesn't, the datagram still arrives.
    #[test]
    fn ecn_marking_comes_back_where_supported() {
        let (a, b) = pair("127.0.0.1:0");
        let features = configure(&b, u16::MAX as u32);

        let to = Addr::new(b.local_addr().unwrap().into());
        let payload = [7u8; 32];
        let iov = [IoSlice::new(&payload)];
        send(&a, &to, ExplicitCongestionNotification::Ect0, &iov, None, 0).unwrap();

        let (len, _from, cmsg, _received) = recv_one(&b, 1500);
        assert_eq!(len, payload.len());

        if features.ecn.is_enabled() && !SEND_ECN_REFUSED.load(Ordering::Relaxed) {
            assert_eq!(cmsg.ecn(), ExplicitCongestionNotification::Ect0);
        }
    }

    /// `peek` reports the length of the next datagram and leaves it on the socket.
    #[test]
    fn peek_reports_the_next_datagram_length() {
        let (a, b) = pair("127.0.0.1:0");
        let payload = [1u8; 3000];
        a.send_to(&payload, b.local_addr().unwrap()).unwrap();

        assert_eq!(peek(&b).unwrap(), payload.len());

        let (len, _from, _cmsg, _received) = recv_one(&b, 4000);
        assert_eq!(len, payload.len());
    }

    /// The decoder reads an ECN and a coalesced-size message, as a receive lays them out.
    #[test]
    fn decode_reads_ecn_and_coalesced_size() {
        let mut control = Control::<{ cmsg::DECODER_LEN }>::default();
        let len = {
            let mut encoder = Encoder::new(&mut control);
            assert!(encoder.push(IPPROTO_IPV6, IPV6_ECN, 0b11));
            assert!(encoder.push(IPPROTO_UDP, UDP_COALESCED_INFO, 1200));
            encoder.len
        };

        // SAFETY: an all-zero `WSAMSG` is valid.
        let mut msg: WinSock::WSAMSG = unsafe { mem::zeroed() };
        msg.Control = WinSock::WSABUF {
            len: len as u32,
            buf: control.0.as_mut_ptr(),
        };

        let mut cmsg = cmsg::Receiver::default();
        decode_control(&msg, &mut cmsg);

        assert_eq!(cmsg.ecn(), ExplicitCongestionNotification::Ce);
        assert_eq!(cmsg.segment_len(), 1200);
    }

    /// The encoder stops at the end of its buffer and writes nothing past it.
    #[test]
    fn encoder_refuses_a_message_past_the_buffer() {
        const N: usize = 2 * space(4);
        let mut control = Control::<N>::default();
        let mut encoder = Encoder::new(&mut control);
        assert!(encoder.push(IPPROTO_IP, IP_ECN, 1));
        assert!(encoder.push(IPPROTO_UDP, UDP_SEND_MSG_SIZE, 1200));
        assert!(!encoder.push(IPPROTO_UDP, UDP_SEND_MSG_SIZE, 1200));
        assert_eq!(encoder.len, N);
    }
}
