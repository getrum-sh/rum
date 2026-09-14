//! Streaming parser for `primary.xml` into a list of available packages.
//!
//! We extract the fields needed for listing/searching/info and for locating
//! the RPM to download: name, arch, EVR, summary, size, location, checksum.
//! Dependency data (`provides`/`requires`) lives in `<format>` and is left for
//! the solver milestone.

use quick_xml::events::Event;
use quick_xml::Reader;
use rum_solve::{Dep, DepFlag, Evr};

use crate::checksum::{Checksum, ChecksumKind};
use crate::interned::{IDep, IPackage, Interner, Store, Sym};
use crate::RepoError;

/// A package as advertised by a repository (not necessarily installed). This is
/// the owned form handed to the resolve/download path; the cache itself stores
/// the compact interned [`Store`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AvailablePackage {
    pub name: String,
    /// Epoch as advertised; 0 means "no epoch" for display purposes.
    pub epoch: u64,
    pub version: String,
    pub release: String,
    pub arch: String,
    pub summary: String,
    /// Compressed RPM size in bytes (`<size package=...>`).
    pub size: u64,
    /// href relative to the repo base URL.
    pub location: String,
    pub checksum: Checksum,
    /// Which repo this came from (filled in by the caller).
    pub repo_id: String,
    /// Capabilities this package provides (from `<format><rpm:provides>`).
    pub provides: Vec<Dep>,
    /// Capabilities this package requires (from `<format><rpm:requires>`).
    pub requires: Vec<Dep>,
    /// Capabilities this package obsoletes (from `<format><rpm:obsoletes>`);
    /// installing it removes/replaces any installed package matching these.
    pub obsoletes: Vec<Dep>,
    /// Capabilities this package conflicts with (from `<format><rpm:conflicts>`);
    /// it cannot be installed alongside a package matching these.
    pub conflicts: Vec<Dep>,
    /// Weak dependencies (from `<format><rpm:recommends>`); installed by
    /// default when satisfiable, matching dnf's `install_weak_deps=1`.
    pub recommends: Vec<Dep>,
    /// File paths this package advertises in primary (satisfy file deps).
    pub files: Vec<String>,
}

impl AvailablePackage {
    pub fn evr(&self) -> String {
        if self.epoch == 0 {
            format!("{}-{}", self.version, self.release)
        } else {
            format!("{}:{}-{}", self.epoch, self.version, self.release)
        }
    }
    pub fn name_arch(&self) -> String {
        format!("{}.{}", self.name, self.arch)
    }
    pub fn nevra(&self) -> String {
        format!("{}-{}.{}", self.name, self.evr(), self.arch)
    }
}

/// Parse primary.xml from any reader into the interned [`Store`].
///
/// Reads incrementally (via an internal `BufReader`) so a large decompressed
/// primary (RHEL's runs to ~1-2GB) is never fully materialized, and interns
/// every string as it goes so the retained form is symbols + a shared arena
/// rather than millions of owned `String`s (that OOM'd a 761MB host on RHEL
/// BaseOS). Only symbols and the arena are kept; `&str` is resolved on demand.
pub fn parse_reader<R: std::io::Read>(input: R, repo_id: &str) -> Result<Store, RepoError> {
    let mut reader = Reader::from_reader(std::io::BufReader::new(input));
    reader.config_mut().trim_text(true);

    let mut itn = Interner::new();
    let repo_sym = itn.intern(repo_id);
    let mut out: Vec<IPackage> = Vec::new();
    let mut buf = Vec::new();

    let mut cur: Option<Builder> = None;
    // Which text-bearing child we're currently capturing.
    let mut field = Field::None;
    // Which dependency list we're inside (provides/requires), if any.
    let mut dep_ctx = DepCtx::None;
    // Only capture the top-level package checksum (pkgid="YES"), not the ones
    // nested in <format>.
    let mut depth_in_package = 0i32;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_vec();
                match name.as_slice() {
                    b"package" => {
                        cur = Some(Builder::default());
                        depth_in_package = 0;
                    }
                    _ if cur.is_some() => {
                        depth_in_package += 1;
                        if let Some(b) = cur.as_mut() {
                            match name.as_slice() {
                                b"name" => field = Field::Name,
                                b"arch" => field = Field::Arch,
                                b"summary" => field = Field::Summary,
                                b"version" => b.read_version(&e),
                                b"checksum" if depth_in_package == 1 => {
                                    b.pending_cksum_kind =
                                        attr_opt(&e, b"type").and_then(|s| ChecksumKind::parse(&s));
                                    field = Field::Checksum;
                                }
                                b"location" => b.location = attr(&e, b"href"),
                                b"size" => b.read_size(&e),
                                b"provides" => dep_ctx = DepCtx::Provides,
                                b"requires" => dep_ctx = DepCtx::Requires,
                                b"recommends" => dep_ctx = DepCtx::Recommends,
                                b"obsoletes" => dep_ctx = DepCtx::Obsoletes,
                                b"conflicts" => dep_ctx = DepCtx::Conflicts,
                                b"suggests" | b"enhances" | b"supplements" => {
                                    dep_ctx = DepCtx::None
                                }
                                b"file" => field = Field::File,
                                b"entry" => push_entry(b, dep_ctx, &e),
                                _ => field = Field::None,
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) if cur.is_some() => {
                // Self-closing attribute-bearing elements.
                let name = e.local_name().as_ref().to_vec();
                if let Some(b) = cur.as_mut() {
                    match name.as_slice() {
                        b"version" => b.read_version(&e),
                        b"location" => b.location = attr(&e, b"href"),
                        b"size" => b.read_size(&e),
                        // <rpm:entry .../> is almost always self-closing.
                        b"entry" => push_entry(b, dep_ctx, &e),
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if let Some(b) = cur.as_mut() {
                    let text = t.unescape().unwrap_or_default();
                    match field {
                        Field::Name => b.name = text.into_owned(),
                        Field::Arch => b.arch = text.into_owned(),
                        Field::Summary => b.summary = text.into_owned(),
                        Field::File => b.files.push(text.into_owned()),
                        Field::Checksum => {
                            if let Some(kind) = b.pending_cksum_kind {
                                b.checksum = Some(Checksum {
                                    kind,
                                    hex: text.into_owned(),
                                });
                            }
                        }
                        Field::None => {}
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e.local_name().as_ref().to_vec();
                if name.as_slice() == b"package" {
                    if let Some(b) = cur.take() {
                        if let Some(pkg) = b.finish_interned(&mut itn, repo_sym) {
                            out.push(pkg);
                        }
                    }
                } else if cur.is_some() {
                    depth_in_package -= 1;
                    if matches!(
                        name.as_slice(),
                        b"provides" | b"requires" | b"recommends" | b"conflicts" | b"obsoletes"
                    ) {
                        dep_ctx = DepCtx::None;
                    }
                }
                field = Field::None;
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(RepoError::Xml(format!("primary.xml: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    Ok(itn.into_store(out))
}

#[derive(Clone, Copy, PartialEq)]
enum Field {
    None,
    Name,
    Arch,
    Summary,
    Checksum,
    File,
}

/// Which dependency list (if any) we're currently inside.
#[derive(Clone, Copy, PartialEq)]
enum DepCtx {
    None,
    Provides,
    Requires,
    Recommends,
    Obsoletes,
    Conflicts,
}

/// Push an `<rpm:entry>` into the provides/requires list per the active context.
fn push_entry(b: &mut Builder, ctx: DepCtx, e: &quick_xml::events::BytesStart) {
    if ctx == DepCtx::None {
        return;
    }
    if let Some(d) = read_entry(e) {
        match ctx {
            DepCtx::Provides => b.provides.push(d),
            DepCtx::Requires => b.requires.push(d),
            DepCtx::Recommends => b.recommends.push(d),
            DepCtx::Obsoletes => b.obsoletes.push(d),
            DepCtx::Conflicts => b.conflicts.push(d),
            DepCtx::None => {}
        }
    }
}

/// Build a `Dep` from an `<rpm:entry .../>` element.
fn read_entry(e: &quick_xml::events::BytesStart) -> Option<Dep> {
    let name = attr(e, b"name");
    if name.is_empty() {
        return None;
    }
    let flag = DepFlag::parse(&attr(e, b"flags"));
    let evr = attr_opt(e, b"ver").map(|ver| {
        let epoch = attr_opt(e, b"epoch").and_then(|s| s.parse().ok());
        Evr::new(epoch, ver, attr_opt(e, b"rel").unwrap_or_default())
    });
    Some(Dep { name, flag, evr })
}

#[derive(Default)]
struct Builder {
    name: String,
    epoch: u64,
    version: String,
    release: String,
    arch: String,
    summary: String,
    size: u64,
    location: String,
    checksum: Option<Checksum>,
    pending_cksum_kind: Option<ChecksumKind>,
    provides: Vec<Dep>,
    requires: Vec<Dep>,
    recommends: Vec<Dep>,
    obsoletes: Vec<Dep>,
    conflicts: Vec<Dep>,
    files: Vec<String>,
}

impl Builder {
    fn read_version(&mut self, e: &quick_xml::events::BytesStart) {
        self.epoch = attr_opt(e, b"epoch")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        self.version = attr(e, b"ver");
        self.release = attr(e, b"rel");
    }
    fn read_size(&mut self, e: &quick_xml::events::BytesStart) {
        self.size = attr_opt(e, b"package")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
    }
    /// Intern this package's fields into `itn`, producing a compact [`IPackage`].
    /// `repo_sym` is the pre-interned repo id (constant across the parse).
    fn finish_interned(self, itn: &mut Interner, repo_sym: Sym) -> Option<IPackage> {
        if self.name.is_empty() || self.version.is_empty() || self.location.is_empty() {
            return None;
        }
        // A package with no usable checksum (missing, or an unsupported `type=`)
        // can't be verified after download, so it's dropped — but noisily, not
        // silently, since an unknown checksum type would otherwise make a whole
        // repo vanish with no explanation.
        let Some(checksum) = self.checksum else {
            tracing::warn!(
                package = %self.name,
                "skipping package with missing or unsupported checksum type"
            );
            return None;
        };
        let idep = |itn: &mut Interner, d: &Dep| IDep {
            name: itn.intern(&d.name),
            evr: itn.intern_evr(&d.evr),
            flag: d.flag,
        };
        Some(IPackage {
            name: itn.intern(&self.name),
            epoch: self.epoch,
            version: itn.intern(&self.version),
            release: itn.intern(&self.release),
            arch: itn.intern(&self.arch),
            summary: itn.intern(&self.summary),
            size: self.size,
            location: itn.intern(&self.location),
            checksum_kind: checksum.kind,
            checksum_hex: itn.intern(&checksum.hex),
            repo_id: repo_sym,
            provides: self.provides.iter().map(|d| idep(itn, d)).collect(),
            requires: self.requires.iter().map(|d| idep(itn, d)).collect(),
            recommends: self.recommends.iter().map(|d| idep(itn, d)).collect(),
            obsoletes: self.obsoletes.iter().map(|d| idep(itn, d)).collect(),
            conflicts: self.conflicts.iter().map(|d| idep(itn, d)).collect(),
            files: self.files.iter().map(|f| itn.intern(f)).collect(),
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
    fn parses_packages() {
        let xml = br#"<?xml version="1.0"?>
        <metadata xmlns="http://linux.duke.edu/metadata/common"
                  xmlns:rpm="http://linux.duke.edu/metadata/rpm" packages="2">
          <package type="rpm">
            <name>bash</name>
            <arch>x86_64</arch>
            <version epoch="0" ver="5.2.15" rel="1.amzn2023"/>
            <checksum type="sha256" pkgid="YES">deadbeef</checksum>
            <summary>The GNU Bourne Again shell</summary>
            <size package="1234" installed="5678" archive="9012"/>
            <location href="Packages/b/bash-5.2.15-1.amzn2023.x86_64.rpm"/>
            <format>
              <rpm:provides>
                <rpm:entry name="bash" flags="EQ" epoch="0" ver="5.2.15" rel="1.amzn2023"/>
                <rpm:entry name="config(bash)"/>
              </rpm:provides>
              <rpm:requires>
                <rpm:entry name="glibc" flags="GE" epoch="0" ver="2.34"/>
                <rpm:entry name="/bin/sh"/>
              </rpm:requires>
              <rpm:recommends>
                <rpm:entry name="bash-completion"/>
              </rpm:recommends>
              <rpm:obsoletes>
                <rpm:entry name="bash-legacy" flags="LT" epoch="0" ver="4.0"/>
              </rpm:obsoletes>
              <rpm:conflicts>
                <rpm:entry name="bash-broken"/>
              </rpm:conflicts>
              <file>/usr/bin/bash</file>
              <file type="dir">/etc/bash</file>
              <checksum>should-not-be-picked</checksum>
            </format>
          </package>
          <package type="rpm">
            <name>zlib</name>
            <arch>x86_64</arch>
            <version epoch="2" ver="1.2.13" rel="3"/>
            <checksum type="sha256" pkgid="YES">cafef00d</checksum>
            <summary>zlib lib</summary>
            <size package="4321"/>
            <location href="Packages/z/zlib-1.2.13-3.x86_64.rpm"/>
          </package>
        </metadata>"#;

        // Parse to the interned store, then round-trip through the archived
        // form (the real warm-cache path) to owned packages for assertions.
        let store = parse_reader(&xml[..], "baseos").unwrap();
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&store).unwrap();
        let archived =
            rkyv::access::<crate::interned::ArchivedStore, rkyv::rancor::Error>(&bytes).unwrap();
        let pkgs = archived.to_owned_packages();
        assert_eq!(pkgs.len(), 2);

        let bash = &pkgs[0];
        assert_eq!(bash.name, "bash");
        assert_eq!(bash.evr(), "5.2.15-1.amzn2023");
        assert_eq!(bash.nevra(), "bash-5.2.15-1.amzn2023.x86_64");
        assert_eq!(bash.size, 1234);
        assert_eq!(bash.summary, "The GNU Bourne Again shell");
        assert_eq!(bash.checksum.hex, "deadbeef"); // not the <format> one
        assert_eq!(bash.repo_id, "baseos");

        // Provides / requires / files parsed from <format>.
        assert_eq!(bash.provides.len(), 2);
        assert_eq!(bash.provides[0].name, "bash");
        assert_eq!(bash.provides[0].flag, rum_solve::DepFlag::Eq);
        assert_eq!(bash.requires.len(), 2);
        assert_eq!(bash.requires[0].name, "glibc");
        assert_eq!(bash.requires[0].flag, rum_solve::DepFlag::Ge);
        assert!(bash.requires[1].evr.is_none()); // /bin/sh unversioned
        assert_eq!(bash.recommends.len(), 1);
        assert_eq!(bash.recommends[0].name, "bash-completion");
        assert_eq!(bash.obsoletes.len(), 1);
        assert_eq!(bash.obsoletes[0].name, "bash-legacy");
        assert_eq!(bash.obsoletes[0].flag, rum_solve::DepFlag::Lt);
        assert_eq!(bash.conflicts.len(), 1);
        assert_eq!(bash.conflicts[0].name, "bash-broken");
        assert_eq!(bash.files, vec!["/usr/bin/bash", "/etc/bash"]);

        let zlib = &pkgs[1];
        assert_eq!(zlib.evr(), "2:1.2.13-3");
    }
}
