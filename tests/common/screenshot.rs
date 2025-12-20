// Screenshot comparison with exact color matching and fuzzy boundary matching

use anyhow::{Context, Result};
use image::{Rgba, RgbaImage};
use std::fs;
use std::path::{Path, PathBuf};

use super::comparison_spec::{ComparisonSpec, ExactRegion};
use super::diff_image::{create_side_by_side, generate_diff_image, DiffType};

#[derive(Debug, Clone)]
pub struct ComparisonResult {
    pub total_diff_pixels: usize,
    pub diff_percentage: f64,
    pub diff_map: Vec<(u32, u32, DiffType)>,
    pub passed: bool,
}


/// Compare two screenshots using a comparison specification
///
/// Returns Ok(ComparisonResult) with details about the comparison
pub fn compare_screenshots(
    golden: &RgbaImage,
    actual: &RgbaImage,
    spec: &ComparisonSpec,
) -> Result<ComparisonResult> {
    // Check dimensions match
    if golden.dimensions() != actual.dimensions() {
        anyhow::bail!(
            "Image dimensions mismatch: golden={}x{}, actual={}x{}",
            golden.width(),
            golden.height(),
            actual.width(),
            actual.height()
        );
    }

    let mut diff_pixels = Vec::new();

    // 1. Exact matching for color-filled regions
    for region in &spec.exact_regions {
        check_exact_region(golden, actual, region, &mut diff_pixels)?;
    }

    // 2. Fuzzy matching for entire image (if no exact regions specified)
    if spec.exact_regions.is_empty() {
        check_fuzzy_match(
            golden,
            actual,
            spec.fuzzy_boundaries.tolerance_px,
            &mut diff_pixels,
        );
    }

    // Calculate statistics
    let total_pixels = (golden.width() * golden.height()) as usize;
    let diff_percentage = (diff_pixels.len() as f64 / total_pixels as f64) * 100.0;

    let passed = diff_pixels.len() <= spec.fuzzy_boundaries.max_diff_pixels
        && diff_percentage <= spec.fuzzy_boundaries.max_diff_percentage;

    Ok(ComparisonResult {
        total_diff_pixels: diff_pixels.len(),
        diff_percentage,
        diff_map: diff_pixels,
        passed,
    })
}

/// Check exact color matching for a specific region
fn check_exact_region(
    golden: &RgbaImage,
    actual: &RgbaImage,
    region: &ExactRegion,
    diff_pixels: &mut Vec<(u32, u32, DiffType)>,
) -> Result<()> {
    let expected = Rgba([region.expected_color[0], region.expected_color[1], region.expected_color[2], 255]);

    for y in region.y..region.y + region.height {
        for x in region.x..region.x + region.width {
            if x >= golden.width() || y >= golden.height() {
                continue; // Skip out of bounds
            }

            let golden_pixel = golden.get_pixel(x, y);
            let actual_pixel = actual.get_pixel(x, y);

            // Check if golden pixel matches expected color
            if !pixels_match_exact(golden_pixel, &expected) {
                anyhow::bail!(
                    "Golden image doesn't match expected color at ({}, {}): expected {:?}, got {:?}",
                    x, y, expected, golden_pixel
                );
            }

            // Check if actual matches golden
            if !pixels_match_exact(actual_pixel, golden_pixel) {
                diff_pixels.push((x, y, DiffType::ColorMismatch));
            }
        }
    }

    Ok(())
}

/// Check fuzzy matching for all pixels
fn check_fuzzy_match(
    golden: &RgbaImage,
    actual: &RgbaImage,
    tolerance_px: u32,
    diff_pixels: &mut Vec<(u32, u32, DiffType)>,
) {
    let (width, height) = golden.dimensions();

    for y in 0..height {
        for x in 0..width {
            if !fuzzy_boundary_match(golden, actual, x, y, tolerance_px) {
                diff_pixels.push((x, y, DiffType::BoundaryMismatch));
            }
        }
    }
}

/// Check if a pixel matches within a tolerance radius (±tolerance_px)
fn fuzzy_boundary_match(
    golden: &RgbaImage,
    actual: &RgbaImage,
    x: u32,
    y: u32,
    tolerance: u32,
) -> bool {
    let golden_pixel = golden.get_pixel(x, y);

    // Check exact match first (fast path)
    let actual_pixel = actual.get_pixel(x, y);
    if pixels_match_exact(golden_pixel, actual_pixel) {
        return true;
    }

    // Check within tolerance radius
    let tolerance_i32 = tolerance as i32;
    for dy in -tolerance_i32..=tolerance_i32 {
        for dx in -tolerance_i32..=tolerance_i32 {
            let check_x = x as i32 + dx;
            let check_y = y as i32 + dy;

            if check_x < 0 || check_y < 0 {
                continue;
            }

            let check_x = check_x as u32;
            let check_y = check_y as u32;

            if check_x >= actual.width() || check_y >= actual.height() {
                continue;
            }

            let actual_pixel = actual.get_pixel(check_x, check_y);
            if pixels_match_exact(golden_pixel, actual_pixel) {
                return true;
            }
        }
    }

    false
}

/// Check if two pixels match (allowing small variance for compression artifacts)
fn pixels_match_exact(p1: &Rgba<u8>, p2: &Rgba<u8>) -> bool {
    const THRESHOLD: i32 = 3;

    (p1[0] as i32 - p2[0] as i32).abs() <= THRESHOLD
        && (p1[1] as i32 - p2[1] as i32).abs() <= THRESHOLD
        && (p1[2] as i32 - p2[2] as i32).abs() <= THRESHOLD
        && (p1[3] as i32 - p2[3] as i32).abs() <= THRESHOLD
}

/// Save comparison failure artifacts
pub fn save_comparison_failure(
    test_name: &str,
    golden: &RgbaImage,
    actual: &RgbaImage,
    result: &ComparisonResult,
) -> Result<PathBuf> {
    let output_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/test-output/failures")
        .join(test_name)
        .join(chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string());

    fs::create_dir_all(&output_dir)
        .with_context(|| format!("Failed to create output directory: {}", output_dir.display()))?;

    // Save images
    golden.save(output_dir.join("golden.png"))?;
    actual.save(output_dir.join("actual.png"))?;

    // Generate and save diff image
    let diff = generate_diff_image(golden, actual, &result.diff_map);
    diff.save(output_dir.join("diff.png"))?;

    // Generate and save side-by-side comparison
    let side_by_side = create_side_by_side(golden, actual, &diff);
    side_by_side.save(output_dir.join("comparison.png"))?;

    // Write text report
    let report = format!(
        "Test: {}\nTotal diff pixels: {}\nDiff percentage: {:.2}%\nPassed: {}\n",
        test_name, result.total_diff_pixels, result.diff_percentage, result.passed
    );
    fs::write(output_dir.join("report.txt"), report)?;

    Ok(output_dir)
}

/// Load a golden image
pub fn load_golden_image<P: AsRef<Path>>(name: P) -> Result<RgbaImage> {
    let golden_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/integration/golden")
        .join(name.as_ref());

    image::open(&golden_path)
        .with_context(|| format!("Failed to load golden image: {}", golden_path.display()))?
        .to_rgba8()
        .pipe(Ok)
}

// Helper trait for method chaining
trait Pipe: Sized {
    fn pipe<F, R>(self, f: F) -> R
    where
        F: FnOnce(Self) -> R,
    {
        f(self)
    }
}

impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pixels_match_exact() {
        let p1 = Rgba([255, 0, 0, 255]);
        let p2 = Rgba([255, 0, 0, 255]);
        assert!(pixels_match_exact(&p1, &p2));

        // Allow small variance
        let p3 = Rgba([255, 2, 1, 255]);
        assert!(pixels_match_exact(&p1, &p3));

        // Reject large variance
        let p4 = Rgba([255, 10, 0, 255]);
        assert!(!pixels_match_exact(&p1, &p4));
    }

    #[test]
    fn test_fuzzy_boundary_match() {
        let mut golden = RgbaImage::new(100, 100);
        let mut actual = RgbaImage::new(100, 100);

        // Fill with different colors
        for pixel in golden.pixels_mut() {
            *pixel = Rgba([255, 0, 0, 255]);
        }
        for pixel in actual.pixels_mut() {
            *pixel = Rgba([0, 255, 0, 255]);
        }

        // Place matching pixel at offset within tolerance
        actual.put_pixel(52, 50, Rgba([255, 0, 0, 255]));

        // Should match due to fuzzy tolerance
        assert!(fuzzy_boundary_match(&golden, &actual, 50, 50, 5));

        // Should not match if too far
        assert!(!fuzzy_boundary_match(&golden, &actual, 50, 50, 1));
    }

    #[test]
    fn test_compare_identical_images() {
        let spec = ComparisonSpec::simple("test");
        let img = RgbaImage::from_pixel(100, 100, Rgba([255, 0, 0, 255]));

        let result = compare_screenshots(&img, &img, &spec).unwrap();

        assert_eq!(result.total_diff_pixels, 0);
        assert_eq!(result.diff_percentage, 0.0);
        assert!(result.passed);
    }

    #[test]
    fn test_compare_dimension_mismatch() {
        let spec = ComparisonSpec::simple("test");
        let img1 = RgbaImage::new(100, 100);
        let img2 = RgbaImage::new(200, 200);

        let result = compare_screenshots(&img1, &img2, &spec);
        assert!(result.is_err());
    }
}
