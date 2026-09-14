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
#   RUM_EL       8 | 9 | 10                   (default: auto via rpm/os-release)
#   RUM_VERSION  release tag, e.g. v0.1.0.6   (default: latest)
set -eu

REPO="getrum-sh/rum"
BINDIR="${RUM_BINDIR:-/usr/local/bin}"

err() { echo "rum-install: $*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

have curl || err "curl is required"
have tar || err "tar is required"
SHA=""
if have sha256sum; then SHA="sha256sum -c"; elif have shasum; then SHA="shasum -a 256 -c"; fi

# --- architecture ------------------------------------------------------------
arch="${RUM_ARCH:-$(uname -m)}"
case "$arch" in
  x86_64 | amd64) arch="x86_64" ;;
  aarch64 | arm64) arch="aarch64" ;;
  *) err "unsupported architecture: $arch (need x86_64 or aarch64)" ;;
esac

# --- RPM era (librpm soname target) ------------------------------------------
# rum links a specific librpm; each RPM major is a distinct build: el8 (rpm
# 4.14), el9 (4.16), el10 (4.19). Amazon Linux 2023 is el9-era.
el="${RUM_EL:-}"
if [ -z "$el" ]; then
  el="$(rpm -E %rhel 2>/dev/null || true)"
  case "$el" in
    8 | 9 | 10) ;;
    *)
      el=""
      if [ -r /etc/os-release ]; then
        # shellcheck disable=SC1091
        . /etc/os-release
        case "${ID:-}" in
          amzn) el=9 ;; # Amazon Linux 2023 == el9 era
        esac
      fi
      [ -z "$el" ] && el=9 && echo "rum-install: could not detect RPM era; defaulting to el9 (override with RUM_EL=8|9|10)" >&2
      ;;
  esac
fi

# --- resolve the release + asset URL -----------------------------------------
if [ -n "${RUM_VERSION:-}" ]; then
  api="https://api.github.com/repos/${REPO}/releases/tags/${RUM_VERSION}"
else
  api="https://api.github.com/repos/${REPO}/releases/latest"
fi

suffix="${arch}-el${el}.tar.gz"
echo "rum-install: arch=${arch} era=el${el}; resolving release..."
json="$(curl -fsSL "$api")" || err "could not query GitHub releases API"
tag="$(printf '%s' "$json" | grep -o '"tag_name"[^,]*' | head -n1 | sed 's/.*"\([^"]*\)"[^"]*$/\1/')"
url="$(printf '%s' "$json" | grep -o "https://[^\"]*rum-[^\"]*-${suffix}" | head -n1)"
[ -n "$url" ] || err "no asset matching *-${suffix} in the release (try a different RUM_EL/RUM_ARCH)"
echo "rum-install: latest release is ${tag:-unknown}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
tarball="$tmp/$(basename "$url")"

echo "rum-install: downloading $(basename "$url")"
curl -fsSL -o "$tarball" "$url"
if [ -n "$SHA" ] && curl -fsSL -o "${tarball}.sha256" "${url}.sha256" 2>/dev/null; then
  ( cd "$tmp" && $SHA "$(basename "$tarball").sha256" >/dev/null ) \
    && echo "rum-install: checksum OK" || err "checksum verification failed"
else
  echo "rum-install: (no checksum tool or .sha256; skipping verification)" >&2
fi

tar -xzf "$tarball" -C "$tmp"
binary="$(find "$tmp" -type f -name rum -perm -u+x | head -n1)"
[ -n "$binary" ] || err "no rum binary in the downloaded archive"

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

echo "rum-install: installed $("$BINDIR/rum" --version) to $BINDIR/rum"
