<div align="center">

# rum

**An extremely fast RPM package manager, written in Rust.**

A drop-in, 10–30× faster alternative to `dnf` and `yum` for enterprise Linux distributions.

[![CI](https://github.com/getrum-sh/rum/actions/workflows/ci.yml/badge.svg)](https://github.com/getrum-sh/rum/actions)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/platform-linux--x86__64%20%7C%20linux--aarch64-blue)](https://github.com/getrum-sh/rum/releases)

<p align="center">
  <a href="#highlights">Highlights</a> •
  <a href="#benchmarks">Benchmarks</a> •
  <a href="#installation">Installation</a> •
  <a href="#quickstart">Quickstart</a> •
  <a href="#features">Features</a> •
  <a href="#architecture">Architecture</a>
</p>

</div>

---

`rum` brings the speed and design philosophy of modern tools like [uv](https://github.com/astral-sh/uv) to the RPM ecosystem.

`dnf` and `yum` are written in Python and suffer from high startup latency, single-threaded metadata parsers, heavy memory fragmentation, and redundant disk roundtrips. `rum` replaces them with a single static Rust binary that starts in **10ms**, refreshes metadata **12× faster**, downloads packages concurrently over HTTP/2, uses **less than half the memory** of DNF, and commits transactions safely via native `librpm`.

Because `rum` reads your existing `dnf.conf`, `/etc/yum.repos.d/*.repo`, and the shared system RPM database directly, it is **100% coexistence-safe**: you can use `rum` and `dnf` interchangeably on the same system without desynchronization.

---

## Highlights

- ⚡ **Dramatically Fast** — Instant 10ms startup (vs 300ms+ in Python DNF). 12× faster `makecache`. 6× faster cold end-to-end installations.
- 📦 **Drop-in Compatible** — Reuses existing `/etc/dnf/dnf.conf`, `/etc/yum.repos.d/*.repo`, GPG keys, variable substitutions (`$releasever`, `$basearch`), metalinks, mirrorlists, and AWS RHUI authentication.
- 🤝 **Flawless Coexistence** — Rum never maintains its own split package database. Every transaction reads from and writes to the shared system `rpmdb` (`/var/lib/rpm`) via in-process `librpm`.
- 🪶 **Strictly Minimal Closures** — Drop soft `Recommends:` bloat with `--no-weak-deps` or `--setopt=install_weak_deps=0` (e.g. installs `ant` with 7 packages instead of DNF's 31).
- 🐳 **Container & CI Lean** — `--nodocs` skips extracting documentation and manpages, dramatically shrinking container image layers.
- 🧠 **Sub-50MB Memory Footprint** — Built with tight memory bounds and low allocation overhead. Runs reliably in RAM-constrained environments (e.g., 512MB micro instances, Kubernetes pods) where DNF frequently gets OOMKilled.
- 🧩 **Native SAT Dependency Solver** — Fast constraint resolution with full support for rich/boolean dependencies (`(A if B)`, `(A or B)`), multilib, filelists fallbacks, and erase-aware cascading uninstalls.
- 🔒 **Safe & Cryptographically Verified** — Full streaming SHA-256 / SHA-384 checksum verification on every metadata file and RPM payload, with signature checking and scriptlet isolation enforced by `librpm`.

---

## Benchmarks

Measured on an Amazon Linux 2023 EC2 instance (aarch64 / arm64), **cold cache** (caches cleared before every run) comparing `rum` directly against system `dnf`:

| Operation | `dnf` | `rum` | Speedup |
|---|---:|---:|---:|
| **Refresh metadata (`makecache`)** | 24.34 s | 1.94 s | **12.5× faster** |
| **Query available packages (`list`)** | 2.01 s | 0.35 s | **5.7× faster** |
| **Cold query (empty cache → search)** | 22.00 s | 2.30 s | **9.5× faster** |
| **Startup latency (`repolist`, warm)** | 0.30 s | 0.01 s | **30× faster** |
| **List group (`group list`, warm)** | 0.65 s | 0.05 s | **13× faster** |
| **Install package + dependencies (`git`)** | 28.30 s | 4.60 s | **6.1× faster** |
| **Peak Resident Set Size (RSS)** | 116.6 MB | 50.8 MB | **56% less memory** |

> **Why is rum so much faster?**
> 1. **Zero-Copy Metadata Cache:** Repository indices are structured for direct memory mapping, eliminating deserialization bottlenecks.
> 2. **Pipelined HTTP/2 Transfers:** Repository metadata and RPM payloads are fetched concurrently across multiplexed connection pools.
> 3. **Native SAT Engine:** Fast constraint solving in Rust without Python interpreter startup or object creation overhead.
> 4. **No Python Runtime:** Eliminates module loading and interpreter initialization on every CLI invocation.

---

## Installation

### Standalone Script (Recommended)

Install the latest release matching your CPU architecture (`x86_64` or `aarch64`) and distribution release era:

```bash
curl -fsSL https://getrum.sh/install.sh | sh
```

To install into a custom location (e.g. without `sudo`):

```bash
RUM_BINDIR=$HOME/.local/bin curl -fsSL https://getrum.sh/install.sh | sh
```

### Pre-Built Binaries

Pre-compiled release binaries are published for both **x86_64** and **aarch64 (arm64)** across RPM eras:

| Build Target | System `librpm` | Compatible Operating Systems |
|---|---|---|
| `el8` | `librpm.so.8` (rpm 4.14) | RHEL 8, Rocky Linux 8, AlmaLinux 8, Oracle Linux 8 |
| `el9` | `librpm.so.9` (rpm 4.16) | **Amazon Linux 2023**, RHEL 9, Rocky Linux 9, AlmaLinux 9, Oracle Linux 9 |
| `el10` | `librpm.so.10` (rpm 4.19) | RHEL 10, Fedora 39+, AlmaLinux 10 |

Download directly from [GitHub Releases](https://github.com/getrum-sh/rum/releases):

```bash
arch=$(uname -m)
el=$(rpm -E %rhel 2>/dev/null); case "$el" in 8|9|10) ;; *) el=9 ;; esac
curl -fsSL -O "https://github.com/getrum-sh/rum/releases/latest/download/rum-latest-${arch}-el${el}.tar.gz"
tar -xzf "rum-latest-${arch}-el${el}.tar.gz"
sudo install -m755 rum /usr/local/bin/rum
```

### Build from Source

Building requires a Rust toolchain (MSRV 1.81+) and `librpm` development headers:

```bash
# On Amazon Linux / RHEL / Fedora
sudo dnf install -y gcc rpm-devel clang

git clone https://github.com/getrum-sh/rum.git
cd rum
cargo build --release --bin rum
sudo install -m755 target/release/rum /usr/local/bin/rum
```

---

## Quickstart

`rum` provides drop-in command parity for daily package management workflows:

```bash
# Refresh metadata in parallel (~1.8s)
rum makecache

# Search and inspect packages
rum search nginx
rum info nginx
rum provides /usr/bin/htop

# Query installed and available packages
rum list installed
rum list available 'python3.11*'

# Check for updates (exit code 100 if updates exist, like dnf)
rum check-update

# Install packages with transaction preview
rum install -y nginx curl

# Lean container installs: skip documentation and manpages
rum install -y --nodocs ripgrep

# Strictly minimal closures: skip soft recommendations
rum install -y --no-weak-deps ant

# Upgrade single packages or the full system
rum upgrade -y
rum upgrade -y redis

# Dependency-aware package removal
rum remove -y nginx

# Comps package groups
rum group list
rum group install -y "Development Tools"
```

---

## Features

### 1. Strictly Minimal Closures (`--no-weak-deps`)
RPM packages often define soft dependencies (`Recommends:`). DNF pulls these recommendations in recursively by default, swelling transaction sizes.

`rum` allows you to eliminate weak dependency bloat using `--no-weak-deps` (or `--setopt=install_weak_deps=0`):

```bash
# Default: pulls in java-devel and all recommended tools (31 packages, 211 MiB)
rum download --resolve ant

# Lean: resolves only strictly required dependencies (7 packages, 203 MiB)
rum download --resolve --no-weak-deps ant
```

### 2. Container-Lean Installs (`--nodocs`)
In container builds and CI workflows, `/usr/share/doc` and `/usr/share/man` waste image space and layer caching time.

Passing `--nodocs` skips unpacking all documentation files while installing binaries and configuration intact:

```bash
rum install -y --nodocs tree
```

### 3. Ultra-Low Memory Footprint (Sub-50MB RSS)
Package managers frequently encounter memory pressure in resource-constrained environments like micro instances or Kubernetes pods with tight memory limits.

`rum` is engineered with active memory bounding and zero unnecessary heap allocations. Peak resident memory remains strictly sub-50MB even when processing hundreds of megabytes of repository metadata across multi-threaded operations, preventing OOM kills.

### 4. Lockstep Upgrades & Scoped Cascading Erase
When removing a package or upgrading interdependent libraries (e.g. `NetworkManager` and `NetworkManager-tui`), `rum` models installed packages as present candidates in the SAT solver:
- Solves dependency cascades to prevent broken leaves (`rpmdb` consistency).
- Safely pulls companion co-dependent packages into the transaction when strict version locks (`= version`) require atomic upgrade.

---

## Command Reference

| Command | Description |
|---|---|
| `rum makecache` | Concurrently download and cache repository metadata |
| `rum repolist` | List active repositories and package counts |
| `rum list <installed\|available\|updates> [glob]` | Query packages matching pattern |
| `rum info <package>` | Display package description, license, EVR, and size |
| `rum search <keywords...>` | Search package names and summaries |
| `rum provides <path\|capability>` | Find which package provides a specific file or capability |
| `rum check-update` | Check for updates (exit 100 on updates, 0 on clean) |
| `rum download [--resolve] <packages...>` | Download RPM files with optional dependency closure |
| `rum install [-y] [--nodocs] <packages...>` | Resolve, download, and commit package installation |
| `rum upgrade [-y] [packages...]` | Upgrade all packages or named subset |
| `rum remove [-y] <packages...>` | Remove packages and unneeded dependencies |
| `rum group <list\|install> [group]` | Query and install comps groups / environments |
| `rum clean <all\|metadata\|packages>` | Clear cached metadata and downloaded RPMs |

### Global Flags

- `-y`, `--assumeyes` — Automatic yes to prompts.
- `--assumeno` — Automatic no; cancels state-changing operations.
- `--nodocs` — Do not install documentation or manpages.
- `--no-weak-deps` / `--no-recommends` — Exclude soft `Recommends:` dependencies.
- `--setopt=<key>=<value>` — Override configuration options (e.g. `--setopt=install_weak_deps=0`, `--setopt=keepcache=1`).
- `-v`, `--verbose` — Increase logging verbosity (`-v`, `-vv`).
- `RUM_LOG=debug` — Tracing output filter.

---

## Architecture

`rum` is engineered as a collection of focused, modular Rust crates:

```text
crates/
├── rum-cli/       # CLI orchestration, transaction rendering, sys config
├── rum-config/    # dnf.conf / .repo INI parsing, variable expansions ($releasever)
├── rum-repo/      # Pure-Rust librepo: async HTTP/2, metalink, zero-copy mmap cache
├── rum-rpm/       # Safe librpm FFI, rpmdb read-only iterator, rpmts transaction engine
└── rum-solve/     # Native SAT solver, bit-exact rpmvercmp, rich/boolean dependencies
```

```
                          ┌───────────────────────────┐
                          │         rum CLI           │
                          └─────────────┬─────────────┘
                                        │
             ┌──────────────────────────┼──────────────────────────┐
             ▼                          ▼                          ▼
  ┌─────────────────────┐    ┌─────────────────────┐    ┌─────────────────────┐
  │     rum-config      │    │      rum-repo       │    │      rum-solve      │
  │                     │    │                     │    │                     │
  │ • dnf.conf / .repo  │    │ • Async HTTP/2      │    │ • Native SAT core   │
  │ • Variable mappings │    │ • Zero-copy cache   │    │ • Bit-exact vercmp  │
  │ • RHUI credentials  │    │ • Metalink/Mirrors  │    │ • Rich dependencies │
  └─────────────────────┘    └──────────┬──────────┘    └──────────┬──────────┘
                                        │                          │
                                        └───────────┬──────────────┘
                                                    ▼
                                         ┌─────────────────────┐
                                         │       rum-rpm       │
                                         │                     │
                                         │ • System /var/lib/rpm
                                         │ • librpm FFI engine │
                                         │ • Native rpmtsRun   │
                                         └─────────────────────┘
```

### Safety & Coexistence Guarantee
`rum` intentionally maintains **no private database** of installed packages. The single source of truth is always the system RPM database (`/var/lib/rpm`). When `rum` installs, upgrades, or erases a package, it hands execution directly to `librpm`'s `rpmtsRun()`. If an RPM scriptlet fails or a conflict is detected at commit time, `librpm` rolls back cleanly without risk of system corruption.

---

## Contributing

Contributions are welcome! To run the local test matrix and validation suite:

```bash
# Format and lint
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings

# Run all workspace unit and integration tests (80+ tests)
cargo test --workspace

# Run integration tests against configured remote hosts / containers
./tests/integration/run-local.sh --regression-only
```

---

## License

Apache-2.0. See [LICENSE](LICENSE) for details.
