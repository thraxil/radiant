//! Image formats radiant can serve.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    Jpeg,
    Png,
    Gif,
    Webp,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Jpeg, Format::Png, Format::Gif, Format::Webp];

    /// Canonical file extension, without the dot. Used in URLs, storage paths and the
    /// upload response's `extension` field.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Jpeg => "jpg",
            Format::Png => "png",
            Format::Gif => "gif",
            Format::Webp => "webp",
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            Format::Jpeg => "image/jpeg",
            Format::Png => "image/png",
            Format::Gif => "image/gif",
            Format::Webp => "image/webp",
        }
    }

    /// Looks up a canonical extension (no dot). `jpeg` is *not* accepted here: the URL layer
    /// redirects it to `jpg`.
    pub fn from_extension(ext: &str) -> Option<Format> {
        Format::ALL.into_iter().find(|f| f.extension() == ext)
    }
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.extension())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_round_trip() {
        for f in Format::ALL {
            assert_eq!(Format::from_extension(f.extension()), Some(f));
        }
        assert_eq!(Format::from_extension("jpeg"), None);
        assert_eq!(Format::from_extension("JPG"), None);
        assert_eq!(Format::from_extension(""), None);
    }
}
