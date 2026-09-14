//! RPM dependency capabilities: a `Dep` is a name plus an optional version
//! constraint, used for both `Provides` and `Requires`.

use std::cmp::Ordering;

use crate::Evr;

/// The comparison operator on a versioned dependency (RPM's sense flags).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub enum DepFlag {
    /// Unversioned: any version satisfies.
    Any,
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

impl DepFlag {
    /// Parse the `flags=` attribute value from primary.xml.
    pub fn parse(s: &str) -> Self {
        match s {
            "EQ" => DepFlag::Eq,
            "LT" => DepFlag::Lt,
            "LE" => DepFlag::Le,
            "GT" => DepFlag::Gt,
            "GE" => DepFlag::Ge,
            _ => DepFlag::Any,
        }
    }
}

/// A dependency capability (name [op evr]).
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct Dep {
    pub name: String,
    pub flag: DepFlag,
    /// Constraint version; `None` for unversioned deps.
    pub evr: Option<Evr>,
}

impl Dep {
    pub fn unversioned(name: impl Into<String>) -> Self {
        Dep {
            name: name.into(),
            flag: DepFlag::Any,
            evr: None,
        }
    }

    /// Does a Provides `(prov_name, prov_evr)` satisfy this requirement?
    ///
    /// Name must match exactly. For versioned requirements, the provided
    /// version must fall in the required range. An unversioned Provides
    /// (`prov_evr == None`) satisfies any requirement on that name (RPM treats
    /// a bare Provides as matching every version request), and an unversioned
    /// requirement is satisfied by any Provides of the name.
    pub fn satisfied_by(&self, prov_name: &str, prov_evr: Option<&Evr>) -> bool {
        if self.name != prov_name {
            return false;
        }
        let (Some(req), Some(prov)) = (&self.evr, prov_evr) else {
            // Either side unversioned: name match is enough.
            return true;
        };
        let ord = compare_partial(prov, req);
        match self.flag {
            DepFlag::Any => true,
            DepFlag::Eq => ord == Ordering::Equal,
            DepFlag::Lt => ord == Ordering::Less,
            DepFlag::Le => ord != Ordering::Greater,
            DepFlag::Gt => ord == Ordering::Greater,
            DepFlag::Ge => ord != Ordering::Less,
        }
    }
}

/// Compare two EVRs for dependency-range purposes, honouring RPM's rule that
/// fields absent from one side are not compared: if either release is empty,
/// releases are ignored; likewise a bare version (no release) compares only
/// epoch+version.
fn compare_partial(a: &Evr, b: &Evr) -> Ordering {
    // RPM's overlap rule compares epoch ONLY when both sides carry one
    // (`rpmver.c`: `if (v1->e && *v1->e && v2->e && *v2->e)`). An absent epoch
    // on either side is a wildcard, NOT 0 — so e.g. Provide `bash = 5.2` (no
    // epoch) satisfies Require `bash >= 2:5.0`.
    if let (Some(ae), Some(be)) = (a.epoch, b.epoch) {
        match ae.cmp(&be) {
            Ordering::Equal => {}
            ne => return ne,
        }
    }
    let ver = crate::rpmvercmp(&a.version, &b.version);
    if ver != Ordering::Equal {
        return ver;
    }
    if a.release.is_empty() || b.release.is_empty() {
        return Ordering::Equal;
    }
    crate::rpmvercmp(&a.release, &b.release)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evr(v: &str, r: &str) -> Evr {
        Evr::new(Some(0), v, r)
    }

    #[test]
    fn unversioned_require_matches_any_provide() {
        let req = Dep::unversioned("libc.so.6");
        assert!(req.satisfied_by("libc.so.6", None));
        assert!(req.satisfied_by("libc.so.6", Some(&evr("2.34", "1"))));
        assert!(!req.satisfied_by("libm.so.6", None));
    }

    #[test]
    fn versioned_ranges() {
        let ge = Dep {
            name: "glibc".into(),
            flag: DepFlag::Ge,
            evr: Some(evr("2.34", "")),
        };
        assert!(ge.satisfied_by("glibc", Some(&evr("2.34", "10"))));
        assert!(ge.satisfied_by("glibc", Some(&evr("2.40", "1"))));
        assert!(!ge.satisfied_by("glibc", Some(&evr("2.33", "9"))));

        let eq = Dep {
            name: "foo".into(),
            flag: DepFlag::Eq,
            evr: Some(evr("1.2", "")),
        };
        // Release ignored because the requirement gave none.
        assert!(eq.satisfied_by("foo", Some(&evr("1.2", "5.amzn2023"))));
        assert!(!eq.satisfied_by("foo", Some(&evr("1.3", "1"))));
    }

    #[test]
    fn bare_provide_satisfies_versioned_require() {
        let lt = Dep {
            name: "cap".into(),
            flag: DepFlag::Lt,
            evr: Some(evr("2.0", "1")),
        };
        assert!(lt.satisfied_by("cap", None));
    }
}
