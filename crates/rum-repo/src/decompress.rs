//! Pure-Rust decompression for the encodings dnf repo metadata uses.
//!
//! Metadata files are named `<hash>-primary.xml.gz` / `.xml.zst` / `.xml.xz`.
//! We pick the decoder from the location suffix; unknown/plain files pass
//! through unchanged.

#[derive(Debug, thiserror::Error)]
pub enum DecompressError {
    #[error("xz decode failed: {0}")]
    Xz(String),
    #[error("zstd decode failed: {0}")]
    Zstd(String),
}

/// A streaming decompressor over in-memory compressed `data`, chosen by the
/// `location` suffix. Lets callers parse huge files (e.g. filelists.xml)
/// without materializing the full decompressed content in memory.
pub fn reader<'a>(
    location: &str,
    data: &'a [u8],
) -> Result<Box<dyn std::io::Read + 'a>, DecompressError> {
    let lower = location.to_ascii_lowercase();
    if lower.ends_with(".gz") {
        Ok(Box::new(flate2::read::GzDecoder::new(data)))
    } else if lower.ends_with(".zst") || lower.ends_with(".zstd") {
        let dec = ruzstd::StreamingDecoder::new(data)
            .map_err(|e| DecompressError::Zstd(e.to_string()))?;
        Ok(Box::new(dec))
    } else if lower.ends_with(".xz") {
        // lzma-rs has no streaming reader; decode once, then stream from memory.
        let out = unxz(data)?;
        Ok(Box::new(std::io::Cursor::new(out)))
    } else {
        Ok(Box::new(data))
    }
}

fn unxz(data: &[u8]) -> Result<Vec<u8>, DecompressError> {
    let mut out = Vec::new();
    let mut reader = std::io::BufReader::new(data);
    lzma_rs::xz_decompress(&mut reader, &mut out)
        .map_err(|e| DecompressError::Xz(e.to_string()))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn read_all(location: &str, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        reader(location, data)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    #[test]
    fn gzip_roundtrip() {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"hello rum").unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(read_all("x-primary.xml.gz", &gz), b"hello rum");
    }

    #[test]
    fn plain_passthrough() {
        assert_eq!(read_all("repomd.xml", b"<xml/>"), b"<xml/>");
    }
}
