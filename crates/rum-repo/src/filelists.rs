//! Streaming parser for `filelists.xml`, filtered to a set of wanted files.
//!
//! `primary.xml` only lists a subset of files (createrepo's "core files"
//! filter: `bin/`, `sbin/`, `/etc/*`). Deep paths a dependency might need
//! (e.g. `/usr/lib64/cmake/Qt5/Qt5Config.cmake`) live only here. This file is
//! large, so we parse it lazily and keep only the file paths we are actually
//! looking for, mapping each to the owning package's pkgid.

use std::collections::HashSet;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::RepoError;

/// Parse `filelists.xml` from a streaming reader, returning `(pkgid,
/// matched_files)` for every package that owns at least one file in `wanted`.
/// `pkgid` matches the primary checksum (`AvailablePackage::checksum.hex`).
///
/// Reads incrementally so the (large, ~100MB decompressed) file is never fully
/// materialized in memory — only the matched paths are retained.
pub fn parse<R: std::io::Read>(
    input: R,
    wanted: &HashSet<String>,
) -> Result<Vec<(String, Vec<String>)>, RepoError> {
    let mut reader = Reader::from_reader(std::io::BufReader::new(input));
    reader.config_mut().trim_text(true);

    let mut out = Vec::new();
    let mut buf = Vec::new();

    let mut cur_pkgid: Option<String> = None;
    let mut cur_files: Vec<String> = Vec::new();
    let mut in_file = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"package" => {
                    cur_pkgid = attr(&e, b"pkgid");
                    cur_files.clear();
                }
                b"file" => in_file = true,
                _ => {}
            },
            Ok(Event::Text(t)) if in_file => {
                let path = t.unescape().unwrap_or_default();
                if wanted.contains(path.as_ref()) {
                    cur_files.push(path.into_owned());
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"file" => in_file = false,
                b"package" => {
                    if let Some(pkgid) = cur_pkgid.take() {
                        if !cur_files.is_empty() {
                            out.push((pkgid, std::mem::take(&mut cur_files)));
                        }
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(RepoError::Xml(format!("filelists.xml: {e}"))),
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_wanted_files() {
        let xml = br#"<?xml version="1.0"?>
        <filelists>
          <package pkgid="aaa" name="qt5" arch="x86_64">
            <version epoch="0" ver="5" rel="1"/>
            <file>/usr/lib64/cmake/Qt5/Qt5Config.cmake</file>
            <file>/usr/share/doc/qt5/README</file>
          </package>
          <package pkgid="bbb" name="other" arch="x86_64">
            <file>/usr/share/other/thing</file>
          </package>
        </filelists>"#;
        let mut wanted = HashSet::new();
        wanted.insert("/usr/lib64/cmake/Qt5/Qt5Config.cmake".to_string());
        let got = parse(&xml[..], &wanted).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, "aaa");
        assert_eq!(got[0].1, vec!["/usr/lib64/cmake/Qt5/Qt5Config.cmake"]);
    }
}
