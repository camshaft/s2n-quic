//! Buffer management for transfer payloads.
//!
//! Buffers are reference-counted memory regions that can be shared across multiple
//! transfers (e.g., for broadcast scenarios). The buffer lifecycle progresses through
//! states: allocated → filled → in-use → freed.

use crate::worker;
use s2n_quic_core::buffer::reader;
use std::sync::Arc;

/// Unique identifier for a buffer.
///
/// Buffer IDs are used to reference buffers in protocol messages and enable
/// efficient lookup in the buffer registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufferId {
    _todo: (),
}

/// Handle for allocating buffers.
///
/// The allocator manages a pool of memory regions and supports NUMA-aware
/// allocation when workers are assigned to specific NUMA nodes.
#[derive(Clone)]
pub struct Allocator {
    #[expect(dead_code)]
    inner: Arc<AllocatorInner>,
}

struct AllocatorInner {
    _todo: (),
}

impl Allocator {
    /// Allocates a buffer with the specified size.
    ///
    /// Returns a handle that can be used to fill the buffer with data and then
    /// use it for one or more transfers.
    ///
    /// # Arguments
    ///
    /// * `size` - The size of the buffer to allocate in bytes
    ///
    /// # Returns
    ///
    /// An allocation handle that can be configured and filled.
    pub async fn allocate(&self, size: usize) -> Allocation {
        let _ = size;
        todo!()
    }

    /// Sets a preferred worker for NUMA locality.
    ///
    /// When specified, the allocator will try to allocate memory on the NUMA
    /// node associated with this worker for better performance.
    pub fn prefer_worker(self, worker: worker::Id) -> Self {
        let _ = worker;
        todo!()
    }
}

/// A buffer allocation that can be configured before filling.
///
/// This type allows applications to specify preferences for worker/NUMA locality
/// before the buffer is actually filled with data.
pub struct Allocation {
    _todo: (),
}

impl Allocation {
    /// Fills the buffer by copying pre-encrypted data.
    ///
    /// Use this when the application has already encrypted the data and wants
    /// to copy it into the allocated buffer.
    ///
    /// # Arguments
    ///
    /// * `data` - The pre-encrypted data to copy
    /// * `auth_tag_range` - The byte range within the data that should be used
    ///   for authenticating control messages (typically the GHASH tag)
    pub fn fill_encrypted(
        self,
        data: &mut impl reader::storage::Infallible,
        auth_tag_range: core::ops::Range<usize>,
    ) -> Message {
        let _ = (data, auth_tag_range);
        todo!()
    }

    /// Fills the buffer by encrypting plaintext data.
    ///
    /// The transport will encrypt the data using an ephemeral key and return
    /// a handle that includes both the encrypted buffer and the key.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The data to encrypt
    pub fn fill_plaintext(self, plaintext: &mut impl reader::storage::Infallible) -> Message {
        let _ = plaintext;
        todo!()
    }
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
    /// Returns the buffer ID for this message.
    pub fn id(&self) -> BufferId {
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
