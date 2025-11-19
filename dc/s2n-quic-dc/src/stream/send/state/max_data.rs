use core::time::Duration;
use s2n_quic_core::{
    ensure, ready,
    state::{event, is},
    time::{timer, Clock, Timer},
    varint::VarInt,
};
use std::task::Poll;
use tracing::debug;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum State {
    #[default]
    Unblocked,
    Blocked,
}

impl State {
    is!(is_unblocked, Unblocked);
    is!(is_blocked, Blocked);

    event! {
        on_blocked(Unblocked => Blocked);
        on_unblocked(Blocked => Unblocked);
    }
}

#[derive(Debug, Clone)]
pub struct MaxData {
    local_value: VarInt,
    peer_value: VarInt,
    timer: Timer,
    state: State,
    /// Indicates if the peer has updated our window since the stream started
    has_received_update: bool,
}

impl MaxData {
    pub fn new(initial_max_data: VarInt) -> Self {
        Self {
            local_value: VarInt::ZERO,
            peer_value: initial_max_data,
            timer: Timer::default(),
            state: State::default(),
            has_received_update: false,
        }
    }

    pub fn on_transmit(
        &mut self,
        offset: VarInt,
        clock: &impl Clock,
        idle_timeout: Duration,
    ) -> bool {
        self.local_value = self.local_value.max(offset);
        ensure!(self.peer_value == self.local_value, false);
        ensure!(self.state.on_blocked().is_ok(), false);
        debug!("flow blocked");
        self.timer.set(clock.get_time() + idle_timeout / 2);
        !self.has_received_update
    }

    pub fn max_sent_offset(&self) -> VarInt {
        self.local_value
    }

    pub fn max_data(&self) -> VarInt {
        self.peer_value
    }

    pub fn on_control_packet(&mut self) {
        self.has_received_update = true;
    }

    pub fn on_max_data_frame(&mut self, max_data: VarInt) -> Option<VarInt> {
        ensure!(self.peer_value < max_data, None);
        let diff = max_data - self.peer_value;
        self.peer_value = max_data;
        self.on_unblocked();
        Some(diff)
    }

    fn on_unblocked(&mut self) {
        ensure!(self.state.on_unblocked().is_ok());
        debug!("flow unblocked");
        self.timer.cancel();
    }

    pub fn on_timeout(&mut self, clock: &impl Clock, idle_timeout: Duration) -> Poll<()> {
        let now = clock.get_time();

        // make sure we've armed the timer if we're blocked
        if self.state.is_blocked() && !self.timer.is_armed() {
            self.timer.set(now + idle_timeout / 2);
        }

        ready!(self.timer.poll_expiration(now));
        self.timer.set(now + idle_timeout / 2);
        Poll::Ready(())
    }
}

impl timer::Provider for MaxData {
    fn timers<Q: timer::Query>(&self, query: &mut Q) -> timer::Result {
        self.timer.timers(query)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;
    use s2n_quic_core::time::{clock::testing as test_clock, timer::Provider};
    use std::task::Poll;

    #[test]
    fn new_initializes_correctly() {
        let initial_max_data = VarInt::from_u32(1000);
        let max_data = MaxData::new(initial_max_data);

        assert_eq!(max_data.max_sent_offset(), VarInt::ZERO);
        assert_eq!(max_data.max_data(), initial_max_data);
        assert!(max_data.state.is_unblocked());
    }

    #[test]
    fn max_sent_offset_returns_local_value() {
        let max_data = MaxData::new(VarInt::from_u32(1000));
        assert_eq!(max_data.max_sent_offset(), VarInt::ZERO);
    }

    #[test]
    fn max_data_returns_peer_value() {
        let initial = VarInt::from_u32(1000);
        let max_data = MaxData::new(initial);
        assert_eq!(max_data.max_data(), initial);
    }

    #[test]
    fn on_transmit_updates_local_value() {
        let mut max_data = MaxData::new(VarInt::from_u32(1000));
        let clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(VarInt::from_u32(100), &clock, idle_timeout);
        assert_eq!(max_data.max_sent_offset(), VarInt::from_u32(100));
    }

    #[test]
    fn on_transmit_uses_max_offset() {
        let mut max_data = MaxData::new(VarInt::from_u32(1000));
        let clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(VarInt::from_u32(100), &clock, idle_timeout);
        max_data.on_transmit(VarInt::from_u32(50), &clock, idle_timeout);

        assert_eq!(max_data.max_sent_offset(), VarInt::from_u32(100));
    }

    #[test]
    fn on_transmit_blocks_when_at_peer_value() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(peer_value, &clock, idle_timeout);

        assert!(max_data.state.is_blocked());
    }

    #[test]
    fn on_transmit_does_not_block_below_peer_value() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(VarInt::from_u32(500), &clock, idle_timeout);

        assert!(max_data.state.is_unblocked());
    }

    #[test]
    fn on_transmit_sets_timer_when_blocked() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let mut clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        clock.inc_by(Duration::from_secs(10));
        max_data.on_transmit(peer_value, &clock, idle_timeout);
        assert!(max_data.is_armed());
        assert!(max_data.state.is_blocked());

        assert_eq!(max_data.on_timeout(&clock, idle_timeout), Poll::Pending);

        clock.inc_by(idle_timeout / 2);
        assert_eq!(max_data.on_timeout(&clock, idle_timeout), Poll::Ready(()));
    }

    #[test]
    fn on_max_data_frame_increases_peer_value() {
        let initial = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(initial);
        let new_value = VarInt::from_u32(2000);

        let diff = max_data.on_max_data_frame(new_value);

        assert_eq!(diff, Some(VarInt::from_u32(1000)));
        assert_eq!(max_data.max_data(), new_value);
    }

    #[test]
    fn on_max_data_frame_returns_none_when_not_increasing() {
        let initial = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(initial);

        assert_eq!(max_data.on_max_data_frame(VarInt::from_u32(1000)), None);
        assert_eq!(max_data.on_max_data_frame(VarInt::from_u32(500)), None);
        assert_eq!(max_data.max_data(), initial);
    }

    #[test]
    fn on_max_data_frame_unblocks() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(peer_value, &clock, idle_timeout);
        assert!(max_data.state.is_blocked());

        max_data.on_max_data_frame(VarInt::from_u32(2000));

        assert!(max_data.state.is_unblocked());
    }

    #[test]
    fn on_max_data_frame_cancels_timer() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let mut clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(peer_value, &clock, idle_timeout);
        max_data.on_max_data_frame(VarInt::from_u32(2000));

        clock.inc_by(idle_timeout);
        assert_eq!(max_data.on_timeout(&clock, idle_timeout), Poll::Pending);
    }

    #[test]
    fn on_timeout_returns_pending_when_timer_not_set() {
        let mut max_data = MaxData::new(VarInt::from_u32(1000));
        let clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        assert_eq!(max_data.on_timeout(&clock, idle_timeout), Poll::Pending);
    }

    #[test]
    fn on_timeout_returns_pending_before_expiration() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let mut clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(peer_value, &clock, idle_timeout);
        clock.inc_by(Duration::from_secs(10));

        assert_eq!(max_data.on_timeout(&clock, idle_timeout), Poll::Pending);
    }

    #[test]
    fn on_timeout_returns_ready_after_expiration() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let mut clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(peer_value, &clock, idle_timeout);
        clock.inc_by(idle_timeout / 2);

        assert_eq!(max_data.on_timeout(&clock, idle_timeout), Poll::Ready(()));
    }

    #[test]
    fn on_timeout_resets_timer_after_expiration() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let mut clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(peer_value, &clock, idle_timeout);
        clock.inc_by(idle_timeout / 2);

        assert_eq!(max_data.on_timeout(&clock, idle_timeout), Poll::Ready(()));

        assert_eq!(max_data.on_timeout(&clock, idle_timeout), Poll::Pending);

        clock.inc_by(idle_timeout / 2);
        assert_eq!(max_data.on_timeout(&clock, idle_timeout), Poll::Ready(()));
    }

    #[test]
    fn state_transitions_blocked_to_unblocked() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        assert!(max_data.state.is_unblocked());

        max_data.on_transmit(peer_value, &clock, idle_timeout);
        assert!(max_data.state.is_blocked());

        max_data.on_max_data_frame(VarInt::from_u32(2000));
        assert!(max_data.state.is_unblocked());
    }

    #[test]
    fn blocking_only_happens_once_per_transition() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(peer_value, &clock, idle_timeout);
        assert!(max_data.state.is_blocked());

        max_data.on_transmit(peer_value, &clock, idle_timeout);
        assert!(max_data.state.is_blocked());
    }

    #[test]
    fn unblocking_only_happens_once_per_transition() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(peer_value, &clock, idle_timeout);
        max_data.on_max_data_frame(VarInt::from_u32(2000));
        assert!(max_data.state.is_unblocked());

        max_data.on_max_data_frame(VarInt::from_u32(3000));
        assert!(max_data.state.is_unblocked());
    }

    #[test]
    fn multiple_block_unblock_cycles() {
        let mut max_data = MaxData::new(VarInt::from_u32(1000));
        let mut clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(VarInt::from_u32(1000), &clock, idle_timeout);
        assert!(max_data.state.is_blocked());

        clock.inc_by(Duration::from_secs(5));
        max_data.on_max_data_frame(VarInt::from_u32(2000));
        assert!(max_data.state.is_unblocked());

        clock.inc_by(Duration::from_secs(5));
        max_data.on_transmit(VarInt::from_u32(2000), &clock, idle_timeout);
        assert!(max_data.state.is_blocked());

        clock.inc_by(Duration::from_secs(5));
        max_data.on_max_data_frame(VarInt::from_u32(3000));
        assert!(max_data.state.is_unblocked());
    }

    #[test]
    fn varint_zero_offset() {
        let mut max_data = MaxData::new(VarInt::from_u32(1000));
        let clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(30);

        max_data.on_transmit(VarInt::ZERO, &clock, idle_timeout);
        assert_eq!(max_data.max_sent_offset(), VarInt::ZERO);
        assert!(max_data.state.is_unblocked());
    }

    #[test]
    fn varint_max_values() {
        let max_value = VarInt::MAX;
        let max_data = MaxData::new(max_value);

        assert_eq!(max_data.max_data(), max_value);
        assert_eq!(max_data.max_sent_offset(), VarInt::ZERO);
    }

    #[test]
    fn timer_half_idle_timeout() {
        let peer_value = VarInt::from_u32(1000);
        let mut max_data = MaxData::new(peer_value);
        let mut clock = test_clock::Clock::default();
        let idle_timeout = Duration::from_secs(100);

        max_data.on_transmit(peer_value, &clock, idle_timeout);

        clock.inc_by(Duration::from_secs(49));
        assert_eq!(max_data.on_timeout(&clock, idle_timeout), Poll::Pending);

        clock.inc_by(Duration::from_secs(1));
        assert_eq!(max_data.on_timeout(&clock, idle_timeout), Poll::Ready(()));
    }
}
