#!/usr/bin/env python3
"""Update known_protocols.txt from upstream Wayland protocol XML repositories.

Fetches interface names from:
  - wayland/wayland (core protocol)
  - wayland-protocols (stable, staging, unstable, experimental)
  - wlr-protocols
  - wlroots (protocol/)
  - plasma-wayland-protocols

Newly discovered upstream interfaces are reported on stderr.
"""

from __future__ import annotations

import io
import json
import sys
import xml.etree.ElementTree as ET
from pathlib import Path
from typing import Dict, List, Optional, Set, Tuple
from urllib.request import urlopen

REPO_ROOT = Path(__file__).resolve().parent.parent
PROTOCOLS_FILE = REPO_ROOT / "known_protocols.txt"

GITLAB_BASE = "https://gitlab.freedesktop.org"
GITLAB_API = f"{GITLAB_BASE}/api/v4/projects"

UPSTREAM_REPOS: Dict[str, Tuple[str, str, str, Optional[str]]] = {
    "core": ("wayland/wayland", "main", GITLAB_BASE, None),
    "wayland-protocols": ("wayland/wayland-protocols", "main", GITLAB_BASE, None),
    "wlr-protocols": ("wlroots/wlr-protocols", "master", GITLAB_BASE, None),
    "plasma-wayland-protocols": (
        "libraries/plasma-wayland-protocols",
        "master",
        "https://invent.kde.org",
        None,
    ),
    "wlroots": (
        "wlroots/wlroots",
        "master",
        GITLAB_BASE,
        "protocol",
    ),
}


def gitlab_raw(project: str, ref: str, path: str, base_url: str = GITLAB_BASE) -> str:
    url = f"{base_url}/{project}/-/raw/{ref}/{path}"
    try:
        return urlopen(url, timeout=15).read().decode("utf-8")
    except Exception as e:
        print(f"  Warning: failed to fetch {url}: {e}", file=sys.stderr)
        return ""


def gitlab_list_xml(
    project: str, ref: str, base_url: str = GITLAB_BASE, subdir: Optional[str] = None
) -> List[str]:
    """List all .xml file paths in a GitLab repo tree (handles pagination)."""
    api = f"{base_url}/api/v4/projects"
    paths: List[str] = []
    page = 1
    while True:
        url = (
            f"{api}/{project.replace('/', '%2F')}/repository/tree"
            f"?recursive=true&per_page=100&page={page}&ref={ref}"
        )
        if subdir:
            url += f"&path={subdir}"
        try:
            data = urlopen(url, timeout=15).read().decode("utf-8")
        except Exception as e:
            print(f"  Warning: failed to list tree for {project}: {e}", file=sys.stderr)
            break
        items = json.loads(data)
        if not items:
            break
        for item in items:
            path = item["path"]
            # Skip test and example XML files
            if "/tests/" in path or path.startswith("tests/"):
                continue
            if item["name"].endswith(".xml"):
                paths.append(path)
        page += 1
    return paths


def extract_interfaces(xml_content: str) -> List[str]:
    """Extract <interface name="..."> from a Wayland protocol XML."""
    try:
        root = ET.fromstring(xml_content)
        return [
            iface.get("name")
            for iface in root.findall("interface")
            if iface.get("name")
        ]
    except ET.ParseError:
        return []


def section_name(path: str) -> str:
    """Derive a section header from the XML path."""
    parts = path.split("/")
    if len(parts) >= 2:
        name = parts[-1].replace(".xml", "")
        return f"# {parts[0]}/{name}"
    return f"# {path}"


def fetch_upstream_interfaces() -> Dict[str, Set[str]]:
    """Fetch all upstream interfaces.

    Returns {section_header: {interface_names, ...}}.
    """
    result: Dict[str, Set[str]] = {}

    for repo_name, (project, ref, base_url, subdir) in UPSTREAM_REPOS.items():
        print(f"  listing xmls from {project} ({ref})...", file=sys.stderr)
        xml_paths = gitlab_list_xml(project, ref, base_url, subdir)
        print(f"    found {len(xml_paths)} xml files", file=sys.stderr)

        for xml_path in sorted(xml_paths):
            raw = gitlab_raw(project, ref, xml_path, base_url)
            if not raw:
                continue
            interfaces = extract_interfaces(raw)
            if not interfaces:
                continue
            key = section_name(xml_path)
            result.setdefault(key, set()).update(interfaces)

    return result


def parse_interfaces_from_lines(lines: List[str]) -> Set[str]:
    """Extract interface names from text lines (skip comments and blanks)."""
    result: Set[str] = set()
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("#") or not stripped:
            continue
        result.add(stripped)
    return result


def main() -> None:
    print("Fetching upstream protocol interfaces...", file=sys.stderr)
    upstream = fetch_upstream_interfaces()

    total_upstream = sum(len(v) for v in upstream.values())
    print(f"Total upstream interfaces: {total_upstream}", file=sys.stderr)

    all_upstream = set().union(*upstream.values())

    # Read previous auto-generated file
    previous_auto: Set[str] = set()
    if PROTOCOLS_FILE.exists():
        previous_auto = parse_interfaces_from_lines(
            PROTOCOLS_FILE.read_text().splitlines()
        )

    brand_new = all_upstream - previous_auto

    # Write auto-generated file
    buf = io.StringIO()

    buf.write(
        "# Code generated by scripts/update_known_protocols.py. DO NOT EDIT.\n"
        "\n"
    )

    seen: Set[str] = set()

    # Auto-generated sections from upstream (sorted by section name).
    # Deduplicated: each interface appears only in the first section it matches.
    for section in sorted(upstream):
        ifaces = sorted(upstream[section] - seen)
        if not ifaces:
            continue
        buf.write(f"{section}\n")
        for name in ifaces:
            buf.write(f"{name}\n")
        seen.update(ifaces)

    buf.write("\n")

    PROTOCOLS_FILE.write_text(buf.getvalue())

    total = len(all_upstream)
    print(f"\nWrote {PROTOCOLS_FILE}", file=sys.stderr)
    print(f"Total interfaces: {total}", file=sys.stderr)
    if brand_new:
        print(f"Newly discovered: {len(brand_new)}", file=sys.stderr)
        for name in sorted(brand_new)[:20]:
            print(f"  + {name}", file=sys.stderr)


if __name__ == "__main__":
    main()
