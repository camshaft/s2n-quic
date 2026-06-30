// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{counter::SharedCounter, rseq::Channels};
use std::sync::Arc;

/// A `BoolCounter` represents an event with a success/failure or other binary state. For example,
/// it can be used to count attempted outgoing connections while also representing the
/// success/failure of those connections in one metric.
#[derive(Clone)]
pub struct BoolCounter {
    channels: Arc<Channels<SharedCounter>>,
    true_: u32,
    false_: u32,
}

impl BoolCounter {
    pub(crate) fn new(channels: Arc<Channels<SharedCounter>>) -> BoolCounter {
        BoolCounter {
            true_: channels.allocate(),
            false_: channels.allocate(),
            channels,
        }
    }

    pub fn record(&self, value: bool) {
        if value {
            self.channels.send_event(((self.true_ as u64) << 32) | 1);
        } else {
            self.channels.send_event(((self.false_ as u64) << 32) | 1);
        }
    }

    /// Drains the accumulated true/false counts and reports them to `backend`.
    pub(crate) fn report(&self, info: &crate::MetricInfo<'_>, backend: &mut dyn crate::Backend) {
        let true_ = self.channels.get_mut(self.true_, std::mem::take).value;
        let false_ = self.channels.get_mut(self.false_, std::mem::take).value;
        backend.record_bool(info, true_, false_);
    }
}
