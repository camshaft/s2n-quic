// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Memory allocator abstraction.

use std::{alloc::Layout, ptr::NonNull};

macro_rules! subheap {
    ($name:ident) => {
        pub mod $name {
            use super::*;

            /// Allocates memory with the given layout.
            ///
            /// Returns `None` if the allocator cannot satisfy the request. Currently this
            /// only happens for zero-sized layouts; once backed by a bounded subheap,
            /// `None` will indicate the memory budget is exhausted.
            #[inline]
            pub fn alloc(layout: Layout) -> Option<NonNull<u8>> {
                if layout.size() == 0 {
                    return None;
                }

                let ptr = unsafe {
                    // SAFETY: layout is non-zero size
                    std::alloc::alloc(layout)
                };

                NonNull::new(ptr)
            }

            /// Deallocates memory previously obtained from [`alloc`].
            ///
            /// # Safety
            ///
            /// * `ptr` must have been returned by a prior call to [`alloc`] with the same `layout`.
            /// * The memory must not have been previously deallocated.
            #[inline]
            pub unsafe fn dealloc(ptr: NonNull<u8>, layout: Layout) {
                std::alloc::dealloc(ptr.as_ptr(), layout);
            }
        }
    };
}

subheap!(packet);
