// Visual diff generation for failed screenshot comparisons

use image::{Rgba, RgbaImage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffType {
    ColorMismatch,
    BoundaryMismatch,
}

/// Generate a visual diff image highlighting differences
/// - Red pixels: color mismatches
/// - Yellow pixels: boundary mismatches
/// - Gray pixels: matching pixels (dimmed for clarity)
pub fn generate_diff_image(
    golden: &RgbaImage,
    _actual: &RgbaImage,
    diff_pixels: &[(u32, u32, DiffType)],
) -> RgbaImage {
    let (width, height) = golden.dimensions();
    let mut diff = RgbaImage::new(width, height);

    // Create a set for fast lookup
    let diff_set: std::collections::HashMap<(u32, u32), DiffType> = diff_pixels
        .iter()
        .map(|(x, y, t)| ((*x, *y), *t))
        .collect();

    for y in 0..height {
        for x in 0..width {
            let pixel = if let Some(diff_type) = diff_set.get(&(x, y)) {
                // Highlight diff pixels
                match diff_type {
                    DiffType::ColorMismatch => Rgba([255, 0, 0, 255]),      // Red
                    DiffType::BoundaryMismatch => Rgba([255, 255, 0, 255]), // Yellow
                }
            } else {
                // Dim matching pixels for contrast
                let golden_pixel = golden.get_pixel(x, y);
                Rgba([
                    golden_pixel[0] / 2,
                    golden_pixel[1] / 2,
                    golden_pixel[2] / 2,
                    255,
                ])
            };

            diff.put_pixel(x, y, pixel);
        }
    }

    diff
}

/// Create a side-by-side comparison image: Golden | Actual | Diff
pub fn create_side_by_side(
    golden: &RgbaImage,
    actual: &RgbaImage,
    diff: &RgbaImage,
) -> RgbaImage {
    let (width, height) = golden.dimensions();
    let mut combined = RgbaImage::new(width * 3 + 20, height); // 10px padding between each

    // Fill background with white
    for pixel in combined.pixels_mut() {
        *pixel = Rgba([255, 255, 255, 255]);
    }

    // Copy golden
    for y in 0..height {
        for x in 0..width {
            combined.put_pixel(x, y, *golden.get_pixel(x, y));
        }
    }

    // Copy actual (with 10px padding)
    for y in 0..height {
        for x in 0..width {
            combined.put_pixel(x + width + 10, y, *actual.get_pixel(x, y));
        }
    }

    // Copy diff (with 10px padding)
    for y in 0..height {
        for x in 0..width {
            combined.put_pixel(x + width * 2 + 20, y, *diff.get_pixel(x, y));
        }
    }

    combined
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_diff_image() {
        let golden = RgbaImage::from_pixel(100, 100, Rgba([255, 0, 0, 255]));
        let actual = RgbaImage::from_pixel(100, 100, Rgba([0, 255, 0, 255]));
        let diff_pixels = vec![(50, 50, DiffType::ColorMismatch)];

        let diff = generate_diff_image(&golden, &actual, &diff_pixels);

        // Check that diff pixel is red
        assert_eq!(diff.get_pixel(50, 50), &Rgba([255, 0, 0, 255]));

        // Check that non-diff pixel is dimmed
        let dimmed = diff.get_pixel(0, 0);
        assert_eq!(dimmed[0], 255 / 2);
    }

    #[test]
    fn test_side_by_side_dimensions() {
        let golden = RgbaImage::new(100, 50);
        let actual = RgbaImage::new(100, 50);
        let diff = RgbaImage::new(100, 50);

        let combined = create_side_by_side(&golden, &actual, &diff);

        assert_eq!(combined.width(), 100 * 3 + 20);
        assert_eq!(combined.height(), 50);
    }
}
