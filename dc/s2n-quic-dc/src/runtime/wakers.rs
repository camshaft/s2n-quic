use std::{
    ops::ControlFlow,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Wake, Waker},
};

#[derive(Default)]
pub struct Wakers {
    pages: Vec<Page>,
    wakers: Vec<Waker>,
}

impl Wakers {
    pub fn waker(&mut self, index: usize) -> Waker {
        while index >= self.wakers.len() {
            let page = Page::default();
            for i in 0..64 {
                let waker = Arc::new(PagedWaker {
                    page: page.clone(),
                    index: i,
                });
                self.wakers.push(Waker::from(waker));
            }
            self.pages.push(page);
        }

        let waker = self.wakers[index].clone();
        waker
    }

    pub fn drain(&mut self) -> impl Iterator<Item = usize> + '_ {
        Drain::new(&self.pages)
    }
}

pub struct Drain<'a> {
    pages: &'a [Page],
    page: usize,
    slots: SlotIter,
    slot: SlotIter,
}

impl<'a> Drain<'a> {
    fn new(pages: &'a [Page]) -> Self {
        let mut iter = Self {
            pages,
            page: 0,
            slots: SlotIter(0),
            slot: SlotIter(0),
        };
        iter.populate_slots();
        iter
    }

    fn populate_slots(&mut self) -> ControlFlow<()> {
        loop {
            let Some(page) = self.pages.get(self.page) else {
                return ControlFlow::Break(());
            };

            let occupied = page.bits[64].swap(0, Ordering::Acquire);
            self.slots = SlotIter(occupied);

            if self.populate_slot().is_break() {
                self.page += 1;
                continue;
            }

            return ControlFlow::Continue(());
        }
    }

    fn populate_slot(&mut self) -> ControlFlow<()> {
        let Some(index) = self.slots.next() else {
            return ControlFlow::Break(());
        };
        let bits = unsafe {
            let page = self.pages.get_unchecked(self.page);
            page.bits.get_unchecked(index).swap(0, Ordering::Acquire)
        };
        self.slot = SlotIter(bits);
        ControlFlow::Continue(())
    }
}

impl Iterator for Drain<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(index) = self.slot.next() {
                return Some(index + self.page * 64);
            }

            if self.populate_slot().is_continue() {
                continue;
            }

            self.page += 1;
            if self.populate_slots().is_break() {
                return None;
            }
        }
    }
}

struct SlotIter(u64);

impl Iterator for SlotIter {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        let zeros = self.0.trailing_zeros();
        if zeros == 64 {
            return None;
        }

        // clear out the bit
        self.0 &= !(1 << zeros);

        let index = zeros as usize;
        Some(index)
    }
}

#[derive(Clone)]
struct Page {
    bits: Arc<[AtomicU64; 65]>,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            bits: Arc::new(std::array::from_fn(|_| AtomicU64::new(0))),
        }
    }
}

impl Page {
    fn wake(&self, index: usize) {
        let offset = index / 64;
        let bit = index % 64;
        let word = unsafe { self.bits.get_unchecked(index) };
        word.fetch_or(1 << bit, Ordering::Release);

        // The last slot tracks which offsets have actual values in them to avoid iterating
        self.bits[64].fetch_or(1 << offset, Ordering::Release);
    }
}

struct PagedWaker {
    page: Page,
    index: usize,
}

impl Wake for PagedWaker {
    fn wake(self: Arc<Self>) {
        self.page.wake(self.index);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.page.wake(self.index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bolero::{check, TypeGenerator};
    use std::collections::BTreeSet;

    #[derive(TypeGenerator, Clone, Debug)]
    enum Op {
        Wake(u16),
        Drain,
    }

    #[test]
    fn model_check() {
        check!().with_type::<Vec<Op>>().for_each(|ops| {
            let mut wakers = Wakers::default();
            let mut woken = BTreeSet::new();

            for op in ops {
                match op {
                    Op::Wake(index) => {
                        let index = *index as usize;
                        let waker = wakers.waker(index);
                        waker.wake();
                        woken.insert(index);
                    }
                    Op::Drain => {
                        let drained = wakers.drain().collect::<BTreeSet<_>>();
                        assert_eq!(woken, drained);
                        woken.clear();
                    }
                }
            }

            let drained = wakers.drain().collect::<BTreeSet<_>>();
            assert_eq!(woken, drained);
        })
    }

    #[test]
    fn slot_iter() {
        check!().with_type::<u64>().cloned().for_each(|bits| {
            let mut iter = SlotIter(bits);
            let mut result = 0;
            while let Some(index) = iter.next() {
                result |= 1 << index;
            }
            assert_eq!(result, bits);
        })
    }
}
