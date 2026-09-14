//! `comps.xml` (package groups & environments) parsing and a compact,
//! mmap-friendly cache — the data behind `rum group ...` / `rum install @grp`.
//!
//! Mirrors the interned-store design used for primary metadata: `comps.xml` is
//! stream-parsed with quick-xml, every string interned once into a `Vec<String>`
//! arena (via the shared [`crate::interned::Interner`]), and the result baked
//! into an rkyv archive (`comps.rkyv`) that the warm path mmaps. Group/env
//! lookup is a linear scan over the (few dozen) entries — no hash index needed
//! at this scale; the win is the zero-copy mmap and interned arena.
//!
//! Only the fields needed to expand a group into package names are kept: ids,
//! default (non-localized) names, and package requirements. Localized
//! names/descriptions and `<category>` are dropped.

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::interned::Interner;
use crate::RepoError;

/// How a package participates in a group (comps `packagereq type=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(derive(Debug, PartialEq, Eq))]
pub enum PkgReqType {
    Mandatory,
    Default,
    Optional,
    /// Installed only if its `requires=` trigger package is installed.
    Conditional,
}

/// A package's membership in a group. Strings are arena symbols.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct CompsPkgRef {
    pub name: u32,
    pub req: PkgReqType,
    /// Trigger package for `Conditional` reqs (arena symbol), else `None`.
    pub condition: Option<u32>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct CompsGroup {
    pub id: u32,
    pub name: u32,
    pub packages: Vec<CompsPkgRef>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct CompsEnvironment {
    pub id: u32,
    pub name: u32,
    /// Group ids (arena symbols) mandatory for this environment.
    pub groups: Vec<u32>,
    /// Optional group ids (excluded by default, like dnf).
    pub option_groups: Vec<u32>,
}

/// The whole comps cache: string arena + groups + environments.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct CompsStore {
    pub strings: Vec<String>,
    pub groups: Vec<CompsGroup>,
    pub environments: Vec<CompsEnvironment>,
}

fn parse_req(v: &[u8]) -> PkgReqType {
    match v {
        b"mandatory" => PkgReqType::Mandatory,
        b"optional" => PkgReqType::Optional,
        b"conditional" => PkgReqType::Conditional,
        // comps' default when unset is "default".
        _ => PkgReqType::Default,
    }
}

/// Which text-bearing element we're currently capturing.
#[derive(Clone, Copy, PartialEq)]
enum Field {
    None,
    Id,
    Name,
    PackageReq,
    GroupId,
}

#[derive(Default)]
struct RawGroup {
    id: Option<u32>,
    name: Option<u32>,
    packages: Vec<CompsPkgRef>,
}

#[derive(Default)]
struct RawEnv {
    id: Option<u32>,
    name: Option<u32>,
    groups: Vec<u32>,
    option_groups: Vec<u32>,
}

/// Parse a decompressed `comps.xml` stream into an interned [`CompsStore`].
pub fn parse_reader<R: std::io::Read>(input: R) -> Result<CompsStore, RepoError> {
    let mut reader = Reader::from_reader(std::io::BufReader::new(input));
    reader.config_mut().trim_text(true);

    let mut itn = Interner::new();
    let mut groups: Vec<CompsGroup> = Vec::new();
    let mut environments: Vec<CompsEnvironment> = Vec::new();
    let mut buf = Vec::new();

    let mut group: Option<RawGroup> = None;
    let mut env: Option<RawEnv> = None;
    let mut field = Field::None;
    // Only the first (default, non-localized) <id>/<name> is captured.
    let mut skip_localized_name = false;
    // Environment <grouplist> vs <optionlist> for routing <groupid>.
    let mut in_optionlist = false;
    // Pending <packagereq> attributes.
    let mut req_type = PkgReqType::Default;
    let mut req_cond: Option<u32> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_vec();
                match name.as_slice() {
                    b"group" => group = Some(RawGroup::default()),
                    b"environment" => env = Some(RawEnv::default()),
                    b"id" => field = Field::Id,
                    b"name" => {
                        // Skip localized names (xml:lang / lang attribute present).
                        skip_localized_name = has_lang(&e);
                        field = Field::Name;
                    }
                    b"optionlist" => in_optionlist = true,
                    b"grouplist" => in_optionlist = false,
                    b"groupid" => field = Field::GroupId,
                    b"packagereq" => {
                        req_type = attr_opt(&e, b"type")
                            .map(|v| parse_req(&v))
                            .unwrap_or(PkgReqType::Default);
                        req_cond = attr_opt(&e, b"requires")
                            .and_then(|v| std::str::from_utf8(&v).ok().map(|s| itn.intern(s)));
                        field = Field::PackageReq;
                    }
                    _ => field = Field::None,
                }
            }
            Ok(Event::Text(t)) => {
                let text = t.unescape().unwrap_or_default();
                match field {
                    Field::Id => {
                        let sym = itn.intern(&text);
                        if let Some(g) = group.as_mut() {
                            g.id.get_or_insert(sym);
                        } else if let Some(ev) = env.as_mut() {
                            ev.id.get_or_insert(sym);
                        }
                    }
                    Field::Name if !skip_localized_name => {
                        let sym = itn.intern(&text);
                        if let Some(g) = group.as_mut() {
                            g.name.get_or_insert(sym);
                        } else if let Some(ev) = env.as_mut() {
                            ev.name.get_or_insert(sym);
                        }
                    }
                    Field::PackageReq => {
                        if let Some(g) = group.as_mut() {
                            g.packages.push(CompsPkgRef {
                                name: itn.intern(&text),
                                req: req_type,
                                condition: req_cond,
                            });
                        }
                    }
                    Field::GroupId => {
                        if let Some(ev) = env.as_mut() {
                            let sym = itn.intern(&text);
                            if in_optionlist {
                                ev.option_groups.push(sym);
                            } else {
                                ev.groups.push(sym);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = e.local_name().as_ref().to_vec();
                match name.as_slice() {
                    b"group" => {
                        if let Some(g) = group.take() {
                            if let (Some(id), name) = (g.id, g.name) {
                                groups.push(CompsGroup {
                                    id,
                                    name: name.unwrap_or(id),
                                    packages: g.packages,
                                });
                            }
                        }
                    }
                    b"environment" => {
                        if let Some(ev) = env.take() {
                            if let (Some(id), name) = (ev.id, ev.name) {
                                environments.push(CompsEnvironment {
                                    id,
                                    name: name.unwrap_or(id),
                                    groups: ev.groups,
                                    option_groups: ev.option_groups,
                                });
                            }
                        }
                    }
                    b"packagereq" => req_cond = None,
                    _ => {}
                }
                field = Field::None;
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(RepoError::Xml(format!("comps.xml: {e}"))),
            _ => {}
        }
        buf.clear();
    }

    Ok(CompsStore {
        strings: itn.into_strings(),
        groups,
        environments,
    })
}

fn has_lang(e: &quick_xml::events::BytesStart) -> bool {
    e.attributes()
        .flatten()
        .any(|a| a.key.local_name().as_ref() == b"lang")
}

fn attr_opt(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<Vec<u8>> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == key)
        .map(|a| a.value.into_owned())
}

// --- Zero-copy read side (over the mmap'd archive) --------------------------

impl ArchivedCompsStore {
    #[inline]
    pub fn sym(&self, s: u32) -> &str {
        self.strings[s as usize].as_str()
    }

    /// A group matching `target` by id or (case-insensitive) name.
    pub fn find_group(&self, target: &str) -> Option<&ArchivedCompsGroup> {
        self.groups.iter().find(|g| {
            self.sym(g.id.to_native()) == target
                || self.sym(g.name.to_native()).eq_ignore_ascii_case(target)
        })
    }

    /// A group by exact id (used when expanding an environment's group list).
    pub fn group_by_id(&self, id: &str) -> Option<&ArchivedCompsGroup> {
        self.groups
            .iter()
            .find(|g| self.sym(g.id.to_native()) == id)
    }

    /// An environment matching `target` by id or (case-insensitive) name.
    pub fn find_environment(&self, target: &str) -> Option<&ArchivedCompsEnvironment> {
        self.environments.iter().find(|e| {
            self.sym(e.id.to_native()) == target
                || self.sym(e.name.to_native()).eq_ignore_ascii_case(target)
        })
    }

    /// `(id, name)` for every group, for `rum group list`.
    pub fn group_listing(&self) -> Vec<(&str, &str)> {
        self.groups
            .iter()
            .map(|g| (self.sym(g.id.to_native()), self.sym(g.name.to_native())))
            .collect()
    }
}

/// An owned mmap handle over a repo's `comps.rkyv`, exposing the archived store.
pub struct CompsHandle {
    bytes: memmap2::Mmap,
}

impl CompsHandle {
    /// Open + validate a `comps.rkyv` file, returning `None` on any failure
    /// (missing/corrupt cache is non-fatal — the repo simply has no groups).
    pub fn open(path: &std::path::Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?;
        // SAFETY: our own cache file, treated as immutable; validated below.
        let bytes = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        rkyv::access::<ArchivedCompsStore, rkyv::rancor::Error>(&bytes).ok()?;
        Some(CompsHandle { bytes })
    }

    pub fn store(&self) -> &ArchivedCompsStore {
        // SAFETY: validated in `open`; bytes are immutable for the handle's life.
        unsafe { rkyv::access_unchecked::<ArchivedCompsStore>(&self.bytes) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_group_and_environment() {
        let xml = br#"<?xml version="1.0"?>
        <comps>
          <group>
            <id>core</id>
            <name>Core</name>
            <name xml:lang="de">Kern</name>
            <packagelist>
              <packagereq type="mandatory">bash</packagereq>
              <packagereq type="default">vim</packagereq>
              <packagereq type="optional">emacs</packagereq>
              <packagereq type="conditional" requires="grub2">grub2-tools</packagereq>
            </packagelist>
          </group>
          <environment>
            <id>minimal-environment</id>
            <name>Minimal Install</name>
            <grouplist><groupid>core</groupid></grouplist>
            <optionlist><groupid>standard</groupid></optionlist>
          </environment>
          <category><id>skip-me</id></category>
        </comps>"#;

        let store = super::parse_reader(&xml[..]).unwrap();
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&store).unwrap();
        let a = rkyv::access::<ArchivedCompsStore, rkyv::rancor::Error>(&bytes).unwrap();

        // Group lookup by id, and by name (case-insensitive).
        let g = a.find_group("core").unwrap();
        assert_eq!(a.sym(g.id.to_native()), "core");
        assert_eq!(a.sym(a.find_group("cOrE").unwrap().id.to_native()), "core"); // by name
        assert!(a.find_group("nope").is_none());

        assert_eq!(g.packages.len(), 4);
        assert_eq!(a.sym(g.packages[0].name.to_native()), "bash");
        assert_eq!(g.packages[0].req, ArchivedPkgReqType::Mandatory);
        assert_eq!(g.packages[3].req, ArchivedPkgReqType::Conditional);
        assert_eq!(
            a.sym(g.packages[3].condition.as_ref().unwrap().to_native()),
            "grub2"
        );
        // German localized name was skipped.
        assert_eq!(a.sym(g.name.to_native()), "Core");

        let env = a.find_environment("Minimal Install").unwrap();
        assert_eq!(env.groups.len(), 1);
        assert_eq!(a.sym(env.groups[0].to_native()), "core");
        assert_eq!(env.option_groups.len(), 1);
        assert_eq!(a.sym(env.option_groups[0].to_native()), "standard");
    }
}
