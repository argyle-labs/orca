#!/usr/bin/env bash
# Commit guard: fail if any REAL fleet address that was sanitized out of the
# tree ever returns. This repo mirrors to a PUBLIC GitHub, so committed files
# must never carry the real LAN range or the real Tailscale ULA prefix.
#
# Scoped ONLY to the two patterns that were fully eliminated:
#   - 10.10.10.   (real LAN range → replaced by the 10.0.0. doc range)
#   - fd7a:       (real Tailscale IPv6 ULA prefix → replaced by fd00:)
#
# Deliberately NOT gated: the 100.64.0.x Tailscale doc range (kept), and the
# functional gitea.scottkey.me / raw.githubusercontent hostnames that must
# resolve at runtime in CI.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

patterns=(
  '10\.10\.10\.'
  'fd7a:'
)

status=0
for p in "${patterns[@]}"; do
  # Exclude this guard script itself — it must name the patterns to check them.
  if hits="$(git grep -nE "$p" -- ':!scripts/check-no-real-ips.sh')"; then
    echo "ERROR: forbidden real-fleet pattern '$p' found:" >&2
    echo "$hits" >&2
    status=1
  fi
done

if [ "$status" -ne 0 ]; then
  echo >&2
  echo "Sanitize these to the doc ranges (10.0.0. / fd00:) before committing." >&2
  exit 1
fi

echo "check-no-real-ips: clean (no 10.10.10. / fd7a: in tracked tree)"
