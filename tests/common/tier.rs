// Test tier system for managing test coverage levels
//
// Tier 0 (Smoke): Unit tests only, < 5s
// Tier 1 (Pre-commit + CI): All specs × sessions × WMs (same-WM), ~60s
// Tier 2 (Merge queue): Full matrix with cross-WM + op-order, ~20min

use std::fmt;

/// Session type for i3mux testing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionType {
    Local,
    Remote,
}

impl fmt::Display for SessionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionType::Local => write!(f, "local"),
            SessionType::Remote => write!(f, "remote"),
        }
    }
}

/// Window manager type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WmType {
    I3,
    Sway,
}

impl fmt::Display for WmType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WmType::I3 => write!(f, "i3"),
            WmType::Sway => write!(f, "sway"),
        }
    }
}

/// Target WM for attach operation (relative to create WM)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AttachTarget {
    /// Attach from same WM as created
    SameWm,
    /// Attach from different WM (cross-WM scenario)
    CrossWm,
}

impl AttachTarget {
    /// Get the actual WM type for attach given the create WM
    pub fn resolve(&self, create_wm: WmType) -> WmType {
        match self {
            AttachTarget::SameWm => create_wm,
            AttachTarget::CrossWm => match create_wm {
                WmType::I3 => WmType::Sway,
                WmType::Sway => WmType::I3,
            },
        }
    }
}

impl fmt::Display for AttachTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttachTarget::SameWm => write!(f, "same"),
            AttachTarget::CrossWm => write!(f, "cross"),
        }
    }
}

/// When to execute layout operations (splits, focus, etc.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpOrder {
    /// Execute operations before detach (current behavior)
    BeforeDetach,
    /// Execute operations after attach (tests restore then modify)
    AfterAttach,
}

impl fmt::Display for OpOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpOrder::BeforeDetach => write!(f, "ops-before"),
            OpOrder::AfterAttach => write!(f, "ops-after"),
        }
    }
}

/// Check if full matrix tests should run
pub fn is_full_matrix_enabled() -> bool {
    std::env::var("I3MUX_FULL_MATRIX").is_ok()
}

/// All layout spec names for parameterized tests
pub const ALL_SPECS: &[&str] = &[
    "restore-hsplit-2",
    "restore-vsplit-2",
    "restore-tabbed-2",
    "restore-tabbed-3",
    "restore-stacked-2",
    "restore-3way-hsplit",
    "restore-3way-vsplit",
    "restore-tabs-in-hsplit",
    "restore-hsplit-in-tabs",
    "restore-vsplit-in-tabs",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attach_target_resolve() {
        assert_eq!(AttachTarget::SameWm.resolve(WmType::I3), WmType::I3);
        assert_eq!(AttachTarget::SameWm.resolve(WmType::Sway), WmType::Sway);
        assert_eq!(AttachTarget::CrossWm.resolve(WmType::I3), WmType::Sway);
        assert_eq!(AttachTarget::CrossWm.resolve(WmType::Sway), WmType::I3);
    }
}
