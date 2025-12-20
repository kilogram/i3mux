// Common test utilities and infrastructure
pub mod comparison_spec;
pub mod diff_image;
pub mod docker;
pub mod environment;
pub mod i3mux;
pub mod network;
pub mod screenshot;

// Re-export commonly used types
pub use comparison_spec::ComparisonSpec;
pub use environment::{ColorScript, Session, TestEnvironment};

// Re-export common external types
pub use anyhow::Result;
