use crate::stream::error::{Error, Kind};
use s2n_quic_core::{
    endpoint::Location,
    frame,
    state::{event, is},
    stream::state::Sender as SenderState,
};

type ConnectionClose = frame::ConnectionClose<'static>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    /// Reset has been queued but not yet transmitted.
    Queued,
    /// Reset was sent for the first time. Peer activity can immediately
    /// re-queue for retransmission since the peer clearly missed it.
    Sent,
    Throttling,
    ThrottlingQueued,
    Finished,
}

impl State {
    is!(is_idle, Idle);
    is!(is_queued, Queued | ThrottlingQueued);
    is!(is_sent, Sent);
    is!(is_throttling, Throttling);
    is!(is_finished, Finished);
    is!(
        is_waiting_ack,
        Queued | Sent | Throttling | ThrottlingQueued
    );

    event! {
        on_reset(Idle => Queued);
        // Maintain if we're throttling or not
        on_sent(Queued => Sent, ThrottlingQueued => Throttling);
        on_ack(Sent | Queued | Throttling | ThrottlingQueued => Finished);
        // Reset to the non-throttled state
        on_pto_timeout(Sent | Throttling => Queued);
        // Trigger an immediate throttled queued
        on_peer_activity(Sent => ThrottlingQueued);
        on_silent_close(Idle | Queued | Sent | Throttling | ThrottlingQueued => Finished);
    }
}

impl From<&State> for SenderState {
    fn from(value: &State) -> Self {
        match value {
            State::Idle => SenderState::Send,
            State::Queued => SenderState::ResetQueued,
            State::Sent | State::Throttling | State::ThrottlingQueued => SenderState::ResetSent,
            State::Finished => SenderState::ResetRecvd,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ErrorState {
    pub error: Error,
    pub source: Location,
}

impl ErrorState {
    fn as_frame(&self) -> Option<ConnectionClose> {
        // No need to send the peer an error if they sent it
        if matches!(self.source, Location::Remote) {
            return None;
        }

        self.error.kind.as_connection_close()
    }
}

#[derive(Clone, Default, Debug)]
pub struct Reset {
    state: State,
    error: Option<ErrorState>,
}

impl Reset {
    pub fn is_idle(&self) -> bool {
        self.state.is_idle()
    }

    pub fn is_queued(&self) -> bool {
        self.state.is_queued()
    }

    pub fn waiting_ack(&self) -> bool {
        self.state.is_waiting_ack()
    }

    pub fn state(&self) -> Option<SenderState> {
        if self.state.is_idle() {
            None
        } else {
            Some((&self.state).into())
        }
    }

    pub fn on_ack(&mut self) {
        let _ = self.state.on_ack();
    }

    /// Re-enqueue the reset for retransmission on PTO timeout.
    ///
    /// PTO provides exponential backoff throttling. This can re-queue from
    /// either `Sent` or `Retransmitting` states.
    pub fn on_pto_timeout(&mut self) {
        let _ = self.state.on_pto_timeout();
    }

    /// Called when the peer sends us a packet while we're waiting for an ACK
    /// to our CONNECTION_CLOSE. On the first occurrence (state == `Sent`),
    /// immediately re-queue so we retransmit quickly. After that, we're in
    /// `Retransmitting` and PTO handles further retransmissions.
    pub fn on_peer_activity(&mut self) {
        let _ = self.state.on_peer_activity();
    }

    pub fn on_error(&mut self, error: Error, source: Location) -> Result<(), ()> {
        let is_idle_timeout = matches!(error.kind(), Kind::IdleTimeout);

        if self.error.is_some() {
            // * If the remote peer also sent us an error then we're done
            // * If we timed out then we're also done
            if source.is_remote() || is_idle_timeout {
                let _ = self.state.on_silent_close();
                return Ok(());
            }
            return Err(());
        }

        self.error = Some(ErrorState { error, source });

        if source.is_remote() || is_idle_timeout {
            let _ = self.state.on_silent_close();
        } else {
            let _ = self.state.on_reset();
        }

        Ok(())
    }

    pub fn error(&self) -> Option<(&Error, Location)> {
        self.error
            .as_ref()
            .map(|state| (&state.error, state.source))
    }

    pub fn try_transmit(&mut self) -> Option<ConnectionClose> {
        let frame = self.error.as_ref()?.as_frame()?;
        let _ = self.state.on_sent();
        Some(frame)
    }
}
