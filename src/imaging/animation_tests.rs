use std::io::Cursor;
use std::num::NonZeroU32;

use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::metadata::LoopCount;
use image::{AnimationDecoder, Delay, Frame, Rgba, RgbaImage};

use super::*;

const ANIMATED: [Format; 3] = [Format::Gif, Format::Png, Format::Webp];
const W: u32 = 60;
const H: u32 = 40;

fn spec(s: &str) -> SizeSpec {
    s.parse().unwrap()
}

fn solid(c: [u8; 4]) -> RgbaImage {
    RgbaImage::from_pixel(W, H, Rgba(c))
}

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const CLEAR: [u8; 4] = [0, 0, 0, 0];

/// Encodes a test animation without going through radiant's own encoders.
fn make(format: Format, frames: &[([u8; 4], u32)], loops: LoopCount) -> Vec<u8> {
    let mut out = Vec::new();
    match format {
        Format::Gif => {
            let mut enc = GifEncoder::new(&mut out);
            enc.set_repeat(match loops {
                LoopCount::Infinite => Repeat::Infinite,
                LoopCount::Finite(n) => Repeat::Finite(n.get() as u16),
            })
            .unwrap();
            enc.encode_frames(frames.iter().map(|(c, ms)| {
                Frame::from_parts(solid(*c), 0, 0, Delay::from_numer_denom_ms(*ms, 1))
            }))
            .unwrap();
        }
        Format::Png => {
            let mut enc = png::Encoder::new(&mut out, W, H);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let plays = match loops {
                LoopCount::Infinite => 0,
                LoopCount::Finite(n) => n.get(),
            };
            enc.set_animated(frames.len() as u32, plays).unwrap();
            let mut w = enc.write_header().unwrap();
            for (c, ms) in frames {
                w.set_frame_delay(*ms as u16, 1000).unwrap();
                w.set_blend_op(png::BlendOp::Source).unwrap();
                w.write_image_data(solid(*c).as_raw()).unwrap();
            }
            w.finish().unwrap();
        }
        Format::Webp => {
            // libwebp directly: the `webp` crate's encoder gets the last frame's duration
            // wrong (see libwebp.rs).
            let imgs: Vec<(RgbaImage, u32)> =
                frames.iter().map(|(c, ms)| (solid(*c), *ms)).collect();
            out = super::libwebp::encode_animation(W, H, &imgs, plays(&loops), 90.0).unwrap();
        }
        Format::Jpeg => unreachable!(),
    }
    out
}

/// `LoopCount` has neither `Debug` nor `PartialEq`. 0 means infinite, as in GIF/APNG/WebP.
fn plays(l: &LoopCount) -> u32 {
    match l {
        LoopCount::Infinite => 0,
        LoopCount::Finite(n) => n.get(),
    }
}

struct Decoded {
    frames: Vec<(RgbaImage, u32)>,
    loops: LoopCount,
}

fn read<'a>(d: impl AnimationDecoder<'a>) -> Decoded {
    let loops = d.loop_count();
    let frames = d
        .into_frames()
        .map(|f| {
            let f = f.unwrap();
            let (n, den) = f.delay().numer_denom_ms();
            (f.into_buffer(), n / den)
        })
        .collect();
    Decoded { frames, loops }
}

/// Decodes an animation with image's decoders (independent of radiant's code).
fn decode_anim(bytes: &[u8], format: Format) -> Decoded {
    match format {
        Format::Gif => read(GifDecoder::new(Cursor::new(bytes)).unwrap()),
        Format::Png => {
            let d = PngDecoder::new(Cursor::new(bytes)).unwrap();
            assert!(d.is_apng().unwrap(), "expected APNG");
            read(d.apng().unwrap())
        }
        Format::Webp => {
            // Through the `webp` crate's decoder (libwebp), not image-webp, which composites
            // some animations wrongly (see libwebp.rs).
            let anim = webp::AnimDecoder::new(bytes).decode().unwrap();
            assert!(anim.len() > 1, "expected animated WebP");
            let mut prev = 0;
            let frames = (0..anim.len())
                .map(|i| {
                    let f = anim.get_frame(i).unwrap();
                    let img: image::DynamicImage = (&f).into();
                    let ms = (f.get_time_ms() - prev) as u32;
                    prev = f.get_time_ms();
                    (img.into_rgba8(), ms)
                })
                .collect();
            let loops = match anim.loop_count {
                0 => LoopCount::Infinite,
                n => LoopCount::Finite(NonZeroU32::new(n).unwrap()),
            };
            Decoded { frames, loops }
        }
        Format::Jpeg => unreachable!(),
    }
}

fn render_anim(bytes: &[u8], src: Format, s: &str, out: Format) -> Result<Vec<u8>, ImagingError> {
    render(
        bytes,
        src,
        &spec(s),
        out,
        &Limits::default(),
        &EncodeSettings::default(),
    )
}

/// Lossy formats (GIF palette, lossy WebP) shift colours a little.
fn assert_colour(p: &Rgba<u8>, expected: [u8; 4], what: &str) {
    for c in 0..4 {
        let d = (i16::from(p[c]) - i16::from(expected[c])).abs();
        assert!(d <= 40, "{what}: got {p:?}, expected about {expected:?}");
    }
}

fn three_frames() -> Vec<([u8; 4], u32)> {
    vec![(RED, 100), (GREEN, 200), (BLUE, 300)]
}

#[test]
fn every_animated_format_to_every_animated_format() {
    for loops in [
        LoopCount::Infinite,
        LoopCount::Finite(NonZeroU32::new(3).unwrap()),
    ] {
        for src in ANIMATED {
            let input = make(src, &three_frames(), loops);
            for out in ANIMATED {
                let what = format!("{src}->{out} plays={}", plays(&loops));
                let bytes = render_anim(&input, src, "30w", out).unwrap();
                assert_eq!(sniff(&bytes), Some(out), "{what}");
                let d = decode_anim(&bytes, out);
                assert_eq!(d.frames.len(), 3, "{what}: frame count");
                assert_eq!(plays(&d.loops), plays(&loops), "{what}: loop count");
                let delays: Vec<u32> = d.frames.iter().map(|(_, ms)| *ms).collect();
                assert_eq!(delays, vec![100, 200, 300], "{what}: delays");
                for ((img, _), (colour, _)) in d.frames.iter().zip(three_frames()) {
                    assert_eq!(img.dimensions(), (30, 20), "{what}");
                    assert_colour(img.get_pixel(15, 10), colour, &what);
                }
            }
        }
    }
}

#[test]
fn square_crop_and_upscale_apply_to_every_frame() {
    for src in ANIMATED {
        let input = make(src, &three_frames(), LoopCount::Infinite);
        for s in ["20s", "120h"] {
            let d = decode_anim(&render_anim(&input, src, s, src).unwrap(), src);
            let expected = spec(s).target_dimensions(W, H);
            assert_eq!(d.frames.len(), 3);
            assert!(
                d.frames.iter().all(|(f, _)| f.dimensions() == expected),
                "{src} {s}"
            );
        }
    }
}

#[test]
fn transparent_frames_stay_transparent() {
    // A fully transparent frame between two opaque ones must not show the frame before it.
    let frames = [(RED, 100), (CLEAR, 100), (BLUE, 100)];
    for src in ANIMATED {
        let input = make(src, &frames, LoopCount::Infinite);
        for out in ANIMATED {
            let d = decode_anim(&render_anim(&input, src, "30w", out).unwrap(), out);
            let what = format!("{src}->{out}");
            assert_colour(d.frames[0].0.get_pixel(15, 10), RED, &what);
            assert!(
                d.frames[1].0.get_pixel(15, 10)[3] < 20,
                "{what}: frame 2 should be transparent, got {:?}",
                d.frames[1].0.get_pixel(15, 10)
            );
            assert_colour(d.frames[2].0.get_pixel(15, 10), BLUE, &what);
        }
    }
}

#[test]
fn animation_to_jpeg_uses_first_frame() {
    for src in ANIMATED {
        let input = make(src, &three_frames(), LoopCount::Infinite);
        let out = render_anim(&input, src, "30w", Format::Jpeg).unwrap();
        let img = image::load_from_memory(&out).unwrap().into_rgba8();
        assert_eq!(img.dimensions(), (30, 20));
        assert_colour(img.get_pixel(15, 10), RED, &format!("{src}->jpg"));
    }
}

#[test]
fn single_frame_sources_stay_static() {
    for src in ANIMATED {
        let input = make(src, &[(GREEN, 100)], LoopCount::Infinite);
        for out in ANIMATED {
            let bytes = render_anim(&input, src, "30w", out).unwrap();
            let what = format!("{src}->{out}");
            match out {
                Format::Png => {
                    let d = PngDecoder::new(Cursor::new(&bytes)).unwrap();
                    assert!(!d.is_apng().unwrap(), "{what}: should be a plain PNG");
                }
                Format::Webp => {
                    let d = WebPDecoder::new(Cursor::new(&bytes)).unwrap();
                    assert!(!d.has_animation(), "{what}: should be a still WebP");
                }
                _ => assert_eq!(decode_anim(&bytes, out).frames.len(), 1, "{what}"),
            }
            let img = image::load_from_memory(&bytes).unwrap().into_rgba8();
            assert_colour(img.get_pixel(15, 10), GREEN, &what);
        }
    }
}

#[test]
fn animated_output_is_deterministic() {
    for src in ANIMATED {
        let input = make(src, &three_frames(), LoopCount::Infinite);
        for out in ANIMATED {
            let a = render_anim(&input, src, "25s", out).unwrap();
            let b = render_anim(&input, src, "25s", out).unwrap();
            assert!(a == b, "{src}->{out} not deterministic");
        }
    }
}

#[test]
fn frame_limits() {
    let input = make(Format::Gif, &three_frames(), LoopCount::Infinite);
    let render_with = |limits: Limits| {
        render(
            &input,
            Format::Gif,
            &spec("30w"),
            Format::Gif,
            &limits,
            &EncodeSettings::default(),
        )
    };

    let e = render_with(Limits {
        max_frames: 2,
        ..Limits::default()
    })
    .unwrap_err();
    assert!(matches!(e, ImagingError::TooManyFrames { limit: 2 }), "{e}");
    assert!(e.is_limit());
    assert!(
        render_with(Limits {
            max_frames: 3,
            ..Limits::default()
        })
        .is_ok()
    );

    // Source is 60x40 = 2400 px per frame, so 3 frames need 7200.
    let e = render_with(Limits {
        max_animation_pixels: 7199,
        ..Limits::default()
    })
    .unwrap_err();
    assert!(matches!(e, ImagingError::AnimationTooLarge { .. }), "{e}");
    assert!(e.is_limit());
    assert!(
        render_with(Limits {
            max_animation_pixels: 7200,
            ..Limits::default()
        })
        .is_ok()
    );

    // An upscaled output counts against the same budget: 120x80 = 9600 px per frame.
    let e = render(
        &input,
        Format::Gif,
        &spec("120w"),
        Format::Gif,
        &Limits {
            max_animation_pixels: 9600 * 3 - 1,
            ..Limits::default()
        },
        &EncodeSettings::default(),
    )
    .unwrap_err();
    assert!(matches!(e, ImagingError::AnimationTooLarge { .. }), "{e}");

    // Converting to a still format only decodes the first frame, so frame limits don't apply.
    assert!(
        render(
            &input,
            Format::Gif,
            &spec("30w"),
            Format::Jpeg,
            &Limits {
                max_frames: 1,
                ..Limits::default()
            },
            &EncodeSettings::default()
        )
        .is_ok()
    );
}
