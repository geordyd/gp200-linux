"""Destructive installer cases use only isolated temporary user prefixes."""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from installlib import InstallError, MANIFEST, digest, install, uninstall, verify_package

PACKAGING = Path(__file__).resolve().parent


class InstallTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="gp200-installer-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.package = self.root / "package"
        self.prefix = self.root / "prefix with spaces ü 50% $quote"
        (self.package / "bin").mkdir(parents=True)
        # Explicit synthetic executable records received arguments and environment.
        (self.package / "bin/gp200-gui").write_text(
            "#!/usr/bin/env python3\nimport os,json,sys\nprint(json.dumps({'args':sys.argv[1:],'wayland':os.environ.get('WAYLAND_DISPLAY')}))\n")
        (self.package / "bin/gp200-gui").chmod(0o755)
        (self.package / "bin/gp200-cli").write_text(
            "#!/usr/bin/env python3\nimport json,sys\nprint(json.dumps({'cli_args':sys.argv[1:]}))\n")
        (self.package / "bin/gp200-cli").chmod(0o755)
        shutil.copyfile(PACKAGING / "gp200-studio", self.package / "bin/gp200-studio")
        (self.package / "bin/gp200-studio").chmod(0o755)
        for name in ["gp200-studio.desktop.in", "gp200-studio-mime.xml", "gp200-studio.svg"]:
            shutil.copyfile(PACKAGING / name, self.package / name)
        (self.package / "README.md").write_text("Synthetic installer test only\n")
        self.write_manifest()

    def write_manifest(self, version="test-1"):
        files = {str(p.relative_to(self.package)): {"sha256": digest(p)}
                 for p in self.package.rglob("*") if p.is_file() and p.name != "release-manifest.json"}
        (self.package / "release-manifest.json").write_text(json.dumps({"schema": 1,
            "version": version, "files": files}))

    def installed_hashes(self):
        return {str(p.relative_to(self.prefix)): digest(p) for p in self.prefix.rglob("*")
                if p.is_file() and p.name != ".gp200-studio-install.lock"}

    def test_install_unicode_spaces_arguments_and_uninstall_preserves_data(self):
        install(self.package, self.prefix, False)
        binary = self.prefix / "bin/gp200-studio"
        preset = "/tmp/My Sounds/蓝色 Lead.prst"
        result = json.loads(subprocess.check_output([str(binary), "--open", preset], text=True))
        self.assertEqual(result["args"], ["--open", preset])
        userdata = self.prefix / "share/gp200-studio/library/personal.json"
        userdata.parent.mkdir(parents=True)
        userdata.write_text("personal preset")
        foreign = self.prefix / "share/doc/gp200-studio/my-note.txt"
        foreign.write_text("my note")
        outcome = uninstall(self.prefix, False)
        self.assertFalse(binary.exists())
        self.assertFalse((self.prefix / MANIFEST).exists())
        self.assertEqual(userdata.read_text(), "personal preset")
        self.assertEqual(foreign.read_text(), "my note")
        self.assertTrue(outcome["userdata_preserved"])

    def test_default_desktop_no_file_and_explicit_x11(self):
        install(self.package, self.prefix, False)
        binary = self.prefix / "bin/gp200-studio"
        env = os.environ | {"WAYLAND_DISPLAY": "test-wayland", "DISPLAY": ":123"}
        no_file = json.loads(subprocess.check_output([str(binary), "--open"], text=True, env=env))
        self.assertEqual(no_file, {"args": [], "wayland": "test-wayland"})
        x11 = json.loads(subprocess.check_output([str(binary), "--x11", "--demo"], text=True, env=env))
        self.assertEqual(x11, {"args": ["--demo"], "wayland": None})
        env.pop("DISPLAY")
        result = subprocess.run([str(binary), "--x11"], capture_output=True, env=env)
        self.assertEqual(result.returncode, 2)

    def test_installed_cli_dispatch_preserves_arguments_and_is_owned(self):
        install(self.package, self.prefix, False)
        binary = self.prefix / "bin/gp200-studio"
        args = ["inspect", "prst", "--input", "/tmp/My Sounds/蓝色 $literal 50%.prst"]
        result = json.loads(subprocess.check_output([str(binary), "cli", *args], text=True))
        self.assertEqual(result, {"cli_args": args})
        runtime = self.prefix / "libexec/gp200-studio/gp200-cli"
        self.assertTrue(runtime.is_file())
        uninstall(self.prefix, False)
        self.assertFalse(runtime.exists())

    def test_package_without_cli_is_rejected_before_install(self):
        (self.package / "bin/gp200-cli").unlink()
        self.write_manifest()
        with self.assertRaisesRegex(InstallError, "required runtime"):
            install(self.package, self.prefix, False)
        self.assertFalse((self.prefix / "bin/gp200-studio").exists())

    def test_upgrade_preserves_user_data_and_updates_owned_files(self):
        install(self.package, self.prefix, False)
        userdata = self.prefix / "share/gp200-studio/manifest.json"
        userdata.parent.mkdir(parents=True)
        userdata.write_bytes(b"unchanged local library")
        (self.package / "README.md").write_text("Synthetic version two")
        self.write_manifest("test-2")
        result = install(self.package, self.prefix, False)
        self.assertEqual(result["version"], "test-2")
        self.assertEqual((self.prefix / "share/doc/gp200-studio/README.md").read_text(), "Synthetic version two")
        self.assertEqual(userdata.read_bytes(), b"unchanged local library")

    def test_refuses_foreign_file_before_any_mutation(self):
        foreign = self.prefix / "bin/gp200-studio"
        foreign.parent.mkdir(parents=True)
        foreign.write_text("existing unowned executable")
        before = self.installed_hashes()
        with self.assertRaisesRegex(InstallError, "unowned"):
            install(self.package, self.prefix, False)
        self.assertEqual(self.installed_hashes(), before)

    def test_changed_owned_file_retained_and_upgrade_refused(self):
        install(self.package, self.prefix, False)
        changed = self.prefix / "share/doc/gp200-studio/README.md"
        changed.write_text("my changes")
        before = self.installed_hashes()
        with self.assertRaisesRegex(InstallError, "modified"):
            install(self.package, self.prefix, False)
        self.assertEqual(self.installed_hashes(), before)
        result = uninstall(self.prefix, False)
        self.assertIn("share/doc/gp200-studio/README.md", result["modified_files_retained"])
        self.assertEqual(changed.read_text(), "my changes")

    def test_symlink_escape_and_manifest_path_traversal_rejected(self):
        outside = self.root / "outside"
        outside.mkdir()
        self.prefix.mkdir()
        (self.prefix / "libexec").symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(InstallError, "symlink"):
            install(self.package, self.prefix, False)
        self.assertEqual(list(outside.iterdir()), [])
        manifest = json.loads((self.package / "release-manifest.json").read_text())
        manifest["files"]["../outside"] = {"sha256": "0" * 64}
        (self.package / "release-manifest.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(InstallError, "relative"):
            verify_package(self.package)

    def test_corrupt_package_rejected_before_install(self):
        (self.package / "README.md").write_text("corrupted")
        with self.assertRaisesRegex(InstallError, "integrity"):
            install(self.package, self.prefix, False)
        self.assertFalse(self.prefix.exists())

    def test_failed_upgrade_rolls_back_exact_original_owned_files(self):
        install(self.package, self.prefix, False)
        before = self.installed_hashes()
        (self.package / "README.md").write_text("new version")
        self.write_manifest("test-2")
        original_replace = os.replace
        count = 0
        def fail_once(source, target):
            nonlocal count
            count += 1
            if count == 5:
                raise OSError("injected rename failure")
            return original_replace(source, target)
        with patch("installlib.os.replace", side_effect=fail_once):
            with self.assertRaisesRegex(OSError, "injected"):
                install(self.package, self.prefix, False)
        self.assertEqual(self.installed_hashes(), before)

    def test_killed_upgrade_recovery_restores_before_retry(self):
        install(self.package, self.prefix, False)
        before = self.installed_hashes()
        (self.package / "README.md").write_text("updated version")
        self.write_manifest("test-2")
        program = '''import os,sys
from pathlib import Path
import installlib
original=installlib.os.replace
count=0
def crash(source,target):
 global count
 count+=1
 if count==5: os._exit(77)
 return original(source,target)
installlib.os.replace=crash
installlib.install(Path(sys.argv[1]),Path(sys.argv[2]),False)
'''
        result = subprocess.run([os.sys.executable, "-c", program, str(self.package), str(self.prefix)],
                                cwd=PACKAGING, capture_output=True)
        self.assertEqual(result.returncode, 77, result.stderr)
        from installlib import recover_interrupted
        outcomes = recover_interrupted(self.prefix)
        self.assertEqual(outcomes, ["restored interrupted install before retry"])
        self.assertEqual(self.installed_hashes(), before)
        result = install(self.package, self.prefix, False)
        self.assertEqual(result["version"], "test-2")

    def test_concurrent_prefix_owner_refuses_install(self):
        from installlib import prefix_lock
        with prefix_lock(self.prefix):
            with self.assertRaisesRegex(InstallError, "Another installer"):
                install(self.package, self.prefix, False)

    @unittest.skipUnless(shutil.which("desktop-file-validate"), "desktop validator unavailable")
    def test_desktop_entry_validates_with_quoted_user_prefix(self):
        install(self.package, self.prefix, False)
        result = subprocess.run(["desktop-file-validate", str(
            self.prefix / "share/applications/gp200-studio.desktop")], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    @unittest.skipUnless(shutil.which("gio"), "GIO desktop launcher unavailable")
    def test_real_desktop_exec_expansion_preserves_unicode_and_space_path(self):
        install(self.package, self.prefix, False)
        preset = self.root / "我的 presets" / "Lead 50%.prst"
        preset.parent.mkdir()
        preset.write_text("synthetic desktop launch file")
        result = subprocess.run(["gio", "launch", str(
            self.prefix / "share/applications/gp200-studio.desktop"), str(preset)],
            capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = [line for line in result.stdout.splitlines() if line.startswith('{"args"')]
        self.assertTrue(lines, result.stdout + result.stderr)
        self.assertEqual(json.loads(lines[-1])["args"], ["--open", str(preset)])


if __name__ == "__main__":
    unittest.main()
