//! Command implementations.

pub mod check_update;
pub mod clean;
pub mod download;
pub mod groups;
pub mod info;
pub mod install;
pub mod list;
pub mod makecache;
pub mod pkgindex;
pub mod remove;
pub mod repo_sync;
pub mod repolist;
pub mod scope;
pub mod search;
pub mod upgrade;

use crate::Command;

/// Prompt for confirmation (dnf-style `Is this ok [y/N]:`). Default No.
pub fn confirm() -> bool {
    use std::io::Write;
    print!("Is this ok [y/N]: ");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

/// Uniform "recognized but not implemented yet" response for planned verbs.
///
/// We deliberately exit non-zero so scripts don't mistake a stub for success.
pub fn not_yet(cmd: &Command) -> anyhow::Result<()> {
    let verb = cmd.verb();
    anyhow::bail!(
        "`rum {verb}` is a recognized command but is not implemented yet in this build.\n\
         Implemented so far: repolist, makecache, list, info, search, check-update.\n\
         Planned next: provides, download, then install/upgrade (SAT solver)."
    );
}
