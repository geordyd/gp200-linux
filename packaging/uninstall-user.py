#!/usr/bin/env python3
"""Remove owned installed files while retaining all app data and changed files."""
import argparse
import json
import sys
from installlib import InstallError, selected_prefix, uninstall

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--prefix", help="same absolute user prefix supplied at install")
parser.add_argument("--no-cache-update", action="store_true")
args = parser.parse_args()
try:
    result = uninstall(selected_prefix(args.prefix), not args.no_cache_update)
    print(json.dumps(result, indent=2))
except (InstallError, OSError, ValueError) as error:
    print(f"GP200 Linux uninstall: {error}", file=sys.stderr)
    sys.exit(1)
