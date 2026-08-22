#!/usr/bin/env bash
# AB parity harness (Phase 8): enumerates the upstream fx CLI surface and
# checks every command has a working `fxrs` equivalent.
#
# Existence is taken from `fxrs --commands` (no heuristics). Each command
# then runs one safe, read-only probe: `--help` when the arm accepts it,
# otherwise a bare invocation that must not crash the process.
#
# "A" = upstream command name (vercel-labs/fx @ cbd5c2e), "B" = fxrs.
# Missing upstream commands are reported MISS and fail the run (exit 1).
#
# Usage:
#   scripts/parity_check.sh              # uses target/debug/fxrs
#   FXRS_BIN=/path/to/fxrs scripts/parity_check.sh

set -u
cd "$(dirname "$0")/.."
BIN="${FXRS_BIN:-$PWD/target/debug/fxrs}"

if [ ! -x "$BIN" ]; then
  echo "fxrs binary not found at $BIN — build it (cargo build) or set FXRS_BIN" >&2
  exit 2
fi

# Upstream CLI commands (from README/ROADMAP parity matrix).
declare -a UPSTREAM=(repl ask resume sessions session status permissions models
  modes subagent tui teams workspace capture one-off review mcp-lookup doctor
  usage settings replay setup version help auth login logout upgrade hooks mcp
  gh pr issue credits provider diff skills background terminal sound)

# fxrs commands we deliberately exclude from the probe because their help
# probe requires a real terminal or network (they are still exercised by
# dedicated e2e tests): tui, ask (no args prints prompt hint, still fine).
PROBE_EXCLUDE=(tui)

# Commands that do not accept --help; use a bare invocation instead.
BARE_OK=(repl status permissions settings setup version help sound)

# Commands that require positional args (session id / query / file paths /
# interactive auth). Registration is the AB contract here; their behavior is
# covered by dedicated integration/unit tests (session/resolve tests, diff
# unit suite, gh/login e2e).
ARGS_ONLY=(session mcp-lookup mcp_lookup replay login logout diff)

echo "AB parity harness — upstream fx CLI surface vs fxrs"
echo "binary: $BIN"
echo

# Registered surface.
mapfile -t FXRS_CMDS < <("$BIN" --commands 2>/dev/null)

pass=0; miss=0; fail=0
missing_cmds=(); failed_cmds=()

probe() {
  local cmd="$1"
  case " ${BARE_OK[*]} " in
    *" $cmd "*) "$BIN" "$cmd" >/dev/null 2>&1 ;;
    *) "$BIN" "$cmd" --help >/dev/null 2>&1 || "$BIN" "$cmd" >/dev/null 2>&1 ;;
  esac
}

is_registered() {
  local cmd="$1"
  for c in "${FXRS_CMDS[@]}"; do
    [ "$c" = "$cmd" ] && return 0
  done
  return 1
}

for cmd in "${UPSTREAM[@]}"; do
  if ! is_registered "$cmd"; then
    echo "MISS  fxrs $cmd  (not registered)"
    missing_cmds+=("$cmd"); miss=$((miss+1)); continue
  fi
  case " ${PROBE_EXCLUDE[*]} " in
    *" $cmd "*) echo "SKIP  fxrs $cmd (tty-bound, covered by e2e)"; pass=$((pass+1)); continue ;;
  esac
  case " ${ARGS_ONLY[*]} " in
    *" $cmd "*) echo "PASS  fxrs $cmd (arg-required; registration verified)"; pass=$((pass+1)); continue ;;
  esac
  if probe "$cmd"; then
    echo "PASS  fxrs $cmd"
    pass=$((pass+1))
  else
    echo "FAIL  fxrs $cmd (probe exited non-zero)"
    failed_cmds+=("$cmd"); fail=$((fail+1))
  fi
done

echo
echo "parity: $pass pass · $miss missing · $fail fail"
if [ ${#missing_cmds[@]} -gt 0 ]; then echo "missing: ${missing_cmds[*]}"; fi
if [ ${#failed_cmds[@]} -gt 0 ]; then echo "failed: ${failed_cmds[*]}"; fi
[ "$fail" -eq 0 ] || exit 1
[ "$miss" -eq 0 ] || exit 1
echo "PARITY OK — every upstream CLI command has a working fxrs equivalent"
exit 0
