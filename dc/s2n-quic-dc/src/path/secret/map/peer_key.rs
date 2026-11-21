// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! The keys of the peers a [`Map`](super::Map) accepts.
//!
//! Once an application sets a list of keys with [`Map::set_peer_keys`](super::Map::set_peer_keys),
//! each handshake, on the client and on the server side, reads the key of the certificate the
//! peer presents and refuses the peer unless the list holds that key. Taking a key out of the list
//! evicts each cached path secret of a peer with that key, so its next handshake is refused too.

use std::sync::Arc;

#[cfg(test)]
mod tests;

const SEQUENCE: u8 = 0x30;
const INTEGER: u8 = 0x02;
/// The `[0] EXPLICIT` tag of a certificate's `version` field.
const VERSION: u8 = 0xa0;

/// A peer's public key: the DER encoding of the `SubjectPublicKeyInfo` of the leaf certificate the
/// peer presents in the handshake.
///
/// Two certificates of one key pair, such as a self-signed certificate that a device issues again,
/// have the same `PeerKey`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PeerKey(Arc<[u8]>);

impl PeerKey {
    /// Wraps the DER encoding of a `SubjectPublicKeyInfo`, as a pairing step records it.
    pub fn from_spki_der(der: impl Into<Arc<[u8]>>) -> Self {
        Self(der.into())
    }

    /// Reads the key of a DER encoded X.509 certificate. Returns `None` where the bytes aren't
    /// one DER encoded certificate.
    pub fn from_certificate_der(certificate: &[u8]) -> Option<Self> {
        // Certificate ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signatureValue }
        let (tag, certificate, rest) = element(certificate)?;
        if tag != SEQUENCE || !rest.is_empty() {
            return None;
        }

        // TBSCertificate ::= SEQUENCE { version [0] EXPLICIT DEFAULT v1, serialNumber,
        //     signature, issuer, validity, subject, subjectPublicKeyInfo, ... }
        let (tag, mut fields, _) = element(certificate)?;
        if tag != SEQUENCE {
            return None;
        }

        if fields.first() == Some(&VERSION) {
            fields = element(fields)?.2;
        }

        // The serial number, the signature algorithm, the issuer, the validity and the subject
        // come before the key.
        for expected in [INTEGER, SEQUENCE, SEQUENCE, SEQUENCE, SEQUENCE] {
            let (tag, _, rest) = element(fields)?;
            if tag != expected {
                return None;
            }
            fields = rest;
        }

        let (tag, _, rest) = element(fields)?;
        if tag != SEQUENCE {
            return None;
        }
        let spki = &fields[..fields.len() - rest.len()];

        Some(Self::from_spki_der(spki))
    }

    /// The DER encoding of the key's `SubjectPublicKeyInfo`.
    pub fn as_spki_der(&self) -> &[u8] {
        &self.0
    }
}

/// Reads one DER element from the start of `input`: its tag, its contents and the bytes after it.
///
/// It reads a length only in its DER form: a length below 128 in one byte, and a longer one in the
/// fewest bytes, at most 4.
fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, input) = input.split_first()?;
    let (&first, input) = input.split_first()?;

    let (len, input) = if first < 0x80 {
        (first as usize, input)
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 4 || input.len() < count {
            return None;
        }
        let (bytes, input) = input.split_at(count);
        if bytes[0] == 0 {
            return None;
        }
        let len = bytes
            .iter()
            .fold(0usize, |len, &byte| (len << 8) | byte as usize);
        if len < 0x80 {
            return None;
        }
        (len, input)
    };

    if input.len() < len {
        return None;
    }
    let (contents, rest) = input.split_at(len);
    Some((tag, contents, rest))
}
