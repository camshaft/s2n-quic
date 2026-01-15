//! Buffer management for transfer payloads.
//!
//! Buffers are reference-counted memory regions that can be shared across multiple
//! transfers (e.g., for broadcast scenarios). The buffer lifecycle progresses through
//! states: allocated → filled → in-use → freed.

use crate::{priority::Priority, ByteVec};
use smallvec::SmallVec;
use std::ops::Range;

/// Handle for allocating buffers.
#[derive(Clone)]
pub struct Allocator {
    // backend: Arc<dyn Backend>,
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
    pub async fn allocate(&self, priority: Priority, data: ByteVec) -> Message {
        let mut builder = self.builder(priority).await;
        builder.push_back(data);
        builder.build().await
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
    pub async fn builder(&self, priority: Priority) -> Builder {
        let _ = priority;
        todo!()
    }
}

/// A buffer allocation that can be configured before filling.
pub struct Builder {
    regions: Regions,
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

    pub async fn build(self) -> Message {
        todo!()
    }
}

struct Regions {
    data: ByteVec,
    tags: SmallVec<[Range<usize>; 1]>,
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
    /// Returns the size of the message in bytes.
    pub fn len(&self) -> usize {
        todo!()
    }

    /// Returns whether the message is empty.
    pub fn is_empty(&self) -> bool {
        todo!()
    }
}
