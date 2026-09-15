//! `rum history` command implementation.
//!
//! Provides transactional auditing and visibility into past package operations:
//! - `rum history [list]` shows the summary log of all transactions.
//! - `rum history info <id>` displays detailed package changes for a transaction.
//! - `rum history tree [id]` displays the hierarchical intent tree (explicit user targets vs dependencies).

use crate::history::{read_records, AlteredPackage, PackageState, TransactionRecord};
use crate::ui;

/// Run the `rum history` command.
pub fn run_list() -> anyhow::Result<()> {
    let records = read_records()?;
    if records.is_empty() {
        println!("No transaction history found.");
        return Ok(());
    }

    println!(
        "{:>6} | {:<32} | {:<20} | {:<10} | {:>7}",
        "ID", "Command line", "Date and time", "Action(s)", "Altered"
    );
    println!("{}", "-".repeat(84));

    for r in &records {
        let cmd = if r.command.len() > 32 {
            format!("{}...", &r.command[..29])
        } else {
            r.command.clone()
        };

        let time = if r.timestamp.ends_with(" UTC") {
            r.timestamp.strip_suffix(" UTC").unwrap_or(&r.timestamp)
        } else {
            &r.timestamp
        };

        println!(
            "{:>6} | {:<32} | {:<20} | {:<10} | {:>7}",
            r.id,
            cmd,
            time,
            r.action,
            r.altered.len()
        );
    }

    Ok(())
}

/// Run `rum history info [id]`. If `id` is None, inspects the latest transaction.
pub fn run_info(id: Option<u64>) -> anyhow::Result<()> {
    let records = read_records()?;
    if records.is_empty() {
        println!("No transaction history found.");
        return Ok(());
    }

    let record = match id {
        Some(target_id) => records.iter().find(|r| r.id == target_id).ok_or_else(|| {
            anyhow::anyhow!("transaction ID {target_id} not found in history")
        })?,
        None => records.last().unwrap(),
    };

    println!("Transaction ID : {}", record.id);
    println!("Begin time     : {}", record.timestamp);
    println!("Command line   : {}", record.command);
    println!("Action         : {}", record.action);
    if !record.requested.is_empty() {
        println!("Requested      : {}", record.requested.join(", "));
    }
    println!("Packages Altered:");

    for p in &record.altered {
        let tag = if p.is_explicit {
            "explicit"
        } else {
            "dependency"
        };
        let action_styled = match p.state {
            PackageState::Installed => ui::bold_green("Install"),
            PackageState::Upgraded => ui::bold_cyan("Upgrade"),
            PackageState::Removed => ui::red("Remove"),
        };
        println!("  {:8} {:<50} ({})", action_styled, p.nevra, tag);
    }

    Ok(())
}

/// Run `rum history tree [id]`.
pub fn run_tree(id: Option<u64>) -> anyhow::Result<()> {
    let records = read_records()?;
    if records.is_empty() {
        println!("No transaction history found.");
        return Ok(());
    }

    let targets: Vec<&TransactionRecord> = match id {
        Some(target_id) => {
            let r = records.iter().find(|r| r.id == target_id).ok_or_else(|| {
                anyhow::anyhow!("transaction ID {target_id} not found in history")
            })?;
            vec![r]
        }
        None => records.iter().collect(),
    };

    for record in targets {
        println!(
            "\n#{} {} ({})",
            ui::bold(&record.id.to_string()),
            record.command,
            record.timestamp
        );

        let (explicit, deps): (Vec<&AlteredPackage>, Vec<&AlteredPackage>) =
            record.altered.iter().partition(|p| p.is_explicit);

        if explicit.is_empty() {
            // All packages marked non-explicit (e.g. wildcard or upgrade-all)
            for p in &record.altered {
                print_package_line(p, "  ");
            }
            continue;
        }

        for (i, exp) in explicit.iter().enumerate() {
            print_package_line(exp, "  ");
            // If this is the last explicit package, show dependencies underneath it
            if i == explicit.len() - 1 && !deps.is_empty() {
                for (di, dep) in deps.iter().enumerate() {
                    let branch = if di == deps.len() - 1 {
                        "    └── "
                    } else {
                        "    ├── "
                    };
                    print_dep_line(dep, branch);
                }
            }
        }
    }

    Ok(())
}

fn print_package_line(p: &AlteredPackage, prefix: &str) {
    let (icon, color_nevra) = match p.state {
        PackageState::Installed => (ui::bold_green("+"), ui::green(&p.nevra)),
        PackageState::Upgraded => (ui::bold_cyan("▲"), ui::cyan(&p.nevra)),
        PackageState::Removed => (ui::red("-"), ui::red(&p.nevra)),
    };
    println!("{prefix}{icon} {color_nevra}");
}

fn print_dep_line(p: &AlteredPackage, branch: &str) {
    let (icon, color_nevra) = match p.state {
        PackageState::Installed => (ui::green("+"), p.nevra.clone()),
        PackageState::Upgraded => (ui::cyan("▲"), p.nevra.clone()),
        PackageState::Removed => (ui::red("-"), p.nevra.clone()),
    };
    println!("{branch}{icon} {color_nevra} (dep)");
}
