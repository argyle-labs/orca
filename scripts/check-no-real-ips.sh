#!/usr/bin/env bash
# Commit guard: no REAL fleet address may enter the tracked tree. This repo
# mirrors to a PUBLIC GitHub, so committed files must carry only documentation
# placeholders.
#
# Two layers:
#
#   1. FORBIDDEN patterns — the actual real ranges. These are NOT hardcoded
#      here: a guard that names the range it protects leaks that range to the
#      public mirror (the previous version of this script did exactly that).
#      They come from `$ORCA_FORBIDDEN_IP_PATTERNS` (newline- or
#      comma-separated regexes) and/or the gitignored
#      `hooks/forbidden-ip-patterns.local`, one regex per line — the same
#      convention `hooks/pii-scanner.sh` uses for personal PII patterns.
#
#   2. ALLOWLIST sweep — structural backstop that needs no configuration.
#      Finds every private/link-local/CGNAT address in the tree and fails on
#      any that is not a known documentation placeholder. This is what catches
#      a NEW real range that nobody thought to configure in layer 1.
#
# Layer 1 alone is a blocklist (only catches what you already know about);
# layer 2 alone cannot distinguish a real address from a placeholder. Together
# they cover both directions.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

status=0

# Every tracked file except this one — it must be able to name patterns.
tracked_files() {
  git ls-files -z | grep -zv '^scripts/check-no-real-ips.sh$'
}

# ── Layer 1: configured real-range patterns ──────────────────────────────────
patterns=()

# Env: newline- or comma-separated. Commas are split so CI can pass a one-liner.
if [ -n "${ORCA_FORBIDDEN_IP_PATTERNS:-}" ]; then
  while IFS= read -r line; do
    line="${line#"${line%%[![:space:]]*}"}"
    line="${line%"${line##*[![:space:]]}"}"
    [ -z "$line" ] && continue
    case "$line" in \#*) continue ;; esac
    patterns+=("$line")
  done < <(printf '%s\n' "$ORCA_FORBIDDEN_IP_PATTERNS" | tr ',' '\n')
fi

LOCAL_PATTERNS="hooks/forbidden-ip-patterns.local"
if [ -f "$LOCAL_PATTERNS" ]; then
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    case "$line" in \#*) continue ;; esac
    patterns+=("$line")
  done <"$LOCAL_PATTERNS"
fi

if [ ${#patterns[@]} -eq 0 ]; then
  echo "check-no-real-ips: no forbidden patterns configured — layer 1 skipped." >&2
  echo "  Set ORCA_FORBIDDEN_IP_PATTERNS or create $LOCAL_PATTERNS to enable it." >&2
else
  for p in "${patterns[@]}"; do
    if hits="$(tracked_files | xargs -0 grep -nHE -- "$p" 2>/dev/null)"; then
      echo "ERROR: forbidden real-fleet pattern found (matched a configured pattern):" >&2
      # Print the file:line hits but NOT the pattern — the pattern is the secret.
      echo "$hits" >&2
      status=1
    fi
  done
fi

# ── Layer 2: allowlist sweep over all private address space ──────────────────
# Anything private in the tree must be one of these. All are IANA/RFC ranges
# reserved for documentation and examples, or non-routable — safe to name here.
#
#   10.0.0.0/16         doc range this repo standardized on. A /16 (not a /24)
#                       because fixtures must be able to sit in DIFFERENT /24s
#                       from each other to exercise same-subnet logic. It is
#                       still one documented RFC1918 range, the real fleet range
#                       lies outside it, and layer 1 catches the real range by
#                       name regardless.
#   100.64.0.0/24       CGNAT doc range used for Tailscale examples
#   127.0.0.0/8         loopback
#   172.17.0.0/16       docker default bridge
#   192.0.2.0/24        TEST-NET-1     198.51.100.0/24  TEST-NET-2
#   203.0.113.0/24      TEST-NET-3
#   0.0.0.0, 255.255.255.255, 224.0.0.0/4 multicast
#   fd00:, fe80:, ::1
allow_res=(
  '^10\.0\.[0-9]{1,3}\.[0-9]{1,3}$'
  '^100\.64\.0\.[0-9]{1,3}$'
  '^127\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}$'
  '^172\.17\.[0-9]{1,3}\.[0-9]{1,3}$'
  '^192\.0\.2\.[0-9]{1,3}$'
  '^198\.51\.100\.[0-9]{1,3}$'
  '^203\.0\.113\.[0-9]{1,3}$'
  '^0\.0\.0\.0$'
  '^255\.255\.255\.255$'
  '^224\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}$'
)

is_allowed() {
  local ip=$1
  local re
  for re in "${allow_res[@]}"; do
    [[ $ip =~ $re ]] && return 0
  done
  return 1
}

# Each alternative must spell out exactly FOUR octets. Writing this as a shared
# `\.N\.N\.N` suffix after the prefixes is WRONG — `192\.168` plus three octets
# demands five, so 192.168.7.1 would sail straight through. Requiring four also
# means semver strings ("10.0.0" in Cargo.lock) don't match.
oct='[0-9]{1,3}'
private_v4="(10\.$oct\.$oct\.$oct"
private_v4+="|169\.254\.$oct\.$oct"
private_v4+="|172\.(1[6-9]|2[0-9]|3[01])\.$oct\.$oct"
private_v4+="|192\.168\.$oct\.$oct"
private_v4+="|100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.$oct\.$oct)"

offenders=""
while IFS= read -r hit; do
  [ -z "$hit" ] && continue
  # grep -oH gives `path:match`. Pass -H only — adding -h suppresses the
  # filename and the error message then names no file.
  ip="${hit##*:}"
  is_allowed "$ip" || offenders+="$hit"$'\n'
done < <(tracked_files | xargs -0 grep -oHE "$private_v4" 2>/dev/null | sort -u)

if [ -n "$offenders" ]; then
  echo "ERROR: private address in the tracked tree that is not a known placeholder:" >&2
  printf '%s' "$offenders" >&2
  echo >&2
  echo "If it is a placeholder, use the 10.0.0.0/16 doc range." >&2
  echo "If it is real, remove it — this repo mirrors to a PUBLIC GitHub." >&2
  status=1
fi

# IPv6: the real Tailscale ULA prefix is configured via layer 1. Here we only
# assert that any ULA literal uses the fd00: doc prefix. Filter on the MATCH,
# not the line — a line carrying both fd00: and a real ULA would otherwise pass.
v6_offenders=""
while IFS= read -r hit; do
  [ -z "$hit" ] && continue
  lit="fd${hit##*:fd}"
  case "$lit" in
    fd00:*) continue ;;
  esac
  v6_offenders+="$hit"$'\n'
done < <(tracked_files | xargs -0 grep -oHEi -- 'fd[0-9a-f]{2}:[0-9a-f:]*' 2>/dev/null | sort -u)

if [ -n "$v6_offenders" ]; then
  echo "ERROR: non-doc IPv6 ULA literal found (use the fd00: doc prefix):" >&2
  printf '%s' "$v6_offenders" >&2
  status=1
fi

[ "$status" -ne 0 ] && exit 1

echo "check-no-real-ips: clean (no unallowlisted private address in the tracked tree)"
