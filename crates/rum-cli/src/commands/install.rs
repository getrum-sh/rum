//! `rum install [-y] <packages...>`
//!
//! Resolves the dependency closure, downloads the RPMs (checksum-verified),
//! shows the transaction, and commits it. When rum runs as root it commits
//! natively through librpm (`rpmtsRun`) via [`rum_rpm::Transaction`]; otherwise
//! (or when `RUM_USE_RPM_BINARY=1`) it shells out to `sudo rpm -U`. Either way
//! rpm performs its own dependency/conflict check and ordering before writing
//! the rpmdb, so a resolver mistake fails cleanly rather than corrupting the
//! system.

use std::path::PathBuf;

use super::{confirm, download};
use crate::sys;
use rum_rpm::Transaction;

pub fn run(packages: &[String], assume_yes: bool, nodocs: bool) -> anyhow::Result<()> {
    if packages.is_empty() {
        anyhow::bail!("Error: No packages specified");
    }

    // Resolve first (no download yet), so we can show the transaction and
    // confirm before fetching anything — matching dnf's order.
    let resolution = download::resolve_packages(packages, true)?;
    if resolution.is_empty() {
        if let Ok(db) = rum_rpm::Rpmdb::open() {
            for p in packages {
                for m in db.by_name(p) {
                    println!("Package \"{}\" is already installed.", m.nevra());
                }
            }
        }
        println!("Nothing to do.");
        return Ok(());
    }

    execute_transaction(&resolution, packages, assume_yes, false, nodocs)
}

pub fn execute_transaction(
    resolution: &download::Resolution,
    targets: &[String],
    assume_yes: bool,
    is_upgrade: bool,
    nodocs: bool,
) -> anyhow::Result<()> {
    let db = rum_rpm::Rpmdb::open().ok();
    let mut upgrading = Vec::new();
    let mut installing = Vec::new();

    for p in resolution.winner_packages() {
        if db.as_ref().is_some_and(|d| d.is_installed(&p.name)) {
            upgrading.push(p.nevra());
        } else {
            installing.push(p.nevra());
        }
    }

    if !upgrading.is_empty() {
        println!("\n{} {} package(s):", crate::ui::bold_cyan("Upgrading"), upgrading.len());
        for n in &upgrading {
            println!("  {} {n}", crate::ui::cyan("▲"));
        }
    }

    if !installing.is_empty() {
        let label = if is_upgrade {
            "Installing dependencies"
        } else {
            "Installing"
        };
        println!("\n{} {} package(s):", crate::ui::bold_green(label), installing.len());
        for n in &installing {
            println!("  {} {n}", crate::ui::green("+"));
        }
    }

    // installonly_limit: installing a new kernel (or other install-only package)
    // keeps old versions side-by-side; prune the oldest beyond the limit, never
    // the running kernel or the one just installed. rpm won't do this — it's
    // dnf's (now rum's) job, added to the same transaction.
    let mut prunes = installonly_prunes_for(resolution);
    if !prunes.is_empty() {
        println!("\nRemoving {} old install-only package(s):", prunes.len());
        for (n, v, r) in &prunes {
            println!("  {n}-{v}-{r}");
        }
    }

    // Scoped erases: installed packages a winner Obsoletes/Conflicts, plus any
    // dependent the resolver could not keep. Committed via the same erase path
    // as install-only prunes (rpm still does its own dependency check).
    let scoped_erases = resolution.erases();
    if !scoped_erases.is_empty() {
        println!(
            "\nRemoving {} conflicting/obsoleted package(s):",
            scoped_erases.len()
        );
        for (n, v, r) in scoped_erases {
            println!("  {n}-{v}-{r}");
        }
        for e in scoped_erases {
            if !prunes.contains(e) {
                prunes.push(e.clone());
            }
        }
    }

    println!("\nTotal download size: {}", human(resolution.total_bytes()));

    if !assume_yes && !confirm() {
        println!("Operation cancelled.");
        return Ok(());
    }

    // The resolve just peaked heap usage building the pool; hand freed pages
    // back to the OS so the native rpm transaction has headroom on small hosts.
    sys::release_free_memory();

    // rpm filenames of install-only winners — these must be added as installs
    // (not upgrades) so rpm keeps the existing builds alongside.
    let installonly_files: std::collections::HashSet<String> = resolution
        .winner_packages()
        .filter(|p| is_install_only(p))
        .filter_map(|p| p.location.rsplit('/').next().map(|s| s.to_string()))
        .collect();

    // Download into rum's package cache, then commit.
    let pkgdir = sys::effective_cachedir("/var/cache/rum").join("packages");
    let fetched = download::fetch(resolution, &pkgdir)?;
    commit_install(&fetched.files, &prunes, &installonly_files, nodocs)?;

    let mut altered = Vec::new();
    for p in resolution.winner_packages() {
        let is_upg = db.as_ref().is_some_and(|d| d.is_installed(&p.name));
        let state = if is_upg {
            crate::history::PackageState::Upgraded
        } else {
            crate::history::PackageState::Installed
        };
        let is_explicit = targets.iter().any(|t| {
            t == &p.name || t == &p.nevra() || t == &format!("{}.{}", p.name, p.arch)
        });
        altered.push(crate::history::AlteredPackage {
            nevra: p.nevra(),
            state,
            is_explicit,
        });
    }
    for (n, v, r) in &prunes {
        altered.push(crate::history::AlteredPackage {
            nevra: format!("{n}-{v}-{r}"),
            state: crate::history::PackageState::Removed,
            is_explicit: false,
        });
    }

    let cmd_line = std::env::args().collect::<Vec<_>>().join(" ");
    let action_label = if is_upgrade { "Upgrade" } else { "Install" };
    crate::history::record_transaction(&cmd_line, action_label, targets, altered);

    Ok(())
}

/// Is this fetched rpm file an install-only package (add without upgrade)?
fn is_installonly_file(
    f: &std::path::Path,
    installonly_files: &std::collections::HashSet<String>,
) -> bool {
    f.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| installonly_files.contains(n))
}

/// Detect install-only packages (kernels): a package that provides an
/// `installonlypkg(...)` capability, or the `kernel`/`kernel-core` names.
fn is_install_only(p: &rum_repo::AvailablePackage) -> bool {
    p.name == "kernel"
        || p.name == "kernel-core"
        || p.provides
            .iter()
            .any(|d| d.name.starts_with("installonlypkg("))
}

/// Compute the (name, version, release) builds to erase to honour
/// installonly_limit for this transaction's install-only winners.
fn installonly_prunes_for(resolution: &download::Resolution) -> Vec<(String, String, String)> {
    let Ok(db) = rum_rpm::Rpmdb::open() else {
        return Vec::new();
    };
    let limit = crate::sys::load_config()
        .map(|c| c.main.installonly_limit as usize)
        .unwrap_or(3)
        .max(1);
    let running = crate::sys::running_kernel_release();
    let mut names: Vec<&str> = resolution
        .winner_packages()
        .filter(|p| is_install_only(p))
        .map(|p| p.name.as_str())
        .collect();
    names.sort_unstable();
    names.dedup();

    let mut out = Vec::new();
    for name in names {
        let new_builds: Vec<(String, String)> = resolution
            .winner_packages()
            .filter(|p| p.name == name)
            .map(|p| (p.version.clone(), p.release.clone()))
            .collect();
        let installed: Vec<(String, String)> = db
            .by_name(name)
            .iter()
            .map(|p| (p.version.clone(), p.release.clone()))
            .collect();
        for (v, r) in installonly_prunes(&installed, running.as_deref(), &new_builds, limit) {
            out.push((name.to_string(), v, r));
        }
    }
    out
}

/// Pure prune selection: of the builds present after installing `new_builds`
/// (installed ∪ new), keep at most `limit`, erasing the OLDEST first but never
/// the running kernel or a just-installed build.
fn installonly_prunes(
    installed: &[(String, String)],
    running: Option<&str>,
    new_builds: &[(String, String)],
    limit: usize,
) -> Vec<(String, String)> {
    use rum_solve::Evr;
    let mut all: Vec<(String, String)> = installed.to_vec();
    for nb in new_builds {
        if !all.contains(nb) {
            all.push(nb.clone());
        }
    }
    if all.len() <= limit {
        return Vec::new();
    }
    all.sort_by(|a, b| Evr::new(None, &a.0, &a.1).compare(&Evr::new(None, &b.0, &b.1)));
    let is_running = |b: &(String, String)| {
        running.is_some_and(|rk| rk.starts_with(&format!("{}-{}", b.0, b.1)))
    };
    let mut excess = all.len() - limit;
    let mut prune = Vec::new();
    for b in &all {
        if excess == 0 {
            break;
        }
        if is_running(b) || new_builds.contains(b) {
            continue; // never prune the running kernel or the just-installed build
        }
        prune.push(b.clone());
        excess -= 1;
    }
    prune
}

fn commit_install(
    files: &[PathBuf],
    prunes: &[(String, String, String)],
    installonly_files: &std::collections::HashSet<String>,
    nodocs: bool,
) -> anyhow::Result<()> {
    let use_binary = std::env::var_os("RUM_USE_RPM_BINARY").is_some();
    if sys::is_root() && !use_binary {
        commit_install_native(files, prunes, installonly_files, nodocs)
    } else {
        commit_install_rpm_binary(files, prunes, installonly_files, nodocs)
    }
}

/// Commit natively through librpm (`rpmtsRun`), no subprocess. Requires root.
/// Install-only files are added without the upgrade flag (kept alongside), and
/// old install-only builds are erased in the SAME transaction.
fn commit_install_native(
    files: &[PathBuf],
    prunes: &[(String, String, String)],
    installonly_files: &std::collections::HashSet<String>,
    nodocs: bool,
) -> anyhow::Result<()> {
    let mut tx =
        Transaction::new().map_err(|e| anyhow::anyhow!("cannot start transaction: {e}"))?;
    if nodocs {
        tx.set_nodocs(true);
    }
    for f in files {
        let upgrade = !is_installonly_file(f, installonly_files);
        tx.add_install(f, upgrade)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    for (n, v, r) in prunes {
        tx.add_erase_exact(n, v, r)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    tx.run(false).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{} Complete!", crate::ui::bold_green("✔"));
    Ok(())
}

/// Fallback path: `sudo rpm -Uvh` (used when rum is not root, or when
/// `RUM_USE_RPM_BINARY` is set). `-U` installs new and upgrades existing, in one
/// ordered transaction with scriptlets; `-h` shows a progress hash. Any
/// install-only prunes are then erased with `rpm -e`.
fn commit_install_rpm_binary(
    files: &[PathBuf],
    prunes: &[(String, String, String)],
    installonly_files: &std::collections::HashSet<String>,
    nodocs: bool,
) -> anyhow::Result<()> {
    let (io, reg): (Vec<&PathBuf>, Vec<&PathBuf>) = files
        .iter()
        .partition(|f| is_installonly_file(f, installonly_files));
    // Regular packages upgrade in place (-U); install-only ones are added
    // alongside (-i) so existing builds are kept.
    for (flag, set) in [("-Uvh", reg), ("-ivh", io)] {
        if set.is_empty() {
            continue;
        }
        let mut cmd = sys::privileged("rpm");
        cmd.arg(flag);
        if nodocs {
            cmd.arg("--nodocs");
        }
        for f in set {
            cmd.arg(f);
        }
        let status = cmd
            .status()
            .map_err(|e| anyhow::anyhow!("failed to launch rpm: {e}"))?;
        if !status.success() {
            anyhow::bail!("rpm transaction failed (exit {:?})", status.code());
        }
    }
    if !prunes.is_empty() {
        let mut ecmd = sys::privileged("rpm");
        ecmd.arg("-e");
        for (n, v, r) in prunes {
            ecmd.arg(format!("{n}-{v}-{r}"));
        }
        let est = ecmd
            .status()
            .map_err(|e| anyhow::anyhow!("failed to launch rpm -e: {e}"))?;
        if !est.success() {
            anyhow::bail!(
                "pruning old install-only packages failed (exit {:?})",
                est.code()
            );
        }
    }
    println!("{} Complete!", crate::ui::bold_green("✔"));
    Ok(())
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
    fn prunes_oldest_keeping_running_and_new() {
        let installed = vec![
            ("6.0.0".into(), "1.el10".into()),
            ("6.1.0".into(), "1.el10".into()),
            ("6.2.0".into(), "1.el10".into()),
        ];
        let new = vec![("6.3.0".into(), "1.el10".into())];
        // 3 installed + 1 new = 4 > limit 3 -> prune the single oldest (6.0.0),
        // keeping 6.1.0, the running 6.2.0, and the just-installed 6.3.0.
        let p = installonly_prunes(&installed, Some("6.2.0-1.el10.x86_64"), &new, 3);
        assert_eq!(p, vec![("6.0.0".to_string(), "1.el10".to_string())]);
    }

    #[test]
    fn no_prune_at_or_under_limit() {
        let installed = vec![("6.0.0".into(), "1".into())];
        let new = vec![("6.1.0".into(), "1".into())];
        assert!(installonly_prunes(&installed, None, &new, 3).is_empty());
    }

    #[test]
    fn never_prunes_running_even_if_oldest() {
        let installed = vec![
            ("6.0.0".into(), "1".into()),
            ("6.1.0".into(), "1".into()),
            ("6.2.0".into(), "1".into()),
        ];
        let new = vec![("6.3.0".into(), "1".into())];
        // Oldest (6.0.0) is the running kernel -> keep it, prune next-oldest.
        let p = installonly_prunes(&installed, Some("6.0.0-1.x86_64"), &new, 3);
        assert_eq!(p, vec![("6.1.0".to_string(), "1".to_string())]);
    }
}
