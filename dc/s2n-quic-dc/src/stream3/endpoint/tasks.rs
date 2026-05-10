// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    path::secret::map::Entry as PathSecretEntry,
    socket::channel::{ByteCost, Receiver, Sender},
};
use core::{
    future::poll_fn,
    task::{self, Poll},
};
use std::sync::Arc;

/// Routing key accessor for stream3 send-side load-balancing tasks.
pub trait PathSecretMapEntry {
    fn path_secret_entry(&self) -> &Arc<PathSecretEntry>;
}

impl<T> PathSecretMapEntry for crate::intrusive_queue::Entry<T>
where
    T: PathSecretMapEntry,
{
    #[inline]
    fn path_secret_entry(&self) -> &Arc<PathSecretEntry> {
        (**self).path_secret_entry()
    }
}

impl PathSecretMapEntry for crate::stream3::frame::Frame {
    #[inline]
    fn path_secret_entry(&self) -> &Arc<PathSecretEntry> {
        &self.path_secret_entry
    }
}

/// Routes items to socket senders by using pick-two path scheduling from the path secret map
/// entry associated with each item.
pub async fn pick_two<T, R, S, Rand>(mut rx: R, mut senders: Vec<S>, random: Rand)
where
    T: ByteCost + PathSecretMapEntry,
    R: Receiver<T>,
    S: Sender<T>,
    Rand: Fn(usize) -> usize,
{
    loop {
        let Some(entry) = rx.recv().await else {
            break;
        };

        let bytes = entry.byte_cost();
        let mut slot = core::mem::MaybeUninit::new(entry);

        let sent = poll_fn(|cx| try_send_pick_two(cx, &mut slot, &mut senders, &random)).await;

        if !sent {
            break;
        }

        rx.on_consumed(bytes);
    }
}

fn try_send_pick_two<T, S, Rand>(
    cx: &mut task::Context<'_>,
    slot: &mut core::mem::MaybeUninit<T>,
    senders: &mut Vec<S>,
    random: &Rand,
) -> Poll<bool>
where
    T: PathSecretMapEntry,
    S: Sender<T>,
    Rand: Fn(usize) -> usize,
{
    loop {
        if senders.is_empty() {
            return Poll::Ready(false);
        }

        let chosen_idx = {
            // SAFETY: `slot` is initialized with `MaybeUninit::new(entry)` and remains
            // initialized until it is consumed by a successful `poll_send`.
            let value = unsafe { &*slot.as_ptr() };
            let picked = value
                .path_secret_entry()
                .pick_sender_by_next_transmission(random);
            debug_assert!(
                picked < senders.len(),
                "picked sender index out of bounds: picked={} senders={}",
                picked,
                senders.len()
            );
            if picked >= senders.len() {
                return Poll::Ready(false);
            }
            picked
        };

        match senders[chosen_idx].poll_send(cx, slot) {
            Poll::Ready(Ok(())) => return Poll::Ready(true),
            Poll::Ready(Err(())) => return Poll::Ready(false),
            Poll::Pending => {
                let len = senders.len();
                for offset in 1..len {
                    let idx = (chosen_idx + offset) % len;
                    match senders[idx].poll_send(cx, slot) {
                        Poll::Ready(Ok(())) => return Poll::Ready(true),
                        Poll::Ready(Err(())) => return Poll::Ready(false),
                        Poll::Pending => {}
                    }
                }
                return Poll::Pending;
            }
        }
    }
}
