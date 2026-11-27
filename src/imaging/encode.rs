use image::codecs::gif::GifEncoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, RgbImage};

use super::{EncodeSettings, ImagingError};
use crate::format::Format;

/// NeuQuant speed for GIF palette quantisation: 1 is slowest and best, 30 is fastest.
/// 10 keeps large images fast with little visible difference.
const GIF_QUANTIZE_SPEED: i32 = 10;

fn encode_error(e: impl std::fmt::Display) -> ImagingError {
    ImagingError::Encode(e.to_string())
}

/// Encodes a normalised (RGB8 or RGBA8) image. Metadata is never written.
pub(super) fn encode(
    img: &DynamicImage,
    format: Format,
    settings: &EncodeSettings,
) -> Result<Vec<u8>, ImagingError> {
    let mut out = Vec::new();
    let (w, h) = (img.width(), img.height());
    match format {
        Format::Jpeg => {
            let rgb = flatten_on_white(img);
            JpegEncoder::new_with_quality(&mut out, settings.jpeg_quality)
                .write_image(rgb.as_raw(), w, h, ExtendedColorType::Rgb8)
                .map_err(encode_error)?;
        }
        Format::Png => {
            let (bytes, color) = raw(img);
            PngEncoder::new(&mut out)
                .write_image(bytes, w, h, color)
                .map_err(encode_error)?;
        }
        Format::Gif => {
            let rgba = img.to_rgba8();
            GifEncoder::new_with_speed(&mut out, GIF_QUANTIZE_SPEED)
                .encode(rgba.as_raw(), w, h, ExtendedColorType::Rgba8)
                .map_err(encode_error)?;
        }
        Format::Webp => {
            let (bytes, color) = raw(img);
            let encoder = match color {
                ExtendedColorType::Rgba8 => webp::Encoder::from_rgba(bytes, w, h),
                _ => webp::Encoder::from_rgb(bytes, w, h),
            };
            let mem = encoder
                .encode_simple(false, settings.webp_quality)
                .map_err(|e| encode_error(format!("{e:?}")))?;
            out.extend_from_slice(&mem);
        }
    }
    Ok(out)
}

fn raw(img: &DynamicImage) -> (&[u8], ExtendedColorType) {
    match img {
        DynamicImage::ImageRgba8(buf) => (buf.as_raw(), ExtendedColorType::Rgba8),
        DynamicImage::ImageRgb8(buf) => (buf.as_raw(), ExtendedColorType::Rgb8),
        _ => unreachable!("encode() takes a normalised image"),
    }
}

/// JPEG has no alpha channel, so composite the image onto a white background.
fn flatten_on_white(img: &DynamicImage) -> RgbImage {
    match img {
        DynamicImage::ImageRgba8(rgba) => {
            let mut rgb = RgbImage::new(rgba.width(), rgba.height());
            for (dst, src) in rgb.pixels_mut().zip(rgba.pixels()) {
                let a = u32::from(src[3]);
                for c in 0..3 {
                    // src*a + 255*(1-a), rounded.
                    dst[c] = ((u32::from(src[c]) * a + 255 * (255 - a) + 127) / 255) as u8;
                }
            }
            rgb
        }
        other => other.to_rgb8(),
    }
}
