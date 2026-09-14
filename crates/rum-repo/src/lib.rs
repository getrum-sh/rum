//! Repository metadata layer for rum.
//!
//! Responsibilities:
//!   * resolve a repo's mirrors (baseurl / mirrorlist / metalink)
//!   * fetch `repodata/repomd.xml`, then the `primary` metadata it references
//!   * verify checksums, decompress, and parse into available packages
//!   * cache the result under the configured cachedir, honouring
//!     `metadata_expire`
//!   * do all of the above across repos in parallel
//!
//! This is the "librepo" replacement, in pure Rust (rustls TLS, pure-Rust
//! decompressors). It performs no writes to the system, only to rum's own
//! cache directory.

mod checksum;
mod comps;
mod decompress;
mod filelists;
mod http;
mod interned;
mod primary;
mod repomd;

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub use checksum::{Checksum, ChecksumKind};
pub use comps::{
    ArchivedCompsGroup, ArchivedCompsStore, ArchivedPkgReqType, CompsHandle, PkgReqType,
};
pub use http::{detect_aws_region, Http};
pub use interned::PkgView;
pub use primary::AvailablePackage;
pub use repomd::{RepoMd, RepoMdData};

use rum_config::Repo;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("http error fetching {url}: {source}")]
    Http {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("no usable mirrors from {0}")]
    NoMirrors(String),
    #[error("all mirrors failed for repo `{0}`")]
    AllMirrorsFailed(String),
    #[error("repo `{repo}` has no `primary` metadata in repomd.xml")]
    NoPrimary { repo: String },
    #[error("checksum mismatch for {file} in repo `{repo}`")]
    ChecksumMismatch { repo: String, file: String },
    #[error("xml parse error: {0}")]
    Xml(String),
    #[error("tls configuration error: {0}")]
    Tls(String),
    #[error("decompression error: {0}")]
    Decompress(#[from] decompress::DecompressError),
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

use interned::ArchivedStore;

/// Backing store for a repo's rkyv cache bytes: an mmap of `primary.rkyv` on the
/// warm path (page-aligned, never fully read into the heap — important on tiny
/// hosts) or the freshly-serialized buffer on a refresh.
enum MetaBytes {
    Mapped(memmap2::Mmap),
    Owned(rkyv::util::AlignedVec),
}

impl std::ops::Deref for MetaBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            MetaBytes::Mapped(m) => &m[..],
            MetaBytes::Owned(v) => v.as_slice(),
        }
    }
}

/// The parsed metadata for one repository after a sync.
///
/// Held as the rkyv-archived interned [`interned::Store`] (mmap or owned bytes).
/// Query commands read `&str` views zero-copy via [`Self::views`] (no
/// per-package allocation); the resolve/download path materializes owned
/// packages via [`Self::to_owned_packages`].
pub struct RepoMetadata {
    pub repo_id: String,
    bytes: MetaBytes,
    len: usize,
    /// True if served from the local cache without hitting the network.
    pub from_cache: bool,
    /// The base URL the metadata was fetched from; package `location` hrefs are
    /// relative to this. Needed to build RPM download URLs.
    pub base_url: String,
    /// Path to this repo's `comps.rkyv` (groups/environments), if it has one.
    /// Loaded lazily via [`Self::load_comps`] only when a group op needs it.
    comps_path: Option<PathBuf>,
}

impl RepoMetadata {
    /// Wrap cache bytes, validating the rkyv layout once so a corrupt/old-format
    /// cache is rejected here (the caller then falls through to a refresh),
    /// exactly as the previous checked-deserialize did.
    fn from_bytes(
        repo_id: String,
        bytes: MetaBytes,
        from_cache: bool,
        base_url: String,
    ) -> Result<Self, RepoError> {
        let archived = rkyv::access::<ArchivedStore, rkyv::rancor::Error>(&bytes)
            .map_err(|e| RepoError::Xml(format!("corrupt metadata cache: {e}")))?;
        let len = archived.len();
        Ok(RepoMetadata {
            repo_id,
            bytes,
            len,
            from_cache,
            base_url,
            comps_path: None,
        })
    }

    /// Load this repo's groups/environments cache (mmap), if it has one.
    pub fn load_comps(&self) -> Option<comps::CompsHandle> {
        comps::CompsHandle::open(self.comps_path.as_ref()?)
    }

    /// The archived interned store backing this repo.
    fn store(&self) -> &ArchivedStore {
        // SAFETY: `bytes` was validated by `rkyv::access` in `from_bytes` and is
        // immutable for the lifetime of `self`, so unchecked access is sound.
        unsafe { rkyv::access_unchecked::<ArchivedStore>(&self.bytes) }
    }

    /// Zero-copy `&str` views over the packages (query commands).
    pub fn views(&self) -> impl Iterator<Item = PkgView<'_>> {
        self.store().views()
    }

    /// Materialize owned packages (used by the resolve/download path, which
    /// mutates and indexes them).
    pub fn to_owned_packages(&self) -> Vec<AvailablePackage> {
        self.store().to_owned_packages()
    }

    /// Rehydrate a single package by its index within this repo (to materialize
    /// only the resolver's winning set).
    pub fn rehydrate(&self, idx: usize) -> Option<AvailablePackage> {
        self.store().package_at(idx)
    }

    /// Return package indices for all packages named `name` using the binary-search index in O(log N).
    pub fn indices_by_name<'a>(&'a self, name: &'a str) -> impl Iterator<Item = usize> + 'a {
        self.store().find_by_name(name)
    }

    /// Return package indices for all packages providing capability or file `cap` using the
    /// binary-search index in O(log N).
    pub fn indices_by_provide<'a>(&'a self, cap: &'a str) -> impl Iterator<Item = usize> + 'a {
        self.store().find_by_provide(cap)
    }

    /// Return a borrowed view of the package at index `idx`.
    pub fn view_at(&self, idx: usize) -> Option<PkgView<'_>> {
        self.store().view_at(idx)
    }

    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Options controlling a sync.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Root of rum's metadata cache (from MainConfig.cachedir).
    pub cachedir: PathBuf,
    /// Ignore cache freshness and re-download.
    pub force_refresh: bool,
    /// yum variables, used to expand `$basearch`/`$releasever` etc. in URLs a
    /// mirrorlist/metalink returns (not just in the repo file).
    pub vars: rum_config::Vars,
}

impl SyncOptions {
    pub fn new(cachedir: impl Into<PathBuf>) -> Self {
        SyncOptions {
            cachedir: cachedir.into(),
            force_refresh: false,
            vars: rum_config::Vars::empty(),
        }
    }
}

/// Sync a single repository, returning its available packages.
pub fn sync_repo(repo: &Repo, opts: &SyncOptions) -> Result<RepoMetadata, RepoError> {
    let dir = opts.cachedir.join(&repo.id);
    let repomd_path = dir.join("repomd.xml");
    // Parsed-metadata cache: the primary.xml parsed into AvailablePackages and
    // serialized with rkyv, so warm runs skip re-parsing tens of MB of XML and
    // query commands read it zero-copy (mmap'd, page-aligned) without a
    // deserialize. Lives entirely under rum's own cachedir (never touches
    // dnf/yum's cache).
    let primary_cache = dir.join("primary.rkyv");
    let baseurl_path = dir.join("baseurl");

    // Fast path: fresh parsed cache plus a recorded base URL.
    if !opts.force_refresh && primary_cache.exists() && is_fresh(&repomd_path, repo.metadata_expire)
    {
        if let Ok(base_url) = std::fs::read_to_string(&baseurl_path) {
            if let Ok(file) = std::fs::File::open(&primary_cache) {
                // SAFETY: the cache is rum's own file; we treat it as immutable
                // and validate its rkyv layout in `from_bytes`.
                if let Ok(mmap) = unsafe { memmap2::Mmap::map(&file) } {
                    match RepoMetadata::from_bytes(
                        repo.id.clone(),
                        MetaBytes::Mapped(mmap),
                        true,
                        base_url.trim().to_string(),
                    ) {
                        Ok(mut md) => {
                            md.comps_path = existing_comps(&dir);
                            tracing::debug!(repo = %repo.id, count = md.len(), "loaded parsed cache");
                            return Ok(md);
                        }
                        // Corrupt/old-format cache: fall through and refresh.
                        Err(e) => tracing::debug!(repo = %repo.id, "cache rejected: {e}"),
                    }
                }
            }
        }
    }

    // Refresh: build an HTTP client honouring this repo's TLS settings, then
    // resolve mirrors and fetch repomd.xml from the first that works.
    let http = Http::for_repo(repo)?;
    let bases = http.resolve_baseurls(&repo.source, &opts.vars)?;
    let (base, repomd_bytes) = fetch_repomd(&http, &repo.id, &bases)?;
    let repomd_str = String::from_utf8_lossy(&repomd_bytes);
    let md = RepoMd::parse(&repomd_str)?;

    let primary_entry = md.get("primary").ok_or_else(|| RepoError::NoPrimary {
        repo: repo.id.clone(),
    })?;

    // Download the primary file directly to disk and verify its (compressed)
    // checksum without buffering tens to >100MB on the heap.
    // The mmap is file-backed / evictable, so on a small host the resident set stays
    // dominated by just the parsed packages + the rkyv buffer. (A 761MB
    // t2.micro OOM'd syncing RHEL BaseOS with the compressed blob heap-resident.)
    std::fs::create_dir_all(&dir).map_err(|source| RepoError::Io {
        path: dir.clone(),
        source,
    })?;
    let download_tmp = dir.join("primary.download");
    let primary_url = join_url(&base, &primary_entry.location);
    http.download_file(&primary_url, &download_tmp, Some(&primary_entry.checksum))
        .map_err(|e| match e {
            RepoError::ChecksumMismatch { file, .. } => RepoError::ChecksumMismatch {
                repo: repo.id.clone(),
                file,
            },
            other => other,
        })?;

    // Stream-decompress + stream-parse from the mmap so the (up to ~1-2GB on
    // RHEL) decompressed XML is never fully materialized either. The compressed
    // checksum verified above already guarantees integrity, so the (optional,
    // secondary) open-checksum over the decompressed bytes is skipped.
    let packages = {
        let file = std::fs::File::open(&download_tmp).map_err(|source| RepoError::Io {
            path: download_tmp.clone(),
            source,
        })?;
        // SAFETY: our own freshly-written file, treated as immutable here.
        let mmap = unsafe { memmap2::Mmap::map(&file) }.map_err(|source| RepoError::Io {
            path: download_tmp.clone(),
            source,
        })?;
        let plain = decompress::reader(&primary_entry.location, &mmap)?;
        primary::parse_reader(plain, &repo.id)?
    };
    let _ = std::fs::remove_file(&download_tmp);
    tracing::info!(repo = %repo.id, count = packages.packages.len(), %base, "refreshed metadata");

    // Persist to cache: repomd.xml (freshness anchor) plus the interned store
    // serialized with rkyv to primary.rkyv (written last, so a present parsed
    // cache always has a matching repomd.xml governing its freshness).
    let encoded = rkyv::to_bytes::<rkyv::rancor::Error>(&packages)
        .map_err(|e| RepoError::Xml(format!("failed to serialize metadata cache: {e}")))?;
    // Drop the interned store before writing/mapping so we don't hold it and the
    // serialized buffer at once.
    drop(packages);
    write_cache(&dir, &repomd_path, &repomd_bytes, &primary_cache, &encoded)?;
    let _ = std::fs::write(&baseurl_path, &base);

    // Build the groups/environments cache (comps.xml), if this repo has one.
    // Best-effort: a missing/failed comps just means no group support here.
    let comps_path = build_comps_cache(&http, &base, &md, &dir);

    // Prefer an mmap of the just-written cache over keeping the (large) rkyv
    // buffer resident: sync_all retains one RepoMetadata per repo, so several
    // big repos (RHEL BaseOS + AppStream) would otherwise pin hundreds of MB of
    // heap at once and OOM a small host. The mmap is file-backed and evictable.
    let bytes = match std::fs::File::open(&primary_cache)
        .ok()
        .and_then(|f| unsafe { memmap2::Mmap::map(&f) }.ok())
    {
        Some(mmap) => {
            drop(encoded);
            MetaBytes::Mapped(mmap)
        }
        None => MetaBytes::Owned(encoded),
    };
    let mut md_out = RepoMetadata::from_bytes(repo.id.clone(), bytes, false, base)?;
    md_out.comps_path = comps_path;
    Ok(md_out)
}

/// The comps cache path if it exists (warm path).
fn existing_comps(dir: &Path) -> Option<PathBuf> {
    let p = dir.join("comps.rkyv");
    p.exists().then_some(p)
}

/// Fetch + parse a repo's `comps.xml` (groups) and bake it into `comps.rkyv`.
/// Returns the cache path on success, `None` if the repo has no comps or on any
/// error (group support is optional and must never fail a sync).
fn build_comps_cache(http: &Http, base: &str, md: &RepoMd, dir: &Path) -> Option<PathBuf> {
    // createrepo emits type "group" (plain) or "group_<compression>".
    let entry = ["group_gz", "group_zst", "group_xz", "group"]
        .iter()
        .find_map(|t| md.get(t))?;
    let url = join_url(base, &entry.location);
    let compressed = http.get_bytes(&url).ok()?;
    if !entry.checksum.verify(&compressed) {
        return None;
    }
    let reader = decompress::reader(&entry.location, &compressed).ok()?;
    let store = comps::parse_reader(reader).ok()?;
    let encoded = rkyv::to_bytes::<rkyv::rancor::Error>(&store).ok()?;
    let path = dir.join("comps.rkyv");
    std::fs::write(&path, &encoded).ok()?;
    tracing::debug!(
        groups = store.groups.len(),
        envs = store.environments.len(),
        "cached comps"
    );
    Some(path)
}

/// Lazily load file ownership from a repo's `filelists.xml`, filtered to
/// `wanted` file paths. Returns `(pkgid, files)` where `pkgid` matches an
/// `AvailablePackage::checksum.hex`. Uses the cached repomd.xml + base URL from
/// a prior [`sync_repo`]; downloads and caches filelists.xml on first use.
///
/// This is the fallback for file-based dependencies whose paths are not in
/// `primary.xml` (createrepo's core-files filter). It is only called when a
/// resolve fails on such a path, so the large filelists file is never fetched
/// on the warm path.
pub fn load_filelists(
    repo: &Repo,
    opts: &SyncOptions,
    wanted: &std::collections::HashSet<String>,
) -> Result<Vec<(String, Vec<String>)>, RepoError> {
    let dir = opts.cachedir.join(&repo.id);
    let repomd_bytes = read_file(&dir.join("repomd.xml"))?;
    let md = RepoMd::parse(&String::from_utf8_lossy(&repomd_bytes))?;
    let entry = md.get("filelists").ok_or_else(|| RepoError::NoPrimary {
        repo: repo.id.clone(),
    })?;

    // Cache the *compressed* filelists (tens of MB) rather than the decompressed
    // form (~100MB+); we stream-decompress+parse it so it's never fully in RAM.
    let cache = dir.join("filelists.cache");
    let compressed = match std::fs::read(&cache) {
        Ok(bytes) if entry.checksum.verify(&bytes) => bytes,
        _ => {
            let baseurl_path = dir.join("baseurl");
            let base_url =
                std::fs::read_to_string(&baseurl_path).map_err(|source| RepoError::Io {
                    path: baseurl_path,
                    source,
                })?;
            let http = Http::for_repo(repo)?;
            let url = join_url(base_url.trim(), &entry.location);
            http.download_file(&url, &cache, Some(&entry.checksum))
                .map_err(|e| match e {
                    RepoError::ChecksumMismatch { file, .. } => RepoError::ChecksumMismatch {
                        repo: repo.id.clone(),
                        file,
                    },
                    other => other,
                })?;
            std::fs::read(&cache).map_err(|source| RepoError::Io {
                path: cache.clone(),
                source,
            })?
        }
    };

    let reader = decompress::reader(&entry.location, &compressed)?;
    filelists::parse(reader, wanted)
}

/// Sync many repos in parallel. Returns one result per repo (in the input
/// order); a failing repo does not abort the others, matching dnf's behaviour
/// of continuing with the repos that are reachable.
pub fn sync_all<'a>(
    repos: &'a [&'a Repo],
    opts: &SyncOptions,
) -> Vec<(String, Result<RepoMetadata, RepoError>)> {
    if repos.is_empty() {
        return Vec::new();
    }
    // Bound parallelism so we don't open an unreasonable number of sockets on
    // hosts with very many repos.
    let max_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(repos.len())
        .min(16);

    let next_idx = std::sync::atomic::AtomicUsize::new(0);
    let mut results: Vec<(String, Result<RepoMetadata, RepoError>)> =
        Vec::with_capacity(repos.len());

    std::thread::scope(|scope| {
        // Dynamic work distribution across worker threads: threads fetch the next
        // repo index atomically. A slow/large repo (e.g. BaseOS) will not cause
        // idle threads to stall behind it.
        let mut handles = Vec::with_capacity(max_threads);
        for _ in 0..max_threads {
            handles.push(scope.spawn(|| {
                let mut part = Vec::new();
                loop {
                    let idx = next_idx.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if idx >= repos.len() {
                        break;
                    }
                    let r = repos[idx];
                    part.push((idx, (r.id.clone(), sync_repo(r, opts))));
                }
                part
            }));
        }
        let mut indexed_results = Vec::with_capacity(repos.len());
        for h in handles {
            if let Ok(part) = h.join() {
                indexed_results.extend(part);
            }
        }
        // Restore input order exactly.
        indexed_results.sort_by_key(|(idx, _)| *idx);
        results = indexed_results.into_iter().map(|(_, res)| res).collect();
    });

    results
}

fn fetch_repomd(
    http: &Http,
    repo_id: &str,
    bases: &[String],
) -> Result<(String, Vec<u8>), RepoError> {
    let mut last_err = None;
    for base in bases {
        let url = format!("{base}/repodata/repomd.xml");
        match http.get_bytes(&url) {
            Ok(bytes) => return Ok((base.clone(), bytes)),
            Err(e) => {
                tracing::debug!(repo = %repo_id, %base, "mirror failed: {e}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| RepoError::AllMirrorsFailed(repo_id.to_string())))
}

/// Is the cached file present and younger than `expire` seconds?
/// `expire < 0` means never expire; `expire == 0` means always stale.
fn is_fresh(path: &Path, expire: i64) -> bool {
    if expire < 0 {
        return path.exists();
    }
    if expire == 0 {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    match SystemTime::now().duration_since(modified) {
        Ok(age) => age <= Duration::from_secs(expire as u64),
        // Modified in the future (clock skew): treat as fresh.
        Err(_) => true,
    }
}

fn join_url(base: &str, href: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        href.trim_start_matches('/')
    )
}

fn read_file(path: &Path) -> Result<Vec<u8>, RepoError> {
    std::fs::read(path).map_err(|source| RepoError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Write the freshness anchor (repomd.xml) and the parsed-metadata cache
/// (primary.rkyv). repomd is written first so the parsed cache never appears
/// without a repomd governing its freshness.
fn write_cache(
    dir: &Path,
    repomd_path: &Path,
    repomd: &[u8],
    primary_cache: &Path,
    parsed: &[u8],
) -> Result<(), RepoError> {
    std::fs::create_dir_all(dir).map_err(|source| RepoError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    std::fs::write(repomd_path, repomd).map_err(|source| RepoError::Io {
        path: repomd_path.to_path_buf(),
        source,
    })?;
    std::fs::write(primary_cache, parsed).map_err(|source| RepoError::Io {
        path: primary_cache.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_joining() {
        assert_eq!(
            join_url("https://a/repo", "repodata/x.gz"),
            "https://a/repo/repodata/x.gz"
        );
        assert_eq!(
            join_url("https://a/repo/", "/repodata/x.gz"),
            "https://a/repo/repodata/x.gz"
        );
    }

    #[test]
    fn freshness_rules() {
        // Non-existent path is never fresh.
        assert!(!is_fresh(Path::new("/nonexistent/repomd.xml"), 3600));

        // Use a freshly-created temp file (mtime ~= now).
        let tmp = std::env::temp_dir().join(format!("rum-fresh-{}", std::process::id()));
        std::fs::write(&tmp, b"x").unwrap();

        // expire == 0 is always stale even if the file exists.
        assert!(!is_fresh(&tmp, 0));
        // A long window on a just-written file is fresh.
        assert!(is_fresh(&tmp, 3600));
        // Never-expire on an existing file is fresh.
        assert!(is_fresh(&tmp, -1));

        std::fs::remove_file(&tmp).ok();
    }
}
