// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Packet number map for tracking sent packets in the frame aggregation model.
//!
//! Each packet number maps to a PacketEntry containing a list of Frames and shared
//! transmission metadata. When a packet is ACKed, all constituent frames get their
//! completion notifications. When a packet is lost, frames are individually evaluated
//! for retransmission (checking TTL and should_transmit).

use crate::{congestion, counter::QueueGauge, intrusive_queue::Queue, stream3::frame::Frame};
use s2n_quic_core::{
    packet::number::{Map as Inner, PacketNumber, PacketNumberRange},
    varint::VarInt,
};

/// Metadata captured when a packet is sent, shared across all frames in that packet.
pub(crate) struct TransmissionInfo {
    pub cc_info: congestion::PacketInfo,
    pub time_sent: s2n_quic_core::time::Timestamp,
    /// Total bytes on the wire for this packet (all frame payloads + headers + encryption overhead)
    pub sent_bytes: u16,
}

/// A single entry in the packet number map.
///
/// Contains all frames that were packed into this packet, plus the shared transmission
/// metadata. When ACKed, each frame's completion fires. When lost, each frame is
/// individually evaluated for retransmission.
pub(crate) struct Packet {
    /// All frames packed into this packet.
    ///
    /// When the packet is ACKed, each frame's completion notification fires. When the
    /// packet is declared lost, each frame is individually evaluated for retransmission.
    /// When this packet is a "shell" (probed to a newer PN), the list will be empty
    /// because the frames have been moved to the probe entry.
    ///
    /// ACK frames (immediate/control frames) are always at the **beginning** of the
    /// queue, followed by non-ACK (data) frames. `ack_frame_count` records how many
    /// leading frames are ACK frames.
    pub frames: Queue<Frame>,
    /// Number of leading ACK (control/immediate) frames in `frames`.
    ///
    /// ACK frames are stale by definition and must not be retransmitted as part of a
    /// PTO probe. [`take_oldest_for_probe`] uses this count to skip them efficiently
    /// without scanning the whole queue.
    ///
    /// [`take_oldest_for_probe`]: Self::take_oldest_for_probe
    pub ack_frame_count: usize,
    /// Transmission metadata shared by all frames in this packet (CCA info, send time,
    /// wire byte count). Taken on first ACK or loss so that RTT/CCA updates are applied
    /// exactly once.
    pub transmission_info: Option<TransmissionInfo>,
    /// PTO probe chain forward pointer.
    ///
    /// When a PTO fires and the assembler retransmits this packet's frames under a new
    /// packet number, `probed_to` is set to that new PN and `frames` is emptied (this
    /// entry becomes a "shell"). The chain can extend across multiple PTO firings:
    ///
    /// ```text
    /// PN_0 (shell, probed_to=PN_1) -> PN_1 (shell, probed_to=PN_2) -> PN_2 (live frames)
    /// ```
    ///
    /// ACK processing follows the chain to the tail to complete the frames found there.
    /// Loss detection on a shell calls `on_packet_lost` for CCA but does not follow the
    /// chain — the probe is still in flight and may succeed independently.
    pub probed_to: Option<PacketNumber>,
}

impl Packet {
    pub fn new(frames: Queue<Frame>, info: TransmissionInfo, ack_frame_count: usize) -> Self {
        // ACK frames must come before non-ACK frames so that take_oldest_for_probe
        // can efficiently skip them using ack_frame_count.
        #[cfg(debug_assertions)]
        {
            let mut seen_non_ack = false;
            for frame in frames.iter() {
                if frame.is_immediate() {
                    debug_assert!(
                        !seen_non_ack,
                        "ACK frame appears after non-ACK frame — ACK frames must be at the front"
                    );
                } else {
                    seen_non_ack = true;
                }
            }
            // Verify the stored count matches the actual number of leading ACK frames.
            let actual = frames.iter().take_while(|f| f.is_immediate()).count();
            debug_assert_eq!(
                actual, ack_frame_count,
                "ack_frame_count ({ack_frame_count}) does not match leading ACK frame count ({actual})"
            );
        }
        Self {
            frames,
            ack_frame_count,
            transmission_info: Some(info),
            probed_to: None,
        }
    }
}

/// Tracks all packets currently in flight, keyed by packet number.
pub(crate) struct Map {
    inner: Inner<Packet>,
    inflight_gauge: QueueGauge,
}

impl Map {
    pub fn new(inflight_gauge: QueueGauge) -> Self {
        Self {
            inner: Default::default(),
            inflight_gauge,
        }
    }

    pub fn insert(&mut self, pn: PacketNumber, entry: Packet) {
        self.inflight_gauge.enqueue(1);
        self.inner.insert(pn, entry);
    }

    /// Remove a range of ACKed packet numbers.
    ///
    /// Returns an iterator of (PacketNumber, Packet) for further processing
    /// (completion notifications, CCA updates).
    pub fn remove_range(
        &mut self,
        range: PacketNumberRange,
    ) -> impl Iterator<Item = (VarInt, Packet)> + '_ {
        RemoveRange {
            inner: self.inner.remove_range(range),
            gauge: &self.inflight_gauge,
        }
    }

    pub fn has_inflight(&self) -> bool {
        self.inner.iter().next().is_some()
    }

    /// Return a mutable reference to the packet at `pn`, if present.
    pub fn get_mut(&mut self, pn: PacketNumber) -> Option<&mut Packet> {
        self.inner.get_mut(pn)
    }

    /// Find the oldest inflight packet number that has data (non-ACK) frames available for probing.
    ///
    /// Returns `None` if all inflight entries are shells or contain only ACK frames or if the
    /// map is empty.
    pub fn oldest_non_shell_pn(&self) -> Option<PacketNumber> {
        self.inner
            .iter()
            .find(|(_, p)| p.frames.len() > p.ack_frame_count)
            .map(|(pn, _)| pn)
    }

    /// Take the frames from the oldest non-shell inflight entry for a PTO probe.
    ///
    /// ACK (control/immediate) frames are stripped before returning — they are stale
    /// and must not be retransmitted. The entry remains in the map with an empty
    /// `frames` list and its `TransmissionInfo` intact. The caller must then call
    /// [`set_probed_to`] to finalise the shell pointer.
    ///
    /// [`set_probed_to`]: Self::set_probed_to
    pub fn take_oldest_for_probe(&mut self) -> Option<(PacketNumber, Queue<Frame>)> {
        let old_pn = self.oldest_non_shell_pn()?;
        let packet = self.inner.get_mut(old_pn)?;

        // Fast path: pop the known leading ACK frames using the stored count.
        for _ in 0..packet.ack_frame_count {
            let _stale = packet.frames.pop_front();
        }
        packet.ack_frame_count = 0;

        let frames = core::mem::take(&mut packet.frames);
        Some((old_pn, frames))
    }

    /// Set the `probed_to` forward pointer on an existing inflight entry.
    ///
    /// Called after a probe segment is successfully encoded: the `old_pn` entry
    /// becomes a shell pointing to `new_pn` (the probe's packet number).
    pub fn set_probed_to(&mut self, old_pn: PacketNumber, new_pn: PacketNumber) {
        if let Some(packet) = self.inner.get_mut(old_pn) {
            packet.probed_to = Some(new_pn);
        }
    }

    /// Follow the `probed_to` chain starting at `pn` and take the frames from the tail.
    ///
    /// Used in ACK processing when a shell is ACKed: the frames to complete live at
    /// the tail of the probe chain. The tail entry's `frames` are emptied but the
    /// entry itself remains in the map with its `TransmissionInfo` intact for later
    /// loss detection or ACK completion.
    ///
    /// Returns `(tail_pn, frames)`.
    pub fn take_chain_tail_frames(&mut self, mut pn: PacketNumber) -> (PacketNumber, Queue<Frame>) {
        // Walk the chain to the tail (first entry with no probed_to link).
        loop {
            match self.inner.get(pn).and_then(|p| p.probed_to) {
                Some(next_pn) => pn = next_pn,
                None => break,
            }
        }
        let frames = self
            .inner
            .get_mut(pn)
            .map(|p| core::mem::take(&mut p.frames))
            .unwrap_or_default();
        (pn, frames)
    }
}

pub(crate) struct RemoveRange<'a, I> {
    inner: I,
    gauge: &'a QueueGauge,
}

impl<'a, I> Iterator for RemoveRange<'a, I>
where
    I: Iterator<Item = (PacketNumber, Packet)>,
{
    type Item = (VarInt, Packet);

    fn next(&mut self) -> Option<Self::Item> {
        let (num, packet) = self.inner.next()?;
        self.gauge.dequeue();
        let num = unsafe { VarInt::new_unchecked(num.as_u64()) };
        Some((num, packet))
    }
}
