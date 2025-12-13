//! Polyfill implementation for libfabric functionality.
//!
//! This module provides a userspace implementation of the libfabric API
//! that can be used when:
//! - libfabric is not installed on the system
//! - The platform doesn't support libfabric (e.g., macOS, Windows)
//! - RDMA hardware is not available
//!
//! The polyfill implements the same API surface as the real libfabric module
//! but uses UDP sockets and userspace memory management instead of hardware RDMA.

use bitflags::bitflags;
use core::fmt;
use std::sync::Arc;

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

    /// Polyfill endpoint (not fully implemented, stub for API compatibility)
    pub struct Endpoint {
        _data: (),
    }

    impl Endpoint {
        pub fn open(_domain: &domain::Domain, _info: &info::Info) -> Result<Self, Error> {
            // Return unimplemented for now - this would need full UDP socket implementation
            Err(Error::from_code(-38)) // ENOSYS
        }
    }

    impl fmt::Debug for Endpoint {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Endpoint (polyfill)").finish()
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

    pub struct Domain {
        _data: (),
    }

    impl Domain {
        pub(crate) fn open(_fabric: &fabric::Fabric, _info: &info::Info) -> Result<Self, Error> {
            Ok(Self { _data: () })
        }

        pub(crate) fn as_ptr(&self) -> *mut core::ffi::c_void {
            core::ptr::null_mut()
        }
    }

    impl fmt::Debug for Domain {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Domain (polyfill)").finish()
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

    /// Polyfill memory region (stubs for API compatibility)
    #[derive(Clone)]
    pub struct Send {
        _data: Arc<()>,
    }

    impl Send {
        pub fn register(
            _domain: &domain::Domain,
            _buffer: crate::ByteVec,
            _access: Access,
        ) -> Result<Self, Error> {
            Err(Error::from_code(-38)) // ENOSYS
        }

        pub fn key(&self) -> u64 {
            0
        }

        pub fn len(&self) -> usize {
            0
        }

        pub fn is_empty(&self) -> bool {
            true
        }
    }

    impl fmt::Debug for Send {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Send (polyfill)").finish()
        }
    }

    pub struct Receive {
        _data: (),
    }

    impl Receive {
        pub fn register(
            _domain: &domain::Domain,
            _buffer: crate::bytevec::BytesMut,
            _access: Access,
        ) -> Result<Self, Error> {
            Err(Error::from_code(-38)) // ENOSYS
        }

        pub fn key(&self) -> u64 {
            0
        }

        pub fn len(&self) -> usize {
            0
        }

        pub fn is_empty(&self) -> bool {
            true
        }
    }

    impl fmt::Debug for Receive {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Receive (polyfill)").finish()
        }
    }

    #[derive(Debug)]
    pub enum MemoryRegion {
        Send(Send),
        Receive(Receive),
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

    #[derive(Clone)]
    pub struct CompletionQueue {
        _data: Arc<()>,
    }

    impl CompletionQueue {
        pub fn open(_domain: &domain::Domain, _size: usize) -> Result<Self, Error> {
            Err(Error::from_code(-38)) // ENOSYS
        }

        pub fn open_with_format(
            _domain: &domain::Domain,
            _size: usize,
            _format: Format,
        ) -> Result<Self, Error> {
            Err(Error::from_code(-38)) // ENOSYS
        }
    }

    impl fmt::Debug for CompletionQueue {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("CompletionQueue (polyfill)").finish()
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

    #[derive(Clone)]
    pub struct AddressVector {
        _data: Arc<()>,
    }

    impl AddressVector {
        pub fn open(_domain: &domain::Domain) -> Result<Self, Error> {
            Err(Error::from_code(-38)) // ENOSYS
        }

        pub fn open_with_attr(
            _domain: &domain::Domain,
            _av_type: Type,
            _count: usize,
        ) -> Result<Self, Error> {
            Err(Error::from_code(-38)) // ENOSYS
        }
    }

    impl fmt::Debug for AddressVector {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("AddressVector (polyfill)").finish()
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
