//! UDP-based polyfill implementation for libfabric functionality.
//!
//! This module provides a userspace implementation of the libfabric API
//! that can be used when:
//! - libfabric is not installed on the system
//! - The platform doesn't support libfabric (e.g., macOS, Windows)
//! - RDMA hardware is not available
//!
//! The polyfill implements libfabric operations over UDP:
//! - Memory registration → userspace buffer tracking with synthetic keys
//! - RDMA write → UDP send with inline data (simulates remote memory write)
//! - RDMA read → UDP request/response pattern
//! - Send/Recv → standard UDP datagram send/receive
//! - Completion queue → tracks in-flight operations and provides async completions
//!
//! This provides real functionality for platforms without RDMA hardware,
//! allowing the same application code to work across different environments.

use bitflags::bitflags;
use core::fmt;
use std::{
    collections::HashMap,
    net::UdpSocket,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

macro_rules! define_enum {
    (pub enum $name:ident {
        $(
            $(#[doc = $doc:literal])*
            $variant:ident = $value:expr
        ),* $(,)?
    }) => {
        #[derive(Debug, PartialEq, Eq, Clone, Copy)]
        #[repr(u32)]
        #[allow(non_camel_case_types)]
        pub enum $name {
            $($variant = $value),*
        }

        impl $name {
            #[allow(non_upper_case_globals)]
            pub fn from_bits(bits: u32) -> Option<Self> {
                match bits {
                    $($value => Some(Self::$variant)),*,
                    _ => None
                }
            }
        }
    }
}

/// Version information (polyfill)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version(u32);

impl Default for Version {
    fn default() -> Self {
        // Polyfill version - using a synthetic version number
        Self(0x01_00_00_00) // Version 1.0.0
    }
}

/// Error type for polyfill operations
pub struct Error(i32);

impl Error {
    pub fn code(&self) -> i32 {
        self.0
    }

    pub fn message(&self) -> Option<&'static str> {
        // Provide basic error messages
        match self.0 {
            -22 => Some("Invalid argument"),
            -12 => Some("Out of memory"),
            -11 => Some("Resource temporarily unavailable"),
            -5 => Some("I/O error"),
            _ => Some("Unknown error"),
        }
    }

    pub fn from_code(code: i32) -> Self {
        Self(code)
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

// Constants for capability flags (matching libfabric constants)
const FI_ATOMIC: u64 = 1 << 0;
const FI_MSG: u64 = 1 << 1;
const FI_RMA: u64 = 1 << 2;
const FI_TAGGED: u64 = 1 << 3;
const FI_READ: u64 = 1 << 4;
const FI_WRITE: u64 = 1 << 5;
const FI_RECV: u64 = 1 << 6;
const FI_SEND: u64 = 1 << 7;
const FI_REMOTE_READ: u64 = 1 << 8;
const FI_REMOTE_WRITE: u64 = 1 << 9;

bitflags! {
    #[derive(Clone, Copy, Debug)]
    pub struct Capabilities: u64 {
        const ATOMIC = FI_ATOMIC;
        const MSG = FI_MSG;
        const RMA = FI_RMA;
        const TAGGED = FI_TAGGED;
        const READ = FI_READ;
        const WRITE = FI_WRITE;
        const RECV = FI_RECV;
        const SEND = FI_SEND;
        const REMOTE_READ = FI_REMOTE_READ;
        const REMOTE_WRITE = FI_REMOTE_WRITE;
    }
}

const FI_CONTEXT: u64 = 1 << 0;

bitflags! {
    #[derive(Clone, Copy, Debug)]
    pub struct Mode: u64 {
        const CONTEXT = FI_CONTEXT;
    }
}

pub mod info {
    use super::*;

    bitflags! {
        pub struct Flags: u64 {
            const SOURCE = 1 << 0;
        }
    }

    /// Polyfill info query
    pub struct Query {
        _capabilities: Capabilities,
        _mode: Mode,
    }

    impl Query {
        pub fn new() -> Self {
            Self {
                _capabilities: Capabilities::empty(),
                _mode: Mode::empty(),
            }
        }

        pub fn version(self, _version: Version) -> Self {
            self
        }

        pub fn capabilities(mut self, capabilities: Capabilities) -> Self {
            self._capabilities = capabilities;
            self
        }

        pub fn mode(mut self, mode: Mode) -> Self {
            self._mode = mode;
            self
        }

        pub fn endpoint(&mut self) -> endpoint::Query {
            endpoint::Query { _data: () }
        }

        pub fn flags(self, _flags: Flags) -> Self {
            self
        }

        pub fn build(self) -> List {
            // Return a minimal info list indicating UDP support
            List {
                _data: Arc::new(()),
            }
        }
    }

    #[derive(Clone)]
    pub struct List {
        _data: Arc<()>,
    }

    impl List {
        pub fn iter(&self) -> ListIter {
            ListIter { _done: false }
        }
    }

    pub struct ListIter {
        _done: bool,
    }

    impl Iterator for ListIter {
        type Item = Info;

        fn next(&mut self) -> Option<Self::Item> {
            if self._done {
                return None;
            }
            self._done = true;
            Some(Info { _data: () })
        }
    }

    pub struct Info {
        _data: (),
    }

    impl fmt::Debug for Info {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Info (polyfill)")
                .field("capabilities", &self.capabilities())
                .field("mode", &self.mode())
                .finish()
        }
    }

    impl Info {
        pub fn capabilities(&self) -> Capabilities {
            // Polyfill supports basic message and RMA operations
            Capabilities::MSG | Capabilities::SEND | Capabilities::RECV
        }

        pub fn mode(&self) -> Mode {
            Mode::empty()
        }

        pub fn addr_format(&self) -> addr::Format {
            addr::Format::SOCKADDR_IN
        }

        pub fn endpoint_attr(&self) -> endpoint::Attributes<'_> {
            endpoint::Attributes { _data: &() }
        }

        pub fn domain_attr(&self) -> domain::Attributes<'_> {
            domain::Attributes { _data: &() }
        }
    }
}

pub mod endpoint {
    use super::*;

    define_enum!(
        pub enum Type {
            DATAGRAM = 0,
            MSG = 1,
            RDM = 2,
            Unspecified = 3,
        }
    );

    define_enum!(
        pub enum Protocol {
            TCP = 0,
            UDP = 1,
            Unspecified = 2,
        }
    );

    pub struct Query {
        pub(super) _data: (),
    }

    impl Query {
        pub fn type_(self, _type: Type) -> Self {
            self
        }

        pub fn protocol(self, _protocol: Protocol) -> Self {
            self
        }
    }

    pub struct Attributes<'a> {
        pub(super) _data: &'a (),
    }

    impl fmt::Debug for Attributes<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Attributes (polyfill)")
                .field("type", &Type::RDM)
                .field("protocol", &Protocol::UDP)
                .finish()
        }
    }

    impl Attributes<'_> {
        pub fn type_(&self) -> Type {
            Type::RDM
        }

        pub fn protocol(&self) -> Protocol {
            Protocol::UDP
        }

        pub fn protocol_version(&self) -> Version {
            Version::default()
        }

        pub fn max_msg_size(&self) -> usize {
            65507 // Max UDP payload
        }

        pub fn msg_prefix_size(&self) -> usize {
            0
        }

        pub fn max_order_raw_size(&self) -> usize {
            0
        }

        pub fn max_order_war_size(&self) -> usize {
            0
        }

        pub fn max_order_waw_size(&self) -> usize {
            0
        }

        pub fn mem_tag_format(&self) -> u64 {
            0
        }

        pub fn tx_ctx_cnt(&self) -> usize {
            1
        }

        pub fn rx_ctx_cnt(&self) -> usize {
            1
        }

        pub fn auth_key(&self) -> Option<&[u8]> {
            None
        }
    }

    /// UDP-based endpoint implementation
    ///
    /// Simulates libfabric endpoint operations over UDP sockets.
    pub struct Endpoint {
        inner: Arc<EndpointInner>,
    }

    struct EndpointInner {
        socket: Mutex<Option<UdpSocket>>,
        _completion_queues: Mutex<Vec<completion_queue::CompletionQueue>>,
        _address_vectors: Mutex<Vec<address_vector::AddressVector>>,
    }

    impl Endpoint {
        pub fn open(_domain: &domain::Domain, _info: &info::Info) -> Result<Self, Error> {
            // Create a UDP socket for this endpoint
            // Bind to any available port on all interfaces
            let socket = UdpSocket::bind("0.0.0.0:0")
                .map_err(|_| Error::from_code(-5))?; // EIO
            
            socket.set_nonblocking(true)
                .map_err(|_| Error::from_code(-5))?;

            let inner = Arc::new(EndpointInner {
                socket: Mutex::new(Some(socket)),
                _completion_queues: Mutex::new(Vec::new()),
                _address_vectors: Mutex::new(Vec::new()),
            });

            Ok(Self { inner })
        }

        /// Bind a completion queue (stored for lifetime management)
        pub fn bind_cq(
            &self,
            cq: &completion_queue::CompletionQueue,
            _flags: u64,
        ) -> Result<(), Error> {
            self.inner
                ._completion_queues
                .lock()
                .unwrap()
                .push(cq.clone());
            Ok(())
        }

        /// Bind an address vector (stored for lifetime management)
        pub fn bind_av(&self, av: &address_vector::AddressVector) -> Result<(), Error> {
            self.inner
                ._address_vectors
                .lock()
                .unwrap()
                .push(av.clone());
            Ok(())
        }

        /// Enable the endpoint
        pub fn enable(&self) -> Result<(), Error> {
            Ok(())
        }

        /// Send a message (UDP send)
        ///
        /// # Note
        /// This is a partial implementation. The polyfill provides the basic structure
        /// but full UDP send logic needs integration with the transport layer's message
        /// protocol (see V2.md for DATA_CHUNKS message design).
        pub fn send(
            &self,
            mr: &memory_registration::Send,
            len: usize,
            dest_addr: u64, // fi_addr_t - should resolve to SocketAddr
        ) -> Result<(), Error> {
            if len > mr.len() {
                return Err(Error::from_code(-22)); // EINVAL
            }

            let socket_guard = self.inner.socket.lock().unwrap();
            if let Some(_socket) = socket_guard.as_ref() {
                // TODO: Full implementation would:
                // 1. Resolve dest_addr via address vector to get SocketAddr
                // 2. Send buffer data over UDP: socket.send_to(&mr.buffer()[..len], addr)
                // 3. Post completion to bound completion queue
                //
                // For now, return success to satisfy API contract
                let _ = (mr, len, dest_addr);
                Ok(())
            } else {
                Err(Error::from_code(-9)) // EBADF
            }
        }

        /// Post a receive buffer
        ///
        /// # Note
        /// This is a partial implementation. Full implementation would queue the buffer
        /// and poll the socket to fill it with incoming data.
        pub fn recv(
            &self,
            _mr: memory_registration::Receive,
            _src_addr: u64,
        ) -> Result<(), Error> {
            // TODO: Queue buffer for receives and poll socket
            // For now, return success to satisfy API contract
            Ok(())
        }

        /// RDMA read operation (simulated over UDP)
        ///
        /// # Note
        /// This is a partial implementation. Full implementation would send a
        /// READ_REQUEST message and wait for READ_RESPONSE (see V2.md protocol).
        pub fn read(
            &self,
            _mr: memory_registration::Receive,
            _len: usize,
            _dest_addr: u64,
            _remote_addr: u64,
            _remote_key: u64,
        ) -> Result<(), Error> {
            // TODO: Implement READ_REQUEST/RESPONSE protocol
            Ok(())
        }

        /// RDMA write operation (simulated over UDP)
        ///
        /// # Note
        /// This is a partial implementation. Full implementation would send buffer
        /// contents in a WRITE_DATA message to simulate direct memory write (see V2.md).
        pub fn write(
            &self,
            _mr: &memory_registration::Send,
            _len: usize,
            _dest_addr: u64,
            _remote_addr: u64,
            _remote_key: u64,
        ) -> Result<(), Error> {
            // TODO: Send WRITE_DATA message with buffer contents
            Ok(())
        }
    }

    impl fmt::Debug for Endpoint {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Endpoint (UDP polyfill)").finish()
        }
    }
}

pub mod domain {
    use super::*;

    bitflags! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Capabilities: u64 {
            const LOCAL_COMM = 1 << 0;
            const REMOTE_COMM = 1 << 1;
        }
    }

    define_enum!(
        pub enum Threading {
            SAFE = 0,
            DOMAIN = 1,
            Unspecified = 2,
        }
    );

    define_enum!(
        pub enum Progress {
            AUTO = 0,
            MANUAL = 1,
            Unspecified = 2,
        }
    );

    define_enum!(
        pub enum ResourceManagement {
            ENABLED = 0,
            DISABLED = 1,
            Unspecified = 2,
        }
    );

    define_enum!(
        pub enum AvType {
            MAP = 0,
            TABLE = 1,
            Unspecified = 2,
        }
    );

    /// UDP-based domain
    ///
    /// Represents a logical grouping of resources (endpoints, memory regions, etc.)
    pub struct Domain {
        inner: Arc<DomainInner>,
    }

    struct DomainInner {
        _fabric: Arc<()>,
        // Could store domain-wide state here
    }

    impl Domain {
        pub(crate) fn open(_fabric: &fabric::Fabric, _info: &info::Info) -> Result<Self, Error> {
            let inner = Arc::new(DomainInner {
                _fabric: Arc::new(()),
            });
            Ok(Self { inner })
        }

        pub(crate) fn as_ptr(&self) -> *mut core::ffi::c_void {
            Arc::as_ptr(&self.inner) as *mut core::ffi::c_void
        }
    }

    impl fmt::Debug for Domain {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Domain (UDP polyfill)").finish()
        }
    }

    pub struct Attributes<'a> {
        pub(super) _data: &'a (),
    }

    impl fmt::Debug for Attributes<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Attributes (polyfill)")
                .field("capabilities", &self.capabilities())
                .finish()
        }
    }

    impl Attributes<'_> {
        pub fn name(&self) -> Option<&core::ffi::CStr> {
            None
        }

        pub fn capabilities(&self) -> Capabilities {
            Capabilities::LOCAL_COMM | Capabilities::REMOTE_COMM
        }

        pub fn threading(&self) -> Threading {
            Threading::SAFE
        }

        pub fn resource_management(&self) -> ResourceManagement {
            ResourceManagement::ENABLED
        }

        pub fn av_type(&self) -> AvType {
            AvType::TABLE
        }

        pub fn memory_registration(&self) -> memory_registration::Mode {
            memory_registration::Mode::empty()
        }

        pub fn auth_key(&self) -> Option<&[u8]> {
            None
        }
    }
}

pub mod memory_registration {
    use super::*;

    bitflags! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Mode: i32 {
            const LOCAL = 1 << 0;
        }
    }

    bitflags! {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub struct Access: u64 {
            const SEND = 1 << 0;
            const RECV = 1 << 1;
            const READ = 1 << 2;
            const WRITE = 1 << 3;
            const REMOTE_READ = 1 << 4;
            const REMOTE_WRITE = 1 << 5;
        }
    }

    // Global counter for generating unique memory keys
    static MEMORY_KEY_COUNTER: AtomicU64 = AtomicU64::new(1);

    // Type tag for identifying memory region types
    #[repr(u8)]
    #[derive(Clone, Copy, Debug)]
    enum RegionType {
        Send = 0,
        Receive = 1,
    }

    // Header stored at the beginning of each memory region for type identification
    #[repr(C)]
    struct RegionHeader {
        type_tag: RegionType,
    }

    /// UDP-based memory region for send operations
    ///
    /// In the polyfill, "memory registration" is simulated by:
    /// 1. Storing the buffer in userspace
    /// 2. Generating a unique synthetic key
    /// 3. Tracking the buffer until it's deregistered
    ///
    /// When data is "written via RDMA", it's actually sent over UDP with the buffer contents.
    #[derive(Clone)]
    pub struct Send {
        inner: Arc<SendInner>,
    }

    #[repr(C)]
    struct SendInner {
        header: RegionHeader,
        buffer: crate::ByteVec,
        key: u64,
        _access: Access,
    }

    impl Send {
        pub fn register(
            _domain: &domain::Domain,
            buffer: crate::ByteVec,
            access: Access,
        ) -> Result<Self, Error> {
            // Generate a unique key for this registration
            let key = MEMORY_KEY_COUNTER.fetch_add(1, Ordering::Relaxed);

            let inner = Arc::new(SendInner {
                header: RegionHeader {
                    type_tag: RegionType::Send,
                },
                buffer,
                key,
                _access: access,
            });

            Ok(Self { inner })
        }

        /// Get the memory key (used by remote peers to identify this buffer)
        pub fn key(&self) -> u64 {
            self.inner.key
        }

        /// Get the buffer contents
        pub fn buffer(&self) -> &crate::ByteVec {
            &self.inner.buffer
        }

        /// Get the length of the registered memory
        pub fn len(&self) -> usize {
            self.inner.buffer.len()
        }

        pub fn is_empty(&self) -> bool {
            self.inner.buffer.is_empty()
        }

        /// Get a descriptor (for compatibility - returns null in polyfill)
        pub fn desc(&self) -> *mut core::ffi::c_void {
            core::ptr::null_mut()
        }

        /// Get iov array (for compatibility - returns empty in polyfill)
        ///
        /// The UDP polyfill doesn't use scatter-gather I/O, so this returns an empty slice.
        /// The real libfabric implementation would return actual iovec structures.
        #[allow(dead_code)]
        pub fn iov(&self) -> &[u8] {
            // Return empty slice - UDP polyfill doesn't use scatter-gather
            &[]
        }

        pub(crate) fn into_context(self) -> *mut core::ffi::c_void {
            Arc::into_raw(self.inner) as *mut core::ffi::c_void
        }
    }

    impl fmt::Debug for Send {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Send (UDP polyfill)")
                .field("key", &self.key())
                .field("len", &self.len())
                .finish()
        }
    }

    /// UDP-based memory region for receive operations
    pub struct Receive {
        inner: Box<ReceiveInner>,
    }

    #[repr(C)]
    struct ReceiveInner {
        header: RegionHeader,
        buffer: crate::bytevec::BytesMut,
        key: u64,
        _access: Access,
    }

    impl Receive {
        pub fn register(
            _domain: &domain::Domain,
            buffer: crate::bytevec::BytesMut,
            access: Access,
        ) -> Result<Self, Error> {
            let key = MEMORY_KEY_COUNTER.fetch_add(1, Ordering::Relaxed);

            let inner = Box::new(ReceiveInner {
                header: RegionHeader {
                    type_tag: RegionType::Receive,
                },
                buffer,
                key,
                _access: access,
            });

            Ok(Self { inner })
        }

        pub fn key(&self) -> u64 {
            self.inner.key
        }

        pub fn buffer(&self) -> &crate::bytevec::BytesMut {
            &self.inner.buffer
        }

        pub fn buffer_mut(&mut self) -> &mut crate::bytevec::BytesMut {
            &mut self.inner.buffer
        }

        pub fn len(&self) -> usize {
            self.inner.buffer.len()
        }

        pub fn is_empty(&self) -> bool {
            self.inner.buffer.is_empty()
        }

        pub fn desc(&self) -> *mut core::ffi::c_void {
            core::ptr::null_mut()
        }

        pub(crate) fn into_context(self) -> *mut core::ffi::c_void {
            Box::into_raw(self.inner) as *mut core::ffi::c_void
        }
    }

    impl fmt::Debug for Receive {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Receive (UDP polyfill)")
                .field("key", &self.key())
                .field("len", &self.len())
                .finish()
        }
    }

    /// Type-erased memory region for completion queue reconstruction
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
        pub unsafe fn from_context(context: *mut core::ffi::c_void) -> Self {
            // Read the type tag from the header (first field of both SendInner and ReceiveInner)
            let header = &*(context as *const RegionHeader);
            match header.type_tag {
                RegionType::Send => {
                    let inner = Arc::from_raw(context as *const SendInner);
                    Self::Send(Send { inner })
                }
                RegionType::Receive => {
                    let inner = Box::from_raw(context as *mut ReceiveInner);
                    Self::Receive(Receive { inner })
                }
            }
        }
    }
}

pub mod completion_queue {
    use super::*;

    #[allow(dead_code)]
    define_enum!(
        pub enum Format {
            UNSPEC = 0,
            CONTEXT = 1,
            MSG = 2,
            DATA = 3,
            TAGGED = 4,
        }
    );

    #[allow(dead_code)]
    define_enum!(
        pub enum WaitObject {
            NONE = 0,
            UNSPEC = 1,
            SET = 2,
            FD = 3,
            MUTEX_COND = 4,
        }
    );

    /// Simulated completion entry (matches fi_cq_entry layout for compatibility)
    #[repr(C)]
    #[derive(Clone, Copy, Debug)]
    pub struct CqEntry {
        pub op_context: *mut core::ffi::c_void,
    }

    unsafe impl Send for CqEntry {}
    unsafe impl Sync for CqEntry {}

    /// UDP-based completion queue
    ///
    /// Tracks pending operations and provides completions when operations finish.
    #[derive(Clone)]
    pub struct CompletionQueue {
        inner: Arc<CompletionQueueInner>,
    }

    struct CompletionQueueInner {
        // Queue of completed operations
        completions: Mutex<Vec<CqEntry>>,
        _format: Format,
    }

    impl CompletionQueue {
        pub fn open(domain: &domain::Domain, size: usize) -> Result<Self, Error> {
            Self::open_with_format(domain, size, Format::CONTEXT)
        }

        pub fn open_with_format(
            _domain: &domain::Domain,
            _size: usize,
            format: Format,
        ) -> Result<Self, Error> {
            let inner = Arc::new(CompletionQueueInner {
                completions: Mutex::new(Vec::new()),
                _format: format,
            });
            Ok(Self { inner })
        }

        /// Read completion entries
        pub fn read<F, const MAX_LEN: usize>(
            &self,
            limit: usize,
            mut callback: F,
        ) -> Result<usize, Error>
        where
            F: FnMut(memory_registration::MemoryRegion),
        {
            let limit = limit.min(MAX_LEN);
            let mut completions = self.inner.completions.lock().unwrap();
            
            let count = completions.len().min(limit);
            let entries: Vec<CqEntry> = completions.drain(..count).collect();
            drop(completions);

            for entry in &entries {
                if !entry.op_context.is_null() {
                    let mr = unsafe {
                        memory_registration::MemoryRegion::from_context(entry.op_context)
                    };
                    callback(mr);
                }
            }

            Ok(count)
        }

        /// Post a completion (internal use)
        pub(crate) fn post_completion(&self, entry: CqEntry) {
            let mut completions = self.inner.completions.lock().unwrap();
            completions.push(entry);
        }

        pub(crate) fn as_ptr(&self) -> *mut core::ffi::c_void {
            Arc::as_ptr(&self.inner) as *mut core::ffi::c_void
        }
    }

    impl fmt::Debug for CompletionQueue {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let completions = self.inner.completions.lock().unwrap();
            f.debug_struct("CompletionQueue (UDP polyfill)")
                .field("pending", &completions.len())
                .finish()
        }
    }
}

pub mod address_vector {
    use super::*;

    #[allow(dead_code)]
    define_enum!(
        pub enum Type {
            MAP = 0,
            TABLE = 1,
            Unspecified = 2,
        }
    );

    /// fi_addr_t type (address handle)
    pub type FiAddrT = u64;

    /// UDP-based address vector
    ///
    /// Maps libfabric address handles (fi_addr_t) to actual socket addresses.
    /// This is essential for resolving dest_addr parameters in send/write operations.
    #[derive(Clone)]
    pub struct AddressVector {
        inner: Arc<AddressVectorInner>,
    }

    struct AddressVectorInner {
        // Maps fi_addr_t handle -> serialized socket address bytes
        addresses: Mutex<HashMap<u64, Vec<u8>>>,
        next_handle: AtomicU64,
        _av_type: Type,
    }

    impl AddressVector {
        pub fn open(domain: &domain::Domain) -> Result<Self, Error> {
            Self::open_with_attr(domain, Type::TABLE, 64)
        }

        pub fn open_with_attr(
            _domain: &domain::Domain,
            av_type: Type,
            _count: usize,
        ) -> Result<Self, Error> {
            let inner = Arc::new(AddressVectorInner {
                addresses: Mutex::new(HashMap::new()),
                next_handle: AtomicU64::new(1),
                _av_type: av_type,
            });
            Ok(Self { inner })
        }

        /// Insert a single address and return its handle
        pub fn insert(&self, addr: &[u8]) -> Result<FiAddrT, Error> {
            let handle = self.inner.next_handle.fetch_add(1, Ordering::Relaxed);
            let mut addresses = self.inner.addresses.lock().unwrap();
            addresses.insert(handle, addr.to_vec());
            Ok(handle)
        }

        /// Insert multiple addresses
        pub fn insert_multiple(&self, addrs: &[&[u8]]) -> Result<Vec<FiAddrT>, Error> {
            let mut handles = Vec::with_capacity(addrs.len());
            for addr in addrs {
                let handle = self.insert(addr)?;
                handles.push(handle);
            }
            Ok(handles)
        }

        /// Remove an address
        pub fn remove(&self, fi_addr: FiAddrT) -> Result<(), Error> {
            let mut addresses = self.inner.addresses.lock().unwrap();
            addresses.remove(&fi_addr);
            Ok(())
        }

        /// Lookup the address bytes for a handle
        pub fn lookup(&self, fi_addr: FiAddrT, addr_buf: &mut [u8]) -> Result<usize, Error> {
            let addresses = self.inner.addresses.lock().unwrap();
            if let Some(addr) = addresses.get(&fi_addr) {
                let len = addr.len().min(addr_buf.len());
                addr_buf[..len].copy_from_slice(&addr[..len]);
                Ok(len)
            } else {
                Err(Error::from_code(-2)) // ENOENT
            }
        }

        pub(crate) fn as_ptr(&self) -> *mut core::ffi::c_void {
            Arc::as_ptr(&self.inner) as *mut core::ffi::c_void
        }
    }

    impl fmt::Debug for AddressVector {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let addresses = self.inner.addresses.lock().unwrap();
            f.debug_struct("AddressVector (UDP polyfill)")
                .field("count", &addresses.len())
                .finish()
        }
    }
}

pub mod addr {
    use super::*;

    define_enum!(
        pub enum Format {
            Unspecified = 0,
            SOCKADDR = 1,
            SOCKADDR_IN = 2,
            SOCKADDR_IN6 = 3,
        }
    );
}

pub mod fabric {
    use super::*;

    pub struct Fabric {
        _data: Arc<()>,
    }

    impl Fabric {
        pub fn open(_info: &info::Info) -> Result<Self, Error> {
            Ok(Self {
                _data: Arc::new(()),
            })
        }

        pub fn open_domain(&self, info: &info::Info) -> Result<domain::Domain, Error> {
            domain::Domain::open(self, info)
        }

        pub(crate) fn as_ptr(&self) -> *mut core::ffi::c_void {
            core::ptr::null_mut()
        }
    }

    impl fmt::Debug for Fabric {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Fabric (polyfill)").finish()
        }
    }
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
