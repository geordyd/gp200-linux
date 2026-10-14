#!/usr/bin/env python3
"""Exercise a native GUI or AppImage through its simulator, without opening MIDI."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import struct
import subprocess
import tempfile


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("executable", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--appimage", action="store_true")
    args = parser.parse_args()
    output = args.output.resolve()
    if output.exists() and any(output.iterdir()):
        parser.error("Use a new or empty --output directory")
    if not shutil.which("xvfb-run"):
        parser.error("xvfb-run is required")
    output.mkdir(parents=True, exist_ok=True)
    entrypoint = [str(args.executable.resolve())]
    if args.appimage:
        entrypoint.append("--appimage-extract-and-run")
    with tempfile.TemporaryDirectory(prefix="gp200-native-smoke-") as temporary:
        isolated = Path(temporary)
        env = os.environ.copy()
        for name in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "GP200_STUDIO_CONFIG", "GP200_FIRMWARE_RESOURCES"):
            env.pop(name, None)
        env.update(XDG_CONFIG_HOME=str(isolated / "config"), XDG_DATA_HOME=str(isolated / "data"),
                   GP200_STUDIO_DATA=str(isolated / "data/gp200-studio"), LIBGL_ALWAYS_SOFTWARE="1")
        for name, arguments in [("version", ["--version"]), ("help", ["--help"]),
                                ("calibration", ["--calibration-helper-info"])]:
            with (output / f"{name}.txt").open("w") as log:
                subprocess.run(entrypoint + arguments, env=env, stdout=log, stderr=log, timeout=30, check=True)
        calibration = json.loads((output / "calibration.txt").read_text())
        if not calibration.get("available") or not isinstance(calibration.get("sha256"), str) or len(calibration["sha256"]) != 64:
            raise ValueError("The packaged calibration helper is unavailable or has no build hash")
        if args.appimage:
            with (output / "cli-help.txt").open("w") as log:
                subprocess.run(entrypoint + ["cli", "--help"], env=env, stdout=log, stderr=log, timeout=30, check=True)
        png = output / "simulator.png"
        command = ["xvfb-run", "-a", "-s", "-screen 0 1920x1080x24"] + entrypoint + [
            "--demo", "--theme", "dark", "--view", "editor", "--size", "960x640", "--screenshot", str(png)]
        with (output / "native-renderer.txt").open("w") as log:
            subprocess.run(command, env=env, stdout=log, stderr=log, timeout=60, check=True)
        header = png.read_bytes()[:24]
        if header[:8] != b"\x89PNG\r\n\x1a\n" or header[12:16] != b"IHDR":
            raise ValueError("Native renderer did not save a PNG")
        dimensions = struct.unpack(">II", header[16:24])
        if dimensions != (960, 640):
            raise ValueError(f"Unexpected screenshot dimensions: {dimensions}")
        (output / "result.json").write_text(json.dumps({"schema": 1, "mode": "simulator",
            "hardware_ports_opened": 0, "appimage": args.appimage, "screenshot_pixels": dimensions}, indent=2) + "\n")
    print(output)


if __name__ == "__main__":
    main()
