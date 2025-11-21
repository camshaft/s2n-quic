// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use s2n_quic_core::{
    ensure,
    state::{event, is},
    varint::VarInt,
};
use tracing::trace;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Unknown,
    Known,
    Sent,
    Acked,
}

impl State {
    is!(is_unknown, Unknown);
    is!(is_known, Known);
    is!(is_sent, Sent);
    is!(is_acked, Acked);

    event! {
        on_known(Unknown => Known);
        on_send_fin(Known => Sent);
        on_fin_ack(Sent => Acked);
    }
}

#[derive(Clone, Debug, Default)]
pub struct Fin {
    state: State,
    value: Option<VarInt>,
}

impl Fin {
    pub fn on_known(&mut self, value: VarInt) -> Result<(), ()> {
        if self.state.on_known().is_ok() {
            trace!(%value, "fin known");
            self.value = Some(value);
            Ok(())
        } else {
            debug_assert_eq!(self.value, Some(value));
            Err(())
        }
    }

    pub fn is_queued(&self) -> bool {
        self.state.is_known()
    }

    pub fn value(&self) -> Option<VarInt> {
        self.value
    }

    pub fn try_transmit(&mut self) -> Option<VarInt> {
        let value = self.value()?;
        let _ = self.state.on_send_fin();
        Some(value)
    }

    pub fn on_transmit(&mut self) {
        debug_assert!(!self.state.is_unknown());
        ensure!(self.state.on_send_fin().is_ok());
        trace!("fin sent");
    }

    pub fn on_ack(&mut self) {
        debug_assert!(!self.state.is_unknown());
        ensure!(self.state.on_fin_ack().is_ok());
        trace!("fin acked")
    }

    pub fn is_acked(&self) -> bool {
        self.state.is_acked()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_initialization() {
        let fin = Fin::default();

        assert_eq!(fin.state, State::Unknown);
        assert_eq!(fin.value(), None);
        assert!(!fin.is_acked());
    }

    #[test]
    fn on_known_transitions_from_unknown() {
        let mut fin = Fin::default();
        let offset = VarInt::from_u32(1000);

        let result = fin.on_known(offset);

        assert!(result.is_ok());
        assert_eq!(fin.state, State::Known);
        assert_eq!(fin.value(), Some(offset));
    }

    #[test]
    fn on_known_returns_error_when_already_known() {
        let mut fin = Fin::default();
        let offset = VarInt::from_u32(1000);

        fin.on_known(offset).unwrap();
        let result = fin.on_known(offset);

        assert!(result.is_err());
        assert_eq!(fin.state, State::Known);
        assert_eq!(fin.value(), Some(offset));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "assertion `left == right` failed")]
    fn on_known_with_different_value_panics_in_debug() {
        let mut fin = Fin::default();
        let first_offset = VarInt::from_u32(1000);
        let second_offset = VarInt::from_u32(2000);

        fin.on_known(first_offset).unwrap();
        let _ = fin.on_known(second_offset); // Panics in debug due to debug_assert_eq!
    }

    #[test]
    fn on_known_with_same_value_returns_error() {
        let mut fin = Fin::default();
        let offset = VarInt::from_u32(1000);

        fin.on_known(offset).unwrap();
        let result = fin.on_known(offset);

        assert!(result.is_err());
        assert_eq!(fin.value(), Some(offset));
    }

    #[test]
    fn value_returns_none_before_known() {
        let fin = Fin::default();
        assert_eq!(fin.value(), None);
    }

    #[test]
    fn value_returns_some_after_known() {
        let mut fin = Fin::default();
        let offset = VarInt::from_u32(5000);

        fin.on_known(offset).unwrap();

        assert_eq!(fin.value(), Some(offset));
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "assertion failed: !self.state.is_unknown()")]
    fn on_transmit_from_unknown_state_panics_in_debug() {
        let mut fin = Fin::default();

        fin.on_transmit(); // Panics in debug mode
    }

    #[test]
    fn on_transmit_from_known_state() {
        let mut fin = Fin::default();
        let offset = VarInt::from_u32(1000);

        fin.on_known(offset).unwrap();
        fin.on_transmit();

        assert_eq!(fin.state, State::Sent);
        assert_eq!(fin.value(), Some(offset));
    }

    #[test]
    fn on_transmit_from_sent_state_is_noop() {
        let mut fin = Fin::default();
        fin.on_known(VarInt::from_u32(1000)).unwrap();

        fin.on_transmit();
        assert_eq!(fin.state, State::Sent);

        fin.on_transmit(); // Should be a no-op
        assert_eq!(fin.state, State::Sent);
    }

    #[test]
    fn on_transmit_from_acked_state_is_noop() {
        let mut fin = Fin::default();
        fin.on_known(VarInt::from_u32(1000)).unwrap();

        fin.on_transmit();
        fin.on_ack();
        assert_eq!(fin.state, State::Acked);

        fin.on_transmit(); // Should be a no-op
        assert_eq!(fin.state, State::Acked);
    }

    #[test]
    fn on_ack_from_sent_state() {
        let mut fin = Fin::default();
        fin.on_known(VarInt::from_u32(1000)).unwrap();

        fin.on_transmit();
        fin.on_ack();

        assert_eq!(fin.state, State::Acked);
        assert!(fin.is_acked());
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "assertion failed: !self.state.is_unknown()")]
    fn on_ack_from_unknown_state_panics_in_debug() {
        let mut fin = Fin::default();

        fin.on_ack(); // Panics in debug mode
    }

    #[test]
    fn on_ack_from_known_state_is_noop() {
        let mut fin = Fin::default();
        let offset = VarInt::from_u32(1000);

        fin.on_known(offset).unwrap();
        fin.on_ack(); // Should be a no-op - Known is not Sent

        assert_eq!(fin.state, State::Known);
        assert!(!fin.is_acked());
    }

    #[test]
    fn on_ack_from_acked_state_is_noop() {
        let mut fin = Fin::default();
        fin.on_known(VarInt::from_u32(1000)).unwrap();

        fin.on_transmit();
        fin.on_ack();
        assert_eq!(fin.state, State::Acked);

        fin.on_ack(); // Should be a no-op
        assert_eq!(fin.state, State::Acked);
        assert!(fin.is_acked());
    }

    #[test]
    fn is_acked_returns_false_for_unknown() {
        let fin = Fin::default();
        assert!(!fin.is_acked());
    }

    #[test]
    fn is_acked_returns_false_for_known() {
        let mut fin = Fin::default();
        fin.on_known(VarInt::from_u32(1000)).unwrap();
        assert!(!fin.is_acked());
    }

    #[test]
    fn is_acked_returns_false_for_sent() {
        let mut fin = Fin::default();
        fin.on_known(VarInt::from_u32(1000)).unwrap();
        fin.on_transmit();
        assert!(!fin.is_acked());
    }

    #[test]
    fn is_acked_returns_true_for_acked() {
        let mut fin = Fin::default();
        fin.on_known(VarInt::from_u32(1000)).unwrap();
        fin.on_transmit();
        fin.on_ack();
        assert!(fin.is_acked());
    }

    #[test]
    fn full_state_transition_with_known() {
        let mut fin = Fin::default();
        let offset = VarInt::from_u32(8192);

        assert_eq!(fin.state, State::Unknown);

        fin.on_known(offset).unwrap();
        assert_eq!(fin.state, State::Known);
        assert_eq!(fin.value(), Some(offset));

        fin.on_transmit();
        assert_eq!(fin.state, State::Sent);
        assert_eq!(fin.value(), Some(offset));

        fin.on_ack();
        assert_eq!(fin.state, State::Acked);
        assert_eq!(fin.value(), Some(offset));
        assert!(fin.is_acked());
    }

    #[test]
    fn varint_zero_offset() {
        let mut fin = Fin::default();

        fin.on_known(VarInt::ZERO).unwrap();

        assert_eq!(fin.value(), Some(VarInt::ZERO));
        assert_eq!(fin.state, State::Known);
    }

    #[test]
    fn varint_max_offset() {
        let mut fin = Fin::default();

        fin.on_known(VarInt::MAX).unwrap();

        assert_eq!(fin.value(), Some(VarInt::MAX));
        assert_eq!(fin.state, State::Known);
    }

    #[test]
    fn clone_preserves_state() {
        let mut fin = Fin::default();
        let offset = VarInt::from_u32(4096);

        fin.on_known(offset).unwrap();
        fin.on_transmit();

        let cloned = fin.clone();

        assert_eq!(cloned.state, State::Sent);
        assert_eq!(cloned.value(), Some(offset));
        assert!(!cloned.is_acked());
    }

    #[test]
    fn multiple_transmissions_preserve_value() {
        let mut fin = Fin::default();
        let offset = VarInt::from_u32(2048);

        fin.on_known(offset).unwrap();
        fin.on_transmit();

        assert_eq!(fin.value(), Some(offset));
        assert_eq!(fin.state, State::Sent);
    }

    #[test]
    fn ack_preserves_value() {
        let mut fin = Fin::default();
        let offset = VarInt::from_u32(16384);

        fin.on_known(offset).unwrap();
        fin.on_transmit();
        fin.on_ack();

        assert_eq!(fin.value(), Some(offset));
        assert!(fin.is_acked());
    }
}
