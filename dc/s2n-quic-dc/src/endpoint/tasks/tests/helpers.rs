// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    endpoint::{
        combinator::{AssemblerCounters, FrameBatch},
        frame::{self, Frame, Header},
        msg, send,
    },
    intrusive::Entry,
    packet::datagram::QueuePair,
    path::secret::map::Entry as PathSecretEntry,
    socket::channel::{intrusive::unsync, Budget, EntryBoxSender, Receiver},
    stream::endpoint::recv,
    time::bach::Clock,
};
use core::task::{Poll, Waker};
use s2n_quic_core::varint::VarInt;
use std::{
    cell::RefCell,
    collections::VecDeque,
    net::SocketAddr,
    rc::Rc,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

// ── Test Waker ───────────────────────────────────────────────────────────

pub struct WakeCount(Arc<WakeCounter>);

impl WakeCount {
    pub fn count(&self) -> usize {
        self.0 .0.load(Ordering::Relaxed)
    }
}

struct WakeCounter(AtomicUsize);

impl std::task::Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn test_waker() -> (Waker, WakeCount) {
    let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let handle = WakeCount(counter.clone());
    let waker = Waker::from(counter);
    (waker, handle)
}

// ── Test Channels ────────────────────────────────────────────────────────

/// A pre-loaded receiver that yields items from a VecDeque, returning None when empty.
pub struct TestReceiver<T> {
    pub values: VecDeque<T>,
}

impl<T> TestReceiver<T> {
    pub fn new(values: impl IntoIterator<Item = T>) -> Self {
        Self {
            values: values.into_iter().collect(),
        }
    }
}

impl<T> Receiver<T> for TestReceiver<T> {
    fn poll_recv(
        &mut self,
        _cx: &mut core::task::Context<'_>,
        budget: &mut Budget,
    ) -> Poll<Option<T>> {
        if budget.is_exhausted() {
            budget.set_needs_wake();
            return Poll::Pending;
        }
        match self.values.pop_front() {
            Some(value) => {
                budget.consume();
                Poll::Ready(Some(value))
            }
            None => Poll::Ready(None),
        }
    }

    fn on_consumed(&mut self, _bytes: u64) {}
}

pub fn entry_channel<T>() -> (
    EntryBoxSender<T, unsync::Sender<crate::intrusive::EntryAdapter<T>>>,
    unsync::Receiver<crate::intrusive::EntryAdapter<T>>,
) {
    let (tx, rx) = unsync::new::<T>();
    (EntryBoxSender::new(tx), rx)
}

// ── ReceiverExt ──────────────────────────────────────────────────────────

pub trait TestReceiverExt<T>: Receiver<T> {
    /// Await the next item from the receiver, returning `None` if the channel is closed.
    async fn recv(&mut self) -> Option<T> {
        use core::future::poll_fn;
        let mut budget = Budget::new(1);
        poll_fn(|cx| {
            budget.reset();
            self.poll_recv(cx, &mut budget)
        })
        .await
    }
}

impl<T, R: Receiver<T>> TestReceiverExt<T> for R {}

// ── Recv Context Builder ─────────────────────────────────────────────────

pub struct RecvContextBuilder {
    peer: SocketAddr,
    remote_sender_id: VarInt,
    dest_sender_id: VarInt,
}

impl Default for RecvContextBuilder {
    fn default() -> Self {
        Self {
            peer: "127.0.0.1:4433".parse().unwrap(),
            remote_sender_id: VarInt::from_u8(0),
            dest_sender_id: VarInt::from_u8(1),
        }
    }
}

impl RecvContextBuilder {
    #[expect(dead_code)]
    pub fn peer(mut self, peer: SocketAddr) -> Self {
        self.peer = peer;
        self
    }

    pub fn remote_sender_id(mut self, id: VarInt) -> Self {
        self.remote_sender_id = id;
        self
    }

    #[expect(dead_code)]
    pub fn dest_sender_id(mut self, id: VarInt) -> Self {
        self.dest_sender_id = id;
        self
    }

    pub fn build(self) -> Rc<RefCell<recv::Context>> {
        let entry =
            PathSecretEntry::fake_deterministic(self.peer, s2n_quic_core::endpoint::Type::Server);
        let opener = entry.secret().application_opener(VarInt::ZERO);
        let clock = crate::time::bach::Clock::default();
        Rc::new(RefCell::new(recv::Context::new(
            entry,
            self.remote_sender_id,
            self.dest_sender_id,
            opener,
            VarInt::ZERO,
            crate::time::precision::Clock::now(&clock),
        )))
    }
}

/// Creates a `PathSecretEntry` routed to the address resolved from `addr`.
///
/// Both the path-secret addr and the peer-data addr are set to the resolved
/// address.  Accepts any Bach [`ToSocketAddrs`] — including group names like
/// `"server:4433"` — so tests can resolve simulated IPs registered by
/// `.group("server")` without hard-coding addresses.
///
/// [`ToSocketAddrs`]: bach::net::ToSocketAddrs
pub async fn test_entry_at(addr: impl bach::net::ToSocketAddrs) -> Arc<PathSecretEntry> {
    let addr = bach::net::lookup_host(addr)
        .await
        .expect("address resolution failed")
        .next()
        .expect("lookup_host returned empty iterator");
    let pse = PathSecretEntry::fake_deterministic(addr, s2n_quic_core::endpoint::Type::Client);
    pse.set_peer_data_addrs(&[addr]);
    pse
}

pub fn test_entry() -> Arc<PathSecretEntry> {
    let addr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
    let pse = PathSecretEntry::fake_deterministic(addr, s2n_quic_core::endpoint::Type::Client);
    pse.set_peer_data_addrs(&[addr]);
    pse
}

/// Creates a minimal FlowData frame for testing pipeline plumbing.
pub fn test_frame(pse: &Arc<PathSecretEntry>) -> Entry<Frame> {
    test_frame_with_payload(pse, 0)
}

/// Creates a FlowData frame whose application payload is `payload_size` zero bytes.
///
/// Use this to produce frames whose encoded size approaches or exceeds a single MTU
/// (1472 bytes in tests).  A payload of ~1300 bytes combined with packet overhead
/// (~80 bytes) fills one segment; a second frame of any size then pushes the
/// estimated packet length beyond the MTU limit, causing the assembler to push it
/// back and re-arm the TX wheel.
pub fn test_frame_with_payload(pse: &Arc<PathSecretEntry>, payload_size: usize) -> Entry<Frame> {
    Entry::new(Frame {
        header: Header::FlowData {
            queue_pair: QueuePair {
                source_queue_id: VarInt::from_u8(1),
                dest_queue_id: VarInt::from_u8(2),
            },
            stream_id: VarInt::from_u8(1),
            offset: VarInt::ZERO,
            is_fin: false,
        },
        source_sender_id: VarInt::MAX,
        payload: bytes::BytesMut::zeroed(payload_size).into(),
        path_secret_entry: pse.clone(),
        completion: None,
        status: frame::TransmissionStatus::Pending,
        ttl: 3,
        transmission_time: None,
    })
}

/// Creates a single-frame FrameBatch with sender_id=0.
pub fn test_batch(pse: &Arc<PathSecretEntry>) -> Entry<FrameBatch> {
    test_batch_with_payload(pse, 0)
}

/// Creates a single-frame FrameBatch carrying `payload_size` bytes of payload.
///
/// Two such batches pushed into a context (with `payload_size ≈ 1300`) will
/// produce enough pending data that the assembler can only send the first one
/// before hitting the per-segment MTU limit, leaving the second pending and
/// re-arming the TX wheel.
pub fn test_batch_with_payload(pse: &Arc<PathSecretEntry>, payload_size: usize) -> Entry<FrameBatch> {
    let mut batch = FrameBatch::single(test_frame_with_payload(pse, payload_size));
    batch.set_sender_id(0);
    Entry::new(batch)
}

/// Creates a `send::Context` for `entry` wrapped in `Rc<RefCell<_>>`.
///
/// All three queue-depth gauges are registered under `test.inflight`, `test.ack`,
/// and `test.pending`.  The context is immediately ready for use — callers push
/// frames and set `ctx.borrow_mut().tx_wheel.target_time` as needed.
pub fn build_send_context(
    entry: &Arc<PathSecretEntry>,
    sender_idx: usize,
    registry: &crate::counter::Registry,
    clock: &Clock,
) -> std::rc::Rc<std::cell::RefCell<send::Context>> {
    let ctx = send::Context::new(
        entry,
        registry.register_queue_gauge("test.inflight"),
        registry.register_queue_gauge("test.ack"),
        registry.register_queue_gauge("test.pending"),
        sender_idx,
        clock,
    )
    .expect("test context should be constructible");
    std::rc::Rc::new(std::cell::RefCell::new(ctx))
}

/// Binds an ephemeral UDP socket, runs `send_socket_assembler` with fixed test defaults
/// (source sender ID 0, port 0, `Gso::default()`, `Pool::new(u16::MAX)`, rate 100 Mbps),
/// and drains the pipeline to completion.
///
/// Use this as the body of a spawned assembler task to avoid repeating pipeline-wiring
/// boilerplate across tests.  Pass the output-side channel receivers to a separate task
/// or follow-on async block for assertions.
pub async fn assembler_pipeline(
    ctx_rx: unsync::Receiver<send::TxWheelAdapter>,
    clock: Clock,
    cancelled_tx: unsync::ListSender<crate::intrusive::EntryAdapter<Frame>>,
    ack_completions_tx: unsync::ListSender<crate::intrusive::EntryAdapter<msg::Sender>>,
    asm_counters: AssemblerCounters,
    tx_wheel_tx: unsync::Sender<send::TxWheelAdapter>,
    pto_wheel_tx: unsync::Sender<send::PtoWheelAdapter>,
    idle_wheel_tx: unsync::Sender<send::IdleWheelAdapter>,
) {
    use crate::{
        endpoint::tasks,
        socket::{pool::Pool, rate::Rate},
    };
    use bach::net::UdpSocket;
    use s2n_quic_core::varint::VarInt;
    use s2n_quic_platform::features::Gso;

    let socket = UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let rx = tasks::send_socket_assembler(
        ctx_rx,
        clock,
        VarInt::from_u8(0),
        0,
        Gso::default(),
        Pool::new(u16::MAX),
        cancelled_tx,
        ack_completions_tx,
        asm_counters,
        Rate::new(100.0),
        socket,
        tx_wheel_tx,
        pto_wheel_tx,
        idle_wheel_tx,
    );
    use crate::socket::channel::ReceiverExt as _;
    rx.drain_budgeted(Some(32)).await;
}
