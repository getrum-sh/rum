//! `rum remove [-y] <packages...>`
//!
//! Erases packages. rpm refuses to remove a package that others still depend
//! on (unless forced), so this is safe by default. We show what will be
//! removed, confirm, then commit: natively through librpm (`rpmtsRun`) when rum
//! runs as root, else via `sudo rpm -e` (also when `RUM_USE_RPM_BINARY=1`).

use super::confirm;
use crate::sys;
use rum_rpm::{Rpmdb, Transaction};

pub fn run(packages: &[String], assume_yes: bool) -> anyhow::Result<()> {
    if packages.is_empty() {
        anyhow::bail!("`rum remove` needs at least one package name");
    }

    // Confirm the named packages are actually installed, and show NEVRAs.
    let db = Rpmdb::open().map_err(|e| anyhow::anyhow!("cannot open rpmdb: {e}"))?;
    let mut matched = Vec::new();
    let mut missing = Vec::new();
    for spec in packages {
        let found = db.by_name(spec);
        if found.is_empty() {
            missing.push(spec.clone());
        } else {
            for p in found {
                matched.push(p.nevra());
            }
        }
    }
    if !missing.is_empty() {
        anyhow::bail!("not installed: {}", missing.join(", "));
    }

    // Protected-package safety (dnf's protected_packages): never remove a
    // package that would break the system or rum itself, nor the running
    // kernel. rpm would happily erase these; we refuse before committing.
    let protected = sys::protected_packages();
    if let Some(spec) = packages.iter().find(|s| protected.contains(&s.as_str())) {
        anyhow::bail!("refusing to remove protected package `{spec}`");
    }
    if let Some(running) = sys::running_kernel_release() {
        for spec in packages {
            if spec.starts_with("kernel")
                && db
                    .by_name(spec)
                    .iter()
                    .any(|p| p.nevra().contains(&running))
            {
                anyhow::bail!(
                    "refusing to remove the running kernel ({running}); boot another kernel first"
                );
            }
        }
    }

    println!("Removing {} package(s):", matched.len());
    matched.sort();
    for n in &matched {
        println!("  {n}");
    }

    if !assume_yes && !confirm() {
        println!("Operation cancelled.");
        return Ok(());
    }

    let use_binary = std::env::var_os("RUM_USE_RPM_BINARY").is_some();
    if sys::is_root() && !use_binary {
        commit_remove_native(packages)
    } else {
        commit_remove_rpm_binary(packages)
    }
}

/// Erase natively through librpm (`rpmtsRun`). Requires root.
fn commit_remove_native(packages: &[String]) -> anyhow::Result<()> {
    let mut tx =
        Transaction::new().map_err(|e| anyhow::anyhow!("cannot start transaction: {e}"))?;
    for p in packages {
        tx.add_erase(p).map_err(|e| anyhow::anyhow!("{e}"))?;
    }
    tx.run(false).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("Complete!");
    Ok(())
}

/// Fallback path: `sudo rpm -e` (not root, or `RUM_USE_RPM_BINARY`).
fn commit_remove_rpm_binary(packages: &[String]) -> anyhow::Result<()> {
    let mut cmd = sys::privileged("rpm");
    cmd.arg("-e");
    for p in packages {
        cmd.arg(p);
    }
    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("failed to launch rpm: {e}"))?;

    if status.success() {
        println!("Complete!");
        Ok(())
    } else {
        anyhow::bail!("rpm removal failed (exit {:?})", status.code())
    }
}
