//! HTTP fetching (pure-Rust TLS via rustls) and mirror/metalink resolution.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;

use crate::RepoError;
use rum_config::{Repo, RepoSource, Vars};

// AWS RHUI (Red Hat's Update Infrastructure on EC2) authorizes content requests
// with two HTTP headers carrying the instance's signed identity, injected by
// the `amazon-id` dnf plugin. The client TLS cert alone gets a 403 on the Pulp
// content endpoint; these headers are what unlock it.
const RHUI_ID_HEADER: &str = "X-RHUI-ID";
const RHUI_SIG_HEADER: &str = "X-RHUI-SIGNATURE";
const IMDS_TOKEN_URL: &str = "http://169.254.169.254/latest/api/token";
const IMDS_DOC_URL: &str = "http://169.254.169.254/latest/dynamic/instance-identity/document";
const IMDS_SIG_URL: &str = "http://169.254.169.254/latest/dynamic/instance-identity/signature";

/// A reusable HTTP client. Holds a connection-pooling agent. Cheap to clone
/// (the agent is `Arc`-backed) and safe to share across threads.
#[derive(Clone)]
pub struct Http {
    agent: ureq::Agent,
    /// Extra headers sent on every request (e.g. the RHUI identity headers).
    headers: Vec<(String, String)>,
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Http {
    pub fn new() -> Self {
        let agent = base_builder().build();
        Http {
            agent,
            headers: Vec::new(),
        }
    }

    /// Build an HTTP client honouring a repo's TLS settings (client
    /// certificate for mutual-TLS repos like Red Hat RHUI, and an extra CA),
    /// and, for RHUI repos, the `X-RHUI-ID`/`X-RHUI-SIGNATURE` identity headers.
    /// Falls back to the default client when no client cert is configured.
    pub fn for_repo(repo: &Repo) -> Result<Self, RepoError> {
        let headers = if is_rhui(repo) {
            rhui_identity_headers()
        } else {
            Vec::new()
        };
        let (Some(cert), Some(key)) = (&repo.sslclientcert, &repo.sslclientkey) else {
            return Ok(Http {
                agent: base_builder().build(),
                headers,
            });
        };
        let config = client_cert_config(cert, key, repo.sslcacert.as_deref())?;
        let agent = base_builder().tls_config(Arc::new(config)).build();
        Ok(Http { agent, headers })
    }

    /// GET a URL and return the raw body bytes.
    pub fn get_bytes(&self, url: &str) -> Result<Vec<u8>, RepoError> {
        let mut req = self.agent.get(url);
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        let resp = req.call().map_err(|e| RepoError::Http {
            url: url.to_string(),
            source: Box::new(e),
        })?;
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf).map_err(|e| {
            RepoError::Http {
                url: url.to_string(),
                source: Box::new(e),
            }
        })?;
        Ok(buf)
    }

    /// Stream download a URL directly to a file, verifying its checksum inline
    /// without loading the entire body into RAM. Writes to a temporary file in
    /// the destination directory first and atomically renames on success.
    pub fn download_file(
        &self,
        url: &str,
        dest: &std::path::Path,
        checksum: Option<&crate::Checksum>,
    ) -> Result<(), RepoError> {
        static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

        let mut req = self.agent.get(url);
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        let resp = req.call().map_err(|e| RepoError::Http {
            url: url.to_string(),
            source: Box::new(e),
        })?;

        let parent = dest.parent().unwrap_or_else(|| std::path::Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| RepoError::Io {
            path: parent.to_path_buf(),
            source,
        })?;

        let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dest_filename = dest
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("download");
        let temp_name = format!(".tmp.{}.{seq}.{dest_filename}", std::process::id());
        let temp_path = parent.join(temp_name);

        struct CleanupGuard<'a>(&'a std::path::Path, bool);
        impl<'a> Drop for CleanupGuard<'a> {
            fn drop(&mut self) {
                if !self.1 {
                    let _ = std::fs::remove_file(self.0);
                }
            }
        }
        let mut guard = CleanupGuard(&temp_path, false);

        let file = std::fs::File::create(&temp_path).map_err(|source| RepoError::Io {
            path: temp_path.clone(),
            source,
        })?;
        let mut writer = std::io::BufWriter::new(file);
        let mut reader = resp.into_reader();

        let matched = if let Some(cs) = checksum {
            cs.copy_and_verify(&mut reader, &mut writer)
                .map_err(|source| RepoError::Io {
                    path: temp_path.clone(),
                    source,
                })?
        } else {
            std::io::copy(&mut reader, &mut writer).map_err(|source| RepoError::Io {
                path: temp_path.clone(),
                source,
            })?;
            std::io::Write::flush(&mut writer).map_err(|source| RepoError::Io {
                path: temp_path.clone(),
                source,
            })?;
            true
        };

        drop(writer);

        if !matched {
            return Err(RepoError::ChecksumMismatch {
                repo: String::new(),
                file: dest_filename.to_string(),
            });
        }

        std::fs::rename(&temp_path, dest).map_err(|source| RepoError::Io {
            path: dest.to_path_buf(),
            source,
        })?;

        guard.1 = true;
        Ok(())
    }

    fn get_text(&self, url: &str) -> Result<String, RepoError> {
        let bytes = self.get_bytes(url)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Resolve a repo source into a list of candidate base URLs (each is a
    /// directory under which `repodata/repomd.xml` is expected), best first.
    ///
    /// URLs returned in a mirrorlist/metalink body may still contain yum
    /// variables (AlmaLinux's mirrorlist emits `.../$basearch/os/`, expecting
    /// the client to substitute), so every resolved URL is variable-expanded
    /// with `vars` — the same substitution the repo file itself gets.
    pub fn resolve_baseurls(
        &self,
        source: &RepoSource,
        vars: &Vars,
    ) -> Result<Vec<String>, RepoError> {
        match source {
            RepoSource::BaseUrls(urls) => Ok(urls.iter().map(|u| trim_trailing_slash(u)).collect()),
            RepoSource::MirrorList(url) => {
                let text = self.get_text(url)?;
                let mut urls: Vec<String> = parse_mirrorlist(&text)
                    .iter()
                    .map(|u| trim_trailing_slash(&vars.expand(u)))
                    .collect();
                // Red Hat RHUI's Pulp "mirror" endpoint is itself a working
                // base URL (repodata lives directly under it), even though the
                // URL it lists in its body may return 403. Add the mirrorlist
                // URL as a last-resort base, tried after the advertised
                // mirrors (so ordinary Fedora-style mirrorlists are unchanged).
                urls.push(trim_trailing_slash(url));
                Ok(urls)
            }
            RepoSource::MetaLink(url) => {
                let text = self.get_text(url)?;
                let urls: Vec<String> = parse_metalink_baseurls(&text)
                    .iter()
                    .map(|u| vars.expand(u))
                    .collect();
                if urls.is_empty() {
                    return Err(RepoError::NoMirrors(url.clone()));
                }
                Ok(urls)
            }
        }
    }
}

fn base_builder() -> ureq::AgentBuilder {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(60))
        .user_agent(concat!("rum/", env!("CARGO_PKG_VERSION")))
}

/// Build a rustls client config with a client certificate (mutual TLS) and,
/// optionally, an extra CA bundle merged with the webpki root store.
fn client_cert_config(
    cert_path: &str,
    key_path: &str,
    cacert_path: Option<&str>,
) -> Result<rustls::ClientConfig, RepoError> {
    let read = |p: &str| -> Result<Vec<u8>, RepoError> {
        std::fs::read(p).map_err(|source| RepoError::Io {
            path: p.into(),
            source,
        })
    };

    // Root store: public roots plus any extra CA (e.g. Red Hat's CDN chain).
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(ca) = cacert_path {
        let ca_bytes = read(ca)?;
        for cert in rustls_pemfile::certs(&mut ca_bytes.as_slice()).flatten() {
            let _ = roots.add(cert);
        }
    }

    // Client certificate chain + private key.
    let cert_bytes = read(cert_path)?;
    let chain: Vec<_> = rustls_pemfile::certs(&mut cert_bytes.as_slice())
        .flatten()
        .collect();
    let key_bytes = read(key_path)?;
    let key = rustls_pemfile::private_key(&mut key_bytes.as_slice())
        .ok()
        .flatten()
        .ok_or_else(|| RepoError::Tls(format!("no private key in {key_path}")))?;

    // Build with an explicit ring provider so we don't depend on a process
    // default crypto provider being installed first.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| RepoError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)
        .map_err(|e| RepoError::Tls(e.to_string()))
}

/// Detect the current AWS region via IMDSv2 (used to substitute the literal
/// `REGION` token in Red Hat RHUI repo URLs, as the `amazon-id` dnf plugin
/// does). Returns `None` off-EC2 or on any failure.
pub fn detect_aws_region() -> Option<String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(500))
        .timeout_read(Duration::from_millis(500))
        .build();
    let token = agent
        .put("http://169.254.169.254/latest/api/token")
        .set("X-aws-ec2-metadata-token-ttl-seconds", "60")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let region = agent
        .get("http://169.254.169.254/latest/meta-data/placement/region")
        .set("X-aws-ec2-metadata-token", &token)
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let region = region.trim().to_string();
    (!region.is_empty()).then_some(region)
}

/// Is this an AWS RHUI repo (needs the instance-identity headers)? Matches
/// dnf's `amazon-id` plugin heuristic (`rhui-` in the id) plus the RHUI host.
fn is_rhui(repo: &Repo) -> bool {
    if repo.id.contains("rhui") {
        return true;
    }
    let host_match = |u: &str| u.contains("aws.ce.redhat.com");
    match &repo.source {
        RepoSource::BaseUrls(urls) => urls.iter().any(|u| host_match(u)),
        RepoSource::MirrorList(u) | RepoSource::MetaLink(u) => host_match(u),
    }
}

/// The `X-RHUI-ID` / `X-RHUI-SIGNATURE` headers for RHUI content authorization,
/// mirroring the `amazon-id` dnf plugin: the EC2 instance-identity document and
/// its signature, each urlsafe-base64-encoded. Fetched once per process (via
/// IMDSv2); empty off-EC2 or on any failure.
fn rhui_identity_headers() -> Vec<(String, String)> {
    static CACHE: OnceLock<Vec<(String, String)>> = OnceLock::new();
    CACHE
        .get_or_init(|| fetch_rhui_identity().unwrap_or_default())
        .clone()
}

fn fetch_rhui_identity() -> Option<Vec<(String, String)>> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(500))
        .timeout_read(Duration::from_millis(500))
        .build();
    let token = agent
        .put(IMDS_TOKEN_URL)
        .set("X-aws-ec2-metadata-token-ttl-seconds", "3600")
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let fetch = |url: &str| -> Option<Vec<u8>> {
        let mut buf = Vec::new();
        let resp = agent
            .get(url)
            .set("X-aws-ec2-metadata-token", &token)
            .call()
            .ok()?;
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut buf).ok()?;
        Some(buf)
    };
    let doc = fetch(IMDS_DOC_URL)?;
    let sig = fetch(IMDS_SIG_URL)?;
    // URL_SAFE keeps '=' padding, matching Python's urlsafe_b64encode.
    let enc = base64::engine::general_purpose::URL_SAFE;
    Some(vec![
        (RHUI_ID_HEADER.to_string(), enc.encode(&doc)),
        (RHUI_SIG_HEADER.to_string(), enc.encode(&sig)),
    ])
}

fn trim_trailing_slash(u: &str) -> String {
    u.trim_end_matches('/').to_string()
}

/// A mirrorlist is plain text: one base URL per line, `#` comments, blanks.
fn parse_mirrorlist(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter(|l| {
            l.starts_with("http://") || l.starts_with("https://") || l.starts_with("file://")
        })
        .map(trim_trailing_slash)
        .collect()
}

/// A metalink points at `repodata/repomd.xml` directly with multiple `<url>`
/// entries; we strip that suffix to recover each mirror's base URL.
fn parse_metalink_baseurls(text: &str) -> Vec<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;

    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut in_url = false;
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.local_name().as_ref() == b"url" => in_url = true,
            Ok(Event::End(e)) if e.local_name().as_ref() == b"url" => in_url = false,
            Ok(Event::Text(t)) if in_url => {
                let full = t.unescape().unwrap_or_default().to_string();
                if let Some(base) = full.strip_suffix("/repodata/repomd.xml") {
                    out.push(base.to_string());
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mirrorlist() {
        let text = "# comment\n\nhttps://a.example/repo/\nhttp://b.example/repo\ngarbage line\n";
        let urls = parse_mirrorlist(text);
        assert_eq!(
            urls,
            vec!["https://a.example/repo", "http://b.example/repo"]
        );
    }

    #[test]
    fn parses_metalink() {
        let xml = r#"<metalink><files><file name="repomd.xml"><resources>
            <url protocol="https">https://m1.example/os/repodata/repomd.xml</url>
            <url protocol="https">https://m2.example/os/repodata/repomd.xml</url>
        </resources></file></files></metalink>"#;
        let urls = parse_metalink_baseurls(xml);
        assert_eq!(urls, vec!["https://m1.example/os", "https://m2.example/os"]);
    }

    #[test]
    fn mirrorlist_urls_get_variable_expanded() {
        // AlmaLinux's mirrorlist body returns URLs that still contain
        // $basearch; the client must substitute (dnf does). Expanding each
        // parsed URL with the repo's vars is what resolve_baseurls does.
        let mut vars = Vars::empty();
        vars.insert("basearch", "x86_64");
        let body = "http://mirror.example/almalinux/9.8/BaseOS/$basearch/os/\n";
        let expanded: Vec<String> = parse_mirrorlist(body)
            .iter()
            .map(|u| trim_trailing_slash(&vars.expand(u)))
            .collect();
        assert_eq!(
            expanded,
            vec!["http://mirror.example/almalinux/9.8/BaseOS/x86_64/os"]
        );
    }
}
