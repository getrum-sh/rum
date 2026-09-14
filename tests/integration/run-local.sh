#!/usr/bin/env bash
#
# Local test-matrix orchestrator: fan the regression (coexistence.sh) and the
# benchmark (bench.sh) out across every reachable test target and print ONE
# compact table. Verbose per-target output is written to log files, never
# stdout, so re-running this is cheap to read (for a human or an agent) --
# that's the whole point: one invocation, a few lines back, drill into a log
# only when something is red.
#
# Targets come from a gitignored file so this repo carries no one's private
# host inventory. Format, one target per line ("#" comments allowed):
#
#     docker:<container>[:label]
#     ssh:<host>[:label]
#
# The optional label is cosmetic (e.g. the rpmdb backend) and only shown in the
# report. Default targets file: tests/integration/targets.local (override with
# $TARGETS_FILE).
#
# Each target must already have a `rum` on its PATH (or at /usr/local/bin/rum);
# provisioning the binary per era is deliberately out of scope -- a target
# without rum is reported SKIP, not silently passed.
#
# Flags:
#   --regression-only | --no-bench   Skip the (slow) benchmark; run only the
#                                     correctness regression. This is the cheap
#                                     gate to wire into a pre-push hook or CI.
set -uo pipefail

NOBENCH=0
for a in "$@"; do
  case "$a" in
    --regression-only|--no-bench) NOBENCH=1 ;;
    *) echo "unknown flag: $a (use --regression-only)" >&2; exit 2 ;;
  esac
done

HERE="$(cd "$(dirname "$0")" && pwd)"
TARGETS_FILE="${TARGETS_FILE:-$HERE/targets.local}"
LOGDIR="${LOGDIR:-/tmp/rum-regress}"
REGRESS="$HERE/coexistence.sh"
ERASE="$HERE/erase.sh"
BENCH="$HERE/bench.sh"

[ -f "$TARGETS_FILE" ] || { echo "no targets file: $TARGETS_FILE" >&2; exit 2; }
mkdir -p "$LOGDIR"

# Resolve rum on a target, run a script there as root, capture to a log. Echoes
# the log path. Args: kind name script-local-path log.
run_on() {
  kind="$1"; name="$2"; script="$3"; log="$4"
  base="$(basename "$script")"
  case "$kind" in
    docker)
      docker cp "$script" "$name:/tmp/$base" >/dev/null 2>&1 || { echo "cp-failed" >"$log"; return 1; }
      # Containers run as root; rum is expected on PATH.
      docker exec "$name" sh -c "RUM=\$(command -v rum || echo /usr/local/bin/rum) bash /tmp/$base" >"$log" 2>&1
      return $?
      ;;
    ssh)
      scp -o ConnectTimeout=5 -q "$script" "$name:/tmp/$base" 2>>"$log" || { echo "scp-failed" >>"$log"; return 1; }
      # sudo resets PATH, so resolve rum first and pass it through explicitly.
      # -n: read stdin from /dev/null, else ssh swallows this while-read loop's
      # remaining target lines and later targets silently never run.
      ssh -n -o ConnectTimeout=5 "$name" \
        "r=\$(command -v rum || echo /usr/local/bin/rum); sudo env RUM=\$r bash /tmp/$base" >"$log" 2>&1
      return $?
      ;;
  esac
}

# One-line reachability + rum presence probe. Echoes "ok" / "unreachable" / "no-rum".
probe() {
  kind="$1"; name="$2"
  case "$kind" in
    docker)
      docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$name" || { echo unreachable; return; }
      docker exec "$name" sh -c 'command -v rum >/dev/null 2>&1 || test -x /usr/local/bin/rum' >/dev/null 2>&1 \
        && echo ok || echo no-rum ;;
    ssh)
      ssh -n -o ConnectTimeout=5 -o BatchMode=yes "$name" \
        'command -v rum >/dev/null 2>&1 || test -x /usr/local/bin/rum' >/dev/null 2>&1 \
        && echo ok || { ssh -n -o ConnectTimeout=5 -o BatchMode=yes "$name" true >/dev/null 2>&1 && echo no-rum || echo unreachable; } ;;
  esac
}

if [ "$NOBENCH" -eq 1 ]; then
  printf '%-26s %-24s %s\n' "TARGET" "REGRESSION" "BENCH"
  printf '%-26s %-24s %s\n' "------" "----------" "-----"
else
  printf '%-26s %-24s %-28s %s\n' "TARGET" "REGRESSION" "makecache(dnf->rum)" "list(dnf->rum)"
  printf '%-26s %-24s %-28s %s\n' "------" "----------" "-------------------" "--------------"
fi

overall=0
while IFS= read -r line; do
  line="${line%%#*}"; line="$(echo "$line" | tr -d '[:space:]')"
  [ -z "$line" ] && continue
  kind="$(echo "$line" | cut -d: -f1)"
  name="$(echo "$line" | cut -d: -f2)"
  label="$(echo "$line" | cut -d: -f3)"
  disp="$name${label:+ ($label)}"

  state="$(probe "$kind" "$name")"
  if [ "$state" = unreachable ]; then
    printf '%-26s %-24s %-28s %s\n' "$disp" "SKIP" "unreachable" "-"; continue
  fi
  if [ "$state" = no-rum ]; then
    printf '%-26s %-24s %-28s %s\n' "$disp" "SKIP" "no rum installed" "-"; continue
  fi

  rlog="$LOGDIR/${name}.regress.log"
  run_on "$kind" "$name" "$REGRESS" "$rlog"; rc=$?

  elog="$LOGDIR/${name}.erase.log"
  run_on "$kind" "$name" "$ERASE" "$elog"; erc=$?

  if [ "$rc" -ne 0 ]; then
    fc="$(grep -m1 '^FAIL:' "$rlog" 2>/dev/null | sed 's/^FAIL: //' | cut -c1-20)"
    reg="FAIL:${fc:-rc$rc}"; overall=1
  elif [ "$erc" -ne 0 ]; then
    efc="$(grep -m1 '^FAIL:' "$elog" 2>/dev/null | sed 's/^FAIL: //' | cut -c1-20)"
    reg="FAIL(erase):${efc:-rc$erc}"; overall=1
  else
    npass_reg="$(grep -c '^PASS:' "$rlog" 2>/dev/null || echo 0)"
    if grep -q '^SKIP:' "$elog" 2>/dev/null; then
      reg="${npass_reg}/${npass_reg} PASS (erase skip)"
    else
      npass_erase="$(grep -c '^PASS:' "$elog" 2>/dev/null || echo 0)"
      reg="${npass_reg}+${npass_erase} PASS"
    fi
  fi

  if [ "$NOBENCH" -eq 1 ]; then
    printf '%-26s %-24s %s\n' "$disp" "$reg" "(bench skipped)"; continue
  fi

  blog="$LOGDIR/${name}.bench.log"
  run_on "$kind" "$name" "$BENCH" "$blog" >/dev/null 2>&1
  mc="$(grep '^BENCH makecache' "$blog" 2>/dev/null | sed -E 's/.*dnf=([^ ]+) rum=([^ ]+) speedup=([^ ]+)/\1->\2 (\3)/' | head -1)"
  ls="$(grep '^BENCH list-available' "$blog" 2>/dev/null | sed -E 's/.*dnf=([^ ]+) rum=([^ ]+) speedup=([^ ]+)/\1->\2 (\3)/' | head -1)"
  # A container that exited mid-run (PID 1 ended, or docker stopped it) yields an
  # empty/errored bench log -- surface that explicitly rather than a silent n/a.
  if [ -z "$mc" ] && grep -q 'is not running\|No such container' "$blog" 2>/dev/null; then
    mc="container down"; ls="-"
  elif [ -z "$mc" ] && [ "$kind" = docker ] \
       && ! docker ps --format '{{.Names}}' 2>/dev/null | grep -qx "$name"; then
    mc="container down"; ls="-"
  fi
  printf '%-26s %-24s %-28s %s\n' "$disp" "$reg" "${mc:-n/a}" "${ls:-n/a}"
done < "$TARGETS_FILE"

echo
echo "logs: $LOGDIR/<target>.{regress,erase,bench}.log"
exit "$overall"
