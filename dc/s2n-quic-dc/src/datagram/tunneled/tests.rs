// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{
    recv::{self, Endpoint, SeenFilter},
    send, Receiver, Sender,
};
use crate::{
    credentials,
    crypto::{self, awslc, KeyPhase, UninitSlice},
    packet::WireVersion,
};
use core::ops::Range;
use s2n_quic_core::{packet::number::SlidingWindowError, varint::VarInt};
use std::collections::BTreeSet;

const KEY: [u8; 16] = [0x11; 16];
const IV: [u8; 12] = [0x22; 12];
const PORT: u16 = 4433;
const TAG_LEN: usize = 16;

// The byte offsets of a packet sealed under `credentials::testing::new(1, 0)`, with a packet
// number and a payload length below 64: the first byte, the 16-byte credential id, a one-byte key
// id, the wire version, the two-byte control port, a one-byte packet number and a one-byte payload
// length, then the payload.
const CREDENTIAL_ID: Range<usize> = 1..17;
const KEY_ID: usize = 17;
const WIRE_VERSION: usize = 18;
const CONTROL_PORT: Range<usize> = 19..21;
const PACKET_NUMBER: usize = 21;
const PAYLOAD_LEN: usize = 22;
const PAYLOAD: usize = 23;

const ROUTING_INFO_BIT: u8 = 0b1000;
const PACKET_NUMBER_BIT: u8 = 0b0100;
const APPLICATION_HEADER_BIT: u8 = 0b0010;
const KEY_PHASE_BIT: u8 = 0b0001;

fn seal_key() -> awslc::seal::Application {
    awslc::seal::Application::new(&KEY, IV, &awslc::AES_128_GCM)
}

fn open_key() -> awslc::open::Application {
    awslc::open::Application::new(&KEY, IV, &awslc::AES_128_GCM)
}

fn sender() -> Sender<awslc::seal::Application> {
    Sender::new(seal_key(), credentials::testing::new(1, 0))
}

fn phase_one_sender() -> Sender<PhaseOne> {
    Sender::new(PhaseOne(seal_key()), credentials::testing::new(1, 0))
}

fn receiver() -> Receiver<awslc::open::Application> {
    Receiver::new(open_key())
}

/// A sealing key that seals as the key of phase one.
struct PhaseOne(awslc::seal::Application);

impl crypto::seal::Application for PhaseOne {
    fn key_phase(&self) -> KeyPhase {
        KeyPhase::One
    }

    fn tag_len(&self) -> usize {
        crypto::seal::Application::tag_len(&self.0)
    }

    fn encrypt(
        &self,
        packet_number: u64,
        header: &[u8],
        extra_payload: Option<&[u8]>,
        payload_and_tag: &mut [u8],
    ) {
        crypto::seal::Application::encrypt(
            &self.0,
            packet_number,
            header,
            extra_payload,
            payload_and_tag,
        )
    }
}

/// Seals `payload` into a packet of the length the send wrote.
fn send<E: crypto::seal::Application>(sender: &Sender<E>, payload: &[u8]) -> Vec<u8> {
    let len = sender
        .estimated_send_size(payload.len())
        .expect("the payload fits a packet");
    let mut packet = vec![0; len];
    let len = sender
        .send_into(&PORT, payload, &mut packet)
        .expect("the send succeeds");
    packet.truncate(len);
    packet
}

fn parse(packet: &mut [u8]) -> recv::Packet<&mut [u8]> {
    let (packet, remaining) = Endpoint::default().parse(packet).expect("the packet parses");
    assert!(remaining.is_empty());
    packet
}

fn parses(packet: &mut [u8]) -> bool {
    Endpoint::default().parse(packet).is_some()
}

fn packet_number(packet: &mut [u8]) -> u64 {
    parse(packet).packet_number().as_u64()
}

fn decrypt(
    receiver: &mut Receiver<awslc::open::Application>,
    packet: &recv::Packet<&mut [u8]>,
) -> Result<Vec<u8>, recv::Error> {
    let mut payload = vec![0; packet.payload().len()];
    receiver.recv_into(packet, UninitSlice::new(&mut payload))?;
    Ok(payload)
}

fn open(
    receiver: &mut Receiver<awslc::open::Application>,
    packet: &mut [u8],
) -> Result<Vec<u8>, recv::Error> {
    decrypt(receiver, &parse(packet))
}

fn open_once(
    receiver: &mut Receiver<awslc::open::Application>,
    seen: &mut SeenFilter,
    packet: &mut [u8],
) -> Result<Vec<u8>, recv::Error> {
    let packet = parse(packet);
    let mut payload = vec![0; packet.payload().len()];
    receiver.recv_into_checked(seen, &packet, UninitSlice::new(&mut payload))?;
    Ok(payload)
}

/// Opens a payload from its parts with the receiver's key and the given packet number.
fn open_parts(
    header: &[u8],
    payload: &[u8],
    tag: &[u8],
    packet_number: u64,
) -> Result<Vec<u8>, recv::Error> {
    let mut out = vec![0; payload.len()];
    crypto::open::Application::decrypt(
        &open_key(),
        KeyPhase::Zero,
        packet_number,
        header,
        payload,
        tag,
        UninitSlice::new(&mut out),
    )?;
    Ok(out)
}

//= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-001
//= type=test
//# A sender MUST give each datagram it seals under a key a packet number that no earlier datagram
//# under that key had, whichever of its callers sends it, and whether or not callers send at the
//# same time.
#[test]
fn concurrent_sends_take_distinct_packet_numbers() {
    const THREADS: u64 = 4;
    const SENDS: u64 = 64;

    let sender = sender();
    let numbers: Vec<u64> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                scope.spawn(|| {
                    (0..SENDS)
                        .map(|_| packet_number(&mut send(&sender, b"payload")))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("the sending thread succeeds"))
            .collect()
    });

    let distinct: BTreeSet<u64> = numbers.iter().copied().collect();
    assert_eq!(distinct.len(), numbers.len());
    assert_eq!(distinct, (0..THREADS * SENDS).collect::<BTreeSet<_>>());
}

//= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-002
//= type=test
//# A sender MUST take zero as the packet number of its first send, and one more than the number
//# of the send before it as the number of each later send, whether or not the send before it
//# failed.
#[test]
fn packet_numbers_start_at_zero_and_count_failed_sends() {
    let sender = sender();
    assert_eq!(packet_number(&mut send(&sender, b"first")), 0);
    assert_eq!(packet_number(&mut send(&sender, b"second")), 1);

    // This send fails for its short buffer, and takes packet number 2 all the same.
    let mut short = [0u8; 4];
    assert!(matches!(
        sender.send_into(&PORT, b"third", &mut short),
        Err(send::Error::PacketBufferTooSmall)
    ));

    assert_eq!(packet_number(&mut send(&sender, b"fourth")), 3);
}

//= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-003
//= type=test
//# A sender MUST NOT seal a datagram whose packet number is larger than a 62-bit variable-length
//# integer holds, and fails that send instead.
#[test]
fn a_packet_number_past_the_varint_range_fails_the_send() {
    let sender = sender();
    let last = VarInt::MAX.as_u64();
    sender.set_next_packet_number(last);
    assert_eq!(packet_number(&mut send(&sender, b"last")), last);

    let len = sender
        .estimated_send_size(4)
        .expect("the payload fits a packet");
    let mut packet = vec![0xAA; len];
    assert!(matches!(
        sender.send_into(&PORT, b"next", &mut packet),
        Err(send::Error::PacketNumberExhaustion)
    ));
    assert!(packet.iter().all(|byte| *byte == 0xAA));
}

//= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-004
//= type=test
//# A sender MUST fail a send whose payload is longer than the packet's payload length field can
//# state.
//= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-006
//= type=test
//# A sender MUST answer a caller, before any send, the most bytes the packet of a payload of a
//# given length can take, or that a payload of that length is too long to send.
#[test]
#[cfg(target_pointer_width = "64")]
fn a_payload_past_the_length_field_is_too_long() {
    // No test can hold a payload of 2^62 bytes, so this checks the length check that the send
    // and the estimate both make.
    let longest = VarInt::MAX.as_u64() as usize;
    assert!(send::payload_len(longest).is_ok());
    assert!(matches!(
        send::payload_len(longest + 1),
        Err(send::Error::PayloadTooLarge)
    ));
    assert_eq!(sender().estimated_send_size(longest + 1), None);
    assert_eq!(sender().estimated_send_size(usize::MAX), None);
}

//= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-005
//= type=test
//# A sender MUST fail a send, without writing to the caller's buffer, where that buffer is shorter
//# than the packet the send would write.
#[test]
fn a_short_buffer_fails_the_send_without_a_write() {
    let sender = sender();
    let payload = b"payload";
    let len = sender
        .estimated_send_size(payload.len())
        .expect("the payload fits a packet");

    for short in [0, 1, PAYLOAD, len - 1] {
        let mut packet = vec![0xAA; short];
        assert!(matches!(
            sender.send_into(&PORT, payload, &mut packet),
            Err(send::Error::PacketBufferTooSmall)
        ));
        assert!(packet.iter().all(|byte| *byte == 0xAA));
    }
}

//= rendered/0009-tunneled-datagrams/01-sending.md#DC-0009-006
//= type=test
//# A sender MUST answer a caller, before any send, the most bytes the packet of a payload of a
//# given length can take, or that a payload of that length is too long to send.
#[test]
fn the_estimate_bounds_each_packet() {
    let sender = sender();
    for packet_number in [0, 63, 64, 16_383, 16_384, VarInt::MAX.as_u64()] {
        for payload_len in [0usize, 1, 63, 64, 1200] {
            sender.set_next_packet_number(packet_number);
            let payload = vec![0x5A; payload_len];
            let estimate = sender
                .estimated_send_size(payload_len)
                .expect("the payload fits a packet");
            let mut packet = vec![0; estimate];
            let len = sender
                .send_into(&PORT, &payload, &mut packet)
                .expect("the send succeeds");
            assert!(
                len <= estimate,
                "packet number {packet_number}, payload {payload_len}: {len} > {estimate}"
            );
        }
    }
}

//= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-007
//= type=test
//# The first byte of a tunneled datagram packet MUST hold 0100 in its high four bits, and in its
//# low four bits, from the highest, the routing information bit, the packet number bit, the
//# application header bit and the key phase bit.
//= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-008
//= type=test
//# A tunneled datagram packet MUST NOT carry routing information, and its routing information bit
//# is clear.
//= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-009
//= type=test
//# A tunneled datagram packet MUST carry its packet number, and its packet number bit is set.
//= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-010
//= type=test
//# A tunneled datagram packet MUST NOT carry an application header, and its application header
//# bit is clear.
#[test]
fn the_first_byte_holds_the_type_and_its_bits() {
    let mut packet = send(&sender(), b"payload");
    assert_eq!(packet[0] >> 4, 0b0100);
    assert_eq!(packet[0] & ROUTING_INFO_BIT, 0);
    assert_eq!(packet[0] & PACKET_NUMBER_BIT, PACKET_NUMBER_BIT);
    assert_eq!(packet[0] & APPLICATION_HEADER_BIT, 0);
    assert_eq!(packet[0] & KEY_PHASE_BIT, 0);
    assert_eq!(packet[0], 0b0100_0100);

    let parsed = parse(&mut packet);
    assert!(!parsed.tag().has_routing_info());
    assert!(parsed.tag().has_packet_number());
    assert!(!parsed.tag().payload_encrypted());
    assert!(parsed.application_header().is_empty());

    let packet = send(&phase_one_sender(), b"payload");
    assert_eq!(packet[0], 0b0100_0101);
}

//= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-011
//= type=test
//# A tunneled datagram packet MUST carry, after its first byte and in this order, the credentials
//# of the key that sealed it, the wire version, the sender's control port, the packet number, the
//# payload's length, the payload and the authentication tag.
//= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-012
//= type=test
//# A sender MUST write wire version zero in each tunneled datagram packet it sends.
//= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-013
//= type=test
//# A sender MUST write as the packet's control port the source port of the control path its caller
//# passes to the send.
#[test]
fn the_fields_follow_the_first_byte_in_order() {
    let credentials = credentials::testing::new(1, 0);
    let payload = b"payload";
    let mut packet = send(&sender(), payload);

    assert_eq!(&packet[CREDENTIAL_ID], credentials.id.as_slice());
    assert_eq!(packet[KEY_ID], 0);
    assert_eq!(packet[WIRE_VERSION], 0);
    assert_eq!(&packet[CONTROL_PORT], &PORT.to_be_bytes()[..]);
    assert_eq!(packet[PACKET_NUMBER], 0);
    assert_eq!(packet[PAYLOAD_LEN] as usize, payload.len());
    assert_eq!(packet.len(), PAYLOAD + payload.len() + TAG_LEN);

    let parsed = parse(&mut packet);
    assert_eq!(parsed.credentials(), &credentials);
    assert_eq!(parsed.wire_version(), WireVersion::ZERO);
    assert_eq!(parsed.source_control_port(), PORT);
    assert_eq!(parsed.packet_number(), VarInt::ZERO);
    assert_eq!(parsed.header().len(), PAYLOAD);
    assert_eq!(parsed.payload().len(), payload.len());
    assert_eq!(parsed.auth_tag().len(), TAG_LEN);
}

//= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-014
//= type=test
//# A sender MUST encrypt the whole payload of each packet.
#[test]
fn the_whole_payload_is_encrypted() {
    let payload = [0u8; 48];
    let mut packet = send(&sender(), &payload);

    // Each 16-byte block of the sealed payload differs from the cleartext.
    for block in packet[PAYLOAD..PAYLOAD + payload.len()].chunks(16) {
        assert_ne!(block, &[0u8; 16][..]);
    }

    assert_eq!(open(&mut receiver(), &mut packet).unwrap(), payload);
}

//= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-028
//= type=test
//# A sender MUST authenticate the whole header of each packet together with its payload.
#[test]
fn the_tag_covers_every_header_byte() {
    let payload = b"payload";
    let mut packet = send(&sender(), payload);
    let parsed = parse(&mut packet);
    let header = parsed.header().to_vec();
    assert_eq!(header.len(), PAYLOAD);

    assert_eq!(
        open_parts(&header, parsed.payload(), parsed.auth_tag(), 0).unwrap(),
        payload
    );

    for index in 0..header.len() {
        let mut changed = header.clone();
        changed[index] ^= 0x01;
        assert_eq!(
            open_parts(&changed, parsed.payload(), parsed.auth_tag(), 0),
            Err(recv::Error::InvalidTag),
            "header byte {index}"
        );
    }
}

//= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-029
//= type=test
//# A sender MUST use the packet's packet number as the nonce of its encryption.
#[test]
fn the_packet_number_is_the_nonce() {
    let sender = sender();
    sender.set_next_packet_number(7);
    let mut packet = send(&sender, b"payload");
    let parsed = parse(&mut packet);
    assert_eq!(parsed.packet_number().as_u64(), 7);

    assert_eq!(
        open_parts(parsed.header(), parsed.payload(), parsed.auth_tag(), 7).unwrap(),
        b"payload"
    );
    for nonce in [0, 6, 8] {
        assert_eq!(
            open_parts(parsed.header(), parsed.payload(), parsed.auth_tag(), nonce),
            Err(recv::Error::InvalidTag),
            "nonce {nonce}"
        );
    }
}

//= rendered/0009-tunneled-datagrams/02-packet.md#DC-0009-030
//= type=test
//# A sender MUST write the phase of the key that seals a packet in the packet's key phase bit.
#[test]
fn the_key_phase_bit_holds_the_sealing_keys_phase() {
    let mut packet = send(&sender(), b"payload");
    assert_eq!(packet[0] & KEY_PHASE_BIT, 0);
    assert_eq!(parse(&mut packet).tag().key_phase(), KeyPhase::Zero);

    let mut packet = send(&phase_one_sender(), b"payload");
    assert_eq!(packet[0] & KEY_PHASE_BIT, KEY_PHASE_BIT);
    assert_eq!(parse(&mut packet).tag().key_phase(), KeyPhase::One);
}

//= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-015
//= type=test
//# A receiver MUST reject a packet whose first byte doesn't hold 0100 in its high four bits.
#[test]
fn a_first_byte_without_the_datagram_type_is_rejected() {
    let packet = send(&sender(), b"payload");
    assert!(parses(&mut packet.clone()));

    for high in [0b0000u8, 0b0011, 0b0101, 0b1000, 0b1100] {
        let mut changed = packet.clone();
        changed[0] = (high << 4) | (packet[0] & 0x0F);
        assert!(!parses(&mut changed), "first byte {:#04x}", changed[0]);
    }
}

//= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-016
//= type=test
//# A receiver MUST reject a packet whose routing information bit is set.
#[test]
fn a_set_routing_information_bit_is_rejected() {
    let mut packet = send(&sender(), b"payload");
    packet[0] |= ROUTING_INFO_BIT;
    assert!(!parses(&mut packet));
}

//= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-017
//= type=test
//# A receiver MUST reject a packet whose application header bit is set.
#[test]
fn a_set_application_header_bit_is_rejected() {
    let mut packet = send(&sender(), b"payload");
    packet[0] |= APPLICATION_HEADER_BIT;
    assert!(!parses(&mut packet));
}

//= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-032
//= type=test
//# A receiver MUST reject a packet whose packet number bit is clear.
#[test]
fn a_clear_packet_number_bit_is_rejected() {
    let mut packet = send(&sender(), b"payload");
    packet[0] &= !PACKET_NUMBER_BIT;
    assert!(!parses(&mut packet));
}

//= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-031
//= type=test
//# A receiver MUST reject a packet whose wire version isn't zero.
#[test]
fn a_wire_version_other_than_zero_is_rejected() {
    let packet = send(&sender(), b"payload");
    for version in [1u8, 2, 0x80, 0xFF] {
        let mut changed = packet.clone();
        changed[WIRE_VERSION] = version;
        assert!(!parses(&mut changed), "wire version {version}");
    }
}

//= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-018
//= type=test
//# A receiver MUST read the last 16 bytes of each packet as its authentication tag.
#[test]
fn the_last_16_bytes_are_the_tag() {
    let mut packet = send(&sender(), b"payload");
    let tag = packet[packet.len() - TAG_LEN..].to_vec();
    assert_eq!(parse(&mut packet).auth_tag(), &tag[..]);
}

//= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-019
//= type=test
//# A receiver MUST NOT hand a payload to the application unless the packet's authentication tag
//# verifies over its header and its payload, under the receiver's key and with its packet number
//# as the nonce.
#[test]
fn a_payload_is_withheld_unless_its_tag_verifies() {
    let packet = send(&sender(), b"payload");
    let mut receiver = receiver();
    assert_eq!(
        open(&mut receiver, &mut packet.clone()).unwrap(),
        b"payload"
    );

    // A change to any byte of the payload or of the tag fails the tag.
    for index in PAYLOAD..packet.len() {
        let mut changed = packet.clone();
        changed[index] ^= 0x01;
        assert_eq!(
            open(&mut receiver, &mut changed),
            Err(recv::Error::InvalidTag),
            "byte {index}"
        );
    }

    // A change to the control port, a header byte, fails the tag.
    let mut changed = packet.clone();
    changed[CONTROL_PORT.start] ^= 0x01;
    assert_eq!(
        open(&mut receiver, &mut changed),
        Err(recv::Error::InvalidTag)
    );

    // Another key fails the tag.
    let mut other = Receiver::new(awslc::open::Application::new(
        &[0x33; 16],
        IV,
        &awslc::AES_128_GCM,
    ));
    assert_eq!(
        open(&mut other, &mut packet.clone()),
        Err(recv::Error::InvalidTag)
    );
}

//= rendered/0009-tunneled-datagrams/03-receiving.md#DC-0009-020
//= type=test
//# A datagram MAY carry more than one tunneled datagram packet, each starting at the byte after
//# the end of the one before it.
#[test]
fn a_datagram_carries_packets_back_to_back() {
    let sender = sender();
    let first = send(&sender, b"first");
    let second = send(&sender, b"second");
    let mut datagram = [first, second.clone()].concat();
    let mut receiver = receiver();
    let endpoint = Endpoint::default();

    let (packet, remaining) = endpoint
        .parse(&mut datagram)
        .expect("the first packet parses");
    assert_eq!(remaining.len(), second.len());
    assert_eq!(decrypt(&mut receiver, &packet).unwrap(), b"first");

    let (packet, remaining) = endpoint.parse(remaining).expect("the second packet parses");
    assert!(remaining.is_empty());
    assert_eq!(decrypt(&mut receiver, &packet).unwrap(), b"second");
}

//= rendered/0009-tunneled-datagrams/04-replay.md#DC-0009-021
//= type=test
//# A receiver MUST, once a packet's authentication tag verifies, check its packet number against
//# its replay window and record it there in one step, and reject the packet where that step finds
//# the number accepted before.
#[test]
fn a_packet_number_is_accepted_once_and_only_after_its_tag_verifies() {
    let packet = send(&sender(), b"payload");
    let mut receiver = receiver();
    let mut seen = SeenFilter::default();

    // A forgery of the same packet number fails its tag, and the window doesn't record it.
    let mut forged = packet.clone();
    let last = forged.len() - 1;
    forged[last] ^= 0x01;
    assert_eq!(
        open_once(&mut receiver, &mut seen, &mut forged),
        Err(recv::Error::InvalidTag)
    );

    assert_eq!(
        open_once(&mut receiver, &mut seen, &mut packet.clone()).unwrap(),
        b"payload"
    );
    assert_eq!(
        open_once(&mut receiver, &mut seen, &mut packet.clone()),
        Err(recv::Error::ReplayDefinitelyDetected)
    );
}

//= rendered/0009-tunneled-datagrams/04-replay.md#DC-0009-022
//= type=test
//# A receiver MUST reject a packet whose packet number is older than the oldest number its replay
//# window holds.
#[test]
fn a_packet_number_older_than_the_window_is_rejected() {
    let sender = sender();
    let mut old = send(&sender, b"old");
    sender.set_next_packet_number(1_000);
    let mut new = send(&sender, b"new");
    let mut receiver = receiver();
    let mut seen = SeenFilter::default();

    assert_eq!(
        open_once(&mut receiver, &mut seen, &mut new).unwrap(),
        b"new"
    );
    assert_eq!(
        open_once(&mut receiver, &mut seen, &mut old),
        Err(recv::Error::ReplayPotentiallyDetected { gap: None })
    );
}

//= rendered/0009-tunneled-datagrams/04-replay.md#DC-0009-023
//= type=test
//# A receiver MUST accept, in its replay check, a packet whose packet number is newer than every
//# number it accepted before.
#[test]
fn a_packet_number_newer_than_every_accepted_number_is_accepted() {
    let sender = sender();
    let mut receiver = receiver();
    let mut seen = SeenFilter::default();

    for packet_number in [0, 1, 5, 129, 130, 10_000] {
        sender.set_next_packet_number(packet_number);
        let mut packet = send(&sender, b"payload");
        assert_eq!(
            open_once(&mut receiver, &mut seen, &mut packet).unwrap(),
            b"payload",
            "packet number {packet_number}"
        );
    }
}

//= rendered/0009-tunneled-datagrams/04-replay.md#DC-0009-024
//= type=test
//# A receiver MUST keep one replay window for each sender's packet numbers, holding the numbers
//# accepted under every key phase it can still open.
#[test]
fn one_window_holds_the_numbers_of_every_key_phase() {
    let phase_zero = send(&sender(), b"zero");
    let phase_one = send(&phase_one_sender(), b"one");

    let mut seen = SeenFilter::default();
    assert_eq!(seen.on_packet(&parse(&mut phase_zero.clone())), Ok(()));
    assert_eq!(
        seen.on_packet(&parse(&mut phase_one.clone())),
        Err(SlidingWindowError::Duplicate)
    );

    let mut seen = SeenFilter::default();
    assert_eq!(seen.on_packet(&parse(&mut phase_one.clone())), Ok(()));
    assert_eq!(
        seen.on_packet(&parse(&mut phase_zero.clone())),
        Err(SlidingWindowError::Duplicate)
    );
}

//= rendered/0009-tunneled-datagrams/05-delivery.md#DC-0009-026
//= type=test
//# A receiver MAY hand payloads to the application in another order than the order in which they
//# were sent.
#[test]
fn payloads_may_arrive_out_of_order() {
    let sender = sender();
    let mut first = send(&sender, b"first");
    let mut second = send(&sender, b"second");
    let mut receiver = receiver();
    let mut seen = SeenFilter::default();

    assert_eq!(
        open_once(&mut receiver, &mut seen, &mut second).unwrap(),
        b"second"
    );
    assert_eq!(
        open_once(&mut receiver, &mut seen, &mut first).unwrap(),
        b"first"
    );
}

//= rendered/0009-tunneled-datagrams/05-delivery.md#DC-0009-027
//= type=test
//# A receiver MUST hand the application each payload it accepts exactly as the sender's caller
//# gave it.
#[test]
fn the_payload_arrives_as_sent() {
    let sender = sender();
    let mut receiver = receiver();

    for len in [0usize, 1, 15, 16, 17, 63, 64, 1200] {
        let payload: Vec<u8> = (0..len).map(|index| index as u8).collect();
        let mut packet = send(&sender, &payload);
        assert_eq!(
            open(&mut receiver, &mut packet).unwrap(),
            payload,
            "length {len}"
        );
    }
}
