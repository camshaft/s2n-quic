// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Causality tracking for transfer dependencies.
//!
//! The causality system enables applications to express ordering and failure dependencies
//! between transfers. Each transfer receives a causality token that can be used as a
//! dependency for subsequent transfers.

/// An opaque token representing a transfer in the causality graph.
///
/// Tokens are returned when transfers are initiated and can be used as dependencies
/// for subsequent transfers to enforce ordering or failure cascading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Token {
    _todo: (),
}

impl Token {
    /// Creates a dependency that waits for request completion and cascades errors.
    pub fn wait_for_request(self) -> Dependency {
        Dependency {
            token: self,
            behavior: Behavior {
                wait_for: WaitFor::Request,
                cascade_errors: true,
            },
        }
    }

    /// Creates a dependency that waits for response completion and cascades errors.
    pub fn wait_for_response(self) -> Dependency {
        Dependency {
            token: self,
            behavior: Behavior {
                wait_for: WaitFor::Response,
                cascade_errors: true,
            },
        }
    }

    /// Creates a dependency that waits for request but doesn't cascade errors.
    pub fn wait_for_request_no_cascade(self) -> Dependency {
        Dependency {
            token: self,
            behavior: Behavior {
                wait_for: WaitFor::Request,
                cascade_errors: false,
            },
        }
    }

    /// Creates a dependency that waits for response but doesn't cascade errors.
    pub fn wait_for_response_no_cascade(self) -> Dependency {
        Dependency {
            token: self,
            behavior: Behavior {
                wait_for: WaitFor::Response,
                cascade_errors: false,
            },
        }
    }
}

/// A causality dependency specification.
#[derive(Clone, Copy, Debug)]
pub struct Dependency {
    /// The causality token of the transfer to depend on
    pub token: Token,

    /// Dependency behavior configuration
    pub behavior: Behavior,
}

/// Defines how a dependency affects the dependent transfer.
#[derive(Clone, Copy, Debug)]
pub struct Behavior {
    /// Whether to wait for the dependency's request or full response
    pub wait_for: WaitFor,

    /// Whether errors in the dependency should cascade to this transfer
    pub cascade_errors: bool,
}

/// Specifies what to wait for in a dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitFor {
    /// Wait only until the dependency's request has been sent
    Request,

    /// Wait until the dependency's response has been received
    Response,
}
