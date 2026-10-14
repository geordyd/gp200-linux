#!/usr/bin/env python3
"""Require an exact v<workspace SemVer> release tag."""
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent
version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
pattern = (r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
           r"(?:-(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
           r"(?:\.(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?"
           r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?")
if len(sys.argv) != 2 or not re.fullmatch(pattern, sys.argv[1]) or sys.argv[1] != f"v{version}":
    sys.exit(f"Release tag must be exactly v{version}, matching Cargo.toml")
print(f"Validated {sys.argv[1]}")
