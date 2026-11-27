//! Animated GIF, APNG and WebP: decode, resize every frame, re-encode.
//!
//! The decoders hand us frames already composited onto the full canvas (disposal and blending
//! applied), so every frame we hold is a complete RGBA image. Every frame is encoded the same
//! way: full canvas, replacing the previous frame. That costs some file size compared with delta
//! frames, but frames with transparency always come out right.

use std::io::Cursor;

use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::codecs::png::PngDecoder;
use image::metadata::LoopCount;
use image::{AnimationDecoder, Delay, DynamicImage, Frame, ImageDecoder, RgbaImage};

use super::{
    EncodeSettings, ImagingError, Limits, decode_error, encode_error, libwebp, planned_output,
    resize,
};
use crate::format::Format;
use crate::spec::SizeSpec;

/// A resized animation ready to encode.
pub(super) struct Animation {
    pub width: u32,
    pub height: u32,
    /// Each frame with its duration in milliseconds.
    pub frames: Vec<(RgbaImage, u32)>,
    /// 0 means loop forever (the GIF, APNG and WebP convention).
    pub loop_count: u32,
}

impl Format {
    pub(crate) fn supports_animation(self) -> bool {
        matches!(self, Format::Gif | Format::Png | Format::Webp)
    }
}

fn plays(l: LoopCount) -> u32 {
    match l {
        LoopCount::Infinite => 0,
        LoopCount::Finite(n) => n.get(),
    }
}

fn delay_ms(delay: Delay) -> u32 {
    let (num, den) = delay.numer_denom_ms();
    (num + den / 2) / den.max(1)
}

/// Frames from image's `AnimationDecoder`, as `(frame, ms)`.
fn image_frames<'a>(
    d: impl AnimationDecoder<'a>,
) -> impl Iterator<Item = Result<(RgbaImage, u32), ImagingError>> {
    d.into_frames().map(|f| {
        let f = f.map_err(decode_error)?;
        let ms = delay_ms(f.delay());
        Ok((f.into_buffer(), ms))
    })
}

/// Decodes and resizes an animated source. Returns `None` when the source isn't actually
/// animated (a single frame, or a plain PNG or WebP), and the caller then takes the static path.
pub(super) fn decode_and_resize(
    bytes: &[u8],
    format: Format,
    spec: &SizeSpec,
    limits: &Limits,
) -> Result<Option<Animation>, ImagingError> {
    match format {
        Format::Gif => {
            let d = GifDecoder::new(Cursor::new(bytes)).map_err(decode_error)?;
            let (dims, loops) = (d.dimensions(), plays(d.loop_count()));
            resize_frames(dims, None, loops, image_frames(d), spec, limits)
        }
        Format::Png => {
            let d = PngDecoder::new(Cursor::new(bytes)).map_err(decode_error)?;
            if !d.is_apng().map_err(decode_error)? {
                return Ok(None);
            }
            let dims = d.dimensions();
            let d = d.apng().map_err(decode_error)?;
            let loops = plays(d.loop_count());
            resize_frames(dims, None, loops, image_frames(d), spec, limits)
        }
        Format::Webp => {
            // A still WebP has no ANIM chunk, and libwebp's animation decoder also reads those
            // as one frame, so one code path covers both.
            let mut d = libwebp::AnimDecoder::new(bytes).map_err(ImagingError::Decode)?;
            if d.frame_count() <= 1 {
                return Ok(None);
            }
            let (dims, count, loops) = (d.canvas(), d.frame_count(), d.loop_count());
            let frames = std::iter::from_fn(move || {
                d.next_frame().map_err(ImagingError::Decode).transpose()
            });
            resize_frames(dims, Some(count), loops, frames, spec, limits)
        }
        Format::Jpeg => Ok(None),
    }
}

/// Resizes frames one at a time while enforcing the limits. Only resized frames are kept.
/// `known_count` (WebP stores it in the header) lets the limits fail before any decoding.
fn resize_frames(
    (src_w, src_h): (u32, u32),
    known_count: Option<u32>,
    loop_count: u32,
    frames: impl Iterator<Item = Result<(RgbaImage, u32), ImagingError>>,
    spec: &SizeSpec,
    limits: &Limits,
) -> Result<Option<Animation>, ImagingError> {
    let src_pixels = u64::from(src_w) * u64::from(src_h);
    if src_pixels > limits.max_input_pixels {
        return Err(ImagingError::InputTooLarge {
            width: src_w,
            height: src_h,
        });
    }
    let (w, h) = planned_output(spec, src_w, src_h, limits)?;
    let per_frame = src_pixels.max(u64::from(w) * u64::from(h));
    let check = |n: u64| {
        if n > u64::from(limits.max_frames) {
            Err(ImagingError::TooManyFrames {
                limit: limits.max_frames,
            })
        } else if n * per_frame > limits.max_animation_pixels {
            // Bounds decode CPU (source pixels) and memory held for resized frames (output).
            Err(ImagingError::AnimationTooLarge {
                limit: limits.max_animation_pixels,
            })
        } else {
            Ok(())
        }
    };
    if let Some(n) = known_count {
        check(u64::from(n))?;
    }

    let mut out = Vec::new();
    for (i, frame) in frames.enumerate() {
        check(i as u64 + 1)?;
        let (img, ms) = frame?;
        let img = resize::resize(DynamicImage::ImageRgba8(img), spec, w, h)?;
        out.push((img.into_rgba8(), ms));
    }
    if out.len() <= 1 {
        return Ok(None);
    }
    Ok(Some(Animation {
        width: w,
        height: h,
        frames: out,
        loop_count,
    }))
}

pub(super) fn encode(
    anim: Animation,
    format: Format,
    settings: &EncodeSettings,
) -> Result<Vec<u8>, ImagingError> {
    match format {
        Format::Gif => encode_gif(anim),
        Format::Png => encode_apng(anim),
        Format::Webp => libwebp::encode_animation(
            anim.width,
            anim.height,
            &anim.frames,
            anim.loop_count,
            settings.webp_quality,
        )
        .map_err(ImagingError::Encode),
        Format::Jpeg => unreachable!("JPEG cannot be animated"),
    }
}

fn encode_gif(anim: Animation) -> Result<Vec<u8>, ImagingError> {
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new_with_speed(&mut out, super::encode::GIF_QUANTIZE_SPEED);
        enc.set_repeat(match anim.loop_count {
            0 => Repeat::Infinite,
            n => Repeat::Finite(u16::try_from(n).unwrap_or(u16::MAX)),
        })
        .map_err(encode_error)?;
        enc.encode_frames(
            anim.frames
                .into_iter()
                .map(|(img, ms)| Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1))),
        )
        .map_err(encode_error)?;
    }
    Ok(out)
}

fn encode_apng(anim: Animation) -> Result<Vec<u8>, ImagingError> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, anim.width, anim.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_animated(anim.frames.len() as u32, anim.loop_count)
        .map_err(encode_error)?;
    let mut writer = enc.write_header().map_err(encode_error)?;
    for (img, ms) in &anim.frames {
        // APNG delays are a u16 fraction of a second, so use milliseconds over 1000.
        writer
            .set_frame_delay(u16::try_from(*ms).unwrap_or(u16::MAX), 1000)
            .map_err(encode_error)?;
        writer
            .set_dispose_op(png::DisposeOp::None)
            .map_err(encode_error)?;
        // Source, not Over: each frame replaces the canvas, transparent pixels included.
        writer
            .set_blend_op(png::BlendOp::Source)
            .map_err(encode_error)?;
        writer
            .write_image_data(img.as_raw())
            .map_err(encode_error)?;
    }
    writer.finish().map_err(encode_error)?;
    Ok(out)
}
