// TOML-based comparison specification for screenshot testing

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonSpec {
    pub name: String,
    #[serde(default)]
    pub exact_regions: Vec<ExactRegion>,
    #[serde(default)]
    pub fuzzy_boundaries: FuzzyBoundaries,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExactRegion {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub expected_color: [u8; 3],  // RGB
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuzzyBoundaries {
    #[serde(default = "default_tolerance")]
    pub tolerance_px: u32,
    #[serde(default = "default_max_diff_pixels")]
    pub max_diff_pixels: usize,
    #[serde(default = "default_max_diff_percentage")]
    pub max_diff_percentage: f64,
}

fn default_tolerance() -> u32 {
    5
}

fn default_max_diff_pixels() -> usize {
    1000
}

fn default_max_diff_percentage() -> f64 {
    1.0
}

impl Default for FuzzyBoundaries {
    fn default() -> Self {
        Self {
            tolerance_px: default_tolerance(),
            max_diff_pixels: default_max_diff_pixels(),
            max_diff_percentage: default_max_diff_percentage(),
        }
    }
}

impl ComparisonSpec {
    /// Load a comparison spec from a TOML file
    pub fn load<P: AsRef<Path>>(name: P) -> Result<Self> {
        let spec_path = Self::spec_path(name.as_ref())?;
        let contents = fs::read_to_string(&spec_path)
            .with_context(|| format!("Failed to read spec file: {}", spec_path.display()))?;

        let spec: ComparisonSpec = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse spec file: {}", spec_path.display()))?;

        Ok(spec)
    }

    /// Save a comparison spec to a TOML file
    pub fn save<P: AsRef<Path>>(&self, name: P) -> Result<()> {
        let spec_path = Self::spec_path(name.as_ref())?;

        // Create parent directory if it doesn't exist
        if let Some(parent) = spec_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create spec directory: {}", parent.display()))?;
        }

        let contents = toml::to_string_pretty(self)
            .context("Failed to serialize spec to TOML")?;

        fs::write(&spec_path, contents)
            .with_context(|| format!("Failed to write spec file: {}", spec_path.display()))?;

        Ok(())
    }

    /// Get the path to a spec file
    fn spec_path(name: &Path) -> Result<PathBuf> {
        let spec_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden/specs");

        let mut path = spec_dir.join(name);
        if path.extension().is_none() {
            path.set_extension("toml");
        }

        Ok(path)
    }

    /// Create a simple spec for basic layouts
    pub fn simple(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            exact_regions: Vec::new(),
            fuzzy_boundaries: FuzzyBoundaries::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let spec = ComparisonSpec::simple("test");
        assert_eq!(spec.fuzzy_boundaries.tolerance_px, 5);
        assert_eq!(spec.fuzzy_boundaries.max_diff_pixels, 1000);
        assert_eq!(spec.fuzzy_boundaries.max_diff_percentage, 1.0);
    }

    #[test]
    fn test_toml_roundtrip() {
        let spec = ComparisonSpec {
            name: "test-layout".to_string(),
            exact_regions: vec![
                ExactRegion {
                    x: 0,
                    y: 20,
                    width: 960,
                    height: 1060,
                    expected_color: [255, 0, 0],
                },
            ],
            fuzzy_boundaries: FuzzyBoundaries::default(),
        };

        let toml_str = toml::to_string(&spec).unwrap();
        let parsed: ComparisonSpec = toml::from_str(&toml_str).unwrap();

        assert_eq!(spec.name, parsed.name);
        assert_eq!(spec.exact_regions.len(), parsed.exact_regions.len());
    }
}
