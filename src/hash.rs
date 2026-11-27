//! Content hashes identifying originals.

use std::fmt;
use std::str::FromStr;

/// A SHA-1 content hash, held as 40 lowercase hex characters.
///
/// This is the image id in every public URL, and it is also the Cask key of the original.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Sha1Hash([u8; 40]);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid hash: expected 40 lowercase hex characters")]
pub struct InvalidHash;

impl Sha1Hash {
    /// Hash some bytes.
    pub fn digest(bytes: &[u8]) -> Self {
        use sha1::{Digest, Sha1};
        let out = Sha1::digest(bytes);
        let mut hex = [0u8; 40];
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for (i, b) in out.iter().enumerate() {
            hex[i * 2] = HEX[(b >> 4) as usize];
            hex[i * 2 + 1] = HEX[(b & 0x0f) as usize];
        }
        Self(hex)
    }

    pub fn as_str(&self) -> &str {
        // Invariant: only ASCII hex digits are ever stored.
        std::str::from_utf8(&self.0).expect("hash is ascii")
    }
}

impl FromStr for Sha1Hash {
    type Err = InvalidHash;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes: [u8; 40] = s.as_bytes().try_into().map_err(|_| InvalidHash)?;
        if bytes
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        {
            Ok(Self(bytes))
        } else {
            Err(InvalidHash)
        }
    }
}

impl fmt::Display for Sha1Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Sha1Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Sha1Hash({})", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_known_value() {
        assert_eq!(
            Sha1Hash::digest(b"abc").as_str(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn parses_valid_hash() {
        let h: Sha1Hash = "a9993e364706816aba3e25717850c26c9cd0d89d".parse().unwrap();
        assert_eq!(h.to_string(), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn rejects_bad_hashes() {
        for bad in [
            "",
            "a9993e364706816aba3e25717850c26c9cd0d89", // 39
            "a9993e364706816aba3e25717850c26c9cd0d89dd", // 41
            "A9993E364706816ABA3E25717850C26C9CD0D89D", // uppercase
            "g9993e364706816aba3e25717850c26c9cd0d89d", // non-hex
            "../3e364706816aba3e25717850c26c9cd0d89d", // path-ish
        ] {
            assert_eq!(bad.parse::<Sha1Hash>(), Err(InvalidHash), "{bad:?}");
        }
    }
}
