//! Transitive dependency resolution.
//!
//! This is a **greedy** resolver (v1): starting from the requested packages, it
//! satisfies each `Requires` with the newest available provider, pruning
//! anything already provided by the installed system or by a package already
//! selected. It does NOT backtrack, so it cannot resolve conflicts that require
//! choosing an older candidate. That correctness upgrade (a proper SAT solve,
//! e.g. via `resolvo`) is a planned follow-up; the interfaces here are shaped so
//! it can be swapped in without changing callers.

use std::collections::HashMap;

use crate::{Dep, Evr};

/// A candidate package the resolver may choose.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Caller-defined handle (e.g. index into the repo package list).
    pub id: usize,
    pub name: String,
    pub arch: String,
    pub evr: Evr,
    pub provides: Vec<Dep>,
    pub requires: Vec<Dep>,
    /// Weak dependencies (Recommends): pulled best-effort when satisfiable.
    pub recommends: Vec<Dep>,
    /// Capabilities this package conflicts with: it cannot coexist with a
    /// package matching these (modeled as a solver constraint). Empty for most.
    pub conflicts: Vec<Dep>,
    /// Repo priority (lower is preferred; dnf default 99). Among candidates,
    /// a higher-priority repo wins over a lower-priority one even at a lower
    /// version — matching dnf's `priority=`.
    pub priority: i32,
}

/// An already-installed package the resolver is allowed to *erase* if the
/// transaction requires it (a winner Conflicts/Obsoletes it, or it depends on
/// something being erased). Only the transaction-relevant "scoped" installed
/// packages are passed as these — the rest of the system stays modeled as cheap
/// dependency-free "present" capabilities (see [`crate::sat`]).
#[derive(Debug, Clone)]
pub struct PresentPackage {
    /// Caller-defined handle (e.g. index into the scoped-installed list),
    /// echoed back in [`Resolved::to_erase`].
    pub id: usize,
    pub name: String,
    pub evr: Evr,
    pub provides: Vec<Dep>,
    pub requires: Vec<Dep>,
}

/// The outcome of a resolve: the candidate ids to install, plus any scoped
/// installed packages the solver chose to erase, in a stable order.
#[derive(Debug, Default)]
pub struct Resolved {
    pub to_install: Vec<usize>,
    /// Ids (from [`PresentPackage::id`]) of scoped installed packages to erase.
    /// Empty unless the scoped-present resolver path is used.
    pub to_erase: Vec<usize>,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no package found matching `{0}`")]
    NotFound(String),
    #[error("unresolved dependency for `{package}`: nothing provides `{requirement}`")]
    Unsatisfied {
        package: String,
        requirement: String,
    },
}

/// A provide as a (name, optional version) pair, from installed packages.
pub type ProvideEntry = (String, Option<Evr>);

/// Resolve the install set for `requested` package specs (matched by name or
/// `name.arch`) against `candidates`, treating `installed_provides` as already
/// satisfied.
pub fn resolve(
    requested: &[String],
    candidates: &[Candidate],
    installed_provides: &[ProvideEntry],
) -> Result<Resolved, ResolveError> {
    // Index available provides: provide-name -> [(candidate index, provide evr)].
    let mut prov_index: HashMap<&str, Vec<(usize, Option<&Evr>)>> = HashMap::new();
    for (ci, c) in candidates.iter().enumerate() {
        // Implicit self-provide: every package provides its own name = evr.
        prov_index
            .entry(c.name.as_str())
            .or_default()
            .push((ci, Some(&c.evr)));
        for p in &c.provides {
            prov_index
                .entry(p.name.as_str())
                .or_default()
                .push((ci, p.evr.as_ref()));
        }
    }

    // Index installed provides for fast satisfaction checks.
    let mut inst_index: HashMap<&str, Vec<Option<&Evr>>> = HashMap::new();
    for (name, evr) in installed_provides {
        inst_index
            .entry(name.as_str())
            .or_default()
            .push(evr.as_ref());
    }

    let mut selected: Vec<bool> = vec![false; candidates.len()];
    let mut order: Vec<usize> = Vec::new();
    let mut queue: Vec<usize> = Vec::new();

    // Seed with the requested packages.
    for spec in requested {
        let ci =
            best_for_spec(spec, candidates).ok_or_else(|| ResolveError::NotFound(spec.clone()))?;
        if !selected[ci] {
            selected[ci] = true;
            order.push(ci);
            queue.push(ci);
        }
    }

    // BFS over requirements.
    let mut head = 0;
    while head < queue.len() {
        let ci = queue[head];
        head += 1;

        // Collect requirements first to avoid holding a borrow across mutation.
        let reqs = candidates[ci].requires.clone();
        let pkg_label = candidates[ci].name.clone();

        for req in &reqs {
            // Skip rpmlib(...) feature requirements (satisfied by rpm itself)
            // and rich/boolean deps like `(a if b)` (not parsed yet).
            if req.name.starts_with("rpmlib(") || req.name.starts_with('(') {
                continue;
            }
            if satisfied_by_installed(req, &inst_index) {
                continue;
            }
            if satisfied_by_selected(req, &prov_index, &selected) {
                continue;
            }
            // Choose the newest-EVR candidate that provides this requirement.
            match best_provider(req, &prov_index, candidates) {
                Some(pi) => {
                    if !selected[pi] {
                        selected[pi] = true;
                        order.push(pi);
                        queue.push(pi);
                    }
                }
                None => {
                    return Err(ResolveError::Unsatisfied {
                        package: pkg_label,
                        requirement: format_req(req),
                    });
                }
            }
        }

        // Weak dependencies (Recommends): pull best-effort — install a provider
        // if one exists, but never fail the resolve if none does. Matches dnf's
        // default install_weak_deps=1. Recommended packages are enqueued, so
        // their own hard deps and recommends are pulled transitively.
        let recs = candidates[ci].recommends.clone();
        for rec in &recs {
            if rec.name.starts_with("rpmlib(") || rec.name.starts_with('(') {
                continue;
            }
            if satisfied_by_installed(rec, &inst_index)
                || satisfied_by_selected(rec, &prov_index, &selected)
            {
                continue;
            }
            if let Some(pi) = best_provider(rec, &prov_index, candidates) {
                if !selected[pi] {
                    selected[pi] = true;
                    order.push(pi);
                    queue.push(pi);
                }
            }
        }
    }

    Ok(Resolved {
        to_install: order,
        to_erase: Vec::new(),
    })
}

/// Pick the highest-EVR candidate matching a requested spec (name or name.arch).
fn best_for_spec(spec: &str, candidates: &[Candidate]) -> Option<usize> {
    candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.name == spec || format!("{}.{}", c.name, c.arch) == *spec)
        .max_by(|(_, a), (_, b)| {
            a.evr
                .compare(&b.evr)
                .then_with(|| arch_pref(&a.arch).cmp(&arch_pref(&b.arch)))
        })
        .map(|(i, _)| i)
}

fn satisfied_by_installed(req: &Dep, inst: &HashMap<&str, Vec<Option<&Evr>>>) -> bool {
    inst.get(req.name.as_str())
        .is_some_and(|provs| provs.iter().any(|pe| req.satisfied_by(&req.name, *pe)))
}

fn satisfied_by_selected(
    req: &Dep,
    prov_index: &HashMap<&str, Vec<(usize, Option<&Evr>)>>,
    selected: &[bool],
) -> bool {
    prov_index.get(req.name.as_str()).is_some_and(|provs| {
        provs
            .iter()
            .any(|(ci, pe)| selected[*ci] && req.satisfied_by(&req.name, *pe))
    })
}

/// The newest-EVR candidate providing `req`.
fn best_provider(
    req: &Dep,
    prov_index: &HashMap<&str, Vec<(usize, Option<&Evr>)>>,
    candidates: &[Candidate],
) -> Option<usize> {
    prov_index
        .get(req.name.as_str())?
        .iter()
        .filter(|(_, pe)| req.satisfied_by(&req.name, *pe))
        .map(|(ci, _)| *ci)
        .max_by(|a, b| {
            candidates[*a]
                .evr
                .compare(&candidates[*b].evr)
                .then_with(|| arch_pref(&candidates[*a].arch).cmp(&arch_pref(&candidates[*b].arch)))
        })
}

/// Lower is preferred: native arch beats noarch beats the rest, so ties in EVR
/// pick a sensible package.
fn arch_pref(arch: &str) -> u8 {
    match arch {
        "x86_64" | "aarch64" => 0,
        "noarch" => 1,
        _ => 2,
    }
}

fn format_req(req: &Dep) -> String {
    match &req.evr {
        Some(e) => format!("{} {} {}", req.name, flag_str(req.flag), e.version),
        None => req.name.clone(),
    }
}

fn flag_str(f: crate::DepFlag) -> &'static str {
    use crate::DepFlag::*;
    match f {
        Any => "",
        Eq => "=",
        Lt => "<",
        Le => "<=",
        Gt => ">",
        Ge => ">=",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DepFlag;

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
        // app -> libb -> libc ; libc requires nothing.
        let cands = vec![
            cand(10, "app", "1.0", &[], &[Dep::unversioned("libb.so")]),
            cand(
                20,
                "libb",
                "1.0",
                &["libb.so"],
                &[Dep::unversioned("libc.so")],
            ),
            cand(30, "libc", "1.0", &["libc.so"], &[]),
        ];
        let r = resolve(&["app".into()], &cands, &[]).unwrap();
        assert_eq!(r.to_install, vec![0, 1, 2]); // indices of app, libb, libc
    }

    #[test]
    fn prunes_installed_dependency() {
        let cands = vec![
            cand(10, "app", "1.0", &[], &[Dep::unversioned("libc.so")]),
            cand(30, "libc", "1.0", &["libc.so"], &[]),
        ];
        // libc.so already provided by the system: only app is selected.
        let installed = vec![("libc.so".to_string(), None)];
        let r = resolve(&["app".into()], &cands, &installed).unwrap();
        assert_eq!(r.to_install, vec![0]);
    }

    #[test]
    fn errors_on_missing_dependency() {
        let cands = vec![cand(
            10,
            "app",
            "1.0",
            &[],
            &[Dep::unversioned("missing.so")],
        )];
        let err = resolve(&["app".into()], &cands, &[]).unwrap_err();
        assert!(matches!(err, ResolveError::Unsatisfied { .. }));
    }

    #[test]
    fn pulls_satisfiable_recommends_skips_missing() {
        let mut app = cand(10, "app", "1.0", &[], &[]);
        app.recommends = vec![Dep::unversioned("plugin"), Dep::unversioned("ghost")];
        let cands = vec![app, cand(20, "plugin", "1.0", &["plugin"], &[])];
        let r = resolve(&["app".into()], &cands, &[]).unwrap().to_install;
        assert!(r.contains(&0), "app selected");
        assert!(r.contains(&1), "satisfiable recommend `plugin` pulled");
        // `ghost` has no provider -> silently skipped; resolve still succeeds.
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn picks_newest_provider() {
        let cands = vec![
            cand(10, "app", "1.0", &[], &[Dep::unversioned("cap")]),
            cand(20, "prov-old", "1.0", &["cap"], &[]),
            cand(30, "prov-new", "2.0", &["cap"], &[]),
        ];
        let r = resolve(&["app".into()], &cands, &[]).unwrap();
        assert!(r.to_install.contains(&2)); // prov-new (index 2), not prov-old
        assert!(!r.to_install.contains(&1));
    }

    #[test]
    fn versioned_requirement_selects_adequate_provider() {
        let cands = vec![
            Candidate {
                id: 1,
                name: "app".into(),
                arch: "x86_64".into(),
                evr: Evr::new(Some(0), "1.0", "1"),
                provides: vec![],
                requires: vec![Dep {
                    name: "glibc".into(),
                    flag: DepFlag::Ge,
                    evr: Some(Evr::new(Some(0), "2.34", "")),
                }],
                recommends: Vec::new(),
                conflicts: Vec::new(),
                priority: 99,
            },
            cand(2, "glibc", "2.40", &[], &[]),
        ];
        let r = resolve(&["app".into()], &cands, &[]).unwrap();
        assert_eq!(r.to_install.len(), 2);
    }
}
