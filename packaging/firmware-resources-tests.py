#!/usr/bin/env python3
"""Inert-file packaging checks. No app executable, Rust, USB or MIDI is run."""
from __future__ import annotations

import argparse
import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("firmware_resources", HERE / "firmware-resources.py")
PACK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACK)
CHILDREN = []


def identity(pid):
    try:
        text = Path(f"/proc/{pid}/stat").read_text()
        return {"pid": pid, "start_ticks": int(text[text.rfind(")") + 2:].split()[19])}
    except FileNotFoundError:
        return None


def pin(path):
    data = path.read_bytes()
    return {"path": str(path), "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}


class ResourceTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="gp200-resource-fixture-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.original = self.root / "original"
        self.original.mkdir(mode=0o700)
        self.destination = self.root / "app"
        self.marker = self.root / "app-was-executed"
        # Merely copied, never executed. Its side effect would expose accidental execution.
        executable = self.original / "inert-app"
        executable.write_text(f"#!/bin/sh\ntouch '{self.marker}'\n")
        executable.chmod(0o700)
        acceptance = self.original / "acceptance.json"
        acceptance.write_text('{"scope":"inert packaging fixture only"}\n')
        sources = []
        for index, name in enumerate(("source.rs", "license file.txt")):
            path = self.original / name
            path.write_text(f"inert accepted source {index}\n")
            sources.append(pin(path))
        inputs = {"sources": sources, "approval_wait_seconds": 120,
                  "transition_seconds": 30, "transfer_seconds": 900}
        for name in PACK.MATERIALS:
            path = self.original / name
            path.write_text(f"inert fixture: {name}; no device authority\n")
            inputs[name] = pin(path)
        self.manifest = {"schema": 1, "kind": "gp200-linux-firmware-resource-inputs",
                         "acceptance": pin(acceptance), "executable": pin(executable), "inputs": inputs}
        self.manifest_path = self.original / "input-manifest.json"
        self.save_manifest()

    def tearDown(self):
        self.assertFalse(self.marker.exists(), "The app fixture must never execute")

    def save_manifest(self):
        self.manifest_path.write_bytes(PACK.json_bytes(self.manifest))
        self.manifest_pin = pin(self.manifest_path)

    def package(self, **kwargs):
        return PACK.package(self.manifest_path, self.manifest_pin["sha256"], self.destination, **kwargs)

    def rejected(self, function, *args, **kwargs):
        with self.assertRaises((PACK.ResourceError, OSError)):
            function(*args, **kwargs)

    def test_package_binds_executable_sources_materials_and_acceptance(self):
        receipt = self.package()
        config = json.loads((self.destination / PACK.CONFIG_NAME).read_bytes())
        bundle = json.loads((self.destination / PACK.BUNDLE_NAME).read_bytes())
        self.assertEqual(config["executable_sha256"], self.manifest["executable"]["sha256"])
        self.assertEqual(len(config["inputs"]["sources"]), len(self.manifest["inputs"]["sources"]) + 3)
        self.assertEqual(bundle["sources"][0]["original"], self.manifest["inputs"]["sources"][0])
        self.assertEqual((self.destination / "gp200-studio").read_bytes(), Path(self.manifest["executable"]["path"]).read_bytes())
        for item in config["inputs"]["sources"] + [config["inputs"][name] for name in PACK.MATERIALS]:
            self.assertTrue(item["path"].startswith(str(self.destination) + "/"))
            self.assertEqual(pin(Path(item["path"])), item)
        for suffix in ("", "firmware-resources", "firmware-resources/sources", "firmware-resources/materials", "firmware-runs"):
            self.assertEqual(stat.S_IMODE((self.destination / suffix).stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE((self.destination / "gp200-studio").stat().st_mode), 0o755)
        self.assertEqual(stat.S_IMODE((self.destination / PACK.CONFIG_NAME).stat().st_mode), 0o600)
        verified = PACK.verify(self.destination, receipt["bundle_manifest"]["sha256"])
        self.assertEqual(verified["config"], receipt["config"])
        self.assertEqual(verified["firmware_operations"], 0)

    def test_changed_original_source_before_package_is_rejected_without_destination(self):
        source = Path(self.manifest["inputs"]["sources"][0]["path"])
        source.write_bytes(b"x" * source.stat().st_size)
        self.rejected(self.package)
        self.assertFalse(self.destination.exists())

    def test_changed_original_executable_is_rejected(self):
        Path(self.manifest["executable"]["path"]).write_bytes(b"different executable\n")
        self.rejected(self.package)
        self.assertFalse(self.destination.exists())

    def test_wrong_manifest_hash_is_rejected(self):
        self.rejected(PACK.package, self.manifest_path, "0" * 64, self.destination)
        self.assertFalse(self.destination.exists())

    def test_changed_pinned_material_is_rejected(self):
        Path(self.manifest["inputs"]["loss"]["path"]).write_bytes(b"replacement loss guidance\n")
        self.rejected(self.package)

    def test_duplicate_sources_and_duplicate_json_fields_are_rejected(self):
        self.manifest["inputs"]["sources"].append(copy.deepcopy(self.manifest["inputs"]["sources"][0]))
        self.save_manifest()
        self.rejected(self.package)
        self.manifest_path.write_bytes(b'{"schema":1,"schema":1}')
        self.rejected(PACK.package, self.manifest_path, pin(self.manifest_path)["sha256"], self.destination)

    def test_unknown_fields_and_bad_schema_are_rejected(self):
        for field in ("root", "inputs", "pin"):
            with self.subTest(field=field):
                changed = copy.deepcopy(self.manifest)
                node = changed if field == "root" else changed["inputs"] if field == "inputs" else changed["executable"]
                node["unknown"] = True
                self.rejected(PACK.validate_manifest, changed)
        for schema in (True, 0, 2, "1"):
            with self.subTest(schema=schema):
                changed = copy.deepcopy(self.manifest)
                changed["schema"] = schema
                self.rejected(PACK.validate_manifest, changed)

    def test_policy_endpoints_accepted_and_outside_or_boolean_rejected(self):
        for name, (low, high) in PACK.POLICIES.items():
            for value in (low, high):
                with self.subTest(name=name, valid=value):
                    changed = copy.deepcopy(self.manifest)
                    changed["inputs"][name] = value
                    PACK.validate_manifest(changed)
            for value in (low - 1, high + 1, True, 1.0, "120"):
                with self.subTest(name=name, invalid=value):
                    changed = copy.deepcopy(self.manifest)
                    changed["inputs"][name] = value
                    self.rejected(PACK.validate_manifest, changed)

    def test_source_count_reserves_room_for_loader_config(self):
        changed = copy.deepcopy(self.manifest)
        template = changed["inputs"]["sources"][0]
        changed["inputs"]["sources"] = [{**template, "path": f"/inert/source-{i}"} for i in range(PACK.MAX_SOURCES)]
        PACK.validate_manifest(changed)
        changed["inputs"]["sources"].append({**template, "path": "/inert/source-extra"})
        self.rejected(PACK.validate_manifest, changed)
        changed["inputs"]["sources"] = []
        self.rejected(PACK.validate_manifest, changed)

    def test_invalid_pin_extent_hash_and_plain_paths_are_rejected(self):
        variants = ({"bytes": 0}, {"bytes": -1}, {"bytes": True}, {"bytes": PACK.MAX_FILE + 1},
                    {"sha256": "A" * 64}, {"sha256": "0" * 63},
                    {"path": "relative"}, {"path": "/tmp/../source"}, {"path": "/tmp/./source"},
                    {"path": "/tmp//source"}, {"path": "/"}, {"path": "/tmp/source\0"})
        for variant in variants:
            with self.subTest(variant=variant):
                changed = copy.deepcopy(self.manifest)
                changed["inputs"]["sources"][0].update(variant)
                self.rejected(PACK.validate_manifest, changed)

    def test_symlink_source_and_symlink_ancestor_are_rejected(self):
        original = Path(self.manifest["inputs"]["sources"][0]["path"])
        real = original.with_suffix(".real")
        original.rename(real)
        original.symlink_to(real)
        self.rejected(self.package)
        original.unlink()
        real.rename(original)
        alias = self.root / "source-alias"
        alias.symlink_to(self.original, target_is_directory=True)
        self.manifest["inputs"]["sources"][0]["path"] = str(alias / original.name)
        self.save_manifest()
        self.rejected(self.package)

    def test_hardlinked_and_special_sources_are_rejected(self):
        source = Path(self.manifest["inputs"]["sources"][0]["path"])
        os.link(source, self.original / "source-hardlink")
        self.rejected(self.package)
        (self.original / "source-hardlink").unlink()
        source.unlink()
        os.mkfifo(source, 0o600)
        self.rejected(self.package)

    def test_in_place_and_named_replacement_during_read_are_rejected(self):
        source = Path(self.manifest["inputs"]["sources"][0]["path"])
        real_read = os.read
        for replacement in (False, True):
            with self.subTest(replacement=replacement):
                source.write_bytes(b"known original\n")
                expected = pin(source)
                mutated = False
                def race(fd, count):
                    nonlocal mutated
                    data = real_read(fd, count)
                    if not mutated:
                        mutated = True
                        if replacement:
                            other = self.original / "replacement"
                            other.write_bytes(b"known original\n")
                            os.replace(other, source)
                        else:
                            source.write_bytes(b"other original\n")
                    return data
                with patch.object(PACK.os, "read", side_effect=race):
                    self.rejected(PACK.read_file, source, expected=expected)

    def test_existing_destination_and_symlink_destination_parent_are_rejected(self):
        self.destination.mkdir(mode=0o700)
        sentinel = self.destination / "sentinel"
        sentinel.write_bytes(b"keep")
        self.rejected(self.package)
        self.assertEqual(sentinel.read_bytes(), b"keep")
        alias = self.root / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        self.rejected(PACK.package, self.manifest_path, self.manifest_pin["sha256"], alias / "new")

    def test_source_change_during_copy_does_not_publish_app_configuration(self):
        source = Path(self.manifest["inputs"]["sources"][0]["path"])
        original_read = PACK.read_file
        changed = False
        def change_after_preflight(path, **kwargs):
            nonlocal changed
            if str(path) == str(source) and kwargs.get("destination") is not None and not changed:
                changed = True
                source.write_bytes(b"x" * source.stat().st_size)
            return original_read(path, **kwargs)
        with patch.object(PACK, "read_file", side_effect=change_after_preflight):
            self.rejected(self.package)
        self.assertTrue(changed)
        self.assertTrue(self.destination.exists())
        self.assertFalse((self.destination / PACK.CONFIG_NAME).exists())

    def test_run_base_must_be_plain_private_and_owned(self):
        unsafe = self.root / "unsafe"
        unsafe.mkdir(mode=0o755)
        self.rejected(PACK.private_directory, unsafe)
        safe = self.root / "safe"
        safe.mkdir(mode=0o700)
        link = self.root / "run-alias"
        link.symlink_to(safe, target_is_directory=True)
        self.rejected(PACK.private_directory, link)
        owner = os.geteuid()
        with patch.object(PACK.os, "geteuid", return_value=owner + 1):
            self.rejected(PACK.private_directory, safe)
        receipt = self.package(run_base=safe)
        self.assertEqual(json.loads((self.destination / PACK.CONFIG_NAME).read_bytes())["inputs"]["run_directory"], str(safe))
        PACK.verify(self.destination, receipt["bundle_manifest"]["sha256"])

    def test_copied_source_executable_and_material_tampering_are_rejected(self):
        receipt = self.package()
        config = json.loads((self.destination / PACK.CONFIG_NAME).read_bytes())
        targets = [self.destination / "gp200-studio", Path(config["inputs"]["sources"][0]["path"]),
                   Path(config["inputs"]["contract"]["path"])]
        for target in targets:
            with self.subTest(target=target.name):
                old = target.read_bytes()
                target.write_bytes(b"x" * len(old))
                self.rejected(PACK.verify, self.destination, receipt["bundle_manifest"]["sha256"])
                self.rejected(PACK.regenerate, self.destination, receipt["bundle_manifest"]["sha256"])
                target.write_bytes(old)

    def test_manifest_tampering_or_wrong_retained_hash_are_rejected(self):
        receipt = self.package()
        self.rejected(PACK.verify, self.destination, "0" * 64)
        manifest = self.destination / PACK.BUNDLE_NAME
        data = json.loads(manifest.read_bytes())
        data["authority"] = "execute-the-pedal"
        manifest.write_bytes(PACK.json_bytes(data))
        self.rejected(PACK.regenerate, self.destination, receipt["bundle_manifest"]["sha256"])

    def test_input_manifest_copy_and_acceptance_copy_tampering_are_rejected(self):
        receipt = self.package()
        bundle = json.loads((self.destination / PACK.BUNDLE_NAME).read_bytes())
        for key in ("input_manifest", "acceptance"):
            target = self.destination / bundle[key]["copy"]["path"]
            data = target.read_bytes()
            target.write_bytes(b"x" * len(data))
            self.rejected(PACK.regenerate, self.destination, receipt["bundle_manifest"]["sha256"])
            target.write_bytes(data)

    def test_config_changed_executable_hash_sources_policy_or_unknown_field_rejected(self):
        receipt = self.package()
        path = self.destination / PACK.CONFIG_NAME
        original = json.loads(path.read_bytes())
        changes = (lambda c: c.update(executable_sha256="0" * 64),
                   lambda c: c["inputs"]["sources"].pop(),
                   lambda c: c["inputs"].update(transfer_seconds=3601),
                   lambda c: c.update(unknown=True))
        for change in changes:
            value = copy.deepcopy(original)
            change(value)
            path.write_bytes(PACK.json_bytes(value))
            self.rejected(PACK.verify, self.destination, receipt["bundle_manifest"]["sha256"])
        path.write_bytes(PACK.json_bytes(original))

    def test_relocation_regenerates_paths_without_original_sources(self):
        receipt = self.package()
        bundle_data = (self.destination / PACK.BUNDLE_NAME).read_bytes()
        relocated = self.root / "moved app"
        self.destination.rename(relocated)
        shutil.rmtree(self.original)
        self.rejected(PACK.verify, relocated, receipt["bundle_manifest"]["sha256"])
        regenerated = PACK.regenerate(relocated, receipt["bundle_manifest"]["sha256"])
        self.assertEqual((relocated / PACK.BUNDLE_NAME).read_bytes(), bundle_data)
        self.assertNotEqual(regenerated["config"]["sha256"], receipt["config"]["sha256"])
        config = json.loads((relocated / PACK.CONFIG_NAME).read_bytes())
        self.assertEqual(config["inputs"]["run_directory"], str(relocated / "firmware-runs"))
        for source in config["inputs"]["sources"]:
            self.assertTrue(source["path"].startswith(str(relocated) + "/"))
        PACK.verify(relocated, receipt["bundle_manifest"]["sha256"])
        again = self.root / "moved again"
        relocated.rename(again)
        PACK.regenerate(again, receipt["bundle_manifest"]["sha256"])
        PACK.verify(again, receipt["bundle_manifest"]["sha256"])

    def test_regeneration_rebuilds_descriptive_config_but_rejects_symlink(self):
        receipt = self.package()
        path = self.destination / PACK.CONFIG_NAME
        path.write_bytes(b"corrupt descriptive configuration\n")
        PACK.regenerate(self.destination, receipt["bundle_manifest"]["sha256"])
        old = self.destination / "old-config"
        path.rename(old)
        path.symlink_to(old)
        self.rejected(PACK.regenerate, self.destination, receipt["bundle_manifest"]["sha256"])
        self.assertEqual(old.read_bytes(), PACK.json_bytes(json.loads(old.read_bytes())))

    def test_verified_bundle_rejects_symlink_or_hardlink_copy(self):
        receipt = self.package()
        source = self.destination / "gp200-studio"
        real = self.destination / "real-app"
        source.rename(real)
        source.symlink_to(real)
        self.rejected(PACK.verify, self.destination, receipt["bundle_manifest"]["sha256"])
        source.unlink()
        real.rename(source)
        os.link(source, self.destination / "hardlinked-app")
        self.rejected(PACK.verify, self.destination, receipt["bundle_manifest"]["sha256"])

    def run_cli(self, args):
        process = subprocess.Popen([sys.executable, "-B", str(HERE / "firmware-resources.py"), *args],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        child = identity(process.pid)
        require_child = child is not None
        stdout, stderr = process.communicate(timeout=30)
        CHILDREN.append({"identity": child, "exit_code": process.returncode,
                         "original_identity_absent": identity(process.pid) != child})
        self.assertTrue(require_child, "Owned Python child start identity must be recorded")
        self.assertTrue(CHILDREN[-1]["original_identity_absent"])
        return process.returncode, stdout, stderr

    def test_cli_creates_and_verifies_only_inert_resources_and_denies_bad_hash(self):
        code, stdout, stderr = self.run_cli(["package", "--manifest", str(self.manifest_path),
                                            "--manifest-sha256", self.manifest_pin["sha256"],
                                            "--destination", str(self.destination)])
        self.assertEqual(code, 0, stderr)
        result = json.loads(stdout)
        self.assertEqual(result["app_executions"], 0)
        self.assertEqual(result["firmware_operations"], 0)
        code, stdout, stderr = self.run_cli(["verify", "--destination", str(self.destination),
                                            "--bundle-manifest-sha256", result["bundle_manifest"]["sha256"]])
        self.assertEqual(code, 0, stderr)
        code, _, _ = self.run_cli(["package", "--manifest", str(self.manifest_path),
                                   "--manifest-sha256", "0" * 64, "--destination", str(self.root / "denied")])
        self.assertEqual(code, 1)
        self.assertFalse((self.root / "denied").exists())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence-dir")
    args = parser.parse_args()
    sources = [HERE / "firmware-resources.py", HERE / "firmware-resources-tests.py",
               HERE.parent / "docs/firmware.md"]
    before = [pin(path) for path in sources]
    test_identity = identity(os.getpid())
    started = time.time_ns()
    evidence = Path(args.evidence_dir) if args.evidence_dir else None
    if evidence:
        evidence.mkdir(mode=0o700)
        (evidence / "START.json").write_bytes(PACK.json_bytes({"schema": 1, "test_process": test_identity,
                                                             "source_pins": before, "started_unix_ns": started}))
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(ResourceTests)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    after = [pin(path) for path in sources]
    unchanged = before == after
    record = {"schema": 1, "test_process": test_identity, "source_pins_before": before,
              "source_pins_after": after, "source_bytes_unchanged": unchanged,
              "tests_run": result.testsRun, "failures": len(result.failures), "errors": len(result.errors),
              "skipped": len(result.skipped), "children": CHILDREN, "finished_unix_ns": time.time_ns(),
              "app_executions": 0, "firmware_operations": 0, "rust_or_native_builds": 0,
              "passed": result.wasSuccessful() and unchanged}
    if evidence:
        (evidence / "RESULT.json").write_bytes(PACK.json_bytes(record))
    return 0 if record["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
