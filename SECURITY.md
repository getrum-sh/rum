# Security Policy

The `rum` project takes the security and integrity of system package management seriously. Because package managers operate with system-level privileges, correctness, cryptographic validation, and memory safety are foundational requirements.

---

## Supported Versions

Security updates are applied to the latest release on the active branch:

| Version | Supported          |
| ------- | ------------------ |
| `0.1.x` | :white_check_mark: |
| `< 0.1` | :x:                |

---

## Reporting a Vulnerability

If you believe you have discovered a security vulnerability in `rum`, please **do not** report it using a public GitHub issue.

Instead, please report it through one of the following private channels:

1. **GitHub Security Advisory (Recommended)**:  
   Navigate to the [Security tab](https://github.com/getrum-sh/rum/security/advisories) on GitHub and click **"Report a vulnerability"** to open a private advisory.

2. **Email**:  
   Send an encrypted or direct email to:  
   - [dev@getrum.sh](mailto:dev@getrum.sh)  
   - [yugal.arora@outlook.com](mailto:yugal.arora@outlook.com)

### Please Include:

- A description of the vulnerability and its potential impact.
- The version of `rum` and the operating system / architecture (e.g., Rocky Linux 9 x86_64, Amazon Linux 2023 aarch64).
- Detailed steps to reproduce the behavior, including relevant package names, repository URLs, or a minimal proof of concept.

---

## Our Response Process

1. **Acknowledgement**: We will acknowledge receipt of your vulnerability report within **48 hours**.
2. **Assessment & Confirmation**: We will investigate and confirm whether the issue represents a security vulnerability and determine its severity.
3. **Patch Development**: We will develop and test a fix across all supported operating system eras (EL8, EL9, EL10) and CPU architectures.
4. **Coordinated Disclosure**: We will coordinate with you on a disclosure timeline, credit your contribution in the release notes, and publish an updated release.

---

## Security Architecture & Scope

`rum` enforces several layers of security defense:

- **Cryptographic Verification**: All repository metadata (`repomd.xml`, `primary.xml`) and RPM payload packages are validated against cryptographic checksums (SHA-256 / SHA-384) before installation. Corrupted or altered payloads are rejected before staging.
- **Memory Safety**: `rum`'s network transport, XML parsing, binary indexing, and SAT solver algorithms are written in 100% memory-safe Rust, preventing buffer overflows, use-after-free, and related memory corruption classes common in legacy C/C++ utilities.
- **Transactional Isolation**: Actual RPM header insertion, scriptlet execution, and rpmdb mutations are committed through system `librpm`, ensuring standard SELinux policies, audit trails, and file contexts are respected.
