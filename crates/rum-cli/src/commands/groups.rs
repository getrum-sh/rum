//! Package-group / environment (`comps.xml`) expansion for `@group` install
//! targets and the `group` subcommand.
//!
//! Groups expand to a flat set of package *names* which are fed to the normal
//! resolver as explicit install targets — rum-solve/rum-rpm never see a group.

use std::collections::{BTreeMap, BTreeSet};

use rum_repo::{
    ArchivedCompsGroup, ArchivedCompsStore, ArchivedPkgReqType, CompsHandle, RepoMetadata,
};
use rum_rpm::Rpmdb;

/// The groups/environments across all synced repos (each a lazily-mmap'd cache).
pub struct Comps {
    handles: Vec<CompsHandle>,
}

impl Comps {
    pub fn load(metas: &[RepoMetadata]) -> Self {
        Comps {
            handles: metas.iter().filter_map(|m| m.load_comps()).collect(),
        }
    }

    /// `(id, name)` for every group across repos, deduped by id, sorted.
    pub fn list(&self) -> Vec<(String, String)> {
        let mut map: BTreeMap<String, String> = BTreeMap::new();
        for h in &self.handles {
            for (id, name) in h.store().group_listing() {
                map.entry(id.to_string())
                    .or_insert_with(|| name.to_string());
            }
        }
        map.into_iter().collect()
    }

    /// Expand a target (group/environment id or name, `@` optional) into package
    /// names. `None` if no group/environment matches. Mandatory + default
    /// packages are included (dnf's default); optional are excluded; conditional
    /// only when their trigger is installed.
    pub fn expand(&self, target: &str, db: Option<&Rpmdb>) -> Option<Vec<String>> {
        let clean = target.trim_start_matches('@');
        let mut out: BTreeSet<String> = BTreeSet::new();

        // Environments first (a name could in theory collide; envs are rarer).
        for h in &self.handles {
            let store = h.store();
            if let Some(env) = store.find_environment(clean) {
                for gid in env.groups.iter() {
                    let id = store.sym(gid.to_native()).to_string();
                    self.collect_group_by_id(&id, db, &mut out);
                }
                return Some(out.into_iter().collect());
            }
        }

        if self.collect_group(clean, db, &mut out) {
            Some(out.into_iter().collect())
        } else {
            None
        }
    }

    /// Collect a group matched by id or name in any repo. Returns whether any
    /// matched.
    fn collect_group(&self, target: &str, db: Option<&Rpmdb>, out: &mut BTreeSet<String>) -> bool {
        let mut found = false;
        for h in &self.handles {
            let store = h.store();
            if let Some(g) = store.find_group(target) {
                collect(store, g, db, out);
                found = true;
            }
        }
        found
    }

    fn collect_group_by_id(&self, id: &str, db: Option<&Rpmdb>, out: &mut BTreeSet<String>) {
        for h in &self.handles {
            let store = h.store();
            if let Some(g) = store.group_by_id(id) {
                collect(store, g, db, out);
            }
        }
    }
}

fn collect(
    store: &ArchivedCompsStore,
    group: &ArchivedCompsGroup,
    db: Option<&Rpmdb>,
    out: &mut BTreeSet<String>,
) {
    for p in group.packages.iter() {
        let include = match p.req {
            ArchivedPkgReqType::Mandatory | ArchivedPkgReqType::Default => true,
            ArchivedPkgReqType::Optional => false,
            ArchivedPkgReqType::Conditional => p.condition.as_ref().is_some_and(|c| {
                let trigger = store.sym(c.to_native());
                db.is_some_and(|d| d.is_installed(trigger))
            }),
        };
        if include {
            out.insert(store.sym(p.name.to_native()).to_string());
        }
    }
}

/// `rum group list` — list available groups (id and name).
pub fn run_list() -> anyhow::Result<()> {
    let synced = super::repo_sync::sync_enabled(false)?;
    let comps = Comps::load(synced.metas());
    let listing = comps.list();
    if listing.is_empty() {
        println!("No groups available (no repo provides comps metadata).");
        return Ok(());
    }
    let w = listing
        .iter()
        .map(|(id, _)| id.len())
        .max()
        .unwrap_or(10)
        .max(10);
    println!("Available Groups:");
    for (id, name) in &listing {
        println!("  {id:<w$}  {name}");
    }
    Ok(())
}
