//! Minimal INI parser for dnf/yum config files.
//!
//! dnf.conf and .repo files are INI-ish: `[section]` headers followed by
//! `key=value` lines. We keep this deliberately small rather than pulling a
//! crate, because the format is simple and we need a few dnf-specific quirks:
//!   * comments start with `#` or `;`
//!   * values may continue onto indented following lines (used by e.g.
//!     `baseurl` / `gpgkey` lists and long `exclude` lines)
//!   * duplicate keys: last one wins (matches dnf/libdnf behaviour)

use std::collections::BTreeMap;

/// A parsed INI document: ordered sections, each a map of key -> value.
#[derive(Debug, Default, Clone)]
pub struct Ini {
    /// Sections in the order they appear. The section name for the
    /// implicit pre-header region (rare in dnf configs) is the empty string.
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub entries: BTreeMap<String, String>,
}

impl Ini {
    pub fn parse(text: &str) -> Self {
        let mut sections: Vec<Section> = Vec::new();
        let mut current: Option<Section> = None;
        // Track the last key so continuation lines can append to it.
        let mut last_key: Option<String> = None;

        for raw in text.lines() {
            let line = raw.trim_end();
            let trimmed = line.trim_start();

            // Blank line or comment.
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                continue;
            }

            // Section header.
            if trimmed.starts_with('[') {
                if let Some(end) = trimmed.find(']') {
                    if let Some(sec) = current.take() {
                        sections.push(sec);
                    }
                    let name = trimmed[1..end].trim().to_string();
                    current = Some(Section {
                        name,
                        entries: BTreeMap::new(),
                    });
                    last_key = None;
                    continue;
                }
            }

            // Continuation line: original line is indented and we have a key.
            let is_indented = line.starts_with(' ') || line.starts_with('\t');
            if is_indented && !trimmed.contains('=') {
                if let (Some(sec), Some(key)) = (current.as_mut(), last_key.as_ref()) {
                    if let Some(val) = sec.entries.get_mut(key) {
                        val.push(' ');
                        val.push_str(trimmed);
                    }
                    continue;
                }
            }

            // key=value.
            if let Some(eq) = trimmed.find('=') {
                let key = trimmed[..eq].trim().to_ascii_lowercase();
                let val = trimmed[eq + 1..].trim().to_string();
                if let Some(sec) = current.as_mut() {
                    sec.entries.insert(key.clone(), val);
                    last_key = Some(key);
                }
            }
        }

        if let Some(sec) = current.take() {
            sections.push(sec);
        }
        Ini { sections }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_and_continuations() {
        let text = "\
[main]
gpgcheck=1
# a comment
best = True

[baseos]
name=Base OS $releasever
baseurl=https://a/repo
    https://b/repo
enabled=1
";
        let ini = Ini::parse(text);
        assert_eq!(ini.sections.len(), 2);
        assert_eq!(ini.sections[0].name, "main");
        assert_eq!(ini.sections[0].entries.get("gpgcheck").unwrap(), "1");
        assert_eq!(ini.sections[0].entries.get("best").unwrap(), "True");

        let baseos = &ini.sections[1];
        assert_eq!(baseos.name, "baseos");
        assert_eq!(
            baseos.entries.get("baseurl").unwrap(),
            "https://a/repo https://b/repo"
        );
    }

    #[test]
    fn last_duplicate_key_wins() {
        let ini = Ini::parse("[main]\ngpgcheck=0\ngpgcheck=1\n");
        assert_eq!(ini.sections[0].entries.get("gpgcheck").unwrap(), "1");
    }
}
