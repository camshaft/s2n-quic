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
pub async fn pick_two<T, R, S>(rx: R, senders: Vec<S>)
where
    T: ByteCost + PathSecretMapEntry,
    R: Receiver<T>,
    S: Sender<T>,
{
    pick_two_with_random(rx, senders, |upper_bound| rand::random_range(..upper_bound)).await
}

pub async fn pick_two_with_random<T, R, S, Rand>(mut rx: R, mut senders: Vec<S>, mut random: Rand)
where
    T: ByteCost + PathSecretMapEntry,
    R: Receiver<T>,
    S: Sender<T>,
    Rand: FnMut(usize) -> usize,
{
    loop {
        let Some(entry) = rx.recv().await else {
            break;
        };

        let bytes = entry.byte_cost();
        let mut slot = core::mem::MaybeUninit::new(entry);

        let sent = poll_fn(|cx| try_send_pick_two(cx, &mut slot, &mut senders, &mut random)).await;

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
    random: &mut Rand,
) -> Poll<bool>
where
    T: PathSecretMapEntry,
    S: Sender<T>,
    Rand: FnMut(usize) -> usize,
{
    loop {
        if senders.is_empty() {
            return Poll::Ready(false);
        }

        let chosen_idx = {
            let value = unsafe { &*slot.as_ptr() };
            let picked = value
                .path_secret_entry()
                .pick_sender_by_next_transmission(&mut *random);
            picked % senders.len()
        };

        match senders[chosen_idx].poll_send(cx, slot) {
            Poll::Ready(Ok(())) => return Poll::Ready(true),
            Poll::Ready(Err(())) => {
                senders.swap_remove(chosen_idx);
                if senders.is_empty() {
                    return Poll::Ready(false);
                }
            }
            Poll::Pending => {
                let len = senders.len();
                let mut retry = false;
                for offset in 1..len {
                    let idx = (chosen_idx + offset) % len;
                    match senders[idx].poll_send(cx, slot) {
                        Poll::Ready(Ok(())) => return Poll::Ready(true),
                        Poll::Ready(Err(())) => {
                            senders.swap_remove(idx);
                            retry = true;
                            break;
                        }
                        Poll::Pending => {}
                    }
                }
                if retry {
                    continue;
                }
                return Poll::Pending;
            }
        }
    }
}
