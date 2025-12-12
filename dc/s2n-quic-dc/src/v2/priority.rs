// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Priority levels for transfer scheduling
//!
//! The system supports exactly 8 priority levels, numbered 0-7, where
//! 0 is the highest priority and 7 is the lowest priority.
//! Higher priority transfers are scheduled before lower priority ones when
//! bandwidth is constrained.

use core::fmt;

/// Priority level for a transfer
///
/// There are exactly 8 priority levels, numbered 0-7:
/// - 0 is the highest priority
/// - 7 is the lowest priority
///
/// Higher priority transfers receive preferential scheduling
/// when bandwidth is limited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Priority(u8);

impl Priority {
    /// Highest priority level (0)
    pub const HIGHEST: Self = Self(0);
    
    /// Lowest priority level (7)
    pub const LOWEST: Self = Self(7);
    
    /// Default priority level (4)
    pub const DEFAULT: Self = Self(4);

    /// Create a new priority level
    ///
    /// # Panics
    ///
    /// Panics if the priority level is greater than 7
    pub const fn new(level: u8) -> Self {
        assert!(level <= 7, "priority level must be 0-7");
        Self(level)
    }

    /// Get the priority level as a u8
    pub const fn as_u8(&self) -> u8 {
        self.0
    }
}

impl Default for Priority {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_priority_levels() {
        assert_eq!(Priority::HIGHEST.as_u8(), 0);
        assert_eq!(Priority::LOWEST.as_u8(), 7);
        assert_eq!(Priority::DEFAULT.as_u8(), 4);
    }

    #[test]
    fn test_priority_ordering() {
        assert!(Priority::HIGHEST < Priority::LOWEST);
        assert!(Priority::new(0) < Priority::new(7));
        assert!(Priority::new(3) < Priority::new(4));
    }

    #[test]
    #[should_panic(expected = "priority level must be 0-7")]
    fn test_invalid_priority() {
        Priority::new(8);
    }
}
