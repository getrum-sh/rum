//! Version and dependency logic for rum.
//!
//! Milestone 1 (this file): RPM version comparison (`rpmvercmp`) and full
//! epoch:version-release (EVR) comparison, plus a helper to pick the newest
//! candidate. These back `check-update`, latest-only listing, and upgrade
//! decisions.
//!
//! SAT dependency resolution over repo `provides`/`requires` is provided by
//! [`resolve_sat`] (backed by the `resolvo` crate); [`resolve`] is the simpler
//! greedy resolver kept as a fallback and for reference.

mod dep;
mod resolve;
mod richdep;
mod sat;
#[cfg(test)]
mod testsupport;
mod vercmp;

use std::cmp::Ordering;

pub use dep::{ArchivedDepFlag, Dep, DepFlag};
pub use resolve::{resolve, Candidate, PresentPackage, ResolveError, Resolved};
pub use richdep::{is_rich, parse_rich, RichExpr};
pub use sat::{
    collect_rich_names, is_ignorable_dep, resolve_sat, resolve_sat_scoped, resolve_sat_scoped_opt,
    resolve_sat_with, resolve_sat_with_opt, CandidateRef, CandidateSource, NameView,
};
pub use vercmp::rpmvercmp;

/// An epoch:version-release tuple, RPM's unit of "which build is newer".
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct Evr {
    /// The epoch, or `None` when absent. RPM distinguishes an absent epoch from
    /// an explicit `0`: for *label* comparison ("which is newer") both count as
    /// 0, but for *dependency overlap* the epoch is compared only when BOTH
    /// sides carry one (see `dep::compare_partial`). `Some(0)` therefore differs
    /// from `None` there, so the two are kept distinct.
    pub epoch: Option<u64>,
    pub version: String,
    pub release: String,
}

impl Evr {
    pub fn new(epoch: Option<u64>, version: impl Into<String>, release: impl Into<String>) -> Self {
        Evr {
            epoch,
            version: version.into(),
            release: release.into(),
        }
    }

    /// Parse an EVR string of the form `[epoch:]version[-release]`, as stored
    /// in the rpmdb / repo metadata dependency versions. A missing epoch is
    /// `None` (distinct from an explicit `0:`); a missing release is empty
    /// (which dep comparison then ignores).
    pub fn parse(s: &str) -> Self {
        let (epoch, rest) = match s.split_once(':') {
            Some((e, r)) => (Some(e.parse::<u64>().unwrap_or(0)), r),
            None => (None, s),
        };
        let (version, release) = match rest.split_once('-') {
            Some((v, r)) => (v.to_string(), r.to_string()),
            None => (rest.to_string(), String::new()),
        };
        Evr {
            epoch,
            version,
            release,
        }
    }

    /// Lossless EVR string for round-tripping through the interned cache:
    /// unlike [`Display`](std::fmt::Display), it emits `0:` for an explicit
    /// `Some(0)` so the absent-vs-`0` distinction survives parse.
    pub fn to_dep_string(&self) -> String {
        match self.epoch {
            Some(e) => format!("{}:{}-{}", e, self.version, self.release),
            None => format!("{}-{}", self.version, self.release),
        }
    }

    /// RPM label comparison: epoch first (numerically, absent == 0), then
    /// version, then release, each with `rpmvercmp`.
    pub fn compare(&self, other: &Evr) -> Ordering {
        self.epoch
            .unwrap_or(0)
            .cmp(&other.epoch.unwrap_or(0))
            .then_with(|| rpmvercmp(&self.version, &other.version))
            .then_with(|| rpmvercmp(&self.release, &other.release))
    }
}

impl PartialOrd for Evr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Evr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.compare(other)
    }
}

impl std::fmt::Display for Evr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // User-facing: omit the epoch when absent or 0 (RPM/dnf convention).
        match self.epoch {
            Some(e) if e != 0 => write!(f, "{}:{}-{}", e, self.version, self.release),
            _ => write!(f, "{}-{}", self.version, self.release),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_dominates_version() {
        let a = Evr::new(Some(1), "1.0", "1");
        let b = Evr::new(Some(0), "9.9", "9");
        assert!(a > b, "higher epoch wins regardless of version");
    }

    #[test]
    fn missing_epoch_compares_as_zero_but_stays_distinct() {
        // For LABEL comparison an absent epoch counts as 0 (RPM/dnf convention)...
        let a = Evr::new(None, "1.0", "1");
        let b = Evr::new(Some(0), "1.0", "1");
        assert_eq!(a.cmp(&b), Ordering::Equal, "absent epoch == 0 for ordering");
        // ...but structurally it is kept distinct, because dependency overlap
        // treats an absent epoch as a wildcard (see dep::compare_partial).
        assert_ne!(a, b, "None and Some(0) must not be structurally equal");
    }

    #[test]
    fn version_then_release() {
        let a = Evr::new(Some(0), "1.2.3", "2.amzn2023");
        let b = Evr::new(Some(0), "1.2.3", "10.amzn2023");
        assert!(a < b, "release 10 newer than release 2");

        let c = Evr::new(Some(0), "1.2.4", "1");
        assert!(c > a, "newer version beats older even with lower release");
    }

    #[test]
    fn tilde_prerelease_is_older() {
        let pre = Evr::new(Some(0), "1.0~rc1", "1");
        let rel = Evr::new(Some(0), "1.0", "1");
        assert!(pre < rel);
    }

    #[test]
    fn parse_evr_forms() {
        // No `:` -> epoch is None (absent), distinct from an explicit `0:`.
        assert_eq!(Evr::parse("2.34-1"), Evr::new(None, "2.34", "1"));
        assert_eq!(
            Evr::parse("1:2.34-5.amzn2023"),
            Evr::new(Some(1), "2.34", "5.amzn2023")
        );
        assert_eq!(Evr::parse("0:2.34-1"), Evr::new(Some(0), "2.34", "1"));
        assert_eq!(Evr::parse("2.34"), Evr::new(None, "2.34", ""));
        assert_eq!(Evr::parse(""), Evr::new(None, "", ""));
        // Round-trip through the lossless interning string preserves presence.
        assert_eq!(
            Evr::parse(&Evr::new(Some(0), "1", "1").to_dep_string()).epoch,
            Some(0)
        );
        assert_eq!(
            Evr::parse(&Evr::new(None, "1", "1").to_dep_string()).epoch,
            None
        );
    }
}
