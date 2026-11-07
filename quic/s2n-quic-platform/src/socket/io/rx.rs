// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    features,
    message::{BatchClock, Message},
    socket::ring::Consumer,
};
use core::{
    task::{Context, Poll},
    time::Duration,
};
use s2n_quic_core::{
    event,
    inet::datagram,
    io::rx,
    path::{LocalAddress, MaxMtu},
    task::waker,
};

/// Structure for receiving messages from consumer channels
pub struct Rx<T: Message> {
    channels: Vec<Consumer<T>>,
    max_mtu: MaxMtu,
    local_address: LocalAddress,
}

impl<T: Message> Rx<T> {
    #[inline]
    pub fn new(channels: Vec<Consumer<T>>, max_mtu: MaxMtu, local_address: LocalAddress) -> Self {
        Self {
            channels,
            max_mtu,
            local_address,
        }
    }
}

impl<T: Message> rx::Rx for Rx<T> {
    type PathHandle = T::Handle;
    type Queue = RxQueue<'static, T>;
    type Error = ();

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        waker::debug_assert_contract(cx, |cx| {
            let mut is_any_ready = false;
            let mut is_all_closed = true;

            // try to acquire any messages we can from the set of channels
            for channel in &mut self.channels {
                match channel.poll_acquire(u32::MAX, cx) {
                    Poll::Ready(_) => {
                        is_all_closed = false;
                        is_any_ready = true;
                    }
                    Poll::Pending => {
                        is_all_closed &= !channel.is_open();
                    }
                }
            }

            // if all of the channels are closed then shut down the task
            if is_all_closed {
                return Err(()).into();
            }

            // if any have items to be consumed the wake the endpoint up
            if is_any_ready {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })
    }

    #[inline]
    fn queue<F: FnOnce(&mut Self::Queue)>(&mut self, f: F) {
        let this: &'static mut Self = unsafe {
            // Safety: As noted in the [transmute examples](https://doc.rust-lang.org/std/mem/fn.transmute.html#examples)
            // it can be used to temporarily extend the lifetime of a reference. In this case, we
            // don't want to use GATs until the MSRV is >=1.65.0, which means `Self::Queue` is not
            // allowed to take generic lifetimes.
            //
            // We are left with using a `'static` lifetime here and encapsulating it in a private
            // field. The `Self::Queue` struct is then borrowed for the lifetime of the `F`
            // function. This will prevent the value from escaping beyond the lifetime of `&mut
            // self`.
            //
            // See https://play.rust-lang.org/?version=stable&mode=debug&edition=2021&gist=9a32abe85c666f36fb2ec86496cc41b4
            //
            // Once https://github.com/aws/s2n-quic/issues/1742 is resolved this code can go away
            core::mem::transmute(self)
        };

        let mut queue = RxQueue {
            channels: &mut this.channels,
            max_mtu: this.max_mtu,
            local_address: &this.local_address,
        };

        f(&mut queue);
    }

    #[inline]
    fn handle_error<E: event::EndpointPublisher>(self, _error: Self::Error, _events: &mut E) {
        // The only reason we would be returning an error is if a channel closed. This could either
        // be because the endpoint is shutting down or one of the tasks panicked. Either way, we
        // don't know what the cause is here so we don't have any events to emit.
    }
}

pub struct RxQueue<'a, T: Message> {
    channels: &'a mut [Consumer<T>],
    max_mtu: MaxMtu,
    local_address: &'a LocalAddress,
}

impl<T: Message> RxQueue<'_, T> {
    /// Calls `on_packet` for each received datagram, where the receive wait of each datagram
    /// ends at one read of `now` for the whole call
    ///
    /// The endpoint calls `for_each` once for a batch of datagrams, and reads its own clock as it
    /// handles the first of them, for the time that each datagram of the batch arrived. The call
    /// reads `now` as it reads the first message that holds the time the socket received it,
    /// just before the endpoint's read, so the wait of a datagram after the first doesn't count
    /// the time that the endpoint spent on the datagrams before it.
    #[inline]
    fn for_each_at<Now, F>(&mut self, now: Now, mut on_packet: F)
    where
        Now: FnMut() -> Option<Duration>,
        F: FnMut(datagram::Header<T::Handle>, &mut [u8]),
    {
        let mut clock = BatchClock::new(now);

        for channel in self.channels.iter_mut() {
            // one last effort to acquire items if some were received since we last polled
            let len = channel.acquire(u32::MAX);

            let data = channel.data();
            debug_assert_eq!(data.len(), len as usize);

            for message in data {
                // call the `on_packet` function for each message received
                //
                // NOTE: it's important that we process all of the messages in the queue as the
                //       channel is completely drained here.
                if let Some(message) = message.rx_read_at(self.local_address, &mut clock) {
                    message.for_each(|header, payload| {
                        clock.on_delivered();
                        on_packet(header, payload);
                    });
                }

                unsafe {
                    // Safety: the message was allocated with the configured MaxMtu
                    message.reset(self.max_mtu.into());
                }
            }

            // release the messages back to the producer
            channel.release(len);
        }
    }
}

impl<T: Message> rx::Queue for RxQueue<'_, T> {
    type Handle = T::Handle;

    #[inline]
    fn for_each<F: FnMut(datagram::Header<Self::Handle>, &mut [u8])>(&mut self, on_packet: F) {
        self.for_each_at(features::rx_timestamps::now, on_packet)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        false
    }
}

#[cfg(all(test, s2n_quic_platform_socket_msg, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::{
        message::{cmsg::Encoder as _, msg::Ext as _},
        socket::ring,
    };
    use core::cell::Cell;
    use s2n_quic_core::{
        inet::{ExplicitCongestionNotification, SocketAddress},
        path::{Handle as _, RemoteAddress},
    };

    /// A message that the socket can hand to the receive queue with the time it received the
    /// datagram
    trait Received: Message {
        /// Writes `received` as the time that the socket received the message's datagram
        fn set_received(&mut self, received: Duration);

        /// Leaves the message as the socket's receive leaves it, where that differs from a write
        fn as_received(&mut self) {}
    }

    /// Writes `received` in `msghdr`'s control messages as an `SCM_TIMESTAMPNS` message
    fn encode_received(msghdr: &mut libc::msghdr, received: Duration) {
        let mut time: libc::timespec = unsafe { core::mem::zeroed() };
        time.tv_sec = received.as_secs() as _;
        time.tv_nsec = received.subsec_nanos() as _;
        let (level, ty) = (libc::SOL_SOCKET, libc::SCM_TIMESTAMPNS);
        let mut encoder = msghdr.cmsg_encoder();
        assert!(encoder.encode_cmsg(level, ty, time).is_ok());
    }

    impl Received for libc::msghdr {
        fn set_received(&mut self, received: Duration) {
            encode_received(self, received);
        }
    }

    #[cfg(s2n_quic_platform_socket_mmsg)]
    impl Received for libc::mmsghdr {
        fn set_received(&mut self, received: Duration) {
            encode_received(&mut self.msg_hdr, received);
        }

        fn as_received(&mut self) {
            // `recvmmsg` writes the datagram's length in `msg_len`, and leaves the buffer's
            // length in `msg_hdr` as `reset` set it
            unsafe {
                self.msg_hdr.set_payload_len(MaxMtu::default().into());
            }
        }
    }

    impl Received for crate::message::simple::Message {
        fn set_received(&mut self, _received: Duration) {
            unreachable!("a simple message doesn't hold the time the socket received it");
        }
    }

    /// The payload of each datagram that `send` writes
    const PAYLOAD: [u8; 4] = [1; 4];

    /// Writes a datagram into `producer` for each of `received`, which holds the time that the
    /// socket received the datagram where the socket reported one
    fn send<T: Received>(producer: &mut ring::Producer<T>, received: &[Option<Duration>]) {
        let count = received.len() as u32;
        assert!(producer.acquire(count) >= count);

        for (entry, received) in producer.data().iter_mut().zip(received) {
            unsafe {
                entry.reset(MaxMtu::default().into());
            }

            let remote_address = RemoteAddress::from(SocketAddress::default());
            let handle = <T as Message>::Handle::from_remote_address(remote_address);
            let ecn = ExplicitCongestionNotification::default();
            entry.tx_write((handle, ecn, &PAYLOAD[..])).unwrap();

            if let Some(received) = received {
                entry.set_received(*received);
            }

            entry.as_received();
        }

        producer.release(count);
    }

    /// Reads a batch from `channels` on a clock that answers `clock`, where the endpoint spends
    /// `spent` on each datagram, and returns each datagram's receive wait and how many times the
    /// batch read the clock
    fn read<T: Message>(
        channels: &mut [ring::Consumer<T>],
        clock: &Cell<Duration>,
        spent: Duration,
    ) -> (Vec<Option<Duration>>, usize) {
        let local_address = LocalAddress::from(SocketAddress::default());
        let mut queue = RxQueue {
            channels,
            max_mtu: MaxMtu::default(),
            local_address: &local_address,
        };

        let reads = Cell::new(0);
        let mut waits = vec![];
        queue.for_each_at(
            || {
                reads.set(reads.get() + 1);
                Some(clock.get())
            },
            |header, payload| {
                assert_eq!(&*payload, &PAYLOAD[..]);
                waits.push(header.receive_wait);
                clock.set(clock.get() + spent);
            },
        );

        (waits, reads.get())
    }

    /// The receive wait of each datagram of a batch ends at the batch's one read of the clock,
    /// and doesn't count the time that the endpoint spent on the datagrams before it
    fn batch_test<T: Received>() {
        let (mut producer, consumer) = ring::pair::<T>(4, u16::from(MaxMtu::default()) as _);
        let mut channels = vec![consumer];
        let ms = Duration::from_millis;
        let received = Duration::from_secs(10);

        // The socket received the datagrams 3 ms and 2 ms before the batch's read, and the
        // endpoint spends 5 ms on each datagram
        send(&mut producer, &[Some(received), Some(received + ms(1))]);
        let clock = Cell::new(received + ms(3));
        let (waits, reads) = read(&mut channels, &clock, ms(5));
        assert_eq!(waits, [Some(ms(3)), Some(ms(2))]);
        assert_eq!(reads, 1);

        // The next batch reads the clock again
        send(&mut producer, &[Some(received + ms(10))]);
        let clock = Cell::new(received + ms(13));
        let (waits, reads) = read(&mut channels, &clock, ms(5));
        assert_eq!(waits, [Some(ms(3))]);
        assert_eq!(reads, 1);

        // The socket received a datagram on each of two channels, 3 ms and 2 ms before the
        // batch's read: the batch reads the clock once for both channels, so the second
        // datagram's wait doesn't count the 5 ms that the endpoint spent on the first
        let (mut other_producer, other_consumer) =
            ring::pair::<T>(4, u16::from(MaxMtu::default()) as _);
        channels.push(other_consumer);
        send(&mut producer, &[Some(received + ms(20))]);
        send(&mut other_producer, &[Some(received + ms(21))]);
        let clock = Cell::new(received + ms(23));
        let (waits, reads) = read(&mut channels, &clock, ms(5));
        assert_eq!(waits, [Some(ms(3)), Some(ms(2))]);
        assert_eq!(reads, 1);
    }

    /// Where a datagram without a receive time goes to the endpoint before the batch reads the
    /// clock, the endpoint reads its own clock at that datagram, so a later datagram of the
    /// batch doesn't hold a wait, and the batch doesn't read the clock
    fn batch_without_first_receive_time_test<T: Received>() {
        let (mut producer, consumer) = ring::pair::<T>(4, u16::from(MaxMtu::default()) as _);
        let mut channels = vec![consumer];
        let ms = Duration::from_millis;
        let received = Duration::from_secs(10);

        send(&mut producer, &[None, Some(received)]);
        let clock = Cell::new(received + ms(3));
        let (waits, reads) = read(&mut channels, &clock, ms(5));
        assert_eq!(waits, [None::<Duration>, None]);
        assert_eq!(reads, 0);
    }

    /// A message type that doesn't report the time the socket received a datagram reads each
    /// message as its `rx_read` does, so the batch's datagrams don't hold a wait, and the batch
    /// doesn't read the clock
    #[test]
    fn simple_batch_test() {
        type Simple = crate::message::simple::Message;
        let (mut producer, consumer) = ring::pair::<Simple>(4, u16::from(MaxMtu::default()) as _);
        let mut channels = vec![consumer];
        let clock = Cell::new(Duration::from_secs(10));

        send(&mut producer, &[None, None]);
        let (waits, reads) = read(&mut channels, &clock, Duration::from_millis(5));
        assert_eq!(waits, [None::<Duration>, None]);
        assert_eq!(reads, 0);
    }

    /// The receive queue reads the realtime clock once for a batch, so two datagrams that the
    /// socket received at the same time hold the same wait, though the endpoint spends time on
    /// the first before it handles the second
    fn realtime_batch_test<T: Received>() {
        let (mut producer, consumer) = ring::pair::<T>(4, u16::from(MaxMtu::default()) as _);
        let mut channels = vec![consumer];
        let received = Duration::from_secs(1);
        send(&mut producer, &[Some(received), Some(received)]);

        let local_address = LocalAddress::from(SocketAddress::default());
        let mut queue = RxQueue {
            channels: &mut channels[..],
            max_mtu: MaxMtu::default(),
            local_address: &local_address,
        };

        let mut waits = vec![];
        rx::Queue::for_each(&mut queue, |header, payload| {
            assert_eq!(&*payload, &PAYLOAD[..]);
            waits.push(header.receive_wait);
            // The sleep moves the realtime clock on, so a read of the clock for each datagram
            // would answer a longer wait for the second datagram
            std::thread::sleep(Duration::from_millis(1));
        });

        assert_eq!(waits.len(), 2);
        assert!(waits[0].is_some());
        assert_eq!(waits[0], waits[1]);
    }

    #[test]
    fn msg_batch_test() {
        batch_test::<crate::message::msg::Message>();
    }

    #[test]
    fn msg_batch_without_first_receive_time_test() {
        batch_without_first_receive_time_test::<crate::message::msg::Message>();
    }

    #[test]
    fn msg_realtime_batch_test() {
        realtime_batch_test::<crate::message::msg::Message>();
    }

    #[test]
    #[cfg(s2n_quic_platform_socket_mmsg)]
    fn mmsg_batch_test() {
        batch_test::<crate::message::mmsg::Message>();
    }

    #[test]
    #[cfg(s2n_quic_platform_socket_mmsg)]
    fn mmsg_batch_without_first_receive_time_test() {
        batch_without_first_receive_time_test::<crate::message::mmsg::Message>();
    }

    #[test]
    #[cfg(s2n_quic_platform_socket_mmsg)]
    fn mmsg_realtime_batch_test() {
        realtime_batch_test::<crate::message::mmsg::Message>();
    }
}
