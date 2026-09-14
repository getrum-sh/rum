//! Tiny shell-style glob matcher for package name filters (`*` and `?`).
//!
//! dnf lets you write `rum list installed 'kernel*'` / `python3-?`. We only
//! need `*` (any run) and `?` (one char); character classes are rare in
//! package specs and omitted for now.

/// Does `text` match `pattern` where `*` matches any run and `?` one char?
pub fn matches(pattern: &str, text: &str) -> bool {
    // Fast path: no wildcards means exact (case-sensitive) compare.
    if !pattern.contains(['*', '?']) {
        return pattern == text;
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    is_match(&p, &t)
}

fn is_match(p: &[char], t: &[char]) -> bool {
    // Classic two-pointer glob with backtracking on the last '*'.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark): (Option<usize>, usize) = (None, 0);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn exact_and_wildcards() {
        assert!(matches("bash", "bash"));
        assert!(!matches("bash", "bashful"));
        assert!(matches("kernel*", "kernel-core"));
        assert!(matches("*-devel", "openssl-devel"));
        assert!(matches("python3-?", "python3-6"));
        assert!(!matches("python3-?", "python3-66"));
        assert!(matches("*", "anything"));
        assert!(matches("glib*2*", "glibc2.34"));
    }
}
