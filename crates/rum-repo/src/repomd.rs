//! Parser for `repodata/repomd.xml` — the index of a repository's metadata.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::checksum::{Checksum, ChecksumKind};
use crate::RepoError;

/// One `<data type="...">` entry in repomd.xml.
#[derive(Debug, Clone)]
pub struct RepoMdData {
    /// e.g. "primary", "filelists", "other", "primary_db", "updateinfo".
    pub data_type: String,
    /// href relative to the repo base URL, e.g. `repodata/<hash>-primary.xml.gz`.
    pub location: String,
    /// Checksum of the (compressed) file as downloaded.
    pub checksum: Checksum,
    /// Checksum of the decompressed content, if advertised.
    pub open_checksum: Option<Checksum>,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct RepoMd {
    pub data: Vec<RepoMdData>,
}

impl RepoMd {
    /// Find the entry for a data type, preferring the XML form over the sqlite
    /// `_db` form (we parse XML, not the sqlite metadata).
    pub fn get(&self, data_type: &str) -> Option<&RepoMdData> {
        self.data.iter().find(|d| d.data_type == data_type)
    }

    pub fn parse(xml: &str) -> Result<RepoMd, RepoError> {
        let mut reader = Reader::from_str(xml);
        reader.config_mut().trim_text(true);

        let mut md = RepoMd::default();
        let mut buf = Vec::new();

        // State for the <data> element currently being built.
        let mut cur: Option<Partial> = None;
        // Which checksum we're inside: 0 = none, 1 = checksum, 2 = open-checksum.
        let mut cksum_ctx = 0u8;
        let mut cksum_kind: Option<ChecksumKind> = None;

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(e)) => {
                    let name = e.local_name().as_ref().to_vec();
                    match name.as_slice() {
                        b"data" => {
                            cur = Some(Partial {
                                data_type: attr(&e, b"type"),
                                ..Default::default()
                            });
                        }
                        b"checksum" => {
                            cksum_ctx = 1;
                            cksum_kind =
                                attr_opt(&e, b"type").and_then(|s| ChecksumKind::parse(&s));
                        }
                        b"open-checksum" => {
                            cksum_ctx = 2;
                            cksum_kind =
                                attr_opt(&e, b"type").and_then(|s| ChecksumKind::parse(&s));
                        }
                        _ => {}
                    }
                }
                Ok(Event::Empty(e)) => {
                    // <location href="..."/> and <size .../> are empty elements.
                    let name = e.local_name().as_ref().to_vec();
                    if let Some(p) = cur.as_mut() {
                        match name.as_slice() {
                            b"location" => p.location = attr(&e, b"href"),
                            b"size" => p.size = attr_opt(&e, b"bytes").and_then(|s| s.parse().ok()),
                            _ => {}
                        }
                    }
                }
                Ok(Event::Text(t)) => {
                    if cksum_ctx != 0 {
                        if let (Some(p), Some(kind)) = (cur.as_mut(), cksum_kind) {
                            let hex = t.unescape().unwrap_or_default().to_string();
                            let c = Checksum { kind, hex };
                            if cksum_ctx == 1 {
                                p.checksum = Some(c);
                            } else {
                                p.open_checksum = Some(c);
                            }
                        }
                    }
                }
                Ok(Event::End(e)) => {
                    let name = e.local_name().as_ref().to_vec();
                    match name.as_slice() {
                        b"checksum" | b"open-checksum" => {
                            cksum_ctx = 0;
                            cksum_kind = None;
                        }
                        b"data" => {
                            if let Some(p) = cur.take() {
                                if let Some(d) = p.finish() {
                                    md.data.push(d);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => return Err(RepoError::Xml(format!("repomd.xml: {e}"))),
                _ => {}
            }
            buf.clear();
        }

        if md.data.is_empty() {
            return Err(RepoError::Xml(
                "repomd.xml contained no <data> entries".into(),
            ));
        }
        Ok(md)
    }
}

#[derive(Default)]
struct Partial {
    data_type: String,
    location: String,
    checksum: Option<Checksum>,
    open_checksum: Option<Checksum>,
    size: Option<u64>,
}

impl Partial {
    fn finish(self) -> Option<RepoMdData> {
        // A data entry is only usable with a type, location and checksum.
        if self.data_type.is_empty() || self.location.is_empty() {
            return None;
        }
        Some(RepoMdData {
            data_type: self.data_type,
            location: self.location,
            checksum: self.checksum?,
            open_checksum: self.open_checksum,
            size: self.size,
        })
    }
}

fn attr(e: &quick_xml::events::BytesStart, key: &[u8]) -> String {
    attr_opt(e, key).unwrap_or_default()
}

fn attr_opt(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == key)
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repomd() {
        let xml = r#"<?xml version="1.0"?>
        <repomd xmlns="http://linux.duke.edu/metadata/repo">
          <data type="primary">
            <checksum type="sha256">aaa</checksum>
            <open-checksum type="sha256">bbb</open-checksum>
            <location href="repodata/aaa-primary.xml.gz"/>
            <size bytes="12345"/>
          </data>
          <data type="filelists">
            <checksum type="sha256">ccc</checksum>
            <location href="repodata/ccc-filelists.xml.gz"/>
          </data>
        </repomd>"#;
        let md = RepoMd::parse(xml).unwrap();
        assert_eq!(md.data.len(), 2);
        let p = md.get("primary").unwrap();
        assert_eq!(p.location, "repodata/aaa-primary.xml.gz");
        assert_eq!(p.checksum.hex, "aaa");
        assert_eq!(p.open_checksum.as_ref().unwrap().hex, "bbb");
        assert_eq!(p.size, Some(12345));
        assert!(md.get("other").is_none());
    }
}
