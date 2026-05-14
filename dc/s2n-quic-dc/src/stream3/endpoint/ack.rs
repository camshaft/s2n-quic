// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! ACK processing and loss detection for the send path.
//!
//! Processes incoming ACK frames against the send::Context's inflight map. When packets
//! are acknowledged, their constituent frames get completion notifications. When packets
//! are declared lost, frames are individually evaluated for retransmission (TTL, cancellation)
//! and survivors are requeued to the wheel.

pub(crate) mod state;

use crate::{
    congestion,
    intrusive_queue::Entry,
    random,
    socket::channel::UnboundedSender,
    stream3::{
        endpoint::send,
        frame::{self, Frame, TransmissionStatus},
    },
};
use core::time::Duration;
use s2n_quic_core::{
    frame::{
        self as quic_frame,
        ack::{AckRanges, EcnCounts},
    },
    packet::number::{PacketNumber, PacketNumberRange, PacketNumberSpace},
    varint::VarInt,
};

/// Process an ACK frame against the send context.
///
/// Removes ACKed packets from the inflight map:
/// - **completed**: frames the writer needs to hear about (successfully ACKed or TTL-exhausted)
/// - **lost**: retransmittable frames (TTL remaining, still transmittable)
/// - **cancelled**: `should_transmit()` is false (writer already gone) — silently dropped
pub(crate) fn process_ack<Clk, Rand>(
    ack: &quic_frame::Ack<impl AckRanges>,
    context: &mut send::Context,
    counters: &super::counters::Send,
    completed: &mut impl UnboundedSender<Entry<Frame>>,
    lost: &mut impl UnboundedSender<Entry<Frame>>,
    cancelled: &mut impl UnboundedSender<Entry<Frame>>,
    clock: &Clk,
    random: &mut Rand,
) where
    Clk: s2n_quic_core::time::Clock + ?Sized,
    Rand: random::Generator,
{
    let now = clock.get_time();
    let ack_delay = ack.ack_delay();

    let mut max_acked_tx_time = None;
    let mut bytes_acked = 0usize;
    let mut cca_args = None;

    let max_acked_pn = ack.largest_acknowledged();

    for range in ack.ack_ranges() {
        let pmin = PacketNumberSpace::Initial.new_packet_number(*range.start());
        let pmax = PacketNumberSpace::Initial.new_packet_number(*range.end());
        let range = PacketNumberRange::new(pmin, pmax);

        // Phase 1: remove ACKed entries from the inflight map.
        //
        // Shell entries (probed_to.is_some()) have empty `frames`; the live frames
        // reside at the tail of the probe chain at a higher PN. We defer chain
        // following until after the iterator is dropped (so the borrow on
        // `context.inflight` is released).
        let mut deferred: Vec<PacketNumber> = Vec::new();

        for (num, mut packet) in context.inflight.remove_range(range) {
            if let Some(tx_info) = packet.transmission_info.take() {
                let time_sent = tx_info.time_sent;
                max_acked_tx_time = max_acked_tx_time.max(Some(time_sent));

                if cca_args
                    .as_ref()
                    .map_or(true, |(prev_time, _): &(_, congestion::PacketInfo)| {
                        *prev_time < time_sent
                    })
                {
                    cca_args = Some((time_sent, tx_info.cc_info));
                }

                bytes_acked += tx_info.sent_bytes as usize;
            }

            tracing::trace!(
                credentials = %context.credentials.id,
                sender_idx = context.sender_idx,
                packet_number = num.as_u64(),
                "packet ACKed"
            );

            if let Some(probe_pn) = packet.probed_to {
                // Shell: the live frames are at the tail of the probe chain.
                // Defer completion to Phase 2 (after the iterator is dropped).
                deferred.push(probe_pn);
            } else {
                for mut entry in packet.frames {
                    entry.status = TransmissionStatus::Acknowledged;
                    let _ = completed.send(entry);
                }
            }
        }
        // remove_range iterator is dropped here; borrow on `context.inflight` released.

        // Phase 2: drain deferred probe chains.
        //
        // `drain_chain_from` walks from the deferred PN to the tail, removes every
        // entry visited (preventing ghost entries that would otherwise keep the PTO
        // wheel armed and trigger spurious loss detection), and returns the tail's
        // live frames together with the TransmissionInfos of all removed entries so
        // that bytes-in-flight accounting stays accurate.
        for probe_pn in deferred {
            let (tail_frames, chain_tx_infos) = context.inflight.drain_chain_from(probe_pn);
            for mut entry in tail_frames {
                entry.status = TransmissionStatus::Acknowledged;
                let _ = completed.send(entry);
            }
            // Account for the implicitly-acknowledged probe entries in CCA.
            for tx_info in chain_tx_infos {
                let time_sent = tx_info.time_sent;
                max_acked_tx_time = max_acked_tx_time.max(Some(time_sent));
                bytes_acked += tx_info.sent_bytes as usize;
                if cca_args
                    .as_ref()
                    .map_or(true, |(prev_time, _): &(_, congestion::PacketInfo)| {
                        *prev_time < time_sent
                    })
                {
                    cca_args = Some((time_sent, tx_info.cc_info));
                }
            }
        }
    }

    // Update RTT estimator and CCA
    if let Some((time_sent, cc_info)) = cca_args {
        let rtt_sample = now
            .saturating_duration_since(time_sent)
            .saturating_sub(ack_delay)
            .max(Duration::from_micros(1));

        context.rtt_estimator.update_rtt(
            Duration::ZERO,
            rtt_sample,
            now,
            true,
            PacketNumberSpace::ApplicationData,
        );

        context.cca.on_packet_ack(
            cc_info.first_sent_time,
            bytes_acked,
            cc_info,
            &context.rtt_estimator,
            random,
            now,
        );

        context.publish_next_transmission_time(now);
    }

    // Process ECN feedback from the peer
    if let Some(ecn_counts) = ack.ecn_counts {
        let prev = context.peer_ecn_counts;
        context.peer_ecn_counts = ecn_counts.max(prev);
        let mut delta = context.peer_ecn_counts;
        delta -= prev;
        if delta != EcnCounts::default() {
            counters.on_peer_ecn(&delta);
            let ce_delta = delta.ce_count.as_u64();
            if ce_delta > 0 {
                context.cca.on_explicit_congestion(ce_delta, now);
            }
        }
    }

    // Run loss detection
    if let Some(max_tx_time) = max_acked_tx_time {
        detect_loss(
            context,
            max_acked_pn,
            max_tx_time,
            completed,
            lost,
            cancelled,
            now,
            random,
        );
    }

    // Update PTO
    let has_remaining_inflight = context.inflight.has_inflight();
    context.pto.on_ack_received(has_remaining_inflight);
}

/// Detect lost packets using the QUIC PN-threshold algorithm.
///
/// Packets with number <= max_acked_pn - 3 are declared lost. For each lost packet,
/// frames are individually evaluated:
/// - should_transmit false → sent to `cancelled` (writer already gone, no notification)
/// - TTL exhausted → sent to `completed` (writer needs failure notification)
/// - Otherwise → decrement TTL and send to `lost` for retransmission
///
/// TODO: Add time-based loss detection (kTimeThreshold = 9/8 * max(smoothed_rtt, latest_rtt)).
fn detect_loss<Rand>(
    context: &mut send::Context,
    max_acked_pn: VarInt,
    max_tx_time: s2n_quic_core::time::Timestamp,
    completed: &mut impl UnboundedSender<Entry<Frame>>,
    lost: &mut impl UnboundedSender<Entry<Frame>>,
    cancelled: &mut impl UnboundedSender<Entry<Frame>>,
    now: s2n_quic_core::time::Timestamp,
    random: &mut Rand,
) where
    Rand: random::Generator,
{
    // TODO: use max_tx_time for time-based loss detection
    let _ = max_tx_time;

    let pn_threshold = max_acked_pn.checked_sub(VarInt::from_u8(3));

    let lost_min = PacketNumberSpace::Initial.new_packet_number(VarInt::ZERO);
    let lost_max = pn_threshold.map(|v| PacketNumberSpace::Initial.new_packet_number(v));

    let Some(lost_max) = lost_max else {
        return;
    };

    let range = PacketNumberRange::new(lost_min, lost_max);
    let mut lost_count = 0usize;
    let mut cancelled_count = 0usize;

    for (num, mut packet) in context.inflight.remove_range(range) {
        let tx_info = packet.transmission_info.take().unwrap();

        tracing::trace!(
            pn = num.as_u64(),
            max_acked = max_acked_pn.as_u64(),
            time_sent = ?tx_info.time_sent,
            "Packet lost by PN threshold"
        );

        context
            .cca
            .on_packet_lost(tx_info.sent_bytes as u32, tx_info.cc_info, random, now);

        for mut entry in packet.frames {
            if !entry.should_transmit() {
                entry.status = TransmissionStatus::Failed(frame::FailureReason::Cancelled);
                let _ = cancelled.send(entry);
                cancelled_count += 1;
                continue;
            }

            if entry.ttl == 0 {
                entry.status = TransmissionStatus::Failed(frame::FailureReason::TransmissionError);
                let _ = completed.send(entry);
                lost_count += 1;
                continue;
            }

            entry.ttl -= 1;
            let _ = lost.send(entry);
            lost_count += 1;
        }
    }

    if lost_count + cancelled_count > 0 {
        tracing::debug!(
            lost_count,
            cancelled_count,
            max_acked = max_acked_pn.as_u64(),
            threshold = pn_threshold.map(|v| v.as_u64()),
            rtt = ?context.rtt_estimator.smoothed_rtt(),
            "Loss detection triggered"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        byte_vec::ByteVec,
        counter::Registry,
        intrusive_queue::Queue,
        packet::datagram::QueuePair,
        path::secret::map::Entry as PathSecretEntry,
        stream3::{
            endpoint::{
                counters,
                inflight::{Packet, TransmissionInfo},
                send,
            },
            frame::{Frame, Header, TransmissionStatus, DEFAULT_TTL},
        },
        xorshift::Rng,
    };
    use bytes::Bytes;
    use core::time::Duration;
    use s2n_quic_core::{
        ack,
        packet::number::PacketNumberSpace,
        recovery::RttEstimator,
        varint::VarInt,
    };
    use std::sync::Arc;

    fn make_pn(n: u64) -> PacketNumber {
        PacketNumberSpace::Initial.new_packet_number(VarInt::new(n).unwrap())
    }

    fn make_frame() -> crate::intrusive_queue::Entry<Frame> {
        let entry = PathSecretEntry::fake("127.0.0.1:9999".parse().unwrap(), None);
        let mut payload = ByteVec::new();
        payload.push_back(Bytes::from_static(b"x"));
        Frame {
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
            payload,
            path_secret_entry: entry,
            completion: None,
            status: TransmissionStatus::default(),
            ttl: DEFAULT_TTL,
            transmission_time: None,
        }
        .into()
    }

    fn make_send_context(registry: &Registry) -> send::Context {
        let entry = PathSecretEntry::fake("127.0.0.1:9999".parse().unwrap(), None);
        let inflight_gauge = registry.register_queue_gauge("test.inflight");
        let ack_gauge = registry.register_queue_gauge("test.ack");
        let pending_gauge = registry.register_queue_gauge("test.pending");
        send::Context::new(&entry, inflight_gauge, ack_gauge, pending_gauge, 0)
    }

    /// Regression test for the deferred-chain overflow bug.
    ///
    /// `process_ack` uses an `ArrayVec<PacketNumber, 8>` (called `deferred`) to buffer
    /// the `probed_to` targets of shell entries found in each ACK range iteration.
    /// Phase 2 then follows each deferred entry to complete the live frames at the chain
    /// tail.
    ///
    /// **Bug**: `deferred.try_push` silently discards any entry beyond the 8th when the
    /// array is full.  If a single contiguous ACK range covers 9 or more shell entries,
    /// the 9th (and beyond) probe chains are never followed.  The frames at those chain
    /// tails are stuck in the inflight map:
    ///
    /// - The writer's completion callback is never invoked for those frames.
    /// - The stuck entries linger until loss detection eventually removes them, at which
    ///   point they trigger spurious `on_packet_lost` calls (unnecessary CWND reduction)
    ///   and the frames may be retransmitted even though the data was acknowledged.
    ///
    /// The comment in the source code states that 8 slots is "more than sufficient"
    /// because the PTO backoff cap bounds the *depth* of a single probe chain.  This is
    /// incorrect: the PTO backoff cap limits how many times *one* original packet can be
    /// probed, not how many distinct shell entries can appear in a single ACK range.  A
    /// burst of 9+ original packets that each received one PTO probe before a large ACK
    /// arrives is a realistic production scenario (e.g. after a brief network outage).
    #[test]
    fn process_ack_completes_all_nine_shells_in_one_range() {
        const NUM_SHELLS: u64 = 9; // one more than the ArrayVec<_, 8> capacity

        let registry = Registry::default();
        let counters = counters::Send::new(&registry);
        let mut rng = Rng::new();
        let mut ctx = make_send_context(&registry);

        // Fixed timestamp for all CCA bookkeeping.
        let now: s2n_quic_core::time::Timestamp =
            unsafe { s2n_quic_core::time::Timestamp::from_duration(Duration::from_millis(100)) };
        let rtt = RttEstimator::new(Duration::from_millis(2));

        // The inflight map requires strictly monotonically increasing packet numbers.
        // Strategy: insert ALL original packets first (PNs 0..NUM_SHELLS-1), then
        // probe each in order (PNs NUM_SHELLS..2*NUM_SHELLS-1).  This guarantees the
        // PN sequence is always increasing when we insert.
        //
        //   PNs 0..NUM_SHELLS-1   : original (will become shells)
        //   PNs NUM_SHELLS..2*NUM_SHELLS-1 : probes
        //
        // All shells have consecutive PNs 0..NUM_SHELLS-1, so a single contiguous ACK
        // range [0, NUM_SHELLS-1] causes a single iteration of the
        // `for range in ack.ack_ranges()` loop — where `deferred` lives — and the
        // 9th try_push silently overflows.
        //
        // Use ctx.cca for all on_packet_sent calls so the CCA state stays consistent
        // with what process_ack's on_packet_ack will see.

        // Step 1: insert all original packets.
        for i in 0..NUM_SHELLS {
            let shell_pn = make_pn(i);
            let mut frames = Queue::new();
            frames.push_back(make_frame());
            let cc_info = ctx.cca.on_packet_sent(now, 100, false, &rtt);
            ctx.inflight.insert(
                shell_pn,
                Packet::new(
                    frames,
                    TransmissionInfo {
                        cc_info,
                        time_sent: now,
                        sent_bytes: 100,
                    },
                ),
            );
        }

        // Step 2: simulate PTO for each original packet in order.
        // take_oldest_for_probe always returns the smallest PN with non-empty frames,
        // which is exactly make_pn(i) during each iteration.
        for i in 0..NUM_SHELLS {
            let shell_pn = make_pn(i);
            let probe_pn = make_pn(NUM_SHELLS + i); // always > max existing PN

            let (taken_pn, probe_frames) = ctx.inflight.take_oldest_for_probe().unwrap();
            assert_eq!(taken_pn, shell_pn, "must take PN {i}");

            let cc_probe = ctx.cca.on_packet_sent(now, 100, false, &rtt);
            ctx.inflight.insert(
                probe_pn,
                Packet::new(
                    probe_frames,
                    TransmissionInfo {
                        cc_info: cc_probe,
                        time_sent: now,
                        sent_bytes: 100,
                    },
                ),
            );
            ctx.inflight.set_probed_to(shell_pn, probe_pn);
        }

        // Build a single contiguous ACK range covering all NUM_SHELLS shell PNs.
        // Using one range ensures every shell falls inside the same `deferred`
        // ArrayVec instance and the overflow is triggered.
        let mut ack_ranges = ack::Ranges::new(usize::MAX);
        ack_ranges
            .insert_packet_number_range(PacketNumberRange::new(
                make_pn(0),
                make_pn(NUM_SHELLS - 1),
            ))
            .expect("range fits");
        let ack_frame = quic_frame::Ack {
            ack_delay: VarInt::ZERO,
            ack_ranges: &ack_ranges,
            ecn_counts: None,
        };

        // Sink for completed, lost, and cancelled frames.
        let mut completed: Queue<Frame> = Queue::new();
        let mut lost: Queue<Frame> = Queue::new();
        let mut cancelled: Queue<Frame> = Queue::new();

        process_ack(
            &ack_frame,
            &mut ctx,
            &counters,
            &mut completed,
            &mut lost,
            &mut cancelled,
            &now, // s2n_quic_core::time::Timestamp implements s2n_quic_core::time::Clock
            &mut rng,
        );

        // Each probe had exactly one data frame, so we expect NUM_SHELLS completions.
        let completed_count = completed.len();
        assert_eq!(
            completed_count,
            NUM_SHELLS as usize,
            "all {NUM_SHELLS} chain tails must be completed after one ACK range \
             covering all {NUM_SHELLS} shells; only {completed_count} were — \
             deferred ArrayVec<PacketNumber, 8> silently dropped entry #9 (overflow)"
        );

        // No probe entries should be left with un-drained frames.
        // Probes were inserted at PNs NUM_SHELLS..2*NUM_SHELLS-1.
        for i in 0..NUM_SHELLS {
            let probe_pn = make_pn(NUM_SHELLS + i);
            if let Some(packet) = ctx.inflight.get_mut(probe_pn) {
                assert!(
                    packet.frames.is_empty(),
                    "probe entry at PN {} has un-drained frames — chain completion was skipped",
                    NUM_SHELLS + i
                );
            }
        }
    }
}
