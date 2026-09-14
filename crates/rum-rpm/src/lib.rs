//! Read-only access to the installed-package database (`/var/lib/rpm`) via the
//! system librpm.
//!
//! This is deliberately read-only: rum never keeps its own notion of "what is
//! installed", it always asks the rpmdb. That is what lets rum coexist with
//! dnf/yum on the same host, both see the same installed set, because there is
//! only one database and librpm abstracts over its backend (sqlite on
//! AL2023/RHEL9, BerkeleyDB on AL2, ndb on SUSE).
//!
//! Writes (installing/erasing) are a separate, later concern and will use the
//! librpm transaction API; they are intentionally not exposed here.

#[cfg(target_os = "linux")]
mod ffi;

#[derive(Debug, thiserror::Error)]
pub enum RpmError {
    #[error("failed to initialize rpm configuration (rpmReadConfigFiles)")]
    ConfigInit,
    #[error("failed to create rpm transaction set")]
    TsCreate,
    #[error("librpm is not available on this platform (built without Linux librpm)")]
    Unsupported,
    #[error("cannot read package {path}: {reason}")]
    PackageRead { path: String, reason: String },
    #[error("nothing to remove: {0} is not installed")]
    NotInstalled(String),
    #[error("rpm transaction failed:\n{0}")]
    TransactionFailed(String),
}

/// One installed package, as read from the rpmdb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    /// Epoch is `None` when the header has no epoch tag (distinct from `Some(0)`).
    pub epoch: Option<u64>,
    pub version: String,
    pub release: String,
    pub arch: String,
    pub summary: String,
    /// Installed size in bytes (RPMTAG_SIZE).
    pub size: u64,
    /// Unix time the package was installed (RPMTAG_INSTALLTIME).
    pub install_time: u64,
}

impl Package {
    /// `epoch:version-release` (epoch omitted when absent), the EVR string.
    pub fn evr(&self) -> String {
        match self.epoch {
            Some(e) => format!("{e}:{}-{}", self.version, self.release),
            None => format!("{}-{}", self.version, self.release),
        }
    }

    /// `name-[epoch:]version-release.arch` — the canonical NEVRA.
    pub fn nevra(&self) -> String {
        format!("{}-{}.{}", self.name, self.evr(), self.arch)
    }

    /// `name.arch` — how dnf labels a package in list output.
    pub fn name_arch(&self) -> String {
        format!("{}.{}", self.name, self.arch)
    }
}

/// A handle to the installed-package database.
pub struct Rpmdb {
    #[cfg(target_os = "linux")]
    ts: ffi::rpmts,
}

/// One installed package with the capability names it provides and requires —
/// enough to scope conflicts/obsoletes targets and their dependents for the
/// resolver (see rum-cli's scoping pass). Versions are the EVR string;
/// capability matching is by name (a safe superset for scoping).
pub struct InstalledPkg {
    pub name: String,
    /// `[epoch:]version-release`.
    pub evr: String,
    /// Provided capabilities (name and optional version, self-name included), excluding files.
    pub provides: Vec<(String, Option<String>)>,
    /// Required capability names (excluding rpmlib(...) and rich `(...)`).
    pub requires: Vec<String>,
}

/// Installed packages' reverse dependencies that a transaction can break:
/// exact `= version` couplings and rich `(A if B)` conditionals.
#[derive(Default)]
pub struct ReverseDeps {
    /// capability -> [(requirer_name, required_evr_string)] for exact `=`.
    pub exact: std::collections::HashMap<String, Vec<(String, String)>>,
    /// (requirer_name, rich-expression-string) for rich requires.
    pub rich: Vec<(String, String)>,
}

/// If `cap` ends in an rpm ISA-color suffix like `(x86-64)`, `(aarch-64)`, or
/// `(x86-32)` — i.e. `(<word>-<digits>)` — return the base capability with that
/// suffix removed; otherwise `None`. Used so an installed pin on the
/// arch-colored provide (`xz-libs(x86-64)`) is indexed under the bare package
/// name the lockstep looks up. A soname like `libc.so.6()(64bit)` does not
/// match (`64bit` has no hyphen) and is left alone.
// Only called from the Linux-only rpmdb scan; the unit test keeps it live.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn strip_isa_suffix(cap: &str) -> Option<&str> {
    let base = cap.strip_suffix(')')?;
    let open = base.rfind('(')?;
    let inner = &base[open + 1..];
    let (word, bits) = inner.split_once('-')?;
    if !word.is_empty()
        && word.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !bits.is_empty()
        && bits.bytes().all(|b| b.is_ascii_digit())
    {
        Some(&base[..open])
    } else {
        None
    }
}

/// A read-write librpm transaction: install and/or erase elements, then commit
/// natively via `rpmtsRun` (rpm does its own dependency check, ordering, and
/// scriptlet execution, writing the rpmdb). This is the native replacement for
/// shelling out to `rpm -U` / `rpm -e`.
pub struct Transaction {
    #[cfg(target_os = "linux")]
    ts: ffi::rpmts,
    /// Owned copies of the install-file paths; their pointers are handed to
    /// librpm as element keys and must outlive the transaction run.
    #[cfg(target_os = "linux")]
    keys: Vec<std::ffi::CString>,
    /// Number of elements added, so callers can detect an empty transaction.
    count: usize,
    /// Do not install documentation / manpages (RPMTRANS_FLAG_NODOCS).
    #[cfg(target_os = "linux")]
    nodocs: bool,
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{ffi, Package, RpmError, Rpmdb, Transaction};
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_uint, c_void};
    use std::path::Path;
    use std::ptr;
    use std::sync::Once;

    static INIT: Once = Once::new();
    static mut INIT_OK: bool = false;

    fn ensure_config() -> Result<(), RpmError> {
        // SAFETY: rpmReadConfigFiles mutates process-global rpm state; guard it
        // behind a Once so it runs exactly once per process.
        INIT.call_once(|| unsafe {
            let rc = ffi::rpmReadConfigFiles(ptr::null(), ptr::null());
            // Not using addr_of_mut here keeps MSRV low; access is serialized
            // by Once so there is no data race.
            #[allow(static_mut_refs)]
            {
                INIT_OK = rc == 0;
            }
        });
        // SAFETY: only read after call_once has completed initialization.
        #[allow(static_mut_refs)]
        let ok = unsafe { INIT_OK };
        if ok {
            Ok(())
        } else {
            Err(RpmError::ConfigInit)
        }
    }

    /// Read a borrowed string tag into an owned `String` (empty if absent).
    unsafe fn get_string(h: ffi::Header, tag: ffi::rpmTagVal) -> String {
        let p = ffi::headerGetString(h, tag);
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }

    unsafe fn header_to_package(h: ffi::Header) -> Package {
        let epoch = if ffi::headerIsEntry(h, ffi::RPMTAG_EPOCH) != 0 {
            Some(ffi::headerGetNumber(h, ffi::RPMTAG_EPOCH))
        } else {
            None
        };
        Package {
            name: get_string(h, ffi::RPMTAG_NAME),
            epoch,
            version: get_string(h, ffi::RPMTAG_VERSION),
            release: get_string(h, ffi::RPMTAG_RELEASE),
            arch: get_string(h, ffi::RPMTAG_ARCH),
            summary: get_string(h, ffi::RPMTAG_SUMMARY),
            size: ffi::headerGetNumber(h, ffi::RPMTAG_SIZE),
            install_time: ffi::headerGetNumber(h, ffi::RPMTAG_INSTALLTIME),
        }
    }

    impl Rpmdb {
        /// Open the system rpmdb at root `/`.
        pub fn open() -> Result<Self, RpmError> {
            ensure_config()?;
            // SAFETY: rpmtsCreate returns an owned transaction set or null.
            let ts = unsafe { ffi::rpmtsCreate() };
            if ts.is_null() {
                return Err(RpmError::TsCreate);
            }
            // SAFETY: ts is non-null; setting root "/" is the default but explicit.
            unsafe {
                let root = CString::new("/").unwrap();
                ffi::rpmtsSetRootDir(ts, root.as_ptr());
                // Open the rpmdb read-only (O_RDONLY = 0) so rum never takes a
                // write lock — reads won't block or fail while dnf/yum runs.
                ffi::rpmtsOpenDB(ts, 0);
            }
            Ok(Rpmdb { ts })
        }

        /// All installed packages.
        pub fn installed(&self) -> Vec<Package> {
            self.iter_with(ffi::RPMDBI_PACKAGES, None)
        }

        /// Installed packages with the exact given name (a name may match more
        /// than one row, e.g. multilib i686 + x86_64, or multiple kernels).
        pub fn by_name(&self, name: &str) -> Vec<Package> {
            self.iter_with(ffi::RPMTAG_NAME, Some(name))
        }

        /// Is any package with this exact name installed?
        pub fn is_installed(&self, name: &str) -> bool {
            !self.by_name(name).is_empty()
        }

        /// The version string of a Provides capability, e.g. the version of
        /// `system-release(releasever)`. This is how dnf derives `$releasever`
        /// (its `distroverpkg`), rather than trusting /etc/os-release.
        pub fn provide_version(&self, provide: &str) -> Option<String> {
            let key = CString::new(provide).ok()?;
            // SAFETY: valid ts; iterator keyed by PROVIDENAME returns headers of
            // packages carrying that Provides. We read the first match.
            unsafe {
                let mi = ffi::rpmtsInitIterator(
                    self.ts,
                    ffi::RPMTAG_PROVIDENAME,
                    key.as_ptr() as *const c_void,
                    0,
                );
                if mi.is_null() {
                    return None;
                }
                let h = ffi::rpmdbNextIterator(mi);
                let out = if h.is_null() {
                    None
                } else {
                    provide_version_from_header(h, provide)
                };
                ffi::rpmdbFreeIterator(mi);
                out
            }
        }

        /// dnf-compatible `$releasever` detection.
        pub fn releasever(&self) -> Option<String> {
            self.provide_version("system-release(releasever)")
        }

        /// Every Provides capability AND installed file path across all
        /// packages, as (name, optional version string). Used to prune
        /// dependencies already satisfied by the system (including file deps
        /// like `/bin/sh`).
        pub fn all_provides(&self) -> Vec<(String, Option<String>)> {
            let mut out = Vec::new();
            // SAFETY: valid ts; iterate every installed header and read its
            // provide/file tags, copying strings out immediately.
            unsafe {
                let mi = ffi::rpmtsInitIterator(self.ts, ffi::RPMDBI_PACKAGES, ptr::null(), 0);
                if mi.is_null() {
                    return out;
                }
                loop {
                    let h = ffi::rpmdbNextIterator(mi);
                    if h.is_null() {
                        break;
                    }
                    collect_capabilities(h, &mut out);
                    collect_files(h, &mut out);
                }
                ffi::rpmdbFreeIterator(mi);
            }
            out
        }

        /// Installed packages' reverse dependencies (exact `= version` couplings
        /// and rich `(A if B)` conditionals) that a transaction could break, so
        /// the affected siblings can be pulled in. Built by one full rpmdb scan
        /// (the RPMTAG_REQUIRENAME iterator index does not exist on the sqlite
        /// backend, so we can't query per-capability).
        pub fn installed_reverse_deps(&self) -> super::ReverseDeps {
            let mut rd = super::ReverseDeps::default();
            // SAFETY: valid ts; iterate every installed header and read its
            // exact-`=` and rich requires.
            unsafe {
                let mi = ffi::rpmtsInitIterator(self.ts, ffi::RPMDBI_PACKAGES, ptr::null(), 0);
                if mi.is_null() {
                    return rd;
                }
                loop {
                    let h = ffi::rpmdbNextIterator(mi);
                    if h.is_null() {
                        break;
                    }
                    let pkg = get_string(h, ffi::RPMTAG_NAME);
                    collect_reverse_requires(h, &pkg, &mut rd);
                }
                ffi::rpmdbFreeIterator(mi);
            }
            rd
        }

        /// Every installed package with its provided/required capability names
        /// (one rpmdb scan). Feeds rum-cli's scoped-present resolver pass.
        pub fn installed_packages_deps(&self) -> Vec<super::InstalledPkg> {
            let mut out = Vec::new();
            // SAFETY: valid ts; iterate every installed header, reading name/EVR
            // plus provide and require capability names.
            unsafe {
                let mi = ffi::rpmtsInitIterator(self.ts, ffi::RPMDBI_PACKAGES, ptr::null(), 0);
                if mi.is_null() {
                    return out;
                }
                loop {
                    let h = ffi::rpmdbNextIterator(mi);
                    if h.is_null() {
                        break;
                    }
                    let pkg = header_to_package(h);
                    let mut caps: Vec<(String, Option<String>)> = Vec::new();
                    collect_capabilities(h, &mut caps);
                    out.push(super::InstalledPkg {
                        name: pkg.name.clone(),
                        evr: pkg.evr(),
                        provides: caps,
                        requires: collect_require_names(h),
                    });
                }
                ffi::rpmdbFreeIterator(mi);
            }
            out
        }

        fn iter_with(&self, tag: ffi::rpmTagVal, key: Option<&str>) -> Vec<Package> {
            let ckey = key.map(|k| CString::new(k).unwrap());
            let (keyp, keylen) = match &ckey {
                Some(c) => (c.as_ptr() as *const c_void, 0usize),
                None => (ptr::null(), 0usize),
            };

            // SAFETY: self.ts is a valid transaction set for the lifetime of &self.
            let mi = unsafe { ffi::rpmtsInitIterator(self.ts, tag, keyp, keylen) };
            if mi.is_null() {
                return Vec::new();
            }

            let mut out = Vec::new();
            // SAFETY: mi is non-null; NextIterator yields headers owned by the
            // iterator, valid until the next call, so we copy immediately.
            unsafe {
                let hint = ffi::rpmdbGetIteratorCount(mi);
                if hint > 0 {
                    out.reserve(hint as usize);
                }
                loop {
                    let h = ffi::rpmdbNextIterator(mi);
                    if h.is_null() {
                        break;
                    }
                    out.push(header_to_package(h));
                }
                ffi::rpmdbFreeIterator(mi);
            }
            out
        }
    }

    /// Walk the parallel PROVIDENAME / PROVIDEVERSION arrays of a header and
    /// return the version paired with `target`.
    unsafe fn provide_version_from_header(h: ffi::Header, target: &str) -> Option<String> {
        let names = ffi::rpmtdNew();
        let vers = ffi::rpmtdNew();
        let mut out = None;

        if ffi::headerGet(h, ffi::RPMTAG_PROVIDENAME, names, 0) != 0
            && ffi::headerGet(h, ffi::RPMTAG_PROVIDEVERSION, vers, 0) != 0
        {
            ffi::rpmtdInit(names);
            loop {
                let idx = ffi::rpmtdNext(names);
                if idx < 0 {
                    break;
                }
                let np = ffi::rpmtdGetString(names);
                if np.is_null() {
                    continue;
                }
                let name = CStr::from_ptr(np).to_string_lossy();
                if name == target {
                    ffi::rpmtdSetIndex(vers, idx);
                    let vp = ffi::rpmtdGetString(vers);
                    if !vp.is_null() {
                        let v = CStr::from_ptr(vp).to_string_lossy().into_owned();
                        if !v.is_empty() {
                            out = Some(v);
                        }
                    }
                    break;
                }
            }
        }

        ffi::rpmtdFreeData(names);
        ffi::rpmtdFree(names);
        ffi::rpmtdFreeData(vers);
        ffi::rpmtdFree(vers);
        out
    }

    /// Record header `h`'s reverse deps: exact-`=` requires as `cap ->
    /// (pkg, version)`, and rich `(...)` requires as `(pkg, expr)`.
    /// Require capability names of header `h`, skipping `rpmlib(...)` and rich
    /// `(...)` boolean requires (which aren't plain capability names).
    unsafe fn collect_require_names(h: ffi::Header) -> Vec<String> {
        let mut out = Vec::new();
        let names = ffi::rpmtdNew();
        if ffi::headerGet(h, ffi::RPMTAG_REQUIRENAME, names, 0) != 0 {
            ffi::rpmtdInit(names);
            loop {
                if ffi::rpmtdNext(names) < 0 {
                    break;
                }
                let np = ffi::rpmtdGetString(names);
                if np.is_null() {
                    continue;
                }
                let n = CStr::from_ptr(np).to_string_lossy();
                if n.starts_with("rpmlib(") || n.starts_with('(') {
                    continue;
                }
                out.push(n.into_owned());
            }
        }
        ffi::rpmtdFreeData(names);
        ffi::rpmtdFree(names);
        out
    }

    unsafe fn collect_reverse_requires(h: ffi::Header, pkg: &str, rd: &mut super::ReverseDeps) {
        let names = ffi::rpmtdNew();
        let flags = ffi::rpmtdNew();
        let vers = ffi::rpmtdNew();
        let have_n = ffi::headerGet(h, ffi::RPMTAG_REQUIRENAME, names, 0) != 0;
        let have_f = ffi::headerGet(h, ffi::RPMTAG_REQUIREFLAGS, flags, 0) != 0;
        let have_v = ffi::headerGet(h, ffi::RPMTAG_REQUIREVERSION, vers, 0) != 0;
        if have_n && have_f && have_v {
            ffi::rpmtdInit(names);
            loop {
                let idx = ffi::rpmtdNext(names);
                if idx < 0 {
                    break;
                }
                ffi::rpmtdSetIndex(flags, idx);
                let fl = ffi::rpmtdGetNumber(flags);
                let np = ffi::rpmtdGetString(names);
                if np.is_null() {
                    continue;
                }
                let name = CStr::from_ptr(np).to_string_lossy().into_owned();
                // A rich/boolean dep is stored as its whole `(...)` expression in
                // REQUIRENAME; capability names never start with '(' (and the
                // RPMSENSE_RICH flag bit is unreliable across rpm versions), so
                // the leading paren is the robust discriminator.
                if name.starts_with('(') {
                    rd.rich.push((pkg.to_string(), name));
                } else if fl & ffi::RPMSENSE_SENSE_MASK == ffi::RPMSENSE_EQUAL {
                    ffi::rpmtdSetIndex(vers, idx);
                    let vp = ffi::rpmtdGetString(vers);
                    if !vp.is_null() {
                        let ver = CStr::from_ptr(vp).to_string_lossy().into_owned();
                        if !ver.is_empty() {
                            // rpm auto-adds an ISA-colored provide `name(x86-64)`
                            // and installed siblings often pin THAT form (e.g.
                            // `xz` needs `xz-libs(x86-64) = ...`). The lockstep
                            // keys on the bare package name of the upgraded
                            // winner, so index the require under its ISA-stripped
                            // base too.
                            if let Some(base) = super::strip_isa_suffix(&name) {
                                rd.exact
                                    .entry(base.to_string())
                                    .or_default()
                                    .push((pkg.to_string(), ver.clone()));
                            }
                            rd.exact
                                .entry(name)
                                .or_default()
                                .push((pkg.to_string(), ver));
                        }
                    }
                }
            }
        }
        ffi::rpmtdFreeData(names);
        ffi::rpmtdFree(names);
        ffi::rpmtdFreeData(flags);
        ffi::rpmtdFree(flags);
        ffi::rpmtdFreeData(vers);
        ffi::rpmtdFree(vers);
    }

    /// Collect a header's Provides capabilities as (name, optional version).
    unsafe fn collect_capabilities(h: ffi::Header, out: &mut Vec<(String, Option<String>)>) {
        let names = ffi::rpmtdNew();
        let vers = ffi::rpmtdNew();
        let have_v = ffi::headerGet(h, ffi::RPMTAG_PROVIDEVERSION, vers, 0) != 0;
        if ffi::headerGet(h, ffi::RPMTAG_PROVIDENAME, names, 0) != 0 {
            ffi::rpmtdInit(names);
            loop {
                let idx = ffi::rpmtdNext(names);
                if idx < 0 {
                    break;
                }
                let np = ffi::rpmtdGetString(names);
                if np.is_null() {
                    continue;
                }
                let name = CStr::from_ptr(np).to_string_lossy().into_owned();
                let ver = if have_v {
                    ffi::rpmtdSetIndex(vers, idx);
                    let vp = ffi::rpmtdGetString(vers);
                    if vp.is_null() {
                        None
                    } else {
                        let s = CStr::from_ptr(vp).to_string_lossy().into_owned();
                        if s.is_empty() {
                            None
                        } else {
                            Some(s)
                        }
                    }
                } else {
                    None
                };
                out.push((name, ver));
            }
        }
        ffi::rpmtdFreeData(names);
        ffi::rpmtdFree(names);
        ffi::rpmtdFreeData(vers);
        ffi::rpmtdFree(vers);
    }

    /// Collect a header's file paths (reconstructed from basenames + dir
    /// index + dirnames), pushed as unversioned provides.
    unsafe fn collect_files(h: ffi::Header, out: &mut Vec<(String, Option<String>)>) {
        let dirnames_td = ffi::rpmtdNew();
        let base_td = ffi::rpmtdNew();
        let didx_td = ffi::rpmtdNew();

        let mut dirs: Vec<(String, bool)> = Vec::new();
        if ffi::headerGet(h, ffi::RPMTAG_DIRNAMES, dirnames_td, 0) != 0 {
            ffi::rpmtdInit(dirnames_td);
            loop {
                if ffi::rpmtdNext(dirnames_td) < 0 {
                    break;
                }
                let p = ffi::rpmtdGetString(dirnames_td);
                if p.is_null() {
                    dirs.push((String::new(), false));
                } else {
                    let d = CStr::from_ptr(p).to_string_lossy().into_owned();
                    let wanted = d.starts_with("/bin")
                        || d.starts_with("/sbin")
                        || d.starts_with("/usr/bin")
                        || d.starts_with("/usr/sbin")
                        || d.starts_with("/etc");
                    dirs.push((d, wanted));
                }
            }
        }

        let have_idx = ffi::headerGet(h, ffi::RPMTAG_DIRINDEXES, didx_td, 0) != 0;
        if !dirs.is_empty() && have_idx && ffi::headerGet(h, ffi::RPMTAG_BASENAMES, base_td, 0) != 0
        {
            ffi::rpmtdInit(base_td);
            loop {
                let idx = ffi::rpmtdNext(base_td);
                if idx < 0 {
                    break;
                }
                ffi::rpmtdSetIndex(didx_td, idx);
                let di = ffi::rpmtdGetNumber(didx_td) as usize;
                let Some((dir, true)) = dirs.get(di) else {
                    continue;
                };
                let bp = ffi::rpmtdGetString(base_td);
                if bp.is_null() {
                    continue;
                }
                let base = CStr::from_ptr(bp).to_string_lossy();
                out.push((format!("{dir}{base}"), None));
            }
        }

        ffi::rpmtdFreeData(dirnames_td);
        ffi::rpmtdFree(dirnames_td);
        ffi::rpmtdFreeData(base_td);
        ffi::rpmtdFree(base_td);
        ffi::rpmtdFreeData(didx_td);
        ffi::rpmtdFree(didx_td);
    }

    impl Drop for Rpmdb {
        fn drop(&mut self) {
            // SAFETY: ts was created by rpmtsCreate and not freed elsewhere.
            unsafe {
                ffi::rpmtsFree(self.ts);
            }
        }
    }

    /// State threaded through the install callback: the FD of the package
    /// currently being unpacked (opened on INST_OPEN_FILE, closed on
    /// INST_CLOSE_FILE).
    struct CbState {
        cur_fd: ffi::FD_t,
    }

    /// librpm hands us `key` (the path pointer we set on each install element)
    /// when it needs the package's file descriptor during unpacking. We Fopen
    /// it and return the FD; on close we Fclose it. All other events are
    /// ignored (rpm still logs its own progress/scriptlet output to stderr).
    extern "C" fn notify_cb(
        _h: *const c_void,
        what: c_uint,
        _amount: u64,
        _total: u64,
        key: *const c_void,
        data: *mut c_void,
    ) -> *mut c_void {
        // SAFETY: `data` is the &mut CbState we passed to rpmtsSetNotifyCallback,
        // valid for the whole run; librpm calls back single-threaded.
        let state = unsafe { &mut *(data as *mut CbState) };
        match what {
            ffi::RPMCALLBACK_INST_OPEN_FILE => {
                if key.is_null() {
                    return ptr::null_mut();
                }
                let mode = c"r.ufdio";
                // SAFETY: key is the CString path pointer from add_install.
                let fd = unsafe { ffi::Fopen(key as *const _, mode.as_ptr()) };
                state.cur_fd = fd;
                fd as *mut c_void
            }
            ffi::RPMCALLBACK_INST_CLOSE_FILE => {
                if !state.cur_fd.is_null() {
                    // SAFETY: cur_fd was returned by Fopen above.
                    unsafe { ffi::Fclose(state.cur_fd) };
                    state.cur_fd = ptr::null_mut();
                }
                ptr::null_mut()
            }
            _ => ptr::null_mut(),
        }
    }

    impl Transaction {
        /// Create an empty read-write transaction rooted at `/`.
        pub fn new() -> Result<Self, RpmError> {
            ensure_config()?;
            // SAFETY: rpmtsCreate returns an owned transaction set or null.
            let ts = unsafe { ffi::rpmtsCreate() };
            if ts.is_null() {
                return Err(RpmError::TsCreate);
            }
            // SAFETY: ts is non-null. We do NOT open the db read-only here (as
            // Rpmdb does) so rpmtsRun can open it read-write to commit.
            unsafe {
                let root = CString::new("/").unwrap();
                ffi::rpmtsSetRootDir(ts, root.as_ptr());
            }
            Ok(Transaction {
                ts,
                keys: Vec::new(),
                count: 0,
                nodocs: false,
            })
        }

        /// Enable or disable documentation/manpage unpacking (RPMTRANS_FLAG_NODOCS).
        pub fn set_nodocs(&mut self, nodocs: bool) {
            self.nodocs = nodocs;
        }

        /// Queue an install of the RPM at `path`. `upgrade=true` (the normal
        /// case) makes rpm replace older builds of the same name; `false` adds
        /// it alongside them (install-only semantics, e.g. kernels).
        pub fn add_install(&mut self, path: &Path, upgrade: bool) -> Result<(), RpmError> {
            let path_str = path.to_string_lossy().into_owned();
            let cpath = CString::new(path_str.clone()).map_err(|_| RpmError::PackageRead {
                path: path_str.clone(),
                reason: "path contains NUL".into(),
            })?;
            let mode = c"r.ufdio";

            // SAFETY: valid ts; Fopen/rpmReadPackageFile/AddInstallElement per
            // the librpm install recipe. The header is freed after being added
            // (AddInstallElement takes its own reference).
            unsafe {
                let fd = ffi::Fopen(cpath.as_ptr(), mode.as_ptr());
                if fd.is_null() {
                    return Err(RpmError::PackageRead {
                        path: path_str,
                        reason: "cannot open file".into(),
                    });
                }
                let mut h: ffi::Header = ptr::null_mut();
                let rc = ffi::rpmReadPackageFile(self.ts, fd, cpath.as_ptr(), &mut h);
                ffi::Fclose(fd);
                match rc {
                    ffi::RPMRC_OK => {}
                    ffi::RPMRC_NOTTRUSTED | ffi::RPMRC_NOKEY => {
                        tracing::warn!(path = %path_str, "package signature not trusted / key missing");
                    }
                    _ => {
                        if !h.is_null() {
                            ffi::headerFree(h);
                        }
                        return Err(RpmError::PackageRead {
                            path: path_str,
                            reason: format!("rpmReadPackageFile failed (rc={rc})"),
                        });
                    }
                }
                if h.is_null() {
                    return Err(RpmError::PackageRead {
                        path: path_str,
                        reason: "no header".into(),
                    });
                }

                // Keep the path CString alive; its pointer is the element key
                // handed back to the notify callback.
                self.keys.push(cpath);
                let key = self.keys.last().unwrap().as_ptr() as *const c_void;
                let upg = if upgrade { 1 } else { 0 };
                let added = ffi::rpmtsAddInstallElement(self.ts, h, key, upg, ptr::null_mut());
                ffi::headerFree(h);
                if added != 0 {
                    self.keys.pop();
                    return Err(RpmError::PackageRead {
                        path: path_str,
                        reason: "rpmtsAddInstallElement failed".into(),
                    });
                }
            }
            self.count += 1;
            Ok(())
        }

        /// Queue erasure of every installed package with the exact name `name`.
        pub fn add_erase(&mut self, name: &str) -> Result<(), RpmError> {
            let key = CString::new(name).map_err(|_| RpmError::NotInstalled(name.into()))?;
            let mut any = false;
            // SAFETY: valid ts; iterate installed headers matching the name and
            // add an erase element for each (dboffset -1, unused by modern rpm).
            unsafe {
                let mi = ffi::rpmtsInitIterator(
                    self.ts,
                    ffi::RPMTAG_NAME,
                    key.as_ptr() as *const c_void,
                    0,
                );
                if !mi.is_null() {
                    loop {
                        let h = ffi::rpmdbNextIterator(mi);
                        if h.is_null() {
                            break;
                        }
                        if ffi::rpmtsAddEraseElement(self.ts, h, -1) == 0 {
                            any = true;
                            self.count += 1;
                        }
                    }
                    ffi::rpmdbFreeIterator(mi);
                }
            }
            if any {
                Ok(())
            } else {
                Err(RpmError::NotInstalled(name.into()))
            }
        }

        /// Queue erasure of only the installed build of `name` at exactly
        /// `version`-`release` (leaving other versions of the same name). Used
        /// to prune old install-only kernels without touching the rest.
        pub fn add_erase_exact(
            &mut self,
            name: &str,
            version: &str,
            release: &str,
        ) -> Result<(), RpmError> {
            let key = CString::new(name).map_err(|_| RpmError::NotInstalled(name.into()))?;
            let mut any = false;
            // SAFETY: valid ts; iterate installed headers of this name and erase
            // only the one whose version+release match.
            unsafe {
                let mi = ffi::rpmtsInitIterator(
                    self.ts,
                    ffi::RPMTAG_NAME,
                    key.as_ptr() as *const c_void,
                    0,
                );
                if !mi.is_null() {
                    loop {
                        let h = ffi::rpmdbNextIterator(mi);
                        if h.is_null() {
                            break;
                        }
                        if get_string(h, ffi::RPMTAG_VERSION) == version
                            && get_string(h, ffi::RPMTAG_RELEASE) == release
                            && ffi::rpmtsAddEraseElement(self.ts, h, -1) == 0
                        {
                            any = true;
                            self.count += 1;
                        }
                    }
                    ffi::rpmdbFreeIterator(mi);
                }
            }
            if any {
                Ok(())
            } else {
                Err(RpmError::NotInstalled(format!(
                    "{name}-{version}-{release}"
                )))
            }
        }

        pub fn is_empty(&self) -> bool {
            self.count == 0
        }

        /// Commit (or, with `test`, dry-run) the transaction: dependency check,
        /// order, then run. Returns an error carrying rpm's problem strings on
        /// failure.
        pub fn run(&mut self, test: bool) -> Result<(), RpmError> {
            let mut state = CbState {
                cur_fd: ptr::null_mut(),
            };
            // SAFETY: valid ts; standard check/order/run sequence. The callback
            // data pointer is valid for the duration of rpmtsRun.
            unsafe {
                let mut flags = if test {
                    ffi::RPMTRANS_FLAG_TEST
                } else {
                    ffi::RPMTRANS_FLAG_NONE
                };
                if self.nodocs {
                    flags |= ffi::RPMTRANS_FLAG_NODOCS;
                }
                ffi::rpmtsSetFlags(self.ts, flags);
                ffi::rpmtsSetNotifyCallback(
                    self.ts,
                    notify_cb,
                    &mut state as *mut CbState as *mut c_void,
                );

                if ffi::rpmtsCheck(self.ts) != 0 {
                    return Err(RpmError::TransactionFailed(self.problems()));
                }
                // rpmtsOrder returns the count of elements it could not order.
                if ffi::rpmtsOrder(self.ts) != 0 {
                    return Err(RpmError::TransactionFailed(
                        "could not order transaction (dependency loop?)".into(),
                    ));
                }
                let rc = ffi::rpmtsRun(self.ts, ptr::null_mut(), ffi::RPMPROB_FILTER_NONE);
                if rc != 0 {
                    let probs = self.problems();
                    let msg = if probs.is_empty() {
                        format!("rpmtsRun returned {rc}")
                    } else {
                        probs
                    };
                    return Err(RpmError::TransactionFailed(msg));
                }
            }
            Ok(())
        }

        /// Collect rpm's current problem set into a newline-joined string.
        fn problems(&self) -> String {
            let mut out = String::new();
            // SAFETY: valid ts; iterate the problem set, copying each string.
            unsafe {
                let ps = ffi::rpmtsProblems(self.ts);
                if ps.is_null() {
                    return out;
                }
                let psi = ffi::rpmpsInitIterator(ps);
                if !psi.is_null() {
                    while ffi::rpmpsiNext(psi) >= 0 {
                        let prob = ffi::rpmpsGetProblem(psi);
                        if prob.is_null() {
                            continue;
                        }
                        let s = ffi::rpmProblemString(prob);
                        if !s.is_null() {
                            let msg = CStr::from_ptr(s).to_string_lossy().into_owned();
                            if !out.is_empty() {
                                out.push('\n');
                            }
                            out.push_str("  ");
                            out.push_str(&msg);
                            // rpmProblemString malloc's; the process exits soon,
                            // so a small leak here is acceptable.
                        }
                    }
                    ffi::rpmpsFreeIterator(psi);
                }
                ffi::rpmpsFree(ps);
            }
            out
        }
    }

    impl Drop for Transaction {
        fn drop(&mut self) {
            // SAFETY: ts was created by rpmtsCreate and not freed elsewhere.
            unsafe {
                ffi::rpmtsFree(self.ts);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl Rpmdb {
    pub fn open() -> Result<Self, RpmError> {
        Err(RpmError::Unsupported)
    }
    pub fn installed(&self) -> Vec<Package> {
        Vec::new()
    }
    pub fn by_name(&self, _name: &str) -> Vec<Package> {
        Vec::new()
    }
    pub fn is_installed(&self, _name: &str) -> bool {
        false
    }
    pub fn provide_version(&self, _provide: &str) -> Option<String> {
        None
    }
    pub fn releasever(&self) -> Option<String> {
        None
    }
    pub fn all_provides(&self) -> Vec<(String, Option<String>)> {
        Vec::new()
    }
    pub fn installed_reverse_deps(&self) -> ReverseDeps {
        ReverseDeps::default()
    }
    pub fn installed_packages_deps(&self) -> Vec<InstalledPkg> {
        Vec::new()
    }
}

#[cfg(not(target_os = "linux"))]
impl Transaction {
    pub fn new() -> Result<Self, RpmError> {
        Err(RpmError::Unsupported)
    }
    pub fn add_install(&mut self, _path: &std::path::Path, _upgrade: bool) -> Result<(), RpmError> {
        Err(RpmError::Unsupported)
    }
    pub fn add_erase(&mut self, _name: &str) -> Result<(), RpmError> {
        Err(RpmError::Unsupported)
    }
    pub fn add_erase_exact(&mut self, _n: &str, _v: &str, _r: &str) -> Result<(), RpmError> {
        Err(RpmError::Unsupported)
    }
    pub fn set_nodocs(&mut self, _nodocs: bool) {}
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn run(&mut self, _test: bool) -> Result<(), RpmError> {
        Err(RpmError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evr_and_nevra_formatting() {
        let p = Package {
            name: "bash".into(),
            epoch: None,
            version: "5.2.15".into(),
            release: "1.amzn2023".into(),
            arch: "x86_64".into(),
            summary: "The GNU Bourne Again shell".into(),
            size: 0,
            install_time: 0,
        };
        assert_eq!(p.evr(), "5.2.15-1.amzn2023");
        assert_eq!(p.nevra(), "bash-5.2.15-1.amzn2023.x86_64");
        assert_eq!(p.name_arch(), "bash.x86_64");

        let e = Package {
            epoch: Some(2),
            ..p.clone()
        };
        assert_eq!(e.evr(), "2:5.2.15-1.amzn2023");
        assert_eq!(e.nevra(), "bash-2:5.2.15-1.amzn2023.x86_64");
    }

    #[test]
    fn isa_suffix_stripping() {
        assert_eq!(strip_isa_suffix("xz-libs(x86-64)"), Some("xz-libs"));
        assert_eq!(strip_isa_suffix("glibc(aarch-64)"), Some("glibc"));
        assert_eq!(strip_isa_suffix("glibc(x86-32)"), Some("glibc"));
        // Not an ISA color: leave sonames and plain names untouched.
        assert_eq!(strip_isa_suffix("libc.so.6()(64bit)"), None);
        assert_eq!(strip_isa_suffix("pkgconfig(foo)"), None);
        assert_eq!(strip_isa_suffix("xz-libs"), None);
    }
}
