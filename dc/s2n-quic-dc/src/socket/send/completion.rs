// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::{
    socket::{
        pool::descriptor,
        send::transmission::{Entry, Transmission},
    },
    sync::intrusive_queue,
};
use s2n_quic_core::time::Timestamp;

pub trait Completion<Info, Meta>: Sized {
    type Completer: Completer<Info, Meta, Self>;

    fn upgrade(&self) -> Option<Self::Completer>;
}

impl<Info, Meta> Completion<Info, Meta> for () {
    type Completer = ();

    fn upgrade(&self) -> Option<Self::Completer> {
        None
    }
}

pub trait Completer<Info, Meta, Completion> {
    fn complete(self, transmission: Entry<Info, Meta, Completion>);
}

impl<Info, Meta, Completion> Completer<Info, Meta, Completion> for () {
    fn complete(self, _transmission: Entry<Info, Meta, Completion>) {}
}

pub struct CompleteTransmission<'a, Info, Meta> {
    pub info: Info,
    pub segment: descriptor::Filled,
    pub transmission_time: Timestamp,
    pub meta: &'a Meta,
}

pub struct Queue<Info, Meta, Completion> {
    completion: intrusive_queue::Queue<Transmission<Info, Meta, Completion>>,
    free: intrusive_queue::Queue<Transmission<Info, Meta, Completion>>,
}

impl<Info, Meta, Completion> Default for Queue<Info, Meta, Completion> {
    #[inline]
    fn default() -> Self {
        Self {
            completion: intrusive_queue::Queue::new(),
            free: intrusive_queue::Queue::new(),
        }
    }
}

impl<Info, Meta, Completion> Queue<Info, Meta, Completion>
where
    Meta: Default,
{
    #[inline]
    pub fn alloc_entry(
        &self,
        batch_size: usize,
        completion_queue: impl FnOnce() -> Completion,
    ) -> Entry<Info, Meta, Completion> {
        let mut entry = self.free.pop_front().unwrap_or_else(|| {
            let transmission = Transmission {
                descriptors: Vec::with_capacity(batch_size),
                total_len: 0,
                meta: Default::default(),
                transmission_time: None,
                completion: None,
                #[cfg(debug_assertions)]
                span: tracing::info_span!("transmission"),
            };
            Entry::new(transmission)
        });

        debug_assert!(entry.descriptors.is_empty());
        entry.descriptors.reserve(batch_size);

        if entry.completion.is_none() {
            entry.completion = Some(completion_queue());
        }

        entry
    }

    #[inline]
    pub fn complete_transmission(&self, entry: Entry<Info, Meta, Completion>) {
        self.completion.push_back(entry);
    }

    #[inline]
    pub fn drain_completion_queue(
        &self,
        mut on_transmission: impl FnMut(CompleteTransmission<Info, Meta>),
    ) -> usize {
        let mut count = 0;

        // try multiple times to drain the completion queue in case something was pushed while we were processing it
        loop {
            let mut remaining = self.completion.take();

            let mut did_process_transmission = false;
            while let Some(mut transmission) = remaining.pop_front() {
                {
                    let transmission = &mut *transmission;
                    let descriptors = transmission.descriptors.drain(..);
                    let transmission_time = transmission.transmission_time.unwrap();
                    let meta = &transmission.meta;
                    for (segment, info) in descriptors {
                        on_transmission(CompleteTransmission {
                            info,
                            segment,
                            transmission_time,
                            meta,
                        });
                    }
                }
                transmission.total_len = 0;
                transmission.meta = Default::default();
                self.free.push_back(transmission);
                count += 1;
                did_process_transmission = true;
            }

            if !did_process_transmission {
                break;
            }
        }

        count
    }
}
