//! Differential-test helper: read `verA verB` per line from stdin, print
//! rum's rpmvercmp result (-1/0/1) per line. Used to cross-check against the
//! host's librpm (`rpm.labelCompare` / `rpmdev-vercmp`).

use std::cmp::Ordering;
use std::io::BufRead;

fn main() {
    let stdin = std::io::stdin();
    for line in stdin.lock().lines().map_while(Result::ok) {
        let mut it = line.split_whitespace();
        let (Some(a), Some(b)) = (it.next(), it.next()) else {
            continue;
        };
        let r = match rum_solve::rpmvercmp(a, b) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        };
        println!("{r}");
    }
}
