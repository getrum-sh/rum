//! Shared helpers for reasoning about installed vs available versions.

use std::collections::HashMap;

use rum_repo::RepoMetadata;
use rum_rpm::{Package, Rpmdb};
use rum_solve::Evr;

/// A repository package reduced to just the fields list/check-update display.
/// Built from the zero-copy archived cache, so only the winners' display
/// strings are cloned (never the full dependency data).
pub struct AvailRow {
    pub name: String,
    pub arch: String,
    pub epoch: u64,
    pub version: String,
    pub release: String,
    pub repo_id: String,
}

impl AvailRow {
    pub fn name_arch(&self) -> String {
        format!("{}.{}", self.name, self.arch)
    }
    pub fn evr(&self) -> String {
        if self.epoch == 0 {
            format!("{}-{}", self.version, self.release)
        } else {
            format!("{}:{}-{}", self.epoch, self.version, self.release)
        }
    }
    pub fn evr_cmp(&self) -> Evr {
        Evr::new(Some(self.epoch), self.version.clone(), self.release.clone())
    }
}

/// EVR of an installed package.
pub fn installed_evr(p: &Package) -> Evr {
    Evr::new(p.epoch, p.version.clone(), p.release.clone())
}

/// Highest installed EVR per `name.arch` (packages can have several installed
/// versions, e.g. kernels; we keep the newest).
pub fn installed_best() -> HashMap<String, Evr> {
    let mut map: HashMap<String, Evr> = HashMap::new();
    if let Ok(db) = Rpmdb::open() {
        for p in db.installed() {
            let evr = installed_evr(&p);
            map.entry(p.name_arch())
                .and_modify(|cur| {
                    if evr > *cur {
                        *cur = evr.clone();
                    }
                })
                .or_insert(evr);
        }
    }
    map
}

/// Highest available package per `name.arch` across all synced repos, read
/// zero-copy from the archived metadata.
pub fn available_best(metas: &[RepoMetadata]) -> HashMap<String, AvailRow> {
    let mut map: HashMap<String, AvailRow> = HashMap::new();
    for m in metas {
        for p in m.views() {
            let key = p.name_arch();
            let evr = p.evr_cmp();
            match map.get(&key) {
                Some(existing) if existing.evr_cmp() >= evr => {}
                _ => {
                    map.insert(
                        key,
                        AvailRow {
                            name: p.name().to_string(),
                            arch: p.arch().to_string(),
                            epoch: p.epoch(),
                            version: p.version().to_string(),
                            release: p.release().to_string(),
                            repo_id: p.repo_id().to_string(),
                        },
                    );
                }
            }
        }
    }
    map
}
