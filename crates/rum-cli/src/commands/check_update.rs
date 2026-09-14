//! `rum check-update` — list installed packages with a newer version available.
//!
//! Follows dnf's exit-code convention: 100 if updates are available, 0 if none.

use super::{pkgindex, repo_sync};
use crate::glob;

pub fn run(patterns: &[String]) -> anyhow::Result<()> {
    let synced = repo_sync::sync_enabled(false)?;
    for r in &synced.repos {
        if let Some(e) = &r.error {
            eprintln!("warning: repo `{}` skipped: {e}", r.id);
        }
    }

    let installed = pkgindex::installed_best();
    let available = pkgindex::available_best(synced.metas());

    // An update exists where the best available EVR strictly exceeds the best
    // installed EVR for the same name.arch.
    let mut updates: Vec<(&str, String, &str)> = Vec::new();
    for (key, inst_evr) in &installed {
        if !patterns.is_empty() && !patterns.iter().any(|p| glob::matches(p, key)) {
            continue;
        }
        if let Some(avail) = available.get(key) {
            if avail.evr_cmp() > *inst_evr {
                updates.push((key, avail.evr(), &avail.repo_id));
            }
        }
    }
    updates.sort_by(|a, b| a.0.cmp(b.0));

    if updates.is_empty() {
        // dnf prints nothing and exits 0 when up to date.
        return Ok(());
    }

    let name_w = updates
        .iter()
        .map(|(k, ..)| k.len())
        .max()
        .unwrap_or(20)
        .max(20);
    let evr_w = updates
        .iter()
        .map(|(_, e, _)| e.len())
        .max()
        .unwrap_or(12)
        .max(12);
    for (name, evr, repo) in &updates {
        println!("{name:<name_w$}  {evr:<evr_w$}  {repo}");
    }

    // Signal "updates available" the way dnf does.
    std::process::exit(100);
}
