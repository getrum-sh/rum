//! SAT-based dependency resolution via the `resolvo` solver.
//!
//! resolvo (like conda's solver) is a name+version CDCL solver with no native
//! concept of RPM "Provides". We bridge that gap:
//!
//!   * Each concrete package is interned once as a solvable under its own name.
//!   * That same solvable is registered as a candidate under *every* capability
//!     it provides (its name, sonames, files, virtual provides). Because it is
//!     the same `SolvableId`, selecting it to satisfy any capability counts as
//!     one install — provides are deduplicated for free.
//!   * The installed system is modeled as preferred "synthetic" solvables (one
//!     per installed capability+version) that carry no dependencies. Sorting
//!     them first makes the solver keep already-installed dependencies instead
//!     of gratuitously upgrading them — matching `dnf install` semantics.
//!
//! Versioned requirements are expressed as `Ranges<Evr>` over our RPM-correct
//! `Evr` ordering, so the solver's version choices use real rpmvercmp. A
//! versioned Provides carries its own advertised version; an *unversioned*
//! Provides is flagged a wildcard and satisfies any require (RPM's rpmdsCompare
//! rule). This yields real backtracking and matches dnf on ordinary closures.

use std::collections::{HashMap, HashSet};
use std::fmt::Display;

use resolvo::utils::Pool;
use resolvo::{
    Candidates, Condition, ConditionId, ConditionalRequirement, Dependencies, DependencyProvider,
    Interner, KnownDependencies, LogicalOperator, NameId, Problem, Requirement, SolvableId, Solver,
    SolverCache, StringId, UnsolvableOrCancelled, VersionSetId, VersionSetUnionId,
};
use version_ranges::Ranges;

use crate::dep::DepFlag;
use crate::resolve::{Candidate, PresentPackage, ResolveError, Resolved};
use crate::richdep::{self, RichExpr};
use crate::{Dep, Evr};

/// A borrowed view of one candidate package, valid only for the duration of a
/// single visit. This lets the solver read package data (name, deps) straight
/// from a caller's zero-copy store (e.g. rum-repo's mmap'd metadata) without
/// materializing an owned `Candidate` for every package — essential for large
/// repos on small hosts.
pub struct CandidateRef<'a> {
    /// Caller-defined handle (e.g. a global package index) echoed back in the
    /// resolved set.
    pub id: usize,
    pub name: &'a str,
    pub arch: &'a str,
    pub evr: Evr,
    pub provides: &'a [Dep],
    pub requires: &'a [Dep],
    pub recommends: &'a [Dep],
    /// Capabilities this package conflicts with (modeled as solver constraints
    /// forbidding coexistence).
    pub conflicts: &'a [Dep],
    /// Repo priority (lower preferred; default 99). Used only to tie-break
    /// provider selection.
    pub priority: i32,
}

/// A name-only view of one candidate, for the cheap pre-passes (which capability
/// names are provided/required/recommended). Carries borrowed `&str`s, so no
/// `Dep`/`String` is allocated — unlike [`CandidateRef`], whose owned `Dep`
/// vectors are only needed for the final pool build.
pub struct NameView<'a> {
    pub name: &'a str,
    pub arch: &'a str,
    /// Provided capability names (excluding files — see the providable pass).
    pub provide_names: &'a [&'a str],
    pub require_names: &'a [&'a str],
    pub recommend_names: &'a [&'a str],
    /// Conflict capability names, so the pool materializes providers for them
    /// (otherwise a conflict constraint has nothing to bite).
    pub conflict_names: &'a [&'a str],
}

/// A source of candidates that can be scanned repeatedly without holding them
/// all in memory at once. The solver runs one cheap `scan_names` pre-pass
/// (requested/providable/required sets) and one full `scan` to build the pool,
/// so implementations must be cheap to re-iterate.
pub trait CandidateSource {
    /// Full candidates (with owned `Dep`s); used once, to build the pool.
    /// `required` is the set of capability names the pool will actually
    /// register, so implementations may drop provides/files not in it (the
    /// build ignores them anyway) to avoid allocating `Dep`s for the millions
    /// of never-depended-upon files in a distro's metadata.
    fn scan(&self, required: &HashSet<String>, visit: &mut dyn FnMut(CandidateRef<'_>));
    /// Name-only pass (borrowed `&str`, no `Dep` allocation); used for the
    /// providable/required/requested sets.
    fn scan_names(&self, visit: &mut dyn FnMut(NameView<'_>));
}

/// A slice of owned `Candidate`s is a trivial source (used by tests and the
/// greedy path). Its `id` is the candidate's own `id` field.
impl CandidateSource for [Candidate] {
    fn scan(&self, required: &HashSet<String>, visit: &mut dyn FnMut(CandidateRef<'_>)) {
        for c in self {
            let provides: Vec<Dep> = c
                .provides
                .iter()
                .filter(|d| required.contains(&d.name))
                .cloned()
                .collect();
            visit(CandidateRef {
                id: c.id,
                name: &c.name,
                arch: &c.arch,
                evr: c.evr.clone(),
                provides: &provides,
                requires: &c.requires,
                recommends: &c.recommends,
                conflicts: &c.conflicts,
                priority: c.priority,
            });
        }
    }
    fn scan_names(&self, visit: &mut dyn FnMut(NameView<'_>)) {
        for c in self {
            let pv: Vec<&str> = c.provides.iter().map(|d| d.name.as_str()).collect();
            let rq: Vec<&str> = c.requires.iter().map(|d| d.name.as_str()).collect();
            let rc: Vec<&str> = c.recommends.iter().map(|d| d.name.as_str()).collect();
            let cf: Vec<&str> = c.conflicts.iter().map(|d| d.name.as_str()).collect();
            visit(NameView {
                name: &c.name,
                arch: &c.arch,
                provide_names: &pv,
                require_names: &rq,
                recommend_names: &rc,
                conflict_names: &cf,
            });
        }
    }
}

struct RpmProvider {
    pool: Pool<Ranges<Evr>>,
    /// capability NameId -> solvables providing it (repo + installed synthetic)
    providers: HashMap<NameId, Vec<SolvableId>>,
    /// solvable -> its dependency requirements
    deps: HashMap<SolvableId, Vec<ConditionalRequirement>>,
    /// solvable -> its constraints (from Conflicts): version sets that any
    /// co-selected package of that capability MUST satisfy (we store the
    /// complement of the conflicting range, so conflicting versions are barred).
    constrains: HashMap<SolvableId, Vec<VersionSetId>>,
    /// solvables that represent already-installed capabilities (preferred, and
    /// excluded from the install output)
    installed: HashSet<SolvableId>,
    /// solvables for scoped *present* packages: already installed, but modeled
    /// as full erasable solvables (real requires/conflicts) so the solver can
    /// keep or drop them. Preferred in sort like `installed`, but each maps back
    /// to a present-package id (`present_of`) so a dropped one becomes an erase.
    present: HashSet<SolvableId>,
    /// present solvable -> its `PresentPackage::id`.
    present_of: HashMap<SolvableId, usize>,
    /// The name-capability solvable of each present package, soft-required so
    /// the package is kept by default (nothing is erased unless a conflict or a
    /// cascaded dependency forces it).
    present_primaries: Vec<SolvableId>,
    /// solvable -> caller candidate id (only for real repo packages)
    candidate_of: HashMap<SolvableId, usize>,
    /// solvable -> its package's repo priority (lower preferred; default 99).
    priority: HashMap<SolvableId, i32>,
    /// solvable -> the underlying package's EVR. Candidate ranking must use the
    /// package version, not the solvable's record (which for a capability
    /// provider is the *provided* version and can tie across package builds).
    pkg_evr: HashMap<SolvableId, Evr>,
    /// Solvables created from an *unversioned* Provides (or a file). Per RPM's
    /// `rpmdsCompare`, a versionless provide satisfies ANY versioned require, so
    /// these must pass the version-set filter unconditionally.
    wildcard: HashSet<SolvableId>,
    /// Conditions referenced by conditional requirements (rich/boolean deps),
    /// indexed by `ConditionId`.
    conditions: Vec<Condition>,
    /// The originating Dep for each versioned version-set, so `filter_candidates`
    /// can apply RPM's partial-EVR comparison (e.g. `= 15.0.7` with no release
    /// matches `15.0.7-3.amzn2023.0.4`) instead of an exact Ranges match.
    vsdep: HashMap<VersionSetId, Dep>,
    /// Version sets that are Conflicts constraints (not requirements). These are
    /// matched by their raw `Ranges` membership — and, unlike a requirement, a
    /// wildcard/unversioned provide is NOT exempt (a conflict on a capability
    /// bars even an unversioned provider of it).
    constrain_vs: HashSet<VersionSetId>,
}

/// `rpmlib(...)` feature flags are satisfied by rpm itself, not repo packages.
pub fn is_ignorable_dep(name: &str) -> bool {
    name.starts_with("rpmlib(")
}

/// Collect every capability name referenced (as a Term) in a rich expression.
pub fn collect_rich_names(e: &RichExpr, out: &mut HashSet<String>) {
    match e {
        RichExpr::Term(d) => {
            out.insert(d.name.clone());
        }
        RichExpr::And(a, b)
        | RichExpr::Or(a, b)
        | RichExpr::If(a, b)
        | RichExpr::Unless(a, b)
        | RichExpr::With(a, b)
        | RichExpr::Without(a, b) => {
            collect_rich_names(a, out);
            collect_rich_names(b, out);
        }
        RichExpr::IfElse(a, b, c) | RichExpr::UnlessElse(a, b, c) => {
            collect_rich_names(a, out);
            collect_rich_names(b, out);
            collect_rich_names(c, out);
        }
    }
}

/// Mint a new ConditionId for `cond`.
fn mint(conds: &mut Vec<Condition>, cond: Condition) -> ConditionId {
    let id = ConditionId::new(conds.len() as u32);
    conds.push(cond);
    id
}

/// Build a resolvo Condition from a rich expression (used on the condition side
/// of `if`/`unless`). and/or become Binary; anything else falls back to its
/// leftmost term.
fn build_cond(
    pool: &Pool<Ranges<Evr>>,
    conds: &mut Vec<Condition>,
    vsdep: &mut HashMap<VersionSetId, Dep>,
    e: &RichExpr,
) -> ConditionId {
    match e {
        RichExpr::And(a, b) => {
            let ca = build_cond(pool, conds, vsdep, a);
            let cb = build_cond(pool, conds, vsdep, b);
            mint(conds, Condition::Binary(LogicalOperator::And, ca, cb))
        }
        RichExpr::Or(a, b) => {
            let ca = build_cond(pool, conds, vsdep, a);
            let cb = build_cond(pool, conds, vsdep, b);
            mint(conds, Condition::Binary(LogicalOperator::Or, ca, cb))
        }
        RichExpr::Term(d) => {
            let cap = pool.intern_package_name(d.name.clone());
            mint(
                conds,
                Condition::Requirement(version_set(pool, vsdep, cap, d)),
            )
        }
        // Rare: a compound condition; approximate with its leftmost term.
        RichExpr::If(a, _)
        | RichExpr::IfElse(a, _, _)
        | RichExpr::Unless(a, _)
        | RichExpr::UnlessElse(a, _, _)
        | RichExpr::With(a, _)
        | RichExpr::Without(a, _) => build_cond(pool, conds, vsdep, a),
    }
}

/// Collect the version sets of all Term leaves (used to build an OR union).
fn collect_or_vss(
    pool: &Pool<Ranges<Evr>>,
    vsdep: &mut HashMap<VersionSetId, Dep>,
    e: &RichExpr,
    out: &mut Vec<VersionSetId>,
) {
    match e {
        RichExpr::Term(d) => {
            let cap = pool.intern_package_name(d.name.clone());
            out.push(version_set(pool, vsdep, cap, d));
        }
        RichExpr::And(a, b) | RichExpr::Or(a, b) => {
            collect_or_vss(pool, vsdep, a, out);
            collect_or_vss(pool, vsdep, b, out);
        }
        _ => {}
    }
}

/// Is a condition expression already satisfied by the installed system? RPM's
/// `if`/`unless` condition on installed state, so we pre-evaluate against the
/// installed capabilities (by name).
fn cond_installed(e: &RichExpr, installed: &HashSet<String>) -> bool {
    match e {
        RichExpr::Term(d) => installed.contains(&d.name),
        RichExpr::And(a, b) => cond_installed(a, installed) && cond_installed(b, installed),
        RichExpr::Or(a, b) => cond_installed(a, installed) || cond_installed(b, installed),
        RichExpr::If(a, _)
        | RichExpr::IfElse(a, _, _)
        | RichExpr::Unless(a, _)
        | RichExpr::UnlessElse(a, _, _)
        | RichExpr::With(a, _)
        | RichExpr::Without(a, _) => cond_installed(a, installed),
    }
}

/// Map a rich expression to resolvo conditional requirements, appending to `out`.
/// `cond` is an ambient condition (from an enclosing `if`). `installed` is the
/// set of installed capability names, used to pre-evaluate `if`/`unless`.
#[allow(clippy::too_many_arguments)]
fn emit_rich(
    pool: &Pool<Ranges<Evr>>,
    conds: &mut Vec<Condition>,
    vsdep: &mut HashMap<VersionSetId, Dep>,
    installed: &HashSet<String>,
    e: &RichExpr,
    cond: Option<ConditionId>,
    out: &mut Vec<ConditionalRequirement>,
) {
    match e {
        RichExpr::Term(d) => {
            let cap = pool.intern_package_name(d.name.clone());
            out.push(ConditionalRequirement {
                condition: cond,
                requirement: Requirement::Single(version_set(pool, vsdep, cap, d)),
            });
        }
        RichExpr::And(a, b) => {
            emit_rich(pool, conds, vsdep, installed, a, cond, out);
            emit_rich(pool, conds, vsdep, installed, b, cond, out);
        }
        RichExpr::Or(..) => {
            let mut vss = Vec::new();
            collect_or_vss(pool, vsdep, e, &mut vss);
            if let Some((first, rest)) = vss.split_first() {
                let union = pool.intern_version_set_union(*first, rest.iter().copied());
                out.push(ConditionalRequirement {
                    condition: cond,
                    requirement: Requirement::Union(union),
                });
            }
        }
        // `then if cond`: if cond already holds on the system, require `then`
        // unconditionally; otherwise make it conditional on cond entering the
        // transaction (so both dnf senses — installed or co-installed — work).
        RichExpr::If(then, c) => {
            if cond_installed(c, installed) {
                emit_rich(pool, conds, vsdep, installed, then, cond, out);
            } else {
                let cid = build_cond(pool, conds, vsdep, c);
                let combined = match cond {
                    None => cid,
                    Some(o) => mint(conds, Condition::Binary(LogicalOperator::And, o, cid)),
                };
                emit_rich(pool, conds, vsdep, installed, then, Some(combined), out);
            }
        }
        // `then if cond else els`: pick the branch by installed state.
        RichExpr::IfElse(then, c, els) => {
            let branch = if cond_installed(c, installed) {
                then
            } else {
                els
            };
            emit_rich(pool, conds, vsdep, installed, branch, cond, out);
        }
        // `body unless cond`: require body unless cond is present.
        RichExpr::Unless(body, c) => {
            if !cond_installed(c, installed) {
                emit_rich(pool, conds, vsdep, installed, body, cond, out);
            }
        }
        RichExpr::UnlessElse(body, c, els) => {
            let branch = if cond_installed(c, installed) {
                els
            } else {
                body
            };
            emit_rich(pool, conds, vsdep, installed, branch, cond, out);
        }
        // `with`/`without`: no intersection in resolvo; require the primary
        // operand (approximation, documented).
        RichExpr::With(a, _) | RichExpr::Without(a, _) => {
            emit_rich(pool, conds, vsdep, installed, a, cond, out)
        }
    }
}

fn version_set(
    pool: &Pool<Ranges<Evr>>,
    vsdep: &mut HashMap<VersionSetId, Dep>,
    cap: NameId,
    dep: &Dep,
) -> VersionSetId {
    // The Ranges is a coarse approximation kept for display; the authoritative
    // match happens in filter_candidates via the stored Dep (RPM semantics).
    let ranges = match (&dep.evr, dep.flag) {
        (None, _) | (_, DepFlag::Any) => Ranges::full(),
        (Some(e), DepFlag::Eq) => Ranges::singleton(e.clone()),
        (Some(e), DepFlag::Lt) => Ranges::strictly_lower_than(e.clone()),
        (Some(e), DepFlag::Le) => Ranges::lower_than(e.clone()),
        (Some(e), DepFlag::Gt) => Ranges::strictly_higher_than(e.clone()),
        (Some(e), DepFlag::Ge) => Ranges::higher_than(e.clone()),
    };
    let vsid = pool.intern_version_set(cap, ranges);
    if dep.evr.is_some() {
        vsdep.insert(vsid, dep.clone());
    }
    vsid
}

/// Build the resolvo provider.
///
/// resolvo requires every candidate returned for a capability to share one
/// package name, so we intern a *distinct* provider solvable per
/// (capability, package), all under the capability's name, each mapping back to
/// the real package and carrying that package's dependencies. To keep the
/// solvable count bounded we only materialize capabilities that are actually
/// required by something (`required`) — the vast majority of provides/files are
/// never depended upon.
#[allow(clippy::too_many_arguments)]
fn build<S: CandidateSource + ?Sized>(
    source: &S,
    installed_provides: &[(String, Option<Evr>)],
    present_packages: &[PresentPackage],
    required: &HashSet<String>,
    include_recommends: bool,
    providable: &HashSet<String>,
) -> RpmProvider {
    let pool = Pool::<Ranges<Evr>>::new();
    let mut providers: HashMap<NameId, Vec<SolvableId>> = HashMap::new();
    let mut deps: HashMap<SolvableId, Vec<ConditionalRequirement>> = HashMap::new();
    let mut constrains_map: HashMap<SolvableId, Vec<VersionSetId>> = HashMap::new();
    let mut candidate_of: HashMap<SolvableId, usize> = HashMap::new();
    let mut priority: HashMap<SolvableId, i32> = HashMap::new();
    let mut installed: HashSet<SolvableId> = HashSet::new();
    let mut pkg_evr: HashMap<SolvableId, Evr> = HashMap::new();
    let mut wildcard: HashSet<SolvableId> = HashSet::new();
    let mut conditions: Vec<Condition> = Vec::new();
    let mut vsdep: HashMap<VersionSetId, Dep> = HashMap::new();
    let mut constrain_vs: HashSet<VersionSetId> = HashSet::new();
    let mut present: HashSet<SolvableId> = HashSet::new();
    let mut present_of: HashMap<SolvableId, usize> = HashMap::new();
    let mut present_primaries: Vec<SolvableId> = Vec::new();
    // Installed capability names, for pre-evaluating rich `if`/`unless`. Present
    // packages are installed too, so their capabilities count here.
    let installed_names: HashSet<String> = installed_provides
        .iter()
        .map(|(n, _)| n.clone())
        .chain(present_packages.iter().flat_map(|p| {
            std::iter::once(p.name.clone()).chain(p.provides.iter().map(|d| d.name.clone()))
        }))
        .collect();

    source.scan(required, &mut |c| {
        let ci = c.id;
        // Build the package's requirements once; every provider solvable for
        // this package shares them, so selecting the package via *any*
        // capability pulls its dependencies.
        let mut reqs: Vec<ConditionalRequirement> = Vec::new();
        // Hard requires, including rich/boolean deps (parsed into conditional
        // requirements / unions).
        for r in c.requires {
            if is_ignorable_dep(&r.name) {
                continue;
            }
            if richdep::is_rich(&r.name) {
                if let Some(expr) = richdep::parse_rich(&r.name) {
                    emit_rich(
                        &pool,
                        &mut conditions,
                        &mut vsdep,
                        &installed_names,
                        &expr,
                        None,
                        &mut reqs,
                    );
                }
                continue;
            }
            let cap = pool.intern_package_name(r.name.clone());
            reqs.push(ConditionalRequirement::from(version_set(
                &pool, &mut vsdep, cap, r,
            )));
        }
        // Weak deps (Recommends): simple, providable ones, installed as if
        // required (dnf default). Rich recommends are rare and skipped.
        if include_recommends {
            for r in c.recommends {
                if is_ignorable_dep(&r.name) || richdep::is_rich(&r.name) {
                    continue;
                }
                if providable.contains(&r.name) {
                    let cap = pool.intern_package_name(r.name.clone());
                    reqs.push(ConditionalRequirement::from(version_set(
                        &pool, &mut vsdep, cap, r,
                    )));
                }
            }
        }

        // Conflicts -> resolvo constraints. A `Conflicts: X [op ver]` forbids
        // co-installing any matching X. resolvo constrains say "a co-selected
        // solvable of this capability MUST be within version set V", so we store
        // the COMPLEMENT of the conflicting range as V: the conflicting versions
        // are then the only ones barred. Unversioned conflict -> forbid all
        // (complement of full = empty). Rich conflicts are skipped.
        let mut cons: Vec<VersionSetId> = Vec::new();
        for x in c.conflicts {
            if is_ignorable_dep(&x.name) || richdep::is_rich(&x.name) {
                continue;
            }
            let cap = pool.intern_package_name(x.name.clone());
            let forbidden = match (&x.evr, x.flag) {
                (None, _) | (_, DepFlag::Any) => Ranges::full(),
                (Some(e), DepFlag::Eq) => Ranges::singleton(e.clone()),
                (Some(e), DepFlag::Lt) => Ranges::strictly_lower_than(e.clone()),
                (Some(e), DepFlag::Le) => Ranges::lower_than(e.clone()),
                (Some(e), DepFlag::Gt) => Ranges::strictly_higher_than(e.clone()),
                (Some(e), DepFlag::Ge) => Ranges::higher_than(e.clone()),
            };
            let vsid = pool.intern_version_set(cap, forbidden.complement());
            constrain_vs.insert(vsid);
            cons.push(vsid);
        }

        // Capabilities this package offers: its own name (versioned, = package
        // EVR) plus every Provides / file. A Provides with no version is
        // unversioned and matches any require (tracked as a wildcard).
        let mut caps: Vec<(&str, Option<&Evr>)> = Vec::with_capacity(c.provides.len() + 1);
        caps.push((c.name, Some(&c.evr)));
        for p in c.provides {
            caps.push((p.name.as_str(), p.evr.as_ref()));
        }

        let mut done: HashSet<&str> = HashSet::new();
        for (capname, prov_evr) in caps {
            if !required.contains(capname) || !done.insert(capname) {
                continue;
            }
            // One provider solvable per (package, capability), interned under
            // the capability name so all providers of a capability share it.
            // The record is the provided version (or the package version as a
            // placeholder for unversioned provides, which are flagged wildcard).
            let cap = pool.intern_package_name(capname.to_string());
            let rec = prov_evr.cloned().unwrap_or_else(|| c.evr.clone());
            let sid = pool.intern_solvable(cap, rec);
            candidate_of.insert(sid, ci);
            pkg_evr.insert(sid, c.evr.clone());
            priority.insert(sid, c.priority);
            deps.insert(sid, reqs.clone());
            if !cons.is_empty() {
                constrains_map.insert(sid, cons.clone());
            }
            if prov_evr.is_none() {
                wildcard.insert(sid);
            }
            providers.entry(cap).or_default().push(sid);
        }
    });

    // Scoped *present* packages: already installed, but modeled as full
    // erasable solvables so the solver can keep them (default, via a soft
    // requirement) or drop them (erase) when a cascaded dependency can no longer
    // be satisfied. Built just like a repo package — real requires and
    // conflicts — but tagged `present`/`present_of` so a dropped one is reported
    // as an erase rather than treated as a fresh install.
    for pp in present_packages {
        let mut reqs: Vec<ConditionalRequirement> = Vec::new();
        for r in &pp.requires {
            if is_ignorable_dep(&r.name) {
                continue;
            }
            if richdep::is_rich(&r.name) {
                if let Some(expr) = richdep::parse_rich(&r.name) {
                    emit_rich(
                        &pool,
                        &mut conditions,
                        &mut vsdep,
                        &installed_names,
                        &expr,
                        None,
                        &mut reqs,
                    );
                }
                continue;
            }
            let cap = pool.intern_package_name(r.name.clone());
            reqs.push(ConditionalRequirement::from(version_set(
                &pool, &mut vsdep, cap, r,
            )));
        }

        // Its own name (versioned) plus every Provides.
        let mut caps: Vec<(&str, Option<&Evr>)> = Vec::with_capacity(pp.provides.len() + 1);
        caps.push((pp.name.as_str(), Some(&pp.evr)));
        for p in &pp.provides {
            caps.push((p.name.as_str(), p.evr.as_ref()));
        }
        let mut done: HashSet<&str> = HashSet::new();
        for (capname, prov_evr) in caps {
            if !required.contains(capname) || !done.insert(capname) {
                continue;
            }
            let is_name = capname == pp.name;
            let cap = pool.intern_package_name(capname.to_string());
            let rec = prov_evr.cloned().unwrap_or_else(|| pp.evr.clone());
            let sid = pool.intern_solvable(cap, rec);
            present.insert(sid);
            present_of.insert(sid, pp.id);
            pkg_evr.insert(sid, pp.evr.clone());
            deps.insert(sid, reqs.clone());
            if prov_evr.is_none() {
                wildcard.insert(sid);
            }
            if is_name {
                present_primaries.push(sid);
            }
            providers.entry(cap).or_default().push(sid);
        }
    }

    // Installed capabilities as preferred, dependency-free synthetic solvables
    // (only for capabilities that are required, and matching the same-name rule
    // since they are interned under the capability name).
    for (name, evr) in installed_provides {
        if !required.contains(name) {
            continue;
        }
        let cap = pool.intern_package_name(name.clone());
        let rec = evr.clone().unwrap_or_else(|| Evr::new(Some(0), "0", ""));
        let sid = pool.intern_solvable(cap, rec.clone());
        installed.insert(sid);
        pkg_evr.insert(sid, rec);
        deps.insert(sid, Vec::new());
        // An installed capability with no version (e.g. a file) matches any require.
        if evr.is_none() {
            wildcard.insert(sid);
        }
        providers.entry(cap).or_default().push(sid);
    }

    RpmProvider {
        pool,
        providers,
        deps,
        constrains: constrains_map,
        installed,
        present,
        present_of,
        present_primaries,
        candidate_of,
        priority,
        pkg_evr,
        wildcard,
        conditions,
        vsdep,
        constrain_vs,
    }
}

impl Interner for RpmProvider {
    type NameId = NameId;
    type SolvableId = SolvableId;

    fn display_solvable(&self, solvable: SolvableId) -> impl Display + '_ {
        let s = self.pool.resolve_solvable(solvable);
        format!("{}-{}", self.pool.resolve_package_name(s.name), s.record)
    }
    fn display_name(&self, name: NameId) -> impl Display + '_ {
        self.pool.resolve_package_name(name).clone()
    }
    fn display_version_set(&self, version_set: VersionSetId) -> impl Display + '_ {
        format!("{:?}", self.pool.resolve_version_set(version_set))
    }
    fn display_string(&self, string_id: StringId) -> impl Display + '_ {
        self.pool.resolve_string(string_id).to_string()
    }
    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        self.pool.resolve_version_set_package_name(version_set)
    }
    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        self.pool.resolve_solvable(solvable).name
    }
    fn version_sets_in_union(
        &self,
        union: VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId> {
        self.pool.resolve_version_set_union(union)
    }
    fn resolve_condition(&self, condition: ConditionId) -> Condition {
        self.conditions[condition.as_u32() as usize].clone()
    }
}

impl DependencyProvider for RpmProvider {
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        // Conflicts constraint: match strictly by the version set's raw Ranges
        // (no wildcard exemption — a conflict on a capability bars even an
        // unversioned provider of it).
        if self.constrain_vs.contains(&version_set) {
            let ranges = self.pool.resolve_version_set(version_set).clone();
            return candidates
                .iter()
                .copied()
                .filter(|s| {
                    let rec = &self.pool.resolve_solvable(*s).record;
                    ranges.contains(rec) != inverse
                })
                .collect();
        }
        let dep = self.vsdep.get(&version_set);
        candidates
            .iter()
            .copied()
            .filter(|s| {
                // An unversioned provide (wildcard) matches any require (RPM's
                // rpmdsCompare). For versioned requires, use the originating
                // Dep's partial-EVR comparison (so `= 15.0.7` with no release
                // matches `15.0.7-3.amzn2023.0.4`); this mirrors the greedy
                // resolver and RPM exactly. Unversioned requires (no stored
                // Dep) match on name alone, which membership already implies.
                let matches = self.wildcard.contains(s)
                    || match dep {
                        Some(d) => {
                            let rec = &self.pool.resolve_solvable(*s).record;
                            d.satisfied_by(&d.name, Some(rec))
                        }
                        None => true,
                    };
                matches != inverse
            })
            .collect()
    }

    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        let list = self.providers.get(&name)?;
        Some(Candidates {
            candidates: list.clone(),
            ..Candidates::default()
        })
    }

    async fn sort_candidates(&self, _solver: &SolverCache<Self>, solvables: &mut [SolvableId]) {
        // Prefer already-installed solvables, then the highest *package* EVR
        // (not the solvable record, which for a capability provider is the
        // provided version and can tie across different package builds).
        let zero = Evr::new(Some(0), "0", "");
        solvables.sort_by(|a, b| {
            // On-disk (installed synthetic OR scoped present) solvables first, so
            // the solver keeps what's already installed instead of upgrading it.
            let ia = self.installed.contains(a) || self.present.contains(a);
            let ib = self.installed.contains(b) || self.present.contains(b);
            // installed first, then higher repo priority (lower number), then
            // higher package EVR — matching dnf (priority trumps version).
            ib.cmp(&ia)
                .then_with(|| {
                    let pa = self.priority.get(a).copied().unwrap_or(99);
                    let pb = self.priority.get(b).copied().unwrap_or(99);
                    pa.cmp(&pb)
                })
                .then_with(|| {
                    let ra = self.pkg_evr.get(a).unwrap_or(&zero);
                    let rb = self.pkg_evr.get(b).unwrap_or(&zero);
                    rb.cmp(ra)
                })
        });
    }

    async fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        Dependencies::Known(KnownDependencies {
            requirements: self.deps.get(&solvable).cloned().unwrap_or_default(),
            constrains: self.constrains.get(&solvable).cloned().unwrap_or_default(),
        })
    }
}

/// Resolve `requested` package specs against `candidates`, treating
/// `installed_provides` as already satisfied. Uses the resolvo SAT solver.
pub fn resolve_sat(
    requested: &[String],
    candidates: &[Candidate],
    installed_provides: &[(String, Option<Evr>)],
) -> Result<Resolved, ResolveError> {
    resolve_sat_with(requested, candidates, installed_provides)
}

/// Resolve against a [`CandidateSource`] (e.g. rum-repo's zero-copy views), so
/// the full package set need never be materialized as owned `Candidate`s.
pub fn resolve_sat_with<S: CandidateSource + ?Sized>(
    requested: &[String],
    source: &S,
    installed_provides: &[(String, Option<Evr>)],
) -> Result<Resolved, ResolveError> {
    resolve_sat_scoped_opt(requested, source, installed_provides, &[], true)
}

/// Like [`resolve_sat_with`], but allows disabling weak dependencies (`Recommends:`).
pub fn resolve_sat_with_opt<S: CandidateSource + ?Sized>(
    requested: &[String],
    source: &S,
    installed_provides: &[(String, Option<Evr>)],
    include_recommends: bool,
) -> Result<Resolved, ResolveError> {
    resolve_sat_scoped_opt(
        requested,
        source,
        installed_provides,
        &[],
        include_recommends,
    )
}

/// Erase-aware resolve: like [`resolve_sat_with`], but also takes the scoped set
/// of *present* (installed) packages the transaction may need to erase. Each is
/// modeled as a full erasable solvable, soft-required so it is kept by default;
/// one the solver drops (because a cascaded dependency can no longer be
/// satisfied) is reported in [`Resolved::to_erase`] by its `PresentPackage::id`.
///
/// The caller supplies only the *cascade* set (dependents of erased targets);
/// the erase of a directly conflicted/obsoleted target is the caller's own
/// decision (the winner replaces it) and its provides should be omitted from
/// `installed_provides` so dependents see it gone.
pub fn resolve_sat_scoped<S: CandidateSource + ?Sized>(
    requested: &[String],
    source: &S,
    installed_provides: &[(String, Option<Evr>)],
    present_packages: &[PresentPackage],
) -> Result<Resolved, ResolveError> {
    resolve_sat_scoped_opt(
        requested,
        source,
        installed_provides,
        present_packages,
        true,
    )
}

/// Like [`resolve_sat_scoped`], but allows controlling whether weak dependencies
/// (`Recommends:`) enter the solver pool.
pub fn resolve_sat_scoped_opt<S: CandidateSource + ?Sized>(
    requested: &[String],
    source: &S,
    installed_provides: &[(String, Option<Evr>)],
    present_packages: &[PresentPackage],
    include_recommends: bool,
) -> Result<Resolved, ResolveError> {
    // Single cheap name-only pass gathering everything the pre-solve sets need:
    // requested package names, the providable set, the hard-required capability
    // names (incl. those named in rich exprs), and the raw recommend names.
    // Walking full `Dep`s here (as the pool build does) is what dominated
    // resolve time on large repos, so this pass stays in borrowed `&str`.
    let mut requested_names: Vec<String> = Vec::new();
    let mut found = vec![false; requested.len()];
    let mut providable: HashSet<String> = HashSet::new();
    let mut hard_required: HashSet<String> = HashSet::new();
    let mut recommend_names: HashSet<String> = HashSet::new();
    source.scan_names(&mut |c| {
        for (i, spec) in requested.iter().enumerate() {
            if !found[i] && (c.name == spec || format!("{}.{}", c.name, c.arch) == *spec) {
                found[i] = true;
                if !requested_names.iter().any(|n| n == c.name) {
                    requested_names.push(c.name.to_string());
                }
            }
        }
        // Providable: package names + non-file provides (files never back a
        // weak dep and would balloon this set on RHEL-scale metadata).
        providable.insert(c.name.to_string());
        for p in c.provide_names {
            if !p.starts_with('/') {
                providable.insert(p.to_string());
            }
        }
        for r in c.require_names {
            if is_ignorable_dep(r) {
                continue;
            }
            if richdep::is_rich(r) {
                if let Some(expr) = richdep::parse_rich(r) {
                    collect_rich_names(&expr, &mut hard_required);
                }
                continue;
            }
            hard_required.insert(r.to_string());
        }
        for r in c.recommend_names {
            if !is_ignorable_dep(r) && !richdep::is_rich(r) {
                recommend_names.insert(r.to_string());
            }
        }
        // Materialize conflict targets too (they need provider solvables — incl.
        // the installed synthetic — for the conflict constraint to bind).
        for x in c.conflict_names {
            if !is_ignorable_dep(x) && !richdep::is_rich(x) {
                hard_required.insert(x.to_string());
            }
        }
    });
    // A requested spec need not be a package name: it may be a capability
    // (e.g. a comps group lists `pkgconfig`, provided by `pkgconf-pkg-config`).
    // If no package name matched but something provides it, use the capability
    // as the root requirement so resolvo picks a provider (like `dnf install
    // <capability>`).
    for (i, spec) in requested.iter().enumerate() {
        if !found[i] && providable.contains(spec.as_str()) {
            found[i] = true;
            if !requested_names.iter().any(|n| n == spec) {
                requested_names.push(spec.clone());
            }
        }
    }
    for (i, spec) in requested.iter().enumerate() {
        if !found[i] {
            return Err(ResolveError::NotFound(spec.clone()));
        }
    }
    for (name, _) in installed_provides {
        providable.insert(name.clone());
    }
    // A present package's own capabilities (name + provides) must be
    // materialized so its provider solvables get built, and its requires must be
    // materialized so those capabilities have providers to satisfy them.
    for pp in present_packages {
        hard_required.insert(pp.name.clone());
        providable.insert(pp.name.clone());
        for p in &pp.provides {
            hard_required.insert(p.name.clone());
            providable.insert(p.name.clone());
        }
        for r in &pp.requires {
            if !is_ignorable_dep(&r.name) && !richdep::is_rich(&r.name) {
                hard_required.insert(r.name.clone());
            } else if richdep::is_rich(&r.name) {
                if let Some(expr) = richdep::parse_rich(&r.name) {
                    collect_rich_names(&expr, &mut hard_required);
                }
            }
        }
    }

    if include_recommends {
        // Try with weak dependencies (dnf's default); if that makes the set
        // unsolvable, retry hard-only so weak deps never cause a failure.
        match attempt(
            requested,
            &requested_names,
            source,
            installed_provides,
            present_packages,
            &providable,
            &hard_required,
            &recommend_names,
            true,
        ) {
            Err(ResolveError::Unsatisfied { .. }) => attempt(
                requested,
                &requested_names,
                source,
                installed_provides,
                present_packages,
                &providable,
                &hard_required,
                &recommend_names,
                false,
            ),
            other => other,
        }
    } else {
        attempt(
            requested,
            &requested_names,
            source,
            installed_provides,
            present_packages,
            &providable,
            &hard_required,
            &recommend_names,
            false,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn attempt<S: CandidateSource + ?Sized>(
    requested: &[String],
    requested_names: &[String],
    source: &S,
    installed_provides: &[(String, Option<Evr>)],
    present_packages: &[PresentPackage],
    providable: &HashSet<String>,
    hard_required: &HashSet<String>,
    recommend_names: &HashSet<String>,
    include_recommends: bool,
) -> Result<Resolved, ResolveError> {
    // Capabilities to materialize: everything hard-required, the requested
    // names, and (when on) the satisfiable Recommends. Assembled from the
    // pre-pass sets — no extra scan.
    let mut required: HashSet<String> = hard_required.clone();
    for n in requested_names {
        required.insert(n.clone());
    }
    if include_recommends {
        for r in recommend_names {
            if providable.contains(r) {
                required.insert(r.clone());
            }
        }
    }

    let provider = build(
        source,
        installed_provides,
        present_packages,
        &required,
        include_recommends,
        providable,
    );

    // Root requirements: each requested package, at any version.
    let mut root = Vec::new();
    for name in requested_names {
        let cap = provider.pool.intern_package_name(name.clone());
        let vs = provider.pool.intern_version_set(cap, Ranges::full());
        root.push(ConditionalRequirement::from(vs));
    }

    // Retain the maps needed to interpret the solution before moving provider.
    let candidate_of = provider.candidate_of.clone();
    let installed = provider.installed.clone();
    let present_of = provider.present_of.clone();
    // Soft requirements: keep each present package (by its name solvable) unless
    // a conflict / cascaded dependency makes doing so impossible. An
    // unsatisfiable soft requirement is silently skipped (that skip is the
    // erase), so nothing is removed just because it was named here.
    let soft: Vec<SolvableId> = provider.present_primaries.clone();

    let mut solver = Solver::new(provider);
    let solved = solver
        .solve(Problem::new().requirements(root).soft_requirements(soft))
        .map_err(|e| match e {
            UnsolvableOrCancelled::Unsolvable(_) => ResolveError::Unsatisfied {
                package: requested.join(", "),
                requirement: "unsatisfiable dependency set".to_string(),
            },
            UnsolvableOrCancelled::Cancelled(_) => ResolveError::Unsatisfied {
                package: requested.join(", "),
                requirement: "resolution cancelled".to_string(),
            },
        })?;

    // Map chosen solvables back to candidate ids, skipping installed synthetics
    // and present (already-installed) solvables. Track which present packages
    // survived — any scoped present package with no surviving solvable is erased.
    let mut to_install = Vec::new();
    let mut kept_present: HashSet<usize> = HashSet::new();
    for s in solved {
        if let Some(&pid) = present_of.get(&s) {
            kept_present.insert(pid);
            continue;
        }
        if installed.contains(&s) {
            continue;
        }
        if let Some(&ci) = candidate_of.get(&s) {
            if !to_install.contains(&ci) {
                to_install.push(ci);
            }
        }
    }
    let to_erase: Vec<usize> = present_packages
        .iter()
        .map(|p| p.id)
        .filter(|id| !kept_present.contains(id))
        .collect();
    Ok(Resolved {
        to_install,
        to_erase,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{assert_installs, assert_scoped, assert_unsolvable, TestRepo};

    fn dep(name: &str) -> Dep {
        Dep::unversioned(name)
    }
    fn cand(id: usize, name: &str, ver: &str, provides: &[&str], requires: &[Dep]) -> Candidate {
        Candidate {
            id,
            name: name.into(),
            arch: "x86_64".into(),
            evr: Evr::new(Some(0), ver, "1"),
            provides: provides.iter().map(|p| Dep::unversioned(*p)).collect(),
            requires: requires.to_vec(),
            recommends: Vec::new(),
            conflicts: Vec::new(),
            priority: 99,
        }
    }

    #[test]
    fn resolves_transitive_chain() {
        let mut r = TestRepo::new();
        r.pkg("app-1.0-1.x86_64").requires("libb.so");
        r.pkg("libb-1.0-1.x86_64")
            .provides("libb.so")
            .requires("libc.so");
        r.pkg("libc-1.0-1.x86_64").provides("libc.so");
        assert_installs(
            &r,
            &["app"],
            &["app-1.0-1.x86_64", "libb-1.0-1.x86_64", "libc-1.0-1.x86_64"],
        );
    }

    #[test]
    fn prunes_installed() {
        let mut r = TestRepo::new();
        r.pkg("app-1.0-1.x86_64").requires("libc.so");
        r.pkg("libc-1.0-1.x86_64").provides("libc.so");
        r.installed("libc.so"); // already provided by the system
        assert_installs(&r, &["app"], &["app-1.0-1.x86_64"]);
    }

    #[test]
    fn backtracks_when_newest_provider_is_a_dead_end() {
        // `cap` has two providers: prov-2 (newest) needs `missing` (unsatisfiable),
        // prov-1 (older) is self-contained. A greedy "newest wins" picks prov-2
        // and fails; a backtracking solver must fall back to prov-1.
        let mut r = TestRepo::new();
        r.pkg("app-1.0-1.x86_64").requires("cap");
        r.pkg("prov-1.0-1.x86_64").provides("cap");
        r.pkg("prov-2.0-1.x86_64")
            .provides("cap")
            .requires("missing");
        assert_installs(&r, &["app"], &["app-1.0-1.x86_64", "prov-1.0-1.x86_64"]);
    }

    #[test]
    fn unversioned_provide_satisfies_versioned_require() {
        // RPM rule: a versionless `Provides: webserver` satisfies
        // `Requires: webserver >= 5.0`, even though the package is v3.0.
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").requires("webserver >= 5.0");
        r.pkg("prov-3.0-1.x86_64").provides("webserver");
        assert_installs(&r, &["app"], &["app-1-1.x86_64", "prov-3.0-1.x86_64"]);
    }

    #[test]
    fn unsatisfiable_require_fails() {
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").requires("nonexistent");
        assert_unsolvable(&r, &["app"]);
    }

    #[test]
    fn recommends_pulled_when_satisfiable() {
        // rum installs Recommends by default (dnf install_weak_deps=1).
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").recommends("extra");
        r.pkg("extra-1-1.x86_64").provides("extra");
        assert_installs(&r, &["app"], &["app-1-1.x86_64", "extra-1-1.x86_64"]);
    }

    #[test]
    fn recommends_skipped_when_weak_deps_disabled() {
        // When include_recommends is false (--no-weak-deps), soft Recommends: are dropped.
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").recommends("extra");
        r.pkg("extra-1-1.x86_64").provides("extra");
        let installs = r.resolve_opt(&["app"], false).unwrap();
        assert_eq!(installs, vec!["app-1-1.x86_64"]);
    }

    #[test]
    fn recommends_dropped_when_unsatisfiable() {
        // A missing weak dep must not fail the transaction — app still installs.
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").recommends("absent");
        assert_installs(&r, &["app"], &["app-1-1.x86_64"]);
    }

    // B2 (conflicts at solve time): `app` needs capX and capY. capX is provided
    // only by px. Of capY's two providers, pyA `Conflicts: capX` — which px (a
    // required in-transaction package) provides — so pyA can't be co-installed;
    // the solver must backtrack to pyB instead of dead-ending at rpm commit.
    #[test]
    fn conflict_forces_alternative_provider() {
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").requires("capX").requires("capY");
        r.pkg("px-1-1.x86_64").provides("capX");
        r.pkg("pyA-1-1.x86_64").provides("capY").conflicts("capX");
        r.pkg("pyB-1-1.x86_64").provides("capY");
        assert_installs(
            &r,
            &["app"],
            &["app-1-1.x86_64", "px-1-1.x86_64", "pyB-1-1.x86_64"],
        );
    }

    // B3 (tie-breaking): capX has two providers — pv-2.0 (newer, low-priority
    // repo 99) and pv-1.0 (older, high-priority repo 1). dnf lets repo priority
    // trump version, so the higher-priority (lower-number) pv-1.0 must win.
    #[test]
    fn higher_priority_repo_wins_over_higher_version() {
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").requires("capX");
        r.pkg("pv-2.0-1.x86_64").provides("capX").priority(99);
        r.pkg("pv-1.0-1.x86_64").provides("capX").priority(1);
        assert_installs(&r, &["app"], &["app-1-1.x86_64", "pv-1.0-1.x86_64"]);
    }

    // A1 (see [[rum-upstream-research]]): RPM's dependency-overlap rule compares
    // epoch ONLY when both sides carry one. Require `bash >= 2:5.0` vs Provide
    // `bash = 5.2` (no epoch) -> RPM/dnf skip the epoch and 5.2 >= 5.0 satisfies.
    #[test]
    fn epoch_skipped_in_overlap_when_provide_has_none() {
        let mut r = TestRepo::new();
        r.pkg("app-1-1.x86_64").requires("bash >= 2:5.0");
        r.pkg("bash-5.2-1.x86_64").provides("bash = 5.2");
        assert_installs(&r, &["app"], &["app-1-1.x86_64", "bash-5.2-1.x86_64"]);
    }

    #[test]
    fn version_locked_arch_qualified_eq_requires() {
        // Reproduces the deep -devel pattern (clang-devel, postgresql*-devel):
        // a package with EQ requires on ARCH-QUALIFIED capabilities that other
        // packages provide versioned, all pinned to one version.
        let evr = |v: &str| Evr::new(Some(0), v, "1");
        let vprov = |name: &str, v: &str| Dep {
            name: name.into(),
            flag: DepFlag::Eq,
            evr: Some(evr(v)),
        };
        // app Requires: lib(x86-64) = 1.0 AND tool(x86-64) = 1.0
        let app = Candidate {
            id: 0,
            name: "app".into(),
            arch: "x86_64".into(),
            evr: evr("1.0"),
            provides: vec![],
            requires: vec![vprov("lib(x86-64)", "1.0"), vprov("tool(x86-64)", "1.0")],
            recommends: vec![],
            conflicts: vec![],
            priority: 99,
        };
        // lib and tool each carry a versioned arch-qualified provide.
        let lib = Candidate {
            id: 1,
            name: "lib".into(),
            arch: "x86_64".into(),
            evr: evr("1.0"),
            provides: vec![vprov("lib(x86-64)", "1.0")],
            requires: vec![],
            recommends: vec![],
            conflicts: vec![],
            priority: 99,
        };
        let tool = Candidate {
            id: 2,
            name: "tool".into(),
            arch: "x86_64".into(),
            evr: evr("1.0"),
            provides: vec![vprov("tool(x86-64)", "1.0")],
            requires: vec![],
            recommends: vec![],
            conflicts: vec![],
            priority: 99,
        };
        let r = resolve_sat(&["app".into()], &[app, lib, tool], &[])
            .unwrap()
            .to_install;
        assert!(
            r.contains(&1) && r.contains(&2),
            "arch-qualified EQ provides must resolve"
        );
    }

    #[test]
    fn rich_if_pulls_dep_when_condition_installed() {
        // app Requires: (extra if trigger). Mirrors mariadb's
        // (mysql-selinux if selinux-policy-targeted).
        let mut app = cand(0, "app", "1.0", &[], &[]);
        app.requires = vec![Dep::unversioned("(extra if trigger)")];
        let extra = cand(1, "extra", "1.0", &["extra"], &[]);

        // trigger installed -> extra should be pulled.
        let installed = vec![("trigger".to_string(), None)];
        let r = resolve_sat(&["app".into()], &[app.clone(), extra.clone()], &installed)
            .unwrap()
            .to_install;
        assert!(r.contains(&1), "extra pulled because trigger is installed");

        // trigger absent -> extra not pulled (condition false).
        let r2 = resolve_sat(&["app".into()], &[app, extra], &[])
            .unwrap()
            .to_install;
        assert!(!r2.contains(&1), "extra not pulled when trigger absent");
    }

    #[test]
    fn rich_or_resolves_via_either_provider() {
        let mut app = cand(0, "app", "1.0", &[], &[]);
        app.requires = vec![Dep::unversioned("(webA or webB)")];
        let weba = cand(1, "webA", "1.0", &["webA"], &[]);
        let r = resolve_sat(&["app".into()], &[app, weba], &[])
            .unwrap()
            .to_install;
        assert!(r.contains(&1), "OR satisfied by the available provider");
    }

    #[test]
    fn eq_require_without_release_matches_any_release() {
        // Reproduces the clang-libs gap: `Requires: cap = 15.0.7` (no release)
        // must match a provider advertising `cap = 15.0.7-3.amzn2023.0.4`.
        let app = Candidate {
            id: 0,
            name: "app".into(),
            arch: "x86_64".into(),
            evr: Evr::new(Some(0), "1.0", "1"),
            provides: vec![],
            requires: vec![Dep {
                name: "cap".into(),
                flag: DepFlag::Eq,
                evr: Some(Evr::new(Some(0), "15.0.7", "")), // version only, no release
            }],
            recommends: vec![],
            conflicts: vec![],
            priority: 99,
        };
        let prov = Candidate {
            id: 1,
            name: "prov".into(),
            arch: "x86_64".into(),
            evr: Evr::new(Some(0), "15.0.7", "3.amzn2023.0.4"),
            provides: vec![Dep {
                name: "cap".into(),
                flag: DepFlag::Eq,
                evr: Some(Evr::new(Some(0), "15.0.7", "3.amzn2023.0.4")),
            }],
            requires: vec![],
            recommends: vec![],
            conflicts: vec![],
            priority: 99,
        };
        let r = resolve_sat(&["app".into()], &[app, prov], &[])
            .unwrap()
            .to_install;
        assert!(
            r.contains(&1),
            "EQ without release must match any release of that version"
        );
    }

    #[test]
    fn unsatisfiable_errors() {
        let cands = vec![cand(0, "app", "1.0", &[], &[dep("nope")])];
        assert!(resolve_sat(&["app".into()], &cands, &[]).is_err());
    }

    // --- Scoped-present (erase-aware) resolution (Step 2) --------------------

    // A present package nothing disturbs is KEPT: not erased, not (re)installed.
    // The soft requirement keeps it even though nothing depends on it — rum must
    // never erase a package just because it entered the scope.
    #[test]
    fn present_package_kept_by_default() {
        let mut r = TestRepo::new();
        r.present("keeper-1-1.x86_64");
        assert_scoped(&r, &[], &[], &[]);
    }

    // Cascade erase: a present consumer requires a capability that no longer has
    // any provider (its target was erased by the caller, and no repo package
    // offers an alternative). The consumer's soft requirement can't be met, so
    // it is erased rather than left broken on disk.
    #[test]
    fn cascade_consumer_erased_when_no_provider_remains() {
        let mut r = TestRepo::new();
        r.present("consumer-1-1.x86_64").requires("libtarget.so");
        assert_scoped(&r, &[], &[], &["consumer-1-1.x86_64"]);
    }

    // Cascade kept via a repo alternative: the same consumer, but a repo package
    // provides the capability. The solver installs the alternative and KEEPS the
    // consumer (no erase) — the dependent survives because its need is met.
    #[test]
    fn cascade_consumer_kept_via_repo_alternative() {
        let mut r = TestRepo::new();
        r.pkg("alt-2-1.x86_64").provides("libtarget.so");
        r.present("consumer-1-1.x86_64").requires("libtarget.so");
        assert_scoped(&r, &[], &["alt-2-1.x86_64"], &[]);
    }

    // Transitive cascade THROUGH the present set: B provides P but itself needs
    // a gone capability, so B is erased; C requires P (only B offered it), so C
    // erases too. This is the "erase B without knowing C exists" trap the scope
    // pass exists to prevent — both must fall together.
    #[test]
    fn transitive_cascade_erases_dependent_of_dropped_present() {
        let mut r = TestRepo::new();
        r.present("B-1-1.x86_64")
            .provides("P")
            .requires("libgone.so");
        r.present("C-1-1.x86_64").requires("P");
        assert_scoped(&r, &[], &[], &["B-1-1.x86_64", "C-1-1.x86_64"]);
    }

    // A present package whose dependency IS satisfied (by another present
    // package that is itself fine) stays — the cascade only fires on a genuine
    // break, not on any dependency edge.
    #[test]
    fn present_chain_all_kept_when_satisfied() {
        let mut r = TestRepo::new();
        r.present("B-1-1.x86_64").provides("P");
        r.present("C-1-1.x86_64").requires("P");
        assert_scoped(&r, &[], &[], &[]);
    }

    #[test]
    fn scoped_resolve_respects_weak_deps() {
        let mut r = TestRepo::new();
        r.present("consumer-1-1.x86_64").requires("needed");
        r.pkg("alt-2-1.x86_64")
            .provides("needed")
            .recommends("extra");
        r.pkg("extra-1-1.x86_64").provides("extra");

        let (inst_with, erases_with) = r.resolve_scoped_opt(&[], true).unwrap();
        assert_eq!(inst_with, vec!["alt-2-1.x86_64", "extra-1-1.x86_64"]);
        assert_eq!(erases_with, Vec::<String>::new());

        let (inst_without, erases_without) = r.resolve_scoped_opt(&[], false).unwrap();
        assert_eq!(inst_without, vec!["alt-2-1.x86_64"]);
        assert_eq!(erases_without, Vec::<String>::new());
    }
}
