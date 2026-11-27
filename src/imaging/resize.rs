use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::DynamicImage;

use super::ImagingError;
use crate::spec::SizeSpec;

/// Converts to 8-bit RGB, or RGBA when the image has alpha. Every output format is 8-bit, and
/// using only two pixel layouts keeps resizing and encoding simple.
pub(super) fn normalize(img: DynamicImage) -> DynamicImage {
    match img {
        DynamicImage::ImageRgb8(_) | DynamicImage::ImageRgba8(_) => img,
        img if img.color().has_alpha() => DynamicImage::ImageRgba8(img.into_rgba8()),
        img => DynamicImage::ImageRgb8(img.into_rgb8()),
    }
}

/// Resizes a normalised image to exactly `w`×`h`.
///
/// `Square` specs centre-crop the source to the destination's aspect ratio. Every other spec
/// already keeps the source aspect ratio (to within rounding), so the whole source is used.
pub(super) fn resize(
    img: DynamicImage,
    spec: &SizeSpec,
    w: u32,
    h: u32,
) -> Result<DynamicImage, ImagingError> {
    if (img.width(), img.height()) == (w, h) {
        return Ok(img);
    }
    let mut dst = match img {
        DynamicImage::ImageRgba8(_) => DynamicImage::new_rgba8(w, h),
        _ => DynamicImage::new_rgb8(w, h),
    };
    let mut options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    if matches!(spec, SizeSpec::Square(_)) {
        options = options.fit_into_destination(Some((0.5, 0.5)));
    }
    Resizer::new()
        .resize(&img, &mut dst, &options)
        .map_err(|e| ImagingError::Resize(e.to_string()))?;
    Ok(dst)
}
