#!/usr/bin/env python3
"""Install this portable archive into the current user's prefix."""
import argparse
import json
from pathlib import Path
import sys
from installlib import InstallError, install, selected_prefix

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--prefix", help="absolute user prefix (default ~/.local)")
parser.add_argument("--no-cache-update", action="store_true")
args = parser.parse_args()
try:
    result = install(Path(__file__).resolve().parent, selected_prefix(args.prefix),
                     not args.no_cache_update)
    print(json.dumps(result, indent=2))
except (InstallError, OSError, ValueError) as error:
    print(f"GP200 Linux install: {error}", file=sys.stderr)
    sys.exit(1)
