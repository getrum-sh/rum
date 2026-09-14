//! `rum makecache` — refresh and cache metadata for all enabled repos.

use super::repo_sync;

pub fn run() -> anyhow::Result<()> {
    let synced = repo_sync::sync_enabled(true)?;

    let mut ok = 0usize;
    let mut failed = 0usize;
    for r in &synced.repos {
        match &r.error {
            None => {
                ok += 1;
                let origin = if r.from_cache { "cached" } else { "refreshed" };
                println!("{:<28} {} packages ({origin})", r.id, r.count);
            }
            Some(e) => {
                failed += 1;
                eprintln!("{:<28} FAILED: {e}", r.id);
            }
        }
    }

    println!(
        "\nMetadata cache created for {ok} repo(s){} in {:.2}s.",
        if failed > 0 {
            format!(" ({failed} failed)")
        } else {
            String::new()
        },
        synced.elapsed.as_secs_f64(),
    );

    if ok == 0 && failed > 0 {
        anyhow::bail!("all repositories failed to refresh");
    }
    Ok(())
}
