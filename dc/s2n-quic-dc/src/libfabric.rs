use bitflags::bitflags;
use core::{fmt, ptr::NonNull};
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
        info: InnerInfo,
    }

    struct InnerInfo(NonNull<fi_info>);

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

    pub struct Domain<'a>(pub(super) &'a fid_domain);

    impl fmt::Debug for Domain<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("fid_domain").finish_non_exhaustive()
        }
    }

    impl Domain<'_> {
        // TODO
    }

    pub struct Attributes<'a>(pub(super) &'a fi_domain_attr);

    impl fmt::Debug for Attributes<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("fi_domain_attr")
                .field("name", &self.name())
                .field("domain", &self.domain())
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
        pub fn domain(&self) -> Option<Domain<'_>> {
            unsafe {
                let domain = self.0.domain;
                if domain.is_null() {
                    return None;
                }
                Some(Domain(&*self.0.domain))
            }
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_info_query() {
        let list = info::Query::new().build();
        for info in list.iter() {
            println!("{:#?}", info);
        }
    }
}
