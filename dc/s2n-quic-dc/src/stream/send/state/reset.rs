use super::{error::Kind, Error};
use s2n_quic_core::{
    endpoint::Location,
    ensure, frame,
    state::{event, is},
    stream::state::Sender as SenderState,
    varint::VarInt,
};

type ConnectionClose = frame::ConnectionClose<'static>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Idle,
    Queued,
    Sent,
    Finished,
}

impl State {
    is!(is_idle, Idle);
    is!(is_queued, Queued);
    is!(is_sent, Sent);
    is!(is_finished, Finished);

    event! {
        on_reset(Idle => Queued);
        on_sent(Queued => Sent);
        on_ack(Sent => Finished);
        on_silent_close(Idle => Finished);
    }
}

impl From<&State> for SenderState {
    fn from(value: &State) -> Self {
        match value {
            State::Idle => SenderState::Send,
            State::Queued => SenderState::ResetQueued,
            State::Sent => SenderState::ResetSent,
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

        match self.error.kind {
            // Don't send a frame if
            Kind::IdleTimeout => None,
            Kind::ApplicationError { error } => Some(frame::ConnectionClose {
                error_code: VarInt::new(*error).unwrap(),
                frame_type: None,
                reason: None,
            }),
            Kind::TransportError { code } => Some(frame::ConnectionClose {
                error_code: code,
                frame_type: Some(VarInt::from_u16(0)),
                reason: None,
            }),
            _ => Some(frame::ConnectionClose {
                error_code: VarInt::from_u16(1),
                frame_type: None,
                reason: None,
            }),
        }
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
        self.state.is_queued() || self.state.is_sent()
    }

    pub fn state(&self) -> Option<SenderState> {
        if self.state.is_idle() {
            None
        } else {
            Some((&self.state).into())
        }
    }

    pub fn on_error(&mut self, error: Error, source: Location) -> Result<(), ()> {
        ensure!(self.error.is_none(), Err(()));

        let is_idle_timeout = matches!(error.kind(), Kind::IdleTimeout);
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
        let _ = self.state.on_sent();
        let frame = self.error.as_ref()?.as_frame()?;
        Some(frame)
    }
}
