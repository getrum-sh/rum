//! `rum download [--resolve] [--destdir DIR] <packages...>`
//!
//! Resolves the requested packages (optionally pulling their full dependency
//! closure with --resolve), then downloads the RPMs in parallel, verifying each
//! against its repo checksum. Does not install anything.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::repo_sync;
use rum_repo::{AvailablePackage, Http, RepoMetadata};
use rum_solve::{
    collect_rich_names, is_ignorable_dep, is_rich, parse_rich, resolve_sat_scoped_opt,
    resolve_sat_with_opt, CandidateRef, CandidateSource, Dep, Evr, NameView, PresentPackage,
    RichExpr,
};

pub fn run(packages: &[String], with_deps: bool, destdir: &Path) -> anyhow::Result<()> {
    if packages.is_empty() {
        anyhow::bail!("`rum download` needs at least one package name");
    }
    let resolution = resolve_packages(packages, with_deps)?;
    if resolution.is_empty() {
        println!("Nothing to download.");
        return Ok(());
    }
    println!(
        "Downloading {} package(s), {} total, to {}",
        resolution.ids.len(),
        human(resolution.total_bytes()),
        destdir.display()
    );
    let fetched = fetch(&resolution, destdir)?;
    println!(
        "\nDownloaded {} package(s), {} in {:.2}s.",
        fetched.files.len(),
        human(fetched.total_bytes),
        fetched.elapsed.as_secs_f64()
    );
    Ok(())
}

/// A resolved transaction: which packages to act on, plus the repo data needed
/// to fetch them. Produced by [`resolve_packages`] *without* downloading, so
/// callers can show the transaction and confirm before any bytes move.
pub struct Resolution {
    /// The full available-package set (owned; the resolve path indexes and
    /// mutates it, e.g. attaching filelists).
    packages: Vec<AvailablePackage>,
    /// repo id -> base URL, and repo id -> HTTP client, for fetching.
    base_urls: HashMap<String, String>,
    clients: HashMap<String, Http>,
    /// Indices into `packages`, in resolved order.
    pub ids: Vec<usize>,
    /// Installed packages to ERASE as part of this transaction, as
    /// `(name, version, release)`: packages a winner Obsoletes/Conflicts
    /// (targets) plus any dependent the resolver could not keep (cascade).
    /// Empty for ordinary installs (no conflicts/obsoletes in play).
    erases: Vec<(String, String, String)>,
}

impl Resolution {
    pub fn empty() -> Self {
        Self {
            packages: Vec::new(),
            base_urls: HashMap::new(),
            clients: HashMap::new(),
            ids: Vec::new(),
            erases: Vec::new(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
    /// NEVRAs of the resolved packages, sorted.
    #[allow(dead_code)]
    pub fn nevras(&self) -> Vec<String> {
        let mut v: Vec<String> = self.ids.iter().map(|&i| self.packages[i].nevra()).collect();
        v.sort();
        v
    }
    pub fn total_bytes(&self) -> u64 {
        self.ids.iter().map(|&i| self.packages[i].size).sum()
    }
    /// The resolved (winning) packages.
    pub fn winner_packages(&self) -> impl Iterator<Item = &AvailablePackage> {
        self.ids.iter().map(move |&i| &self.packages[i])
    }
    /// Installed packages to erase as part of this transaction, as
    /// `(name, version, release)`.
    pub fn erases(&self) -> &[(String, String, String)] {
        &self.erases
    }
}

/// The result of downloading a resolved package set.
pub struct Fetched {
    pub files: Vec<PathBuf>,
    pub total_bytes: u64,
    pub elapsed: std::time::Duration,
}

/// Resolve `packages` (optionally with their dependency closure) against the
/// enabled repos. Does NOT download anything.
pub fn resolve_packages(packages: &[String], with_deps: bool) -> anyhow::Result<Resolution> {
    let synced = repo_sync::sync_enabled(false)?;
    for r in &synced.repos {
        if let Some(e) = &r.error {
            eprintln!("warning: repo `{}` skipped: {e}", r.id);
        }
    }
    resolve_internal_synced(synced, packages, with_deps, None)
}

/// Resolve packages for upgrade: upgrades all installed packages with updates
/// available in enabled repos (if `packages` is empty), or the specified packages.
/// Does NOT download anything.
pub fn resolve_upgrade(packages: &[String]) -> anyhow::Result<Resolution> {
    let synced = repo_sync::sync_enabled(false)?;
    for r in &synced.repos {
        if let Some(e) = &r.error {
            eprintln!("warning: repo `{}` skipped: {e}", r.id);
        }
    }

    let Ok(db) = rum_rpm::Rpmdb::open() else {
        return Ok(Resolution::empty());
    };

    let installed = super::pkgindex::installed_best();
    let available = super::pkgindex::available_best(synced.metas());

    let mut to_upgrade: Vec<String> = Vec::new();
    let mut upg_set: HashSet<String> = HashSet::new();
    let mut matched_any = false;

    if packages.is_empty() {
        for (key, inst_evr) in &installed {
            if let Some(avail) = available.get(key) {
                if avail.evr_cmp() > *inst_evr && !upg_set.contains(&avail.name) {
                    to_upgrade.push(avail.name_arch());
                    upg_set.insert(avail.name.clone());
                }
            }
        }
        // Also check if any available package obsoletes an installed package
        let all_installed = db.installed();
        for m in synced.metas() {
            for p in m.views() {
                let obs = p.obsoletes();
                if obs.is_empty() {
                    continue;
                }
                let replaces = all_installed.iter().any(|inst| {
                    let ievr = Evr::new(inst.epoch, &inst.version, &inst.release);
                    obs.iter().any(|o| o.satisfied_by(&inst.name, Some(&ievr)))
                });
                if replaces {
                    let name = p.name().to_string();
                    if !upg_set.contains(&name) && !db.is_installed(&name) {
                        to_upgrade.push(p.name_arch());
                        upg_set.insert(name);
                    }
                }
            }
        }
    } else {
        for (key, inst_evr) in &installed {
            let name = key.split('.').next().unwrap_or(key);
            let hit = packages
                .iter()
                .any(|p| crate::glob::matches(p, key) || crate::glob::matches(p, name));
            if !hit {
                continue;
            }
            matched_any = true;
            if let Some(avail) = available.get(key) {
                if avail.evr_cmp() > *inst_evr && !upg_set.contains(&avail.name) {
                    to_upgrade.push(avail.name_arch());
                    upg_set.insert(avail.name.clone());
                }
            }
        }
        if !matched_any {
            eprintln!("No match for argument: {}", packages.join(" "));
            return Ok(Resolution::empty());
        }
        if to_upgrade.is_empty() {
            println!("Package(s) already up to date.");
            return Ok(Resolution::empty());
        }
    }

    if to_upgrade.is_empty() {
        return Ok(Resolution::empty());
    }

    resolve_internal_synced(synced, &to_upgrade, true, Some(&upg_set))
}

fn resolve_internal_synced(
    synced: repo_sync::Synced,
    packages: &[String],
    with_deps: bool,
    upgrading_names: Option<&HashSet<String>>,
) -> anyhow::Result<Resolution> {
    // Expand any `@group` / `@environment` targets into package names (fed to
    // the resolver as explicit installs). Plain package specs pass through.
    let requested: Vec<String> = {
        let has_group = packages.iter().any(|p| p.starts_with('@'));
        if !has_group {
            packages.to_vec()
        } else {
            let comps = super::groups::Comps::load(synced.metas());
            let db = rum_rpm::Rpmdb::open().ok();
            let mut explicit = Vec::new();
            let mut group_members: Vec<String> = Vec::new();
            for spec in packages {
                if let Some(group) = spec.strip_prefix('@') {
                    match comps.expand(group, db.as_ref()) {
                        Some(names) if !names.is_empty() => group_members.extend(names),
                        Some(_) => eprintln!("warning: group `{spec}` is empty"),
                        None => anyhow::bail!("no group or environment matching `{spec}`"),
                    }
                } else {
                    explicit.push(spec.clone());
                }
            }
            // A group can list members not present in the enabled repos (e.g.
            // AL2023's @development lists `rcs`); dnf silently skips those, so we
            // drop group members that no repo provides (by name or capability)
            // rather than failing the whole group. Explicit targets still error.
            if !group_members.is_empty() {
                let mut available: HashSet<String> = HashSet::new();
                for m in synced.metas() {
                    for p in m.views() {
                        available.insert(p.name().to_string());
                        for pr in p.provide_names() {
                            available.insert(pr.to_string());
                        }
                    }
                }
                let before = group_members.len();
                group_members.retain(|n| {
                    available.contains(n) || db.as_ref().is_some_and(|d| d.is_installed(n))
                });
                let dropped = before - group_members.len();
                if dropped > 0 {
                    eprintln!("note: skipped {dropped} group package(s) not in the enabled repos");
                }
            }
            explicit.extend(group_members);
            explicit
        }
    };
    // Resolve against the repos' zero-copy views (no owned Vec of the whole
    // package set), then materialize only the winning packages. `gid` is a
    // global package index; `offsets[ri]..offsets[ri+1]` is repo ri's range.
    let (packages_out, ids, erases) = {
        let metas = synced.metas();
        let priorities = synced.priorities();
        let mut offsets = Vec::with_capacity(metas.len() + 1);
        let mut acc = 0usize;
        for m in metas {
            offsets.push(acc);
            acc += m.len();
        }
        offsets.push(acc);

        let rehydrate = |gid: usize| -> Option<AvailablePackage> {
            let ri = offsets.partition_point(|&o| o <= gid).saturating_sub(1);
            metas.get(ri).and_then(|m| m.rehydrate(gid - offsets[ri]))
        };

        // Installed provides (for the SAT installed-synthetics), computed once.
        let installed = if with_deps {
            if let Some(upg) = upgrading_names {
                if let Ok(db) = rum_rpm::Rpmdb::open() {
                    let all_inst = db.installed_packages_deps();
                    let mut upgraded_caps = HashSet::new();
                    let mut surviving_caps = HashSet::new();
                    for p in &all_inst {
                        if upg.contains(&p.name) {
                            upgraded_caps.insert(p.name.clone());
                            for (pv, _) in &p.provides {
                                upgraded_caps.insert(pv.clone());
                            }
                        } else {
                            surviving_caps.insert(p.name.clone());
                            for (pv, _) in &p.provides {
                                surviving_caps.insert(pv.clone());
                            }
                        }
                    }
                    let excluded_caps: HashSet<String> =
                        upgraded_caps.difference(&surviving_caps).cloned().collect();

                    db.all_provides()
                        .into_iter()
                        .filter(|(name, _)| !excluded_caps.contains(name))
                        .map(|(name, ver)| (name, ver.map(|s| Evr::parse(&s))))
                        .collect()
                } else {
                    installed_provides()
                }
            } else {
                installed_provides()
            }
        } else {
            Vec::new()
        };

        // One resolve pass for a target set (multilib-filtered; SAT with the
        // filelists fallback, or best-match for a bare download).
        let resolve_targets = |targets: &[String]| -> anyhow::Result<Vec<usize>> {
            let allow = arches_for(targets);
            if !with_deps {
                let mut gids = Vec::new();
                for spec in targets {
                    match best_match_views(spec, metas, &offsets, &allow) {
                        Some(g) if !gids.contains(&g) => gids.push(g),
                        Some(_) => {}
                        None => anyhow::bail!("no package found matching `{spec}`"),
                    }
                }
                return Ok(gids);
            }
            let include_recommends = crate::sys::load_config()
                .map(|c| c.main.install_weak_deps)
                .unwrap_or(true);
            let mut extra: HashMap<String, Vec<String>> = HashMap::new();
            let first = {
                let mut src = MetasSource {
                    metas,
                    offsets: &offsets,
                    extra: &extra,
                    allow_arches: &allow,
                    priorities,
                    reachable: None,
                };
                let reachable = src.compute_reachable(targets, &installed, include_recommends);
                src.reachable = Some(reachable);
                resolve_sat_with_opt(targets, &src, &installed, include_recommends)
            };
            match first {
                Ok(r) => Ok(r.to_install),
                Err(e) => {
                    // A file-path requirement may only be satisfiable via
                    // filelists.xml (not primary). Fetch those paths, attach as
                    // extra provides on their owning packages, and retry once.
                    let wanted = unmet_file_requires_views(metas, &installed);
                    if wanted.is_empty() {
                        return Err(anyhow::anyhow!("dependency resolution failed: {e}"));
                    }
                    for (pkgid, files) in repo_sync::load_filelists(&wanted) {
                        extra.entry(pkgid).or_default().extend(files);
                    }
                    let mut src = MetasSource {
                        metas,
                        offsets: &offsets,
                        extra: &extra,
                        allow_arches: &allow,
                        priorities,
                        reachable: None,
                    };
                    let reachable = src.compute_reachable(targets, &installed, include_recommends);
                    src.reachable = Some(reachable);
                    Ok(
                        resolve_sat_with_opt(targets, &src, &installed, include_recommends)
                            .map_err(|e| anyhow::anyhow!("dependency resolution failed: {e}"))?
                            .to_install,
                    )
                }
            }
        };

        // Lockstep-upgrade augmentation: an installed package co-built with one
        // we're upgrading may require it at an EXACT `= version` (e.g.
        // `NetworkManager-tui` needs the old `NetworkManager`); upgrading only
        // part of the set makes rpm reject the transaction. Detect such broken
        // installed requirers via the rpmdb and pull them into the target set,
        // re-resolving to a fixpoint (bounded), so the whole coupled set
        // upgrades together — matching dnf.
        let gids = resolve_targets(&requested)?;
        let mut winners: Vec<AvailablePackage> = gids.into_iter().filter_map(rehydrate).collect();
        // Installed packages this transaction must erase (targets a winner
        // Obsoletes/Conflicts, plus dependents the resolver can't keep). Stays
        // empty for ordinary installs. Populated in the scoped-present block.
        let mut erases: Vec<(String, String, String)> = Vec::new();

        // Lockstep-upgrade augmentation. An installed package we're upgrading may
        // have installed co-built siblings pinned to it via `= exact-version`
        // (e.g. NetworkManager-{tui,cloud-setup}); a name target won't upgrade
        // them (the resolver keeps the installed version), so we pull the repo
        // build at the SAME new EVR (co-built subpackages share it) directly
        // into the winner set, to a fixpoint. rpm validates the final set.
        if with_deps {
            if let Ok(db) = rum_rpm::Rpmdb::open() {
                let rd = db.installed_reverse_deps();
                let mut seen: HashSet<String> = winners.iter().map(|w| w.name.clone()).collect();
                let mut queue: Vec<AvailablePackage> = winners.clone();
                while let Some(w) = queue.pop() {
                    let w_evr = Evr::new(Some(w.epoch), w.version.clone(), w.release.clone());
                    // Only upgrades of installed packages can break a pinned sibling.
                    if db.by_name(&w.name).is_empty() {
                        continue;
                    }
                    let Some(requirers) = rd.exact.get(&w.name) else {
                        continue;
                    };
                    for (rqr, reqver) in requirers {
                        // Label comparison (epoch-normalized): the require string
                        // may omit the epoch that `w_evr` carries, so structural
                        // `==` would spuriously differ post the Evr epoch change.
                        if seen.contains(rqr)
                            || Evr::parse(reqver).compare(&w_evr) == std::cmp::Ordering::Equal
                        {
                            continue;
                        }
                        // The co-built sibling build that pins the NEW version
                        // shares its EVR; pull that exact one from the repo.
                        if let Some(pkg) = find_pkg_at(rqr, &w_evr, metas) {
                            seen.insert(rqr.clone());
                            winners.push(pkg.clone());
                            queue.push(pkg);
                        }
                    }
                }

                // Rich reverse-dep augmentation. An installed package may carry a
                // conditional `(X = V if Y)` require: once Y is present (installed
                // or being installed), rpm demands X at V. rum's resolver doesn't
                // model installed packages' rich requires, so satisfy them here by
                // pulling X. Example: installed `systemd` needs
                // `(systemd-rpm-macros = ... if rpm-build)`, and `@development`
                // pulls `rpm-build`, so `systemd-rpm-macros` must come along.
                // Iterate to a fixpoint (bounded by the finite rich-dep set).
                loop {
                    let mut added = false;
                    for (rqr, expr) in &rd.rich {
                        // Only `(then if cond)` / `(then if cond else _)` where both
                        // sides are plain terms are actionable as a pull.
                        let (then, cond) = match rum_solve::parse_rich(expr) {
                            Some(RichExpr::If(t, c)) | Some(RichExpr::IfElse(t, c, _)) => {
                                match (*t, *c) {
                                    (RichExpr::Term(t), RichExpr::Term(c)) => (t, c),
                                    _ => continue,
                                }
                            }
                            _ => continue,
                        };
                        // Only relevant if this transaction touches the requirer OR
                        // introduces the condition; untouched background packages are skipped.
                        let rqr_touching = seen.contains(rqr);
                        let cond_in_seen = seen.contains(&cond.name)
                            || winners
                                .iter()
                                .any(|w| w.provides.iter().any(|p| p.name == cond.name));
                        if !rqr_touching && !cond_in_seen {
                            continue;
                        }
                        // Condition active? (being installed, or already installed)
                        let cond_active = cond_in_seen
                            || db.is_installed(&cond.name)
                            || db.provide_version(&cond.name).is_some();
                        if !cond_active {
                            continue;
                        }
                        // Already satisfied by a winner or an installed build?
                        let seen_ok = seen.contains(&then.name)
                            || winners
                                .iter()
                                .any(|w| w.provides.iter().any(|p| p.name == then.name));
                        if seen_ok {
                            continue;
                        }
                        let want_evr = then.evr.clone();
                        let installed_ok = match &want_evr {
                            Some(v) => {
                                let match_pkg = db.by_name(&then.name).iter().any(|p| {
                                    Evr::new(p.epoch, &p.version, &p.release).compare(v)
                                        == std::cmp::Ordering::Equal
                                });
                                let match_cap = match db.provide_version(&then.name) {
                                    Some(ref pv) => {
                                        Evr::parse(pv).compare(v) == std::cmp::Ordering::Equal
                                    }
                                    None => false,
                                };
                                match_pkg || match_cap
                            }
                            None => {
                                db.is_installed(&then.name)
                                    || db.provide_version(&then.name).is_some()
                            }
                        };
                        if installed_ok {
                            continue;
                        }
                        // Pull the provider: the exact build for a versioned `=`,
                        // else the newest available by name.
                        let pulled = match &want_evr {
                            Some(v) => find_pkg_at(&then.name, v, metas),
                            None => best_match_views(&then.name, metas, &offsets, &arches_for(&[]))
                                .and_then(rehydrate),
                        };
                        if let Some(pkg) = pulled {
                            seen.insert(then.name.clone());
                            winners.push(pkg);
                            added = true;
                        }
                    }
                    if !added {
                        break;
                    }
                }

                // Obsoletes replacement (B1). dnf's obsoletes processing: when
                // the transaction touches an installed package that an available
                // package Obsoletes, pull the obsoleter in so it REPLACES the
                // obsoleted one — rpm erases the obsoleted at commit because the
                // obsoleter is in the transaction. Scoped to the transaction's
                // lineage (installed packages that are requested or being
                // upgraded), NOT a global system sweep, matching dnf.
                let affected: HashSet<String> = requested
                    .iter()
                    .cloned()
                    .chain(seen.iter().cloned())
                    .collect();
                let affected_installed: Vec<(String, Evr)> = db
                    .installed()
                    .into_iter()
                    .filter(|p| affected.contains(&p.name))
                    .map(|p| (p.name.clone(), Evr::new(p.epoch, p.version, p.release)))
                    .collect();
                if !affected_installed.is_empty() {
                    for (ri, m) in metas.iter().enumerate() {
                        for (pi, p) in m.views().enumerate() {
                            let obs = p.obsoletes();
                            if obs.is_empty() {
                                continue;
                            }
                            let replaces = affected_installed.iter().any(|(iname, ievr)| {
                                obs.iter().any(|o| o.satisfied_by(iname, Some(ievr)))
                            });
                            if !replaces {
                                continue;
                            }
                            let name = p.name().to_string();
                            // Skip if already a winner or already installed (only
                            // pull a NEW obsoleter that isn't in the plan yet).
                            if seen.contains(&name) || db.is_installed(&name) {
                                continue;
                            }
                            if let Some(pkg) = rehydrate(offsets[ri] + pi) {
                                seen.insert(name);
                                winners.push(pkg);
                            }
                        }
                    }
                }

                // Install-only semantics (kernels): a requested install-only
                // package installs the NEWEST available build ALONGSIDE existing
                // ones, so the resolver treating an installed build as
                // "satisfied" is wrong here. If a newer build than what's
                // installed exists, pull it in explicitly (commit then prunes to
                // installonly_limit).
                for spec in &requested {
                    let mut best: Option<(usize, Evr)> = None;
                    for (ri, m) in metas.iter().enumerate() {
                        for pi in m.indices_by_name(spec) {
                            if let Some(p) = m.view_at(pi) {
                                let is_io = matches!(p.name(), "kernel" | "kernel-core")
                                    || p.provide_names()
                                        .iter()
                                        .any(|n| n.starts_with("installonlypkg("));
                                if !is_io {
                                    continue;
                                }
                                let evr = p.evr_cmp();
                                // (explicit match, not map_or/is_none_or: keeps MSRV
                                // 1.81 while satisfying clippy's unnecessary_map_or)
                                let better = match &best {
                                    Some((_, b)) => evr > *b,
                                    None => true,
                                };
                                if better {
                                    best = Some((offsets[ri] + pi, evr));
                                }
                            }
                        }
                    }
                    if let Some((gid, evr)) = best {
                        let newest_installed = db
                            .by_name(spec)
                            .iter()
                            .map(|q| Evr::new(q.epoch, &q.version, &q.release))
                            .max();
                        let newer = match &newest_installed {
                            Some(ni) => evr > *ni,
                            None => true,
                        };
                        if newer && !seen.contains(spec) {
                            if let Some(pkg) = rehydrate(gid) {
                                seen.insert(spec.clone());
                                winners.push(pkg);
                            }
                        }
                    }
                }

                // Scoped-present erase resolution (steps 1-3). When a winner
                // Conflicts/Obsoletes something, find the minimal set of
                // INSTALLED packages the transaction must reason about — the
                // targeted packages plus their transitive dependents — and let
                // the SAT solver decide, for each dependent, whether it can be
                // kept (its need met by a repo alternative or another installed
                // package) or must be erased too. Ordinary installs skip all of
                // this (target_caps is empty), so they pay nothing.
                let conflict_and_obsolete_deps: Vec<&rum_solve::Dep> = winners
                    .iter()
                    .flat_map(|w| w.conflicts.iter().chain(w.obsoletes.iter()))
                    .collect();
                if !conflict_and_obsolete_deps.is_empty() {
                    let inst = db.installed_packages_deps();
                    let target_indices: Vec<usize> = (0..inst.len())
                        .filter(|&i| {
                            conflict_and_obsolete_deps
                                .iter()
                                .any(|d| installed_matches_dep(&inst[i], d))
                        })
                        .collect();

                    if !target_indices.is_empty() {
                        let target_set: HashSet<usize> = target_indices.iter().copied().collect();
                        let scoped = super::scope::scope_installed(&target_indices, &inst);
                        // The scoped set is the whole RSS argument for erase-aware
                        // resolution: only these installed packages enter the solver
                        // (not the full rpmdb), so log the ratio to make the bound
                        // observable (RUM_LOG=debug).
                        tracing::debug!(
                            scoped = scoped.len(),
                            targets = target_indices.len(),
                            installed = inst.len(),
                            "scoped-present installed set (erase-aware)"
                        );
                        // Owned (not borrowed from `winners`) so we can push cascade
                        // alternatives into `winners` later without a borrow clash.
                        let winner_names: HashSet<String> =
                            winners.iter().map(|w| w.name.clone()).collect();

                        // Present (cascade) packages the solver may keep or erase.
                        let mut cascade: Vec<PresentPackage> = Vec::new();
                        let mut cascade_idx: Vec<usize> = Vec::new(); // present id -> inst index
                        for &i in &scoped {
                            if target_set.contains(&i) {
                                // Erase the target outright (unless a same-name winner
                                // is upgrading it, which rpm -U handles instead).
                                if !winner_names.contains(inst[i].name.as_str()) {
                                    push_erase(&mut erases, &inst[i]);
                                }
                            } else {
                                let id = cascade.len();
                                cascade.push(present_package(id, &inst[i]));
                                cascade_idx.push(i);
                            }
                        }

                        if !cascade.is_empty() {
                            // Passive installed provides with every scoped package's
                            // capabilities removed: targets are gone, and cascade
                            // packages are modeled as real (erasable) present
                            // solvables instead, so a dependent's need is only met if
                            // a surviving package (installed or repo) actually offers
                            // it.
                            let scoped_caps: HashSet<&str> = scoped
                                .iter()
                                .flat_map(|&i| {
                                    std::iter::once(inst[i].name.as_str())
                                        .chain(inst[i].provides.iter().map(|(n, _)| n.as_str()))
                                })
                                .collect();
                            let inst_prov: Vec<(String, Option<Evr>)> = installed
                                .iter()
                                .filter(|(n, _)| !scoped_caps.contains(n.as_str()))
                                .cloned()
                                .collect();
                            let allow = arches_for(&requested);
                            let extra_empty: HashMap<String, Vec<String>> = HashMap::new();
                            let mut seeds = Vec::new();
                            for p in &cascade {
                                seeds.push(p.name.clone());
                                for pv in &p.provides {
                                    seeds.push(pv.name.clone());
                                }
                                for r in &p.requires {
                                    seeds.push(r.name.clone());
                                }
                            }
                            let mut src = MetasSource {
                                metas,
                                offsets: &offsets,
                                extra: &extra_empty,
                                allow_arches: &allow,
                                priorities,
                                reachable: None,
                            };
                            let include_recommends = crate::sys::load_config()
                                .map(|c| c.main.install_weak_deps)
                                .unwrap_or(true);
                            let reachable =
                                src.compute_reachable(&seeds, &inst_prov, include_recommends);
                            src.reachable = Some(reachable);
                            // No new root requirement: we are reconciling the present
                            // set. Each cascade package is soft-required, so the
                            // solver keeps it if possible and drops (erases) it if
                            // not, pulling any needed repo alternative into to_install.
                            if let Ok(res) = resolve_sat_scoped_opt(
                                &[],
                                &src,
                                &inst_prov,
                                &cascade,
                                include_recommends,
                            ) {
                                for id in res.to_erase {
                                    let i = cascade_idx[id];
                                    if !winner_names.contains(inst[i].name.as_str()) {
                                        push_erase(&mut erases, &inst[i]);
                                    }
                                }
                                // A kept cascade package satisfied by a NEW repo
                                // alternative: pull that alternative into the install
                                // set so the dependent is not left broken.
                                for gid in res.to_install {
                                    if let Some(pkg) = rehydrate(gid) {
                                        if !seen.contains(&pkg.name) {
                                            seen.insert(pkg.name.clone());
                                            winners.push(pkg);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let ids = (0..winners.len()).collect::<Vec<_>>();
        (winners, ids, erases)
    };

    Ok(Resolution {
        packages: packages_out,
        base_urls: synced.base_urls,
        clients: synced.clients,
        ids,
        erases,
    })
}

/// A [`CandidateSource`] over the synced repos' zero-copy views. Reads package
/// data straight from the mmap'd metadata; the only owned allocations per
/// candidate are the transient `Dep` vectors, dropped after each visit.
struct MetasSource<'a> {
    metas: &'a [RepoMetadata],
    offsets: &'a [usize],
    extra: &'a HashMap<String, Vec<String>>,
    /// Package arches to consider (host arch + noarch, plus any explicitly
    /// requested). Packages of other arches are skipped (multilib policy).
    allow_arches: &'a HashSet<String>,
    /// Repo priority per `metas` index (lower preferred; resolver tie-break).
    priorities: &'a [i32],
    /// Reachable candidate global package IDs pruned via goal-directed BFS.
    /// When `Some`, `scan` and `scan_names` only visit these packages in O(1) time
    /// instead of scanning the full universe of tens of thousands of packages.
    reachable: Option<HashSet<usize>>,
}

impl<'a> MetasSource<'a> {
    pub fn compute_reachable(
        &self,
        targets: &[String],
        installed: &[(String, Option<Evr>)],
        include_recommends: bool,
    ) -> HashSet<usize> {
        let mut queue = std::collections::VecDeque::new();
        let mut visited_caps: HashSet<String> = HashSet::new();
        let mut reachable: HashSet<usize> = HashSet::new();

        let mut inst_map: HashMap<&str, Vec<Option<&Evr>>> = HashMap::new();
        for (name, evr) in installed {
            inst_map
                .entry(name.as_str())
                .or_default()
                .push(evr.as_ref());
        }

        let is_installed_satisfied = |req: &Dep| -> bool {
            if let Some(provs) = inst_map.get(req.name.as_str()) {
                provs.iter().any(|pe| req.satisfied_by(&req.name, *pe))
            } else {
                false
            }
        };

        for target in targets {
            queue.push_back(target.clone());
        }

        while let Some(cap) = queue.pop_front() {
            if !visited_caps.insert(cap.clone()) {
                continue;
            }

            let base_name = if let Some(arch) = explicit_arch(&cap) {
                cap.strip_suffix(&format!(".{arch}")).unwrap_or(&cap)
            } else {
                &cap
            };

            let mut enqueue_pkg_deps = |p: rum_repo::PkgView<'_>| {
                for req in p.requires() {
                    if is_ignorable_dep(&req.name) {
                        continue;
                    }
                    if is_rich(&req.name) {
                        if let Some(expr) = parse_rich(&req.name) {
                            let mut names = HashSet::new();
                            collect_rich_names(&expr, &mut names);
                            for n in names {
                                queue.push_back(n);
                            }
                        }
                        continue;
                    }
                    if !is_installed_satisfied(&req) {
                        queue.push_back(req.name.clone());
                    }
                }
                if include_recommends {
                    for rec in p.recommends() {
                        if is_ignorable_dep(&rec.name) || is_rich(&rec.name) {
                            continue;
                        }
                        if !is_installed_satisfied(&rec) {
                            queue.push_back(rec.name.clone());
                        }
                    }
                }
                for con in p.conflicts() {
                    if is_ignorable_dep(&con.name) || is_rich(&con.name) {
                        continue;
                    }
                    queue.push_back(con.name.clone());
                }
            };

            // 1. By name
            for (ri, m) in self.metas.iter().enumerate() {
                let base_offset = self.offsets[ri];
                for pi in m.indices_by_name(base_name) {
                    if let Some(p) = m.view_at(pi) {
                        if !self.allow_arches.contains(p.arch()) {
                            continue;
                        }
                        if p.name() == cap || p.name_arch() == cap || p.name() == base_name {
                            let gid = base_offset + pi;
                            if reachable.insert(gid) {
                                enqueue_pkg_deps(p);
                            }
                        }
                    }
                }
            }

            // 2. By provide
            for (ri, m) in self.metas.iter().enumerate() {
                let base_offset = self.offsets[ri];
                for pi in m.indices_by_provide(&cap) {
                    if let Some(p) = m.view_at(pi) {
                        if !self.allow_arches.contains(p.arch()) {
                            continue;
                        }
                        let gid = base_offset + pi;
                        if reachable.insert(gid) {
                            enqueue_pkg_deps(p);
                        }
                    }
                }
            }

            // 3. Extra filelists (if any loaded)
            for (pkg_hex, files) in self.extra {
                if files.iter().any(|f| f == &cap) {
                    for (ri, m) in self.metas.iter().enumerate() {
                        let base_offset = self.offsets[ri];
                        for (pi, p) in m.views().enumerate() {
                            if p.checksum_hex() == pkg_hex {
                                let gid = base_offset + pi;
                                if reachable.insert(gid) {
                                    enqueue_pkg_deps(p);
                                }
                            }
                        }
                    }
                }
            }
        }

        reachable
    }
}

impl CandidateSource for MetasSource<'_> {
    fn scan(&self, required: &HashSet<String>, visit: &mut dyn FnMut(CandidateRef<'_>)) {
        if let Some(ref reachable) = self.reachable {
            let mut sorted: Vec<usize> = reachable.iter().copied().collect();
            sorted.sort_unstable();
            for gid in sorted {
                let ri = self
                    .offsets
                    .partition_point(|&o| o <= gid)
                    .saturating_sub(1);
                let pi = gid - self.offsets[ri];
                if let Some(p) = self.metas[ri].view_at(pi) {
                    if !self.allow_arches.contains(p.arch()) {
                        continue;
                    }
                    let mut provides = p.provides_with_files_filtered(required);
                    if let Some(files) = self.extra.get(p.checksum_hex()) {
                        for f in files {
                            if required.contains(f) {
                                provides.push(Dep::unversioned(f.clone()));
                            }
                        }
                    }
                    let requires = p.requires();
                    let recommends = p.recommends();
                    let conflicts = p.conflicts();
                    visit(CandidateRef {
                        id: gid,
                        name: p.name(),
                        arch: p.arch(),
                        evr: p.evr_cmp(),
                        provides: &provides,
                        requires: &requires,
                        recommends: &recommends,
                        conflicts: &conflicts,
                        priority: self.priorities.get(ri).copied().unwrap_or(99),
                    });
                }
            }
        } else {
            for (ri, m) in self.metas.iter().enumerate() {
                let base = self.offsets[ri];
                for (pi, p) in m.views().enumerate() {
                    // Skip disallowed arches; `pi` still advances so global ids
                    // (base + pi) stay aligned with `rehydrate`.
                    if !self.allow_arches.contains(p.arch()) {
                        continue;
                    }
                    let mut provides = p.provides_with_files_filtered(required);
                    if let Some(files) = self.extra.get(p.checksum_hex()) {
                        for f in files {
                            if required.contains(f) {
                                provides.push(Dep::unversioned(f.clone()));
                            }
                        }
                    }
                    let requires = p.requires();
                    let recommends = p.recommends();
                    let conflicts = p.conflicts();
                    visit(CandidateRef {
                        id: base + pi,
                        name: p.name(),
                        arch: p.arch(),
                        evr: p.evr_cmp(),
                        provides: &provides,
                        requires: &requires,
                        recommends: &recommends,
                        conflicts: &conflicts,
                        priority: self.priorities.get(ri).copied().unwrap_or(99),
                    });
                }
            }
        }
    }

    fn scan_names(&self, visit: &mut dyn FnMut(NameView<'_>)) {
        if let Some(ref reachable) = self.reachable {
            let mut sorted: Vec<usize> = reachable.iter().copied().collect();
            sorted.sort_unstable();
            for gid in sorted {
                let ri = self
                    .offsets
                    .partition_point(|&o| o <= gid)
                    .saturating_sub(1);
                let pi = gid - self.offsets[ri];
                if let Some(p) = self.metas[ri].view_at(pi) {
                    if !self.allow_arches.contains(p.arch()) {
                        continue;
                    }
                    let provide_names = p.provide_names();
                    let require_names = p.require_names();
                    let recommend_names = p.recommend_names();
                    let conflict_names = p.conflict_names();
                    visit(NameView {
                        name: p.name(),
                        arch: p.arch(),
                        provide_names: &provide_names,
                        require_names: &require_names,
                        recommend_names: &recommend_names,
                        conflict_names: &conflict_names,
                    });
                }
            }
        } else {
            for m in self.metas {
                for p in m.views() {
                    if !self.allow_arches.contains(p.arch()) {
                        continue;
                    }
                    let provide_names = p.provide_names();
                    let require_names = p.require_names();
                    let recommend_names = p.recommend_names();
                    let conflict_names = p.conflict_names();
                    visit(NameView {
                        name: p.name(),
                        arch: p.arch(),
                        provide_names: &provide_names,
                        require_names: &require_names,
                        recommend_names: &recommend_names,
                        conflict_names: &conflict_names,
                    });
                }
            }
        }
    }
}

/// Allowed package arches for a target set: host arch + noarch, plus any arch
/// a target explicitly names (`glibc.i686`).
fn arches_for(targets: &[String]) -> HashSet<String> {
    let mut a = HashSet::new();
    a.insert(std::env::consts::ARCH.to_string());
    a.insert("noarch".to_string());
    for spec in targets {
        if let Some(arch) = explicit_arch(spec) {
            a.insert(arch.to_string());
        }
    }
    a
}

/// Find the repo package named `name` at exactly EVR `evr` across all repos,
/// returned as an owned `AvailablePackage` (used to pull a co-built sibling at
/// the same new version as the package it's pinned to).
/// Uses binary search over repo indices in O(log N).
fn find_pkg_at(name: &str, evr: &Evr, metas: &[RepoMetadata]) -> Option<AvailablePackage> {
    for m in metas {
        for pi in m.indices_by_name(name) {
            if let Some(p) = m.view_at(pi) {
                if &p.evr_cmp() == evr {
                    return m.rehydrate(pi);
                }
            }
        }
    }
    None
}

/// The trailing `.arch` of a spec, if it names a known RPM architecture
/// (so `glibc.i686` re-allows i686, but `python3.11` is not an arch).
fn explicit_arch(spec: &str) -> Option<&str> {
    const ARCHES: &[&str] = &[
        "x86_64", "i686", "i386", "aarch64", "noarch", "armv7hl", "ppc64le", "s390x", "riscv64",
    ];
    let (_, arch) = spec.rsplit_once('.')?;
    ARCHES.contains(&arch).then_some(arch)
}

/// Highest-EVR package matching `spec` (name or name.arch or virtual capability)
/// across all repos, restricted to allowed arches, returned as a global index.
/// Uses binary search over repo name and provide indices in O(log N).
fn best_match_views(
    spec: &str,
    metas: &[RepoMetadata],
    offsets: &[usize],
    allow_arches: &HashSet<String>,
) -> Option<usize> {
    let base_name = if let Some(arch) = explicit_arch(spec) {
        spec.strip_suffix(&format!(".{arch}")).unwrap_or(spec)
    } else {
        spec
    };

    let mut best: Option<(usize, Evr)> = None;
    let mut found_by_name = false;

    // Fast path: binary search by package name in O(log N).
    for (ri, m) in metas.iter().enumerate() {
        for pi in m.indices_by_name(base_name) {
            found_by_name = true;
            if let Some(p) = m.view_at(pi) {
                if !allow_arches.contains(p.arch()) {
                    continue;
                }
                if p.name() == spec || p.name_arch() == spec {
                    let evr = p.evr_cmp();
                    let better = match &best {
                        Some((_, b)) => evr > *b,
                        None => true,
                    };
                    if better {
                        best = Some((offsets[ri] + pi, evr));
                    }
                }
            }
        }
    }

    if found_by_name && best.is_some() {
        return best.map(|(g, _)| g);
    }

    // Fallback: spec might be a virtual capability or file provide (e.g. `pkgconfig(...)`).
    // Binary search provide_index in O(log P).
    for (ri, m) in metas.iter().enumerate() {
        for pi in m.indices_by_provide(spec) {
            if let Some(p) = m.view_at(pi) {
                if !allow_arches.contains(p.arch()) {
                    continue;
                }
                let evr = p.evr_cmp();
                let better = match &best {
                    Some((_, b)) => evr > *b,
                    None => true,
                };
                if better {
                    best = Some((offsets[ri] + pi, evr));
                }
            }
        }
    }

    best.map(|(g, _)| g)
}

/// File-path requirements (`/...`) that nothing provides via primary
/// provides/files or the installed system — candidates for the filelists
/// fallback.
fn unmet_file_requires_views(
    metas: &[RepoMetadata],
    installed: &[(String, Option<Evr>)],
) -> HashSet<String> {
    let mut providable: HashSet<String> = HashSet::new();
    for m in metas {
        for p in m.views() {
            providable.insert(p.name().to_string());
            for n in p.provide_names() {
                providable.insert(n.to_string());
            }
            for n in p.file_names() {
                providable.insert(n.to_string());
            }
        }
    }
    for (n, _) in installed {
        providable.insert(n.clone());
    }
    let mut wanted = HashSet::new();
    for m in metas {
        for p in m.views() {
            for r in p.require_names() {
                if r.starts_with('/') && !providable.contains(r) {
                    wanted.insert(r.to_string());
                }
            }
        }
    }
    wanted
}

/// Download a resolved set to `destdir`, verifying checksums.
pub fn fetch(resolution: &Resolution, destdir: &Path) -> anyhow::Result<Fetched> {
    std::fs::create_dir_all(destdir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", destdir.display()))?;

    let mut jobs: Vec<Job> = Vec::new();
    for &i in &resolution.ids {
        let p = &resolution.packages[i];
        let base = resolution
            .base_urls
            .get(&p.repo_id)
            .cloned()
            .unwrap_or_default();
        if base.is_empty() {
            anyhow::bail!(
                "no base URL known for repo `{}` (run `rum makecache`)",
                p.repo_id
            );
        }
        jobs.push(Job {
            url: join_url(&base, &p.location),
            dest: destdir.join(basename(&p.location)),
            checksum: p.checksum.clone(),
            nevra: p.nevra(),
            repo_id: p.repo_id.clone(),
            size: p.size,
        });
    }

    // Sort jobs by size descending (Longest Processing Time first scheduling)
    // so large packages start downloading immediately across worker threads,
    // avoiding the long-tail straggler problem where a large RPM stalls at the end.
    jobs.sort_by_key(|a| std::cmp::Reverse(a.size));

    let total_bytes = resolution.total_bytes();
    let start = std::time::Instant::now();
    let failures = download_all(&jobs, &resolution.clients);
    let elapsed = start.elapsed();

    if !failures.is_empty() {
        for f in &failures {
            eprintln!("FAILED {f}");
        }
        anyhow::bail!("{} package(s) failed to download", failures.len());
    }

    // Return files preserving the deterministic resolution order.
    let files: Vec<PathBuf> = resolution
        .ids
        .iter()
        .map(|&i| destdir.join(basename(&resolution.packages[i].location)))
        .collect();

    Ok(Fetched {
        files,
        total_bytes,
        elapsed,
    })
}

struct Job {
    url: String,
    dest: PathBuf,
    checksum: rum_repo::Checksum,
    nevra: String,
    repo_id: String,
    size: u64,
}

/// Download all jobs across a bounded set of worker threads, using each repo's
/// own HTTP client (so mutual-TLS repos present their client certificate).
/// Uses a dynamic atomic work-queue so faster downloads do not stall behind
/// stragglers.
fn download_all(jobs: &[Job], clients: &std::collections::HashMap<String, Http>) -> Vec<String> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(jobs.len())
        .min(16);

    let default = Http::new();
    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let mut failures = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let default = &default;
            handles.push(scope.spawn(|| {
                let mut errs = Vec::new();
                loop {
                    let idx = next_idx.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if idx >= jobs.len() {
                        break;
                    }
                    let job = &jobs[idx];
                    let http = clients.get(&job.repo_id).unwrap_or(default);
                    if let Err(e) = download_one(http, job) {
                        errs.push(format!("{}: {e}", job.nevra));
                    }
                }
                errs
            }));
        }
        for h in handles {
            if let Ok(mut errs) = h.join() {
                failures.append(&mut errs);
            }
        }
    });
    failures
}

fn download_one(http: &Http, job: &Job) -> anyhow::Result<()> {
    if job.dest.exists() && job.checksum.verify_file(&job.dest) {
        return Ok(());
    }
    http.download_file(&job.url, &job.dest, Some(&job.checksum))
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Convert a scoped [`InstalledPkg`] into a solver [`PresentPackage`] (an
fn installed_matches_dep(p: &rum_rpm::InstalledPkg, dep: &rum_solve::Dep) -> bool {
    if dep.name == p.name {
        let evr = Evr::parse(&p.evr);
        if dep.satisfied_by(&p.name, Some(&evr)) {
            return true;
        }
    }
    for (prov_name, prov_ver) in &p.provides {
        if dep.name == *prov_name {
            let evr = prov_ver.as_ref().map(|v| Evr::parse(v));
            if dep.satisfied_by(prov_name, evr.as_ref()) {
                return true;
            }
        }
    }
    false
}

/// Convert an installed package into a solver `PresentPackage` (a real,
/// erasable, soft-kept installed package). Its provides/requires carry
/// exact or unversioned constraints matching the installed database.
fn present_package(id: usize, p: &rum_rpm::InstalledPkg) -> PresentPackage {
    PresentPackage {
        id,
        name: p.name.clone(),
        evr: Evr::parse(&p.evr),
        provides: p
            .provides
            .iter()
            .map(|(n, v)| rum_solve::Dep {
                name: n.clone(),
                flag: if v.is_some() {
                    rum_solve::DepFlag::Eq
                } else {
                    rum_solve::DepFlag::Any
                },
                evr: v.as_ref().map(|s| Evr::parse(s)),
            })
            .collect(),
        requires: p.requires.iter().map(Dep::unversioned).collect(),
    }
}

/// Push an erase entry `(name, version, release)` for an installed package,
/// splitting its `[epoch:]version-release` string (epoch is dropped — the erase
/// matches on version+release, as `rpm -e name-version-release` does).
fn push_erase(erases: &mut Vec<(String, String, String)>, p: &rum_rpm::InstalledPkg) {
    let evr = p.evr.rsplit(':').next().unwrap_or(&p.evr); // strip leading epoch
    if let Some((version, release)) = evr.rsplit_once('-') {
        let entry = (p.name.clone(), version.to_string(), release.to_string());
        if !erases.contains(&entry) {
            erases.push(entry);
        }
    }
}

/// Installed provides (capabilities + files) as (name, optional EVR).
fn installed_provides() -> Vec<(String, Option<Evr>)> {
    match rum_rpm::Rpmdb::open() {
        Ok(db) => db
            .all_provides()
            .into_iter()
            .map(|(name, ver)| (name, ver.map(|s| Evr::parse(&s))))
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn join_url(base: &str, href: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        encode_path(href.trim_start_matches('/'))
    )
}

/// Percent-encode a URL path (RFC 3986): every byte outside the unreserved set
/// (ALPHA / DIGIT / `-._~`) is `%`-escaped, except the `/` separator, `%` (so
/// an already-encoded href is not double-encoded), and `?`/`#`/`&`/`=` so any
/// query string is preserved. Repo `location` hrefs are raw filenames, so a
/// literal `+` (e.g. `gcc-c++-...rpm`) must become `%2B` — S3 treats an
/// unencoded `+` as a different key and returns 403 AccessDenied, which is why
/// `+`-named packages failed to download while every other package worked.
fn encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'/'
            | b'%'
            | b'?'
            | b'#'
            | b'&'
            | b'=' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn basename(location: &str) -> &str {
    location.rsplit('/').next().unwrap_or(location)
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_escapes_plus_keeps_structure() {
        // '+' must become %2B (the gcc-c++ / libstdc++ download bug); path
        // separators, '..', and unreserved chars are preserved.
        assert_eq!(
            encode_path("../../../../blobstore/abc/gcc-c++-11.5.0-5.amzn2023.x86_64.rpm"),
            "../../../../blobstore/abc/gcc-c%2B%2B-11.5.0-5.amzn2023.x86_64.rpm"
        );
        // Spaces encode; already-encoded input is not double-encoded.
        assert_eq!(encode_path("a b"), "a%20b");
        assert_eq!(encode_path("a%2Bb"), "a%2Bb");
        // A query string is preserved verbatim.
        assert_eq!(
            encode_path("blobstore/x/f.rpm?k=v"),
            "blobstore/x/f.rpm?k=v"
        );
    }

    #[test]
    fn join_url_encodes_href() {
        assert_eq!(
            join_url(
                "https://h/core/x86_64/",
                "../../blobstore/z/libstdc++-1.rpm"
            ),
            "https://h/core/x86_64/../../blobstore/z/libstdc%2B%2B-1.rpm"
        );
    }

    #[test]
    fn download_all_skips_existing_cached_files() {
        let tmp = std::env::temp_dir().join(format!("rum-test-cache-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);

        // Pre-create file with known content "hello"
        let f1 = tmp.join("f1.rpm");
        std::fs::write(&f1, b"hello").unwrap();
        // echo -n "hello" | sha256sum
        let cs1 = rum_repo::Checksum {
            kind: rum_repo::ChecksumKind::Sha256,
            hex: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
        };

        let f2 = tmp.join("f2.rpm");
        std::fs::write(&f2, b"world").unwrap();
        // echo -n "world" | sha256sum
        let cs2 = rum_repo::Checksum {
            kind: rum_repo::ChecksumKind::Sha256,
            hex: "486ea46224d1bb4fb680f34f7c9ad96a8f24ec88be73ea8e5a6c65260e9cb8a7".into(),
        };

        let jobs = vec![
            Job {
                url: "http://example.invalid/f1.rpm".into(),
                dest: f1.clone(),
                checksum: cs1,
                nevra: "f1-1.0-1.x86_64".into(),
                repo_id: "test".into(),
                size: 5,
            },
            Job {
                url: "http://example.invalid/f2.rpm".into(),
                dest: f2.clone(),
                checksum: cs2,
                nevra: "f2-1.0-1.x86_64".into(),
                repo_id: "test".into(),
                size: 5,
            },
        ];

        // Should succeed without making any network calls because both are valid cache hits.
        let failures = download_all(&jobs, &std::collections::HashMap::new());
        assert!(failures.is_empty(), "expected 0 failures, got {failures:?}");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
