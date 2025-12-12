// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Causality tracking for transfer dependencies
//!
//! The causality system tracks dependencies between transfers, enabling
//! applications to express ordering constraints and failure propagation
//! semantics.

use core::fmt;

/// Opaque token representing a transfer in the causality map
///
/// Applications receive causality tokens when initiating transfers.
/// These tokens can be used as dependencies for subsequent transfers.
/// The token encodes the slot index and generation number to enable
/// lock-free lookups and stale token detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CausalityToken {
    slot: u32,
    generation: u32,
}

impl CausalityToken {
    /// Create a new causality token (internal use only)
    #[doc(hidden)]
    pub fn new(slot: u32, generation: u32) -> Self {
        Self { slot, generation }
    }

    /// Get the slot index
    pub(crate) fn slot(&self) -> u32 {
        self.slot
    }

    /// Get the generation number
    pub(crate) fn generation(&self) -> u32 {
        self.generation
    }
}

impl fmt::Display for CausalityToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CausalityToken({}/{})", self.slot, self.generation)
    }
}

/// Type of dependency relationship between transfers
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DependencyType {
    /// Wait for the dependency's response and cascade errors
    ///
    /// The dependent transfer waits until the dependency has completed
    /// its full request-response cycle. If the dependency fails, the
    /// dependent transfer fails automatically.
    WaitForResponseCascadeError,

    /// Wait for the dependency's request and cascade errors
    ///
    /// The dependent transfer waits only until the dependency's request
    /// has been sent (but not for the response). If the dependency fails,
    /// the error cascades to the dependent. This enables pipelining.
    WaitForRequestCascadeError,

    /// Wait for the dependency's response but don't cascade errors
    ///
    /// The dependent transfer waits for the dependency's full completion
    /// but continues processing even if the dependency fails.
    WaitForResponseNoCascade,

    /// Wait for the dependency's request but don't cascade errors
    ///
    /// The dependent transfer waits only for the dependency's request
    /// to be sent and continues regardless of the dependency's outcome.
    /// This provides loose ordering with maximum concurrency.
    WaitForRequestNoCascade,
}

impl DependencyType {
    /// Returns true if this dependency type waits for the response
    pub fn waits_for_response(&self) -> bool {
        matches!(
            self,
            DependencyType::WaitForResponseCascadeError
                | DependencyType::WaitForResponseNoCascade
        )
    }

    /// Returns true if this dependency type cascades errors
    pub fn cascades_errors(&self) -> bool {
        matches!(
            self,
            DependencyType::WaitForResponseCascadeError
                | DependencyType::WaitForRequestCascadeError
        )
    }
}

impl Default for DependencyType {
    fn default() -> Self {
        DependencyType::WaitForResponseCascadeError
    }
}

/// A dependency specification for a transfer
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Dependency {
    /// The causality token of the dependency
    pub token: CausalityToken,
    /// The type of dependency relationship
    pub dependency_type: DependencyType,
}

impl Dependency {
    /// Create a new dependency
    pub fn new(token: CausalityToken, dependency_type: DependencyType) -> Self {
        Self {
            token,
            dependency_type,
        }
    }

    /// Create a dependency that waits for response and cascades errors
    pub fn wait_for_response(token: CausalityToken) -> Self {
        Self::new(token, DependencyType::WaitForResponseCascadeError)
    }

    /// Create a dependency that waits for request and cascades errors
    pub fn wait_for_request(token: CausalityToken) -> Self {
        Self::new(token, DependencyType::WaitForRequestCascadeError)
    }

    /// Create a dependency that waits for response without cascading errors
    pub fn wait_for_response_no_cascade(token: CausalityToken) -> Self {
        Self::new(token, DependencyType::WaitForResponseNoCascade)
    }

    /// Create a dependency that waits for request without cascading errors
    pub fn wait_for_request_no_cascade(token: CausalityToken) -> Self {
        Self::new(token, DependencyType::WaitForRequestNoCascade)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_causality_token() {
        let token = CausalityToken::new(42, 1);
        assert_eq!(token.slot(), 42);
        assert_eq!(token.generation(), 1);
    }

    #[test]
    fn test_dependency_type_predicates() {
        assert!(DependencyType::WaitForResponseCascadeError.waits_for_response());
        assert!(DependencyType::WaitForResponseCascadeError.cascades_errors());

        assert!(!DependencyType::WaitForRequestCascadeError.waits_for_response());
        assert!(DependencyType::WaitForRequestCascadeError.cascades_errors());

        assert!(DependencyType::WaitForResponseNoCascade.waits_for_response());
        assert!(!DependencyType::WaitForResponseNoCascade.cascades_errors());

        assert!(!DependencyType::WaitForRequestNoCascade.waits_for_response());
        assert!(!DependencyType::WaitForRequestNoCascade.cascades_errors());
    }

    #[test]
    fn test_dependency_constructors() {
        let token = CausalityToken::new(1, 1);

        let dep1 = Dependency::wait_for_response(token);
        assert_eq!(dep1.dependency_type, DependencyType::WaitForResponseCascadeError);

        let dep2 = Dependency::wait_for_request(token);
        assert_eq!(dep2.dependency_type, DependencyType::WaitForRequestCascadeError);

        let dep3 = Dependency::wait_for_response_no_cascade(token);
        assert_eq!(dep3.dependency_type, DependencyType::WaitForResponseNoCascade);

        let dep4 = Dependency::wait_for_request_no_cascade(token);
        assert_eq!(dep4.dependency_type, DependencyType::WaitForRequestNoCascade);
    }
}
