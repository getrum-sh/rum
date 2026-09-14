//! `rum clean [all|metadata|packages]` — remove cached data (like `dnf clean`).

use std::path::Path;

use crate::sys;

pub fn run(what: &str) -> anyhow::Result<()> {
    let cachedir = match sys::load_config() {
        Ok(cfg) => sys::effective_cachedir(&cfg.main.cachedir),
        // Fall back to the default cache location if config can't be loaded.
        Err(_) => sys::effective_cachedir("/var/cache/rum"),
    };

    match what {
        "all" => {
            remove(&cachedir)?;
            println!("Removed all cached data under {}", cachedir.display());
        }
        "packages" => {
            let pkgs = cachedir.join("packages");
            remove(&pkgs)?;
            println!("Removed cached packages under {}", pkgs.display());
        }
        "metadata" | "expire-cache" => {
            // Remove everything except the downloaded packages.
            let mut removed = 0;
            if let Ok(entries) = std::fs::read_dir(&cachedir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.file_name().is_some_and(|n| n == "packages") {
                        continue;
                    }
                    remove(&path)?;
                    removed += 1;
                }
            }
            println!("Removed cached metadata for {removed} repo(s)");
        }
        other => anyhow::bail!("unknown clean type `{other}` (expected: all, metadata, packages)"),
    }
    Ok(())
}

fn remove(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    result.map_err(|e| anyhow::anyhow!("cannot remove {}: {e}", path.display()))
}
