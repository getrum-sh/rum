//! dnf/yum variable substitution.
//!
//! Repo files reference `$releasever`, `$basearch`, `$arch`, `$releasever_major`,
//! `$releasever_minor`, plus arbitrary user variables defined as files under
//! `/etc/dnf/vars/` (and legacy `/etc/yum/vars/`). Both `$var` and `${var}`
//! forms are supported, matching libdnf.

use std::collections::HashMap;
use std::path::Path;

/// Resolved substitution variables.
#[derive(Debug, Clone)]
pub struct Vars {
    map: HashMap<String, String>,
}

impl Vars {
    /// Build an empty set (useful for tests).
    pub fn empty() -> Self {
        Vars {
            map: HashMap::new(),
        }
    }

    pub fn insert(&mut self, key: impl Into<String>, val: impl Into<String>) {
        self.map.insert(key.into(), val.into());
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str)
    }

    /// Detect variables from the running system.
    ///
    /// `basearch`/`arch` come from the machine architecture; `releasever`
    /// from os-release (VERSION_ID). User vars from `/etc/dnf/vars/*` override
    /// nothing built-in but add to the set.
    pub fn detect() -> Self {
        let mut map = HashMap::new();

        let arch = detect_arch();
        map.insert("arch".to_string(), arch.clone());
        map.insert("basearch".to_string(), basearch_for(&arch).to_string());

        if let Some(rv) = detect_releasever() {
            // releasever_major/minor split on the first '.'.
            let (major, minor) = match rv.split_once('.') {
                Some((a, b)) => (a.to_string(), b.to_string()),
                None => (rv.clone(), String::new()),
            };
            map.insert("releasever".to_string(), rv);
            map.insert("releasever_major".to_string(), major);
            map.insert("releasever_minor".to_string(), minor);
        }

        for dir in ["/etc/dnf/vars", "/etc/yum/vars"] {
            load_var_dir(Path::new(dir), &mut map);
        }

        Vars { map }
    }

    /// Substitute `$var` / `${var}` occurrences in `input`.
    ///
    /// Unknown variables are left untouched (dnf behaviour), so a typo is
    /// visible in the resulting URL rather than silently becoming empty.
    pub fn expand(&self, input: &str) -> String {
        let bytes = input.as_bytes();
        let mut out = String::with_capacity(input.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'$' && i + 1 < bytes.len() {
                let (name, next) = if bytes[i + 1] == b'{' {
                    // ${name}
                    match input[i + 2..].find('}') {
                        Some(rel) => {
                            let end = i + 2 + rel;
                            (&input[i + 2..end], end + 1)
                        }
                        None => ("", i + 1), // unterminated, treat '$' literally
                    }
                } else {
                    // $name : name is [A-Za-z0-9_]+
                    let start = i + 1;
                    let mut end = start;
                    while end < bytes.len()
                        && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                    {
                        end += 1;
                    }
                    (&input[start..end], end)
                };

                if !name.is_empty() {
                    if let Some(val) = self.map.get(name) {
                        out.push_str(val);
                        i = next;
                        continue;
                    }
                    // Unknown variable: emit verbatim ($name / ${name}).
                    out.push_str(&input[i..next]);
                    i = next;
                    continue;
                }
            }
            // Not a variable start; copy the byte via char boundary safe slice.
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }
}

fn detect_arch() -> String {
    // Prefer uname machine; fall back to the compile-time target arch.
    #[cfg(unix)]
    {
        if let Ok(out) = std::process::Command::new("uname").arg("-m").output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
        }
    }
    std::env::consts::ARCH.to_string()
}

/// Map a machine arch to dnf's `basearch` (the repo-level arch family).
fn basearch_for(arch: &str) -> &'static str {
    match arch {
        "i386" | "i486" | "i586" | "i686" => "i386",
        "x86_64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        a if a.starts_with("armv") => "arm",
        "ppc64le" => "ppc64le",
        "s390x" => "s390x",
        "riscv64" => "riscv64",
        // Unknown: return the arch itself, leaked to a 'static via match arms
        // is not possible, so default to x86_64's family only when truly known.
        _ => "noarch",
    }
}

fn detect_releasever() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("VERSION_ID=") {
            let v = rest.trim().trim_matches('"').to_string();
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

fn load_var_dir(dir: &Path, map: &mut HashMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Ok(content) = std::fs::read_to_string(&path) {
            // Value is the first non-empty line, trimmed.
            let val = content
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim();
            map.insert(name.to_string(), val.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        let mut v = Vars::empty();
        v.insert("releasever", "2023");
        v.insert("basearch", "x86_64");
        v.insert("arch", "x86_64");
        v
    }

    #[test]
    fn expands_both_forms() {
        let v = vars();
        assert_eq!(
            v.expand("https://cdn/al/$releasever/$basearch/os"),
            "https://cdn/al/2023/x86_64/os"
        );
        assert_eq!(
            v.expand("https://cdn/${releasever}-${basearch}/"),
            "https://cdn/2023-x86_64/"
        );
    }

    #[test]
    fn unknown_var_is_left_verbatim() {
        let v = vars();
        assert_eq!(v.expand("a/$nope/b"), "a/$nope/b");
        assert_eq!(v.expand("a/${nope}/b"), "a/${nope}/b");
    }

    #[test]
    fn trailing_dollar_is_literal() {
        let v = vars();
        assert_eq!(v.expand("cost=5$"), "cost=5$");
    }

    #[test]
    fn basearch_mapping() {
        assert_eq!(basearch_for("i686"), "i386");
        assert_eq!(basearch_for("aarch64"), "aarch64");
        assert_eq!(basearch_for("x86_64"), "x86_64");
    }
}
