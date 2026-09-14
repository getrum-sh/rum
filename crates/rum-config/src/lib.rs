//! Configuration loading for rum.
//!
//! rum intentionally reuses the existing dnf/yum configuration on the host:
//!   * global config from `/etc/dnf/dnf.conf` (falling back to `/etc/yum.conf`)
//!   * repositories from `*.repo` files under `/etc/yum.repos.d/`
//!   * variables (`$releasever`, `$basearch`, user vars) from the system
//!
//! This is what lets rum coexist with dnf/yum: it sees the same repos. Note
//! that installed-package state is deliberately NOT part of this config, it is
//! always read live from the rpmdb so rum and dnf never disagree about what is
//! installed.

mod ini;
mod repo;
mod vars;

use std::path::{Path, PathBuf};

pub use repo::{MainConfig, Repo, RepoSource};
pub use vars::Vars;

use ini::Ini;
use repo::{parse_bool, parse_duration_secs};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("i/o error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Fully resolved configuration: `[main]` defaults plus every repository.
#[derive(Debug, Clone)]
pub struct Config {
    pub main: MainConfig,
    pub repos: Vec<Repo>,
    pub vars: Vars,
}

/// Filesystem locations rum reads. Overridable for tests / `--installroot`.
#[derive(Debug, Clone)]
pub struct Paths {
    pub main_config_candidates: Vec<PathBuf>,
    pub repos_dir: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Paths {
            main_config_candidates: vec![
                PathBuf::from("/etc/dnf/dnf.conf"),
                PathBuf::from("/etc/yum.conf"),
            ],
            repos_dir: PathBuf::from("/etc/yum.repos.d"),
        }
    }
}

impl Config {
    /// Load configuration from the default system locations.
    pub fn load_system() -> Result<Self, ConfigError> {
        Self::load_with(&Paths::default(), Vars::detect())
    }

    /// Load configuration from explicit paths with explicit variables.
    pub fn load_with(paths: &Paths, vars: Vars) -> Result<Self, ConfigError> {
        let (main, main_ini) = load_main(&paths.main_config_candidates)?;
        let mut repos = Vec::new();

        // Repos may also be declared inline in the main config file.
        collect_repos(&main_ini, &main, &vars, &mut repos);

        if let Ok(entries) = std::fs::read_dir(&paths.repos_dir) {
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "repo"))
                .collect();
            files.sort();
            for file in files {
                let text = std::fs::read_to_string(&file).map_err(|source| ConfigError::Io {
                    path: file.clone(),
                    source,
                })?;
                let repo_ini = Ini::parse(&text);
                collect_repos(&repo_ini, &main, &vars, &mut repos);
            }
        }

        Ok(Config { main, repos, vars })
    }

    /// Repos with `enabled=1`, in priority then id order.
    pub fn enabled_repos(&self) -> Vec<&Repo> {
        let mut v: Vec<&Repo> = self.repos.iter().filter(|r| r.enabled).collect();
        v.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));
        v
    }
}

fn load_main(candidates: &[PathBuf]) -> Result<(MainConfig, Ini), ConfigError> {
    for path in candidates {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let ini = Ini::parse(&text);
                let main = build_main(&ini);
                return Ok((main, ini));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.clone(),
                    source,
                })
            }
        }
    }
    // No dnf.conf / yum.conf present. dnf treats the main config as optional
    // (its settings just fall back to defaults) and still reads the repo files
    // under /etc/yum.repos.d/. Minimal images (e.g. Oracle Linux *-slim, which
    // ship repo files but no dnf.conf) rely on this, so fall back to defaults
    // rather than failing.
    Ok((MainConfig::default(), Ini::parse("")))
}

fn build_main(ini: &Ini) -> MainConfig {
    let mut main = MainConfig::default();
    let Some(section) = ini.sections.iter().find(|s| s.name == "main") else {
        return main;
    };
    let e = &section.entries;
    if let Some(v) = e.get("cachedir") {
        main.cachedir = v.clone();
    }
    if let Some(v) = e.get("gpgcheck") {
        main.gpgcheck = parse_bool(v, main.gpgcheck);
    }
    if let Some(v) = e.get("best") {
        main.best = parse_bool(v, main.best);
    }
    if let Some(v) = e.get("clean_requirements_on_remove") {
        main.clean_requirements_on_remove = parse_bool(v, main.clean_requirements_on_remove);
    }
    if let Some(v) = e.get("keepcache") {
        main.keepcache = parse_bool(v, main.keepcache);
    }
    if let Some(v) = e.get("installonly_limit") {
        if let Ok(n) = v.parse() {
            main.installonly_limit = n;
        }
    }
    if let Some(v) = e.get("metadata_expire") {
        main.metadata_expire = parse_duration_secs(v, main.metadata_expire);
    }
    if let Some(v) = e.get("install_weak_deps") {
        main.install_weak_deps = parse_bool(v, main.install_weak_deps);
    }
    main
}

/// Extract every repo section (i.e. every section other than `main`) from an
/// INI document, applying variable substitution and inheriting `[main]`.
fn collect_repos(ini: &Ini, main: &MainConfig, vars: &Vars, out: &mut Vec<Repo>) {
    for section in &ini.sections {
        if section.name == "main" || section.name.is_empty() {
            continue;
        }
        let e = &section.entries;
        let expand = |s: &str| vars.expand(s);

        let source = if let Some(url) = e.get("mirrorlist") {
            RepoSource::MirrorList(expand(url))
        } else if let Some(url) = e.get("metalink") {
            RepoSource::MetaLink(expand(url))
        } else if let Some(urls) = e.get("baseurl") {
            RepoSource::BaseUrls(urls.split_whitespace().map(expand).collect())
        } else {
            // A section with no source isn't a usable repo; skip it.
            continue;
        };

        let gpgcheck = e
            .get("gpgcheck")
            .map(|v| parse_bool(v, main.gpgcheck))
            .unwrap_or(main.gpgcheck);

        let repo = Repo {
            id: section.name.clone(),
            name: e
                .get("name")
                .map(|s| expand(s))
                .unwrap_or_else(|| section.name.clone()),
            source,
            enabled: e
                .get("enabled")
                .map(|v| parse_bool(v, true))
                .unwrap_or(true),
            gpgcheck,
            repo_gpgcheck: e
                .get("repo_gpgcheck")
                .map(|v| parse_bool(v, false))
                .unwrap_or(false),
            gpgkeys: e
                .get("gpgkey")
                .map(|s| s.split_whitespace().map(expand).collect())
                .unwrap_or_default(),
            priority: e.get("priority").and_then(|v| v.parse().ok()).unwrap_or(99),
            metadata_expire: e
                .get("metadata_expire")
                .map(|v| parse_duration_secs(v, main.metadata_expire))
                .unwrap_or(main.metadata_expire),
            sslclientcert: e.get("sslclientcert").map(|s| expand(s)),
            sslclientkey: e.get("sslclientkey").map(|s| expand(s)),
            sslcacert: e.get("sslcacert").map(|s| expand(s)),
            sslverify: e
                .get("sslverify")
                .map(|v| parse_bool(v, true))
                .unwrap_or(true),
        };
        out.push(repo);
    }
}

/// Convenience: does a system dnf/yum config appear to exist here?
pub fn system_has_config(paths: &Paths) -> bool {
    paths
        .main_config_candidates
        .iter()
        .any(|p| Path::new(p).exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        p
    }

    fn test_vars() -> Vars {
        let mut v = Vars::empty();
        v.insert("releasever", "2023");
        v.insert("basearch", "x86_64");
        v
    }

    #[test]
    fn loads_main_and_repos_with_substitution() {
        let tmp = std::env::temp_dir().join(format!("rum-cfg-test-{}", std::process::id()));
        let repos = tmp.join("repos.d");
        std::fs::create_dir_all(&repos).unwrap();

        let main_path = write(
            &tmp,
            "dnf.conf",
            "[main]\ngpgcheck=1\nbest=True\ninstallonly_limit=5\n",
        );
        write(
            &repos,
            "amazonlinux.repo",
            "[baseos]\nname=AL $releasever\nbaseurl=https://cdn/$releasever/$basearch/os\ngpgcheck=1\nenabled=1\n\n[disabled]\nbaseurl=https://x/\nenabled=0\n",
        );

        let paths = Paths {
            main_config_candidates: vec![main_path],
            repos_dir: repos,
        };
        let cfg = Config::load_with(&paths, test_vars()).unwrap();

        assert_eq!(cfg.main.installonly_limit, 5);
        assert!(cfg.main.best);
        assert_eq!(cfg.repos.len(), 2);

        let enabled = cfg.enabled_repos();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id, "baseos");
        assert_eq!(enabled[0].name, "AL 2023");
        match &enabled[0].source {
            RepoSource::BaseUrls(urls) => {
                assert_eq!(urls[0], "https://cdn/2023/x86_64/os");
            }
            other => panic!("unexpected source: {other:?}"),
        }

        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn missing_main_config_falls_back_to_defaults_and_repos() {
        // Minimal images (e.g. Oracle Linux *-slim) ship repo files but no
        // dnf.conf/yum.conf; rum must still load, using default [main] settings.
        let tmp = std::env::temp_dir().join(format!("rum-cfg-nomain-{}", std::process::id()));
        let repos = tmp.join("repos.d");
        std::fs::create_dir_all(&repos).unwrap();
        write(
            &repos,
            "ol.repo",
            "[ol9_baseos]\nname=OL $releasever BaseOS\nbaseurl=https://yum/$releasever/$basearch/\nenabled=1\n",
        );

        let paths = Paths {
            // Point only at a non-existent main config.
            main_config_candidates: vec![tmp.join("does-not-exist.conf")],
            repos_dir: repos,
        };
        let cfg = Config::load_with(&paths, test_vars()).unwrap();

        // Defaults applied, and the repo file was still read.
        assert_eq!(
            cfg.main.installonly_limit,
            MainConfig::default().installonly_limit
        );
        assert_eq!(cfg.enabled_repos().len(), 1);
        assert_eq!(cfg.enabled_repos()[0].id, "ol9_baseos");

        std::fs::remove_dir_all(&tmp).ok();
    }
}
