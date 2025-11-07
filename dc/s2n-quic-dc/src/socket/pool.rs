// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use crate::socket::pool::descriptor::Unfilled;
use core::fmt;

pub mod descriptor;

#[derive(Clone)]
pub struct Pool {
    max_packet_size: u16,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Pool")
            .field("max_packet_size", &self.max_packet_size)
            .finish()
    }
}

impl Pool {
    /// Creates a pool with the given `max_packet_size`.
    ///
    /// # Notes
    ///
    /// `max_packet_size` does not account for GRO capabilities of the underlying socket. If
    /// GRO is enabled, the `max_packet_size` should be set to `u16::MAX`.
    #[inline]
    pub fn new(max_packet_size: u16) -> Self {
        Pool { max_packet_size }
    }

    /// Allocates a new unfilled packet.
    ///
    /// Returns `None` if the packet allocator is exhausted (backpressure signal).
    #[inline]
    pub fn alloc(&self) -> Option<Unfilled> {
        Unfilled::new(self.max_packet_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::pool::descriptor::Filled;
    use bolero::{check, TypeGenerator};
    use std::{
        collections::VecDeque,
        net::{Ipv4Addr, SocketAddr},
    };

    #[derive(TypeGenerator, Debug)]
    enum Op {
        Alloc,
        DropUnfilled {
            idx: u8,
        },
        Fill {
            idx: u8,
            port: u8,
            segment_count: u8,
            segment_len: u8,
        },
        DropFilled {
            idx: u8,
        },
    }

    struct Model {
        pool: Pool,
        unfilled: VecDeque<Unfilled>,
        filled: VecDeque<Filled>,
    }

    impl Model {
        fn new(max_packet_size: u16) -> Self {
            let pool = Pool::new(max_packet_size);
            Self {
                pool,
                unfilled: VecDeque::new(),
                filled: VecDeque::new(),
            }
        }

        fn alloc(&mut self) {
            if let Some(desc) = self.pool.alloc() {
                self.unfilled.push_back(desc);
            }
        }

        fn drop_unfilled(&mut self, idx: usize) {
            if self.unfilled.is_empty() {
                return;
            }

            let idx = idx % self.unfilled.len();
            let _ = self.unfilled.remove(idx).unwrap();
        }

        fn drop_filled(&mut self, idx: usize) {
            if self.filled.is_empty() {
                return;
            }
            let idx = idx % self.filled.len();
            let _ = self.filled.remove(idx).unwrap();
        }

        fn fill(&mut self, idx: usize, port: u16, segment_count: u8, segment_len: u8) {
            let Self {
                unfilled, filled, ..
            } = self;

            if unfilled.is_empty() {
                return;
            }
            let idx = idx % unfilled.len();
            let unfilled = unfilled.remove(idx).unwrap();

            let src = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);

            let segment_len = segment_len as usize;
            let segment_count = segment_count as usize;
            let mut actual_segment_count = 0;

            let res = unfilled.fill_with(|addr, cmsg, mut payload| {
                if port == 0 {
                    return Err(());
                }

                addr.set(src.into());

                if segment_count > 1 {
                    cmsg.set_segment_len(segment_len as _);
                }
                let mut offset = 0;

                for segment_idx in 0..segment_count {
                    let remaining = &mut payload[offset..];
                    let len = remaining.len().min(segment_len);
                    if len == 0 {
                        break;
                    }

                    actual_segment_count += 1;
                    remaining[..len].fill(segment_idx as u8);
                    offset += len;
                }

                Ok(offset)
            });

            assert_eq!(res.is_err(), port == 0);

            if let Ok(segments) = res {
                for (idx, segment) in segments.into_iter().enumerate() {
                    // we allow only one segment to be empty. this makes it easier to log when we get empty packets, which are unexpected
                    if segment.is_empty() {
                        assert_eq!(actual_segment_count, 0);
                        assert_eq!(idx, 0);
                        continue;
                    }

                    assert!(
                        idx < actual_segment_count,
                        "{idx} < {actual_segment_count}, {:?}",
                        segment.payload()
                    );

                    //  the final segment is allowed to be undersized
                    if idx == actual_segment_count - 1 {
                        assert!(segment.len() as usize <= segment_len);
                    } else {
                        assert_eq!(segment.len() as usize, segment_len);
                    }

                    // make sure bytes match the segment pattern
                    for byte in segment.payload().iter() {
                        assert_eq!(*byte, idx as u8);
                    }

                    filled.push_back(segment);
                }
            }
        }

        fn apply(&mut self, op: &Op) {
            match op {
                Op::Alloc => self.alloc(),
                Op::DropUnfilled { idx } => self.drop_unfilled(*idx as usize),
                Op::Fill {
                    idx,
                    port,
                    segment_count,
                    segment_len,
                } => self.fill(*idx as _, *port as _, *segment_count, *segment_len),
                Op::DropFilled { idx } => self.drop_filled(*idx as usize),
            }
        }
    }

    #[test]
    fn model_test() {
        check!()
            .with_type::<Vec<Op>>()
            .with_test_time(core::time::Duration::from_secs(20))
            .for_each(|ops| {
                let max_packet_size = 1000;
                let mut model = Model::new(max_packet_size);
                for op in ops {
                    model.apply(op);
                }
            });
    }
}
