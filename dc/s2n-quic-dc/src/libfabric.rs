use bitflags::bitflags;
use core::{ffi::CStr, fmt, ptr::NonNull};
use libc::iovec;
use ofi_libfabric_sys::bindgen::*;

#[macro_use]
mod macros;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version(u32);

impl Default for Version {
    fn default() -> Self {
        Self(unsafe { fi_version() })
    }
}

impl Version {}

pub struct Error(i32);

impl Error {
    pub fn code(&self) -> i32 {
        self.0
    }

    pub fn message(&self) -> Option<&'static str> {
        let msg = unsafe { fi_strerror(self.0) };
        if msg.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(msg).to_str().ok() }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("code", &self.code())
            .field("message", &self.message())
            .finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = self.message().unwrap_or("unknown error");
        write!(f, "{msg}")
    }
}

impl Error {
    pub fn from_code(code: i32) -> Self {
        Self(code)
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug)]
    pub struct Capabilities: u64 {
        /// Specifies that the endpoint supports some set of atomic operations.
        ///
        /// Endpoints supporting this capability support operations defined by struct fi_ops_atomic. In the absence
        /// of any relevant flags, FI_ATOMIC implies the ability to initiate and be the target of remote atomic reads and
        /// writes. Applications can use the FI_READ, FI_WRITE, FI_REMOTE_READ, and FI_REMOTE_WRITE flags to restrict
        /// the types of atomic operations supported by an endpoint.
        const ATOMIC = FI_ATOMIC as u64;
        /// Requests that the provider support the association of a user specified identifier with each address vector (AV) address.
        ///
        /// User identifiers are returned with completion data in place of the AV address. See fi_domain(3) and fi_av(3) for more details.
        const AV_USER_ID = FI_AV_USER_ID as u64;
        /// Requests support for collective operations.
        ///
        /// Endpoints that support this capability support the collective operations defined in fi_collective(3).
        const COLLECTIVE = FI_COLLECTIVE as u64;
        /// Requests that the communication endpoint use the source address of an incoming message when matching it with a receive buffer.
        ///
        /// If this capability is not set, then the src_addr parameter for msg and tagged receive operations is ignored.
        const DIRECTED_RECV = FI_DIRECTED_RECV as u64;
        /// Similar to FI_DIRECTED_RECV, but only applies to tagged receive operations.
        const TAGGED_DIRECTED_RECV = FI_TAGGED_DIRECTED_RECV as u64;
        /// Similar to FI_DIRECTED_RECV, but requires the source address to be exact, i.e., FI_ADDR_UNSPEC is not allowed.
        ///
        /// This capability can be used alone, or in conjunction with FI_DIRECTED_RECV or FI_TAGGED_DIRECTED_RECV as a modifier
        /// to disallow FI_ADDR_UNSPEC being used as the source address.
        const EXACT_DIRECTED_RECV = FI_EXACT_DIRECTED_RECV as u64;
        /// Indicates that the endpoint support the FI_FENCE flag on data transfer operations.
        ///
        /// Support requires tracking that all previous transmit requests to a specified remote endpoint complete prior to
        /// initiating the fenced operation. Fenced operations are often used to enforce ordering between operations that are not
        /// otherwise guaranteed by the underlying provider or protocol.
        const FENCE = FI_FENCE as u64;
        /// Specifies that the endpoint should support transfers to and from device memory.
        const HMEM = FI_HMEM as u64;
        /// Indicates that the endpoint support host local communication.
        ///
        /// This flag may be used in conjunction with FI_REMOTE_COMM to
        /// indicate that local and remote communication are required. If neither FI_LOCAL_COMM or FI_REMOTE_COMM are specified, then
        /// the provider will indicate support for the configuration that minimally affects performance. Providers that set FI_LOCAL_COMM
        /// but not FI_REMOTE_COMM, for example a shared memory provider, may only be used to communication between processes on the same system.
        const LOCAL_COMM = FI_LOCAL_COMM as u64;
        /// Specifies that an endpoint should support sending and receiving messages or datagrams.
        ///
        /// Message capabilities imply support for send and/or receive queues. Endpoints supporting this capability support operations defined by struct fi_ops_msg.
        /// The caps may be used to specify or restrict the type of messaging operations that are supported. In the absence of any relevant flags,
        /// FI_MSG implies the ability to send and receive messages. Applications can use the FI_SEND and FI_RECV flags to optimize an endpoint as send-only or receive-only.
        const MSG = FI_MSG as u64;
        /// Indicates that the endpoint support multicast data transfers.
        ///
        /// This capability must be paired with FI_MSG. Applications can use FI_SEND and FI_RECV to optimize multicast as send-only or receive-only.
        const MULTICAST = FI_MULTICAST as u64;
        /// Specifies that the endpoint must support the FI_MULTI_RECV flag when posting receive buffers.
        const MULTI_RECV = FI_MULTI_RECV as u64;
        /// Specifies that the endpoint must support the FI_MULTI_RECV flag when posting tagged receive buffers.
        const TAGGED_MULTI_RECV = FI_TAGGED_MULTI_RECV as u64;
        /// Requests that endpoints which support multiple receive contexts allow an initiator to target (or name) a specific receive context as part of a data transfer operation.
        const NAMED_RX_CTX = FI_NAMED_RX_CTX as u64;
        /// Specifies that the provider must support being used as a peer provider in the peer API flow.
        ///
        /// The provider must support importing owner_ops when opening a CQ, counter, and shared receive queue.
        const PEER = FI_PEER as u64;
        /// Indicates that the user requires an endpoint capable of initiating reads against remote memory regions.
        ///
        /// This flag requires that FI_RMA and/or FI_ATOMIC be set.
        const READ = FI_READ as u64;
        /// Indicates that the user requires an endpoint capable of receiving message data transfers.
        ///
        /// Message transfers include base message operations as well as tagged message functionality.
        const RECV = FI_RECV as u64;
        /// Indicates that the endpoint support communication with endpoints located at remote nodes (across the fabric).
        ///
        /// See FI_LOCAL_COMM for additional details. Providers that set FI_REMOTE_COMM but not FI_LOCAL_COMM, for example NICs that lack
        /// loopback support, cannot be used to communicate with processes on the same system.
        const REMOTE_COMM = FI_REMOTE_COMM as u64;
        /// Indicates that the user requires an endpoint capable of receiving read memory operations from remote endpoints.
        ///
        /// This flag requires that FI_RMA and/or FI_ATOMIC be set.
        const REMOTE_READ = FI_REMOTE_READ as u64;
        /// Indicates that the user requires an endpoint capable of receiving write memory operations from remote endpoints.
        ///
        /// This flag requires that FI_RMA and/or FI_ATOMIC be set.
        const REMOTE_WRITE = FI_REMOTE_WRITE as u64;
        /// Specifies that the endpoint should support RMA read and write operations.
        ///
        /// Endpoints supporting this capability support operations defined by struct fi_ops_rma. In the absence of any relevant flags,
        /// FI_RMA implies the ability to initiate and be the target of remote memory reads and writes. Applications can use the FI_READ,
        /// FI_WRITE, FI_REMOTE_READ, and FI_REMOTE_WRITE flags to restrict the types of RMA operations supported by an endpoint.
        const RMA = FI_RMA as u64;
        /// Requests that an endpoint support the generation of completion events when it is the target of an RMA and/or atomic operation.
        ///
        /// This flag requires that FI_REMOTE_READ and/or FI_REMOTE_WRITE be enabled on the endpoint.
        const RMA_EVENT = FI_RMA_EVENT as u64;
        /// Indicates that the provider is ‘persistent memory aware’ and supports RMA operations to and from persistent memory.
        ///
        /// Persistent memory aware providers must support registration of memory that is backed by non- volatile memory, RMA transfers
        /// to/from persistent memory, and enhanced completion semantics. This flag requires that FI_RMA be set. This capability is experimental.
        const RMA_PMEM = FI_RMA_PMEM as u64;
        /// Indicates that the user requires an endpoint capable of sending message data transfers.
        ///
        /// Message transfers include base message operations as well as tagged message functionality.
        const SEND = FI_SEND as u64;
        /// Requests or indicates support for address vectors which may be shared among multiple processes.
        const SHARED_AV = FI_SHARED_AV as u64;
        /// Requests that the endpoint return source addressing data as part of its completion data.
        ///
        /// This capability only applies to connectionless endpoints. Note that returning source address information may require that the provider
        /// perform address translation and/or look-up based on data available in the underlying protocol in order to provide the requested data,
        /// which may adversely affect performance. The performance impact may be greater for address vectors of type FI_AV_TABLE.
        const SOURCE = FI_SOURCE as u64;
        /// Must be paired with FI_SOURCE. When specified, this requests that raw source addressing data be returned as part of completion data for any address
        /// that has not been inserted into the local address vector. Use of this capability may require the provider to validate incoming source address data
        /// against addresses stored in the local address vector, which may adversely affect performance.
        const SOURCE_ERR = FI_SOURCE_ERR as u64;
        /// Specifies that the endpoint should handle tagged message transfers.
        ///
        /// Tagged message transfers associate a user-specified key or tag with each message that is used for matching purposes at the remote side.
        /// Endpoints supporting this capability support operations defined by struct fi_ops_tagged. In the absence of any relevant flags, FI_TAGGED
        /// implies the ability to send and receive tagged messages. Applications can use the FI_SEND and FI_RECV flags to optimize an endpoint as send-only or receive-only.
        const TAGGED = FI_TAGGED as u64;
        /// Indicates that the endpoint should support triggered operations. Endpoints support this capability must meet the usage model as described by fi_trigger(3).
        const TRIGGER = FI_TRIGGER as u64;
        /// Indicates that the user requires an endpoint capable of initiating writes against remote memory regions. This flag requires that FI_RMA and/or FI_ATOMIC be set.
        const WRITE = FI_WRITE as u64;
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug)]
    pub struct Mode: u64 {
        /// Applications can reference multiple data buffers as part of a single operation through the use of IO vectors (SGEs).
        ///
        /// Typically, the contents of an IO vector are copied by the provider into an internal buffer area, or directly to the underlying hardware.
        /// However, when a large number of IOV entries are supported, IOV buffering may have a negative impact on performance and memory consumption.
        /// The FI_ASYNC_IOV mode indicates that the application must provide the buffering needed for the IO vectors. When set, an application must not modify
        /// an IO vector of length > 1, including any related memory descriptor array, until the associated operation has completed.
        const ASYNC_IOV = FI_ASYNC_IOV as u64;
        /// Specifies that the provider requires that applications use struct fi_context as their per operation context parameter for operations that generated full completions.
        ///
        /// This structure should be treated as opaque to the application. For performance reasons, this structure must be allocated by the user, but may be used by
        /// the fabric provider to track the operation. Typically, users embed struct fi_context within their own context structure. The struct fi_context must remain
        /// valid until the corresponding operation completes or is successfully canceled. As such, fi_context should NOT be allocated on the stack. Doing so is likely
        /// to result in stack corruption that will be difficult to debug. Users should not update or interpret the fields in this structure, or reuse it until the original
        /// operation has completed. If an operation does not generate a completion (i.e. the endpoint was configured with FI_SELECTIVE_COMPLETION and the operation was
        /// not initiated with the FI_COMPLETION flag) then the context parameter is ignored by the fabric provider. The structure is specified in rdma/fabric.h.
        const CONTEXT = FI_CONTEXT as u64;
        /// This bit is similar to FI_CONTEXT, but doubles the provider’s requirement on the size of the per context structure.
        ///
        /// When set, this specifies that the provider requires that applications use struct fi_context2 as their per operation context parameter. Or, optionally,
        /// an application can provide an array of two fi_context structures (e.g. struct fi_context[2]) instead. The requirements for using struct fi_context2 are
        /// identical as defined for FI_CONTEXT above.
        const CONTEXT2 = FI_CONTEXT2 as u64;

        /// Message prefix mode indicates that an application will provide buffer space in front of all message send and receive buffers for use by the provider.
        ///
        /// Typically, the provider uses this space to implement a protocol, with the protocol headers being written into the prefix area. The contents of the prefix
        /// space should be treated as opaque. The use of FI_MSG_PREFIX may improve application performance over certain providers by reducing the number of IO vectors
        /// referenced by underlying hardware and eliminating provider buffer allocation.
        ///
        /// FI_MSG_PREFIX only applies to send and receive operations, including tagged sends and receives. RMA and atomics do not require the application to provide
        /// prefix buffers. Prefix buffer space must be provided with all sends and receives, regardless of the size of the transfer or other transfer options. The ownership
        /// of prefix buffers is treated the same as the corresponding message buffers, but the size of the prefix buffer is not counted toward any message limits, including inject.
        ///
        /// Applications that support prefix mode must supply buffer space before their own message data. The size of space that must be provided is specified by the
        /// msg_prefix_size endpoint attribute. Providers are required to define a msg_prefix_size that is a multiple of 8 bytes. Additionally, applications may receive
        /// provider generated packets that do not contain application data. Such received messages will indicate a transfer size of that is equal to or
        /// smaller than msg_prefix_size.
        ///
        /// The buffer pointer given to all send and receive operations must point to the start of the prefix region of the buffer (as opposed to the payload).
        /// For scatter-gather send/recv operations, the prefix buffer must be a contiguous region, though it may or may not be directly adjacent to the payload
        /// portion of the buffer.
        const FI_MSG_PREFIX = FI_MSG_PREFIX as u64;
    }
}

pub mod info {
    use super::*;
    use std::{ops, sync::Arc};

    bitflags! {
        pub struct Flags: u64 {
            /// Indicates that the node parameter is a numeric string representation of a fabric address, such as a dotted decimal IP address.
            ///
            /// Use of this flag will suppress any lengthy name resolution protocol.
            const NUMERIC_HOST = FI_NUMERICHOST;
            /// Indicates that the caller is only querying for what providers are potentially available.
            ///
            /// All providers will return exactly one fi_info struct, regardless of whether that provider is usable on the current platform or not.
            /// The returned fi_info struct will contain default values for all members, with the exception of fabric_attr. The fabric_attr member will have
            /// the prov_name and prov_version values filled in.
            const PROV_ATTR_ONLY = FI_PROV_ATTR_ONLY;
            /// Indicates that the node and service parameters specify the local source address to associate with an endpoint.
            ///
            /// If specified, either the node and/or service parameter must be non-NULL. This flag is often used with passive endpoints.
            const SOURCE = FI_SOURCE;
        }
    }

    pub struct Query {
        hints: NonNull<fi_info>,
        version: Option<Version>,
        flags: Flags,
    }

    impl Query {
        pub fn new() -> Self {
            unsafe {
                let hints = fi_allocinfo();
                let hints = NonNull::new(hints).expect("info could not be allocated");
                let version = None;
                let flags = Flags::empty();
                Self {
                    hints,
                    version,
                    flags,
                }
            }
        }

        pub fn version(mut self, version: Version) -> Self {
            self.version = Some(version);
            self
        }

        pub fn capabilities(mut self, capabilities: Capabilities) -> Self {
            unsafe {
                self.hints.as_mut().caps = capabilities.bits();
            }
            self
        }

        pub fn mode(mut self, mode: Mode) -> Self {
            unsafe {
                self.hints.as_mut().mode = mode.bits();
            }
            self
        }

        pub fn endpoint(&mut self) -> endpoint::Query<'_> {
            unsafe { endpoint::Query(&mut *self.hints.as_mut().ep_attr) }
        }

        pub fn domain(&mut self) -> domain::Query<'_> {
            unsafe { domain::Query(&mut *self.hints.as_mut().domain_attr) }
        }

        pub fn flags(mut self, flags: Flags) -> Self {
            self.flags = flags;
            self
        }

        pub fn build(self) -> List {
            let version = self.version.unwrap_or_else(Version::default);

            let mut info_ptr = core::ptr::null_mut();
            let node = core::ptr::null_mut(); // TODO where does this come from?
            let service = core::ptr::null_mut(); // TODO where does this come from?
            let flags = self.flags.bits();
            let hints = self.hints.as_ptr();
            let ret = unsafe { fi_getinfo(version.0, node, service, flags, hints, &mut info_ptr) };

            assert_eq!(ret, 0, "fi_getinfo failed");
            let info = NonNull::new(info_ptr).expect("info could not be allocated");
            let storage = Arc::new(Inner(info));
            List(storage)
        }
    }

    impl Drop for Query {
        fn drop(&mut self) {
            unsafe {
                fi_freeinfo(self.hints.as_ptr());
            }
        }
    }

    #[derive(Clone)]
    pub struct List(Arc<Inner>);

    impl List {
        pub fn iter(&self) -> ListIter {
            let storage = self.0.clone();
            let cursor = storage.0.as_ptr();
            ListIter { storage, cursor }
        }
    }

    pub struct ListIter {
        storage: Arc<Inner>,
        cursor: *mut fi_info,
    }

    impl Iterator for ListIter {
        type Item = Info;

        fn next(&mut self) -> Option<Self::Item> {
            let cursor = NonNull::new(self.cursor)?;
            let storage = self.storage.clone();
            self.cursor = unsafe { (*cursor.as_ref()).next };
            let info = InnerInfo(cursor);
            Some(Info { storage, info })
        }
    }

    pub struct Info {
        #[allow(dead_code)]
        storage: Arc<Inner>,
        pub(super) info: InnerInfo,
    }

    pub(super) struct InnerInfo(pub(super) NonNull<fi_info>);

    impl ops::Deref for InnerInfo {
        type Target = fi_info;

        fn deref(&self) -> &Self::Target {
            unsafe { self.0.as_ref() }
        }
    }

    struct Inner(NonNull<fi_info>);

    impl Drop for Inner {
        fn drop(&mut self) {
            unsafe {
                fi_freeinfo(self.0.as_ptr());
            }
        }
    }

    impl ops::Deref for Inner {
        type Target = fi_info;

        fn deref(&self) -> &Self::Target {
            unsafe { self.0.as_ref() }
        }
    }

    impl fmt::Debug for Info {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("fi_info")
                .field("capabilities", &self.capabilities())
                .field("mode", &self.mode())
                .field("addr_format", &self.addr_format())
                .field("src_addr", &self.src_addr())
                .field("dest_addr", &self.dest_addr())
                .field("endpoint", &self.endpoint_attr())
                .field("domain", &self.domain_attr())
                .finish_non_exhaustive()
        }
    }

    impl Info {
        pub fn capabilities(&self) -> Capabilities {
            Capabilities::from_bits_truncate(self.info.caps)
        }

        pub fn mode(&self) -> Mode {
            Mode::from_bits_truncate(self.info.mode)
        }

        pub fn addr_format(&self) -> addr::Format {
            addr::Format::from_bits(self.info.addr_format).unwrap_or(addr::Format::Unspecified)
        }

        pub fn src_addr(&self) -> Option<&[u8]> {
            unsafe {
                let ptr = self.info.src_addr as *const u8;
                let len = self.info.src_addrlen;
                if ptr.is_null() {
                    return None;
                }

                Some(core::slice::from_raw_parts(ptr, len))
            }
        }

        pub fn dest_addr(&self) -> Option<&[u8]> {
            unsafe {
                let ptr = self.info.dest_addr as *const u8;
                let len = self.info.dest_addrlen;

                if ptr.is_null() {
                    return None;
                }

                Some(core::slice::from_raw_parts(ptr, len))
            }
        }

        pub fn endpoint_attr(&self) -> endpoint::Attributes<'_> {
            unsafe {
                let ep_attr = self.info.ep_attr;
                assert!(!ep_attr.is_null());
                endpoint::Attributes(&*ep_attr)
            }
        }

        pub fn domain_attr(&self) -> domain::Attributes<'_> {
            unsafe {
                let domain_attr = self.info.domain_attr;
                assert!(!domain_attr.is_null());
                domain::Attributes(&*domain_attr)
            }
        }
    }
}

pub mod endpoint {
    #![allow(non_camel_case_types)]

    use super::*;
    use std::sync::{Arc, Mutex};

    define_enum!(
        pub enum Type {
            /// Supports a connectionless, unreliable datagram communication.
            ///
            /// Message boundaries are maintained, but the maximum message size may be limited to the fabric MTU.
            /// Flow control is not guaranteed.
            DATAGRAM = fi_ep_type_FI_EP_DGRAM,
            /// Provides a reliable, connection-oriented data transfer service with flow control that maintains message boundaries.
            MSG = fi_ep_type_FI_EP_MSG,
            /// Reliable datagram message. Provides a reliable, connectionless data transfer service with flow control that maintains message boundaries.
            RDM = fi_ep_type_FI_EP_RDM,
            /// The type of endpoint is not specified. This is usually provided as input, with other attributes of the endpoint or the provider selecting the type.
            Unspecified = fi_ep_type_FI_EP_UNSPEC,
        }
    );

    define_enum!(
        pub enum Protocol {
            EFA = FI_PROTO_EFA,
            IB_RDM = FI_PROTO_IB_RDM,
            IB_UD = FI_PROTO_IB_UD,
            IWARP = FI_PROTO_IWARP,
            RXD = FI_PROTO_RXD,
            RXM = FI_PROTO_RXM,
            TCP = FI_PROTO_SOCK_TCP,
            UDP = FI_PROTO_UDP,
            SHM = FI_PROTO_SHM,
            Unspecified = FI_PROTO_UNSPEC,
        }
    );

    pub struct Endpoint {
        inner: Arc<Inner>,
    }

    struct Inner {
        fid: NonNull<fid_ep>,
        completion_queues: Mutex<Vec<completion_queue::CompletionQueue>>,
        address_vectors: Mutex<Vec<address_vector::AddressVector>>,
    }

    impl Endpoint {
        /// Open an endpoint from a domain
        pub fn open(domain: &domain::Domain, info: &info::Info) -> Result<Self, Error> {
            let mut ep_ptr: *mut fid_ep = core::ptr::null_mut();
            let ret = unsafe {
                fi_endpoint(
                    domain.as_ptr(),
                    info.info.0.as_ptr(),
                    &mut ep_ptr,
                    core::ptr::null_mut(),
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            let fid = NonNull::new(ep_ptr).expect("endpoint pointer was null");
            let inner = Arc::new(Inner {
                fid,
                completion_queues: Default::default(),
                address_vectors: Default::default(),
            });
            Ok(Self { inner })
        }

        /// Bind a completion queue to the endpoint
        ///
        /// Flags should be FI_TRANSMIT, FI_RECV, or both (FI_TRANSMIT | FI_RECV)
        pub fn bind_cq(
            &self,
            cq: &completion_queue::CompletionQueue,
            flags: u64, // TODO make this a bitflag struct
        ) -> Result<(), Error> {
            let ret = unsafe {
                fi_ep_bind(
                    self.inner.fid.as_ptr(),
                    &mut (*cq.as_ptr()).fid as *mut fid,
                    flags,
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            // Store the completion queue so it lives as long as the endpoint does
            self.inner
                .completion_queues
                .lock()
                .unwrap()
                .push(cq.clone());

            Ok(())
        }

        /// Bind an address vector to the endpoint
        pub fn bind_av(&self, av: &address_vector::AddressVector) -> Result<(), Error> {
            let ret = unsafe {
                fi_ep_bind(
                    self.inner.fid.as_ptr(),
                    &mut (*av.as_ptr()).fid as *mut fid,
                    0,
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            // Store the address vector so it lives as long as the endpoint does
            self.inner.address_vectors.lock().unwrap().push(av.clone());

            Ok(())
        }

        /// Enable the endpoint for data transfers
        ///
        /// Must be called after binding completion queue and address vector
        pub fn enable(&self) -> Result<(), Error> {
            let ret = unsafe { fi_enable(self.inner.fid.as_ptr()) };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            Ok(())
        }

        /// Get the local address of this endpoint
        ///
        /// Must be called after the endpoint is enabled.
        /// Returns the raw address bytes suitable for insertion into an address vector.
        pub fn getname(&self) -> Result<Vec<u8>, Error> {
            let mut addrlen = 256;
            let mut addr_buf = vec![0u8; addrlen];

            let ret = unsafe {
                fi_getname(
                    &mut (*self.inner.fid.as_ptr()).fid as *mut fid,
                    addr_buf.as_mut_ptr() as *mut core::ffi::c_void,
                    &mut addrlen,
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            addr_buf.truncate(addrlen);
            Ok(addr_buf)
        }

        /// Send a message to a destination endpoint
        ///
        /// The data is sent from the registered memory region. The buffer will remain
        /// valid until the completion is received (the MemoryRegion owns the buffer).
        ///
        /// # Parameters
        /// - `mr`: Registered memory region containing the data to send
        /// - `len`: Number of bytes to send from the buffer (must be <= mr.len())
        /// - `dest_addr`: Destination address (from address vector)
        ///
        /// # Returns
        /// Ok(()) if the operation was successfully posted. The actual completion
        /// will be reported via the bound completion queue.
        pub fn send(
            &self,
            mr: memory_registration::Send,
            dest_addr: fi_addr_t,
        ) -> Result<(), Error> {
            let iov = mr.iov();

            let ret = unsafe {
                fi_sendv(
                    self.inner.fid.as_ptr(),
                    iov.as_ptr(),
                    &mut mr.desc(),
                    iov.len(),
                    dest_addr,
                    // Clone and send the memory region in the completion queue
                    // so it doesn't get dropped before the transport finishes sending it
                    mr.into_context(),
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret as i32));
            }

            Ok(())
        }

        /// Post a receive buffer to accept incoming messages
        ///
        /// The mutable buffer will be filled with data from the sender. The buffer
        /// remains valid until the completion is received.
        ///
        /// # Parameters
        /// - `mr`: Mutable registered memory region to receive into
        /// - `src_addr`: Source address to match, or None to accept from any sender
        ///
        /// # Returns
        /// Ok(()) if the operation was successfully posted. The actual completion
        /// will be reported via the bound completion queue, at which point you can
        /// access the received data from the mutable buffer.
        pub fn recv(
            &self,
            mr: memory_registration::Receive,
            src_addr: Option<fi_addr_t>,
        ) -> Result<(), Error> {
            let buffer = mr.buffer();

            let ret = unsafe {
                fi_recv(
                    self.inner.fid.as_ptr(),
                    buffer.as_ptr() as *mut core::ffi::c_void,
                    buffer.capacity(),
                    mr.desc(),
                    src_addr.unwrap_or(u64::MAX),
                    // Transfer ownership of the mutable region to the completion queue
                    mr.into_context(),
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret as i32));
            }

            Ok(())
        }

        /// Initiate an RMA read operation (pull data from remote peer)
        ///
        /// Reads data from a remote peer's registered memory region into your local buffer.
        /// This is a one-sided operation - the remote peer is not notified.
        ///
        /// # Parameters
        /// - `mr`: Local receive buffer to read data into
        /// - `len`: Number of bytes to read (must be <= mr.capacity())
        /// - `dest_addr`: Remote endpoint address (from address vector)
        /// - `remote_addr`: Virtual address of remote buffer (if FI_MR_VIRT_ADDR)
        /// - `remote_key`: Remote memory key (from peer's Send/Receive region)
        ///
        /// # Returns
        /// Ok(()) if the operation was successfully posted. The actual completion
        /// will be reported via the bound completion queue.
        pub fn read(
            &self,
            mr: memory_registration::Receive,
            len: usize,
            dest_addr: fi_addr_t,
            remote_addr: u64,
            remote_key: u64,
        ) -> Result<(), Error> {
            let buffer = mr.buffer();

            if len > buffer.capacity() {
                return Err(Error::from_code(-(FI_EINVAL as i32)));
            }

            let ret = unsafe {
                fi_read(
                    self.inner.fid.as_ptr(),
                    buffer.as_ptr() as *mut core::ffi::c_void,
                    len,
                    mr.desc(),
                    dest_addr,
                    remote_addr,
                    remote_key,
                    // Transfer ownership to completion queue
                    mr.into_context(),
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret as i32));
            }

            Ok(())
        }

        /// Initiate an RMA write operation (push data to remote peer)
        ///
        /// Writes data from your local buffer to a remote peer's registered memory region.
        /// This is a one-sided operation - the remote peer is not notified unless they
        /// requested FI_RMA_EVENT.
        ///
        /// # Parameters
        /// - `mr`: Local send buffer containing data to write
        /// - `len`: Number of bytes to write (must be <= mr.len())
        /// - `dest_addr`: Remote endpoint address (from address vector)
        /// - `remote_addr`: Virtual address of remote buffer (if FI_MR_VIRT_ADDR)
        /// - `remote_key`: Remote memory key (from peer's Send/Receive region)
        ///
        /// # Returns
        /// Ok(()) if the operation was successfully posted. The actual completion
        /// will be reported via the bound completion queue.
        pub fn write(
            &self,
            mr: &memory_registration::Send,
            len: usize,
            dest_addr: fi_addr_t,
            remote_addr: u64,
            remote_key: u64,
        ) -> Result<(), Error> {
            if len > mr.len() {
                return Err(Error::from_code(-(FI_EINVAL as i32)));
            }

            let iov = mr.iov();

            let ret = unsafe {
                fi_writev(
                    self.inner.fid.as_ptr(),
                    iov.as_ptr(),
                    &mut mr.desc(),
                    iov.len(),
                    dest_addr,
                    remote_addr,
                    remote_key,
                    // Clone and send to completion queue
                    mr.clone().into_context(),
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret as i32));
            }

            Ok(())
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            unsafe {
                if let Some(close) = (*self.fid.as_ptr())
                    .fid
                    .ops
                    .as_ref()
                    .and_then(|ops| (*ops).close)
                {
                    let _ = close(&mut (*self.fid.as_ptr()).fid as *mut fid);
                }
            }
        }
    }

    impl fmt::Debug for Endpoint {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Endpoint").finish_non_exhaustive()
        }
    }

    unsafe impl Send for Endpoint {}
    unsafe impl Sync for Endpoint {}

    pub struct Query<'a>(pub(super) &'a mut fi_ep_attr);

    impl Query<'_> {
        pub fn type_(self, type_: Type) -> Self {
            self.0.type_ = type_ as u32;
            self
        }

        pub fn protocol(self, protocol: Protocol) -> Self {
            self.0.protocol = protocol as u32;
            self
        }
    }

    pub struct Attributes<'a>(pub(super) &'a fi_ep_attr);

    impl fmt::Debug for Attributes<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("fi_ep_attr")
                .field("type", &self.type_())
                .field("protocol", &self.protocol())
                .field("protocol_version", &self.protocol_version())
                .field("max_msg_size", &self.max_msg_size())
                .field("msg_prefix_size", &self.msg_prefix_size())
                .field("max_order_raw_size", &self.max_order_raw_size())
                .field("max_order_war_size", &self.max_order_war_size())
                .field("max_order_waw_size", &self.max_order_waw_size())
                .field("mem_tag_format", &self.mem_tag_format())
                .field("tx_ctx_cnt", &self.tx_ctx_cnt())
                .field("rx_ctx_cnt", &self.rx_ctx_cnt())
                .field("auth_key", &self.auth_key().map(|v| v.len()))
                .finish_non_exhaustive()
        }
    }

    impl Attributes<'_> {
        pub fn type_(&self) -> Type {
            Type::from_bits(self.0.type_).unwrap_or(Type::Unspecified)
        }

        pub fn protocol(&self) -> Protocol {
            Protocol::from_bits(self.0.protocol).unwrap_or(Protocol::Unspecified)
        }

        pub fn protocol_version(&self) -> Version {
            Version(self.0.protocol_version)
        }

        pub fn max_msg_size(&self) -> usize {
            self.0.max_msg_size as usize
        }

        pub fn msg_prefix_size(&self) -> usize {
            self.0.msg_prefix_size as usize
        }

        pub fn max_order_raw_size(&self) -> usize {
            self.0.max_order_raw_size as usize
        }

        pub fn max_order_war_size(&self) -> usize {
            self.0.max_order_war_size as usize
        }

        pub fn max_order_waw_size(&self) -> usize {
            self.0.max_order_waw_size as usize
        }

        pub fn mem_tag_format(&self) -> u64 {
            self.0.mem_tag_format
        }

        pub fn tx_ctx_cnt(&self) -> usize {
            self.0.tx_ctx_cnt as usize
        }

        pub fn rx_ctx_cnt(&self) -> usize {
            self.0.rx_ctx_cnt as usize
        }

        pub fn auth_key(&self) -> Option<&[u8]> {
            unsafe {
                let ptr = self.0.auth_key;
                if ptr.is_null() {
                    return None;
                }
                let len = self.0.auth_key_size;
                Some(core::slice::from_raw_parts(ptr, len))
            }
        }
    }
}

pub mod domain {
    use super::*;
    use core::ffi::CStr;
    use std::sync::Arc;

    bitflags! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Capabilities: u64 {
            /// When the domain is configured with FI_DIRECTED_RECV and FI_AV_AUTH_KEY, memory regions can be limited to specific authorization keys.
            const DIRECTED_RECV = FI_DIRECTED_RECV as u64;
            /// Indicates that the domain supports the ability to open address vectors with the FI_AV_USER_ID flag.
            ///
            /// If this domain capability is not set, address vectors cannot be opened with FI_AV_USER_ID. Note that FI_AV_USER_ID can still be
            /// supported through the AV insert calls without this domain capability set. See fi_av(3).
            const AV_USER_ID = FI_AV_USER_ID as u64;
            /// Specifies that the domain must support importing resources to be used in the the peer API flow.
            ///
            /// The domain must support importing owner_ops when opening a CQ, counter, and shared receive queue.
            const PEER = FI_PEER as u64;
            /// At a conceptual level, this field indicates that the underlying device supports loopback communication.
            ///
            /// More specifically, this field indicates that an endpoint may communicate with other endpoints that are allocated from the same underlying
            /// named domain. If this field is not set, an application may need to use an alternate domain or mechanism (e.g. shared memory) to communicate
            /// with peers that execute on the same node.
            const LOCAL_COMM = FI_LOCAL_COMM as u64;
            /// This field indicates that the underlying provider supports communication with nodes that are reachable over the network.
            ///
            /// If this field is not set, then the provider only supports communication between processes that execute on the same node – a shared memory provider, for example.
            const REMOTE_COMM = FI_REMOTE_COMM as u64;
            /// Indicates that the domain supports the ability to share address vectors among multiple processes using the named address vector feature.
            const SHARED_AV = FI_SHARED_AV as u64;
        }
    }

    define_enum!(
        pub enum Threading {
            /// The completion threading model is intended for providers that make use of manual progress.
            ///
            /// Applications must serialize access to all objects that are associated through the use of having a shared completion structure. This includes endpoint,
            /// completion queue, counter, wait set, and poll set objects.
            ///
            /// For example, threads must serialize access to an endpoint and its bound completion queue(s) and/or counters. Access to endpoints that share the same
            /// completion queue must also be serialized.
            ///
            /// The use of FI_THREAD_COMPLETION can increase parallelism over FI_THREAD_SAFE, but requires the use of isolated resources.
            COMPLETION = fi_threading_FI_THREAD_COMPLETION,
            /// A domain serialization model requires applications to serialize access to all objects belonging to a domain.
            DOMAIN = fi_threading_FI_THREAD_DOMAIN,
            /// The endpoint threading model is similar to FI_THREAD_FID, but with the added restriction that serialization is required when accessing the same endpoint,
            /// even if multiple transmit and receive contexts are used.
            ///
            /// Conceptually, FI_THREAD_ENDPOINT maps well to providers that implement fabric services in hardware but use a single command queue to access different data flows.
            ENDPOINT = fi_threading_FI_THREAD_ENDPOINT,
            /// A fabric descriptor (FID) serialization model requires applications to serialize access to individual fabric resources associated with data transfer
            /// operations and completions.
            ///
            /// Multiple threads must be serialized when accessing the same endpoint, transmit context, receive context, completion queue, counter, wait set, or poll set.
            /// Serialization is required only by threads accessing the same object.
            /// For example, one thread may be initiating a data transfer on an endpoint, while another thread reads from a completion queue associated with the endpoint.
            ///
            /// Serialization to endpoint access is only required when accessing the same endpoint data flow. Multiple threads may initiate transfers on different transmit
            /// contexts of the same endpoint without serializing, and no serialization is required between the submission of data transmit requests and data receive operations.
            ///
            /// In general, FI_THREAD_FID allows the provider to be implemented without needing internal locking when handling data transfers. Conceptually, FI_THREAD_FID
            /// maps well to providers that implement fabric services in hardware and provide separate command queues to different data flows.
            FID = fi_threading_FI_THREAD_FID,
            /// A thread safe serialization model allows a multi-threaded application to access any allocated resources through any interface without restriction.
            ///
            /// All providers are required to support FI_THREAD_SAFE.
            SAFE = fi_threading_FI_THREAD_SAFE,
            /// This value indicates that no threading model has been defined.
            ///
            /// It may be used on input hints to the fi_getinfo call. When specified, providers will return a threading model that allows for the greatest level of parallelism.
            Unspecified = fi_threading_FI_THREAD_UNSPEC,
        }
    );

    define_enum!(
        pub enum Progress {
            AUTO = fi_progress_FI_PROGRESS_AUTO,
            CONTROL_UNIFIED = fi_progress_FI_PROGRESS_CONTROL_UNIFIED,
            MANUAL = fi_progress_FI_PROGRESS_MANUAL,
            Unspecified = fi_progress_FI_PROGRESS_UNSPEC,
        }
    );

    define_enum!(
        pub enum ResourceManagement {
            DISABLED = fi_resource_mgmt_FI_RM_DISABLED,
            ENABLED = fi_resource_mgmt_FI_RM_ENABLED,
            Unspecified = fi_resource_mgmt_FI_RM_UNSPEC,
        }
    );

    define_enum!(
        pub enum AvType {
            MAP = fi_av_type_FI_AV_MAP,
            TABLE = fi_av_type_FI_AV_TABLE,
            Unspecified = fi_av_type_FI_AV_UNSPEC,
        }
    );

    pub struct Query<'a>(pub(super) &'a mut fi_domain_attr);

    impl<'a> Query<'a> {
        pub fn memory_registration(self, mode: memory_registration::Mode) -> Self {
            self.0.mr_mode = mode.bits();
            self
        }

        pub fn av_type(self, av_type: AvType) -> Self {
            self.0.av_type = av_type as _;
            self
        }
    }

    pub struct Domain {
        inner: Arc<Inner>,
    }

    struct Inner {
        fid: NonNull<fid_domain>,
    }

    impl Domain {
        pub(crate) fn open(fabric: &fabric::Fabric, info: &info::Info) -> Result<Self, Error> {
            let mut domain_ptr: *mut fid_domain = core::ptr::null_mut();
            let ret = unsafe {
                fi_domain(
                    fabric.as_ptr(),
                    info.info.0.as_ptr(),
                    &mut domain_ptr,
                    core::ptr::null_mut(),
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            let fid = NonNull::new(domain_ptr).expect("domain pointer was null");
            let inner = Arc::new(Inner { fid });
            Ok(Self { inner })
        }

        pub(crate) fn as_ptr(&self) -> *mut fid_domain {
            self.inner.fid.as_ptr()
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            unsafe {
                if let Some(close) = (*self.fid.as_ptr())
                    .fid
                    .ops
                    .as_ref()
                    .and_then(|ops| (*ops).close)
                {
                    close(&mut (*self.fid.as_ptr()).fid as *mut fid);
                }
            }
        }
    }

    impl fmt::Debug for Domain {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Domain").finish_non_exhaustive()
        }
    }

    pub struct Attributes<'a>(pub(super) &'a fi_domain_attr);

    impl fmt::Debug for Attributes<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("fi_domain_attr")
                .field("name", &self.name())
                .field("capabilities", &self.capabilities())
                .field("threading", &self.threading())
                .field("resource_management", &self.resource_management())
                .field("av_type", &self.av_type())
                .field("memory_registration", &self.memory_registration())
                .field("auth_key", &self.auth_key().map(|v| v.len()))
                .finish_non_exhaustive()
        }
    }

    impl Attributes<'_> {
        pub fn name(&self) -> Option<&CStr> {
            if self.0.name.is_null() {
                return None;
            }
            Some(unsafe { CStr::from_ptr(self.0.name) })
        }

        pub fn capabilities(&self) -> Capabilities {
            Capabilities::from_bits_truncate(self.0.caps)
        }

        pub fn threading(&self) -> Threading {
            Threading::from_bits(self.0.threading).unwrap_or(Threading::Unspecified)
        }

        pub fn resource_management(&self) -> ResourceManagement {
            ResourceManagement::from_bits(self.0.resource_mgmt)
                .unwrap_or(ResourceManagement::Unspecified)
        }

        pub fn av_type(&self) -> AvType {
            AvType::from_bits(self.0.av_type).unwrap_or(AvType::Unspecified)
        }

        pub fn memory_registration(&self) -> memory_registration::Mode {
            memory_registration::Mode::from_bits_truncate(self.0.mr_mode)
        }

        pub fn auth_key(&self) -> Option<&[u8]> {
            unsafe {
                let ptr = self.0.auth_key;
                if ptr.is_null() {
                    return None;
                }
                let len = self.0.auth_key_size;
                Some(core::slice::from_raw_parts(ptr, len))
            }
        }
    }
}

pub mod memory_registration {
    use super::*;
    use crate::bytevec::{ByteVec, BytesMut};
    use core::pin::Pin;
    use std::sync::Arc;

    bitflags! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Mode: i32 {
            /// Indicates that memory registration occurs on allocated data buffers, and physical pages must back all virtual addresses being registered.
            const ALLOCATED = FI_MR_ALLOCATED as i32;
            /// Requires data buffers passed to collective operations be explicitly registered for collective operations using the FI_COLLECTIVE flag.
            const COLLECTIVE = FI_MR_COLLECTIVE as i32;
            /// Memory registration occurs at the endpoint level, rather than domain.
            const ENDPOINT = FI_MR_ENDPOINT as i32;
            /// The provider is optimized around having applications register memory for locally accessed data buffers.
            ///
            /// Data buffers used in send and receive operations and as the source buffer for RMA and atomic operations must be registered by the
            /// application for access domains opened with this capability.
            const LOCAL = FI_MR_LOCAL as i32;
            /// Indicates that the application is responsible for notifying the provider when the page tables referencing a registered memory region may have been updated.
            const MMU_NOTIFY = FI_MR_MMU_NOTIFY as i32;
            /// Memory registration keys are selected and returned by the provider.
            const PROV_KEY = FI_MR_PROV_KEY as i32;
            /// The provider requires additional setup as part of their memory registration process. This mode is required by providers that use a memory key that is larger than 64-bits.
            const RAW = FI_MR_RAW as i32;
            /// Indicates that the memory regions associated with completion counters must be explicitly enabled after being bound to any counter.
            const RMA_EVENT = FI_MR_RMA_EVENT as i32;
            /// Registered memory regions are referenced by peers using the virtual address of the registered memory region, rather than a 0-based offset.
            const VIRT_ADDR = FI_MR_VIRT_ADDR as i32;
        }
    }

    bitflags! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Access: u64 {
            /// Memory region can be used as source for send operations
            const SEND = FI_SEND as u64;
            /// Memory region can be used for receiving messages
            const RECV = FI_RECV as u64;
            /// Memory region can be used as source for RMA read operations
            const READ = FI_READ as u64;
            /// Memory region can be used as source for RMA write operations
            const WRITE = FI_WRITE as u64;
            /// Memory region can be used as target for remote RMA read operations
            const REMOTE_READ = FI_REMOTE_READ as u64;
            /// Memory region can be used as target for remote RMA write operations
            const REMOTE_WRITE = FI_REMOTE_WRITE as u64;
        }
    }

    #[repr(u8)]
    #[derive(Clone, Copy, Debug)]
    enum RegionType {
        Send = 0,
        Receive = 1,
    }

    #[repr(C)]
    struct RegionHeader {
        type_id: RegionType,
    }

    /// Send buffer region for send operations and RMA writes
    ///
    /// Can be cloned cheaply for retransmissions. Uses Arc for shared ownership.
    #[derive(Clone)]
    pub struct Send {
        inner: Arc<SendInner>,
    }

    #[repr(C)]
    struct SendInner {
        // The header _must_ come first in order for the completion queue to know the type
        header: RegionHeader,
        fid: NonNull<fid_mr>,
        buffer: ByteVec,
        key: u64,
        desc: *mut core::ffi::c_void,
        iov: Pin<Box<[iovec]>>,
    }

    impl Send {
        pub fn register(
            domain: &domain::Domain,
            buffer: ByteVec,
            access: Access,
            key: u64,
        ) -> Result<Self, Error> {
            let iovecs: Vec<iovec> = buffer
                .chunks()
                .map(|chunk| iovec {
                    iov_base: chunk.as_ptr() as *mut core::ffi::c_void,
                    iov_len: chunk.len(),
                })
                .collect();
            let iov = Pin::new(iovecs.into_boxed_slice());

            let mut mr_ptr: *mut fid_mr = core::ptr::null_mut();

            let ret = unsafe {
                fi_mr_regv(
                    domain.as_ptr(),
                    iov.as_ptr(),
                    iov.len(),
                    access.bits(),
                    0,   // offset
                    key, // requested_key
                    0,   // flags
                    &mut mr_ptr,
                    core::ptr::null_mut(),
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            let fid = NonNull::new(mr_ptr).expect("memory region pointer was null");
            let (key, desc) = unsafe {
                let key = fi_mr_key(mr_ptr);
                let desc = fi_mr_desc(mr_ptr);
                (key, desc)
            };

            let inner = Arc::new(SendInner {
                header: RegionHeader {
                    type_id: RegionType::Send,
                },
                fid,
                buffer,
                key,
                desc,
                iov,
            });

            Ok(Self { inner })
        }

        /// Get the memory key for this region (used for remote access)
        pub fn key(&self) -> u64 {
            self.inner.key
        }

        /// Get the memory descriptor for this region (used for local operations)
        pub fn desc(&self) -> *mut core::ffi::c_void {
            self.inner.desc
        }

        /// Get a reference to the underlying buffer
        pub fn buffer(&self) -> &ByteVec {
            &self.inner.buffer
        }

        /// Get the total length of the memory region
        pub fn len(&self) -> usize {
            self.inner.buffer.len()
        }

        /// Check if the memory region is empty
        pub fn is_empty(&self) -> bool {
            self.inner.buffer.is_empty()
        }

        /// Get the iovec array for vectored operations
        pub fn iov(&self) -> &[iovec] {
            &self.inner.iov
        }

        /// Convert the MemoryRegion into a context pointer for libfabric operations
        ///
        /// This consumes the MemoryRegion and converts it into a raw pointer that can
        /// be passed as the context parameter to libfabric operations. The MemoryRegion
        /// will be kept alive until `from_context` is called in the completion handler.
        pub(crate) fn into_context(self) -> *mut core::ffi::c_void {
            Arc::into_raw(self.inner) as *mut core::ffi::c_void
        }
    }

    impl Drop for SendInner {
        fn drop(&mut self) {
            unsafe {
                if let Some(close) = (*self.fid.as_ptr())
                    .fid
                    .ops
                    .as_ref()
                    .and_then(|ops| (*ops).close)
                {
                    let _ = close(&mut (*self.fid.as_ptr()).fid as *mut fid);
                }
            }
        }
    }

    impl fmt::Debug for Send {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Send")
                .field("key", &self.key())
                .field("len", &self.len())
                .field("chunks", &self.buffer().chunks().len())
                .finish()
        }
    }

    // SAFETY: Send is safe to send across threads because:
    // 1. The fid is managed by Arc and reference counted
    // 2. The buffer (ByteVec) is already Send
    // 3. The raw pointers (desc) are only used within libfabric calls
    unsafe impl std::marker::Send for Send {}
    unsafe impl Sync for Send {}

    /// Receive buffer region for recv and RMA read operations
    ///
    /// Cannot be cloned - unique ownership of mutable buffer.
    pub struct Receive {
        inner: Box<ReceiveInner>,
    }

    #[repr(C)]
    struct ReceiveInner {
        header: RegionHeader,
        fid: NonNull<fid_mr>,
        buffer: BytesMut,
        key: u64,
        desc: *mut core::ffi::c_void,
    }

    impl Receive {
        pub fn register(
            domain: &domain::Domain,
            mut buffer: BytesMut,
            access: Access,
            key: u64,
        ) -> Result<Self, Error> {
            use bytes::BufMut;

            let mut mr_ptr: *mut fid_mr = core::ptr::null_mut();
            let buf = buffer.chunk_mut();

            let ret = unsafe {
                fi_mr_reg(
                    domain.as_ptr(),
                    buf.as_mut_ptr() as *const _,
                    buf.len(),
                    access.bits(),
                    0,
                    key,
                    0,
                    &mut mr_ptr,
                    core::ptr::null_mut(),
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            let fid = NonNull::new(mr_ptr).expect("memory region pointer was null");
            let (key, desc) = unsafe {
                let key = fi_mr_key(mr_ptr);
                let desc = fi_mr_desc(mr_ptr);
                (key, desc)
            };

            let inner = Box::new(ReceiveInner {
                header: RegionHeader {
                    type_id: RegionType::Receive,
                },
                fid,
                buffer,
                key,
                desc,
            });

            Ok(Self { inner })
        }

        pub fn key(&self) -> u64 {
            self.inner.key
        }

        pub fn desc(&self) -> *mut core::ffi::c_void {
            self.inner.desc
        }

        pub fn buffer(&self) -> &BytesMut {
            &self.inner.buffer
        }

        pub fn buffer_mut(&mut self) -> &mut BytesMut {
            &mut self.inner.buffer
        }

        pub fn len(&self) -> usize {
            self.inner.buffer.len()
        }

        pub fn is_empty(&self) -> bool {
            self.inner.buffer.is_empty()
        }

        pub(crate) fn into_context(self) -> *mut core::ffi::c_void {
            Box::into_raw(self.inner) as *mut core::ffi::c_void
        }
    }

    impl Drop for ReceiveInner {
        fn drop(&mut self) {
            unsafe {
                if let Some(close) = (*self.fid.as_ptr())
                    .fid
                    .ops
                    .as_ref()
                    .and_then(|ops| (*ops).close)
                {
                    let _ = close(&mut (*self.fid.as_ptr()).fid as *mut fid);
                }
            }
        }
    }

    impl fmt::Debug for Receive {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Receive")
                .field("key", &self.key())
                .field("len", &self.len())
                .finish()
        }
    }

    // SAFETY: Send is safe to send across threads because:
    // 1. The fid is managed by Box
    // 2. The buffer (BytesMut) is already Send
    // 3. The raw pointers (desc) are only used within libfabric calls
    unsafe impl std::marker::Send for Receive {}
    unsafe impl Sync for Receive {}

    /// Type-erased memory region for completion queue reconstruction
    ///
    /// Only used when reconstructing from context pointer in completion handler.
    #[derive(Debug)]
    pub enum MemoryRegion {
        Send(Send),
        Receive(Receive),
    }

    impl MemoryRegion {
        /// Reconstruct a MemoryRegion from a context pointer
        ///
        /// # Safety
        /// The context pointer must have been created by `into_context()` from either
        /// Send or Receive.
        pub unsafe fn from_context(
            context: *mut core::ffi::c_void,
            recv_len: Option<usize>,
        ) -> Self {
            let header = &*(context as *const RegionHeader);
            match header.type_id {
                RegionType::Send => {
                    let inner = Arc::from_raw(context as *const SendInner);
                    Self::Send(Send { inner })
                }
                RegionType::Receive => {
                    let mut inner = Box::from_raw(context as *mut ReceiveInner);
                    if let Some(len) = recv_len {
                        unsafe {
                            inner.buffer.set_len(len);
                        }
                    }
                    Self::Receive(Receive { inner })
                }
            }
        }
    }
}

pub mod completion_queue {
    use super::*;
    use std::sync::Arc;

    bitflags! {
        #[derive(Clone, Copy, Debug)]
        pub struct CompletionFlags: u64 {
            /// Receive operation completed
            const RECV = FI_RECV as u64;
            /// Send operation completed
            const SEND = FI_SEND as u64;
            /// RMA read operation completed
            const READ = FI_READ as u64;
            /// RMA write operation completed
            const WRITE = FI_WRITE as u64;
            /// Remote read operation completed
            const REMOTE_READ = FI_REMOTE_READ as u64;
            /// Remote write operation completed
            const REMOTE_WRITE = FI_REMOTE_WRITE as u64;
            /// Remote CQ data available
            const REMOTE_CQ_DATA = FI_REMOTE_CQ_DATA as u64;
            /// Multi-receive buffer consumed
            const MULTI_RECV = FI_MULTI_RECV as u64;
            /// Tagged message operation
            const TAGGED = FI_TAGGED as u64;
            /// Message operation
            const MSG = FI_MSG as u64;
        }
    }

    define_enum!(
        pub enum Format {
            /// No completion data is provided
            UNSPEC = fi_cq_format_FI_CQ_FORMAT_UNSPEC,
            /// Provides only the context data
            CONTEXT = fi_cq_format_FI_CQ_FORMAT_CONTEXT,
            /// Provides context and basic completion data
            MSG = fi_cq_format_FI_CQ_FORMAT_MSG,
            /// Provides context and detailed completion data
            DATA = fi_cq_format_FI_CQ_FORMAT_DATA,
            /// Provides context and tagged message completion data
            TAGGED = fi_cq_format_FI_CQ_FORMAT_TAGGED,
        }
    );

    define_enum!(
        pub enum WaitObject {
            NONE = fi_wait_obj_FI_WAIT_NONE,
            UNSPEC = fi_wait_obj_FI_WAIT_UNSPEC,
            SET = fi_wait_obj_FI_WAIT_SET,
            FD = fi_wait_obj_FI_WAIT_FD,
            MUTEX_COND = fi_wait_obj_FI_WAIT_MUTEX_COND,
        }
    );

    #[derive(Clone)]
    pub struct CompletionQueue {
        inner: Arc<Inner>,
    }

    unsafe impl Send for CompletionQueue {}
    unsafe impl Sync for CompletionQueue {}

    struct Inner {
        fid: NonNull<fid_cq>,
    }

    impl CompletionQueue {
        /// Open a completion queue
        pub fn open(domain: &domain::Domain, size: usize) -> Result<Self, Error> {
            Self::open_with_format(domain, size, Format::CONTEXT)
        }

        /// Open a completion queue with a specific format
        pub fn open_with_format(
            domain: &domain::Domain,
            size: usize,
            format: Format,
        ) -> Result<Self, Error> {
            let mut attr = fi_cq_attr {
                size,
                flags: 0,
                format: format as _,
                wait_obj: WaitObject::NONE as _,
                signaling_vector: 0,
                wait_cond: fi_cq_wait_cond_FI_CQ_COND_NONE as _,
                wait_set: core::ptr::null_mut(),
            };

            let mut cq_ptr: *mut fid_cq = core::ptr::null_mut();
            let ret = unsafe {
                fi_cq_open(
                    domain.as_ptr(),
                    &mut attr,
                    &mut cq_ptr,
                    core::ptr::null_mut(),
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            let fid = NonNull::new(cq_ptr).expect("completion queue pointer was null");
            let inner = Arc::new(Inner { fid });
            Ok(Self { inner })
        }

        /// Read and process completion entries, automatically freeing MemoryRegions
        ///
        /// This reads completions and automatically reconstructs and drops the MemoryRegion
        /// from each completion context. The callback receives both the completion entry
        /// and the reconstructed MemoryRegion.
        ///
        /// # Parameters
        /// - `limit`: Maximum number of entries to read (must be <= MAX_LEN)
        /// - `callback`: Called for each completion with (memory_region)
        ///
        /// The MemoryRegion will be dropped after the callback returns.
        pub fn read<F, const MAX_LEN: usize>(
            &self,
            limit: usize,
            mut callback: F,
        ) -> Result<usize, Error>
        where
            F: FnMut(memory_registration::MemoryRegion),
        {
            debug_assert!(limit <= MAX_LEN, "limit exceeds MAX_LEN");
            let limit = limit.min(MAX_LEN);

            let mut entries: [fi_cq_msg_entry; MAX_LEN] = [unsafe { core::mem::zeroed() }; MAX_LEN];
            let count = unsafe { self.read_msg(&mut entries[..limit])? };

            for entry in &entries[..count] {
                if !entry.op_context.is_null() {
                    dbg!(entry, CompletionFlags::from_bits_truncate(entry.flags));
                    let mr = unsafe {
                        memory_registration::MemoryRegion::from_context(
                            entry.op_context,
                            Some(entry.len),
                        )
                    };
                    callback(mr);
                    // mr is automatically dropped here
                }
            }

            Ok(count)
        }

        /// Read MSG format completion entries (non-blocking)
        unsafe fn read_msg(&self, entries: &mut [fi_cq_msg_entry]) -> Result<usize, Error> {
            let ret = unsafe {
                fi_cq_read(
                    self.inner.fid.as_ptr(),
                    entries.as_mut_ptr() as *mut core::ffi::c_void,
                    entries.len(),
                )
            };

            if ret < 0 {
                if ret == -(FI_EAGAIN as isize) {
                    return Ok(0);
                }
                Err(Error::from_code(ret as i32))
            } else {
                Ok(ret as usize)
            }
        }

        /// Read completion entries (non-blocking)
        ///
        /// Returns the number of entries read, or an error.
        /// Returns 0 if no completions are available (FI_EAGAIN).
        ///
        /// **Important**: The `op_context` field in each entry contains a MemoryRegion
        /// pointer. You must call `memory_registration::MemoryRegion::from_context()`
        /// on each entry to properly free the memory region.
        pub unsafe fn read_raw(&self, entries: &mut [fi_cq_entry]) -> Result<usize, Error> {
            let ret = unsafe {
                fi_cq_read(
                    self.inner.fid.as_ptr(),
                    entries.as_mut_ptr() as *mut core::ffi::c_void,
                    entries.len(),
                )
            };

            if ret < 0 {
                // -FI_EAGAIN means no completions available, return 0
                if ret == -(FI_EAGAIN as isize) {
                    return Ok(0);
                }
                Err(Error::from_code(ret as i32))
            } else {
                Ok(ret as usize)
            }
        }

        /// Read completion entries with timeout (blocking)
        ///
        /// Blocks until at least one completion is available or timeout expires.
        /// A timeout of -1 means wait indefinitely.
        pub fn sread(&self, entries: &mut [fi_cq_entry], timeout_ms: i32) -> Result<usize, Error> {
            let ret = unsafe {
                fi_cq_sread(
                    self.inner.fid.as_ptr(),
                    entries.as_mut_ptr() as *mut core::ffi::c_void,
                    entries.len(),
                    core::ptr::null_mut(),
                    timeout_ms,
                )
            };

            if ret < 0 {
                Err(Error::from_code(ret as i32))
            } else {
                Ok(ret as usize)
            }
        }

        /// Read detailed completion data
        pub fn readfrom(
            &self,
            entries: &mut [fi_cq_msg_entry],
            src_addrs: &mut [fi_addr_t],
        ) -> Result<usize, Error> {
            debug_assert_eq!(entries.len(), src_addrs.len());

            let ret = unsafe {
                fi_cq_readfrom(
                    self.inner.fid.as_ptr(),
                    entries.as_mut_ptr() as *mut core::ffi::c_void,
                    entries.len(),
                    src_addrs.as_mut_ptr(),
                )
            };

            if ret < 0 {
                if ret == -(FI_EAGAIN as isize) {
                    return Ok(0);
                }
                Err(Error::from_code(ret as i32))
            } else {
                Ok(ret as usize)
            }
        }

        /// Read error entry from the completion queue
        ///
        /// Should be called when a completion operation returns an error to get details.
        pub fn readerr(&self) -> Result<fi_cq_err_entry, Error> {
            let mut err_entry = unsafe { core::mem::zeroed::<fi_cq_err_entry>() };

            let ret = unsafe {
                fi_cq_readerr(
                    self.inner.fid.as_ptr(),
                    &mut err_entry,
                    0, // flags
                )
            };

            if ret < 0 {
                return Err(Error::from_code(ret as i32));
            }

            Ok(err_entry)
        }

        pub(crate) fn as_ptr(&self) -> *mut fid_cq {
            self.inner.fid.as_ptr()
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            unsafe {
                if let Some(close) = (*self.fid.as_ptr())
                    .fid
                    .ops
                    .as_ref()
                    .and_then(|ops| (*ops).close)
                {
                    let _ = close(&mut (*self.fid.as_ptr()).fid as *mut fid);
                }
            }
        }
    }

    impl fmt::Debug for CompletionQueue {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("CompletionQueue").finish_non_exhaustive()
        }
    }
}

pub mod address_vector {
    use super::*;
    use std::sync::Arc;

    define_enum!(
        pub enum Type {
            /// Address vector uses a map for lookups
            MAP = fi_av_type_FI_AV_MAP,
            /// Address vector uses a table for lookups
            TABLE = fi_av_type_FI_AV_TABLE,
            Unspecified = fi_av_type_FI_AV_UNSPEC,
        }
    );

    #[derive(Clone)]
    pub struct AddressVector {
        inner: Arc<Inner>,
    }

    unsafe impl Send for AddressVector {}
    unsafe impl Sync for AddressVector {}

    struct Inner {
        fid: NonNull<fid_av>,
    }

    impl AddressVector {
        /// Open an address vector with default settings (TABLE type)
        ///
        /// Uses FI_AV_TABLE as FI_AV_MAP was deprecated in libfabric 2.x
        pub fn open(domain: &domain::Domain) -> Result<Self, Error> {
            Self::open_with_attr(domain, Type::TABLE, 64)
        }

        /// Open an address vector with specific attributes
        pub fn open_with_attr(
            domain: &domain::Domain,
            av_type: Type,
            count: usize,
        ) -> Result<Self, Error> {
            let mut attr = fi_av_attr {
                type_: av_type as _,
                rx_ctx_bits: 0,
                count,
                ep_per_node: 0,
                name: core::ptr::null(),
                map_addr: core::ptr::null_mut(),
                flags: 0,
            };

            let mut av_ptr: *mut fid_av = core::ptr::null_mut();
            let ret = unsafe {
                fi_av_open(
                    domain.as_ptr(),
                    &mut attr,
                    &mut av_ptr,
                    core::ptr::null_mut(),
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            let fid = NonNull::new(av_ptr).expect("address vector pointer was null");
            let inner = Arc::new(Inner { fid });
            Ok(Self { inner })
        }

        /// Insert a single address into the address vector
        ///
        /// Returns the libfabric address handle (fi_addr_t) for this address.
        pub fn insert(&self, addr: &[u8]) -> Result<fi_addr_t, Error> {
            let mut fi_addr: fi_addr_t = 0;
            let ret = unsafe {
                fi_av_insert(
                    self.inner.fid.as_ptr(),
                    addr.as_ptr() as *const core::ffi::c_void,
                    1,
                    &mut fi_addr,
                    0,
                    core::ptr::null_mut(),
                )
            };

            if ret < 0 {
                return Err(Error::from_code(ret as i32));
            }

            Ok(fi_addr)
        }

        /// Insert multiple addresses into the address vector
        ///
        /// Returns a Vec of libfabric address handles, one per input address.
        /// All addresses must be the same size.
        pub fn insert_multiple(&self, addrs: &[&[u8]]) -> Result<Vec<fi_addr_t>, Error> {
            if addrs.is_empty() {
                return Ok(Vec::new());
            }

            // Flatten addresses into a single buffer
            let addr_len = addrs[0].len();
            let mut addr_buf = Vec::with_capacity(addr_len * addrs.len());
            for addr in addrs {
                if addr.len() != addr_len {
                    return Err(Error::from_code(-(FI_EINVAL as i32)));
                }
                addr_buf.extend_from_slice(addr);
            }

            let mut fi_addrs = vec![0_u64; addrs.len()];
            let ret = unsafe {
                fi_av_insert(
                    self.inner.fid.as_ptr(),
                    addr_buf.as_ptr() as *const core::ffi::c_void,
                    addrs.len(),
                    fi_addrs.as_mut_ptr(),
                    0,
                    core::ptr::null_mut(),
                )
            };

            if ret < 0 {
                return Err(Error::from_code(ret as i32));
            }

            if ret as usize != addrs.len() {
                return Err(Error::from_code(-(FI_EINVAL as i32)));
            }

            Ok(fi_addrs)
        }

        /// Remove an address from the address vector
        pub fn remove(&self, fi_addr: fi_addr_t) -> Result<(), Error> {
            let ret = unsafe {
                fi_av_remove(
                    self.inner.fid.as_ptr(),
                    &fi_addr as *const fi_addr_t as *mut fi_addr_t,
                    1,
                    0,
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            Ok(())
        }

        /// Lookup the address string for a given fi_addr_t
        ///
        /// Returns the raw address bytes. The format depends on the provider.
        pub fn lookup(&self, fi_addr: fi_addr_t, addr_buf: &mut [u8]) -> Result<usize, Error> {
            let mut addrlen = addr_buf.len();
            let ret = unsafe {
                fi_av_lookup(
                    self.inner.fid.as_ptr(),
                    fi_addr,
                    addr_buf.as_mut_ptr() as *mut core::ffi::c_void,
                    &mut addrlen,
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            Ok(addrlen)
        }

        pub(crate) fn as_ptr(&self) -> *mut fid_av {
            self.inner.fid.as_ptr()
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            unsafe {
                if let Some(close) = (*self.fid.as_ptr())
                    .fid
                    .ops
                    .as_ref()
                    .and_then(|ops| (*ops).close)
                {
                    let _ = close(&mut (*self.fid.as_ptr()).fid as *mut fid);
                }
            }
        }
    }

    impl fmt::Debug for AddressVector {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("AddressVector").finish_non_exhaustive()
        }
    }
}

pub mod addr {
    use super::*;

    define_enum!(
        pub enum Format {
            EFA = FI_ADDR_EFA,
            PSMX2 = FI_ADDR_PSMX2,
            PSMX3 = FI_ADDR_PSMX3,
            STR = FI_ADDR_STR,
            Unspecified = FI_FORMAT_UNSPEC,
            SOCKADDR = FI_SOCKADDR,
            SOCKADDR_IB = FI_SOCKADDR_IB,
            /// IPv4
            SOCKADDR_IN = FI_SOCKADDR_IN,
            /// IPv6
            SOCKADDR_IN6 = FI_SOCKADDR_IN6,
        }
    );
}

pub mod fabric {
    use super::*;
    use std::sync::Arc;

    pub struct Fabric {
        inner: Arc<Inner>,
    }

    struct Inner {
        fid: NonNull<fid_fabric>,
    }

    impl Fabric {
        pub fn open(info: &info::Info) -> Result<Self, Error> {
            let mut fabric_ptr: *mut fid_fabric = core::ptr::null_mut();
            let ret = unsafe {
                fi_fabric(
                    info.info.fabric_attr,
                    &mut fabric_ptr,
                    core::ptr::null_mut(),
                )
            };

            if ret != 0 {
                return Err(Error::from_code(ret));
            }

            let fid = NonNull::new(fabric_ptr).expect("fabric pointer was null");
            let inner = Arc::new(Inner { fid });
            Ok(Self { inner })
        }

        pub fn open_domain(&self, info: &info::Info) -> Result<domain::Domain, Error> {
            domain::Domain::open(self, info)
        }

        pub(crate) fn as_ptr(&self) -> *mut fid_fabric {
            self.inner.fid.as_ptr()
        }
    }

    impl Drop for Inner {
        fn drop(&mut self) {
            unsafe {
                if let Some(close) = (*self.fid.as_ptr())
                    .fid
                    .ops
                    .as_ref()
                    .and_then(|ops| (*ops).close)
                {
                    close(&mut (*self.fid.as_ptr()).fid as *mut fid as *mut fid);
                }
            }
        }
    }

    impl fmt::Debug for Fabric {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Fabric").finish_non_exhaustive()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ByteVec;
    use bytes::BytesMut;

    #[test]
    fn basic_info_query() {
        let list = info::Query::new().build();
        for info in list.iter() {
            println!("{:#?}", info);
        }
    }

    #[test]
    fn tcp_rxm_ping_pong() {
        // Query for TCP endpoint with RXM provider
        let mut query = info::Query::new()
            .capabilities(Capabilities::MSG | Capabilities::SEND | Capabilities::RECV);

        query
            .endpoint()
            .type_(endpoint::Type::RDM)
            .protocol(endpoint::Protocol::RXM);

        query
            .domain()
            .memory_registration(memory_registration::Mode::PROV_KEY)
            .av_type(domain::AvType::TABLE);

        let list = query.build();

        // Find a suitable provider
        let info = list
            .iter()
            .find(|info| {
                info.endpoint_attr().protocol() == endpoint::Protocol::RXM
                    && info.addr_format() == addr::Format::SOCKADDR_IN
                    && info.domain_attr().name().is_some_and(|name| name == c"lo")
            })
            .expect("No TCP/RXM provider found");

        println!("Using provider: {:#?}", info);

        // Open fabric and domain
        let fabric = fabric::Fabric::open(&info).expect("Failed to open fabric");
        let domain = fabric.open_domain(&info).expect("Failed to open domain");

        // Create completion queues for both endpoints (using MSG format to get message length)
        let cq_size = 128;
        let sender_cq = completion_queue::CompletionQueue::open_with_format(
            &domain,
            cq_size,
            completion_queue::Format::MSG,
        )
        .expect("Failed to open CQ");
        let receiver_cq = completion_queue::CompletionQueue::open_with_format(
            &domain,
            cq_size,
            completion_queue::Format::MSG,
        )
        .expect("Failed to open CQ");

        // Create address vector
        let av = address_vector::AddressVector::open(&domain).expect("Failed to open AV");

        // Create sender endpoint
        let sender_ep =
            endpoint::Endpoint::open(&domain, &info).expect("Failed to create sender endpoint");

        // Bind sender CQ and AV
        sender_ep
            .bind_cq(&sender_cq, (FI_TRANSMIT | FI_RECV) as _)
            .expect("Failed to bind sender CQ");
        sender_ep.bind_av(&av).expect("Failed to bind sender AV");

        // Create receiver endpoint
        let receiver_ep =
            endpoint::Endpoint::open(&domain, &info).expect("Failed to create receiver endpoint");

        // Bind receiver CQ and AV
        receiver_ep
            .bind_cq(&receiver_cq, (FI_TRANSMIT | FI_RECV) as _)
            .expect("Failed to bind receiver CQ");
        receiver_ep
            .bind_av(&av)
            .expect("Failed to bind receiver AV");

        // Enable endpoints
        sender_ep.enable().expect("Failed to enable sender");
        receiver_ep.enable().expect("Failed to enable receiver");

        // For RDM endpoints, we need to register addresses in the address vector
        // Use the src_addr from the provider info to set up localhost communication
        let receiver_addr = {
            let src_addr = receiver_ep.getname().unwrap();
            println!("Registering receiver address: {} bytes", src_addr.len());
            av.insert(&src_addr)
                .expect("Failed to insert receiver address into AV")
        };

        println!("Receiver fi_addr_t: {}", receiver_addr);

        // Prepare test data
        let send_data = {
            let mut chunks = ByteVec::new();
            chunks.push_back("Hello, ".into());
            chunks.push_back("libfabric".into());
            chunks.push_back(" ping-pong!".into());
            chunks
        };
        let send_len = send_data.len();

        // Register send memory region
        let send_mr = memory_registration::Send::register(
            &domain,
            send_data.clone(),
            memory_registration::Access::SEND,
            123,
        )
        .expect("Failed to register send memory");

        println!("Send MR key: {}", send_mr.key());

        // Register receive memory region
        let recv_data = BytesMut::with_capacity(send_len);
        let recv_mr = memory_registration::Receive::register(
            &domain,
            recv_data,
            memory_registration::Access::RECV,
            124,
        )
        .expect("Failed to register receive memory");

        println!("Recv MR key: {}", recv_mr.key());

        // Post receive buffer first
        receiver_ep
            .recv(recv_mr, None)
            .expect("Failed to post receive");

        println!("Receive buffer posted, sending message...");

        // Use thread::scope to run sender and receiver concurrently
        let recv_buffer = std::thread::scope(|s| {
            // Receiver thread
            let recv_handle = s.spawn(|| {
                println!("Receiver thread: waiting for message...");
                for _ in 0..1000 {
                    let mut recv_buffer = None;
                    match receiver_cq.read::<_, 1>(1, |mr| {
                        println!("Receive completed: {:?}", mr);
                        if let memory_registration::MemoryRegion::Receive(recv_mr) = mr {
                            recv_buffer = Some(recv_mr);
                        }
                    }) {
                        Ok(count) if count > 0 => return recv_buffer,
                        Ok(_) => {} // No completions yet
                        Err(e) if e.code() == -(FI_EAVAIL as i32) => {
                            // Error available in CQ, read it
                            let err_entry = receiver_cq.readerr().expect("Failed to read error");
                            panic!(
                                "CQ error: code={}, prov_errno={}",
                                err_entry.err, err_entry.prov_errno
                            );
                        }
                        Err(e) => panic!("Failed to read receive completion: {:?}", e),
                    }
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
                panic!("Receive did not complete");
            });

            // Sender thread
            let send_handle = s.spawn(|| {
                println!("Sender thread: sending message...");

                // Send message with retry on FI_EAGAIN
                loop {
                    match sender_ep.send(send_mr.clone(), receiver_addr) {
                        Ok(()) => break,
                        Err(e) if e.code() == -(FI_EAGAIN as i32) => {
                            let _ = unsafe { sender_cq.read_raw(&mut []) };
                            std::thread::sleep(std::time::Duration::from_micros(100));
                            continue;
                        }
                        Err(e) => panic!("Failed to send message: {:?}", e),
                    }
                }

                println!("Message posted, waiting for send completion...");

                // Wait for send completion
                for _ in 0..1000 {
                    let count = sender_cq
                        .read::<_, 1>(1, |mr| {
                            println!("Send completed: {:?}", mr);
                        })
                        .expect("Failed to read send completion");

                    if count > 0 {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(100));
                }
                panic!("Send did not complete");
            });

            // Wait for both to complete
            send_handle.join().unwrap();
            recv_handle.join().unwrap()
        });

        println!("Both send and receive completed successfully");

        // Verify received data
        if let Some(recv_mr) = recv_buffer {
            let recv_bytes = recv_mr.buffer();
            let recv_slice = &recv_bytes[..send_len];
            assert_eq!(
                recv_slice, b"Hello, libfabric ping-pong!",
                "Received data does not match sent data"
            );
            println!(
                "Data verified: {:?}",
                std::str::from_utf8(recv_slice).unwrap()
            );
        } else {
            panic!("No receive buffer found");
        }

        println!("TCP/RXM ping-pong test completed successfully!");
    }
}
