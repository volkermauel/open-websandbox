#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 the open-websandbox contributors
# SPDX-License-Identifier: AGPL-3.0-only

"""Workbench toolchain manifest generator (stdlib json only).

Three consumers, one source of truth (``rust/runtime/tools.json``):

* ``--print-apt`` / ``--print-pip``: the package lists the Dockerfile expands
  into ``apt-get install --no-install-recommends`` / system-wide pip installs,
  so the image contents and the manifest can never drift.
* default (``--output``): the LLM capability manifest
  (``/usr/local/share/sandbox-capabilities.md`` in the image) — a compact,
  **path-free** per-area ``name — version`` inventory probed at build time,
  with the build-time dpkg base count baked into the footer. Workspace-path
  recipes deliberately live in the runtime's Rust-built "Workspace
  conventions" section (``rust/runtime/src/system.rs``), not here: this file
  is static at build time while the workspace root (WORKDIR) varies per
  deployment.
* ``--self-test``: schema + rendering invariants (no cluster, no image
  needed) — run by tests/e2e/test_toolchain.py and locally.
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

VERSION_RE = re.compile(r"\d+(?:\.\d+)+")
PROBE_TIMEOUT_SECS = 15

# Version probes per area. Tolerant of absence: a tool that is missing or
# prints no version is simply listed without one. Keys that match a package
# in tools.json annotate that package's version in the inventory line;
# tarball/tools.json-external entries (Node, pwsh, dotnet, duckdb, cfr,
# ilspycmd, soffice) append to their area's line.
PROBES: dict[str, list[tuple[str, list[str], str | None]]] = {
    "Archives": [
        ("gzip", ["gzip", "--version"], "gzip"),
        ("7z", ["7z"], "p7zip-full"),
        ("unar", ["unar"], "unar"),
        ("unrar", ["unrar"], "unrar"),
    ],
    "General CLI": [
        ("git", ["git", "--version"], "git"),
        ("curl", ["curl", "--version"], "curl"),
        ("jq", ["jq", "--version"], "jq"),
        ("rg", ["rg", "--version"], "ripgrep"),
        ("fd", ["fdfind", "--version"], "fd-find"),
        ("shellcheck", ["shellcheck", "--version"], "shellcheck"),
    ],
    "Python and data": [
        ("python3", ["python3", "--version"], None),
        ("pandas", ["python3", "-c", "import pandas; print(pandas.__version__)"], "pandas"),
        ("numpy", ["python3", "-c", "import numpy; print(numpy.__version__)"], "numpy"),
    ],
    "R": [
        ("R", ["R", "--version"], "r-base-core"),
    ],
    "Docs depth": [
        ("pandoc", ["pandoc", "--version"], "pandoc"),
        ("soffice", ["soffice", "--version"], None),
        ("qpdf", ["qpdf", "--version"], "qpdf"),
        ("tesseract", ["tesseract", "--version"], "tesseract-ocr"),
    ],
    "Windows packaging": [
        ("wixl", ["wixl", "--version"], "msitools"),
        ("makensis", ["makensis", "-VERSION"], "makensis"),
        ("pwsh", ["pwsh", "--version"], None),
        ("dotnet", ["dotnet", "--version"], None),
        ("ilspycmd", ["ilspycmd", "--version"], None),
    ],
    "Reverse engineering light": [
        ("gdb", ["gdb", "--version"], "gdb"),
        ("radare2", ["radare2", "-v"], "radare2"),
        ("java", ["java", "--version"], "default-jdk-headless"),
        ("cfr", ["cfr"], None),
    ],
    "DB clients": [
        ("sqlite3", ["sqlite3", "--version"], "sqlite3"),
        ("duckdb", ["duckdb", "--version"], None),
    ],
}

# Probe-only areas not present in tools.json (tarball-distributed).
EXTRA_AREAS: list[tuple[str, list[tuple[str, list[str], None]]]] = [
    (
        "Node (npm; corepack: pnpm, yarn)",
        [("node", ["node", "--version"], None), ("npm", ["npm", "--version"], None)],
    ),
]

FOOTER_FINAL = (
    "Live state may differ after package installs — run sandbox-tools "
    "(or apt list --installed) to check."
)


def load_tools(path: Path) -> dict:
    with path.open(encoding="utf-8") as fh:
        return json.load(fh)


def validate_tools(doc: dict) -> list[str]:
    """Return a list of schema violations (empty = valid)."""
    errs: list[str] = []
    areas = doc.get("areas")
    if not isinstance(areas, list) or not areas:
        return ["tools.json: 'areas' must be a non-empty list"]
    seen: set[str] = set()
    for i, area in enumerate(areas):
        if not isinstance(area, dict):
            errs.append(f"areas[{i}]: not an object")
            continue
        name = area.get("name")
        if not isinstance(name, str) or not name.strip():
            errs.append(f"areas[{i}]: missing/empty 'name'")
        elif name in seen:
            errs.append(f"areas[{i}]: duplicate area name {name!r}")
        else:
            seen.add(name)
        for key in ("apt", "pip"):
            pkgs = area.get(key, [])
            if not isinstance(pkgs, list) or any(
                not isinstance(p, str) or not p.strip() for p in pkgs
            ):
                errs.append(f"areas[{i}] ({name!r}): '{key}' must be a list of non-empty strings")
    return errs


def apt_packages(doc: dict) -> list[str]:
    """Deduplicated apt package list across areas, in manifest order."""
    out: list[str] = []
    for area in doc["areas"]:
        for p in area.get("apt", []):
            if p not in out:
                out.append(p)
    return out


def pip_packages(doc: dict) -> list[str]:
    out: list[str] = []
    for area in doc["areas"]:
        for p in area.get("pip", []):
            if p not in out:
                out.append(p)
    return out


def probe_version(argv: list[str]) -> str | None:
    """Run a version probe; return the first dotted number or None."""
    try:
        proc = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=PROBE_TIMEOUT_SECS,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    text = (proc.stdout or "") + "\n" + (proc.stderr or "")
    match = VERSION_RE.search(text)
    return match.group(0) if match else None


def dpkg_count() -> int | None:
    try:
        proc = subprocess.run(
            ["dpkg-query", "-W", "-f", "${db:Status-Abbrev} ${binary:Package}\n"],
            capture_output=True,
            text=True,
            timeout=PROBE_TIMEOUT_SECS,
            check=False,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    if proc.returncode != 0:
        return None
    # db:Status-Abbrev 'ii ' = installed; skip blank names.
    installed = [
        ln.split(" ", 1)[-1]
        for ln in proc.stdout.splitlines()
        if ln.startswith("ii ") and ln.split(" ", 1)[-1].strip()
    ]
    return len(installed) or None


def render_manifest(doc: dict) -> str:
    """Render the path-free capability manifest (~15 lines, well under 500 tokens)."""
    lines: list[str] = [
        "Toolchain baked into this base image, by area:",
        "",
    ]
    for area in list(doc["areas"]) + [
        {"name": name, "apt": [], "pip": []} for name, _ in EXTRA_AREAS
    ]:
        probes = PROBES.get(area["name"]) or dict(EXTRA_AREAS).get(area["name"]) or []
        versions: dict[str, str | None] = {label: probe_version(argv) for label, argv, _ in probes}
        items: list[str] = []
        covered: set[str] = set()
        for label, _, pkg in probes:
            if pkg:
                covered.add(pkg)
            version = versions.get(label)
            items.append(f"{label} {version}" if version else label)
        for pkg in area.get("apt", []):
            if pkg not in covered:
                items.append(pkg)
        pip_items = [
            f"{p} {versions[p]}" if versions.get(p) else p for p in area.get("pip", [])
        ]
        if pip_items:
            items.append("pip: " + ", ".join(pip_items))
        lines.append(f"- {area['name']} — " + ", ".join(items))
    if shutil.which("sudo"):
        lines.append(
            "- sudo — apt-get only (update, install, remove, purge, upgrade, "
            "full-upgrade, clean, autoremove); writes the ephemeral per-pod rootfs"
        )
    count = dpkg_count()
    lines.append("")
    lines.append(f"Base image dpkg packages: {count if count is not None else 'unknown'}.")
    lines.append(FOOTER_FINAL)
    lines.append("")
    return "\n".join(lines)


def self_test(tools_path: Path) -> int:
    """Validate tools.json + rendering invariants. Exit 0 on success."""
    failures: list[str] = []
    try:
        doc = load_tools(tools_path)
    except (OSError, json.JSONDecodeError) as exc:
        print(f"FAIL: cannot load {tools_path}: {exc}")
        return 1
    failures.extend(validate_tools(doc))
    if failures:
        for f in failures:
            print(f"FAIL: {f}")
        return 1
    if not apt_packages(doc) or not pip_packages(doc):
        print("FAIL: manifest has no apt or no pip packages")
        return 1
    for required in ("pandas", "openpyxl", "numpy"):
        if required not in pip_packages(doc):
            print(f"FAIL: required pip package {required!r} missing from tools.json")
            return 1
    manifest = render_manifest(doc)
    # Path-free invariant: workspace/install recipes live in the runtime's
    # Rust-built section, never in this static file.
    for forbidden in ("/workspace", "/packages", "/tmp", "/home/", "/opt/"):
        if forbidden in manifest:
            print(f"FAIL: manifest contains path {forbidden!r} (must be path-free):")
            for ln in manifest.splitlines():
                if forbidden in ln:
                    print(f"  {ln}")
            return 1
    if "Toolchain baked into this base image" not in manifest:
        print("FAIL: manifest missing inventory header")
        return 1
    if not manifest.rstrip("\n").endswith(FOOTER_FINAL):
        print("FAIL: manifest does not end with the live-state hint")
        return 1
    if len(manifest) > 4000:
        print(f"FAIL: manifest too large ({len(manifest)} chars > 4000) — token budget blown")
        return 1
    areas = [a["name"] for a in doc["areas"]]
    print(f"PASS: tools.json valid ({len(areas)} areas, "
          f"{len(apt_packages(doc))} apt, {len(pip_packages(doc))} pip pkgs)")
    print(f"PASS: manifest renders path-free, {len(manifest.splitlines())} lines, "
          f"{len(manifest)} chars")
    return 0


def main() -> int:
    default_tools = Path(__file__).resolve().parent / "tools.json"
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--tools", type=Path, default=default_tools,
                        help="path to tools.json (default: alongside this script)")
    parser.add_argument("--output", type=Path,
                        default=Path("/usr/local/share/sandbox-capabilities.md"),
                        help="manifest output path (default: image location)")
    parser.add_argument("--print-apt", action="store_true",
                        help="print the space-separated apt package list and exit")
    parser.add_argument("--print-pip", action="store_true",
                        help="print the space-separated pip package list and exit")
    parser.add_argument("--stdout", action="store_true",
                        help="render the manifest to stdout instead of --output")
    parser.add_argument("--self-test", action="store_true",
                        help="validate tools.json + rendering invariants and exit")
    args = parser.parse_args()

    try:
        doc = load_tools(args.tools)
    except (OSError, json.JSONDecodeError) as exc:
        print(f"error: cannot load {args.tools}: {exc}", file=sys.stderr)
        return 2
    errs = validate_tools(doc)
    if errs:
        for e in errs:
            print(f"error: {e}", file=sys.stderr)
        return 2

    if args.self_test:
        return self_test(args.tools)
    if args.print_apt:
        print(" ".join(apt_packages(doc)))
        return 0
    if args.print_pip:
        print(" ".join(pip_packages(doc)))
        return 0

    manifest = render_manifest(doc)
    if args.stdout:
        sys.stdout.write(manifest)
        return 0
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(manifest, encoding="utf-8")
    print(f"wrote {args.output} ({len(manifest.splitlines())} lines)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
