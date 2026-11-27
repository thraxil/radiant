//! Image decoding, scaling and encoding.
//!
//! This module is purely CPU-bound and synchronous. Callers run it on a blocking thread.
//!
//! * [`sniff`] works out the format from the bytes (never trusting a client's `Content-Type`).
//! * [`probe`] reads only the header, enforces the input pixel limit, and reports the dimensions
//!   after EXIF orientation is applied.
//! * [`render`] decodes, orients, resizes and encodes a variant.
//!
//! Static images only for now. Animated inputs are rendered from their first frame
//! (animation support is milestone M3).

mod encode;
mod resize;

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageError, ImageReader};

use crate::format::Format;
use crate::spec::SizeSpec;

/// Resource limits applied while decoding and rendering.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Maximum `width * height` of a decoded source image.
    pub max_input_pixels: u64,
    /// Maximum width *and* height of a rendered output.
    pub max_output_dimension: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_pixels: 100_000_000,
            max_output_dimension: 4096,
        }
    }
}

/// Encoder settings. Keep these fixed for the life of a deployment: changing them changes the
/// bytes of newly rendered variants, though nothing breaks.
#[derive(Clone, Copy, Debug)]
pub struct EncodeSettings {
    /// 1–100.
    pub jpeg_quality: u8,
    /// 0–100 (lossy).
    pub webp_quality: f32,
}

impl Default for EncodeSettings {
    fn default() -> Self {
        Self {
            jpeg_quality: 90,
            webp_quality: 85.0,
        }
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum ImagingError {
    #[error("unsupported or unrecognised image format")]
    Unsupported,
    #[error("source image is {width}x{height}, over the input pixel limit")]
    InputTooLarge { width: u32, height: u32 },
    #[error("output would be {width}x{height}, over the output dimension limit")]
    OutputTooLarge { width: u32, height: u32 },
    #[error("could not decode image: {0}")]
    Decode(String),
    #[error("could not resize image: {0}")]
    Resize(String),
    #[error("could not encode image: {0}")]
    Encode(String),
}

/// What [`probe`] learns from an image header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageInfo {
    pub format: Format,
    /// Width after EXIF orientation is applied.
    pub width: u32,
    /// Height after EXIF orientation is applied.
    pub height: u32,
}

/// Detects the image format from magic bytes.
pub fn sniff(bytes: &[u8]) -> Option<Format> {
    match image::guess_format(bytes).ok()? {
        image::ImageFormat::Jpeg => Some(Format::Jpeg),
        image::ImageFormat::Png => Some(Format::Png),
        image::ImageFormat::Gif => Some(Format::Gif),
        image::ImageFormat::WebP => Some(Format::Webp),
        _ => None,
    }
}

fn image_format(f: Format) -> image::ImageFormat {
    match f {
        Format::Jpeg => image::ImageFormat::Jpeg,
        Format::Png => image::ImageFormat::Png,
        Format::Gif => image::ImageFormat::Gif,
        Format::Webp => image::ImageFormat::WebP,
    }
}

fn decode_error(e: ImageError) -> ImagingError {
    ImagingError::Decode(e.to_string())
}

/// A decoder whose header has been read and checked against the limits, but whose pixels have
/// not been decoded yet.
struct Header<D> {
    decoder: D,
    orientation: image::metadata::Orientation,
    info: ImageInfo,
}

fn read_header<'a>(
    bytes: &'a [u8],
    format: Format,
    limits: &Limits,
) -> Result<Header<impl ImageDecoder + 'a>, ImagingError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), image_format(format));
    // Pixel limits are enforced below. image's own allocation limit (default 512 MiB) stays as
    // a backstop.
    reader.limits(image::Limits::default());
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > limits.max_input_pixels {
        return Err(ImagingError::InputTooLarge { width, height });
    }
    let orientation = decoder.orientation().map_err(decode_error)?;
    let (width, height) = if swaps_axes(orientation) {
        (height, width)
    } else {
        (width, height)
    };
    Ok(Header {
        decoder,
        orientation,
        info: ImageInfo {
            format,
            width,
            height,
        },
    })
}

fn swaps_axes(o: image::metadata::Orientation) -> bool {
    use image::metadata::Orientation::*;
    matches!(o, Rotate90 | Rotate270 | Rotate90FlipH | Rotate270FlipH)
}

/// Sniffs the format and reads the header without decoding pixels.
pub fn probe(bytes: &[u8], limits: &Limits) -> Result<ImageInfo, ImagingError> {
    let format = sniff(bytes).ok_or(ImagingError::Unsupported)?;
    Ok(read_header(bytes, format, limits)?.info)
}

/// Output dimensions for `spec` applied to a source of `width`×`height` (already oriented),
/// checked against the limits. Callers can use this to reject a request before fetching or
/// decoding anything.
///
/// `full` is exempt from the output dimension limit. It can never be larger than the source,
/// and the input pixel limit already bounds the source.
pub fn planned_output(
    spec: &SizeSpec,
    width: u32,
    height: u32,
    limits: &Limits,
) -> Result<(u32, u32), ImagingError> {
    let (w, h) = spec.target_dimensions(width, height);
    if !spec.is_full() && (w > limits.max_output_dimension || h > limits.max_output_dimension) {
        return Err(ImagingError::OutputTooLarge {
            width: w,
            height: h,
        });
    }
    Ok((w, h))
}

/// Renders `bytes` (an image in `src_format`) at `spec`, encoded as `out_format`.
///
/// For the same inputs and settings, the output is byte-for-byte identical.
pub fn render(
    bytes: &[u8],
    src_format: Format,
    spec: &SizeSpec,
    out_format: Format,
    limits: &Limits,
    settings: &EncodeSettings,
) -> Result<Vec<u8>, ImagingError> {
    let header = read_header(bytes, src_format, limits)?;
    let (w, h) = planned_output(spec, header.info.width, header.info.height, limits)?;

    let mut img = DynamicImage::from_decoder(header.decoder).map_err(decode_error)?;
    img.apply_orientation(header.orientation);
    let img = resize::normalize(img);
    let img = resize::resize(img, spec, w, h)?;
    encode::encode(&img, out_format, settings)
}

#[cfg(test)]
mod tests;
