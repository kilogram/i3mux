// Common test utilities and infrastructure
pub mod screenshot;
pub mod comparison_spec;
pub mod diff_image;
pub mod environment;
pub mod docker;
pub mod i3mux;
pub mod network;

// Re-export commonly used types
pub use screenshot::{compare_screenshots, ComparisonResult};
pub use comparison_spec::ComparisonSpec;
pub use diff_image::generate_diff_image;
pub use environment::{TestEnvironment, Session, ColorScript};

// Re-export common external types
pub use anyhow::Result;
pub use image::RgbaImage;
pub use std::time::Duration;
