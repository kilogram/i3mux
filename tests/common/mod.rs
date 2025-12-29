// Common test utilities and infrastructure
pub mod comparison_spec;
pub mod diff_image;
pub mod docker;
pub mod environment;
pub mod i3mux;
pub mod network;
pub mod screenshot;
pub mod tier;

// Re-export commonly used types
pub use comparison_spec::ComparisonSpec;
pub use docker::{DualContainerManager, TestWmType};
pub use environment::{ColorScript, DualTestEnvironment, Session, TestEnvironment};
pub use tier::{is_full_matrix_enabled, AttachTarget, OpOrder, SessionType, WmType, ALL_SPECS};

// Re-export common external types
pub use anyhow::Result;
