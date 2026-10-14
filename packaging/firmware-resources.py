#!/usr/bin/env python3
"""Package pinned app resources. This program never executes the app or opens a pedal.

An independently retained manifest digest is required. A matching digest proves
bytes, not that the caller has performed the acceptance review described by them.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import uuid

MAX_FILE = 1024 * 1024 * 1024
MAX_JSON = 16 * 1024 * 1024
# The app adds its configuration file to the source list at load time.
MAX_SOURCES = 2996
MATERIALS = ("loss", "recovery", "vendor_guidance", "contract")
POLICIES = {"approval_wait_seconds": (30, 900), "transition_seconds": (5, 120),
            "transfer_seconds": (60, 3600)}
CONFIG_NAME = "firmware-resources.json"
BUNDLE_NAME = "firmware-resources/bundle-manifest.json"
PIN_KEYS = {"path", "bytes", "sha256"}


class ResourceError(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise ResourceError(message)


def exact(value, keys, name):
    require(isinstance(value, dict) and set(value) == set(keys),
            f"{name} has missing or unknown fields")


def absolute(value):
    require(isinstance(value, str) and value.startswith("/") and "\0" not in value,
            "Path must be absolute")
    parts = value.split("/")[1:]
    require(parts and all(part not in ("", ".", "..") for part in parts),
            "Path must have plain components")
    return Path(value)


def relative(value):
    require(isinstance(value, str) and not value.startswith("/") and "\0" not in value,
            "Bundle path must be relative")
    require(all(part not in ("", ".", "..") for part in value.split("/")),
            "Bundle path must have plain components")
    return value


def digest(value):
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-f]{64}", value) is not None,
            "SHA-256 must be 64 lowercase hexadecimal characters")
    return value


def validate_pin(pin, *, copied=False):
    exact(pin, PIN_KEYS, "File pin")
    (relative if copied else absolute)(pin["path"])
    require(type(pin["bytes"]) is int and 0 < pin["bytes"] <= MAX_FILE,
            "Pinned file extent must be between 1 byte and 1 GiB")
    digest(pin["sha256"])


def open_parent(path):
    """Walk directories through no-follow descriptors, including every ancestor."""
    path = absolute(str(path))
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for component in path.parts[1:-1]:
            next_fd = os.open(component, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                              dir_fd=fd)
            os.close(fd)
            fd = next_fd
        return fd, path.name
    except BaseException:
        os.close(fd)
        raise


def named_stat(path):
    parent, name = open_parent(path)
    try:
        return os.stat(name, dir_fd=parent, follow_symlinks=False)
    finally:
        os.close(parent)


def stable(metadata):
    return (metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_nlink,
            metadata.st_size, metadata.st_mtime_ns, metadata.st_ctime_ns)


def read_file(path, *, expected=None, limit=MAX_FILE, destination=None, mode=0o600, retain=False):
    """Read or copy one unchanged regular file; destination is create-new only."""
    parent, name = open_parent(path)
    source_fd = output_fd = None
    content = bytearray() if destination is None and retain else None
    try:
        source_fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=parent)
        before = os.fstat(source_fd)
        require(stat.S_ISREG(before.st_mode) and before.st_nlink == 1,
                "Input must be a plain file with one link")
        require(0 < before.st_size <= limit, "Input exceeds its bounded extent")
        if expected is not None:
            validate_pin(expected)
            require(str(path) == expected["path"] and before.st_size == expected["bytes"],
                    "Input path or extent differs from its pin")
        if destination is not None:
            out_parent, out_name = open_parent(destination)
            try:
                output_fd = os.open(out_name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                    mode, dir_fd=out_parent)
                os.fchmod(output_fd, mode)
            finally:
                os.close(out_parent)
        sha = hashlib.sha256()
        count = 0
        while True:
            block = os.read(source_fd, 1024 * 1024)
            if not block:
                break
            count += len(block)
            require(count <= before.st_size, "Input grew while reading")
            sha.update(block)
            if content is not None:
                content.extend(block)
            if output_fd is not None:
                offset = 0
                while offset < len(block):
                    written = os.write(output_fd, block[offset:])
                    require(written > 0, "Copy made no progress")
                    offset += written
        after = os.fstat(source_fd)
        require(count == before.st_size and stable(before) == stable(after)
                and stable(before) == stable(named_stat(path)), "Input changed while reading")
        pin = {"path": str(path), "bytes": count, "sha256": sha.hexdigest()}
        require(expected is None or pin == expected, "Input hash differs from its pin")
        if output_fd is not None:
            os.fsync(output_fd)
            out_parent, _ = open_parent(destination)
            try:
                os.fsync(out_parent)
            finally:
                os.close(out_parent)
        return pin, bytes(content) if content is not None else None
    finally:
        if source_fd is not None:
            os.close(source_fd)
        if output_fd is not None:
            os.close(output_fd)
        os.close(parent)


def json_bytes(value):
    return (json.dumps(value, indent=2, ensure_ascii=True, sort_keys=True) + "\n").encode()


def parse_json(data):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            require(key not in result, "Duplicate JSON field")
            result[key] = value
        return result
    try:
        return json.loads(data, object_pairs_hook=unique,
                          parse_constant=lambda value: (_ for _ in ()).throw(ResourceError(value)))
    except (UnicodeError, json.JSONDecodeError) as error:
        raise ResourceError(f"Invalid resource JSON: {error}") from error


def policies(value):
    for key, (low, high) in POLICIES.items():
        require(type(value[key]) is int and low <= value[key] <= high,
                f"{key} must be within {low}..{high}")


def validate_manifest(value):
    exact(value, {"schema", "kind", "acceptance", "executable", "inputs"}, "Input manifest")
    require(type(value["schema"]) is int and value["schema"] == 1
            and value["kind"] == "gp200-linux-firmware-resource-inputs", "Unsupported input manifest")
    validate_pin(value["acceptance"])
    validate_pin(value["executable"])
    inputs = value["inputs"]
    exact(inputs, {"sources", *MATERIALS, *POLICIES}, "Input manifest inputs")
    require(isinstance(inputs["sources"], list) and 1 <= len(inputs["sources"]) <= MAX_SOURCES,
            f"Source list must contain 1..{MAX_SOURCES} pins")
    seen = set()
    for pin in inputs["sources"]:
        validate_pin(pin)
        require(pin["path"] not in seen, "Duplicate source path")
        seen.add(pin["path"])
    for key in MATERIALS:
        validate_pin(inputs[key])
    policies(inputs)
    return value


def private_directory(path, *, create=False):
    path = absolute(str(path))
    parent, name = open_parent(path)
    try:
        if create:
            os.mkdir(name, 0o700, dir_fd=parent)
        fd = os.open(name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=parent)
        try:
            info = os.fstat(fd)
            require(info.st_uid == os.geteuid() and stat.S_IMODE(info.st_mode) == 0o700,
                    "Directory must be owned by the current user with mode 0700")
            require(stable(info) == stable(named_stat(path)), "Directory changed during admission")
            os.fsync(fd)
        finally:
            os.close(fd)
        os.fsync(parent)
    finally:
        os.close(parent)
    return path


def publish(path, data, *, replace=False):
    require(0 < len(data) <= MAX_JSON, "Generated resource JSON exceeds its limit")
    parent, name = open_parent(path)
    temporary = None
    fd = None
    try:
        previous = None
        if replace:
            previous = os.stat(name, dir_fd=parent, follow_symlinks=False)
            require(stat.S_ISREG(previous.st_mode) and previous.st_nlink == 1
                    and previous.st_uid == os.geteuid(), "Existing config must be an owned plain file")
            temporary = ".firmware-resources-" + uuid.uuid4().hex
        target = temporary or name
        fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600, dir_fd=parent)
        os.fchmod(fd, 0o600)
        offset = 0
        while offset < len(data):
            written = os.write(fd, data[offset:])
            require(written > 0, "Publication made no progress")
            offset += written
        os.fsync(fd)
        os.close(fd)
        fd = None
        if temporary:
            require(stable(previous) == stable(os.stat(name, dir_fd=parent, follow_symlinks=False)),
                    "Existing config changed during regeneration")
            os.replace(temporary, name, src_dir_fd=parent, dst_dir_fd=parent)
            temporary = None
        os.fsync(parent)
    finally:
        if fd is not None:
            os.close(fd)
        if temporary:
            os.unlink(temporary, dir_fd=parent)
        os.close(parent)
    expected = {"path": str(path), "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    read_file(path, expected=expected, limit=MAX_JSON)
    return expected


def source_name(index, original):
    name = re.sub(r"[^a-zA-Z0-9._-]", "_", Path(original["path"]).name)[:64] or "source"
    return f"firmware-resources/sources/{index:04d}-{name}"


def mapping(original, copied):
    return {"original": original, "copy": {"path": copied, "bytes": original["bytes"],
                                           "sha256": original["sha256"]}}


def bundle_description(manifest, input_pin):
    inputs = manifest["inputs"]
    return {"schema": 1, "kind": "gp200-linux-firmware-resource-bundle",
            "authority": "descriptive-only",
            "input_manifest": mapping(input_pin, "firmware-resources/accepted-input-manifest.json"),
            "acceptance": mapping(manifest["acceptance"], "firmware-resources/acceptance-receipt"),
            "executable": mapping(manifest["executable"], "gp200-studio"),
            "sources": [mapping(pin, source_name(index, pin))
                        for index, pin in enumerate(inputs["sources"], 1)],
            "materials": {key: mapping(inputs[key], f"firmware-resources/materials/{key}")
                          for key in MATERIALS},
            "policies": {key: inputs[key] for key in POLICIES}}


def mappings(bundle):
    return [bundle["input_manifest"], bundle["acceptance"], bundle["executable"],
            *bundle["sources"], *bundle["materials"].values()]


def copied_pin(destination, record):
    pin = dict(record["copy"])
    pin["path"] = str(destination / relative(pin["path"]))
    return pin


def resources(destination, bundle, bundle_pin, run_base):
    inputs = {"sources": [copied_pin(destination, record) for record in bundle["sources"]]
              + [copied_pin(destination, bundle["input_manifest"]),
                 copied_pin(destination, bundle["acceptance"]), bundle_pin],
              "run_directory": str(run_base), **bundle["policies"]}
    inputs.update({key: copied_pin(destination, record) for key, record in bundle["materials"].items()})
    return {"schema": 1, "executable_sha256": bundle["executable"]["copy"]["sha256"],
            "inputs": inputs}


def verify_bundle(destination, expected_sha256):
    destination = private_directory(destination)
    pin, data = read_file(destination / BUNDLE_NAME, limit=MAX_JSON, retain=True)
    require(pin["sha256"] == digest(expected_sha256), "Retained bundle manifest hash differs")
    bundle = parse_json(data)
    exact(bundle, {"schema", "kind", "authority", "input_manifest", "acceptance", "executable",
                   "sources", "materials", "policies"}, "Bundle manifest")
    # Validate the retained original manifest before using any other mapping.
    record = bundle["input_manifest"]
    exact(record, {"original", "copy"}, "Manifest mapping")
    validate_pin(record["original"])
    validate_pin(record["copy"], copied=True)
    require(record["copy"]["path"] == "firmware-resources/accepted-input-manifest.json", "Unexpected manifest path")
    input_pin = copied_pin(destination, record)
    _, accepted_bytes = read_file(input_pin["path"], expected=input_pin, limit=MAX_JSON, retain=True)
    accepted = validate_manifest(parse_json(accepted_bytes))
    require(bundle == bundle_description(accepted, record["original"]),
            "Bundle provenance differs from the retained input manifest")
    seen = set()
    for record in mappings(bundle):
        copied = copied_pin(destination, record)
        require(copied["path"] not in seen, "Duplicate copied path")
        seen.add(copied["path"])
        read_file(copied["path"], expected=copied,
                  destination=None, limit=MAX_JSON if record is bundle["input_manifest"] else MAX_FILE)
    return destination, bundle, pin


def receipt(command, destination, bundle_pin, config_pin):
    return {"schema": 1, "command": command, "destination": str(destination),
            "bundle_manifest": bundle_pin, "config": config_pin,
            "app_executions": 0, "firmware_operations": 0,
            "authority": "descriptive-only"}


def package(manifest_path, expected_sha256, destination, run_base=None):
    input_pin, data = read_file(absolute(str(manifest_path)), limit=MAX_JSON, retain=True)
    require(input_pin["sha256"] == digest(expected_sha256), "Input manifest hash differs")
    manifest = validate_manifest(parse_json(data))
    # Reject invalid sources before creating a partial destination.
    for pin in [manifest["acceptance"], manifest["executable"],
                *manifest["inputs"]["sources"], *(manifest["inputs"][key] for key in MATERIALS)]:
        read_file(pin["path"], expected=pin)
    destination = private_directory(destination, create=True)
    for suffix in ("firmware-resources", "firmware-resources/sources", "firmware-resources/materials"):
        private_directory(destination / suffix, create=True)
    run_base = private_directory(run_base) if run_base is not None else private_directory(destination / "firmware-runs", create=True)
    bundle = bundle_description(manifest, input_pin)
    for record in mappings(bundle):
        target = destination / record["copy"]["path"]
        read_file(record["original"]["path"], expected=record["original"], destination=target,
                  mode=0o755 if record is bundle["executable"] else 0o600)
        read_file(target, expected=copied_pin(destination, record))
    bundle_pin = publish(destination / BUNDLE_NAME, json_bytes(bundle))
    verify_bundle(destination, bundle_pin["sha256"])
    config_pin = publish(destination / CONFIG_NAME, json_bytes(resources(destination, bundle, bundle_pin, run_base)))
    verify(destination, bundle_pin["sha256"])
    return receipt("package", destination, bundle_pin, config_pin)


def verify(destination, expected_sha256):
    destination, bundle, bundle_pin = verify_bundle(destination, expected_sha256)
    config_pin, config_data = read_file(destination / CONFIG_NAME, limit=MAX_JSON, retain=True)
    config = parse_json(config_data)
    exact(config, {"schema", "executable_sha256", "inputs"}, "App resources")
    require(isinstance(config["inputs"], dict) and "run_directory" in config["inputs"], "Run directory is missing")
    run_base = private_directory(config["inputs"]["run_directory"])
    require(config == resources(destination, bundle, bundle_pin, run_base),
            "App resources differ from the exact bundle or current destination")
    return receipt("verify", destination, bundle_pin, config_pin)


def regenerate(destination, expected_sha256, run_base=None):
    destination, bundle, bundle_pin = verify_bundle(destination, expected_sha256)
    if run_base is None:
        run_base = private_directory(destination / "firmware-runs")
    else:
        run_base = private_directory(run_base)
    config_pin = publish(destination / CONFIG_NAME,
                         json_bytes(resources(destination, bundle, bundle_pin, run_base)), replace=True)
    verify(destination, expected_sha256)
    return receipt("regenerate", destination, bundle_pin, config_pin)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    create = sub.add_parser("package", help="Copy exact pinned resources into a new owned 0700 destination")
    create.add_argument("--manifest", required=True)
    create.add_argument("--manifest-sha256", required=True)
    create.add_argument("--destination", required=True)
    create.add_argument("--run-base", help="Existing owned 0700 base; default is a new directory inside the bundle")
    for command in ("verify", "regenerate"):
        item = sub.add_parser(command)
        item.add_argument("--destination", required=True)
        item.add_argument("--bundle-manifest-sha256", required=True)
        if command == "regenerate":
            item.add_argument("--run-base", help="Existing owned 0700 base; default is firmware-runs inside the bundle")
    args = parser.parse_args(argv)
    try:
        if args.command == "package":
            result = package(args.manifest, args.manifest_sha256, args.destination, args.run_base)
        elif args.command == "verify":
            result = verify(args.destination, args.bundle_manifest_sha256)
        else:
            result = regenerate(args.destination, args.bundle_manifest_sha256, args.run_base)
    except (ResourceError, OSError) as error:
        print(f"Firmware resources: {error}", file=sys.stderr)
        return 1
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
