//! Parsing `/image/{hash}/{size}/{filename}` into an [`ImageRequest`], including reticulum's
//! normalisation redirects.

use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};

use crate::format::Format;
use crate::hash::Sha1Hash;
use crate::spec::SizeSpec;

/// A fully parsed and canonical request for an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ImageRequest {
    pub hash: Sha1Hash,
    pub spec: SizeSpec,
    /// The output format, taken from the filename's extension.
    pub format: Format,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseOutcome {
    Ok(ImageRequest),
    /// The request is valid but not canonical. Respond with a 301 to this path.
    Redirect(String),
    /// Respond with a 404.
    NotFound(&'static str),
}

/// Characters to escape when putting a filename back into a path segment.
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'/')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

pub fn image_path(hash: &Sha1Hash, spec: &SizeSpec, filename: &str) -> String {
    format!(
        "/image/{hash}/{spec}/{}",
        utf8_percent_encode(filename, SEGMENT)
    )
}

/// Splits a filename into `(stem, extension)` the way Go's `filepath.Ext` does: the extension
/// is everything after the last `.`.
fn split_extension(filename: &str) -> Option<(&str, &str)> {
    filename.rsplit_once('.')
}

/// Parses the three path segments, already percent-decoded.
///
/// Reticulum redirects in two separate hops (size first, then `.jpeg`). We combine them into a
/// single redirect to the same final URL.
pub fn parse_image_request(hash: &str, size: &str, filename: &str) -> ParseOutcome {
    let Ok(hash) = hash.parse::<Sha1Hash>() else {
        return ParseOutcome::NotFound("invalid hash");
    };
    let Ok(spec) = size.parse::<SizeSpec>() else {
        return ParseOutcome::NotFound("invalid size");
    };
    let Some((stem, ext)) = split_extension(filename) else {
        return ParseOutcome::NotFound("missing extension");
    };

    let (format, filename_canonical) = if ext == "jpeg" {
        (Format::Jpeg, false)
    } else {
        match Format::from_extension(ext) {
            Some(f) => (f, true),
            None => return ParseOutcome::NotFound("unsupported extension"),
        }
    };

    if !filename_canonical || spec.to_string() != size {
        let filename = if filename_canonical {
            filename.to_owned()
        } else {
            format!("{stem}.jpg")
        };
        return ParseOutcome::Redirect(image_path(&hash, &spec, &filename));
    }

    ParseOutcome::Ok(ImageRequest { hash, spec, format })
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &str = "a9993e364706816aba3e25717850c26c9cd0d89d";

    fn ok(spec: &str, format: Format) -> ParseOutcome {
        ParseOutcome::Ok(ImageRequest {
            hash: H.parse().unwrap(),
            spec: spec.parse().unwrap(),
            format,
        })
    }

    fn redirect(path: &str) -> ParseOutcome {
        ParseOutcome::Redirect(path.to_owned())
    }

    #[test]
    fn canonical_requests_parse() {
        assert_eq!(
            parse_image_request(H, "full", "image.jpg"),
            ok("full", Format::Jpeg)
        );
        assert_eq!(
            parse_image_request(H, "100s", "whatever.png"),
            ok("100s", Format::Png)
        );
        assert_eq!(
            parse_image_request(H, "100w200h", "x.webp"),
            ok("100w200h", Format::Webp)
        );
        assert_eq!(
            parse_image_request(H, "200h", "a.b.gif"),
            ok("200h", Format::Gif)
        );
    }

    #[test]
    fn redirects_non_canonical_size() {
        assert_eq!(
            parse_image_request(H, "200h100w", "image.jpg"),
            redirect(&format!("/image/{H}/100w200h/image.jpg"))
        );
    }

    #[test]
    fn redirects_jpeg_extension() {
        assert_eq!(
            parse_image_request(H, "100s", "photo.jpeg"),
            redirect(&format!("/image/{H}/100s/photo.jpg"))
        );
        // Only the final extension is rewritten.
        assert_eq!(
            parse_image_request(H, "100s", "a.jpeg.jpeg"),
            redirect(&format!("/image/{H}/100s/a.jpeg.jpg"))
        );
    }

    #[test]
    fn combines_size_and_jpeg_redirects() {
        assert_eq!(
            parse_image_request(H, "x100sx", "photo.jpeg"),
            redirect(&format!("/image/{H}/100s/photo.jpg"))
        );
    }

    #[test]
    fn redirect_reencodes_filename() {
        assert_eq!(
            parse_image_request(H, "0100w", "my photo?.png"),
            redirect(&format!("/image/{H}/100w/my%20photo%3F.png"))
        );
    }

    #[test]
    fn not_found_cases() {
        use ParseOutcome::NotFound;
        assert!(matches!(
            parse_image_request("abc", "full", "image.jpg"),
            NotFound(_)
        ));
        assert!(matches!(
            parse_image_request(H, "debug", "image.jpg"),
            NotFound(_)
        ));
        assert!(matches!(
            parse_image_request(H, "0s", "image.jpg"),
            NotFound(_)
        ));
        assert!(matches!(
            parse_image_request(H, "full", "image"),
            NotFound(_)
        ));
        assert!(matches!(
            parse_image_request(H, "full", "image.bmp"),
            NotFound(_)
        ));
        assert!(matches!(
            parse_image_request(H, "full", "image.JPG"),
            NotFound(_)
        ));
    }
}
