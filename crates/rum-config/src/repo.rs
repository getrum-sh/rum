//! Repository and main-config models built from parsed INI + variables.

/// How a repo advertises its mirrors / package location.
#[derive(Debug, Clone)]
pub enum RepoSource {
    /// One or more explicit base URLs (dnf allows a list).
    BaseUrls(Vec<String>),
    /// A mirrorlist URL returning a plain list of base URLs.
    MirrorList(String),
    /// A metalink URL returning an XML metalink document.
    MetaLink(String),
}

/// A single repository definition (post variable-substitution).
#[derive(Debug, Clone)]
pub struct Repo {
    /// Section id, e.g. `baseos`.
    pub id: String,
    /// Human name, e.g. `Amazon Linux 2023 repository`.
    pub name: String,
    pub source: RepoSource,
    pub enabled: bool,
    /// Whether package GPG signatures are checked. Inherits from [main].
    pub gpgcheck: bool,
    /// Whether repo metadata (repomd.xml) signature is checked.
    pub repo_gpgcheck: bool,
    /// GPG key URIs (may be file:// or https://), variable-expanded.
    pub gpgkeys: Vec<String>,
    /// Repo priority (lower is preferred); dnf default is 99.
    pub priority: i32,
    /// Metadata freshness window in seconds (dnf `metadata_expire`).
    pub metadata_expire: i64,
    /// TLS client certificate (PEM) for mutual-TLS repos (e.g. Red Hat RHUI).
    pub sslclientcert: Option<String>,
    /// TLS client private key (PEM) paired with `sslclientcert`.
    pub sslclientkey: Option<String>,
    /// Extra CA bundle (PEM) to trust for the repo's TLS server.
    pub sslcacert: Option<String>,
    /// Whether to verify the repo's TLS certificate (dnf `sslverify`).
    pub sslverify: bool,
}

/// The `[main]` section: global defaults.
#[derive(Debug, Clone)]
pub struct MainConfig {
    pub cachedir: String,
    pub gpgcheck: bool,
    pub best: bool,
    pub clean_requirements_on_remove: bool,
    pub installonly_limit: u32,
    pub metadata_expire: i64,
    pub keepcache: bool,
    pub install_weak_deps: bool,
}

impl Default for MainConfig {
    fn default() -> Self {
        MainConfig {
            // rum keeps its own cache namespace so it never collides with dnf's
            // on-disk metadata format. Installed-package state is NOT cached
            // here; that always comes live from the rpmdb.
            cachedir: "/var/cache/rum".to_string(),
            gpgcheck: true,
            best: true,
            clean_requirements_on_remove: true,
            installonly_limit: 3,
            metadata_expire: 48 * 3600,
            keepcache: false,
            install_weak_deps: true,
        }
    }
}

/// Parse a dnf-style boolean (`1/0`, `true/false`, `yes/no`, `on/off`).
pub(crate) fn parse_bool(s: &str, default: bool) -> bool {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "enabled" => true,
        "0" | "false" | "no" | "off" | "disabled" => false,
        _ => default,
    }
}

/// Parse dnf's `metadata_expire`: seconds, or a number with a unit suffix
/// (`s`/`m`/`h`/`d`), or `never`/`-1`.
pub(crate) fn parse_duration_secs(s: &str, default: i64) -> i64 {
    let s = s.trim();
    if s.eq_ignore_ascii_case("never") || s == "-1" {
        return -1;
    }
    let (num, mult): (&str, i64) = match s.chars().last() {
        Some('s') | Some('S') => (&s[..s.len() - 1], 1),
        Some('m') | Some('M') => (&s[..s.len() - 1], 60),
        Some('h') | Some('H') => (&s[..s.len() - 1], 3600),
        Some('d') | Some('D') => (&s[..s.len() - 1], 86400),
        _ => (s, 1),
    };
    num.trim()
        .parse::<i64>()
        .map(|n| n * mult)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn booleans() {
        assert!(parse_bool("1", false));
        assert!(parse_bool("True", false));
        assert!(!parse_bool("off", true));
        assert!(parse_bool("garbage", true));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("3600", 0), 3600);
        assert_eq!(parse_duration_secs("6h", 0), 6 * 3600);
        assert_eq!(parse_duration_secs("2d", 0), 2 * 86400);
        assert_eq!(parse_duration_secs("never", 0), -1);
        assert_eq!(parse_duration_secs("bogus", 42), 42);
    }
}
