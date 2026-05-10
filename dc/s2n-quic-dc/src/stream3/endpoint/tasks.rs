// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    intrusive_queue::{Entry, Queue},
    path::secret::map::Entry as PathSecretEntry,
    socket::channel::{ByteCost, Receiver, Sender},
    stream3::frame::Frame,
};
use core::{
    future::poll_fn,
    task::{self, Poll},
};
use std::sync::Arc;

/// Routing key accessor for stream3 send-side load-balancing tasks.
pub trait PathSecretMapEntry {
    fn path_secret_entry(&self) -> &Arc<PathSecretEntry>;
}

impl<T> PathSecretMapEntry for crate::intrusive_queue::Entry<T>
where
    T: PathSecretMapEntry,
{
    #[inline]
    fn path_secret_entry(&self) -> &Arc<PathSecretEntry> {
        (**self).path_secret_entry()
    }
}

impl PathSecretMapEntry for crate::stream3::frame::Frame {
    #[inline]
    fn path_secret_entry(&self) -> &Arc<PathSecretEntry> {
        &self.path_secret_entry
    }
}

/// Conservative packet-level overhead estimate for stream3 frame batches.
///
/// Uses the same upper-bound constant as datagram partials so batching leaves room for packet
/// fields that are added later by workers (credentials, packet number, routing, tag, etc).
const MAX_FRAME_BATCH_PACKET_OVERHEAD: u64 =
    crate::packet::datagram::partial::MAX_FLOW_DATA_HEADER_OVERHEAD as u64;
const BATCH_FRAMES_POLL_BUDGET: usize = 10;

/// A queue of frames grouped for a single path-secret entry.
///
/// This wrapper keeps the queue byte-cost estimate and path-secret entry so it can be routed
/// through `pick_two`.
pub struct FrameBatch {
    queue: Queue<Frame>,
    path_secret_entry: Arc<PathSecretEntry>,
    byte_cost: u64,
}

impl FrameBatch {
    #[inline]
    fn new(first: Entry<Frame>) -> Self {
        let path_secret_entry = first.path_secret_entry.clone();
        let byte_cost = MAX_FRAME_BATCH_PACKET_OVERHEAD.saturating_add(first.byte_cost());
        let mut queue = Queue::new();
        queue.push_back(first);

        Self {
            queue,
            path_secret_entry,
            byte_cost,
        }
    }

    #[inline]
    fn push_with_cost(&mut self, frame: Entry<Frame>, frame_cost: u64) {
        self.byte_cost = self.byte_cost.saturating_add(frame_cost);
        self.queue.push_back(frame);
    }

    /// Returns the number of frames currently buffered in this batch.
    #[inline]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Returns true when this batch contains no frames.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Borrows the underlying intrusive queue of frames.
    #[inline]
    pub fn queue(&self) -> &Queue<Frame> {
        &self.queue
    }

    /// Consumes the batch and returns the underlying frame queue.
    #[inline]
    pub fn into_queue(self) -> Queue<Frame> {
        self.queue
    }
}

impl From<FrameBatch> for Queue<Frame> {
    #[inline]
    fn from(value: FrameBatch) -> Self {
        value.into_queue()
    }
}

impl ByteCost for FrameBatch {
    #[inline]
    fn byte_cost(&self) -> u64 {
        self.byte_cost
    }
}

impl PathSecretMapEntry for FrameBatch {
    #[inline]
    fn path_secret_entry(&self) -> &Arc<PathSecretEntry> {
        &self.path_secret_entry
    }
}

/// Receiver combinator that batches consecutive frame entries by path-secret entry and byte budget.
///
/// Batches target roughly one datagram (`path_secret_entry.max_datagram_size()`) while accounting
/// for frame metadata and conservative packet overhead. A batch always contains at least one frame.
pub struct BatchFramesByPathSecret<R> {
    inner: R,
    buffered: Option<Entry<Frame>>,
}

impl<R> BatchFramesByPathSecret<R>
where
    R: Receiver<Entry<Frame>>,
{
    #[inline]
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buffered: None,
        }
    }

    #[inline]
    fn take_first(&mut self, cx: &mut task::Context<'_>) -> Poll<Option<Entry<Frame>>> {
        if let Some(frame) = self.buffered.take() {
            return Poll::Ready(Some(frame));
        }

        self.inner.poll_recv(cx)
    }
}

impl<R> Receiver<FrameBatch> for BatchFramesByPathSecret<R>
where
    R: Receiver<Entry<Frame>>,
{
    fn poll_recv(&mut self, cx: &mut task::Context<'_>) -> Poll<Option<FrameBatch>> {
        let Some(first) = (match self.take_first(cx) {
            Poll::Ready(frame) => frame,
            Poll::Pending => return Poll::Pending,
        }) else {
            return Poll::Ready(None);
        };

        let target_bytes = first.path_secret_entry.max_datagram_size() as u64;
        let mut batch = FrameBatch::new(first);

        // Keep poll work bounded and return the current batch so the executor can make progress.
        for _ in 0..BATCH_FRAMES_POLL_BUDGET {
            if batch.byte_cost() >= target_bytes {
                break;
            }

            match self.inner.poll_recv(cx) {
                Poll::Ready(Some(frame_entry)) => {
                    if !Arc::ptr_eq(batch.path_secret_entry(), frame_entry.path_secret_entry()) {
                        self.buffered = Some(frame_entry);
                        break;
                    }

                    let frame_cost = frame_entry.byte_cost();
                    let next_cost = batch.byte_cost().saturating_add(frame_cost);
                    if next_cost > target_bytes {
                        self.buffered = Some(frame_entry);
                        break;
                    }

                    batch.push_with_cost(frame_entry, frame_cost);
                }
                Poll::Ready(None) | Poll::Pending => break,
            }
        }

        Poll::Ready(Some(batch))
    }

    #[inline]
    fn on_consumed(&mut self, bytes: u64) {
        self.inner.on_consumed(bytes);
    }
}

/// Routes items to socket senders by using pick-two path scheduling from the path secret map
/// entry associated with each item.
pub async fn pick_two<T, R, S, Rand>(mut rx: R, mut senders: Vec<S>, random: Rand)
where
    T: ByteCost + PathSecretMapEntry,
    R: Receiver<T>,
    S: Sender<T>,
    Rand: Fn(usize) -> usize,
{
    loop {
        let Some(entry) = rx.recv().await else {
            break;
        };

        let bytes = entry.byte_cost();
        let mut slot = core::mem::MaybeUninit::new(entry);

        let sent = poll_fn(|cx| try_send_pick_two(cx, &mut slot, &mut senders, &random)).await;

        if !sent {
            // SAFETY: `slot` is initialized above with `MaybeUninit::new(entry)` and only
            // consumed by successful send.
            unsafe { slot.assume_init_drop() };
            break;
        }

        rx.on_consumed(bytes);
    }
}

fn try_send_pick_two<T, S, Rand>(
    cx: &mut task::Context<'_>,
    slot: &mut core::mem::MaybeUninit<T>,
    senders: &mut Vec<S>,
    random: &Rand,
) -> Poll<bool>
where
    T: PathSecretMapEntry,
    S: Sender<T>,
    Rand: Fn(usize) -> usize,
{
    if senders.is_empty() {
        return Poll::Ready(false);
    }

    let chosen_idx = {
        // SAFETY: `slot` is initialized with `MaybeUninit::new(entry)` and remains
        // initialized until it is consumed by a successful `poll_send`.
        let value = unsafe { &*slot.as_ptr() };
        let picked = value
            .path_secret_entry()
            .pick_sender_by_next_transmission(random);
        debug_assert!(
            picked < senders.len(),
            "picked sender index out of bounds: picked={} senders={}",
            picked,
            senders.len()
        );
        if picked >= senders.len() {
            return Poll::Ready(false);
        }
        picked
    };

    match senders[chosen_idx].poll_send(cx, slot) {
        Poll::Ready(Ok(())) => Poll::Ready(true),
        Poll::Ready(Err(())) => Poll::Ready(false),
        Poll::Pending => {
            let len = senders.len();
            for offset in 1..len {
                let idx = (chosen_idx + offset) % len;
                match senders[idx].poll_send(cx, slot) {
                    Poll::Ready(Ok(())) => return Poll::Ready(true),
                    Poll::Ready(Err(())) => return Poll::Ready(false),
                    Poll::Pending => {}
                }
            }
            Poll::Pending
        }
    }
}

// ── Pipeline Task Functions ────────────────────────────────────────────────

/// Routes frame submissions to socket workers using pick-two load balancing.
///
/// Reads batches of frames from the sharded submission channel, groups consecutive frames for
/// the same path-secret entry, and routes each batch to a send socket via pick-two scheduling.
///
/// # Waker registration
///
/// The sharded receiver requires explicit waker registration before it can wake the task.
/// This function registers the waker on entry (before blocking) so the channel can wake us
/// when new frames arrive.
pub async fn frame_dispatch<Rand>(
    frame_rx: crate::stream3::frame::SubmissionReceiver,
    socket_senders: Vec<crate::socket::channel::cell::sync::Sender<FrameBatch>>,
    random: Rand,
) where
    Rand: Fn(usize) -> usize,
{
    // Register the receiver's waker before first poll so senders can wake us up.
    let mut pending_rx = Some(frame_rx);
    let frame_rx = core::future::poll_fn(|cx| {
        let rx = pending_rx.take().unwrap();
        rx.register(cx.waker());
        Poll::Ready(rx)
    })
    .await;

    let rx = crate::socket::channel::FlattenList::new(frame_rx);
    let rx = BatchFramesByPathSecret::new(rx);
    pick_two(rx, socket_senders, random).await;
}

/// Per-socket send worker: receives frame batches, assembles packets, sends via socket.
///
/// Maintains a per-peer [`send::Context`] cache. For each incoming [`FrameBatch`] the frames are
/// pushed onto the matching context's pending queue, [`assemble`] is called to encrypt and pack
/// them into GSO segments, and the segments are sent through `socket`. Concurrently, incoming
/// [`msg::Sender::Ack`] messages are decoded and fed into [`ack::process_ack`] to update CCA and
/// loss-recovery state.
///
/// [`send::Context`]: crate::stream3::endpoint::send::Context
/// [`assemble`]: crate::stream3::endpoint::assemble::assemble
/// [`ack::process_ack`]: crate::stream3::endpoint::ack::process_ack
pub async fn socket_send_task<Socket>(
    _socket: Socket,
    _batch_rx: crate::socket::channel::cell::sync::Receiver<FrameBatch>,
    _ack_rx: crate::socket::channel::intrusive_queue::sync::Receiver<
        crate::stream3::endpoint::msg::Sender,
    >,
    _sender_idx: usize,
    _source_control_port: u16,
    _gso: s2n_quic_platform::features::Gso,
    _pool: crate::socket::pool::Pool,
) where
    Socket: crate::socket::send::Socket,
{
    todo!("socket_send_task: assemble frames and send via socket — see stream3 send-path TODO")
}

/// Per-socket receive worker: reads raw UDP segments and routes decoded packets to dispatch.
///
/// Drives a [`SocketReceiver`] → [`FlattenSegments`] → [`RouterAdapter`] chain. Each segment is
/// handed to [`ChannelRouter`] which decodes the outer packet header, dispatches datagram packets
/// to `packet_tx`, and records decode errors.
///
/// [`SocketReceiver`]: crate::socket::channel::SocketReceiver
/// [`FlattenSegments`]: crate::socket::channel::FlattenSegments
/// [`RouterAdapter`]: crate::socket::channel::RouterAdapter
/// [`ChannelRouter`]: crate::stream3::endpoint::worker::ChannelRouter
pub async fn socket_recv_task<Socket>(
    socket: Socket,
    pool: crate::socket::pool::Pool,
    tx: crate::socket::channel::intrusive_queue::sync::Sender<
        crate::packet::datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    >,
    decode_error_counter: crate::counter::Counter,
) where
    Socket: crate::socket::recv::Socket,
{
    use crate::{
        socket::{
            channel::{FlattenSegments, InspectErr, Receiver as _, SocketReceiver},
            recv::router::Router as _,
        },
        stream3::endpoint::worker::ChannelRouter,
    };

    let rx = SocketReceiver::new(socket, pool);
    // SocketReceiver yields io::Result<Segments>; InspectErr logs errors and unwraps to Segments.
    let rx = InspectErr::new(rx, |err| {
        tracing::warn!(%err, "socket recv error");
    });
    let mut rx = FlattenSegments::new(rx);
    let mut router = ChannelRouter {
        tx,
        decode_error_counter,
    };

    loop {
        let Some(segment) = rx.recv().await else {
            break;
        };
        let bytes = segment.len() as u64;
        router.on_segment(segment);
        rx.on_consumed(bytes);
    }
}

/// Per-worker packet dispatch loop: decrypts, deduplicates, and dispatches received packets.
///
/// Owns a [`recv::Cache`] for per-sender crypto state. For each packet, calls
/// [`dispatch::process`] which:
/// * Authenticates and decrypts the payload,
/// * Deduplicates by packet number,
/// * Updates ACK state and ECN counts,
/// * Dispatches each decoded frame to its type handler (flow queues, ACK channels, acceptors).
///
/// Dispatch errors are silently dropped — they represent invalid/duplicate/unauthenticated
/// packets which should not terminate the worker.
///
/// [`recv::Cache`]: crate::stream3::endpoint::recv::Cache
/// [`dispatch::process`]: crate::stream3::endpoint::dispatch::process
pub async fn packet_dispatch_task<Clk>(
    mut packet_rx: crate::socket::channel::intrusive_queue::sync::Receiver<
        crate::packet::datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    >,
    worker_id: usize,
    idle_timeout: core::time::Duration,
    path_secret_map: crate::path::secret::Map,
    acceptor_registry: crate::acceptor::Registry<crate::stream3::Stream>,
    frame_tx: crate::stream3::frame::SubmissionSender,
    mut ack_sender: crate::stream3::endpoint::routing::AckSender,
    mut queue_dispatcher: crate::stream3::endpoint::msg::queue::Dispatcher,
    counters: crate::stream3::endpoint::counters::Dispatch,
    clock: Clk,
) where
    Clk: s2n_quic_core::time::Clock + crate::clock::precision::Clock,
{
    use crate::{
        socket::channel::Receiver as ChannelReceiver,
        stream3::endpoint::{dispatch, recv},
    };

    type PacketEntry = crate::intrusive_queue::Entry<
        crate::packet::datagram::decoder::Packet<crate::socket::pool::descriptor::Filled>,
    >;

    let mut recv_cache = recv::Cache::new(idle_timeout, worker_id);
    // Response frames (ACKs sent back to peers) re-enter via the same submission channel.
    let mut response_tx = frame_tx.clone();

    loop {
        let packet = core::future::poll_fn(|cx| {
            <_ as ChannelReceiver<PacketEntry>>::poll_recv(&mut packet_rx, cx)
        })
        .await;
        let Some(packet) = packet else {
            break;
        };

        let _ = dispatch::process(
            packet,
            &mut recv_cache,
            &path_secret_map,
            &acceptor_registry,
            &frame_tx,
            &mut response_tx,
            &mut ack_sender,
            &mut queue_dispatcher,
            &clock,
            &counters,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        byte_vec::ByteVec,
        path::secret::map::Entry as PathSecretEntry,
        stream3::frame::{Header, TransmissionStatus, DEFAULT_TTL},
    };
    use bytes::Bytes;
    use core::{future::Future, mem::MaybeUninit, task::Poll};
    use s2n_quic_core::varint::VarInt;
    use std::{
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };

    struct TestItem {
        path_secret_entry: Arc<PathSecretEntry>,
        byte_cost: u64,
        drop_counter: Arc<AtomicUsize>,
    }

    impl Drop for TestItem {
        fn drop(&mut self) {
            self.drop_counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl ByteCost for TestItem {
        fn byte_cost(&self) -> u64 {
            self.byte_cost
        }
    }

    impl PathSecretMapEntry for TestItem {
        fn path_secret_entry(&self) -> &Arc<PathSecretEntry> {
            &self.path_secret_entry
        }
    }

    #[derive(Clone, Copy)]
    enum SenderBehavior {
        Pending,
        ReadyOk,
        ReadyErr,
    }

    struct TestSender {
        behavior: SenderBehavior,
        calls: usize,
    }

    impl Sender<TestItem> for TestSender {
        fn poll_send(
            &mut self,
            _cx: &mut task::Context<'_>,
            value: &mut MaybeUninit<TestItem>,
        ) -> Poll<Result<(), ()>> {
            self.calls += 1;

            match self.behavior {
                SenderBehavior::Pending => Poll::Pending,
                SenderBehavior::ReadyOk => {
                    // SAFETY: successful send consumes the value.
                    unsafe { value.assume_init_drop() };
                    Poll::Ready(Ok(()))
                }
                SenderBehavior::ReadyErr => Poll::Ready(Err(())),
            }
        }
    }

    struct TestReceiver {
        values: VecDeque<TestItem>,
        consumed: u64,
    }

    impl Receiver<TestItem> for TestReceiver {
        fn poll_recv(&mut self, _cx: &mut task::Context<'_>) -> Poll<Option<TestItem>> {
            Poll::Ready(self.values.pop_front())
        }

        fn on_consumed(&mut self, bytes: u64) {
            self.consumed += bytes;
        }
    }

    struct TestFrameReceiver {
        values: VecDeque<Entry<Frame>>,
        consumed: u64,
    }

    impl Receiver<Entry<Frame>> for TestFrameReceiver {
        fn poll_recv(&mut self, _cx: &mut task::Context<'_>) -> Poll<Option<Entry<Frame>>> {
            Poll::Ready(self.values.pop_front())
        }

        fn on_consumed(&mut self, bytes: u64) {
            self.consumed += bytes;
        }
    }

    fn test_path_secret_entry() -> Arc<PathSecretEntry> {
        let peer = "127.0.0.1:4433"
            .parse()
            .expect("failed to parse hardcoded loopback address 127.0.0.1:4433");
        PathSecretEntry::fake(peer, None)
    }

    fn new_test_item(
        path_secret_entry: Arc<PathSecretEntry>,
        drop_counter: Arc<AtomicUsize>,
    ) -> TestItem {
        TestItem {
            path_secret_entry,
            byte_cost: 123,
            drop_counter,
        }
    }

    fn new_test_frame(path_secret_entry: Arc<PathSecretEntry>, payload_len: usize) -> Entry<Frame> {
        let mut payload = ByteVec::new();
        if payload_len > 0 {
            payload.push_back(Bytes::from(vec![0u8; payload_len]));
        }

        Entry::new(Frame {
            header: Header::Control {
                dest_sender_id: VarInt::from_u8(1),
            },
            source_sender_id: VarInt::MAX,
            payload,
            path_secret_entry,
            completion: None,
            status: TransmissionStatus::Pending,
            ttl: DEFAULT_TTL,
            transmission_time: None,
        })
    }

    fn with_noop_context<R>(f: impl FnOnce(&mut task::Context<'_>) -> R) -> R {
        let waker = s2n_quic_core::task::waker::noop();
        let mut cx = task::Context::from_waker(&waker);
        f(&mut cx)
    }

    #[test]
    fn selected_sender_is_polled_before_alternates() {
        let mut slot = MaybeUninit::new(new_test_item(
            test_path_secret_entry(),
            Arc::new(AtomicUsize::new(0)),
        ));
        let mut senders = vec![
            TestSender {
                behavior: SenderBehavior::ReadyOk,
                calls: 0,
            },
            TestSender {
                behavior: SenderBehavior::ReadyOk,
                calls: 0,
            },
        ];
        let result = with_noop_context(|cx| try_send_pick_two(cx, &mut slot, &mut senders, &|_| 0));
        assert_eq!(result, Poll::Ready(true));
        assert_eq!(senders[0].calls, 1);
        assert_eq!(senders[1].calls, 0);
    }

    #[test]
    fn falls_back_to_alternate_sender_when_selected_sender_is_pending() {
        let mut slot = MaybeUninit::new(new_test_item(
            test_path_secret_entry(),
            Arc::new(AtomicUsize::new(0)),
        ));
        let mut senders = vec![
            TestSender {
                behavior: SenderBehavior::Pending,
                calls: 0,
            },
            TestSender {
                behavior: SenderBehavior::ReadyOk,
                calls: 0,
            },
        ];
        let result = with_noop_context(|cx| try_send_pick_two(cx, &mut slot, &mut senders, &|_| 0));
        assert_eq!(result, Poll::Ready(true));
        assert_eq!(senders[0].calls, 1);
        assert_eq!(senders[1].calls, 1);
    }

    #[test]
    fn shuts_down_on_sender_error() {
        let mut slot = MaybeUninit::new(new_test_item(
            test_path_secret_entry(),
            Arc::new(AtomicUsize::new(0)),
        ));
        let mut senders = vec![
            TestSender {
                behavior: SenderBehavior::ReadyErr,
                calls: 0,
            },
            TestSender {
                behavior: SenderBehavior::ReadyOk,
                calls: 0,
            },
        ];
        let result = with_noop_context(|cx| try_send_pick_two(cx, &mut slot, &mut senders, &|_| 0));
        assert_eq!(result, Poll::Ready(false));
        assert_eq!(senders[0].calls, 1);
        assert_eq!(senders[1].calls, 0);

        // SAFETY: `Err` keeps the value in slot and caller must drop it.
        unsafe { slot.assume_init_drop() };
    }

    #[test]
    fn pick_two_drops_unsent_entry_on_shutdown() {
        let drop_counter = Arc::new(AtomicUsize::new(0));
        let rx = TestReceiver {
            values: [new_test_item(test_path_secret_entry(), drop_counter.clone())].into(),
            consumed: 0,
        };
        let senders = vec![TestSender {
            behavior: SenderBehavior::ReadyErr,
            calls: 0,
        }];
        let mut fut = core::pin::pin!(pick_two(rx, senders, |_| 0));
        let result = with_noop_context(|cx| fut.as_mut().poll(cx));
        assert_eq!(result, Poll::Ready(()));
        assert_eq!(drop_counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn batch_frames_groups_by_same_path_secret() {
        let path_a = test_path_secret_entry();
        let path_b = test_path_secret_entry();
        path_a.update_max_datagram_size(4_096);
        path_b.update_max_datagram_size(4_096);

        let rx = TestFrameReceiver {
            values: VecDeque::from([
                new_test_frame(path_a.clone(), 16),
                new_test_frame(path_a.clone(), 16),
                new_test_frame(path_b.clone(), 16),
            ]),
            consumed: 0,
        };
        let mut batcher = BatchFramesByPathSecret::new(rx);

        let first = with_noop_context(|cx| batcher.poll_recv(cx));
        let Poll::Ready(Some(first)) = first else {
            panic!("expected first batch");
        };
        assert_eq!(first.len(), 2);
        assert!(Arc::ptr_eq(first.path_secret_entry(), &path_a));

        let second = with_noop_context(|cx| batcher.poll_recv(cx));
        let Poll::Ready(Some(second)) = second else {
            panic!("expected second batch");
        };
        assert_eq!(second.len(), 1);
        assert!(Arc::ptr_eq(second.path_secret_entry(), &path_b));
    }

    #[test]
    fn batch_frames_enforces_datagram_byte_budget() {
        let path = test_path_secret_entry();
        path.update_max_datagram_size(220);

        let rx = TestFrameReceiver {
            values: VecDeque::from([
                new_test_frame(path.clone(), 70),
                new_test_frame(path.clone(), 70),
                new_test_frame(path.clone(), 70),
            ]),
            consumed: 0,
        };
        let mut batcher = BatchFramesByPathSecret::new(rx);

        let first = with_noop_context(|cx| batcher.poll_recv(cx));
        let Poll::Ready(Some(first)) = first else {
            panic!("expected first batch");
        };
        assert_eq!(first.len(), 1);
        assert!(first.byte_cost() <= 220);
        let frame_cost = first
            .queue()
            .peek_front()
            .expect("batch must contain the first frame")
            .byte_cost();
        assert!(first.byte_cost().saturating_add(frame_cost) > 220);

        let second = with_noop_context(|cx| batcher.poll_recv(cx));
        let Poll::Ready(Some(second)) = second else {
            panic!("expected second batch");
        };
        assert_eq!(second.len(), 1);

        let third = with_noop_context(|cx| batcher.poll_recv(cx));
        let Poll::Ready(Some(third)) = third else {
            panic!("expected third batch");
        };
        assert_eq!(third.len(), 1);
    }

    #[test]
    fn batch_frames_forwards_on_consumed() {
        let path = test_path_secret_entry();
        let rx = TestFrameReceiver {
            values: VecDeque::from([new_test_frame(path, 0)]),
            consumed: 0,
        };
        let mut batcher = BatchFramesByPathSecret::new(rx);

        batcher.on_consumed(321);
        assert_eq!(batcher.inner.consumed, 321);
    }
}
