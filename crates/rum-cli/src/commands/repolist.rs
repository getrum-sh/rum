//! `rum repolist` — list configured repositories from the host dnf/yum config.

use rum_config::{Repo, RepoSource};

use crate::sys;

pub fn run(all: bool, enabled_flag: bool, disabled: bool) -> anyhow::Result<()> {
    let cfg = sys::load_config()?;

    // Selection: --all shows everything; --disabled shows only disabled;
    // default (and --enabled) shows only enabled.
    let show_all = all;
    let mut repos: Vec<&Repo> = cfg
        .repos
        .iter()
        .filter(|r| {
            if show_all {
                true
            } else if disabled {
                !r.enabled
            } else {
                // default or explicit --enabled
                let _ = enabled_flag;
                r.enabled
            }
        })
        .collect();
    repos.sort_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.id.cmp(&b.id)));

    if repos.is_empty() {
        println!("No repositories match the given filter.");
        return Ok(());
    }

    // Column widths.
    let id_w = repos.iter().map(|r| r.id.len()).max().unwrap_or(8).max(8);
    let status_w = "status".len().max("disabled".len());

    println!(
        "{:<id_w$}  {:<status_w$}  repo name",
        "repo id",
        "status",
        id_w = id_w,
        status_w = status_w,
    );
    for r in &repos {
        let status = if r.enabled { "enabled" } else { "disabled" };
        println!(
            "{:<id_w$}  {:<status_w$}  {}",
            r.id,
            status,
            r.name,
            id_w = id_w,
            status_w = status_w,
        );
    }

    if tracing::enabled!(tracing::Level::INFO) {
        for r in &repos {
            let src = match &r.source {
                RepoSource::BaseUrls(urls) => format!("baseurl={}", urls.join(", ")),
                RepoSource::MirrorList(u) => format!("mirrorlist={u}"),
                RepoSource::MetaLink(u) => format!("metalink={u}"),
            };
            tracing::info!(repo = %r.id, gpgcheck = r.gpgcheck, priority = r.priority, "{src}");
        }
    }

    Ok(())
}
