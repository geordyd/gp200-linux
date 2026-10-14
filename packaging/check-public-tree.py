#!/usr/bin/env python3
"""Reject private development inputs in Git's public source index.

This checks files, not a connected pedal. Run it before creating a public
commit; .gitignore alone cannot remove files already tracked by Git.
"""
from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import subprocess
import sys

PRIVATE_PREFIXES = (
    "analysis/", "implementation/", "plans/", "mockups/", "screenshots/",
    "tools/", "target/", "dist/", ".local-data/", ".dev-logs/", ".cursor/",
)
PRIVATE_FILES = {
    ".hardware-session.lock",
    "crates/gp200-transport/test-fixtures/firmware/genuine-entry-review.json",
    "tests/fixtures/assets/andertons-profiles.json",
}
KNOWN_VENDOR_FIRMWARE = {
    "c368618773097b07c556c951eb10792f9d57be929323366648eb6e083d0d23af",
}


def problems(root: Path, names: list[str]) -> list[str]:
    errors = []
    for name in names:
        path = root / name
        if name.startswith(PRIVATE_PREFIXES) or name in PRIVATE_FILES:
            errors.append(f"private development input is tracked: {name}")
            continue
        if path.name == ".env" or (path.name.startswith(".env.") and path.name != ".env.example"):
            errors.append(f"local environment file is tracked: {name}")
        if path.suffix.lower() in {".exe", ".dll", ".so", ".dylib", ".appimage"}:
            errors.append(f"compiled binary is tracked: {name}")
        if path.is_symlink():
            errors.append(f"source symlink needs an explicit publication review: {name}")
        elif not path.is_file():
            errors.append(f"tracked source file is missing: {name}")
        elif path.stat().st_size > 10 * 1024 * 1024:
            errors.append(f"source file exceeds 10 MiB: {name}")
        elif path.suffix.lower() == ".bin":
            content = path.read_bytes()
            if hashlib.sha256(content).hexdigest() in KNOWN_VENDOR_FIRMWARE:
                errors.append(f"official firmware image is tracked: {name}")
            elif content.startswith(b"FRMW") and len(content) > 1024 * 1024:
                errors.append(f"large firmware image requires private storage: {name}")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    args = parser.parse_args()
    root = args.root.resolve()
    raw = subprocess.check_output(["git", "-C", str(root), "ls-files", "-z"])
    names = [name.decode("utf-8") for name in raw.split(b"\0") if name]
    errors = problems(root, names)
    if errors:
        print("Public source check failed:\n" + "\n".join(errors), file=sys.stderr)
        return 1
    print(f"Public source check passed: {len(names)} tracked files; no private development trees or vendor firmware.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
