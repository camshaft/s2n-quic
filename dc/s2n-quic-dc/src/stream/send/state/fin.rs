// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use s2n_quic_core::{
    ensure,
    state::{event, is},
    varint::VarInt,
};
use tracing::debug;

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
        on_send_fin(Unknown | Known => Sent);
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
            debug!(%value, "fin known");
            self.value = Some(value);
            Ok(())
        } else {
            debug_assert_eq!(self.value, Some(value));
            Err(())
        }
    }

    pub fn value(&self) -> Option<VarInt> {
        self.value
    }

    pub fn on_transmit(&mut self) {
        ensure!(self.state.on_send_fin().is_ok());
        tracing::debug!("fin sent");
    }

    pub fn on_ack(&mut self) {
        ensure!(self.state.on_fin_ack().is_ok());
        debug!("fin acked")
    }

    pub fn is_acked(&self) -> bool {
        self.state.is_acked()
    }
}
