//! Shared infrastructure of the container integration suite.

pub mod comparison_spec;
pub mod diff_image;
pub mod docker;
pub mod environment;
pub mod i3mux;
pub mod screenshot;

pub use anyhow::Result;
pub use comparison_spec::ComparisonSpec;
pub use environment::{assert_same_screen, wait_until, HostShell, TestEnvironment};
pub use i3mux::{terminal, Session};
