// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

mod bidirectional;
pub mod client;
mod coop;
pub(crate) mod metrics;
mod reader;
pub mod server;
mod writer;

pub use crate::endpoint::Error;
pub use bidirectional::Stream;
pub use client::Client;
pub use reader::Reader;
pub use server::Server;
pub use writer::{MsgFlags, Writer};

#[deprecated = "use crate::endpoint instead"]
pub use crate::endpoint;
