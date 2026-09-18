#!/bin/sh
# rum installer — detects your CPU architecture and RPM era, downloads the
# matching release binary from GitHub, verifies its checksum, and installs it.
#
#   curl -fsSL https://getrum.sh/install.sh | sh
#   (or: curl -fsSL https://raw.githubusercontent.com/getrum-sh/rum/main/scripts/install.sh | sh)
#
# Environment overrides:
#   RUM_BINDIR   install directory            (default: /usr/local/bin)
#   RUM_ARCH     x86_64 | aarch64             (default: auto via uname -m)
#   RUM_EL       8 | 9 | 10                   (default: auto via soname/rpm/os-release)
#   RUM_VERSION  release tag, e.g. v0.1.0.8   (default: latest)
set -eu

REPO="getrum-sh/rum"
BINDIR="${RUM_BINDIR:-/usr/local/bin}"

err() { echo "rum-install: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

have curl || err "curl is required"

# --- distribution compatibility check ----------------------------------------
if [ -f /etc/alpine-release ] || ( have ldd && ldd --version 2>&1 | grep -iq musl ); then
  err "Alpine Linux (musl libc) is not supported; rum requires a glibc-based RPM distribution (RHEL, Rocky, AlmaLinux, Fedora, Amazon Linux, CentOS)"
fi

SHA=""
if have sha256sum; then SHA="sha256sum -c"; elif have shasum; then SHA="shasum -a 256 -c"; fi

# --- architecture ------------------------------------------------------------
arch="${RUM_ARCH:-$(uname -m)}"
case "$arch" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *) err "unsupported architecture: $arch (need x86_64 or aarch64)" ;;
esac

# --- RPM era (librpm soname probe) -------------------------------------------
# rum links against system librpm:
#   el8  -> librpm.so.8   (rpm 4.14: RHEL/Rocky/Alma/Oracle 8, Amazon Linux 2)
#   el9  -> librpm.so.9   (rpm 4.16/4.18: RHEL/Rocky/Alma/Oracle 9, Amazon Linux 2023)
#   el10 -> librpm.so.10  (rpm 4.19/4.20/6.x: Fedora 38+, RHEL/Oracle 10)
el="${RUM_EL:-}"
if [ -z "$el" ]; then
  so=""

  # 1. Probe ldconfig cache
  if have ldconfig; then
    so="$(ldconfig -p 2>/dev/null | grep -o 'librpm\.so\.[0-9]\+' | head -n 1 || true)"
  fi

  # 2. Probe common library filesystem paths
  if [ -z "$so" ]; then
    for dir in /usr/lib64 /lib64 /usr/lib /lib /usr/lib/x86_64-linux-gnu /usr/lib/aarch64-linux-gnu; do
      for f in "$dir"/librpm.so.10 "$dir"/librpm.so.9 "$dir"/librpm.so.8; do
        if [ -e "$f" ]; then
          so="$(basename "$f")"
          break 2
        fi
      done
    done
  fi

  # 3. Probe rpm --version if shared object not directly visible
  if [ -z "$so" ] && have rpm; then
    case "$(rpm --version 2>/dev/null || true)" in
      *" 4.14"*) so="librpm.so.8" ;;
      *" 4.16"*|*" 4.17"*|*" 4.18"*) so="librpm.so.9" ;;
      *" 4.19"*|*" 4.20"*|*" 6."*) so="librpm.so.10" ;;
    esac
  fi

  # 4. Fallback: inspect /etc/os-release and %rhel
  if [ -z "$so" ]; then
    rhel="$(rpm -E %rhel 2>/dev/null || true)"
    case "$rhel" in
      8|9|10) so="librpm.so.${rhel}" ;;
      *)
        if [ -r /etc/os-release ]; then
          . /etc/os-release
          case "${ID:-}" in
            fedora) so="librpm.so.10" ;;
            amzn)
              case "${VERSION_ID:-}" in
                2) so="librpm.so.8" ;;
                *) so="librpm.so.9" ;;
              esac
              ;;
            rhel|rocky|almalinux|centos|ol)
              case "${VERSION_ID:-}" in
                8*) so="librpm.so.8" ;;
                9*) so="librpm.so.9" ;;
                10*) so="librpm.so.10" ;;
              esac
              ;;
          esac
        fi
        ;;
    esac
  fi

  case "$so" in
    librpm.so.10) el="10" ;;
    librpm.so.9)  el="9" ;;
    librpm.so.8)  el="8" ;;
    *)
      el=9
      echo "rum-install: could not detect RPM era; defaulting to el9 (override with RUM_EL=8|9|10)" >&2
      ;;
  esac
fi

# --- resolve release & asset URL ---------------------------------------------
if [ -n "${RUM_VERSION:-}" ]; then
  api="https://api.github.com/repos/${REPO}/releases/tags/${RUM_VERSION}"
else
  api="https://api.github.com/repos/${REPO}/releases/latest"
fi

echo "rum-install: arch=${arch} era=el${el}; resolving release..."
json="$(curl -fsSL "$api")" || err "could not query GitHub releases API"
tag="$(printf '%s' "$json" | grep -o '"tag_name"[^,]*' | head -n1 | sed 's/.*"\([^"]*\)"[^"]*$/\1/')"
echo "rum-install: release is ${tag:-unknown}"

# Prefer standalone executable binary (requires no tar / extraction utility)
url_bin="$(printf '%s' "$json" | grep -o "https://[^\"]*rum-[^\"]*-${arch}-el${el}\"" | tr -d '"' | head -n1 || true)"
url_tar="$(printf '%s' "$json" | grep -o "https://[^\"]*rum-[^\"]*-${arch}-el${el}\.tar\.gz\"" | tr -d '"' | head -n1 || true)"

is_tarball=0
if [ -n "$url_bin" ]; then
  url="$url_bin"
elif [ -n "$url_tar" ]; then
  url="$url_tar"
  is_tarball=1
else
  err "no asset matching *-${arch}-el${el} in the release (try a different RUM_EL/RUM_ARCH)"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
asset_name="$(basename "$url")"
downloaded="$tmp/$asset_name"

echo "rum-install: downloading $asset_name"
curl -fsSL -o "$downloaded" "$url"
if [ -n "$SHA" ] && curl -fsSL -o "${downloaded}.sha256" "${url}.sha256" 2>/dev/null; then
  ( cd "$tmp" && $SHA "$asset_name.sha256" >/dev/null ) \
    && echo "rum-install: checksum OK" || err "checksum verification failed"
else
  echo "rum-install: (no checksum tool or .sha256; skipping verification)" >&2
fi

if [ "$is_tarball" = "0" ]; then
  binary="$downloaded"
  chmod +x "$binary"
else
  if ! have tar && ! have python3; then
    if have microdnf && [ "$(id -u)" = "0" ]; then
      echo "rum-install: tar is required to unpack this release archive; installing tar via microdnf..."
      microdnf install -y tar >/dev/null 2>&1 || true
    elif have dnf && [ "$(id -u)" = "0" ]; then
      echo "rum-install: tar is required to unpack this release archive; installing tar via dnf..."
      dnf install -y tar >/dev/null 2>&1 || true
    fi
  fi

  if have tar; then
    tar -xzf "$downloaded" -C "$tmp"
  elif have python3; then
    python3 -m tarfile -e "$downloaded" "$tmp"
  else
    err "tar or python3 is required to unpack this release archive"
  fi

  binary=""
  for f in "$tmp"/rum "$tmp"/*/rum "$tmp"/*/*/rum; do
    if [ -f "$f" ]; then
      binary="$f"
      break
    fi
  done
  [ -n "$binary" ] || err "no rum binary found in the downloaded archive"
  chmod +x "$binary"
fi

# --- install (sudo only if needed) -------------------------------------------
install_cmd="install -m 0755"
if [ -w "$BINDIR" ] || [ "$(id -u)" = "0" ]; then
  $install_cmd "$binary" "$BINDIR/rum"
elif have sudo; then
  echo "rum-install: installing to $BINDIR (needs sudo)"
  sudo $install_cmd "$binary" "$BINDIR/rum"
else
  err "cannot write to $BINDIR and sudo is unavailable; set RUM_BINDIR to a writable dir"
fi

# --- verification & dynamic linker smoke test --------------------------------
if ! ver="$("$BINDIR/rum" --version 2>&1)"; then
  echo "rum-install: error: installed binary failed verification or dynamic linker checks:" >&2
  echo "$ver" >&2
  rm -f "$BINDIR/rum"
  exit 1
fi

echo "rum-install: verified $ver"
echo "rum-install: successfully installed to $BINDIR/rum"
