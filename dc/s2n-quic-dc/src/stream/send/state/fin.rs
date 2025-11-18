// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use s2n_quic_core::state::{event, is};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum State {
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
