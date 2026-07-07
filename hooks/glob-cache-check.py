#!/usr/bin/env python3
"""
PreToolUse:Glob hook — serve from bloodhound cache if available.

Before any Glob call in a registered project, this hook checks the registry.
If all matching entries have valid git hashes (file unchanged), it outputs
the cached paths and blocks the Glob (exit 2). If any are stale or missing,
it exits 0 and lets the Glob run (which glob-cache-write.py will then cache).

Input (stdin): JSON with tool_name, tool_input
Exit 2 = block, return cached result to model
Exit 0 = allow Glob to run
"""

import sys
import json
import os
import re
import subprocess
import fnmatch
from pathlib import Path

BRAIN = Path.home() / "brain" / "ai" / "claude" / "memory"


def get_git_hash(file_path: str) -> str:
    try:
        result = subprocess.run(
            ["git", "log", "-1", "--format=%h", "--", file_path],
            capture_output=True, text=True, timeout=3,
            cwd=os.path.dirname(file_path) or "."
        )
        return result.stdout.strip() or ""
    except Exception:
        return ""


def find_registry(search_path: str) -> tuple[Path | None, str]:
    if not BRAIN.exists():
        return None, ""
    for memory_dir in BRAIN.iterdir():
        if not memory_dir.is_dir():
            continue
        registry = memory_dir / "registry.md"
        if not registry.exists():
            continue
        content = registry.read_text()
        m = re.search(r"^root:\s*(.+)$", content, re.MULTILINE)
        if m:
            root = m.group(1).strip()
            if search_path.startswith(root) or root.startswith(search_path.rstrip("/")):
                return registry, root
    return None, ""


def parse_registry_entries(content: str, project_root: str) -> list[dict]:
    """Return list of {rel_path, abs_path, git_hash, domain} dicts."""
    entries = []
    current_domain = "misc"
    for line in content.splitlines():
        stripped = line.strip()
        if stripped.startswith("# ") and not stripped.startswith("# ---"):
            current_domain = stripped[2:].strip()
            continue
        if stripped.startswith("---") or stripped.startswith("indexed:") or stripped.startswith("root:") or not stripped:
            continue
        # Parse: rel/path [hash] — annotation  OR  rel/path [hash]  OR  rel/path
        m = re.match(r"^([^\[—\s][^\[—]*?)\s*(?:\[([a-f0-9]+)\])?\s*(?:—.*)?$", stripped)
        if m:
            rel_path = m.group(1).strip()
            git_hash = m.group(2) or ""
            abs_path = os.path.join(project_root, rel_path)
            entries.append({
                "rel_path": rel_path,
                "abs_path": abs_path,
                "git_hash": git_hash,
                "domain": current_domain,
            })
    return entries


def paths_match_pattern(entries: list[dict], pattern: str, search_path: str) -> list[dict]:
    """Filter entries whose abs_path matches the glob pattern under search_path."""
    matching = []
    for entry in entries:
        abs_path = entry["abs_path"]
        # Check path is under search_path
        if search_path and not abs_path.startswith(search_path):
            continue
        # Match against pattern (relative to search_path or absolute)
        rel_to_search = abs_path[len(search_path):].lstrip("/") if search_path else abs_path
        if fnmatch.fnmatch(rel_to_search, pattern) or fnmatch.fnmatch(abs_path, pattern):
            matching.append(entry)
    return matching


def validate_entries(entries: list[dict]) -> tuple[list[dict], bool]:
    """Check git hashes. Returns (valid_entries, all_valid)."""
    valid = []
    all_valid = True
    for entry in entries:
        if not entry["git_hash"]:
            all_valid = False
            continue
        current_hash = get_git_hash(entry["abs_path"])
        if current_hash and current_hash == entry["git_hash"]:
            valid.append(entry)
        else:
            all_valid = False
    return valid, all_valid


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        sys.exit(0)

    if data.get("tool_name") != "Glob":
        sys.exit(0)

    tool_input = data.get("tool_input", {})
    pattern = tool_input.get("pattern", "")
    search_path = tool_input.get("path", "") or os.getcwd()

    if not pattern:
        sys.exit(0)

    registry_path, project_root = find_registry(search_path)
    if not registry_path:
        sys.exit(0)  # No index for this project — let Glob run

    content = registry_path.read_text()
    entries = parse_registry_entries(content, project_root)
    matching = paths_match_pattern(entries, pattern, search_path)

    if not matching:
        sys.exit(0)  # Nothing in cache for this pattern — let Glob run

    valid_entries, all_valid = validate_entries(matching)

    if not all_valid:
        sys.exit(0)  # Some entries stale — let Glob run and re-cache

    # Full cache hit — return results and block the Glob
    paths = [e["abs_path"] for e in valid_entries]
    print(
        f"BLOODHOUND CACHE HIT — {len(paths)} results for pattern `{pattern}` in {project_root}:\n"
        + "\n".join(paths),
        file=sys.stderr
    )
    sys.exit(2)


if __name__ == "__main__":
    main()
