#!/usr/bin/env bash
#
# Integration test: rum coexists with dnf/yum on the same host.
#
# rum keeps NO package database of its own — installed state is read live from
# the shared rpmdb. This test asserts that invariant end to end on a real RPM
# system: a package installed by dnf/yum is seen by rum (and not reinstalled),
# and a package installed by rum is a real, verifiable rpm transaction.
#
# Requires: root, a working dnf/yum, network access to the distro repos, and a
# `rum` binary (path via $RUM, default ./target/release/rum).
set -euo pipefail

RUM="${RUM:-./target/release/rum}"
PKG="${TEST_PKG:-tree}"   # small, dependency-light, in RHEL/Rocky AppStream

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

echo "== rum coexistence / idempotency integration test (pkg=$PKG) =="
"$RUM" --version || fail "rum binary not runnable"

# Clean slate.
dnf remove -y "$PKG" >/dev/null 2>&1 || true

# --- 1. A package installed by dnf is detected by rum -------------------------
dnf install -y "$PKG" >/dev/null 2>&1 || fail "dnf could not install $PKG"
rpm -q "$PKG" >/dev/null || fail "rpm does not see $PKG after dnf install"

"$RUM" list installed "$PKG" 2>/dev/null | grep -q "^${PKG}\." \
  || fail "rum list installed did not show dnf-installed $PKG (coexistence broken)"
pass "rum sees the dnf-installed package"

# --- 2. rum install of an already-installed package is a no-op ----------------
out="$("$RUM" install -y "$PKG" 2>&1 || true)"
echo "$out" | grep -qi "Nothing to do" \
  || fail "rum install of already-installed $PKG was not a no-op; got: $out"
pass "rum install is idempotent for a dnf-installed package"

rpm -q "$PKG" >/dev/null || fail "$PKG disappeared after rum no-op install"

# --- 3. rum can install a package (real rpm transaction) ----------------------
dnf remove -y "$PKG" >/dev/null 2>&1 || true
rpm -q "$PKG" >/dev/null 2>&1 && fail "failed to remove $PKG before rum install"

"$RUM" install -y "$PKG" >/dev/null 2>&1 || fail "rum install $PKG failed"
rpm -q "$PKG" >/dev/null || fail "rpm does not see $PKG after rum install"
pass "rum installed the package (verified via rpm -q)"

# --- 4. rum remove actually erases --------------------------------------------
"$RUM" remove -y "$PKG" >/dev/null 2>&1 || fail "rum remove $PKG failed"
rpm -q "$PKG" >/dev/null 2>&1 && fail "$PKG still present after rum remove"
pass "rum removed the package"

# --- 5. Installing a virtual CAPABILITY resolves to a real provider -----------
# Regression: `rum install vim` (vim is a capability provided by vim-enhanced,
# NOT a package name) once resolved to an unrelated, older openssh. Assert that
# installing a virtual capability actually installs a package that provides it.
CAP="${TEST_CAP:-vim}"
if dnf -q provides "$CAP" >/dev/null 2>&1; then
  dnf remove -y "$CAP" vim-enhanced >/dev/null 2>&1 || true
  out="$("$RUM" install -y "$CAP" 2>&1 || true)"
  rpm -q --whatprovides "$CAP" >/dev/null 2>&1 \
    || fail "rum install '$CAP' did not install any provider of it (got: $out)"
  pass "rum install of a virtual capability ($CAP) resolved to a real provider"
  dnf remove -y vim-enhanced >/dev/null 2>&1 || true
fi

# --- 6. rum refuses to remove a protected package -----------------------------
# rum must not let a routine `remove` take out a package the system needs to
# keep functioning (bash/glibc/systemd/rum/dnf/yum). This is a guardrail, not a
# resolver decision, so it must trigger before any rpm transaction runs.
out="$("$RUM" remove -y bash 2>&1 || true)"
echo "$out" | grep -qi "protected" \
  || fail "rum remove bash was not refused as a protected package (got: $out)"
rpm -q bash >/dev/null 2>&1 || fail "bash was removed despite protection"
pass "rum refuses to remove a protected package (bash)"

# --- 7. rum upgrade of an already up-to-date package is a no-op ---------------
dnf install -y "$PKG" >/dev/null 2>&1 || fail "dnf could not install $PKG"
out="$("$RUM" upgrade -y "$PKG" 2>&1 || true)"
echo "$out" | grep -qi -e "already up to date" -e "Nothing to do" \
  || fail "rum upgrade of up-to-date $PKG was not a clean no-op; got: $out"
pass "rum upgrade is a clean no-op when package is already up to date"
dnf remove -y "$PKG" >/dev/null 2>&1 || true

echo "== all coexistence/idempotency checks passed =="
