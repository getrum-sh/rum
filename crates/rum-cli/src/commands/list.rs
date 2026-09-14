//! `rum list [installed|available|all] [patterns...]`

use super::{pkgindex, repo_sync};
use crate::glob;
use rum_rpm::{Package, Rpmdb};

pub fn run(what: &str, patterns: &[String]) -> anyhow::Result<()> {
    match what {
        "installed" => list_installed(patterns),
        "available" => list_available(patterns),
        "all" => list_all(patterns),
        other => anyhow::bail!("unknown list type `{other}` (expected: installed, available, all)"),
    }
}

fn list_installed(patterns: &[String]) -> anyhow::Result<()> {
    let db = Rpmdb::open().map_err(|e| anyhow::anyhow!("cannot open rpmdb: {e}"))?;
    let mut pkgs = db.installed();
    if !patterns.is_empty() {
        pkgs.retain(|p| patterns.iter().any(|pat| installed_matches(pat, p)));
    }
    pkgs.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.arch.cmp(&b.arch)));

    if pkgs.is_empty() {
        return empty_result(patterns);
    }

    let name_w = width(pkgs.iter().map(|p| p.name_arch().len()), 20);
    let evr_w = width(pkgs.iter().map(|p| p.evr().len()), 12);
    println!("Installed Packages");
    for p in &pkgs {
        println!(
            "{:<name_w$}  {:<evr_w$}  @System",
            p.name_arch(),
            p.evr(),
            name_w = name_w,
            evr_w = evr_w
        );
    }
    Ok(())
}

fn list_available(patterns: &[String]) -> anyhow::Result<()> {
    let synced = repo_sync::sync_enabled(false)?;
    warn_failed(&synced.repos);

    // Latest available version per name.arch (dnf's default, not
    // --showduplicates), excluding anything already installed at an equal or
    // newer EVR (nothing to offer).
    let installed = pkgindex::installed_best();
    let best = pkgindex::available_best(synced.metas());

    let mut avail: Vec<_> = best
        .into_values()
        .filter(|p| match installed.get(&p.name_arch()) {
            Some(inst) => p.evr_cmp() > *inst,
            None => true,
        })
        .filter(|p| patterns.is_empty() || patterns.iter().any(|pat| available_matches(pat, p)))
        .collect();
    avail.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.arch.cmp(&b.arch)));
    if avail.is_empty() {
        return empty_result(patterns);
    }

    let name_w = width(avail.iter().map(|p| p.name_arch().len()), 20);
    let evr_w = width(avail.iter().map(|p| p.evr().len()), 12);
    println!("Available Packages");
    for p in &avail {
        println!(
            "{:<name_w$}  {:<evr_w$}  {}",
            p.name_arch(),
            p.evr(),
            p.repo_id,
            name_w = name_w,
            evr_w = evr_w
        );
    }
    Ok(())
}

fn list_all(patterns: &[String]) -> anyhow::Result<()> {
    list_installed(patterns).ok();
    println!();
    list_available(patterns)
}

fn installed_matches(pattern: &str, p: &Package) -> bool {
    glob::matches(pattern, &p.name) || glob::matches(pattern, &p.name_arch())
}

fn available_matches(pattern: &str, p: &pkgindex::AvailRow) -> bool {
    glob::matches(pattern, &p.name) || glob::matches(pattern, &p.name_arch())
}

fn width(lens: impl Iterator<Item = usize>, min: usize) -> usize {
    lens.max().unwrap_or(min).max(min)
}

fn warn_failed(repos: &[repo_sync::RepoStat]) {
    for r in repos {
        if let Some(e) = &r.error {
            eprintln!("warning: repo `{}` skipped: {e}", r.id);
        }
    }
}

fn empty_result(patterns: &[String]) -> anyhow::Result<()> {
    if patterns.is_empty() {
        println!("No packages to list.");
        Ok(())
    } else {
        anyhow::bail!("No matching packages.")
    }
}
