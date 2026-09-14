#!/usr/bin/env bash
#
# Benchmark: rum vs dnf/yum across key operations users experience:
# 1. metadata refresh (`makecache`) - cold
# 2. enumerating available packages (`list-available`) - warm
# 3. searching packages (`search`) - warm
# 4. dependency resolution & package download (`download --resolve`)
#
# Measures both wall-clock elapsed time AND peak resident memory (Max RSS).
#
# Machine-greppable output (compatible with run-local.sh orchestrator):
#     BENCH makecache dnf=24.06 rum=1.89 speedup=12.7x
#     BENCH list-available dnf=1.69 rum=0.61 speedup=2.8x
#     BENCH search dnf=0.75 rum=0.03 speedup=25.0x
#     BENCH download-resolve dnf=1.14 rum=1.23 speedup=0.9x
#     BENCH_RSS makecache dnf=516MB rum=103MB ratio=5.0x
#     ...
#
# Requires: a working dnf/yum, network to the distro repos, and a `rum` binary
# (path via $RUM, default ./target/release/rum). Run as root so makecache and
# clean can write the system cache.
set -euo pipefail

RUM="${RUM:-./target/release/rum}"
DNF=dnf; command -v dnf >/dev/null 2>&1 || DNF=yum
SEARCH_QUERY="${SEARCH_QUERY:-compiler}"
DL_PKG="${DL_PKG:-nginx}"

TMP_DIR="$(mktemp -d /tmp/rum-bench.XXXXXX)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

# Measure a command: outputs "<elapsed_seconds> <max_rss_kb>".
measure() {
  local timefile="$TMP_DIR/time.$$"
  rm -f "$timefile"

  if [ -x "/usr/bin/time" ]; then
    /usr/bin/time -f "%e %M" -o "$timefile" "$@" >/dev/null 2>&1 || true
    if [ -f "$timefile" ]; then
      cat "$timefile"
      rm -f "$timefile"
      return 0
    fi
  fi

  if command -v python3 >/dev/null 2>&1; then
    python3 -c '
import sys, time, subprocess, resource
s = time.time()
subprocess.run(sys.argv[1:], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
e = time.time()
ru = resource.getrusage(resource.RUSAGE_CHILDREN)
print(f"{e-s:.2f} {ru.ru_maxrss}")
' "$@" 2>/dev/null && return 0 || true
  fi

  # Fallback to date +%s.%N (RSS reported as 0)
  local s e
  s=$(date +%s.%N)
  "$@" >/dev/null 2>&1 || true
  e=$(date +%s.%N)
  awk "BEGIN{printf \"%.2f 0\", $e-$s}"
}

# dnf secs / rum secs -> "N.Nx" (guard divide-by-zero).
speedup() { awk "BEGIN{ if ($2+0>0) printf \"%.1fx\", $1/$2; else printf \"n/a\" }"; }

# Format KB to MB string
to_mb() {
  awk "BEGIN{ if ($1+0>0) printf \"%.0fMB\", $1/1024; else printf \"n/a\" }"
}

# RSS reduction ratio: dnf RSS / rum RSS -> "N.Nx"
rss_ratio() {
  awk "BEGIN{ if ($2+0>0 && $1+0>0) printf \"%.1fx\", $1/$2; else printf \"n/a\" }"
}

"$RUM" --version >/dev/null 2>&1 || { echo "BENCH error: rum not runnable: $RUM" >&2; exit 1; }

echo "== rum vs $DNF micro-benchmark =="
echo "rum: $("$RUM" --version 2>/dev/null || echo unknown)"
echo "$DNF: $("$DNF" --version 2>/dev/null | head -1 || echo unknown)"
echo

# --- 1. makecache (cold) ------------------------------------------------------
# Clear both caches before each timed run for clean-slate comparison.
"$DNF" clean all >/dev/null 2>&1 || true
"$RUM" clean all >/dev/null 2>&1 || true

d_mc_raw=$(measure "$DNF" makecache)
d_mc_time=$(echo "$d_mc_raw" | awk '{print $1}')
d_mc_rss=$(echo "$d_mc_raw" | awk '{print $2}')

"$RUM" clean all >/dev/null 2>&1 || true

r_mc_raw=$(measure "$RUM" makecache)
r_mc_time=$(echo "$r_mc_raw" | awk '{print $1}')
r_mc_rss=$(echo "$r_mc_raw" | awk '{print $2}')

# --- 2. list available (warm) -------------------------------------------------
# Both caches are warm; measures zero-copy metadata parse and string formatting.
d_ls_raw=$(measure "$DNF" list --available)
d_ls_time=$(echo "$d_ls_raw" | awk '{print $1}')
d_ls_rss=$(echo "$d_ls_raw" | awk '{print $2}')

r_ls_raw=$(measure "$RUM" list available)
r_ls_time=$(echo "$r_ls_raw" | awk '{print $1}')
r_ls_rss=$(echo "$r_ls_raw" | awk '{print $2}')

# --- 3. search (warm) ---------------------------------------------------------
# Search across names, summaries, and descriptions.
d_sr_raw=$(measure "$DNF" search "$SEARCH_QUERY")
d_sr_time=$(echo "$d_sr_raw" | awk '{print $1}')
d_sr_rss=$(echo "$d_sr_raw" | awk '{print $2}')

r_sr_raw=$(measure "$RUM" search "$SEARCH_QUERY")
r_sr_time=$(echo "$r_sr_raw" | awk '{print $1}')
r_sr_rss=$(echo "$r_sr_raw" | awk '{print $2}')

# --- 4. download --resolve (dependency resolution + RPM fetch) ----------------
dnf_dl_dir="$TMP_DIR/dnf_rpms"
rum_dl_dir="$TMP_DIR/rum_rpms"
mkdir -p "$dnf_dl_dir" "$rum_dl_dir"

if "$DNF" download --help >/dev/null 2>&1; then
  d_dl_raw=$(measure "$DNF" download --resolve --destdir "$dnf_dl_dir" "$DL_PKG")
else
  d_dl_raw=$(measure "$DNF" install -y --downloadonly --downloaddir="$dnf_dl_dir" "$DL_PKG")
fi
d_dl_time=$(echo "$d_dl_raw" | awk '{print $1}')
d_dl_rss=$(echo "$d_dl_raw" | awk '{print $2}')

r_dl_raw=$(measure "$RUM" download --resolve --destdir "$rum_dl_dir" "$DL_PKG")
r_dl_time=$(echo "$r_dl_raw" | awk '{print $1}')
r_dl_rss=$(echo "$r_dl_raw" | awk '{print $2}')

# --- Machine-greppable output lines (for run-local.sh) -----------------------
echo "BENCH makecache dnf=${d_mc_time} rum=${r_mc_time} speedup=$(speedup "$d_mc_time" "$r_mc_time")"
echo "BENCH list-available dnf=${d_ls_time} rum=${r_ls_time} speedup=$(speedup "$d_ls_time" "$r_ls_time")"
echo "BENCH search dnf=${d_sr_time} rum=${r_sr_time} speedup=$(speedup "$d_sr_time" "$r_sr_time")"
echo "BENCH download-resolve dnf=${d_dl_time} rum=${r_dl_time} speedup=$(speedup "$d_dl_time" "$r_dl_time")"

echo "BENCH_RSS makecache dnf=$(to_mb "$d_mc_rss") rum=$(to_mb "$r_mc_rss") ratio=$(rss_ratio "$d_mc_rss" "$r_mc_rss")"
echo "BENCH_RSS list-available dnf=$(to_mb "$d_ls_rss") rum=$(to_mb "$r_ls_rss") ratio=$(rss_ratio "$d_ls_rss" "$r_ls_rss")"
echo "BENCH_RSS search dnf=$(to_mb "$d_sr_rss") rum=$(to_mb "$r_sr_rss") ratio=$(rss_ratio "$d_sr_rss" "$r_sr_rss")"
echo "BENCH_RSS download-resolve dnf=$(to_mb "$d_dl_rss") rum=$(to_mb "$r_dl_rss") ratio=$(rss_ratio "$d_dl_rss" "$r_dl_rss")"

# --- Human-readable Summary Table --------------------------------------------
echo
echo "=========================================================================================="
printf "%-22s %-12s %-12s %-10s %-12s %-12s %-10s\n" "Operation" "$DNF Time" "rum Time" "Speedup" "$DNF RSS" "rum RSS" "Mem Ratio"
echo "------------------------------------------------------------------------------------------"
printf "%-22s %-12s %-12s %-10s %-12s %-12s %-10s\n" \
  "makecache (cold)" "${d_mc_time}s" "${r_mc_time}s" "$(speedup "$d_mc_time" "$r_mc_time")" \
  "$(to_mb "$d_mc_rss")" "$(to_mb "$r_mc_rss")" "$(rss_ratio "$d_mc_rss" "$r_mc_rss")"
printf "%-22s %-12s %-12s %-10s %-12s %-12s %-10s\n" \
  "list available" "${d_ls_time}s" "${r_ls_time}s" "$(speedup "$d_ls_time" "$r_ls_time")" \
  "$(to_mb "$d_ls_rss")" "$(to_mb "$r_ls_rss")" "$(rss_ratio "$d_ls_rss" "$r_ls_rss")"
printf "%-22s %-12s %-12s %-10s %-12s %-12s %-10s\n" \
  "search ($SEARCH_QUERY)" "${d_sr_time}s" "${r_sr_time}s" "$(speedup "$d_sr_time" "$r_sr_time")" \
  "$(to_mb "$d_sr_rss")" "$(to_mb "$r_sr_rss")" "$(rss_ratio "$d_sr_rss" "$r_sr_rss")"
printf "%-22s %-12s %-12s %-10s %-12s %-12s %-10s\n" \
  "download ($DL_PKG)" "${d_dl_time}s" "${r_dl_time}s" "$(speedup "$d_dl_time" "$r_dl_time")" \
  "$(to_mb "$d_dl_rss")" "$(to_mb "$r_dl_rss")" "$(rss_ratio "$d_dl_rss" "$r_dl_rss")"
echo "=========================================================================================="
