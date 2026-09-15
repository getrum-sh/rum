# Contributing to rum

Thank you for your interest in contributing to **rum**!

`rum` is an extremely fast, drop-in replacement for `dnf` and `yum` written in Rust. Our goal is to bring modern developer tooling performance (instant startup, sub-50MB memory footprint, parallel I/O, zero-copy metadata) to the RPM ecosystem while maintaining 100% coexistence safety with existing systems.

---

## Table of Contents

- [Core Principles](#core-principles)
- [Workspace Architecture](#workspace-architecture)
- [Development Setup](#development-setup)
- [Testing & Quality Checks](#testing--quality-checks)
- [Containerized Coexistence Testing](#containerized-coexistence-testing)
- [Pull Request Guidelines](#pull-request-guidelines)
- [Commit Message Format](#commit-message-format)
- [Reporting Issues & Security](#reporting-issues--security)

---

## Core Principles

Every contribution must align with these non-negotiable architectural tenets:

1. **Strict RPM Coexistence**  
   Rum never maintains a proprietary or divergent package database. All state-changing operations read from and commit to `/var/lib/rpm` using `librpm`. A package installed with `dnf` must be recognized by `rum`, and vice versa.

2. **Performance First**  
   Hot paths must be zero-copy where possible. Metadata caches use `rkyv` for direct memory-mapped access. String allocations in metadata parsing must be interned. Aim for $O(1)$ capability index lookups and linear SAT candidate reductions.

3. **Lean Dependency Tree**  
   Avoid heavy external dependencies. We prioritize standard library features, minimal high-performance primitives, and auditable crates.

4. **Unix & TTY Discipline**  
   All output must respect terminal state: color formatting must be disabled when stdout is not a TTY or when `NO_COLOR` is set. Piped output must handle `SIGPIPE` cleanly without panicking.

---

## Workspace Architecture

Rum is organized as a Cargo workspace with dedicated crates:

```
crates/
├── rum-config/   # Parser for /etc/dnf/dnf.conf, repo files, and variable substitution ($releasever, $basearch)
├── rum-repo/     # Repository sync, repomd.xml parsing, rkyv warm-cache storage, comps, and HTTP/2 fetching
├── rum-rpm/      # Zero-overhead, safe Rust FFI wrapper around system librpm and librpmio
├── rum-solve/    # SAT-based dependency solver (powered by resolvo), RPM EVR comparator, and rich boolean deps
└── rum-cli/      # Binary CLI interface, commands, UI styling, and transactional history logging
```

---

## Development Setup

### 1. Prerequisites

- **Rust**: Version **1.81.0** or newer (stable).
- **C Compiler & Build Tools**: `gcc` (or `clang`), `pkg-config`, `git`.
- **System RPM Libraries**:
  - **Fedora / RHEL / Rocky / AlmaLinux / Amazon Linux**:
    ```bash
    sudo dnf install -y gcc rpm-devel popt-devel openssl-devel pkgconfig
    ```
  - **Ubuntu / Debian**:
    ```bash
    sudo apt-get update && sudo apt-get install -y gcc librpm-dev libpopt-dev libssl-dev pkg-config
    ```
  - **macOS**:
    Development and query commands can run on macOS. Native RPM transaction tests require Linux with `librpm`.

### 2. Building the Project

Clone the repository and build the workspace:

```bash
git clone https://github.com/getrum-sh/rum.git
cd rum
cargo build
```

The compiled binary will be located at `target/debug/rum`.

---

## Testing & Quality Checks

Before submitting a pull request, ensure all local checks pass. Continuous Integration (CI) enforces these on all pushes:

### 1. Formatting
```bash
cargo fmt --all --check
```

### 2. Lints (Clippy)
```bash
cargo clippy --all-targets --all-features -- -D warnings
```

### 3. Unit & Integration Tests
```bash
cargo test --all --all-features
```

---

## Containerized Coexistence Testing

Because `rum` interacts directly with `librpm` and the system `rpmdb`, full integration tests run within an Enterprise Linux container environment (such as Rocky Linux 9 or Amazon Linux 2023).

You can run the coexistence suite using Docker or Podman:

```bash
docker run --rm -v "$(pwd)":/src -w /src rockylinux:9 bash -c "
  dnf install -y gcc rpm-devel popt-devel openssl-devel pkgconfig git
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
  source \$HOME/.cargo/env
  cargo test --all --all-features
"
```

---

## Pull Request Guidelines

1. **Branch Naming**: Use short, descriptive branch names like `feat/provides-command`, `fix/repo-sync-timeout`, or `perf/xml-interning`.
2. **Atomic Commits**: Keep changes scoped and easy to review. Avoid bundling unrelated refactors with functional changes.
3. **Add Tests**: Accompany new features or bug fixes with unit tests or regression tests.
4. **Documentation**: If CLI arguments, behavior, or configuration options change, update the relevant documentation and [README.md](README.md).

---

## Commit Message Format

We follow the [Conventional Commits](https://www.conventionalcommits.org/) specification:

- `feat(cli): add rum provides command for fast capability lookup`
- `fix(repo): handle HTTP 404 cleanly when syncing optional filelists`
- `perf(repo): optimize capability bucket indexing in primary.rkyv`
- `docs: update installation instructions and benchmarks`
- `test(solve): add test case for conflicting rich dependencies`

---

## Reporting Issues & Security

- **Bug Reports & Feature Requests**: Please search existing issues before opening a new one on [GitHub Issues](https://github.com/getrum-sh/rum/issues).
- **Security Vulnerabilities**: If you discover a security vulnerability, please do **not** open a public issue. Email us privately at [dev@getrum.sh](mailto:dev@getrum.sh).

---

Thank you for helping make package management on Linux blisteringly fast!
