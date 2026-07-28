#!/usr/bin/env python3
"""Generate projects/system/src/plugin_catalog.json from the argyle-labs org.

A repo is an orca plugin iff its root Cargo.toml declares `[package.metadata.orca]`.
This is the single source of truth: to appear in the catalog, a public repo
self-identifies by carrying that marker — no hand-maintained allowlist.

Fields per entry mirror the existing shape:
  { "name", "status", "targetSoftware", "repoUrl", "docsUrl" }
  status = "available" if the repo has any published release, else "unreleased".

Merge / safety invariant: the output is a SUPERSET of the current file. Any entry
already in the catalog that the live scan does NOT cover (e.g. a marker-less or
transiently-unreachable repo) is preserved verbatim. If any existing name would
disappear, the script exits non-zero without writing.

Usage:
  scripts/gen-plugin-catalog.py            # write the file (fails loud on drops)
  scripts/gen-plugin-catalog.py --check    # exit 1 if the file is out of date; no write
"""

from __future__ import annotations

import base64
import json
import subprocess
import sys
from pathlib import Path

ORG = "argyle-labs"
CATALOG = Path(__file__).resolve().parents[1] / "projects/system/src/plugin_catalog.json"
MARKER = "[package.metadata.orca]"


def gh(*args: str) -> str:
    """Run a gh CLI command, returning stdout (empty string on non-zero exit)."""
    r = subprocess.run(["gh", *args], capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else ""


def public_repos() -> list[str]:
    out = gh("repo", "list", ORG, "--no-archived", "--visibility", "public",
             "-L", "300", "--json", "name")
    return sorted(r["name"] for r in json.loads(out or "[]"))


def cargo_toml(repo: str) -> str | None:
    """Root Cargo.toml text, or None if absent/unreadable."""
    out = gh("api", f"repos/{ORG}/{repo}/contents/Cargo.toml", "--jq", ".content")
    if not out.strip():
        return None
    try:
        return base64.b64decode(out).decode("utf-8", "replace")
    except Exception:
        return None


def target_software(cargo: str, repo: str) -> str:
    """`target_software` under [package.metadata.orca] if set, else the repo name.

    Every existing catalog entry uses name == targetSoftware, so the repo name is
    the correct default; an explicit override is honored when present.
    """
    in_table = False
    for line in cargo.splitlines():
        s = line.strip()
        if s.startswith("["):
            in_table = s == MARKER
            continue
        if in_table and s.startswith("target_software"):
            _, _, val = s.partition("=")
            return val.strip().strip('"').strip("'") or repo
    return repo


def has_stable_release(repo: str) -> bool:
    """True iff the repo publishes a STABLE (non-prerelease) release.

    Matches the catalog's `status` semantics: "available" means installable via
    `plugin.install --name` (stable-only by default). A repo with only `-rc`
    prereleases is "unreleased" — still catalog-listed, installable only with
    `--prerelease`.
    """
    out = gh("release", "list", "--repo", f"{ORG}/{repo}",
             "--exclude-pre-releases", "-L", "1", "--json", "tagName")
    return bool(json.loads(out or "[]"))


def entry(repo: str, cargo: str) -> dict:
    return {
        "name": repo,
        "status": "available" if has_stable_release(repo) else "unreleased",
        "targetSoftware": target_software(cargo, repo),
        "repoUrl": f"https://github.com/{ORG}/{repo}",
        "docsUrl": f"https://github.com/{ORG}/{repo}#readme",
    }


def generate() -> tuple[dict, list[str], list[tuple[str, str, str]], list[str], list[str]]:
    old = json.loads(CATALOG.read_text())
    old_by_name = {p["name"]: p for p in old["plugins"]}

    generated: dict[str, dict] = {}
    for repo in public_repos():
        cargo = cargo_toml(repo)
        if cargo and MARKER in cargo:
            generated[repo] = entry(repo, cargo)

    # Merge: generated entries win; existing entries the live scan didn't cover
    # are preserved verbatim so the output is always a superset.
    merged = dict(generated)
    preserved = []
    for name, p in old_by_name.items():
        if name not in merged:
            merged[name] = p
            preserved.append(name)

    added = sorted(n for n in generated if n not in old_by_name)
    changed = [
        (n, old_by_name[n]["status"], generated[n]["status"])
        for n in sorted(generated)
        if n in old_by_name and old_by_name[n]["status"] != generated[n]["status"]
    ]
    dropped = sorted(n for n in old_by_name if n not in merged)

    out = {"comment": old["comment"],
           "plugins": [merged[n] for n in sorted(merged)]}
    return out, added, changed, preserved, dropped


def dump(obj: dict) -> str:
    return json.dumps(obj, indent=2, ensure_ascii=False) + "\n"


def main() -> int:
    check = "--check" in sys.argv[1:]
    out, added, changed, preserved, dropped = generate()

    if dropped:
        print("FATAL: these existing catalog entries would be dropped:", file=sys.stderr)
        for n in dropped:
            print(f"  - {n}", file=sys.stderr)
        return 2

    new_text = dump(out)
    if new_text == CATALOG.read_text():
        print("plugin_catalog.json is up to date.")
        return 0

    if check:
        print("plugin_catalog.json is OUT OF DATE (run scripts/gen-plugin-catalog.py).",
              file=sys.stderr)
        return 1

    CATALOG.write_text(new_text)
    print(f"Wrote {CATALOG.relative_to(Path.cwd())} ({len(out['plugins'])} plugins).")
    if added:
        print("added:   " + ", ".join(added))
    if changed:
        print("status changes:")
        for n, a, b in changed:
            print(f"  {n}: {a} -> {b}")
    if preserved:
        print("preserved (no live marker found — left as-is): " + ", ".join(preserved))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
