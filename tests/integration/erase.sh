#!/usr/bin/env bash
#
# Integration test: scoped-present, erase-aware resolution on a real RPM system.
#
# Builds a tiny synthetic repo where an available package Conflicts an installed
# one, and asserts rum does what dnf does: erase the conflicting target AND
# cascade to any dependent that would be left broken — but KEEP a dependent when
# a repo alternative can still satisfy it (the whole point of letting the SAT
# solver, not a blunt closure, decide the cascade).
#
# Requires: root, rpmbuild + createrepo_c + python3, and a `rum` binary
# (path via $RUM, default ./target/release/rum). Everything is namespaced under
# the `et-` prefix and a private repo, so it never touches the real system set.
set -uo pipefail

RUM="${RUM:-./target/release/rum}"
WORK="${WORK:-/tmp/rum-erase-test}"
REPO="$WORK/repo"
PFX=et

SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  SUDO="sudo"
fi

# Erase each named package individually: `rpm -e a b c` is atomic and aborts
# (removing nothing) if any one isn't installed, which would leave stale state.
erase_all() {
  for p in "$@"; do $SUDO rpm -e "$p" 2>/dev/null || true; done
}
SERVER_PID=""
STASH_DIR=""
cleanup() {
  [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
  erase_all "${PFX}-consumer" "${PFX}-base" "${PFX}-winner" "${PFX}-altbase" "${PFX}-upg" "${PFX}-obs" "${PFX}-old"
  $SUDO rm -f "/etc/yum.repos.d/${PFX}test.repo"
  if [ -n "$STASH_DIR" ] && [ -d "$STASH_DIR" ]; then
    for f in "$STASH_DIR"/*.repo; do
      [ -f "$f" ] && $SUDO mv "$f" /etc/yum.repos.d/
    done
  fi
}
trap cleanup EXIT

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*" >&2; cleanup; exit 1; }

# Tool prerequisite check: skip gracefully if packaging tools are missing.
for cmd in rpmbuild createrepo_c python3; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "SKIP: $cmd not installed"
    exit 0
  fi
done

# Allocate a free ephemeral port if PORT not explicitly set.
PORT="${PORT:-$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()' 2>/dev/null || echo 8099)}"

# Resolve RUM to absolute path if a relative path was given and exists.
if [[ "$RUM" == *"/"* ]] && [ -e "$RUM" ]; then
  RUM="$(cd "$(dirname "$RUM")" && pwd)/$(basename "$RUM")"
fi

if ! command -v "$RUM" >/dev/null 2>&1 && [ ! -x "$RUM" ]; then
  fail "rum binary not runnable: $RUM"
fi

echo "== rum erase-aware (scoped-present) integration test =="

$SUDO rm -rf "$WORK"
mkdir -p "$WORK/rpms" "$REPO" "$WORK/rpmbuild"/{SPECS,BUILD,RPMS,SOURCES,SRPMS}
STASH_DIR="$WORK/stashed_repos"
mkdir -p "$STASH_DIR"
# Stash existing distro repos so erase.sh operates purely on its private test repo
# (matching line 13: "never touches the real system set").
for f in /etc/yum.repos.d/*.repo; do
  if [ -f "$f" ] && [ "$(basename "$f")" != "${PFX}test.repo" ]; then
    $SUDO mv "$f" "$STASH_DIR/"
  fi
done

# Build one minimal noarch rpm: name + a single relation line (Provides/Requires/Conflicts).
build_rpm() {
  local name="$1" rel="$2" ver="${3:-1}"
  cat >"$WORK/rpmbuild/SPECS/${name}-${ver}.spec" <<EOF
Name: $name
Version: $ver
Release: 1
Summary: synthetic test package $name
License: MIT
BuildArch: noarch
$rel
%description
synthetic package for the rum erase-aware integration test
%files
EOF
  rpmbuild --define "_topdir $WORK/rpmbuild" -bb "$WORK/rpmbuild/SPECS/${name}-${ver}.spec" >/dev/null 2>&1 \
    || fail "rpmbuild $name v$ver failed"
  cp "$WORK/rpmbuild/RPMS/noarch/$name-$ver-1.noarch.rpm" "$WORK/rpms/"
}

build_rpm "${PFX}-base" "Provides: ${PFX}-bcap"       # installed target
build_rpm "${PFX}-consumer" "Requires: ${PFX}-bcap"   # installed dependent of the target's cap
build_rpm "${PFX}-winner" "Conflicts: ${PFX}-base"    # available; conflicts the target
build_rpm "${PFX}-altbase" "Provides: ${PFX}-bcap"    # available; an alternative provider of the cap

$SUDO tee "/etc/yum.repos.d/${PFX}test.repo" >/dev/null <<EOF
[${PFX}test]
name=rum erase test
baseurl=http://127.0.0.1:${PORT}/
enabled=1
gpgcheck=0
EOF

# Serve the repo over HTTP (rum fetches via its HTTP client, like a real repo).
(cd "$REPO" && python3 -m http.server "$PORT" >/dev/null 2>&1) &
SERVER_PID=$!
sleep 1
kill -0 "$SERVER_PID" 2>/dev/null || fail "http server failed to start on port $PORT"

# Publish a given set of built rpms as the repo's available packages.
publish() {
  rm -f "$REPO"/*.rpm
  for r in "$@"; do
    if [ -f "$WORK/rpms/$r.noarch.rpm" ]; then
      cp "$WORK/rpms/$r.noarch.rpm" "$REPO/"
    else
      cp "$WORK/rpms/$r-1-1.noarch.rpm" "$REPO/"
    fi
  done
  createrepo_c --quiet "$REPO" || fail "createrepo_c failed"
  $SUDO "$RUM" clean all >/dev/null 2>&1 || true
  $SUDO "$RUM" makecache >/dev/null 2>&1 || fail "rum makecache failed"
}

# Fresh install of the target + its dependent (via rpm, one transaction).
install_target_set() {
  erase_all "${PFX}-consumer" "${PFX}-base" "${PFX}-winner" "${PFX}-altbase"
  $SUDO rpm -i "$WORK/rpms/${PFX}-base-1-1.noarch.rpm" "$WORK/rpms/${PFX}-consumer-1-1.noarch.rpm" \
    || fail "seed install of base+consumer failed"
}

# --- Test A: conflict with no alternative -> target AND dependent erased ------
publish "${PFX}-winner"
install_target_set
$SUDO "$RUM" install -y "${PFX}-winner" >/dev/null || fail "rum install winner failed (A)"
rpm -q "${PFX}-winner" >/dev/null || fail "winner not installed (A)"
rpm -q "${PFX}-base" >/dev/null 2>&1 && fail "conflicting base was not erased (A)"
rpm -q "${PFX}-consumer" >/dev/null 2>&1 && fail "broken dependent consumer was not cascade-erased (A)"
pass "conflict cascade: winner installed; base + broken dependent both erased"

# --- Test B: conflict WITH a repo alternative -> target erased, dependent kept -
publish "${PFX}-winner" "${PFX}-altbase"
install_target_set
$SUDO "$RUM" install -y "${PFX}-winner" >/dev/null || fail "rum install winner failed (B)"
rpm -q "${PFX}-winner" >/dev/null || fail "winner not installed (B)"
rpm -q "${PFX}-base" >/dev/null 2>&1 && fail "conflicting base was not erased (B)"
rpm -q "${PFX}-altbase" >/dev/null || fail "repo alternative altbase was not pulled in (B)"
rpm -q "${PFX}-consumer" >/dev/null || fail "dependent consumer was wrongly erased despite an alternative (B)"
pass "conflict + alternative: base erased, altbase installed, dependent kept"

# --- Test C: upgrade-all with scoped-present erase resolution -----------------
build_rpm "${PFX}-upg" "" "1"
build_rpm "${PFX}-upg" "" "2"
build_rpm "${PFX}-old" "" "1"
build_rpm "${PFX}-obs" "Obsoletes: ${PFX}-old" "1"

erase_all "${PFX}-consumer" "${PFX}-base" "${PFX}-winner" "${PFX}-altbase" "${PFX}-upg" "${PFX}-obs" "${PFX}-old"
$SUDO rpm -i "$WORK/rpms/${PFX}-upg-1-1.noarch.rpm" "$WORK/rpms/${PFX}-old-1-1.noarch.rpm" \
  || fail "seed install of upg-1 + old-1 failed (C)"

publish "${PFX}-upg-2-1" "${PFX}-obs-1-1"
$SUDO "$RUM" upgrade -y >/dev/null || fail "rum upgrade failed (C)"
rpm -q "${PFX}-upg-2-1" >/dev/null || fail "upg was not upgraded to version 2 (C)"
rpm -q "${PFX}-obs" >/dev/null || fail "obs was not installed (C)"
rpm -q "${PFX}-old" >/dev/null 2>&1 && fail "obsoleted old was not erased by upgrade (C)"
pass "upgrade-all: version upgrade + obsoletes erase-cascade both verified"

echo "== all erase-aware resolution checks passed =="
