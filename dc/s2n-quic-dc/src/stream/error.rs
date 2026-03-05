// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Unified error type for stream errors.
//!
//! Fatal errors are stored in the shared error location and visible to both halves.
//! Non-fatal errors are returned directly to the caller.

use crate::{
    credentials,
    crypto::open,
    event::IntoEvent,
    packet::{self, stream},
    stream::{packet_number, TransportFeatures},
};
use core::{fmt, panic::Location};
use s2n_quic_core::{buffer, endpoint::Location as EndpointLocation, frame, varint::VarInt};

/// A stream error with its source location in the code.
#[derive(Clone, Copy)]
pub struct Error {
    pub(crate) kind: Kind,
    pub(crate) location: &'static Location<'static>,
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("kind", &self.kind)
            .field("crate", &"s2n-quic-dc")
            .field("file", &self.file())
            .field("line", &self.location.line())
            .finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let Self { kind, location } = self;
        let file = self.file();
        let line = location.line();
        write!(f, "[s2n-quic-dc::{file}:{line}]: {kind}")
    }
}

impl core::error::Error for Error {}

impl IntoEvent<Error> for Error {
    fn into_event(self) -> Error {
        self
    }
}

impl Error {
    #[track_caller]
    #[inline]
    pub fn new(kind: Kind) -> Self {
        Self {
            kind,
            location: Location::caller(),
        }
    }

    #[inline]
    pub fn kind(&self) -> &Kind {
        &self.kind
    }

    #[inline]
    fn file(&self) -> &'static str {
        self.location
            .file()
            .trim_start_matches(concat!(env!("CARGO_MANIFEST_DIR"), "/src/"))
    }

    /// Returns true if this error is fatal for the given transport features.
    ///
    /// Fatal errors terminate the entire stream. Non-fatal errors are local
    /// to the caller (e.g., dropped packets on UDP).
    #[inline]
    pub fn is_fatal(&self, features: &TransportFeatures) -> bool {
        // if the transport is a stream then any error we encounter is fatal
        if features.is_stream() {
            return true;
        }

        !matches!(
            self.kind(),
            Kind::Decode
                | Kind::Crypto(_)
                | Kind::Duplicate
                | Kind::CredentialMismatch { .. }
                | Kind::StreamMismatch { .. }
        )
    }

    /// Returns a `ConnectionClose` frame for this error, if one should be sent.
    #[inline]
    pub fn connection_close(&self) -> Option<frame::ConnectionClose<'static>> {
        use s2n_quic_core::transport;

        fn from_code(code: u8) -> Option<frame::ConnectionClose<'static>> {
            Some(frame::ConnectionClose {
                error_code: VarInt::from_u8(code),
                frame_type: None,
                reason: None,
            })
        }

        match self.kind {
            // Don't send a close for idle timeout
            Kind::IdleTimeout => None,
            // We don't have working crypto keys so we can't respond
            Kind::KeyReplayPrevented
            | Kind::KeyReplayMaybePrevented { .. }
            | Kind::UnknownPathSecret => None,

            // Non-fatal recv errors that may be fatal for reliable transports
            Kind::Decode
            | Kind::Crypto(_)
            | Kind::Duplicate
            | Kind::CredentialMismatch { .. }
            | Kind::StreamMismatch { .. }
            | Kind::UnexpectedPacket { .. }
            | Kind::UnexpectedRetransmission => Some(transport::Error::PROTOCOL_VIOLATION.into()),

            Kind::NormalClose => from_code(Kind::NORMAL_CODE),
            Kind::ApplicationPanic => from_code(Kind::PANIC_CODE),
            Kind::AcceptQueueFull => from_code(Kind::ACCEPT_FULL_CODE),
            Kind::MaxSojournTime => from_code(Kind::MAX_SOJOURN_TIME_CODE),
            Kind::FlowReset => from_code(Kind::FLOW_RESET_CODE),

            Kind::ApplicationError { error } => Some(frame::ConnectionClose {
                error_code: VarInt::new(*error).unwrap(),
                frame_type: None,
                reason: None,
            }),
            Kind::TransportError { code } => Some(frame::ConnectionClose {
                error_code: code,
                frame_type: Some(VarInt::from_u16(0)),
                reason: None,
            }),

            // Receiver-originated fatal errors
            Kind::MaxDataExceeded => Some(transport::Error::FLOW_CONTROL_ERROR.into()),
            Kind::InvalidFin | Kind::TruncatedTransport => {
                Some(transport::Error::FINAL_SIZE_ERROR.into())
            }
            Kind::OutOfOrder { .. } => Some(transport::Error::STREAM_STATE_ERROR.into()),
            Kind::OutOfRange => Some(transport::Error::STREAM_LIMIT_ERROR.into()),

            // Sender API errors - these don't get connection close frames
            Kind::PayloadTooLarge
            | Kind::PacketBufferTooSmall
            | Kind::StreamFinished
            | Kind::FinalSizeChanged => None,

            // All other fatal errors get a generic error code
            Kind::PacketNumberExhaustion
            | Kind::RetransmissionFailure
            | Kind::FrameError { .. }
            | Kind::FatalError => Some(frame::ConnectionClose {
                error_code: VarInt::from_u16(1),
                frame_type: None,
                reason: None,
            }),
        }
    }
}

impl From<Kind> for Error {
    #[track_caller]
    #[inline]
    fn from(kind: Kind) -> Self {
        Self::new(kind)
    }
}

/// All error variants for stream operations.
///
/// Fatal variants are stored in the shared error location and terminate the entire stream.
/// Non-fatal variants are returned directly to the caller.
#[derive(Clone, Copy, Debug, thiserror::Error)]
pub enum Kind {
    // === Transport-level errors (from either half) ===
    #[error("the idle timer expired")]
    IdleTimeout,
    #[error("the crypto key has been replayed and is invalid")]
    KeyReplayPrevented,
    #[error("the crypto key has been potentially replayed (gap: {gap:?}) and is invalid")]
    KeyReplayMaybePrevented { gap: Option<u64> },
    #[error("the stream is using an unknown path secret")]
    UnknownPathSecret,
    #[error("the stream was reset by the peer with code {code}")]
    TransportError { code: VarInt },
    #[error("the application panicked and dropped the stream")]
    ApplicationPanic,
    #[error("the accept queue was full and the stream was dropped")]
    AcceptQueueFull,
    #[error("the stream was dropped due to high sojourn time")]
    MaxSojournTime,
    #[error("the flow state was reset and the stream is unrecoverable")]
    FlowReset,
    #[error("the stream was closed without an error")]
    NormalClose,
    #[error("the stream was closed with application code {error}")]
    ApplicationError {
        error: s2n_quic_core::application::Error,
    },

    // === Sender-originated fatal errors ===
    #[error("the number of packets able to be sent on the sender has been exceeded")]
    PacketNumberExhaustion,
    #[error("retransmission not possible")]
    RetransmissionFailure,
    #[error("an invalid frame was received: {decoder}")]
    FrameError { decoder: s2n_codec::DecoderError },
    #[error("the stream experienced an unrecoverable error")]
    FatalError,

    // === Receiver-originated fatal errors ===
    #[error("the peer exceeded the max data window")]
    MaxDataExceeded,
    #[error("invalid fin")]
    InvalidFin,
    #[error("out of range")]
    OutOfRange,
    #[error("the stream expected in-order delivery of {expected} but got {actual}")]
    OutOfOrder { expected: u64, actual: u64 },
    #[error("the transport has been truncated without authentication")]
    TruncatedTransport,
    #[error("unexpected retransmission packet")]
    UnexpectedRetransmission,

    // === Sender API errors (non-fatal, returned to caller) ===
    #[error("payload provided is too large and exceeded the maximum offset")]
    PayloadTooLarge,
    #[error("the provided packet buffer is too small for the minimum packet size")]
    PacketBufferTooSmall,
    #[error("stream has been finished")]
    StreamFinished,
    #[error("the final size of the stream has changed")]
    FinalSizeChanged,

    // === Receiver non-fatal errors (local, not shared) ===
    #[error("could not decode packet")]
    Decode,
    #[error("could not decrypt packet: {0}")]
    Crypto(open::Error),
    #[error("packet has already been processed")]
    Duplicate,
    #[error("the packet was for another credential ({actual:?}) but was handled by {expected:?}")]
    CredentialMismatch {
        expected: credentials::Credentials,
        actual: credentials::Credentials,
    },
    #[error("the packet was for another stream ({actual}) but was handled by {expected}")]
    StreamMismatch {
        expected: stream::Id,
        actual: stream::Id,
    },
    #[error("unexpected packet: {packet:?}")]
    UnexpectedPacket { packet: packet::Kind },
}

impl Kind {
    /// The application closed normally
    pub const NORMAL_CODE: u8 = 0;
    /// The application panicked
    pub const PANIC_CODE: u8 = 1;
    /// The accept queue has overflowed and this stream has been dropped
    pub const ACCEPT_FULL_CODE: u8 = 2;
    /// The stream was dropped due to high sojourn time
    pub const MAX_SOJOURN_TIME_CODE: u8 = 3;
    /// The flow state is gone and the stream is unrecoverable
    pub const FLOW_RESET_CODE: u8 = 4;

    #[inline]
    #[track_caller]
    pub(crate) fn err(self) -> Error {
        Error::new(self)
    }
}

/// A stored error along with its origin (local or remote).
#[derive(Clone, Copy, Debug)]
pub struct StoredError {
    pub error: Error,
    pub source: EndpointLocation,
}

impl From<Error> for std::io::Error {
    #[inline]
    #[track_caller]
    fn from(error: Error) -> Self {
        Self::new(error.kind.into(), error)
    }
}

impl From<Kind> for std::io::ErrorKind {
    #[inline]
    fn from(kind: Kind) -> Self {
        use std::io::ErrorKind;
        match kind {
            Kind::PayloadTooLarge => ErrorKind::BrokenPipe,
            Kind::PacketBufferTooSmall => ErrorKind::InvalidInput,
            Kind::PacketNumberExhaustion => ErrorKind::BrokenPipe,
            Kind::RetransmissionFailure => ErrorKind::BrokenPipe,
            Kind::StreamFinished => ErrorKind::UnexpectedEof,
            Kind::FinalSizeChanged => ErrorKind::InvalidInput,
            Kind::IdleTimeout => ErrorKind::TimedOut,
            Kind::KeyReplayPrevented => ErrorKind::PermissionDenied,
            Kind::KeyReplayMaybePrevented { .. } => ErrorKind::PermissionDenied,
            Kind::UnknownPathSecret => ErrorKind::PermissionDenied,
            Kind::NormalClose => ErrorKind::Other,
            Kind::ApplicationPanic => ErrorKind::ConnectionAborted,
            Kind::AcceptQueueFull => ErrorKind::ConnectionRefused,
            Kind::MaxSojournTime => ErrorKind::ConnectionRefused,
            Kind::FlowReset => ErrorKind::ConnectionReset,
            Kind::ApplicationError { .. } => ErrorKind::ConnectionReset,
            Kind::TransportError { .. } => ErrorKind::ConnectionAborted,
            Kind::FrameError { .. } => ErrorKind::InvalidData,
            Kind::FatalError => ErrorKind::BrokenPipe,
            Kind::MaxDataExceeded => ErrorKind::ConnectionAborted,
            Kind::InvalidFin => ErrorKind::InvalidData,
            Kind::OutOfRange => ErrorKind::ConnectionAborted,
            Kind::OutOfOrder { .. } => ErrorKind::InvalidData,
            Kind::TruncatedTransport => ErrorKind::UnexpectedEof,
            Kind::UnexpectedRetransmission => ErrorKind::InvalidData,
            Kind::Decode => ErrorKind::InvalidData,
            Kind::Crypto(_) => ErrorKind::InvalidData,
            Kind::Duplicate => ErrorKind::InvalidData,
            Kind::CredentialMismatch { .. } | Kind::StreamMismatch { .. } => ErrorKind::InvalidData,
            Kind::UnexpectedPacket {
                packet: packet::Kind::FlowReset,
            } => ErrorKind::ConnectionReset,
            Kind::UnexpectedPacket {
                packet:
                    packet::Kind::UnknownPathSecret
                    | packet::Kind::StaleKey
                    | packet::Kind::ReplayDetected,
            } => ErrorKind::ConnectionRefused,
            Kind::UnexpectedPacket {
                packet: packet::Kind::Stream | packet::Kind::Control | packet::Kind::Datagram,
            } => ErrorKind::InvalidData,
        }
    }
}

impl From<packet_number::ExhaustionError> for Error {
    #[inline]
    #[track_caller]
    fn from(_error: packet_number::ExhaustionError) -> Self {
        Kind::PacketNumberExhaustion.err()
    }
}

impl From<buffer::Error<core::convert::Infallible>> for Error {
    #[inline]
    #[track_caller]
    fn from(error: buffer::Error<core::convert::Infallible>) -> Self {
        match error {
            buffer::Error::OutOfRange => Kind::PayloadTooLarge.err(),
            buffer::Error::InvalidFin => Kind::FinalSizeChanged.err(),
            buffer::Error::ReaderError(_) => unreachable!(),
        }
    }
}

impl From<buffer::Error<Error>> for Error {
    #[inline]
    #[track_caller]
    fn from(value: buffer::Error<Error>) -> Self {
        match value {
            buffer::Error::OutOfRange => Kind::OutOfRange.err(),
            buffer::Error::InvalidFin => Kind::InvalidFin.err(),
            buffer::Error::ReaderError(error) => error,
        }
    }
}

impl From<open::Error> for Error {
    #[track_caller]
    fn from(value: open::Error) -> Self {
        match value {
            open::Error::ReplayDefinitelyDetected => Kind::KeyReplayPrevented,
            open::Error::ReplayPotentiallyDetected { gap } => Kind::KeyReplayMaybePrevented { gap },
            error => Kind::Crypto(error),
        }
        .err()
    }
}
