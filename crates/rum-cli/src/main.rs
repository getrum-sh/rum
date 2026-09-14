//! rum: a fast, parallel, Rust-native yum/dnf-compatible package manager.
//!
//! This is the v0.1 skeleton. Read/query commands that do not mutate system
//! state are implemented against the real host config; state-changing commands
//! (`install`, `remove`, `upgrade`) are stubbed and will be wired to the solver
//! and the librpm transaction layer in later milestones.

mod commands;
mod glob;
mod sys;

use clap::{Parser, Subcommand};

/// rum: Rust yum/dnf-compatible package manager.
#[derive(Parser, Debug)]
#[command(
    name = "rum",
    version = env!("RUM_VERSION_STRING"),
    about = "A fast, parallel, Rust-native yum/dnf-compatible package manager",
    long_about = None,
)]
struct Cli {
    /// Increase verbosity (-v, -vv). Overrides RUM_LOG.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Assume yes to all prompts (dnf -y).
    #[arg(short = 'y', long = "assumeyes", global = true)]
    assume_yes: bool,

    /// Do not prompt; assume no. Overrides -y for state-changing ops.
    #[arg(long = "assumeno", global = true)]
    assume_no: bool,

    /// Do not install documentation / manpages (RPMTRANS_FLAG_NODOCS).
    #[arg(long, global = true)]
    nodocs: bool,

    /// Do not install weak dependencies (Recommends:).
    #[arg(long = "no-weak-deps", alias = "no-recommends", global = true)]
    no_weak_deps: bool,

    /// Override configuration options (e.g. --setopt=install_weak_deps=0).
    #[arg(long = "setopt", global = true)]
    setopt: Vec<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List enabled/all software repositories (like `dnf repolist`).
    Repolist {
        /// Show all repos including disabled ones.
        #[arg(long)]
        all: bool,
        /// Show only enabled repos (default).
        #[arg(long)]
        enabled: bool,
        /// Show only disabled repos.
        #[arg(long)]
        disabled: bool,
    },

    /// List packages (installed / available). [planned]
    List {
        /// What to list: installed, available, or all.
        #[arg(default_value = "all")]
        what: String,
        /// Optional name globs to filter by.
        patterns: Vec<String>,
    },

    /// Show detailed information about a package. [planned]
    Info { packages: Vec<String> },

    /// Search package metadata by keyword. [planned]
    Search { terms: Vec<String> },

    /// Find which package provides a file or capability. [planned]
    Provides { spec: String },

    /// Check for available updates without installing. [planned]
    #[command(name = "check-update")]
    CheckUpdate { packages: Vec<String> },

    /// Download packages (optionally with dependencies) without installing.
    Download {
        packages: Vec<String>,
        /// Also download the full dependency closure.
        #[arg(long)]
        resolve: bool,
        /// Directory to write RPMs into (default: current directory).
        #[arg(long, default_value = ".")]
        destdir: String,
    },

    /// Refresh and cache repository metadata (like `dnf makecache`). [planned]
    Makecache,

    /// Install packages. [planned: solve now, commit via librpm later]
    Install { packages: Vec<String> },

    /// Remove packages. [planned]
    Remove { packages: Vec<String> },

    /// Manage package groups / environments (like `dnf group`).
    Group {
        #[command(subcommand)]
        action: GroupAction,
    },

    /// Upgrade packages (all, or the named ones).
    #[command(alias = "update")]
    Upgrade { packages: Vec<String> },

    /// Clean cached data (like `dnf clean`). [planned]
    Clean {
        /// What to clean: all, metadata, packages.
        #[arg(default_value = "all")]
        what: String,
    },
}

#[derive(Subcommand, Debug)]
enum GroupAction {
    /// List available groups.
    List,
    /// Install the packages in one or more groups (mandatory + default).
    Install { groups: Vec<String> },
}

fn main() -> anyhow::Result<()> {
    reset_sigpipe();
    // Cap glibc malloc arenas before any threads spawn, so the resolve heap can
    // be reclaimed before the in-process rpm transaction (keeps large installs
    // within a small host's RAM).
    sys::bound_malloc_arenas();
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    sys::set_global_overrides(&cli.setopt, cli.no_weak_deps);

    // --assumeno overrides --assumeyes for state-changing operations.
    let assume_yes = cli.assume_yes && !cli.assume_no;

    match cli.command {
        Command::Repolist {
            all,
            enabled,
            disabled,
        } => commands::repolist::run(all, enabled, disabled),
        Command::List { what, patterns } => commands::list::run(&what, &patterns),
        Command::Info { packages } => commands::info::run(&packages),
        Command::Search { terms } => commands::search::run(&terms),
        Command::Makecache => commands::makecache::run(),
        Command::CheckUpdate { packages } => commands::check_update::run(&packages),
        Command::Download {
            packages,
            resolve,
            destdir,
        } => commands::download::run(&packages, resolve, std::path::Path::new(&destdir)),
        Command::Install { packages } => commands::install::run(&packages, assume_yes, cli.nodocs),
        Command::Remove { packages } => commands::remove::run(&packages, assume_yes),
        Command::Group { action } => match action {
            GroupAction::List => commands::groups::run_list(),
            GroupAction::Install { groups } => {
                // Group install == installing each group's `@`-target.
                let targets: Vec<String> = groups
                    .iter()
                    .map(|g| {
                        if g.starts_with('@') {
                            g.clone()
                        } else {
                            format!("@{g}")
                        }
                    })
                    .collect();
                commands::install::run(&targets, assume_yes, cli.nodocs)
            }
        },
        Command::Upgrade { packages } => commands::upgrade::run(&packages, assume_yes, cli.nodocs),
        Command::Clean { what } => commands::clean::run(&what),
        other => {
            // Every other command is a recognized dnf verb we have not wired
            // up yet. Be explicit rather than silently doing nothing.
            commands::not_yet(&other)
        }
    }
}

/// Restore the default SIGPIPE disposition on Unix.
///
/// Rust sets SIGPIPE to SIG_IGN at startup, so writing to a closed pipe (e.g.
/// `rum list installed | head`) surfaces as an EPIPE write error and panics.
/// CLI tools want the classic behaviour: be terminated by SIGPIPE and exit
/// quietly (status 141). Matches what ripgrep/fd do.
#[cfg(unix)]
fn reset_sigpipe() {
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    const SIGPIPE: i32 = 13; // same value on Linux and macOS
    const SIG_DFL: usize = 0;
    // SAFETY: resetting a signal handler to the default is a well-defined,
    // async-signal-safe operation done once before any threads are spawned.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

#[cfg(not(unix))]
fn reset_sigpipe() {}

fn init_tracing(verbose: u8) {
    use tracing_subscriber::{fmt, EnvFilter};
    let default = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let filter = EnvFilter::try_from_env("RUM_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();
}

// Give `commands::not_yet` a Debug handle on the command name.
impl Command {
    fn verb(&self) -> &'static str {
        match self {
            Command::Repolist { .. } => "repolist",
            Command::List { .. } => "list",
            Command::Info { .. } => "info",
            Command::Search { .. } => "search",
            Command::Provides { .. } => "provides",
            Command::CheckUpdate { .. } => "check-update",
            Command::Download { .. } => "download",
            Command::Makecache => "makecache",
            Command::Install { .. } => "install",
            Command::Remove { .. } => "remove",
            Command::Upgrade { .. } => "upgrade",
            Command::Clean { .. } => "clean",
            Command::Group { .. } => "group",
        }
    }
}
