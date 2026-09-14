//! `rum search <terms...>` — match repo package name/summary against keywords.

use std::collections::BTreeSet;

use super::repo_sync;

pub fn run(terms: &[String]) -> anyhow::Result<()> {
    if terms.is_empty() {
        anyhow::bail!("`rum search` needs at least one term");
    }
    let synced = repo_sync::sync_enabled(false)?;
    let needles: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();

    // A package matches if every term appears in its name or summary
    // (dnf's default AND semantics across terms).
    let mut hits: Vec<(String, String)> = Vec::new();
    let mut seen = BTreeSet::new();
    for m in synced.metas() {
        for p in m.views() {
            let hay = format!("{} {}", p.name(), p.summary()).to_lowercase();
            if needles.iter().all(|n| hay.contains(n)) && seen.insert(p.name_arch()) {
                hits.push((p.name_arch(), p.summary().to_string()));
            }
        }
    }
    hits.sort_by(|a, b| a.0.cmp(&b.0));

    if hits.is_empty() {
        anyhow::bail!("No matches for: {}", terms.join(", "));
    }

    let name_w = hits
        .iter()
        .map(|(na, _)| na.len())
        .max()
        .unwrap_or(20)
        .max(20);
    println!("Matched Packages");
    for (na, summary) in &hits {
        let summary = summary.replace('\n', " ");
        println!("{na:<name_w$}  {summary}");
    }
    Ok(())
}
