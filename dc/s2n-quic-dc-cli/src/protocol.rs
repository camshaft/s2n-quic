// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use bytes::{Buf, BufMut, Bytes, BytesMut};
use s2n_codec::DecoderError;

/// Request sent from client to server
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    /// How long the server should wait before sending response (in milliseconds)
    pub response_delay_ms: u64,
    /// Size of the response the server should send (in bytes)
    pub response_size: u64,
}

impl Request {
    pub fn new(response_delay_ms: u64, response_size: u64) -> Self {
        Self {
            response_delay_ms,
            response_size,
        }
    }

    pub fn encode(&self) -> Bytes {
        let mut buffer = BytesMut::with_capacity(16);
        buffer.put_u64(self.response_delay_ms);
        buffer.put_u64(self.response_size);
        buffer.freeze()
    }

    pub fn decode(mut data: &[u8]) -> Result<Self, DecoderError> {
        if data.len() < 16 {
            return Err(DecoderError::UnexpectedEof(16 - data.len()));
        }

        let response_delay_ms = data.get_u64();
        let response_size = data.get_u64();

        Ok(Self {
            response_delay_ms,
            response_size,
        })
    }
}

/// Simple request storage that implements the required traits for RPC
pub struct RequestStorage {
    data: Bytes,
    offset: usize,
}

impl RequestStorage {
    pub fn new(request: Request) -> Self {
        Self {
            data: request.encode(),
            offset: 0,
        }
    }
}

impl s2n_quic_core::buffer::reader::Storage for RequestStorage {
    type Error = core::convert::Infallible;

    fn buffered_len(&self) -> usize {
        self.data.len().saturating_sub(self.offset)
    }

    fn read_chunk(
        &mut self,
        _watermark: usize,
    ) -> Result<s2n_quic_core::buffer::reader::storage::Chunk, Self::Error> {
        let remaining = &self.data[self.offset..];
        if remaining.is_empty() {
            Ok(s2n_quic_core::buffer::reader::storage::Chunk::Slice(&[]))
        } else {
            Ok(s2n_quic_core::buffer::reader::storage::Chunk::Bytes(
                self.data.slice(self.offset..),
            ))
        }
    }

    fn partial_copy_into<Dest>(
        &mut self,
        dest: &mut Dest,
    ) -> Result<s2n_quic_core::buffer::reader::storage::Chunk, Self::Error>
    where
        Dest: s2n_quic_core::buffer::writer::Storage + ?Sized,
    {
        let remaining = &self.data[self.offset..];
        let len = remaining.len().min(dest.remaining_capacity());
        if len > 0 {
            let _ = dest.put_slice(&remaining[..len]);
            self.offset += len;
        }
        self.read_chunk(0)
    }

    fn buffer_is_empty(&self) -> bool {
        self.offset >= self.data.len()
    }
}
