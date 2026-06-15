// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Fast non-cryptographic PRNG for load balancing decisions (pick-two, etc).

use std::hash::Hasher;

#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new() -> Self {
        #[cfg(test)]
        if bolero::is_active() {
            use bach::rand::any;
            return Self(any::<u64>() | 1);
        }

        #[cfg(any(test, feature = "testing"))]
        if bach::is_active() {
            let seed = bach::group::current()
                .id()
                .wrapping_mul(0x9E37_79B9_7F4A_7C15);
            return Self(seed | 1);
        }

        let seed =
            std::hash::BuildHasher::build_hasher(&std::collections::hash_map::RandomState::new())
                .finish();
        Self(seed | 1)
    }

    /// Creates a new `Rng` from a fixed seed, bypassing all environment-based
    /// seed selection.  Use this when you need a fully deterministic sequence
    /// that is independent of the Bach group or OS randomness (e.g. in
    /// snapshot-locked tests).
    pub fn with_seed(seed: u64) -> Self {
        Self(seed | 1)
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        #[cfg(test)]
        if bolero::is_active() {
            use bach::rand::any;
            return any();
        }

        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    #[inline]
    pub fn next_usize(&mut self, bound: usize) -> usize {
        debug_assert!(bound > 0);

        #[cfg(test)]
        if bolero::is_active() {
            use bach::rand::Any;
            return (..bound).any();
        }

        self.next_u64() as usize % bound
    }

    /// Returns a uniformly distributed `f64` in the half-open range `[0, 1)`.
    ///
    /// Uses the top 53 bits of `next_u64` to fill the mantissa, so it inherits the same
    /// deterministic behavior under `bolero`/`bach` as the other generators.
    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        // 53 bits of randomness scaled to [0, 1): the largest precision an f64 mantissa holds.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

impl Default for Rng {
    fn default() -> Self {
        Self::new()
    }
}

impl s2n_quic_core::random::Generator for Rng {
    fn public_random_fill(&mut self, dest: &mut [u8]) {
        let mut offset = 0;
        while offset < dest.len() {
            let bytes = self.next_u64().to_ne_bytes();
            let remaining = dest.len() - offset;
            let to_copy = remaining.min(8);
            dest[offset..offset + to_copy].copy_from_slice(&bytes[..to_copy]);
            offset += to_copy;
        }
    }

    fn private_random_fill(&mut self, _dest: &mut [u8]) {
        panic!("xorshift::Rng must not be used for private/cryptographic randomness");
    }
}
