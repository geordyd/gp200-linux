#!/usr/bin/env python3
"""Independent stdlib writer for the documented portable v1 format.

Synthetic data only: no vendor .prst compatibility or pedal write evidence.
"""
import hashlib
import json
from pathlib import Path
import struct
import zipfile

ROOT = Path(__file__).resolve().parent


def json_bytes(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()


def main():
    blobs = {}

    def blob(data):
        key = hashlib.sha256(data).hexdigest()
        blobs[key] = data
        return key

    library = "preset-synthetic-v1"
    versions = {}
    dependency = {"UserSlot": {"kind": "ImpulseResponse", "slot": 2, "content": None}}
    original_bytes = b"opaque original fixture\x00\xff"
    original_hash = blob(original_bytes)
    for ordinal in range(2):
        bits = [0x7FC000A5, 0x80000000 if ordinal == 0 else 0x40900000] + [0] * 13
        body = struct.pack("<BBHI15I", 0, 2, 15, 0x12345678, *bits)
        payload = struct.pack("<HH", 0x14, len(body)) + body
        payload += struct.pack("<HH", 0xBEEF, 2) + b"\x00\xff"
        payload += struct.pack("<HH", 0xBEEF, 1) + b"\x88"
        version_id = f"revision-synthetic-{ordinal}"
        snapshot = {
            "source": {"Capture": {"capture_id": "synthetic-library-v1"}},
            "device_model": "Gp200", "firmware": None, "target": None,
            "context": None, "revision": ordinal,
            "raw": {"kind": 0x12, "payload": list(payload)},
            "view": {
                "name": "Unknown", "tempo": "Unknown", "level": "Unknown",
                "chain": "Unknown", "modules": [{
                    "module": 0, "state_raw": 2, "slot_count_raw": 15,
                    "model": 0x12345678, "parameters": bits, "raw_record_index": 0,
                }],
            },
            "asset_refs": [dependency], "evidence": [],
        }
        versions[version_id] = {
            "id": version_id, "library": library,
            "parent": None if ordinal == 0 else "revision-synthetic-0",
            "snapshot_hash": blob(json_bytes(snapshot)),
            "raw_hash": blob(bytes([0x12]) + payload),
            "metadata": {
                "name": f"Synthetic 音色 {ordinal}", "author": "Fixture writer",
                "style": "Test", "notes": "Independent Python writer",
                "tags": ["fixture"], "favorite": False,
            },
            "created_at": f"2026-10-10T00:00:0{ordinal}.000000000Z",
            "originals": [{"name": "opaque.native", "format": "synthetic-opaque",
                           "hash": original_hash, "bytes": len(original_bytes)}],
            "model_names": [],
        }
    manifest = {
        "format": "gp200-studio.bundle", "schema_version": 1,
        "scope": "preset-only",
        "entries": {library: {
            "id": library, "latest": "revision-synthetic-1",
            "versions": ["revision-synthetic-0", "revision-synthetic-1"],
            "trashed": False,
        }},
        "versions": versions, "assets": {},
        "blobs": {key: len(data) for key, data in sorted(blobs.items())},
        "missing_assets": [dependency],
    }
    path = ROOT / "portable-v1-independent.gp200bundle"
    with zipfile.ZipFile(path, "w") as archive:
        for name, data in [("manifest.json", json_bytes(manifest))] + [
            (f"blobs/{key}", data) for key, data in sorted(blobs.items())
        ]:
            info = zipfile.ZipInfo(name, date_time=(2026, 10, 10, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o100600 << 16
            archive.writestr(info, data)
    checksum = hashlib.sha256(path.read_bytes()).hexdigest()
    (ROOT / "checksums.json").write_text(json.dumps({path.name: checksum}, indent=2) + "\n")


if __name__ == "__main__":
    main()
