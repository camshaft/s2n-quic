//! Buffer management for transfer payloads.
//!
//! Buffers are reference-counted memory regions that can be shared across multiple
//! transfers (e.g., for broadcast scenarios). The buffer lifecycle progresses through
//! states: allocated → filled → in-use → freed.

use crate::ByteVec;
use std::{ops::Range, pin::Pin, sync::Arc};

// Only include libfabric-specific message handling when the feature is enabled
#[cfg(feature = "libfabric")]
mod libfabric;
mod udp;

/// Unique identifier for a message.
///
/// Message IDs are used to reference buffers in protocol messages and enable
/// efficient lookup by the transport.
///
/// For UDP, the ID is a slot index.
/// For libfabric, the ID is the raw pointer to the iov struct for this message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Id(u64);

impl Id {
    /// Creates a new Id
    #[inline]
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw value of the ID
    #[inline]
    pub(crate) fn as_u64(self) -> u64 {
        self.0
    }
}

/// Key used to authenticate the message
///
/// For UDP, this is the generation ID for the slot index.
/// For libfabric, this is the `fi_mr_key`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Key(u64);

impl Key {
    /// Creates a new Key
    #[inline]
    pub(crate) fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw value
    #[inline]
    pub(crate) fn as_u64(self) -> u64 {
        self.0
    }
}

/// Trait for transport-specific buffer allocation implementations
///
/// Different transports (UDP, libfabric) have different requirements for
/// buffer management. This trait abstracts those differences.
trait Backend: 'static + Send + Sync {
    /// Allocates a new message with the given data
    fn register(&self, data: Regions) -> Pin<Arc<dyn Handle>>;
}

trait Handle: 'static + Send + Sync {
    fn id(&self) -> Id;
    fn key(&self) -> Key;
    fn len(&self) -> usize;
}

/// Handle for allocating buffers.
///
/// The allocator manages a pool of memory regions and supports NUMA-aware
/// allocation when workers are assigned to specific NUMA nodes.
#[derive(Clone)]
pub struct Allocator {
    backend: Arc<dyn Backend>,
}

impl Allocator {
    /// Creates a new UDP-based allocator
    ///
    /// Uses slot-based allocation for message identification
    pub fn new_udp() -> Self {
        todo!()
    }

    // TODO: Add when libfabric module is ready
    // /// Creates a new libfabric-based allocator
    // ///
    // /// Uses fi_mr_regv for memory registration and pointer-based identification
    // pub fn new_libfabric(/* libfabric domain */) -> Self {
    //     Self {
    //         backend: Arc::new(LibfabricAllocator::new(/* ... */)),
    //     }
    // }
}

impl Allocator {
    /// Allocates a message from the given data.
    ///
    /// This is a convenience method for the common pattern of allocating a buffer
    /// and immediately filling it with data.
    ///
    /// # Arguments
    ///
    /// * `data` - The data to store in the message
    ///
    /// # Returns
    ///
    /// A message ready to be used in transfers.
    pub async fn allocate(&self, data: ByteVec) -> Message {
        let mut builder = self.builder().await;
        builder.push_back(data);
        builder.build()
    }

    /// Returns a builder for constructing a message with more control.
    ///
    /// Use this when you need to:
    /// - Add multiple chunks
    /// - Mix plaintext and encrypted data
    /// - Insert data at specific positions
    ///
    /// For simple cases, prefer `allocate()` which takes data directly.
    ///
    /// # Returns
    ///
    /// A builder that can be configured and filled.
    pub async fn builder(&self) -> Builder {
        todo!()
    }
}

/// A buffer allocation that can be configured before filling.
///
/// This type allows applications to specify preferences for worker/NUMA locality
/// before the buffer is actually filled with data.
pub struct Builder {
    _todo: (),
}

impl Builder {
    pub fn push_front(&mut self, data: ByteVec) {
        let _ = data;
    }

    pub fn push_back(&mut self, data: ByteVec) {
        let _ = data;
    }

    /// Pushes a pre-encrypted chunk of data into the front of the message.
    ///
    /// The provided tag is used to authenticate the payload and bind it to
    /// the transmission.
    pub fn push_front_encrypted(&mut self, data: ByteVec, tag: Range<usize>) {
        let _ = data;
        let _ = tag;
    }

    /// Pushes a pre-encrypted chunk of data onto the back of the message.
    ///
    /// The provided tag is used to authenticate the payload and bind it to
    /// the transmission.
    pub fn push_back_encrypted(&mut self, data: ByteVec, tag: Range<usize>) {
        let _ = data;
        let _ = tag;
    }

    pub fn len(&self) -> usize {
        todo!()
    }

    pub fn is_empty(&self) -> bool {
        todo!()
    }

    pub fn build(self) -> Message {
        todo!()
    }
}

struct Regions {
    data: ByteVec,
    tags: Vec<Range<usize>>,
}

/// A filled buffer ready for use in transfers.
///
/// This type is cloneable and can be used in multiple transfers. The underlying
/// buffer is reference-counted and will be freed when all transfers complete.
#[derive(Clone)]
pub struct Message {
    _todo: (),
}

impl Message {
    /// Returns the ID for this message.
    pub fn id(&self) -> Id {
        todo!()
    }

    /// Returns the size of the message in bytes.
    pub fn len(&self) -> usize {
        todo!()
    }

    /// Returns whether the message is empty.
    pub fn is_empty(&self) -> bool {
        todo!()
    }
}
