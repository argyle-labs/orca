#!/usr/bin/env python3
"""Generate projects/system/src/plugin_catalog.json from the argyle-labs org.

A repo is an orca plugin iff its root Cargo.toml declares an orca marker table —
either `[package.metadata.orca]` (single-binary repo) or `[workspace.metadata.orca]`
(workspace repo). This is the single source of truth: to appear in the catalog, a
public repo self-identifies by carrying that marker — no hand-maintained allowlist.

Most repos ship ONE plugin artifact named after the repo. A workspace repo that
builds several independent plugin binaries (e.g. `arr` → sonarr/radarr/lidarr/
prowlarr/readarr) declares them explicitly:

  [workspace.metadata.orca]
  plugins = ["lidarr", "prowlarr", "radarr", "readarr", "sonarr"]

Each listed name becomes its own catalog entry (one entry == one release artifact
`{name}-v{version}-{triple}`), all sharing the repo's URL and release status.

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
# A repo self-identifies as a plugin by carrying either marker table. Single-package
# repos use `[package.metadata.orca]`; workspace repos use `[workspace.metadata.orca]`.
MARKERS = ("[package.metadata.orca]", "[workspace.metadata.orca]")


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


def has_marker(cargo: str) -> bool:
    return any(m in cargo for m in MARKERS)


def orca_table(cargo: str) -> dict[str, str]:
    """Raw key→value strings from the repo's orca marker table (either variant).

    Values are returned verbatim (still quoted / still `[...]`); callers parse the
    specific keys they care about. The first marker table wins.
    """
    out: dict[str, str] = {}
    in_table = False
    for line in cargo.splitlines():
        s = line.strip()
        if s.startswith("["):
            in_table = s in MARKERS
            continue
        if in_table and "=" in s and not s.startswith("#"):
            key, _, val = s.partition("=")
            out[key.strip()] = val.strip()
    return out


def plugin_names(table: dict[str, str], repo: str) -> list[str]:
    """The plugin binary names this repo publishes.

    A `plugins = ["a", "b"]` list means a workspace repo shipping several
    independent plugin binaries — one catalog entry each. Absent that, the repo
    is a single plugin named after itself.
    """
    raw = table.get("plugins")
    if not raw:
        return [repo]
    inner = raw.strip().lstrip("[").rstrip("]")
    names = [p.strip().strip('"').strip("'") for p in inner.split(",")]
    names = [n for n in names if n]
    return names or [repo]


def target_software(table: dict[str, str], name: str) -> str:
    """`target_software` override if set, else the plugin's own name."""
    val = table.get("target_software", "")
    return val.strip('"').strip("'") or name


def stable_asset_names(repo: str) -> list[str]:
    """Asset names of the repo's latest STABLE (non-prerelease) release, or [].

    Status is per-artifact, not per-repo: a workspace whose latest stable ships
    only some binaries (or, like `arr` v0.1.0, a now-defunct bundle) leaves the
    others "unreleased" until a stable release actually publishes their asset.
    """
    tag = gh("release", "list", "--repo", f"{ORG}/{repo}",
             "--exclude-pre-releases", "-L", "1", "--json", "tagName")
    tags = json.loads(tag or "[]")
    if not tags:
        return []
    out = gh("release", "view", tags[0]["tagName"], "--repo", f"{ORG}/{repo}",
             "--json", "assets", "--jq", ".assets[].name")
    return [line for line in out.splitlines() if line.strip()]


def has_stable_asset(name: str, asset_names: list[str]) -> bool:
    """True iff a stable release ships this plugin's install artifact.

    The install contract is `{name}-v{version}-{triple}[.ext]`, so a matching
    stable asset starts with `{name}-v`.
    """
    return any(a.startswith(f"{name}-v") for a in asset_names)


def entries(repo: str, cargo: str) -> list[dict]:
    """One catalog entry per plugin binary the repo publishes.

    Single-binary repos yield one entry named after the repo; a workspace repo
    with a `plugins = [...]` list yields one entry per listed binary. All entries
    from a repo share its URL and release status.
    """
    table = orca_table(cargo)
    assets = stable_asset_names(repo)
    repo_url = f"https://github.com/{ORG}/{repo}"
    docs_url = f"{repo_url}#readme"
    return [
        {
            "name": name,
            "status": "available" if has_stable_asset(name, assets) else "unreleased",
            "targetSoftware": target_software(table, name),
            "repoUrl": repo_url,
            "docsUrl": docs_url,
        }
        for name in plugin_names(table, repo)
    ]


def generate() -> tuple[dict, list[str], list[tuple[str, str, str]], list[str], list[str], list[str]]:
    old = json.loads(CATALOG.read_text())
    old_by_name = {p["name"]: p for p in old["plugins"]}

    generated: dict[str, dict] = {}
    # repoUrl of every repo the live scan actually covered — used to allow a stale
    # single-entry (e.g. old bundled `arr`) to be superseded by that same repo's
    # split per-binary entries without tripping the drop guard.
    generated_repo_urls: set[str] = set()
    for repo in public_repos():
        cargo = cargo_toml(repo)
        if cargo and has_marker(cargo):
            generated_repo_urls.add(f"https://github.com/{ORG}/{repo}")
            for e in entries(repo, cargo):
                generated[e["name"]] = e

    # Merge: generated entries win; existing entries the live scan didn't cover
    # are preserved verbatim so the output is always a superset — EXCEPT an old
    # entry whose repo WAS scanned but no longer emits that name (a repo that
    # renamed or split its plugins), which is a legitimate supersede, not a drop.
    merged = dict(generated)
    preserved = []
    superseded = []
    for name, p in old_by_name.items():
        if name in merged:
            continue
        if p.get("repoUrl") in generated_repo_urls:
            superseded.append(name)
            continue
        merged[name] = p
        preserved.append(name)

    added = sorted(n for n in generated if n not in old_by_name)
    changed = [
        (n, old_by_name[n]["status"], generated[n]["status"])
        for n in sorted(generated)
        if n in old_by_name and old_by_name[n]["status"] != generated[n]["status"]
    ]
    dropped = sorted(n for n in old_by_name if n not in merged and n not in superseded)

    out = {"comment": old["comment"],
           "plugins": [merged[n] for n in sorted(merged)]}
    return out, added, changed, preserved, dropped, superseded


def dump(obj: dict) -> str:
    return json.dumps(obj, indent=2, ensure_ascii=False) + "\n"


def main() -> int:
    check = "--check" in sys.argv[1:]
    out, added, changed, preserved, dropped, superseded = generate()

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
    if superseded:
        print("superseded (repo split/renamed its plugins — dropped): " + ", ".join(superseded))
    if preserved:
        print("preserved (no live marker found — left as-is): " + ", ".join(preserved))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
