//! `rum info <packages...>` — show details for installed packages.
//!
//! v0.1 reports installed packages only. Once repo metadata is available this
//! will also describe available (not-yet-installed) packages.

use crate::glob;
use rum_rpm::Rpmdb;

pub fn run(packages: &[String]) -> anyhow::Result<()> {
    if packages.is_empty() {
        anyhow::bail!("`rum info` needs at least one package name");
    }

    let db = Rpmdb::open().map_err(|e| anyhow::anyhow!("cannot open rpmdb: {e}"))?;
    let installed = db.installed();

    let mut matched = Vec::new();
    for pat in packages {
        for p in &installed {
            if (glob::matches(pat, &p.name) || glob::matches(pat, &p.name_arch()))
                && !matched
                    .iter()
                    .any(|m: &&rum_rpm::Package| m.nevra() == p.nevra())
            {
                matched.push(p);
            }
        }
    }

    if matched.is_empty() {
        anyhow::bail!(
            "No matching installed packages for: {}",
            packages.join(", ")
        );
    }

    matched.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.arch.cmp(&b.arch)));

    println!("Installed Packages");
    for p in matched {
        let epoch = p
            .epoch
            .map(|e| e.to_string())
            .unwrap_or_else(|| "(none)".into());
        println!("Name        : {}", p.name);
        println!("Epoch       : {epoch}");
        println!("Version     : {}", p.version);
        println!("Release     : {}", p.release);
        println!("Architecture: {}", p.arch);
        println!("Size        : {}", human_size(p.size));
        println!("Repository   : @System");
        println!("Summary     : {}", p.summary);
        println!();
    }
    Ok(())
}

fn human_size(bytes: u64) -> String {
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
