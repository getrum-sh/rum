//! `rum upgrade [-y] [packages...]`
//!
//! Upgrades all installed packages with newer versions available in enabled repos,
//! or only the specified packages / globs.

use super::{download, install};

pub fn run(packages: &[String], assume_yes: bool, nodocs: bool) -> anyhow::Result<()> {
    let resolution = download::resolve_upgrade(packages)?;
    if resolution.is_empty() {
        if packages.is_empty() {
            println!("Dependencies resolved.");
            println!("Nothing to do.");
        }
        return Ok(());
    }

    install::execute_transaction(&resolution, assume_yes, true, nodocs)
}
