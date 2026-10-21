//! Container integration suite: headless Sway plus an sshd remote.
//!
//! Every test starts its own pair of containers, so tests never share
//! state. Session-level tests run for a local and a remote session.
//!
//! ```text
//!   tests/integration.sh                        # whole suite (builds musl first)
//!   tests/integration.sh scrollback -- --nocapture
//!   UPDATE_GOLDENS=1 tests/integration.sh       # rewrite the goldens
//! ```
#![allow(
    // Test helpers: every one can fail, and panics are the test outcome.
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    // Pixel arithmetic on u32 image coordinates in the screenshot code.
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

mod common;

mod detach_attach;
mod detach_attach_spec;
mod infrastructure;
mod layout_basic;
mod layout_nested;
mod layout_tabbed;
mod rebind;
mod sessions;
mod terminal;
mod window_command;
