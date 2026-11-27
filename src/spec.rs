//! Size specs: the `{size}` segment of `/image/{hash}/{size}/{filename}`.
//!
//! The grammar copies `github.com/thraxil/resize` `MakeSizeSpec`, which reticulum uses:
//!
//! * `full`: the exact string `full`, meaning no scaling.
//! * `Ns`: if a `<digits>s` appears *anywhere*, the result is an N×N centre-cropped square.
//!   This takes precedence over `w`/`h`.
//! * Otherwise the first `<digits>w` and the first `<digits>h` found anywhere set width and/or
//!   height.
//!
//! The parser is lenient (`x100sx` parses as `100s`, and `200h100w` as `100w200h`). The HTTP
//! layer compares the input with [`SizeSpec::to_string`] and redirects non-canonical forms to the
//! canonical one, as reticulum does.
//!
//! Deliberate differences from the Go code, where it produced nonsense:
//! * A string with no usable dimensions (e.g. `fullx`, `abc`) is rejected. Go produced a `-1h`
//!   spec and a chain of redirects.
//! * Zero dimensions, and numbers too large for a `u32`, are rejected. Go's `Atoi` silently
//!   turned overflow into `0`.

use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SizeSpec {
    /// No scaling.
    Full,
    /// Centre-crop to a square, then scale to N×N.
    Square(NonZeroU32),
    /// Scale to this width, keeping aspect ratio.
    Width(NonZeroU32),
    /// Scale to this height, keeping aspect ratio.
    Height(NonZeroU32),
    /// Fit inside a width×height box, keeping aspect ratio, with no cropping.
    Fit {
        width: NonZeroU32,
        height: NonZeroU32,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InvalidSizeSpec {
    #[error("size spec has no dimensions")]
    NoDimensions,
    #[error("size spec dimension must be a positive integer that fits in 32 bits")]
    BadDimension,
}

impl SizeSpec {
    pub fn is_full(&self) -> bool {
        matches!(self, SizeSpec::Full)
    }

    /// The largest output dimension this spec asks for directly, or `None` for [`SizeSpec::Full`].
    ///
    /// Because the other side follows the source aspect ratio, a `Width` or `Height` spec can
    /// still produce a larger opposite side. The imaging code checks the real output size
    /// against the limits.
    pub fn max_requested_dimension(&self) -> Option<u32> {
        match *self {
            SizeSpec::Full => None,
            SizeSpec::Square(n) | SizeSpec::Width(n) | SizeSpec::Height(n) => Some(n.get()),
            SizeSpec::Fit { width, height } => Some(width.get().max(height.get())),
        }
    }

    /// Output width and height for a source image of `src_w`×`src_h`, after EXIF orientation.
    ///
    /// Upscaling is allowed. Each side is at least 1 pixel.
    pub fn target_dimensions(&self, src_w: u32, src_h: u32) -> (u32, u32) {
        let (sw, sh) = (u64::from(src_w.max(1)), u64::from(src_h.max(1)));
        // Round to nearest, never 0, saturating at u32::MAX.
        let scale = |num: u64, mul: u64, den: u64| -> u32 {
            let v = (num * mul + den / 2) / den;
            v.clamp(1, u64::from(u32::MAX)) as u32
        };
        match *self {
            SizeSpec::Full => (src_w, src_h),
            SizeSpec::Square(n) => (n.get(), n.get()),
            SizeSpec::Width(w) => (w.get(), scale(sh, u64::from(w.get()), sw)),
            SizeSpec::Height(h) => (scale(sw, u64::from(h.get()), sh), h.get()),
            SizeSpec::Fit { width, height } => {
                let (bw, bh) = (u64::from(width.get()), u64::from(height.get()));
                // Compare bw/sw with bh/sh without floating point. The smaller ratio limits the size.
                if bw * sh <= bh * sw {
                    (width.get(), scale(sh, bw, sw))
                } else {
                    (scale(sw, bh, sh), height.get())
                }
            }
        }
    }
}

/// Finds the first maximal run of ASCII digits immediately followed by `suffix`. This matches
/// Go's leftmost-first `regexp.FindString("\\d+<suffix>")`: the leftmost match always begins at
/// the start of a digit run, because the run can extend back to its start.
fn find_dimension(s: &str, suffix: u8) -> Option<Result<NonZeroU32, InvalidSizeSpec>> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i < b.len() && b[i] == suffix {
                let n = s[start..i]
                    .parse::<u32>()
                    .ok()
                    .and_then(NonZeroU32::new)
                    .ok_or(InvalidSizeSpec::BadDimension);
                return Some(n);
            }
        } else {
            i += 1;
        }
    }
    None
}

impl FromStr for SizeSpec {
    type Err = InvalidSizeSpec;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "full" {
            return Ok(SizeSpec::Full);
        }
        if let Some(n) = find_dimension(s, b's') {
            return Ok(SizeSpec::Square(n?));
        }
        let width = find_dimension(s, b'w').transpose()?;
        let height = find_dimension(s, b'h').transpose()?;
        match (width, height) {
            (Some(width), Some(height)) => Ok(SizeSpec::Fit { width, height }),
            (Some(w), None) => Ok(SizeSpec::Width(w)),
            (None, Some(h)) => Ok(SizeSpec::Height(h)),
            (None, None) => Err(InvalidSizeSpec::NoDimensions),
        }
    }
}

impl fmt::Display for SizeSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SizeSpec::Full => f.write_str("full"),
            SizeSpec::Square(n) => write!(f, "{n}s"),
            SizeSpec::Width(w) => write!(f, "{w}w"),
            SizeSpec::Height(h) => write!(f, "{h}h"),
            SizeSpec::Fit { width, height } => write!(f, "{width}w{height}h"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<SizeSpec, InvalidSizeSpec> {
        s.parse()
    }

    #[test]
    fn canonical_forms_round_trip() {
        for s in [
            "full",
            "100s",
            "200w",
            "300h",
            "100w200h",
            "1s",
            "4096w4096h",
        ] {
            assert_eq!(p(s).unwrap().to_string(), s, "{s}");
        }
    }

    /// Lenient inputs and the canonical form reticulum redirects them to.
    #[test]
    fn lenient_inputs_canonicalise_like_go() {
        let cases = [
            ("200h100w", "100w200h"),
            ("x100sx", "100s"),
            ("100w100s", "100s"), // square takes precedence
            ("100s200w", "100s"),
            ("0100w", "100w"), // leading zeros dropped (Go Atoi)
            ("1w2w", "1w"),    // first match wins
            ("10h20w", "20w10h"),
            ("w100w", "100w"),
            ("100wide", "100w"),
            ("FULL100h", "100h"),
            ("full100w", "100w"),
        ];
        for (input, canonical) in cases {
            assert_eq!(p(input).unwrap().to_string(), canonical, "{input}");
        }
    }

    #[test]
    fn rejects_specs_without_dimensions() {
        for s in ["", "fullx", "Full", "abc", "w", "100", "100x", "-w", "100S"] {
            assert_eq!(p(s), Err(InvalidSizeSpec::NoDimensions), "{s:?}");
        }
    }

    #[test]
    fn rejects_zero_and_overflow() {
        for s in ["0s", "0w", "0h", "100w0h", "99999999999w", "00s"] {
            assert_eq!(p(s), Err(InvalidSizeSpec::BadDimension), "{s:?}");
        }
    }

    #[test]
    fn target_dimensions() {
        // (spec, src_w, src_h, expected)
        let cases = [
            ("full", 640, 480, (640, 480)),
            ("100s", 640, 480, (100, 100)),
            ("100s", 10, 20, (100, 100)), // upscale
            ("320w", 640, 480, (320, 240)),
            ("1280w", 640, 480, (1280, 960)), // upscale
            ("240h", 640, 480, (320, 240)),
            ("100w", 3, 1000, (100, 33333)), // tall image: height follows aspect
            ("1w", 1000, 1, (1, 1)),         // never 0
            // fit inside the box, no crop
            ("100w100h", 640, 480, (100, 75)),
            ("100w100h", 480, 640, (75, 100)),
            ("300w100h", 640, 480, (133, 100)),
            ("100w300h", 640, 480, (100, 75)),
            ("2000w2000h", 640, 480, (2000, 1500)), // upscale
            ("640w480h", 640, 480, (640, 480)),
        ];
        for (spec, w, h, expected) in cases {
            assert_eq!(
                p(spec).unwrap().target_dimensions(w, h),
                expected,
                "{spec} on {w}x{h}"
            );
        }
    }

    #[test]
    fn max_requested_dimension() {
        assert_eq!(p("full").unwrap().max_requested_dimension(), None);
        assert_eq!(p("100s").unwrap().max_requested_dimension(), Some(100));
        assert_eq!(p("100w300h").unwrap().max_requested_dimension(), Some(300));
    }
}
