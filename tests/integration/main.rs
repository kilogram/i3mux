// Integration tests for i3mux
// Run with: cargo test --test integration
// Run including remote tests: cargo test --test integration -- --include-ignored
// Update goldens with: UPDATE_GOLDENS=1 cargo test --test integration

mod common;

mod detach_attach;
mod edge_cases;
mod infrastructure;
mod layout_basic;
mod layout_multiway;
mod layout_nested;
mod layout_tabbed;
mod network;

use common::*;

// ==================== Parameterized Session Types ====================
// Tests run for both local and remote sessions (remote tests are #[ignore] by default)

/// Helper to get workspace number for a session type
pub fn workspace_for_session(base: u32, session: &Session) -> String {
    match session {
        Session::Local => base.to_string(),
        Session::Remote(_) => (base + 100).to_string(), // Offset remote tests to avoid conflicts
    }
}

/// Helper to determine if test should be ignored (for remote sessions)
pub fn should_ignore_session(session: &Session) -> bool {
    matches!(session, Session::Remote(_))
}
