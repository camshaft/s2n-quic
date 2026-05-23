// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#![allow(dead_code)]
#![allow(
    clippy::borrow_deref_ref,
    clippy::arc_with_non_send_sync,
    clippy::clone_on_copy,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::drain_collect,
    clippy::drop_non_drop,
    clippy::enum_variant_names,
    clippy::explicit_auto_deref,
    clippy::items_after_test_module,
    clippy::let_and_return,
    clippy::manual_is_multiple_of,
    clippy::manual_range_contains,
    clippy::mem_replace_with_default,
    clippy::missing_transmute_annotations,
    clippy::module_inception,
    clippy::needless_borrow,
    clippy::needless_return,
    clippy::new_without_default,
    clippy::nonminimal_bool,
    clippy::question_mark,
    clippy::redundant_locals,
    clippy::redundant_closure,
    clippy::result_large_err,
    clippy::type_complexity,
    clippy::unnecessary_cast,
    clippy::unnecessary_map_or,
    clippy::useless_conversion,
    clippy::while_let_on_iterator,
    clippy::while_let_loop
)]

#[macro_use]
pub mod tracing;

pub mod acceptor;
pub mod allocator;
pub mod busy_poll;
pub mod byte_vec;
pub mod congestion;
pub mod control;
pub mod counter;
pub mod credentials;
pub mod crypto;
pub mod datagram;
pub mod endpoint;
pub mod event;
pub mod flow;
pub mod intrusive;
pub mod msg;
pub mod packet;
pub mod path;
pub mod psk;
pub mod recovery;
pub mod runtime;
pub mod socket;
pub mod stream;
pub mod task;
pub mod time;
pub mod uds;
pub mod xorshift;

#[deprecated = "use stream instead of stream3"]
pub use stream as stream3;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

pub use s2n_quic_core::dc::{Version, SUPPORTED_VERSIONS};
