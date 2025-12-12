// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Buffer management for transfer payloads
//!
//! The buffer system provides reference-counted memory regions for
//! efficient transmission to multiple peers, supporting both single-peer
//! transfers and broadcast scenarios.

use bytes::Bytes;
use core::fmt;
use std::sync::Arc;

/// Unique identifier for a buffer
///
/// Buffer IDs consist of a sequence number and generation counter
/// to prevent ambiguity when buffers are recycled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferId {
    sequence: u32,
    generation: u32,
}

impl BufferId {
    /// Create a new buffer ID (internal use only)
    #[doc(hidden)]
    pub fn new(sequence: u32, generation: u32) -> Self {
        Self {
            sequence,
            generation,
        }
    }

    /// Get the sequence number
    pub(crate) fn sequence(&self) -> u32 {
        self.sequence
    }

    /// Get the generation number
    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }
}

impl fmt::Display for BufferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BufferId({}/{})", self.sequence, self.generation)
    }
}

/// Lifecycle state of a buffer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferState {
    /// Buffer is allocated but not yet written
    Allocated,
    /// Application is currently writing data to the buffer
    Writing,
    /// Buffer is filled and ready for transmission
    Filled,
}

/// A memory region for transfer data
///
/// Buffers can consist of multiple contiguous memory regions to support
/// messages that span multiple blocks. Each buffer has a unique ID and
/// tracks its lifecycle state.
#[derive(Debug, Clone)]
pub struct BufferDescriptor {
    /// Unique identifier for this buffer
    pub id: BufferId,
    /// Current lifecycle state
    pub state: BufferState,
    /// Memory regions comprising this buffer
    pub regions: Vec<BufferRegion>,
    /// Total size across all regions
    pub total_size: usize,
}

/// A contiguous memory region within a buffer
#[derive(Debug, Clone)]
pub struct BufferRegion {
    /// The actual data
    pub data: Bytes,
    /// Offset of this region within the overall buffer
    pub offset: usize,
}

impl BufferDescriptor {
    /// Create a new buffer descriptor (internal use only)
    #[doc(hidden)]
    pub fn new(id: BufferId, total_size: usize) -> Self {
        Self {
            id,
            state: BufferState::Allocated,
            regions: Vec::new(),
            total_size,
        }
    }

    /// Mark the buffer as being written to
    pub fn start_writing(&mut self) {
        assert_eq!(self.state, BufferState::Allocated);
        self.state = BufferState::Writing;
    }

    /// Mark the buffer as filled and ready for transmission
    pub fn finish_writing(&mut self) {
        assert_eq!(self.state, BufferState::Writing);
        self.state = BufferState::Filled;
    }

    /// Add a memory region to this buffer
    pub fn add_region(&mut self, data: Bytes, offset: usize) {
        assert_eq!(self.state, BufferState::Writing);
        self.regions.push(BufferRegion { data, offset });
    }
}

/// A reference-counted buffer handle
///
/// Multiple transfers can hold references to the same buffer.
/// The buffer is automatically freed when all references are dropped.
pub struct Buffer {
    inner: Arc<BufferDescriptor>,
}

impl Buffer {
    /// Create a new buffer handle (internal use only)
    #[doc(hidden)]
    pub fn new(descriptor: BufferDescriptor) -> Self {
        Self {
            inner: Arc::new(descriptor),
        }
    }

    /// Get the buffer ID
    pub fn id(&self) -> BufferId {
        self.inner.id
    }

    /// Get the buffer state
    pub fn state(&self) -> BufferState {
        self.inner.state
    }

    /// Get the total size of the buffer
    pub fn total_size(&self) -> usize {
        self.inner.total_size
    }

    /// Get the memory regions
    pub fn regions(&self) -> &[BufferRegion] {
        &self.inner.regions
    }

    /// Get the underlying descriptor
    pub(crate) fn descriptor(&self) -> &BufferDescriptor {
        &self.inner
    }
}

impl Clone for Buffer {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Buffer")
            .field("id", &self.inner.id)
            .field("state", &self.inner.state)
            .field("total_size", &self.inner.total_size)
            .field("regions", &self.inner.regions.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_id() {
        let id = BufferId::new(100, 5);
        assert_eq!(id.sequence(), 100);
        assert_eq!(id.generation(), 5);
    }

    #[test]
    fn test_buffer_lifecycle() {
        let id = BufferId::new(1, 1);
        let mut desc = BufferDescriptor::new(id, 1024);
        
        assert_eq!(desc.state, BufferState::Allocated);
        
        desc.start_writing();
        assert_eq!(desc.state, BufferState::Writing);
        
        desc.add_region(Bytes::from(vec![1, 2, 3]), 0);
        assert_eq!(desc.regions.len(), 1);
        
        desc.finish_writing();
        assert_eq!(desc.state, BufferState::Filled);
    }

    #[test]
    fn test_buffer_clone() {
        let id = BufferId::new(1, 1);
        let desc = BufferDescriptor::new(id, 1024);
        let buffer1 = Buffer::new(desc);
        let buffer2 = buffer1.clone();
        
        assert_eq!(buffer1.id(), buffer2.id());
        assert_eq!(Arc::strong_count(&buffer1.inner), 2);
    }

    #[test]
    #[should_panic]
    fn test_buffer_invalid_state_transition() {
        let id = BufferId::new(1, 1);
        let mut desc = BufferDescriptor::new(id, 1024);
        desc.finish_writing(); // Should panic - not in Writing state
    }
}
