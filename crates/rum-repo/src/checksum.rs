//! Checksum kinds referenced by repo metadata, and verification.

use sha1::Sha1;
use sha2::{Digest, Sha224, Sha256, Sha384, Sha512};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum ChecksumKind {
    Sha1,
    Sha256,
    Sha512,
    // Appended (not reordered) so existing rkyv cache discriminants stay stable.
    Sha224,
    Sha384,
}

impl ChecksumKind {
    /// Parse the `type=` attribute value used in repomd/primary XML. Covers the
    /// full createrepo_c set (sha1/224/256/384/512); an unknown type is `None`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "sha" | "sha1" => Some(Self::Sha1),
            "sha224" => Some(Self::Sha224),
            "sha256" => Some(Self::Sha256),
            "sha384" => Some(Self::Sha384),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct Checksum {
    pub kind: ChecksumKind,
    /// Lowercase hex digest.
    pub hex: String,
}

enum AnyHasher {
    Sha1(Sha1),
    Sha224(Sha224),
    Sha256(Sha256),
    Sha384(Sha384),
    Sha512(Sha512),
}

impl AnyHasher {
    fn for_kind(kind: ChecksumKind) -> Self {
        match kind {
            ChecksumKind::Sha1 => AnyHasher::Sha1(Sha1::new()),
            ChecksumKind::Sha224 => AnyHasher::Sha224(Sha224::new()),
            ChecksumKind::Sha256 => AnyHasher::Sha256(Sha256::new()),
            ChecksumKind::Sha384 => AnyHasher::Sha384(Sha384::new()),
            ChecksumKind::Sha512 => AnyHasher::Sha512(Sha512::new()),
        }
    }

    fn update(&mut self, data: &[u8]) {
        match self {
            AnyHasher::Sha1(h) => Digest::update(h, data),
            AnyHasher::Sha224(h) => Digest::update(h, data),
            AnyHasher::Sha256(h) => Digest::update(h, data),
            AnyHasher::Sha384(h) => Digest::update(h, data),
            AnyHasher::Sha512(h) => Digest::update(h, data),
        }
    }

    fn finalize_hex(self) -> String {
        match self {
            AnyHasher::Sha1(h) => hex::encode(Digest::finalize(h)),
            AnyHasher::Sha224(h) => hex::encode(Digest::finalize(h)),
            AnyHasher::Sha256(h) => hex::encode(Digest::finalize(h)),
            AnyHasher::Sha384(h) => hex::encode(Digest::finalize(h)),
            AnyHasher::Sha512(h) => hex::encode(Digest::finalize(h)),
        }
    }
}

impl Checksum {
    /// Compute the digest of `data` and compare (case-insensitively) to the
    /// expected hex value.
    pub fn verify(&self, data: &[u8]) -> bool {
        let actual = match self.kind {
            ChecksumKind::Sha1 => hex::encode(Sha1::digest(data)),
            ChecksumKind::Sha224 => hex::encode(Sha224::digest(data)),
            ChecksumKind::Sha256 => hex::encode(Sha256::digest(data)),
            ChecksumKind::Sha384 => hex::encode(Sha384::digest(data)),
            ChecksumKind::Sha512 => hex::encode(Sha512::digest(data)),
        };
        actual.eq_ignore_ascii_case(&self.hex)
    }

    /// Verify the contents of a reader against this checksum without buffering
    /// the full payload in memory.
    pub fn verify_reader<R: std::io::Read>(&self, reader: &mut R) -> std::io::Result<bool> {
        let mut hasher = AnyHasher::for_kind(self.kind);
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize_hex().eq_ignore_ascii_case(&self.hex))
    }

    /// Verify a file on disk against this checksum. Returns false if the file
    /// cannot be opened or read.
    pub fn verify_file(&self, path: &std::path::Path) -> bool {
        let Ok(file) = std::fs::File::open(path) else {
            return false;
        };
        let mut reader = std::io::BufReader::new(file);
        self.verify_reader(&mut reader).unwrap_or(false)
    }

    /// Copy from `reader` to `writer`, computing the checksum on-the-fly.
    /// Returns Ok(true) if the computed checksum matches `self.hex`.
    pub fn copy_and_verify<R: std::io::Read, W: std::io::Write>(
        &self,
        reader: &mut R,
        writer: &mut W,
    ) -> std::io::Result<bool> {
        let mut hasher = AnyHasher::for_kind(self.kind);
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            writer.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
        }
        writer.flush()?;
        Ok(hasher.finalize_hex().eq_ignore_ascii_case(&self.hex))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_roundtrip() {
        // echo -n "abc" | sha256sum
        let c = Checksum {
            kind: ChecksumKind::Sha256,
            hex: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
        };
        assert!(c.verify(b"abc"));
        assert!(!c.verify(b"abd"));
    }

    #[test]
    fn kind_parsing() {
        assert_eq!(ChecksumKind::parse("sha"), Some(ChecksumKind::Sha1));
        assert_eq!(ChecksumKind::parse("SHA256"), Some(ChecksumKind::Sha256));
        assert_eq!(ChecksumKind::parse("sha224"), Some(ChecksumKind::Sha224));
        assert_eq!(ChecksumKind::parse("sha384"), Some(ChecksumKind::Sha384));
        assert_eq!(ChecksumKind::parse("md5"), None);
    }

    #[test]
    fn sha384_verify() {
        // echo -n "abc" | sha384sum
        let c = Checksum {
            kind: ChecksumKind::Sha384,
            hex: "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
                  8086072ba1e7cc2358baeca134c825a7"
                .into(),
        };
        assert!(c.verify(b"abc"));
        assert!(!c.verify(b"abd"));
    }

    #[test]
    fn streaming_verify_and_copy() {
        let c = Checksum {
            kind: ChecksumKind::Sha256,
            hex: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
        };
        let mut cursor = std::io::Cursor::new(b"abc");
        assert!(c.verify_reader(&mut cursor).unwrap());

        let mut bad_cursor = std::io::Cursor::new(b"abd");
        assert!(!c.verify_reader(&mut bad_cursor).unwrap());

        let mut src = std::io::Cursor::new(b"abc");
        let mut dest = Vec::new();
        assert!(c.copy_and_verify(&mut src, &mut dest).unwrap());
        assert_eq!(&dest, b"abc");

        let mut bad_src = std::io::Cursor::new(b"abd");
        let mut bad_dest = Vec::new();
        assert!(!c.copy_and_verify(&mut bad_src, &mut bad_dest).unwrap());
        assert_eq!(&bad_dest, b"abd");
    }
}
