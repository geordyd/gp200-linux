#!/usr/bin/env python3
"""Verify a real portable archive in an isolated prefix, opening no MIDI port."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import wave
import zipfile

from installlib import digest


def run(command: list[str], env: dict, log: Path, timeout: int = 30) -> dict:
    started = time.monotonic()
    with log.open("w") as output:
        process = subprocess.Popen(command, env=env, stdout=output, stderr=output,
                                   start_new_session=True)
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            raise RuntimeError(f"Smoke command timed out: {command[0]}") from None
    if code:
        raise RuntimeError(f"Smoke command returned {code}: {command[0]}; see {log}")
    return {"exit_code": code, "milliseconds": round((time.monotonic() - started) * 1000),
            "log": str(log), "log_sha256": digest(log)}


def png_info(path: Path) -> dict:
    with path.open("rb") as source:
        data = source.read(33)
    if data[:8] != b"\x89PNG\r\n\x1a\n" or data[12:16] != b"IHDR":
        raise RuntimeError(f"Screenshot is not PNG: {path}")
    width, height = struct.unpack(">II", data[16:24])
    if (width, height) != (960, 640):
        raise RuntimeError(f"Unexpected screenshot dimensions: {width}x{height}")
    return {"path": str(path), "sha256": digest(path), "width": width, "height": height}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--preset", type=Path, help="optional explicit inert .prst import fixture")
    args = parser.parse_args()
    archive = args.archive.resolve()
    output = args.output.resolve()
    if output.exists() and any(output.iterdir()):
        parser.error("--output must be new or empty; smoke evidence is never overwritten")
    output.mkdir(parents=True, exist_ok=True)
    checksum_file = archive.with_suffix(archive.suffix + ".sha256")
    expected = checksum_file.read_text().split()[0]
    if digest(archive) != expected:
        raise RuntimeError("Archive checksum mismatch")
    result = {"schema": 1, "archive": str(archive), "archive_sha256": expected,
              "hardware_requests": 0, "checks": {}, "screenshots": []}
    with tempfile.TemporaryDirectory(prefix="gp200-package-smoke-") as temporary:
        folder = Path(temporary)
        with tarfile.open(archive, "r:gz") as source:
            source.extractall(folder / "extracted", filter="data")
        packages = list((folder / "extracted").iterdir())
        if len(packages) != 1 or not packages[0].is_dir():
            raise RuntimeError("Unexpected portable archive root")
        package = packages[0]
        prefix = folder / "installed ü with spaces 50% $literal"
        env = os.environ.copy()
        env.pop("GP200_STUDIO_CONFIG", None)
        env.pop("GP200_FIRMWARE_RESOURCES", None)
        env.update(XDG_DATA_HOME=str(folder / "data"), XDG_CONFIG_HOME=str(folder / "config"),
                   GP200_STUDIO_DATA=str(folder / "data/gp200-studio"), LIBGL_ALWAYS_SOFTWARE="1")
        result["release_manifest"] = json.loads((package / "release-manifest.json").read_text())
        install = [sys.executable, str(package / "install-user.py"), "--prefix", str(prefix)]
        result["checks"]["install"] = run(install, env, output / "install.txt")
        executable = str(prefix / "bin/gp200-studio")
        result["checks"]["version"] = run([executable, "--version"], env, output / "version.txt", 5)
        result["checks"]["help"] = run([executable, "--help"], env, output / "help.txt", 5)
        result["checks"]["cli_version"] = run([executable, "cli", "--version"], env, output / "cli-version.txt", 5)
        result["checks"]["cli_help"] = run([executable, "cli", "--help"], env, output / "cli-help.txt", 5)
        wave_fixture = folder / "离线 audio $literal 50%.wav"
        with wave.open(str(wave_fixture), "wb") as audio:
            audio.setnchannels(1)
            audio.setsampwidth(2)
            audio.setframerate(44100)
            audio.writeframes(struct.pack("<hhhh", 0, 1000, -1000, 0))
        inspect_log = output / "cli-offline-inspect.json"
        result["checks"]["cli_offline_inspect"] = run([executable, "cli", "inspect", "wav", "--input", str(wave_fixture)], env, inspect_log, 5)
        inspected = json.loads(inspect_log.read_text())
        if not inspected.get("ok") or inspected["data"].get("hardware_access") is not False or inspected["data"]["sha256"] != digest(wave_fixture):
            raise RuntimeError("Installed offline CLI did not validate the selected WAV exactly")
        support = output / "installed-support.zip"
        result["checks"]["cli_support_export"] = run([executable, "cli", "support", "export", "--output", str(support)], env, output / "cli-support.json", 5)
        with zipfile.ZipFile(support) as archive:
            if set(archive.namelist()) != {"summary.json", "profiles.json", "manifest.json"}:
                raise RuntimeError("Default support bundle included unexpected user data")
        result["support_bundle_sha256"] = digest(support)
        if args.preset:
            cli_fixture = folder / "离线 My presets $literal 50%.prst"
            shutil.copyfile(args.preset.resolve(), cli_fixture)
            native_log = output / "cli-offline-prst.json"
            result["checks"]["cli_offline_prst"] = run([executable, "cli", "inspect", "prst", "--input", str(cli_fixture)], env, native_log, 5)
            native = json.loads(native_log.read_text())
            if not native.get("ok") or native["data"]["sha256"] != digest(cli_fixture):
                raise RuntimeError("Installed CLI native preset inspection differs from the selected file")
        embedded = result["release_manifest"].get("embedded_calibration_runtime")
        if embedded is not None:
            info_log = output / "calibration-helper-info.json"
            result["checks"]["embedded_calibration"] = run(
                [executable, "--calibration-helper-info"], env, info_log, 5)
            info = json.loads(info_log.read_text())
            if info.get("available") != embedded.get("available") or (
                embedded.get("available") and info.get("sha256") != embedded.get("sha256")
            ):
                raise RuntimeError("Installed helper identity differs from the packaged runtime manifest")
        if shutil.which("desktop-file-validate"):
            result["checks"]["desktop"] = run(["desktop-file-validate", str(
                prefix / "share/applications/gp200-studio.desktop")], env, output / "desktop.txt", 5)
        if env.get("WAYLAND_DISPLAY") or env.get("WAYLAND_SOCKET"):
            png = output / "installed-wayland-light.png"
            result["checks"]["wayland"] = run([executable, "--theme", "light", "--view", "settings",
                "--size", "960x640", "--screenshot", str(png)], env, output / "wayland.txt", 20)
            result["screenshots"].append(png_info(png))
        if not shutil.which("xvfb-run"):
            raise RuntimeError("X11 packaging smoke requires xvfb-run")
        png = output / "installed-x11-dark.png"
        command = ["xvfb-run", "-a", "-s", "-screen 0 1920x1080x24", executable,
                   "--x11", "--theme", "dark", "--view", "library", "--size", "960x640",
                   "--screenshot", str(png)]
        if args.preset:
            fixture = folder / "我的 presets" / "Lead 50%.prst"
            fixture.parent.mkdir()
            shutil.copyfile(args.preset.resolve(), fixture)
            command += ["--open", str(fixture)]
            result["import_fixture_sha256"] = digest(fixture)
        result["checks"]["x11"] = run(command, env, output / "x11.txt", 20)
        result["screenshots"].append(png_info(png))
        userdata = prefix / "share/gp200-studio/preserved-smoke-data.bin"
        userdata.parent.mkdir(parents=True, exist_ok=True)
        userdata.write_bytes(b"unrelated local application data\0")
        userhash = digest(userdata)
        result["checks"]["upgrade"] = run(install, env, output / "upgrade.txt")
        if digest(userdata) != userhash:
            raise RuntimeError("Upgrade changed userdata")
        result["checks"]["uninstall"] = run([sys.executable, str(package / "uninstall-user.py"),
            "--prefix", str(prefix)], env, output / "uninstall.txt")
        if digest(userdata) != userhash or (prefix / "bin/gp200-studio").exists():
            raise RuntimeError("Uninstall ownership/data preservation check failed")
        result["userdata_preserved"] = True
    (output / "smoke-result.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(json.dumps({"result": str(output / "smoke-result.json"),
                      "checks_passed": list(result["checks"]), "hardware_requests": 0}))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print(f"GP200 Linux package smoke: {error}", file=sys.stderr)
        sys.exit(1)
