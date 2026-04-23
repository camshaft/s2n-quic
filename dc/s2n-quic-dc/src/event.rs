// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(any(test, feature = "testing"))]
use s2n_quic_core::event::snapshot;

pub use s2n_quic_core::event::{Event, IntoEvent, Timestamp};

/// Global atomic counter for assigning unique stream IDs within a process.
static NEXT_CONNECTION_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// Returns a unique connection ID for a new stream.
#[inline]
pub fn next_connection_id() -> u64 {
    NEXT_CONNECTION_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

/// Provides metadata related to an event
pub trait Meta: core::fmt::Debug {
    /// A context from which the event is being emitted
    ///
    /// An event can occur in the context of an Endpoint or Connection
    fn subject(&self) -> api::Subject;
}

impl Meta for api::ConnectionMeta {
    fn subject(&self) -> api::Subject {
        builder::Subject::Connection { id: self.id }.into_event()
    }
}

impl Meta for api::EndpointMeta {
    fn subject(&self) -> api::Subject {
        builder::Subject::Endpoint {}.into_event()
    }
}

pub mod diagnostic;

mod generated;
pub use generated::*;

pub mod metrics {
    pub use crate::event::generated::metrics::*;
    pub use s2n_quic_core::event::metrics::Recorder;

    pub mod aggregate {
        pub use crate::event::generated::metrics::aggregate::*;
        pub use s2n_quic_core::event::metrics::aggregate::{
            info, AsVariant, BoolRecorder, Info, Metric, NominalRecorder, Recorder, Registry, Units,
        };

        pub mod probe {
            pub use crate::event::generated::metrics::probe::*;
            pub use s2n_quic_core::event::metrics::aggregate::probe::dynamic;
        }
    }
}

pub mod disabled {
    #[derive(Clone, Debug, Default)]
    pub struct Subscriber(());

    impl super::Subscriber for Subscriber {
        type ConnectionContext = ();

        #[inline]
        fn create_connection_context(
            &self,
            _meta: &super::api::ConnectionMeta,
            _info: &super::api::ConnectionInfo,
        ) -> Self::ConnectionContext {
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    struct AnomalyCounter {
        count: Arc<AtomicU64>,
    }

    impl Subscriber for AnomalyCounter {
        type ConnectionContext = ();

        fn create_connection_context(
            &self,
            _meta: &api::ConnectionMeta,
            _info: &api::ConnectionInfo,
        ) -> Self::ConnectionContext {
        }

        fn on_anomalous_event<M: Meta, E: Event>(&self, _meta: &M, _event: &E) {
            self.count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn test_timestamp() -> Timestamp {
        unsafe {
            s2n_quic_core::time::Timestamp::from_duration(core::time::Duration::from_secs(1))
        }
        .into_event()
    }

    #[test]
    fn anomalous_event_fires_for_acceptor_udp_io_error() {
        let count = Arc::new(AtomicU64::new(0));
        let subscriber = AnomalyCounter {
            count: count.clone(),
        };

        let meta = builder::EndpointMeta {
            timestamp: test_timestamp(),
        };
        let publisher = EndpointPublisherSubscriber::new(meta, None, &subscriber);

        // Publish an anomalous event (AcceptorUdpIoError is tagged #[anomalous])
        let error = std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "udp error");
        publisher.on_acceptor_udp_io_error(builder::AcceptorUdpIoError { error: &error });

        assert_eq!(count.load(Ordering::Relaxed), 1, "on_anomalous_event should fire for AcceptorUdpIoError");
    }

    #[test]
    fn anomalous_event_does_not_fire_for_normal_events() {
        let count = Arc::new(AtomicU64::new(0));
        let subscriber = AnomalyCounter {
            count: count.clone(),
        };

        let meta = builder::EndpointMeta {
            timestamp: test_timestamp(),
        };
        let publisher = EndpointPublisherSubscriber::new(meta, None, &subscriber);

        // Publish a non-anomalous event (AcceptorTcpStarted is NOT tagged #[anomalous])
        let addr = s2n_quic_core::inet::SocketAddress::default();
        publisher.on_acceptor_tcp_started(builder::AcceptorTcpStarted {
            id: 0,
            local_address: &addr,
            backlog: 128,
        });

        assert_eq!(count.load(Ordering::Relaxed), 0, "on_anomalous_event should NOT fire for non-anomalous events");
    }
}
