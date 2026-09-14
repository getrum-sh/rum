//! Raw FFI declarations for the subset of librpm we use for read-only rpmdb
//! access. Hand-written (rather than bindgen) because the surface is tiny and
//! stable across the rpm 4.x we target (AL2 through AL2023 / RHEL9).
#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int, c_uint, c_void};

// Opaque handle types. We only ever hold pointers to these.
pub enum rpmts_s {}
pub type rpmts = *mut rpmts_s;

pub enum rpmdbMatchIterator_s {}
pub type rpmdbMatchIterator = *mut rpmdbMatchIterator_s;

pub enum headerToken_s {}
pub type Header = *mut headerToken_s;

// Tag-data container, used to read (possibly array-valued) header tags.
pub enum rpmtd_s {}
pub type rpmtd = *mut rpmtd_s;

// rpmTagVal / rpmDbiTagVal are int32_t in rpm.
pub type rpmTagVal = c_int;

// Header tags we read (from rpmtag.h).
pub const RPMTAG_NAME: rpmTagVal = 1000;
pub const RPMTAG_VERSION: rpmTagVal = 1001;
pub const RPMTAG_RELEASE: rpmTagVal = 1002;
pub const RPMTAG_EPOCH: rpmTagVal = 1003;
pub const RPMTAG_SUMMARY: rpmTagVal = 1004;
pub const RPMTAG_SIZE: rpmTagVal = 1009;
pub const RPMTAG_ARCH: rpmTagVal = 1022;
pub const RPMTAG_INSTALLTIME: rpmTagVal = 1008;
pub const RPMTAG_PROVIDENAME: rpmTagVal = 1047;
pub const RPMTAG_PROVIDEVERSION: rpmTagVal = 1113;
pub const RPMTAG_REQUIREFLAGS: rpmTagVal = 1048;
pub const RPMTAG_REQUIRENAME: rpmTagVal = 1049;
pub const RPMTAG_REQUIREVERSION: rpmTagVal = 1050;

/// RPMSENSE version-comparison bits (rpmds.h). The exact `=` operator has
/// EQUAL set and neither LESS nor GREATER; the mask isolates those three.
pub const RPMSENSE_SENSE_MASK: u64 = 0x0e; // LESS(2)|GREATER(4)|EQUAL(8)
pub const RPMSENSE_EQUAL: u64 = 0x08;
pub const RPMTAG_BASENAMES: rpmTagVal = 1117;
pub const RPMTAG_DIRINDEXES: rpmTagVal = 1116;
pub const RPMTAG_DIRNAMES: rpmTagVal = 1118;

// rpmDbiTag values (from rpmdb.h). RPMDBI_PACKAGES iterates every installed
// header; a plain RPMTAG_* value keys the iterator by that tag.
pub const RPMDBI_PACKAGES: rpmTagVal = 0;

// --- Transaction (write) API ------------------------------------------------

// Opaque rpmio file descriptor and problem-set handles.
pub enum _FD_s {}
pub type FD_t = *mut _FD_s;
pub enum rpmProblem_s {}
pub type rpmProblem = *mut rpmProblem_s;
pub enum rpmps_s {}
pub type rpmps = *mut rpmps_s;
pub enum rpmpsi_s {}
pub type rpmpsi = *mut rpmpsi_s;

// rpmReadPackageFile result (rpmtypes.h rpmRC_e).
pub type rpmRC = c_int;
pub const RPMRC_OK: rpmRC = 0;
pub const RPMRC_NOTTRUSTED: rpmRC = 3;
pub const RPMRC_NOKEY: rpmRC = 4;

// rpmtransFlags (rpmts.h). TEST runs check+order+run without touching the db.
pub type rpmtransFlags = c_int;
pub const RPMTRANS_FLAG_NONE: rpmtransFlags = 0;
pub const RPMTRANS_FLAG_TEST: rpmtransFlags = 1 << 0;
#[allow(dead_code)]
pub const RPMTRANS_FLAG_NOSCRIPTS: rpmtransFlags = 1 << 2;
pub const RPMTRANS_FLAG_NODOCS: rpmtransFlags = 1 << 5;

// rpmprobFilterFlags (rpmps.h): which problems to ignore in rpmtsRun.
pub type rpmprobFilterFlags = c_int;
pub const RPMPROB_FILTER_NONE: rpmprobFilterFlags = 0;

// rpmCallbackType bits (rpmcallback.h) we handle: hand rpm the package FD
// during unpacking.
pub const RPMCALLBACK_INST_OPEN_FILE: c_uint = 1 << 2;
pub const RPMCALLBACK_INST_CLOSE_FILE: c_uint = 1 << 3;

/// The install/erase progress callback ABI (rpmCallbackFunction). On
/// INST_OPEN_FILE it must return the opened package FD for `key`.
pub type rpmCallbackFunction = extern "C" fn(
    h: *const c_void,
    what: c_uint,
    amount: u64,
    total: u64,
    key: *const c_void,
    data: *mut c_void,
) -> *mut c_void;

extern "C" {
    /// Read rpm config files / macros. Pass NULL/NULL for defaults. 0 = ok.
    pub fn rpmReadConfigFiles(file: *const c_char, target: *const c_char) -> c_int;

    pub fn rpmtsCreate() -> rpmts;
    pub fn rpmtsFree(ts: rpmts) -> rpmts;
    pub fn rpmtsSetRootDir(ts: rpmts, root_dir: *const c_char) -> c_int;
    /// Open the rpmdb with an explicit mode (O_RDONLY = 0). Opening read-only
    /// means rum never takes a write lock and won't block/fail while dnf runs.
    pub fn rpmtsOpenDB(ts: rpmts, dbmode: c_int) -> c_int;

    /// Create a database iterator. `keyp`/`keylen` may be NULL/0 to match all.
    /// For string keys, keylen 0 means "use strlen".
    pub fn rpmtsInitIterator(
        ts: rpmts,
        rpmtag: rpmTagVal,
        keyp: *const c_void,
        keylen: usize,
    ) -> rpmdbMatchIterator;

    /// Advance the iterator. Returns NULL when exhausted. The returned Header
    /// is owned by the iterator and valid only until the next call.
    pub fn rpmdbNextIterator(mi: rpmdbMatchIterator) -> Header;

    pub fn rpmdbFreeIterator(mi: rpmdbMatchIterator) -> rpmdbMatchIterator;

    /// Number of headers the iterator will yield (for pre-sizing).
    pub fn rpmdbGetIteratorCount(mi: rpmdbMatchIterator) -> c_int;

    /// Whether a tag is present in the header (to distinguish "epoch 0" from
    /// "no epoch", which matters for EVR comparison).
    pub fn headerIsEntry(h: Header, tag: rpmTagVal) -> c_int;

    /// Borrow a string tag. Pointer is valid while the header is alive.
    pub fn headerGetString(h: Header, tag: rpmTagVal) -> *const c_char;

    /// Read a numeric tag. Returns 0 if absent.
    pub fn headerGetNumber(h: Header, tag: rpmTagVal) -> u64;

    // Tag-data container API, for reading array-valued tags (e.g. Provides).
    pub fn rpmtdNew() -> rpmtd;
    pub fn rpmtdFree(td: rpmtd) -> rpmtd;
    pub fn rpmtdFreeData(td: rpmtd);
    /// Fill `td` with the value of `tag`. `flags` 0 = default. Nonzero = ok.
    pub fn headerGet(h: Header, tag: rpmTagVal, td: rpmtd, flags: c_int) -> c_int;
    pub fn rpmtdInit(td: rpmtd) -> c_int;
    /// Advance; returns the current index (>= 0), or -1 when exhausted.
    pub fn rpmtdNext(td: rpmtd) -> c_int;
    pub fn rpmtdSetIndex(td: rpmtd, index: c_int) -> c_int;
    /// Current string element (NULL if not a string / out of range).
    pub fn rpmtdGetString(td: rpmtd) -> *const c_char;
    /// Current numeric element (0 if not numeric / out of range).
    pub fn rpmtdGetNumber(td: rpmtd) -> u64;

    // --- Transaction API (librpm) + file I/O (librpmio) ---------------------

    /// Open a file via rpmio (mode e.g. "r.ufdio"). Returns an FD_t handle.
    pub fn Fopen(path: *const c_char, fmode: *const c_char) -> FD_t;
    pub fn Fclose(fd: FD_t) -> c_int;

    /// Read + verify a package file's header. `rc` is an rpmRC; on OK (and the
    /// lenient NOTTRUSTED/NOKEY) `*hdrp` is set to an owned Header.
    pub fn rpmReadPackageFile(ts: rpmts, fd: FD_t, fn_: *const c_char, hdrp: *mut Header) -> rpmRC;

    /// Add an install (upgrade=1 => replace older) element to the transaction.
    /// `key` is handed back to the notify callback to locate the package file.
    pub fn rpmtsAddInstallElement(
        ts: rpmts,
        h: Header,
        key: *const c_void,
        upgrade: c_int,
        relocs: *mut c_void,
    ) -> c_int;

    /// Add an erase element for an installed header (dboffset unused; pass -1).
    pub fn rpmtsAddEraseElement(ts: rpmts, h: Header, dboffset: c_int) -> c_int;

    /// Dependency check; 0 = no problems.
    pub fn rpmtsCheck(ts: rpmts) -> c_int;
    /// Order elements for install/erase; 0 = fully ordered.
    pub fn rpmtsOrder(ts: rpmts) -> c_int;
    /// Run the transaction. Returns 0 on success, >0 = number of problems,
    /// <0 = error.
    pub fn rpmtsRun(ts: rpmts, okProbs: rpmps, ignoreSet: rpmprobFilterFlags) -> c_int;

    pub fn rpmtsSetFlags(ts: rpmts, flags: rpmtransFlags) -> rpmtransFlags;
    pub fn rpmtsSetNotifyCallback(
        ts: rpmts,
        notify: rpmCallbackFunction,
        notifyData: *mut c_void,
    ) -> c_int;

    /// Free a header reference obtained from rpmReadPackageFile.
    pub fn headerFree(h: Header) -> Header;

    // Problem-set inspection (to report dependency/conflict failures).
    pub fn rpmtsProblems(ts: rpmts) -> rpmps;
    pub fn rpmpsInitIterator(ps: rpmps) -> rpmpsi;
    pub fn rpmpsiNext(psi: rpmpsi) -> c_int;
    pub fn rpmpsGetProblem(psi: rpmpsi) -> rpmProblem;
    /// Human-readable problem description (malloc'd; we copy then leak — the
    /// process exits shortly after).
    pub fn rpmProblemString(prob: rpmProblem) -> *mut c_char;
    pub fn rpmpsFreeIterator(psi: rpmpsi) -> rpmpsi;
    pub fn rpmpsFree(ps: rpmps) -> rpmps;
}
