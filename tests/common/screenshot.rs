//! Screenshot comparison: exact colors in declared regions, and a fuzzy
//! per-pixel match (a pixel matches if the same color is within
//! `tolerance_px` in the other image) everywhere else.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use image::{Rgba, RgbaImage};

use super::comparison_spec::{ComparisonSpec, ExactRegion};
use super::diff_image::{create_side_by_side, generate_diff_image, DiffType};

/// Per-channel difference still counted as the same color.
const CHANNEL_THRESHOLD: i32 = 3;

#[derive(Debug, Clone)]
pub struct ComparisonResult {
    pub total_diff_pixels: usize,
    pub diff_percentage: f64,
    pub diff_map: Vec<(u32, u32, DiffType)>,
    pub passed: bool,
}

/// Compare `actual` with `golden` under `spec`.
///
/// With exact regions, only those regions are checked, and the golden
/// itself must have the expected color there. Without, every pixel is
/// checked fuzzily. The result passes if at most
/// `max_diff_percentage` of the pixels differ.
pub fn compare_screenshots(
    golden: &RgbaImage,
    actual: &RgbaImage,
    spec: &ComparisonSpec,
) -> Result<ComparisonResult> {
    if golden.dimensions() != actual.dimensions() {
        bail!(
            "image sizes differ: golden {}x{}, actual {}x{}",
            golden.width(),
            golden.height(),
            actual.width(),
            actual.height()
        );
    }
    let mut diff_map = Vec::new();
    for region in &spec.exact_regions {
        check_exact_region(golden, actual, region, &mut diff_map)?;
    }
    if spec.exact_regions.is_empty() {
        check_fuzzy_match(
            golden,
            actual,
            spec.fuzzy_boundaries.tolerance_px,
            &mut diff_map,
        );
    }
    let total = f64::from(golden.width()) * f64::from(golden.height());
    let diff_percentage = diff_map.len() as f64 / total * 100.0;
    Ok(ComparisonResult {
        total_diff_pixels: diff_map.len(),
        diff_percentage,
        passed: diff_percentage <= spec.fuzzy_boundaries.max_diff_percentage,
        diff_map,
    })
}

fn check_exact_region(
    golden: &RgbaImage,
    actual: &RgbaImage,
    region: &ExactRegion,
    diff_map: &mut Vec<(u32, u32, DiffType)>,
) -> Result<()> {
    let [r, g, b] = region.expected_color;
    let expected = Rgba([r, g, b, 255]);
    let xs = region.x..(region.x + region.width).min(golden.width());
    let ys = region.y..(region.y + region.height).min(golden.height());
    for y in ys {
        for x in xs.clone() {
            let golden_pixel = *golden.get_pixel(x, y);
            if !same_color(golden_pixel, expected) {
                bail!("golden has {golden_pixel:?} at ({x}, {y}), the spec expects {expected:?}");
            }
            if !same_color(*actual.get_pixel(x, y), golden_pixel) {
                diff_map.push((x, y, DiffType::ColorMismatch));
            }
        }
    }
    Ok(())
}

fn check_fuzzy_match(
    golden: &RgbaImage,
    actual: &RgbaImage,
    tolerance_px: u32,
    diff_map: &mut Vec<(u32, u32, DiffType)>,
) {
    for (x, y, _) in golden.enumerate_pixels() {
        if !fuzzy_match(golden, actual, x, y, tolerance_px) {
            diff_map.push((x, y, DiffType::BoundaryMismatch));
        }
    }
}

/// Whether the golden pixel at (x, y) appears in `actual` within
/// `tolerance` pixels in each direction.
fn fuzzy_match(golden: &RgbaImage, actual: &RgbaImage, x: u32, y: u32, tolerance: u32) -> bool {
    let wanted = *golden.get_pixel(x, y);
    if same_color(*actual.get_pixel(x, y), wanted) {
        return true;
    }
    let xs = x.saturating_sub(tolerance)..=(x + tolerance).min(actual.width() - 1);
    let ys = y.saturating_sub(tolerance)..=(y + tolerance).min(actual.height() - 1);
    ys.flat_map(|cy| xs.clone().map(move |cx| (cx, cy)))
        .any(|(cx, cy)| same_color(*actual.get_pixel(cx, cy), wanted))
}

/// Equal up to [`CHANNEL_THRESHOLD`] per channel (compression, dithering).
fn same_color(a: Rgba<u8>, b: Rgba<u8>) -> bool {
    a.0.iter()
        .zip(b.0.iter())
        .all(|(p, q)| (i32::from(*p) - i32::from(*q)).abs() <= CHANNEL_THRESHOLD)
}

/// Save golden, actual, diff, side-by-side and a report under
/// `tests/test-output/failures/{test}/{timestamp}/`.
pub fn save_comparison_failure(
    test_name: &str,
    golden: &RgbaImage,
    actual: &RgbaImage,
    result: &ComparisonResult,
) -> Result<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/test-output/failures")
        .join(test_name.replace("::", "__"))
        .join(chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f").to_string());
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    golden.save(dir.join("golden.png"))?;
    actual.save(dir.join("actual.png"))?;
    let diff = generate_diff_image(golden, &result.diff_map);
    diff.save(dir.join("diff.png"))?;
    create_side_by_side(golden, actual, &diff).save(dir.join("comparison.png"))?;
    fs::write(
        dir.join("report.txt"),
        format!(
            "Test: {test_name}\nTotal diff pixels: {}\nDiff percentage: {:.2}%\nPassed: {}\n",
            result.total_diff_pixels, result.diff_percentage, result.passed
        ),
    )?;
    Ok(dir)
}

/// Load `tests/integration/golden/{name}`.
pub fn load_golden_image(name: impl AsRef<Path>) -> Result<RgbaImage> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/integration/golden")
        .join(name.as_ref());
    Ok(image::open(&path)
        .with_context(|| format!("cannot load golden image {}", path.display()))?
        .to_rgba8())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RED: Rgba<u8> = Rgba([255, 0, 0, 255]);
    const GREEN: Rgba<u8> = Rgba([0, 255, 0, 255]);

    #[test]
    fn same_color_allows_small_channel_noise_only() {
        assert!(same_color(RED, RED));
        assert!(same_color(RED, Rgba([255, 2, 1, 255])));
        assert!(!same_color(RED, Rgba([255, 10, 0, 255])));
    }

    #[test]
    fn fuzzy_match_finds_the_color_within_tolerance_only() {
        let golden = RgbaImage::from_pixel(100, 100, RED);
        let mut actual = RgbaImage::from_pixel(100, 100, GREEN);
        actual.put_pixel(52, 50, RED);
        assert!(fuzzy_match(&golden, &actual, 50, 50, 5));
        assert!(!fuzzy_match(&golden, &actual, 50, 50, 1));
    }

    #[test]
    fn fuzzy_match_at_the_image_edge_does_not_panic() {
        let golden = RgbaImage::from_pixel(10, 10, RED);
        let actual = RgbaImage::from_pixel(10, 10, GREEN);
        assert!(!fuzzy_match(&golden, &actual, 0, 0, 3));
        assert!(!fuzzy_match(&golden, &actual, 9, 9, 3));
    }

    #[test]
    fn identical_images_pass_with_no_diff() {
        let spec = ComparisonSpec::simple("test");
        let img = RgbaImage::from_pixel(100, 100, RED);
        let result = compare_screenshots(&img, &img, &spec).unwrap();
        assert_eq!(result.total_diff_pixels, 0);
        assert!(result.passed);
    }

    #[test]
    fn a_different_image_fails() {
        let spec = ComparisonSpec::simple("test");
        let golden = RgbaImage::from_pixel(100, 100, RED);
        let actual = RgbaImage::from_pixel(100, 100, GREEN);
        let result = compare_screenshots(&golden, &actual, &spec).unwrap();
        assert_eq!(result.total_diff_pixels, 100 * 100);
        assert!(!result.passed);
    }

    #[test]
    fn size_mismatch_is_an_error() {
        let spec = ComparisonSpec::simple("test");
        let result =
            compare_screenshots(&RgbaImage::new(100, 100), &RgbaImage::new(200, 200), &spec);
        assert!(result.is_err());
    }
}
