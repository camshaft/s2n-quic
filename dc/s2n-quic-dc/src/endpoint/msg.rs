// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    endpoint::id::LocalSenderId, path::secret::map::Entry as PathSecretEntry,
    stream::endpoint::ack::state,
};
use bytes::BytesMut;
use core::time::Duration;
use s2n_quic_core::varint::VarInt;
use std::sync::Arc;

/// Fragment metadata carried in a stream data message.
///
/// This mirrors [`crate::endpoint::frame::Fragment`] but lives in the
/// application-level message layer so the reader can track pending fragments
/// without referencing the frame header types directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fragment {
    /// Fragment identifier — monotonically assigned by the writer per message.
    pub id: VarInt,
    /// Total byte count for this message. `Some` only in the first message for
    /// this `id`; `None` in all continuation messages.
    pub total_size: Option<VarInt>,
}

pub enum Stream {
    FlowValidated,
    Data {
        offset: VarInt,
        fin: bool,
        payload: BytesMut,
        /// Optional message-fragment metadata. `None` for plain stream data.
        fragment: Option<Fragment>,
    },
    Reset {
        error_code: VarInt,
    },
}

pub enum Control {
    Frames {
        payload: BytesMut,
    },
    /// Inline window update: the new `maximum_data` value advertised by the reader.
    ///
    /// This is the fast path dispatched from a [`Header::QueueMaxData`] frame,
    /// avoiding payload allocation and QUIC frame decoding for the common
    /// flow-control case.
    MaxData {
        maximum_data: VarInt,
    },
    Reset {
        error_code: VarInt,
    },
}

pub enum Sender {
    /// Inbound ACK from the peer — decoded payload drives loss detection and CCA updates.
    ReceivedAck {
        local_sender_id: LocalSenderId,
        path_secret_entry: Arc<PathSecretEntry>,
        payload: BytesMut,
        /// Wire-time ACK delay: time from when the largest acknowledged packet was received
        /// by the peer to when the ACK was sent.  Extracted from `Header::Ack.ack_delay` by
        /// the dispatch layer and subtracted from the RTT sample in `process_ack`.
        ack_delay: Duration,
    },
    /// Notification carrying a freshly encoded outbound ACK body from recv worker.
    /// The send worker stamps wire-time ack_delay during assembly.
    PendingAck(state::Submission),
}

impl Sender {
    /// Returns the send socket index this message should route to.
    #[inline]
    pub fn sender_idx(&self) -> LocalSenderId {
        match self {
            Self::ReceivedAck {
                local_sender_id, ..
            } => *local_sender_id,
            Self::PendingAck(submission) => submission.local_sender_id,
        }
    }

    /// Returns a reference to the path secret entry for context lookup.
    #[inline]
    pub fn path_secret_entry(&self) -> &Arc<PathSecretEntry> {
        match self {
            Self::ReceivedAck {
                path_secret_entry, ..
            } => path_secret_entry,
            Self::PendingAck(submission) => &submission.path_secret_entry,
        }
    }

    /// Returns the recv dispatch worker ID for completion routing, if applicable.
    #[inline]
    pub fn recv_worker_id(&self) -> Option<super::id::RecvDispatchWorkerId> {
        match self {
            Self::ReceivedAck { .. } => None,
            Self::PendingAck(submission) => Some(submission.recv_worker_id),
        }
    }
}

pub mod queue {
    use crate::flow;

    pub type Allocator = flow::queue::Allocator<super::Stream, super::Control, flow::Handle>;
    pub type Dispatcher = flow::queue::Dispatch<super::Stream, super::Control, flow::Handle>;
    pub type Control = flow::queue::Control<super::Stream, super::Control, flow::Handle>;
    pub type Stream = flow::queue::Stream<super::Stream, super::Control, flow::Handle>;
}
