use image::{DynamicImage, GenericImageView, Rgb, RgbImage, Rgba, RgbaImage};

use super::*;

fn spec(s: &str) -> SizeSpec {
    s.parse().unwrap()
}

/// A gradient so that resizing has real work to do.
fn gradient(w: u32, h: u32, alpha: bool) -> DynamicImage {
    let rgb = RgbImage::from_fn(w, h, |x, y| {
        Rgb([(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, 128])
    });
    if alpha {
        let mut rgba = DynamicImage::ImageRgb8(rgb).into_rgba8();
        for (x, _, p) in rgba.enumerate_pixels_mut() {
            p[3] = (x * 255 / w.max(1)) as u8;
        }
        DynamicImage::ImageRgba8(rgba)
    } else {
        DynamicImage::ImageRgb8(rgb)
    }
}

fn encoded(img: &DynamicImage, format: Format) -> Vec<u8> {
    encode::encode(img, format, &EncodeSettings::default()).unwrap()
}

fn decode(bytes: &[u8]) -> DynamicImage {
    image::load_from_memory(bytes).unwrap()
}

fn render_default(
    bytes: &[u8],
    src: Format,
    s: &str,
    out: Format,
) -> Result<Vec<u8>, ImagingError> {
    render(
        bytes,
        src,
        &spec(s),
        out,
        &Limits::default(),
        &EncodeSettings::default(),
    )
}

/// Inserts an EXIF APP1 segment that sets the orientation tag, right after a JPEG's SOI marker.
fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8]);
    let mut tiff = Vec::new();
    tiff.extend_from_slice(b"MM\x00\x2A\x00\x00\x00\x08"); // big-endian, IFD at offset 8
    tiff.extend_from_slice(&1u16.to_be_bytes()); // one entry
    tiff.extend_from_slice(&0x0112u16.to_be_bytes()); // Orientation
    tiff.extend_from_slice(&3u16.to_be_bytes()); // SHORT
    tiff.extend_from_slice(&1u32.to_be_bytes()); // count
    tiff.extend_from_slice(&orientation.to_be_bytes());
    tiff.extend_from_slice(&[0, 0]); // padding to 4 bytes
    tiff.extend_from_slice(&0u32.to_be_bytes()); // no next IFD
    let payload_len = 6 + tiff.len();
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend_from_slice(&((payload_len + 2) as u16).to_be_bytes());
    out.extend_from_slice(b"Exif\0\0");
    out.extend_from_slice(&tiff);
    out.extend_from_slice(&jpeg[2..]);
    out
}

#[test]
fn sniffs_supported_formats() {
    let img = gradient(8, 8, false);
    for f in Format::ALL {
        assert_eq!(sniff(&encoded(&img, f)), Some(f), "{f}");
    }
}

#[test]
fn sniff_rejects_unknown() {
    assert_eq!(sniff(b""), None);
    assert_eq!(sniff(b"hello world, not an image"), None);
    // A BMP header: a real format that we don't support.
    let bmp = b"BM\x3a\x00\x00\x00\x00\x00\x00\x00\x36\x00\x00\x00";
    assert_eq!(sniff(bmp), None);
    assert!(matches!(
        probe(bmp, &Limits::default()),
        Err(ImagingError::Unsupported)
    ));
}

#[test]
fn probe_reports_format_and_dimensions() {
    let img = gradient(64, 48, false);
    for f in Format::ALL {
        let info = probe(&encoded(&img, f), &Limits::default()).unwrap();
        assert_eq!(
            info,
            ImageInfo {
                format: f,
                width: 64,
                height: 48
            },
            "{f}"
        );
    }
}

#[test]
fn every_format_converts_to_every_format_at_every_spec_kind() {
    let cases = [
        ("full", (120, 80)),
        ("60s", (60, 60)),
        ("60w", (60, 40)),
        ("40h", (60, 40)),
        ("60w60h", (60, 40)),
        ("300w", (300, 200)), // upscale
        ("200s", (200, 200)), // upscale + crop
    ];
    for alpha in [false, true] {
        let img = gradient(120, 80, alpha);
        for src in Format::ALL {
            let bytes = encoded(&img, src);
            for out in Format::ALL {
                for (s, (w, h)) in cases {
                    let result = render_default(&bytes, src, s, out)
                        .unwrap_or_else(|e| panic!("{src}->{out} {s} alpha={alpha}: {e}"));
                    assert_eq!(sniff(&result), Some(out), "{src}->{out} {s}");
                    assert_eq!(
                        decode(&result).dimensions(),
                        (w, h),
                        "{src}->{out} {s} alpha={alpha}"
                    );
                }
            }
        }
    }
}

#[test]
fn square_crop_is_centred() {
    // 300x100 in three vertical bands: red | green | blue.
    let img = RgbImage::from_fn(300, 100, |x, _| match x {
        0..100 => Rgb([255, 0, 0]),
        100..200 => Rgb([0, 255, 0]),
        _ => Rgb([0, 0, 255]),
    });
    let png = encoded(&DynamicImage::ImageRgb8(img), Format::Png);
    let out = decode(&render_default(&png, Format::Png, "50s", Format::Png).unwrap()).into_rgb8();
    assert_eq!(out.dimensions(), (50, 50));
    for (x, y) in [(2, 2), (25, 25), (47, 47)] {
        let p = out.get_pixel(x, y);
        assert!(
            p[1] > 200 && p[0] < 50 && p[2] < 50,
            "pixel {x},{y} = {p:?}"
        );
    }
}

#[test]
fn fit_spec_does_not_crop() {
    // Same bands. Fitting into a square box must keep all three.
    let img = RgbImage::from_fn(300, 100, |x, _| match x {
        0..100 => Rgb([255, 0, 0]),
        100..200 => Rgb([0, 255, 0]),
        _ => Rgb([0, 0, 255]),
    });
    let png = encoded(&DynamicImage::ImageRgb8(img), Format::Png);
    let out =
        decode(&render_default(&png, Format::Png, "60w60h", Format::Png).unwrap()).into_rgb8();
    assert_eq!(out.dimensions(), (60, 20));
    assert!(out.get_pixel(2, 10)[0] > 200, "left is red");
    assert!(out.get_pixel(57, 10)[2] > 200, "right is blue");
}

#[test]
fn exif_orientation_is_applied() {
    // 40x20: left half black, right half white. Orientation 6 means "rotate 90° clockwise to
    // display", so the result is 20x40 with black on top.
    let img = RgbImage::from_fn(40, 20, |x, _| {
        if x < 20 {
            Rgb([0, 0, 0])
        } else {
            Rgb([255, 255, 255])
        }
    });
    let jpeg = with_exif_orientation(&encoded(&DynamicImage::ImageRgb8(img), Format::Jpeg), 6);

    let info = probe(&jpeg, &Limits::default()).unwrap();
    assert_eq!((info.width, info.height), (20, 40));

    for out in Format::ALL {
        let rendered = decode(&render_default(&jpeg, Format::Jpeg, "full", out).unwrap());
        assert_eq!(rendered.dimensions(), (20, 40), "{out}");
        let rgb = rendered.into_rgb8();
        assert!(rgb.get_pixel(10, 5)[0] < 60, "{out}: top should be black");
        assert!(
            rgb.get_pixel(10, 35)[0] > 200,
            "{out}: bottom should be white"
        );
    }
    // Scaling uses the oriented dimensions.
    let scaled = render_default(&jpeg, Format::Jpeg, "10w", Format::Png).unwrap();
    assert_eq!(decode(&scaled).dimensions(), (10, 20));
}

#[test]
fn output_strips_exif() {
    let jpeg = with_exif_orientation(&encoded(&gradient(40, 20, false), Format::Jpeg), 6);
    let out = render_default(&jpeg, Format::Jpeg, "full", Format::Jpeg).unwrap();
    assert!(!out.windows(6).any(|w| w == b"Exif\0\0"));
    // Already rotated and no tag, so it must not be rotated again.
    assert_eq!(probe(&out, &Limits::default()).unwrap().width, 20);
}

#[test]
fn alpha_is_flattened_onto_white_for_jpeg() {
    let transparent = DynamicImage::ImageRgba8(RgbaImage::from_pixel(10, 10, Rgba([0, 0, 0, 0])));
    let png = encoded(&transparent, Format::Png);
    let out = decode(&render_default(&png, Format::Png, "full", Format::Jpeg).unwrap()).into_rgb8();
    let p = out.get_pixel(5, 5);
    assert!(p.0.iter().all(|&c| c > 245), "{p:?}");
}

#[test]
fn alpha_is_preserved_for_png_and_webp() {
    let half = DynamicImage::ImageRgba8(RgbaImage::from_pixel(20, 20, Rgba([200, 10, 10, 128])));
    let png = encoded(&half, Format::Png);
    for out in [Format::Png, Format::Webp] {
        let rgba = decode(&render_default(&png, Format::Png, "10s", out).unwrap()).into_rgba8();
        let a = rgba.get_pixel(5, 5)[3];
        assert!((118..=138).contains(&a), "{out}: alpha {a}");
    }
}

#[test]
fn rendering_is_deterministic() {
    let img = gradient(97, 61, true);
    for src in Format::ALL {
        let bytes = encoded(&img, src);
        for out in Format::ALL {
            let a = render_default(&bytes, src, "50w", out).unwrap();
            let b = render_default(&bytes, src, "50w", out).unwrap();
            assert!(a == b, "{src}->{out} not deterministic");
        }
    }
}

#[test]
fn input_pixel_limit() {
    let png = encoded(&gradient(20, 20, false), Format::Png);
    let limits = Limits {
        max_input_pixels: 399,
        ..Limits::default()
    };
    assert!(matches!(
        probe(&png, &limits),
        Err(ImagingError::InputTooLarge {
            width: 20,
            height: 20
        })
    ));
    assert!(matches!(
        render(
            &png,
            Format::Png,
            &spec("10s"),
            Format::Png,
            &limits,
            &EncodeSettings::default()
        ),
        Err(ImagingError::InputTooLarge { .. })
    ));
    let limits = Limits {
        max_input_pixels: 400,
        ..Limits::default()
    };
    assert!(probe(&png, &limits).is_ok());
}

#[test]
fn output_dimension_limit() {
    let limits = Limits::default();
    // A tall image scaled by width: the height follows the aspect ratio and goes over the limit.
    assert!(matches!(
        planned_output(&spec("100w"), 3, 1000, &limits),
        Err(ImagingError::OutputTooLarge {
            width: 100,
            height: 33333
        })
    ));
    assert!(matches!(
        planned_output(&spec("4097s"), 10, 10, &limits),
        Err(ImagingError::OutputTooLarge { .. })
    ));
    assert_eq!(
        planned_output(&spec("4096s"), 10, 10, &limits).unwrap(),
        (4096, 4096)
    );
    // `full` is exempt, because the input pixel limit already bounds it.
    assert_eq!(
        planned_output(&spec("full"), 5000, 10, &limits).unwrap(),
        (5000, 10)
    );

    let png = encoded(&gradient(3, 300, false), Format::Png);
    let small = Limits {
        max_output_dimension: 1000,
        ..Limits::default()
    };
    assert!(matches!(
        render(
            &png,
            Format::Png,
            &spec("20w"),
            Format::Png,
            &small,
            &EncodeSettings::default()
        ),
        Err(ImagingError::OutputTooLarge { .. })
    ));
}

#[test]
fn corrupt_input_is_an_error_not_a_panic() {
    let png = encoded(&gradient(50, 50, false), Format::Png);
    let truncated = &png[..png.len() / 2];
    assert!(matches!(
        render_default(truncated, Format::Png, "10s", Format::Png),
        Err(ImagingError::Decode(_))
    ));
    // Claimed format doesn't match the bytes.
    assert!(matches!(
        render_default(&png, Format::Jpeg, "10s", Format::Png),
        Err(ImagingError::Decode(_))
    ));
}
