// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use super::{free_list, handle::Sender, inner::Error, queue_id};
use s2n_quic_core::varint::VarInt;
use std::sync::{Arc, RwLock};

pub struct State<S: 'static, C: 'static, Key: 'static> {
    pub(super) pages: RwLock<SenderPages<S, C, Key>>,
}

impl<S: 'static, C: 'static, Key: 'static> State<S, C, Key> {
    #[inline]
    pub fn new(epoch: usize) -> Arc<Self> {
        Arc::new(Self {
            pages: RwLock::new(SenderPages::new(epoch)),
        })
    }
}

pub struct Senders<S: 'static, C: 'static, Key: 'static, const INITIAL_PAGE_SIZE: usize> {
    pub(super) state: Arc<State<S, C, Key>>,
    pub(super) local: Vec<Arc<[Sender<S, C, Key>]>>,
    pub(super) memory_handle: Arc<free_list::Memory<S, C, Key>>,
}

impl<S: 'static, C: 'static, Key: 'static, const INITIAL_PAGE_SIZE: usize> Clone
    for Senders<S, C, Key, INITIAL_PAGE_SIZE>
{
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            memory_handle: self.memory_handle.clone(),
            local: self.local.clone(),
        }
    }
}

impl<S: 'static, C: 'static, Key: 'static, const INITIAL_PAGE_SIZE: usize> Senders<S, C, Key, INITIAL_PAGE_SIZE> {
    #[inline]
    fn refresh_pages(&mut self) {
        let Ok(senders) = self.state.pages.read() else {
            return;
        };

        if self.local.len() == senders.pages.len() {
            return;
        }

        self.local
            .extend_from_slice(&senders.pages[self.local.len()..]);
    }

    /// Computes the page index and intra-page offset for a slot in O(1).
    ///
    /// Pages grow exponentially: page `n` has `INITIAL_PAGE_SIZE * 2^n` slots and
    /// starts at slot `(2^n - 1) * INITIAL_PAGE_SIZE`.  Given a slot `s`:
    ///
    /// ```text
    /// k         = s / INITIAL_PAGE_SIZE
    /// page_idx  = floor(log2(k + 1))          -- highest set bit of (k+1)
    /// page_start = (2^page_idx - 1) * INITIAL_PAGE_SIZE
    /// offset    = s - page_start
    /// ```
    #[inline]
    fn find_page(slot: usize) -> (usize, usize) {
        let k = slot / INITIAL_PAGE_SIZE;
        let page_idx = (k + 1).ilog2() as usize;
        let page_start = ((1usize << page_idx) - 1) * INITIAL_PAGE_SIZE;
        (page_idx, slot - page_start)
    }

    #[inline]
    pub fn lookup<T, F, V>(&mut self, queue_id: VarInt, entry: T, f: F) -> Result<V, Error<T>>
    where
        F: FnOnce(&Sender<S, C, Key>, T) -> Result<V, Error<T>>,
    {
        let slot = queue_id::index(queue_id);
        let (page_idx, offset) = Self::find_page(slot);

        if self.local.len() <= page_idx {
            self.refresh_pages();
            if self.local.len() <= page_idx {
                return Err(Error::Unallocated(entry));
            }
        }

        let Some(page) = self.local.get(page_idx) else {
            return Err(Error::Unallocated(entry));
        };
        let Some(sender) = page.get(offset) else {
            return Err(Error::Unallocated(entry));
        };

        let Some(current_queue_id) = sender.try_queue_id() else {
            return Err(Error::Unallocated(entry));
        };

        if current_queue_id != queue_id {
            return Err(Error::Unallocated(entry));
        }

        f(sender, entry)
    }

    #[inline]
    /// Iterates every currently known sender page entry and invokes `f`.
    ///
    /// # Performance
    ///
    /// This is intentionally expensive: it performs an O(total_queues) walk across
    /// the entire sender table and should only be used for rare control-plane fanout
    /// operations (for example, credential-wide invalidations). Never call this on
    /// hot data-path packet/frame processing.
    pub fn for_each_sender(&mut self, mut f: impl FnMut(&Sender<S, C, Key>)) {
        self.refresh_pages();

        for page in &self.local {
            for sender in page.iter() {
                f(sender);
            }
        }
    }
}

pub(super) struct SenderPages<S: 'static, C: 'static, Key: 'static> {
    pub(super) pages: Vec<Arc<[Sender<S, C, Key>]>>,
    pub(super) epoch: usize,
}

impl<S: 'static, C: 'static, Key: 'static> SenderPages<S, C, Key> {
    #[inline]
    pub(super) fn new(epoch: usize) -> Self {
        Self {
            pages: Vec::with_capacity(8),
            epoch,
        }
    }
}
