// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Worker infrastructure for distributing packets across send/recv sockets.

use crate::{
    counter::{Counter, Registry},
    credentials,
    endpoint::id::{Id, IdMap, RecvDispatchWorkerId, RemoteSenderId},
    intrusive::Entry,
    packet::{self, datagram::RoutingInfo},
    socket::{channel, pool::descriptor, recv::router::Router},
    stream::endpoint::routing,
    tracing::*,
};
use s2n_quic_core::varint::VarInt;

// ── Packet Router ──────────────────────────────────────────────────────────

/// Routes decoded datagram packets to one of N dispatch queues based on a hash
/// of (credentials.id, source_sender_id).
///
/// This ensures that all packets from the same peer always land in the same
/// dispatch task, maintaining coherent ACK space and packet-number deduplication.
pub(crate) struct FanOutRouter<D, Route, Inv> {
    txs: IdMap<RecvDispatchWorkerId, D>,
    route: Route,
    invalidation_tx: Inv,
    decode_error_counter: Counter,
    routed_counter: Counter,
    route_send_err_counter: Counter,
    per_worker_routed: IdMap<RecvDispatchWorkerId, Counter>,
    /// The local interface of the socket this router receives from, which it records on each
    /// packet it dispatches
    local_interface: Option<crate::socket::interface::Id>,
}

impl<D, Route: routing::SenderRoute, Inv> FanOutRouter<D, Route, Inv> {
    pub fn new(
        txs: IdMap<RecvDispatchWorkerId, D>,
        invalidation_tx: Inv,
        counters: &Registry,
    ) -> Self {
        let route = Route::new(txs.len());
        let per_worker_routed = RecvDispatchWorkerId::range(txs.len())
            .map(|id| {
                (
                    id,
                    counters.register_nominal("router.routed", format_args!("recv.{id}")),
                )
            })
            .collect();
        Self {
            txs,
            route,
            invalidation_tx,
            decode_error_counter: counters.register("!router.decode_err"),
            routed_counter: counters.register("router.routed"),
            route_send_err_counter: counters.register("!router.send_err"),
            per_worker_routed,
            local_interface: None,
        }
    }

    /// Records the local interface of the socket this router receives from on each packet it
    /// dispatches, so that the packet's acknowledgements leave on that interface
    pub fn with_local_interface(
        mut self,
        local_interface: Option<crate::socket::interface::Id>,
    ) -> Self {
        self.local_interface = local_interface;
        self
    }
}

impl<D, Route, Inv> Router for FanOutRouter<D, Route, Inv>
where
    D: channel::UnboundedSender<Entry<packet::datagram::decoder::Packet<descriptor::Filled>>>,
    Route: routing::SenderRoute,
    Inv: channel::UnboundedSender<Entry<descriptor::Filled>>,
{
    fn is_open(&self) -> bool {
        true
    }

    #[inline]
    fn dispatch_datagram_packet(
        &mut self,
        packet: packet::datagram::decoder::Packet<descriptor::Filled>,
    ) {
        let RoutingInfo::SenderId { source_sender_id } = packet.routing_info() else {
            info!(?packet, "invalid packet routing info");
            return;
        };
        let source_sender_id = RemoteSenderId::new(source_sender_id);
        let idx = self
            .route
            .worker_id_for_recv(packet.credentials(), source_sender_id);
        let packet = match self.local_interface {
            Some(local_interface) => {
                let (meta, mut segment) = packet.into_parts();
                segment.set_local_interface(Some(local_interface));
                packet::datagram::decoder::Packet::from_parts(meta, segment)
                    .expect("the segment still holds the packet")
            }
            None => packet,
        };
        self.routed_counter.add(1);
        self.per_worker_routed[idx].add(1);
        if self.txs[idx].send(packet.into()).is_err() {
            self.route_send_err_counter.add(1);
        }
    }

    #[inline]
    fn handle_datagram_packet(
        &mut self,
        _remote_address: s2n_quic_core::inet::SocketAddress,
        _ecn: s2n_quic_core::inet::ExplicitCongestionNotification,
        _packet: packet::datagram::decoder::Packet<&mut [u8]>,
    ) {
    }

    fn dispatch_unknown_path_secret_packet(
        &mut self,
        _queue_id: Option<VarInt>,
        _credentials: credentials::Id,
        segment: descriptor::Filled,
    ) {
        let _ = self.invalidation_tx.send(segment.into());
    }

    fn dispatch_stale_key_packet(
        &mut self,
        _sender_id: Option<VarInt>,
        _credentials: credentials::Id,
        segment: descriptor::Filled,
    ) {
        let _ = self.invalidation_tx.send(segment.into());
    }

    fn dispatch_replay_detected_packet(
        &mut self,
        _queue_id: Option<VarInt>,
        _credentials: credentials::Id,
        segment: descriptor::Filled,
    ) {
        let _ = self.invalidation_tx.send(segment.into());
    }

    fn on_decode_error(
        &mut self,
        error: s2n_codec::DecoderError,
        remote_address: s2n_quic_core::inet::SocketAddress,
        segment: descriptor::Filled,
    ) {
        self.decode_error_counter.add(1);
        debug!(
            ?error,
            %remote_address,
            packet_len = segment.len(),
            "failed to decode packet"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::{seal, KeyPhase},
        endpoint::routing::ModuloRoute,
        socket::{interface, rate::Rate},
    };
    use core::cell::RefCell;
    use s2n_codec::EncoderBuffer;
    use std::rc::Rc;

    type Dispatched = Entry<packet::datagram::decoder::Packet<descriptor::Filled>>;

    const PAYLOAD: &[u8] = b"datagram payload";
    const TAG_LEN: usize = 16;

    /// A channel that keeps each value sent on it
    struct Sent<T>(Rc<RefCell<Vec<T>>>);

    impl<T> channel::UnboundedSender<T> for Sent<T> {
        fn send(&mut self, value: T) -> Result<(), T> {
            self.0.borrow_mut().push(value);
            Ok(())
        }
    }

    /// Writes the payload unencrypted, with a zero tag after it
    struct NoSeal;

    impl seal::Application for NoSeal {
        fn key_phase(&self) -> KeyPhase {
            KeyPhase::Zero
        }

        fn tag_len(&self) -> usize {
            TAG_LEN
        }

        fn encrypt(
            &self,
            _packet_number: u64,
            _header: &[u8],
            extra_payload: Option<&[u8]>,
            payload_and_tag: &mut [u8],
        ) {
            let extra_payload = extra_payload.unwrap_or_default();
            let tag_offset = payload_and_tag.len() - TAG_LEN;
            let extra_offset = tag_offset - extra_payload.len();
            payload_and_tag[extra_offset..tag_offset].copy_from_slice(extra_payload);
            payload_and_tag[tag_offset..].fill(0);
        }
    }

    /// Returns the identity of the first interface a configuration lists, which isn't the
    /// default interface
    fn listed_interface() -> interface::Id {
        let table = interface::Table::resolve(
            &[interface::Config::default()],
            Rate::new(1.0),
            &[],
            &[],
            &interface::System,
        );
        let id = table.interfaces()[1].id;
        assert_ne!(id, interface::Id::DEFAULT);
        id
    }

    /// Returns a segment that holds one datagram packet from sender 3, as a recv socket fills it
    fn datagram_segment() -> descriptor::Filled {
        let credentials = credentials::Credentials {
            id: credentials::Id::default(),
            key_id: VarInt::from_u8(1),
        };
        let unfilled = descriptor::Unfilled::<descriptor::SyncRecycler>::new(256)
            .expect("the allocator has memory");
        let segments = unfilled
            .fill_with(|_addr, _cmsg, mut iov| {
                let mut header: &[u8] = &[];
                let mut payload = PAYLOAD;
                let len = packet::datagram::encoder::encode(
                    EncoderBuffer::new(&mut iov[..]),
                    4433,
                    RoutingInfo::SenderId {
                        source_sender_id: VarInt::from_u8(3),
                    },
                    Some(VarInt::from_u8(1)),
                    VarInt::ZERO,
                    &mut header,
                    VarInt::from_u8(PAYLOAD.len() as u8),
                    &mut payload,
                    &NoSeal,
                    &credentials,
                );
                Ok::<_, core::convert::Infallible>(len)
            })
            .expect("the fill doesn't fail");
        segments.take_filled()
    }

    /// Dispatches `segment` through a router of one worker that records `local_interface`, and
    /// returns the packet the worker receives
    fn dispatch(
        local_interface: Option<interface::Id>,
        segment: descriptor::Filled,
    ) -> packet::datagram::decoder::Packet<descriptor::Filled> {
        let dispatched = Rc::new(RefCell::new(Vec::<Dispatched>::new()));
        let invalidated = Rc::new(RefCell::new(Vec::<Entry<descriptor::Filled>>::new()));
        let txs = RecvDispatchWorkerId::range(1)
            .map(|id| (id, Sent(dispatched.clone())))
            .collect::<IdMap<_, _>>();
        let counters = Registry::new();
        let invalidation_tx = Sent(invalidated.clone());
        let mut router = FanOutRouter::<_, ModuloRoute, _>::new(txs, invalidation_tx, &counters)
            .with_local_interface(local_interface);
        router.on_segment(segment);
        assert!(invalidated.borrow().is_empty());
        let mut dispatched = dispatched.borrow_mut();
        assert_eq!(dispatched.len(), 1);
        dispatched.pop().unwrap().into_inner()
    }

    #[test]
    fn the_router_records_its_interface_on_each_packet_it_dispatches() {
        let local_interface = listed_interface();
        let packet = dispatch(Some(local_interface), datagram_segment());
        assert_eq!(packet.storage().local_interface(), Some(local_interface));
        // The router takes the packet apart to record the interface: the packet it sends keeps
        // its routing information and its payload
        assert_eq!(
            packet.routing_info(),
            RoutingInfo::SenderId {
                source_sender_id: VarInt::from_u8(3),
            }
        );
        assert_eq!(packet.payload(), PAYLOAD);
    }

    #[test]
    fn a_router_without_an_interface_leaves_the_packet_without_one() {
        let packet = dispatch(None, datagram_segment());
        assert_eq!(packet.storage().local_interface(), None);
        assert_eq!(packet.payload(), PAYLOAD);
    }
}
