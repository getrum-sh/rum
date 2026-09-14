//! A faithful, byte-exact port of RPM's `rpmvercmp` (lib/rpmvercmp.c).
//!
//! Correctness here is non-negotiable: the whole package manager's notion of
//! "which version is newer" rests on it, and RPM's algorithm has subtle rules
//! (leading-zero stripping, "more digits wins", numeric-beats-alpha, and the
//! `~` (sorts before) / `^` (sorts after) separators). We deliberately mirror
//! the C control flow rather than "improving" it, and validate against the
//! host's own rpm.

use std::cmp::Ordering;

#[inline]
fn is_alnum(b: u8) -> bool {
    b.is_ascii_alphanumeric()
}
#[inline]
fn is_digit(b: u8) -> bool {
    b.is_ascii_digit()
}
#[inline]
fn is_alpha(b: u8) -> bool {
    b.is_ascii_alphabetic()
}

/// Compare two version (or release) strings the way RPM does.
pub fn rpmvercmp(a: &str, b: &str) -> Ordering {
    // Fast path: identical strings.
    if a == b {
        return Ordering::Equal;
    }

    let a = a.as_bytes();
    let b = b.as_bytes();
    let (mut i, mut j) = (0usize, 0usize);

    while i < a.len() || j < b.len() {
        // Skip separators: anything that is not alnum, '~' or '^'.
        while i < a.len() && !is_alnum(a[i]) && a[i] != b'~' && a[i] != b'^' {
            i += 1;
        }
        while j < b.len() && !is_alnum(b[j]) && b[j] != b'~' && b[j] != b'^' {
            j += 1;
        }

        let ca = a.get(i).copied();
        let cb = b.get(j).copied();

        // Tilde sorts before everything, including the empty string.
        if ca == Some(b'~') || cb == Some(b'~') {
            if ca != Some(b'~') {
                return Ordering::Greater;
            }
            if cb != Some(b'~') {
                return Ordering::Less;
            }
            i += 1;
            j += 1;
            continue;
        }

        // Caret is like tilde reversed, but if one side ends here the side
        // that continues (has the caret) is the *higher* version.
        if ca == Some(b'^') || cb == Some(b'^') {
            if ca.is_none() {
                return Ordering::Less;
            }
            if cb.is_none() {
                return Ordering::Greater;
            }
            if ca != Some(b'^') {
                return Ordering::Greater;
            }
            if cb != Some(b'^') {
                return Ordering::Less;
            }
            i += 1;
            j += 1;
            continue;
        }

        // If either ran to the end, we're done segmenting.
        if !(i < a.len() && j < b.len()) {
            break;
        }

        // Grab a maximal run of one type (all-digit or all-alpha) from `a`,
        // and the corresponding run from `b`.
        let seg1_start = i;
        let seg2_start = j;
        let isnum = is_digit(a[i]);
        if isnum {
            while i < a.len() && is_digit(a[i]) {
                i += 1;
            }
            while j < b.len() && is_digit(b[j]) {
                j += 1;
            }
        } else {
            while i < a.len() && is_alpha(a[i]) {
                i += 1;
            }
            while j < b.len() && is_alpha(b[j]) {
                j += 1;
            }
        }

        let mut seg1 = &a[seg1_start..i];
        let mut seg2 = &b[seg2_start..j];

        // seg1 is guaranteed non-empty (a[seg1_start] was alnum). If seg2 is
        // empty, the two segments are of different types: numeric is newer.
        if seg2.is_empty() {
            return if isnum {
                Ordering::Greater
            } else {
                Ordering::Less
            };
        }

        if isnum {
            // Numbers: strip leading zeros, then "more digits wins".
            seg1 = strip_leading_zeros(seg1);
            seg2 = strip_leading_zeros(seg2);
            match seg1.len().cmp(&seg2.len()) {
                Ordering::Greater => return Ordering::Greater,
                Ordering::Less => return Ordering::Less,
                Ordering::Equal => {}
            }
        }

        // Same length (or alpha): lexical byte compare == C strcmp for ASCII.
        match seg1.cmp(seg2) {
            Ordering::Equal => {}
            other => return other,
        }
        // Equal segment; loop continues with i/j already past it.
    }

    // All comparable segments equal: whoever has characters left over wins.
    match (i < a.len(), j < b.len()) {
        (false, false) => Ordering::Equal,
        (false, true) => Ordering::Less,
        (true, false) => Ordering::Greater,
        // Both still have chars only if they were pure separators; treat equal.
        (true, true) => Ordering::Equal,
    }
}

fn strip_leading_zeros(seg: &[u8]) -> &[u8] {
    let mut k = 0;
    while k < seg.len() && seg[k] == b'0' {
        k += 1;
    }
    &seg[k..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    // Cases drawn from RPM's own rpmvercmp.at regression suite.
    const CASES: &[(&str, &str, Ordering)] = &[
        ("1.0", "1.0", Equal),
        ("1.0", "2.0", Less),
        ("2.0", "1.0", Greater),
        ("2.0.1", "2.0.1", Equal),
        ("2.0", "2.0.1", Less),
        ("2.0.1", "2.0", Greater),
        ("2.0.1a", "2.0.1", Greater),
        ("2.0.1", "2.0.1a", Less),
        ("5.5p1", "5.5p2", Less),
        ("5.5p2", "5.5p10", Less),
        ("5.5p10", "5.5p1", Greater),
        ("10xyz", "10.1xyz", Less),
        ("xyz10", "xyz10.1", Less),
        ("xyz.4", "8", Less), // numeric newer than alpha
        ("6.0.rc1", "6.0", Greater),
        ("6.0", "6.0.rc1", Less),
        ("10b2", "10a1", Greater),
        ("10a2", "10b2", Less),
        ("1.0aa", "1.0a", Greater),
        // leading-zero / digit-count rules
        ("10.0001", "10.1", Equal),
        ("10.0001", "10.0039", Less),
        ("4.999.9", "5.0", Less),
        ("20101121", "20101122", Less),
        // separators are all equivalent
        ("2.0", "2_0", Equal),
        ("2_0", "2.0", Equal),
        ("+", "_", Equal),
        // tilde: sorts before
        ("1.0~rc1", "1.0", Less),
        ("1.0", "1.0~rc1", Greater),
        ("1.0~rc1", "1.0~rc2", Less),
        ("1.0~rc1~git123", "1.0~rc1", Less),
        // caret: sorts after
        ("1.0^", "1.0", Greater),
        ("1.0", "1.0^", Less),
        ("1.0^git1", "1.0", Greater),
        ("1.0^git1", "1.0^git2", Less),
        ("1.0^git1", "1.01", Less),
        ("1.0^20160101", "1.0.1", Less),
        ("1.0~rc1^git1", "1.0~rc1", Greater),
        ("1.0^git1~pre", "1.0^git1", Less),
    ];

    #[test]
    fn rpm_regression_suite() {
        for (a, b, want) in CASES {
            let got = rpmvercmp(a, b);
            assert_eq!(
                got, *want,
                "rpmvercmp({a:?}, {b:?}) = {got:?}, want {want:?}"
            );
            // Antisymmetry: swapping arguments reverses the result.
            assert_eq!(
                rpmvercmp(b, a),
                want.reverse(),
                "antisymmetry for {a:?} vs {b:?}"
            );
        }
    }
}
