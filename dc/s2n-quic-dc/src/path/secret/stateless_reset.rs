// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{credentials::Id, packet::secret_control::TAG_LEN};
use aws_lc_rs::hmac;

#[derive(Debug)]
pub struct Signer {
    key: hmac::Key,
}

impl Signer {
    /// Creates a signer with the given secret
    pub fn new(secret: &[u8]) -> Self {
        let key = hmac::Key::new(hmac::HMAC_SHA384, secret);
        Self { key }
    }

    /// Returns a random `Signer`
    ///
    /// Note that this signer cannot be used across restarts and will result in an endpoint
    /// producing invalid `UnknownPathSecret` packets.
    pub fn random() -> Self {
        let mut secret = [0u8; 32];
        // In Bach simulation tests derive the key from the current group's ID so
        // that signers (and the stateless-reset tokens they produce) are stable
        // across runs and amenable to snapshot testing.
        #[cfg(any(test, feature = "testing"))]
        if bach::is_active() {
            let mut seed = bach::group::current()
                .id()
                .wrapping_mul(0x9E3779B97F4A7C15)
                | 1;
            for chunk in secret.chunks_mut(8) {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                chunk.copy_from_slice(&seed.to_ne_bytes()[..chunk.len()]);
            }
            return Self::new(&secret);
        }
        aws_lc_rs::rand::fill(&mut secret).unwrap();
        Self::new(&secret)
    }

    pub fn sign(&self, id: &Id) -> [u8; TAG_LEN] {
        let mut stateless_reset = [0; TAG_LEN];

        let tag = hmac::sign(&self.key, &**id);
        stateless_reset.copy_from_slice(&tag.as_ref()[..TAG_LEN]);

        stateless_reset
    }
}
