#!/usr/bin/env python3
"""
PostToolUse:Glob hook — write-through cache for bloodhound.

After any Glob call in a registered project, this hook appends new file paths
to that project's bloodhound registry. Agents then get cache hits on subsequent
lookups without calling Glob again.

Input (stdin): JSON with tool_name, tool_input, tool_response
Exit 0 always — this hook never blocks.
"""

import sys
import json
import os
import re
import subprocess
from pathlib import Path
from datetime import datetime, timezone

BRAIN = Path.home() / "brain" / "ai" / "claude" / "memory"

# Domain classification by path segment keywords
DOMAIN_RULES = [
    (["auth", "middleware", "session", "oauth", "redirect", "token"], "auth"),
    (["db", "database", "migrat", "schema", "kysely"], "database"),
    (["route", "api", "webhook", "endpoint", "handler"], "routes"),
    (["service", "services"], "services"),
    (["job", "hivemind", "background", "worker", "queue"], "jobs"),
    (["component", "ui", "widget", "layout", "page", "form"], "ui"),
    (["config", "next.config", "tailwind", "tsconfig", "eslint", ".env"], "config"),
    (["test", "spec", "fixture", "__tests__"], "tests"),
    (["type", "types", "generated", ".d.ts"], "types"),
    (["lib", "utils", "util", "helper", "shared"], "lib"),
]


def classify_domain(path_str: str) -> str:
    lower = path_str.lower()
    for keywords, domain in DOMAIN_RULES:
        if any(kw in lower for kw in keywords):
            return domain
    return "misc"


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
    """Find the registry.md whose root: covers the given search path."""
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


def parse_existing_paths(registry_content: str) -> set[str]:
    """Extract all file paths already in the registry."""
    paths = set()
    for line in registry_content.splitlines():
        line = line.strip()
        if line.startswith("#") or line.startswith("---") or not line:
            continue
        # Path is everything before the first [ or —
        m = re.match(r"^([^\[\—]+)", line)
        if m:
            paths.add(m.group(1).strip())
    return paths


def add_paths_to_registry(registry_path: Path, new_paths: list[str], project_root: str):
    """Append new paths to the registry under their domain sections."""
    content = registry_path.read_text()
    existing = parse_existing_paths(content)

    # Group new paths by domain
    by_domain: dict[str, list[str]] = {}
    for p in new_paths:
        rel = p[len(project_root):].lstrip("/") if p.startswith(project_root) else p
        if rel in existing or p in existing:
            continue
        domain = classify_domain(rel)
        by_domain.setdefault(domain, []).append((p, rel))

    if not by_domain:
        return  # Nothing new

    lines = content.splitlines()

    for domain, path_pairs in by_domain.items():
        # Find the domain section header
        section_line = None
        for i, line in enumerate(lines):
            if line.strip() == f"# {domain}":
                section_line = i
                break

        entries = []
        for abs_path, rel_path in path_pairs:
            git_hash = get_git_hash(abs_path)
            hash_str = f" [{git_hash}]" if git_hash else ""
            entries.append(f"{rel_path}{hash_str}")

        if section_line is not None:
            # Insert after the section header (find end of section)
            insert_at = section_line + 1
            while insert_at < len(lines) and not lines[insert_at].startswith("#"):
                insert_at += 1
            for i, entry in enumerate(entries):
                lines.insert(insert_at + i, entry)
        else:
            # Append new section at end
            lines.append("")
            lines.append(f"# {domain}")
            lines.extend(entries)

    registry_path.write_text("\n".join(lines) + "\n")


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        sys.exit(0)

    if data.get("tool_name") != "Glob":
        sys.exit(0)

    tool_input = data.get("tool_input", {})
    tool_response = data.get("tool_response", "")
    search_path = tool_input.get("path", "") or os.getcwd()

    # Parse file paths from response
    if isinstance(tool_response, list):
        paths = [str(p) for p in tool_response if p]
    elif isinstance(tool_response, str):
        # Could be newline-separated or JSON array embedded in string
        try:
            parsed = json.loads(tool_response)
            paths = parsed if isinstance(parsed, list) else [tool_response]
        except Exception:
            paths = [p.strip() for p in tool_response.strip().splitlines() if p.strip()]
    else:
        sys.exit(0)

    if not paths:
        sys.exit(0)

    registry_path, project_root = find_registry(search_path)
    if not registry_path:
        sys.exit(0)  # Project not indexed yet — skip silently

    # Only cache non-directory file paths
    file_paths = [p for p in paths if os.path.isfile(p) or not os.path.exists(p)]

    add_paths_to_registry(registry_path, file_paths, project_root)


if __name__ == "__main__":
    main()
