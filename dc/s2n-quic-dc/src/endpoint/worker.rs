// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Worker infrastructure for distributing packets across send/recv sockets.

use crate::{
    counter::{Counter, Registry},
    credentials,
    endpoint::id::{Id, IdMap, RecvDispatchWorkerId, RemoteSenderId},
    intrusive::{Entry, Queue},
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
    /// Per-worker staging queues. A decoded datagram is appended here during the completion-batch
    /// drain instead of being sent immediately; [`on_batch_complete`](Router::on_batch_complete)
    /// splices each non-empty staging queue into its worker channel with a single locked append +
    /// single wake, collapsing the per-packet lock/wake of a large GRO/multishot batch into one per
    /// destination. Reused across batches (drained by `mem::take`, never reallocated).
    staging:
        IdMap<RecvDispatchWorkerId, Queue<packet::datagram::decoder::Packet<descriptor::Filled>>>,
    route: Route,
    invalidation_tx: Inv,
    decode_error_counter: Counter,
    routed_counter: Counter,
    route_send_err_counter: Counter,
    per_worker_routed: IdMap<RecvDispatchWorkerId, Counter>,
}

impl<D, Route: routing::SenderRoute, Inv> FanOutRouter<D, Route, Inv> {
    pub fn new(
        txs: IdMap<RecvDispatchWorkerId, D>,
        invalidation_tx: Inv,
        counters: &Registry,
    ) -> Self {
        let route = Route::new(txs.len());
        let staging = RecvDispatchWorkerId::range(txs.len())
            .map(|id| (id, Queue::new()))
            .collect();
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
            staging,
            route,
            invalidation_tx,
            decode_error_counter: counters.register("!router.decode_err"),
            routed_counter: counters.register("router.routed"),
            route_send_err_counter: counters.register("!router.send_err"),
            per_worker_routed,
        }
    }
}

impl<D, Route, Inv> Router for FanOutRouter<D, Route, Inv>
where
    D: channel::UnboundedSender<Queue<packet::datagram::decoder::Packet<descriptor::Filled>>>,
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
        self.routed_counter.add(1);
        self.per_worker_routed[idx].add(1);
        // Stage the packet for its destination worker. The channel send (lock + wake) is deferred to
        // `on_batch_complete`, so a whole completion batch costs one lock + one wake per worker rather
        // than one per packet. Intrusive `push_back` preserves arrival order within the batch.
        self.staging[idx].push_back(packet.into());
    }

    #[inline]
    fn on_batch_complete(&mut self) {
        // Flush each worker's staged packets in one locked splice + one wake. Empty staging queues are
        // skipped, so at low concurrency (each batch touches ~one worker with ~one packet) this is the
        // same single lock + wake as the per-packet path — the coalescing win only materializes as the
        // per-batch fan-out grows.
        for ((_, tx), (_, staging)) in self.txs.iter_mut().zip(self.staging.iter_mut()) {
            if staging.is_empty() {
                continue;
            }
            let batch = core::mem::take(staging);
            if tx.send(batch).is_err() {
                self.route_send_err_counter.add(1);
            }
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
        credentials::Credentials,
        packet::datagram,
        path::secret::map::Entry as PathSecretEntry,
        socket::pool::{self, descriptor::SyncRecycler},
    };
    use s2n_codec::DecoderBufferMut;
    use s2n_quic_core::{endpoint, varint::VarInt};
    use std::{cell::Cell, net::SocketAddr, rc::Rc};

    const TAG_LEN: usize = 16;

    /// A destination sender that only *counts* — how many times [`send`](channel::UnboundedSender::send)
    /// was called (== the number of channel-lock acquisitions + receiver wakes) and how many entries
    /// crossed in total. Cloned into every worker slot so the counters aggregate the whole batch. This
    /// is the direct, rig-free measurement of the idea's metric #1 (wakes/lock-acquisitions per packet).
    #[derive(Clone)]
    struct CountingSender {
        sends: Rc<Cell<usize>>,
        entries: Rc<Cell<usize>>,
    }

    impl channel::UnboundedSender<Queue<datagram::decoder::Packet<descriptor::Filled>>>
        for CountingSender
    {
        fn send(
            &mut self,
            batch: Queue<datagram::decoder::Packet<descriptor::Filled>>,
        ) -> Result<(), Queue<datagram::decoder::Packet<descriptor::Filled>>> {
            self.sends.set(self.sends.get() + 1);
            self.entries.set(self.entries.get() + batch.len());
            // `batch` (and its `Filled` descriptors) drops here → recycles, as on the real path.
            Ok(())
        }
    }

    /// No-op invalidation sink — the datagram-dispatch path under test never touches it.
    #[derive(Clone)]
    struct NoopInv;

    impl channel::UnboundedSender<Entry<descriptor::Filled>> for NoopInv {
        fn send(&mut self, _v: Entry<descriptor::Filled>) -> Result<(), Entry<descriptor::Filled>> {
            Ok(())
        }
    }

    /// Build a real, on-the-wire datagram packet whose `Meta` decodes with the given `source_sender_id`
    /// (the routing field the fan-out hash reads). The application header/payload are arbitrary — the
    /// dispatch path only reads `routing_info()` + `credentials()`, never the sealed body.
    fn build_packet(source_sender_id: u8) -> datagram::decoder::Packet<descriptor::Filled> {
        let peer: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let sealer_entry = PathSecretEntry::builder(peer)
            .endpoint_type(endpoint::Type::Client)
            .build(None);
        let key_id = VarInt::ZERO;
        let sealer = sealer_entry.secret().application_sealer(key_id);
        let credentials = Credentials {
            id: *sealer_entry.secret().id(),
            key_id,
        };

        let app_header = [0u8; 8];
        let payload = [0u8; 16];

        let mut buf = vec![0u8; 65536];
        let routing_info = datagram::RoutingInfo::SenderId {
            source_sender_id: VarInt::from_u8(source_sender_id),
        };
        let mut header_reader = &app_header[..];
        let mut payload_reader = &payload[..];
        let encoded_len = datagram::encoder::encode(
            s2n_codec::EncoderBuffer::new(&mut buf),
            443,
            routing_info,
            Some(VarInt::ZERO),
            VarInt::try_from(app_header.len() as u64).unwrap(),
            &mut header_reader,
            VarInt::try_from(payload.len() as u64).unwrap(),
            &mut payload_reader,
            &sealer,
            &credentials,
        );
        assert!(encoded_len > 0);

        let pool = pool::Pool::new(u16::MAX);
        let unfilled = pool.alloc::<SyncRecycler>().expect("pool alloc");
        let segments = unfilled
            .fill_with(|addr, _cmsg, mut iov| {
                iov[..encoded_len].copy_from_slice(&buf[..encoded_len]);
                addr.set(peer.into());
                Ok::<_, core::convert::Infallible>(encoded_len)
            })
            .expect("fill_with");
        let mut filled = segments.take_filled();
        let meta = {
            let decode_buf = DecoderBufferMut::new(&mut filled[..]);
            datagram::decoder::Meta::decode(&decode_buf, (), TAG_LEN).expect("meta decode")
        };
        meta.with_storage(filled).expect("with_storage")
    }

    /// Build a `FanOutRouter` over `n_workers` counting senders sharing `sends`/`entries` counters.
    fn router_over(
        n_workers: usize,
        sends: &Rc<Cell<usize>>,
        entries: &Rc<Cell<usize>>,
    ) -> FanOutRouter<CountingSender, routing::PowerOfTwoRoute, NoopInv> {
        let registry = Registry::default();
        let txs: IdMap<RecvDispatchWorkerId, CountingSender> =
            RecvDispatchWorkerId::range(n_workers)
                .map(|id| {
                    (
                        id,
                        CountingSender {
                            sends: sends.clone(),
                            entries: entries.clone(),
                        },
                    )
                })
                .collect();
        FanOutRouter::new(txs, NoopInv, &registry)
    }

    /// Metric #1, high fan-in: many packets in ONE completion batch cost at most one send (== one
    /// channel-lock + one wake) per destination worker, NOT one per packet — while still delivering
    /// every packet. This is exactly the coalescing the idea predicts.
    #[test]
    fn batch_coalesces_wakes_high_fanin() {
        let n_workers = 4;
        let sends = Rc::new(Cell::new(0));
        let entries = Rc::new(Cell::new(0));
        let mut router = router_over(n_workers, &sends, &entries);

        let m = 64usize;
        for i in 0..m {
            // Vary the routing field so packets spread across workers (the exact spread is irrelevant
            // to the assertions below — they hold for any distribution).
            router.dispatch_datagram_packet(build_packet((i % 251) as u8 + 1));
        }

        // Dispatch stages only — no lock/wake happens per packet.
        assert_eq!(sends.get(), 0, "dispatch must not send or wake per packet");
        assert_eq!(entries.get(), 0);

        router.on_batch_complete();

        assert!(sends.get() >= 1, "a non-empty batch must deliver");
        assert!(
            sends.get() <= n_workers,
            "at most one send (one lock + one wake) per worker per batch; got {}",
            sends.get()
        );
        assert!(
            sends.get() < m,
            "coalescing: far fewer sends ({}) than packets ({m})",
            sends.get()
        );
        assert_eq!(entries.get(), m, "every packet must still be delivered");
    }

    /// Negative control: at low concurrency (a batch of one packet), the change is a no-op — one
    /// dispatch → exactly one send, identical to the per-packet path. If this regressed to >1 the
    /// coalescing would be adding, not removing, wakes.
    #[test]
    fn single_packet_batch_is_one_send() {
        let sends = Rc::new(Cell::new(0));
        let entries = Rc::new(Cell::new(0));
        let mut router = router_over(4, &sends, &entries);

        router.dispatch_datagram_packet(build_packet(1));
        assert_eq!(sends.get(), 0, "still deferred until batch completes");

        router.on_batch_complete();
        assert_eq!(
            sends.get(),
            1,
            "one packet ⇒ one send (no coalescing overhead)"
        );
        assert_eq!(entries.get(), 1);

        // An empty batch flushes nothing.
        sends.set(0);
        router.on_batch_complete();
        assert_eq!(sends.get(), 0, "empty batch must not send or wake");
    }
}
