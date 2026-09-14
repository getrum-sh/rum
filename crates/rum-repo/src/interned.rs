//! Compact, interned representation of repository metadata.
//!
//! RHEL's `primary.xml` holds millions of small strings (file paths, capability
//! names, arches, versions). Storing each as an owned `String` blew up a small
//! host's RAM during parse (a 761MB t2.micro OOM'd on RHEL BaseOS). Here every
//! string is interned once into a `Vec<String>` arena and referenced by a
//! 4-byte symbol; packages hold symbols, not strings. The arena + packages are
//! exactly what we rkyv-serialize, so the mmap'd warm cache is the same compact
//! form and resolving a symbol is a zero-copy slice into the mapped arena.
//!
//! Encapsulation ("the architectural wall"): `lasso` is used *only* as the
//! parse-time interner and its `Spur` never leaves this module — the stored
//! symbol type is a plain `u32`, which (unlike `lasso::Rodeo`) round-trips
//! through rkyv/mmap. Everything outside rum-repo sees only `&str`.

use lasso::{Key, Rodeo};

use crate::checksum::ChecksumKind;
use rum_solve::{Dep, DepFlag, Evr};

/// A symbol: index into [`Store::strings`].
pub type Sym = u32;

/// A dependency capability with interned name and (optional) EVR string.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct IDep {
    pub name: Sym,
    /// Interned EVR string (`[epoch:]version[-release]`); `None` if unversioned.
    pub evr: Option<Sym>,
    pub flag: DepFlag,
}

/// A package with every string field interned to a [`Sym`].
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct IPackage {
    pub name: Sym,
    pub epoch: u64,
    pub version: Sym,
    pub release: Sym,
    pub arch: Sym,
    pub summary: Sym,
    pub size: u64,
    pub location: Sym,
    pub checksum_kind: ChecksumKind,
    pub checksum_hex: Sym,
    pub repo_id: Sym,
    pub provides: Vec<IDep>,
    pub requires: Vec<IDep>,
    pub recommends: Vec<IDep>,
    pub obsoletes: Vec<IDep>,
    pub conflicts: Vec<IDep>,
    pub files: Vec<Sym>,
}

/// A sorted index entry mapping an interned name/capability symbol to a package index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct IndexEntry {
    pub key: Sym,
    pub pkg_idx: u32,
}

/// The whole cache: the string arena plus the interned packages,
/// along with binary-searchable sorted name and capability indices.
/// This is the rkyv root written to `primary.rkyv`.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Store {
    /// `strings[sym]` is the text for symbol `sym`.
    pub strings: Vec<String>,
    pub packages: Vec<IPackage>,
    /// Index sorted alphabetically by package name (`strings[key]`).
    pub name_index: Vec<IndexEntry>,
    /// Index sorted alphabetically by capability/file provide name (`strings[key]`).
    pub provide_index: Vec<IndexEntry>,
}

/// Parse-time interner. Wraps `lasso::Rodeo`; `Spur` is confined here and only
/// `u32` symbols are handed out / stored.
pub struct Interner {
    rodeo: Rodeo,
}

impl Interner {
    pub fn new() -> Self {
        Interner {
            rodeo: Rodeo::default(),
        }
    }

    /// Intern a string, returning its stable symbol.
    #[inline]
    pub fn intern(&mut self, s: &str) -> Sym {
        self.rodeo.get_or_intern(s).into_usize() as Sym
    }

    /// Intern an optional EVR. Uses the lossless `to_dep_string()` (not Display)
    /// so an explicit `0:` epoch survives the round-trip distinct from an absent
    /// one — the dependency-overlap rule relies on that distinction.
    pub fn intern_evr(&mut self, evr: &Option<Evr>) -> Option<Sym> {
        evr.as_ref().map(|e| self.intern(&e.to_dep_string()))
    }

    /// Materialize the interned strings in symbol order (`strings[sym]`).
    pub fn into_strings(self) -> Vec<String> {
        let mut strings = vec![String::new(); self.rodeo.len()];
        for (sym, text) in self.rodeo.iter() {
            strings[sym.into_usize()] = text.to_owned();
        }
        strings
    }

    /// Finish, materializing the arena in symbol order alongside `packages`
    /// and building the sorted name and provide indices for O(log N) binary search.
    pub fn into_store(self, packages: Vec<IPackage>) -> Store {
        let strings = self.into_strings();

        let mut name_index = Vec::with_capacity(packages.len());
        for (idx, p) in packages.iter().enumerate() {
            name_index.push(IndexEntry {
                key: p.name,
                pkg_idx: idx as u32,
            });
        }
        name_index.sort_by(|a, b| {
            strings[a.key as usize]
                .cmp(&strings[b.key as usize])
                .then_with(|| a.pkg_idx.cmp(&b.pkg_idx))
        });

        let mut provide_index = Vec::new();
        for (idx, p) in packages.iter().enumerate() {
            let pkg_idx = idx as u32;
            for prov in &p.provides {
                provide_index.push(IndexEntry {
                    key: prov.name,
                    pkg_idx,
                });
            }
            for &file_sym in &p.files {
                provide_index.push(IndexEntry {
                    key: file_sym,
                    pkg_idx,
                });
            }
        }
        provide_index.sort_by(|a, b| {
            strings[a.key as usize]
                .cmp(&strings[b.key as usize])
                .then_with(|| a.pkg_idx.cmp(&b.pkg_idx))
        });
        provide_index.dedup_by(|a, b| a.key == b.key && a.pkg_idx == b.pkg_idx);

        Store {
            strings,
            packages,
            name_index,
            provide_index,
        }
    }
}

impl Default for Interner {
    fn default() -> Self {
        Self::new()
    }
}

// --- Zero-copy `&str` views over the archived (mmap'd) store ----------------

impl ArchivedStore {
    #[inline]
    fn sym(&self, sym: Sym) -> &str {
        self.strings[sym as usize].as_str()
    }

    /// Iterate borrowed views over every package.
    pub fn views(&self) -> impl Iterator<Item = PkgView<'_>> {
        self.packages
            .iter()
            .map(move |pkg| PkgView { store: self, pkg })
    }

    /// Return a borrowed view of package at `idx`.
    pub fn view_at(&self, idx: usize) -> Option<PkgView<'_>> {
        self.packages
            .get(idx)
            .map(|pkg| PkgView { store: self, pkg })
    }

    pub fn len(&self) -> usize {
        self.packages.len()
    }

    /// Find all package indices with exact package name `name` in O(log N) time.
    pub fn find_by_name<'a>(&'a self, name: &'a str) -> impl Iterator<Item = usize> + 'a {
        let start = self
            .name_index
            .partition_point(|entry| self.sym(entry.key.to_native()) < name);
        self.name_index[start..]
            .iter()
            .take_while(move |entry| self.sym(entry.key.to_native()) == name)
            .map(|entry| entry.pkg_idx.to_native() as usize)
    }

    /// Find all package indices providing capability or file `cap` in O(log N) time.
    pub fn find_by_provide<'a>(&'a self, cap: &'a str) -> impl Iterator<Item = usize> + 'a {
        let start = self
            .provide_index
            .partition_point(|entry| self.sym(entry.key.to_native()) < cap);
        self.provide_index[start..]
            .iter()
            .take_while(move |entry| self.sym(entry.key.to_native()) == cap)
            .map(|entry| entry.pkg_idx.to_native() as usize)
    }
}

/// A borrowed, `&str`-only view of one archived package. This is the only
/// package shape that leaves rum-repo for the query commands.
pub struct PkgView<'a> {
    store: &'a ArchivedStore,
    pkg: &'a ArchivedIPackage,
}

impl<'a> PkgView<'a> {
    #[inline]
    fn s(&self, sym: Sym) -> &'a str {
        self.store.sym(sym)
    }
    pub fn name(&self) -> &'a str {
        self.s(self.pkg.name.to_native())
    }
    pub fn arch(&self) -> &'a str {
        self.s(self.pkg.arch.to_native())
    }
    pub fn version(&self) -> &'a str {
        self.s(self.pkg.version.to_native())
    }
    pub fn release(&self) -> &'a str {
        self.s(self.pkg.release.to_native())
    }
    pub fn summary(&self) -> &'a str {
        self.s(self.pkg.summary.to_native())
    }
    pub fn repo_id(&self) -> &'a str {
        self.s(self.pkg.repo_id.to_native())
    }
    pub fn epoch(&self) -> u64 {
        self.pkg.epoch.to_native()
    }
    pub fn name_arch(&self) -> String {
        format!("{}.{}", self.name(), self.arch())
    }
    pub fn evr(&self) -> String {
        if self.epoch() == 0 {
            format!("{}-{}", self.version(), self.release())
        } else {
            format!("{}:{}-{}", self.epoch(), self.version(), self.release())
        }
    }
    pub fn evr_cmp(&self) -> Evr {
        Evr::new(Some(self.epoch()), self.version(), self.release())
    }
    pub fn checksum_hex(&self) -> &'a str {
        self.s(self.pkg.checksum_hex.to_native())
    }

    /// Provided capability names (excluding files), borrowed from the arena.
    pub fn provide_names(&self) -> Vec<&'a str> {
        self.pkg
            .provides
            .iter()
            .map(|d| self.s(d.name.to_native()))
            .collect()
    }
    /// Required capability names (may include file paths), borrowed.
    pub fn require_names(&self) -> Vec<&'a str> {
        self.pkg
            .requires
            .iter()
            .map(|d| self.s(d.name.to_native()))
            .collect()
    }
    /// Recommended (weak) capability names, borrowed.
    pub fn recommend_names(&self) -> Vec<&'a str> {
        self.pkg
            .recommends
            .iter()
            .map(|d| self.s(d.name.to_native()))
            .collect()
    }
    /// Advertised file paths, borrowed.
    pub fn file_names(&self) -> Vec<&'a str> {
        self.pkg
            .files
            .iter()
            .map(|f| self.s(f.to_native()))
            .collect()
    }

    /// Owned `Dep`s for the resolver's hard requires.
    pub fn requires(&self) -> Vec<Dep> {
        self.pkg
            .requires
            .iter()
            .map(|d| self.store.dep(d))
            .collect()
    }
    /// Owned `Dep`s for weak (Recommends) deps.
    pub fn recommends(&self) -> Vec<Dep> {
        self.pkg
            .recommends
            .iter()
            .map(|d| self.store.dep(d))
            .collect()
    }
    /// Owned `Dep`s for Obsoletes.
    pub fn obsoletes(&self) -> Vec<Dep> {
        self.pkg
            .obsoletes
            .iter()
            .map(|d| self.store.dep(d))
            .collect()
    }
    /// Owned `Dep`s for Conflicts.
    pub fn conflicts(&self) -> Vec<Dep> {
        self.pkg
            .conflicts
            .iter()
            .map(|d| self.store.dep(d))
            .collect()
    }
    /// Conflict capability names, borrowed (for the required-set pre-pass).
    pub fn conflict_names(&self) -> Vec<&'a str> {
        self.pkg
            .conflicts
            .iter()
            .map(|d| self.s(d.name.to_native()))
            .collect()
    }
    /// Provides plus advertised files (as unversioned provides), keeping only
    /// capabilities whose name is in `keep`. Resolving the name (cheap `&str`)
    /// before building the `Dep` avoids allocating for the many never-required
    /// file provides in a distro's metadata.
    pub fn provides_with_files_filtered(
        &self,
        keep: &std::collections::HashSet<String>,
    ) -> Vec<Dep> {
        let mut v: Vec<Dep> = Vec::new();
        for d in self.pkg.provides.iter() {
            if keep.contains(self.s(d.name.to_native())) {
                v.push(self.store.dep(d));
            }
        }
        for f in self.pkg.files.iter() {
            let name = self.s(f.to_native());
            if keep.contains(name) {
                v.push(Dep::unversioned(name));
            }
        }
        v
    }
}

// --- Materialize owned `AvailablePackage`s (resolve/download path) ----------

impl ArchivedStore {
    /// Resolve every archived package into an owned [`AvailablePackage`].
    pub fn to_owned_packages(&self) -> Vec<crate::AvailablePackage> {
        self.packages.iter().map(|p| self.owned(p)).collect()
    }

    /// Rehydrate a single package by index (used to materialize only the
    /// resolver's winning set, not the whole repo).
    pub fn package_at(&self, idx: usize) -> Option<crate::AvailablePackage> {
        self.packages.get(idx).map(|p| self.owned(p))
    }

    fn owned(&self, p: &ArchivedIPackage) -> crate::AvailablePackage {
        crate::AvailablePackage {
            name: self.sym(p.name.to_native()).to_string(),
            epoch: p.epoch.to_native(),
            version: self.sym(p.version.to_native()).to_string(),
            release: self.sym(p.release.to_native()).to_string(),
            arch: self.sym(p.arch.to_native()).to_string(),
            summary: self.sym(p.summary.to_native()).to_string(),
            size: p.size.to_native(),
            location: self.sym(p.location.to_native()).to_string(),
            checksum: crate::Checksum {
                kind: checksum_kind(&p.checksum_kind),
                hex: self.sym(p.checksum_hex.to_native()).to_string(),
            },
            repo_id: self.sym(p.repo_id.to_native()).to_string(),
            provides: p.provides.iter().map(|d| self.dep(d)).collect(),
            requires: p.requires.iter().map(|d| self.dep(d)).collect(),
            recommends: p.recommends.iter().map(|d| self.dep(d)).collect(),
            obsoletes: p.obsoletes.iter().map(|d| self.dep(d)).collect(),
            conflicts: p.conflicts.iter().map(|d| self.dep(d)).collect(),
            files: p
                .files
                .iter()
                .map(|f| self.sym(f.to_native()).to_string())
                .collect(),
        }
    }

    fn dep(&self, d: &ArchivedIDep) -> Dep {
        Dep {
            name: self.sym(d.name.to_native()).to_string(),
            flag: dep_flag(&d.flag),
            evr: d.evr.as_ref().map(|s| Evr::parse(self.sym(s.to_native()))),
        }
    }
}

fn dep_flag(a: &ArchivedDepFlag) -> DepFlag {
    match a {
        ArchivedDepFlag::Any => DepFlag::Any,
        ArchivedDepFlag::Eq => DepFlag::Eq,
        ArchivedDepFlag::Lt => DepFlag::Lt,
        ArchivedDepFlag::Le => DepFlag::Le,
        ArchivedDepFlag::Gt => DepFlag::Gt,
        ArchivedDepFlag::Ge => DepFlag::Ge,
    }
}

fn checksum_kind(a: &ArchivedChecksumKind) -> ChecksumKind {
    match a {
        ArchivedChecksumKind::Sha1 => ChecksumKind::Sha1,
        ArchivedChecksumKind::Sha224 => ChecksumKind::Sha224,
        ArchivedChecksumKind::Sha256 => ChecksumKind::Sha256,
        ArchivedChecksumKind::Sha384 => ChecksumKind::Sha384,
        ArchivedChecksumKind::Sha512 => ChecksumKind::Sha512,
    }
}

// Bring the archived enum names into scope for the match arms above.
use crate::checksum::ArchivedChecksumKind;
use rum_solve::ArchivedDepFlag;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_search_indices() {
        let mut itn = Interner::new();
        let sym_bash = itn.intern("bash");
        let sym_zsh = itn.intern("zsh");
        let sym_coreutils = itn.intern("coreutils");
        let sym_v1 = itn.intern("1.0");
        let sym_v2 = itn.intern("2.0");
        let sym_r1 = itn.intern("1");
        let sym_x86_64 = itn.intern("x86_64");
        let sym_aarch64 = itn.intern("aarch64");
        let sym_empty = itn.intern("");
        let sym_bin_sh = itn.intern("/bin/sh");
        let sym_shell = itn.intern("shell");
        let sym_cat = itn.intern("/bin/cat");

        let p_bash_x86 = IPackage {
            name: sym_bash,
            epoch: 0,
            version: sym_v1,
            release: sym_r1,
            arch: sym_x86_64,
            summary: sym_empty,
            size: 100,
            location: sym_empty,
            checksum_kind: ChecksumKind::Sha256,
            checksum_hex: sym_empty,
            repo_id: sym_empty,
            provides: vec![IDep {
                name: sym_shell,
                evr: None,
                flag: DepFlag::Any,
            }],
            requires: Vec::new(),
            recommends: Vec::new(),
            obsoletes: Vec::new(),
            conflicts: Vec::new(),
            files: vec![sym_bin_sh],
        };

        let p_zsh = IPackage {
            name: sym_zsh,
            epoch: 0,
            version: sym_v1,
            release: sym_r1,
            arch: sym_x86_64,
            summary: sym_empty,
            size: 200,
            location: sym_empty,
            checksum_kind: ChecksumKind::Sha256,
            checksum_hex: sym_empty,
            repo_id: sym_empty,
            provides: vec![IDep {
                name: sym_shell,
                evr: None,
                flag: DepFlag::Any,
            }],
            requires: Vec::new(),
            recommends: Vec::new(),
            obsoletes: Vec::new(),
            conflicts: Vec::new(),
            files: Vec::new(),
        };

        let p_bash_arm = IPackage {
            name: sym_bash,
            epoch: 0,
            version: sym_v2,
            release: sym_r1,
            arch: sym_aarch64,
            summary: sym_empty,
            size: 100,
            location: sym_empty,
            checksum_kind: ChecksumKind::Sha256,
            checksum_hex: sym_empty,
            repo_id: sym_empty,
            provides: vec![IDep {
                name: sym_shell,
                evr: None,
                flag: DepFlag::Any,
            }],
            requires: Vec::new(),
            recommends: Vec::new(),
            obsoletes: Vec::new(),
            conflicts: Vec::new(),
            files: vec![sym_bin_sh],
        };

        let p_coreutils = IPackage {
            name: sym_coreutils,
            epoch: 0,
            version: sym_v1,
            release: sym_r1,
            arch: sym_x86_64,
            summary: sym_empty,
            size: 500,
            location: sym_empty,
            checksum_kind: ChecksumKind::Sha256,
            checksum_hex: sym_empty,
            repo_id: sym_empty,
            provides: Vec::new(),
            requires: Vec::new(),
            recommends: Vec::new(),
            obsoletes: Vec::new(),
            conflicts: Vec::new(),
            files: vec![sym_cat],
        };

        let store = itn.into_store(vec![p_bash_x86, p_zsh, p_bash_arm, p_coreutils]);

        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&store).unwrap();
        let archived = rkyv::access::<ArchivedStore, rkyv::rancor::Error>(&bytes).unwrap();

        // 1. Binary search by package name
        let bash_matches: Vec<usize> = archived.find_by_name("bash").collect();
        assert_eq!(bash_matches.len(), 2);
        assert!(bash_matches.contains(&0));
        assert!(bash_matches.contains(&2));

        let zsh_matches: Vec<usize> = archived.find_by_name("zsh").collect();
        assert_eq!(zsh_matches, vec![1]);

        let coreutils_matches: Vec<usize> = archived.find_by_name("coreutils").collect();
        assert_eq!(coreutils_matches, vec![3]);

        let none_matches: Vec<usize> = archived.find_by_name("nonexistent").collect();
        assert!(none_matches.is_empty());

        // 2. Binary search by provide (capabilities and advertised files)
        let shell_providers: Vec<usize> = archived.find_by_provide("shell").collect();
        assert_eq!(shell_providers.len(), 3);
        assert!(shell_providers.contains(&0));
        assert!(shell_providers.contains(&1));
        assert!(shell_providers.contains(&2));

        let sh_file_providers: Vec<usize> = archived.find_by_provide("/bin/sh").collect();
        assert_eq!(sh_file_providers.len(), 2);
        assert!(sh_file_providers.contains(&0));
        assert!(sh_file_providers.contains(&2));

        let cat_file_providers: Vec<usize> = archived.find_by_provide("/bin/cat").collect();
        assert_eq!(cat_file_providers, vec![3]);

        let none_providers: Vec<usize> = archived.find_by_provide("nonexistent_cap").collect();
        assert!(none_providers.is_empty());
    }
}
