//! Test harness for resolver tests, modeled on dnf/hawkey's libsolv "testtags"
//! fixtures: declare synthetic packages with a terse DSL, run a resolve, and
//! assert on the resulting NEVRA set (never on internal candidate ids).
//!
//! This is the in-code analogue of hawkey's `=Pkg:/=Req:/=Prv:` `.repo` files —
//! it feeds owned [`Candidate`]s straight to [`resolve_sat`], so pure resolver
//! logic (dependency closure, backtracking, EVR matching, multilib) is tested
//! with no I/O, no rpm, and no network.
//!
//! Currently gated `#[cfg(test)]` (used by in-crate tests). Promote to a
//! `test-support` cargo feature when rum-repo/integration tests need it too.

use crate::resolve::{Candidate, PresentPackage, ResolveError};
use crate::{
    resolve_sat, resolve_sat_scoped, resolve_sat_scoped_opt, resolve_sat_with_opt, Dep, DepFlag,
    Evr,
};

const ARCHES: &[&str] = &[
    "x86_64", "i686", "i386", "aarch64", "noarch", "armv7hl", "ppc64le", "s390x", "riscv64",
];

/// A synthetic package universe: available candidates plus the installed set.
pub struct TestRepo {
    avail: Vec<Candidate>,
    installed: Vec<(String, Option<Evr>)>,
    /// Scoped *present* (erasable) installed packages, with a parallel NEVRA
    /// label per entry for erase-set assertions.
    present: Vec<PresentPackage>,
    present_nevra: Vec<String>,
}

impl Default for TestRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl TestRepo {
    pub fn new() -> Self {
        TestRepo {
            avail: Vec::new(),
            installed: Vec::new(),
            present: Vec::new(),
            present_nevra: Vec::new(),
        }
    }

    /// Declare an available package by NEVRA (`"walrus-2-5.noarch"`,
    /// `"baby-6:4.9-3.x86_64"`; arch optional, defaults x86_64). Chain
    /// `.requires`/`.provides`/`.recommends` to add relations.
    pub fn pkg(&mut self, nevra: &str) -> PkgB<'_> {
        let (name, evr, arch) = parse_nevra(nevra);
        let id = self.avail.len();
        self.avail.push(Candidate {
            id,
            name,
            arch,
            evr,
            provides: Vec::new(),
            requires: Vec::new(),
            recommends: Vec::new(),
            conflicts: Vec::new(),
            priority: 99,
        });
        PkgB {
            c: self.avail.last_mut().unwrap(),
        }
    }

    /// Mark an installed capability (the `@System.repo` analogue). `cap` is a
    /// dep string: `"bash"` or `"bash = 5.2"`.
    pub fn installed(&mut self, cap: &str) -> &mut Self {
        let d = parse_dep(cap);
        self.installed.push((d.name, d.evr));
        self
    }

    /// Declare a scoped *present* (erasable) installed package by NEVRA. Unlike
    /// [`installed`](Self::installed) (a dependency-free capability), a present
    /// package carries real relations and can be *erased* by the resolver.
    /// Chain `.requires`/`.provides`.
    pub fn present(&mut self, nevra: &str) -> PresentB<'_> {
        let (name, evr, arch) = parse_nevra(nevra);
        let id = self.present.len();
        self.present_nevra.push(format!("{name}-{evr}.{arch}"));
        self.present.push(PresentPackage {
            id,
            name,
            evr,
            provides: Vec::new(),
            requires: Vec::new(),
        });
        PresentB {
            p: self.present.last_mut().unwrap(),
        }
    }

    /// Resolve `want` (package name/`name.arch` specs) and return the install
    /// set as sorted NEVRA strings.
    pub fn resolve(&self, want: &[&str]) -> Result<Vec<String>, ResolveError> {
        let req: Vec<String> = want.iter().map(|s| s.to_string()).collect();
        let res = resolve_sat(&req, &self.avail, &self.installed)?;
        let mut out: Vec<String> = res
            .to_install
            .iter()
            .map(|id| {
                let c = &self.avail[*id];
                format!("{}-{}.{}", c.name, c.evr, c.arch)
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// Resolve `want` with explicit control over weak dependencies.
    pub fn resolve_opt(
        &self,
        want: &[&str],
        include_recommends: bool,
    ) -> Result<Vec<String>, ResolveError> {
        let req: Vec<String> = want.iter().map(|s| s.to_string()).collect();
        let res = resolve_sat_with_opt(
            &req,
            self.avail.as_slice(),
            &self.installed,
            include_recommends,
        )?;
        let mut out: Vec<String> = res
            .to_install
            .iter()
            .map(|id| {
                let c = &self.avail[*id];
                format!("{}-{}.{}", c.name, c.evr, c.arch)
            })
            .collect();
        out.sort();
        Ok(out)
    }

    /// Erase-aware resolve: returns `(installs, erases)` as sorted NEVRA sets.
    /// `erases` are drawn from the declared [`present`](Self::present) packages
    /// the solver chose to drop.
    pub fn resolve_scoped(
        &self,
        want: &[&str],
    ) -> Result<(Vec<String>, Vec<String>), ResolveError> {
        let req: Vec<String> = want.iter().map(|s| s.to_string()).collect();
        let res = resolve_sat_scoped(&req, self.avail.as_slice(), &self.installed, &self.present)?;
        let mut installs: Vec<String> = res
            .to_install
            .iter()
            .map(|id| {
                let c = &self.avail[*id];
                format!("{}-{}.{}", c.name, c.evr, c.arch)
            })
            .collect();
        installs.sort();
        let mut erases: Vec<String> = res
            .to_erase
            .iter()
            .map(|id| self.present_nevra[*id].clone())
            .collect();
        erases.sort();
        Ok((installs, erases))
    }

    /// Erase-aware resolve with explicit weak dependency control: returns `(installs, erases)`.
    pub fn resolve_scoped_opt(
        &self,
        want: &[&str],
        include_recommends: bool,
    ) -> Result<(Vec<String>, Vec<String>), ResolveError> {
        let req: Vec<String> = want.iter().map(|s| s.to_string()).collect();
        let res = resolve_sat_scoped_opt(
            &req,
            self.avail.as_slice(),
            &self.installed,
            &self.present,
            include_recommends,
        )?;
        let mut installs: Vec<String> = res
            .to_install
            .iter()
            .map(|id| {
                let c = &self.avail[*id];
                format!("{}-{}.{}", c.name, c.evr, c.arch)
            })
            .collect();
        installs.sort();
        let mut erases: Vec<String> = res
            .to_erase
            .iter()
            .map(|id| self.present_nevra[*id].clone())
            .collect();
        erases.sort();
        Ok((installs, erases))
    }
}

/// Builder for one present package's relations (returned by [`TestRepo::present`]).
pub struct PresentB<'a> {
    p: &'a mut PresentPackage,
}

impl PresentB<'_> {
    pub fn requires(self, d: &str) -> Self {
        self.p.requires.push(parse_dep(d));
        self
    }
    pub fn provides(self, d: &str) -> Self {
        self.p.provides.push(parse_dep(d));
        self
    }
}

/// Builder for one package's relations (returned by [`TestRepo::pkg`]).
pub struct PkgB<'a> {
    c: &'a mut Candidate,
}

impl PkgB<'_> {
    pub fn requires(self, d: &str) -> Self {
        self.c.requires.push(parse_dep(d));
        self
    }
    pub fn provides(self, d: &str) -> Self {
        self.c.provides.push(parse_dep(d));
        self
    }
    pub fn recommends(self, d: &str) -> Self {
        self.c.recommends.push(parse_dep(d));
        self
    }
    pub fn conflicts(self, d: &str) -> Self {
        self.c.conflicts.push(parse_dep(d));
        self
    }
    /// Set the package's repo priority (lower is preferred; default 99).
    pub fn priority(self, p: i32) -> Self {
        self.c.priority = p;
        self
    }
}

/// Assert the resolve of `want` installs exactly `expected` (NEVRA set); extras
/// or omissions fail (the analogue of dnf's `assertResult`).
pub fn assert_installs(repo: &TestRepo, want: &[&str], expected: &[&str]) {
    let got = repo.resolve(want).expect("resolve should succeed");
    let mut exp: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    exp.sort();
    assert_eq!(got, exp, "resolved set mismatch");
}

/// Assert the erase-aware resolve of `want` installs exactly `expect_installs`
/// and erases exactly `expect_erases` (both NEVRA sets).
pub fn assert_scoped(
    repo: &TestRepo,
    want: &[&str],
    expect_installs: &[&str],
    expect_erases: &[&str],
) {
    let (installs, erases) = repo.resolve_scoped(want).expect("resolve should succeed");
    let mut ei: Vec<String> = expect_installs.iter().map(|s| s.to_string()).collect();
    ei.sort();
    let mut ee: Vec<String> = expect_erases.iter().map(|s| s.to_string()).collect();
    ee.sort();
    assert_eq!(installs, ei, "install set mismatch");
    assert_eq!(erases, ee, "erase set mismatch");
}

/// Assert `want` cannot be resolved (the analogue of hawkey `assertFalse(run())`).
pub fn assert_unsolvable(repo: &TestRepo, want: &[&str]) {
    assert!(
        repo.resolve(want).is_err(),
        "expected an unsatisfiable resolve for {want:?}"
    );
}

/// Parse `name[-[epoch:]version-release][.arch]` into (name, Evr, arch).
fn parse_nevra(s: &str) -> (String, Evr, String) {
    let (rest, arch) = match s.rsplit_once('.') {
        Some((r, a)) if ARCHES.contains(&a) => (r, a.to_string()),
        _ => (s, "x86_64".to_string()),
    };
    let (name_ver, rel) = rest
        .rsplit_once('-')
        .unwrap_or_else(|| panic!("NEVRA {s:?} needs a `-version-release`"));
    let (name, ver) = name_ver
        .rsplit_once('-')
        .unwrap_or_else(|| panic!("NEVRA {s:?} needs a `-version-release`"));
    (name.to_string(), Evr::parse(&format!("{ver}-{rel}")), arch)
}

/// Parse a dep string: `"name"` (unversioned) or `"name OP evr"` where OP is one
/// of `= < <= > >=` (or the RPM sense words `EQ LT LE GT GE`).
fn parse_dep(s: &str) -> Dep {
    let toks: Vec<&str> = s.split_whitespace().collect();
    match toks.as_slice() {
        [name] => Dep::unversioned(*name),
        [name, op, ver] => {
            let flag = match *op {
                "=" | "EQ" => DepFlag::Eq,
                "<" | "LT" => DepFlag::Lt,
                "<=" | "LE" => DepFlag::Le,
                ">" | "GT" => DepFlag::Gt,
                ">=" | "GE" => DepFlag::Ge,
                other => panic!("bad dep operator {other:?} in {s:?}"),
            };
            Dep {
                name: (*name).to_string(),
                flag,
                evr: Some(Evr::parse(ver)),
            }
        }
        _ => panic!("bad dep string {s:?} (want `name` or `name OP evr`)"),
    }
}
