#!/usr/bin/env python3
"""Build public AppImage, portable archive and checksums; publish nothing."""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import urllib.request

from installlib import digest

ROOT = Path(__file__).resolve().parent.parent
PACKAGING = ROOT / "packaging"
spec = importlib.util.spec_from_file_location("portable", PACKAGING / "build-portable.py")
portable = importlib.util.module_from_spec(spec)
assert spec.loader is not None
spec.loader.exec_module(portable)


def fetch_tools(folder: Path) -> tuple[dict[str, Path], dict]:
    lock = json.loads((PACKAGING / "appimage-tools.json").read_text())
    folder.mkdir(parents=True, exist_ok=True)
    paths = {}
    for name, entry in lock["tools"].items():
        path = folder / f"{name}-{entry['version']}-x86_64"
        if path.exists():
            if path.is_symlink() or not path.is_file() or digest(path) != entry["sha256"]:
                raise ValueError(f"Cached tool differs from its pinned digest: {path}")
        else:
            # Download bytes to a new file, validate before execution, and never run remote shell code.
            with tempfile.NamedTemporaryFile(dir=folder, prefix=".download-", delete=False) as output:
                temporary = Path(output.name)
                try:
                    with urllib.request.urlopen(entry["url"], timeout=60) as source:
                        shutil.copyfileobj(source, output)
                    output.flush()
                    if digest(temporary) != entry["sha256"]:
                        raise ValueError(f"Tool download differs from its pinned digest: {name}")
                    temporary.replace(path)
                finally:
                    temporary.unlink(missing_ok=True)
        path.chmod(0o755 if entry.get("executable", True) else 0o644)
        paths[name] = path.resolve()
    return paths, lock


def include_runtime_source(work: Path, documentation: Path, tools: dict, lock: dict) -> None:
    """Retain libfuse's corresponding source, its patch and the runtime relink recipe."""
    source = work / "runtime-source"
    revision = lock["tools"]["runtime"]["source_revision"]
    subprocess.run(["git", "init", "--quiet", str(source)], check=True)
    subprocess.run(["git", "-C", str(source), "fetch", "--quiet", "--depth=1",
                    "https://github.com/AppImage/type2-runtime.git", revision], check=True)
    actual = subprocess.check_output(["git", "-C", str(source), "rev-parse", "FETCH_HEAD"], text=True).strip()
    if actual != revision:
        raise ValueError("AppImage runtime source differs from the locked revision")
    destination = documentation / "licenses/appimage-runtime/source"
    destination.mkdir(parents=True)
    with (destination / "type2-runtime.tar").open("wb") as archive:
        subprocess.run(["git", "-C", str(source), "archive", "--format=tar",
                        "--prefix=type2-runtime/", revision], stdout=archive, check=True)
    shutil.copyfile(tools["libfuse-source"], destination / "fuse-3.15.0.tar.xz")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/dist")
    parser.add_argument("--tools-dir", type=Path, default=ROOT / "target/appimage-tools")
    parser.add_argument("--epoch", type=int, default=int(os.environ.get("SOURCE_DATE_EPOCH", "0")))
    args = parser.parse_args()
    if args.epoch < 0:
        parser.error("--epoch must be nonnegative")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    name = f"GP200-Linux-{portable.package_version()}-x86_64"
    appimage, archive, sums = output / f"{name}.AppImage", output / f"{name}.tar.gz", output / "SHA256SUMS"
    if any(path.exists() for path in (appimage, archive, sums)):
        raise ValueError("Use a new output directory; release assets are never overwritten")
    binary, cli, inputs = portable.build_binaries()
    with tempfile.TemporaryDirectory(prefix="gp200-linux-release-") as temporary:
        work = Path(temporary)
        stage = work / name
        manifest = portable.stage_package(binary, cli, stage, args.epoch, inputs)
        tools, lock = fetch_tools(args.tools_dir.resolve())
        appdir = work / "GP200-Linux.AppDir"
        (appdir / "usr/bin").mkdir(parents=True)
        for source in (stage / "bin").iterdir():
            shutil.copy2(source, appdir / "usr/bin" / source.name)
        documentation = appdir / "usr/share/doc/gp200-studio"
        documentation.mkdir(parents=True)
        for source in stage.iterdir():
            if source.name == "bin":
                continue
            if source.is_dir():
                shutil.copytree(source, documentation / source.name)
            else:
                shutil.copy2(source, documentation / source.name)
        desktop = work / "gp200-studio.desktop"
        desktop.write_text((PACKAGING / "gp200-studio.desktop.in").read_text().replace(
            "/usr/bin/env -- @EXEC@", "gp200-studio"))
        env = os.environ.copy()
        env.update(ARCH="x86_64", VERSION=portable.package_version(), SOURCE_DATE_EPOCH=str(args.epoch))
        # winit loads these X11 support libraries at runtime; ELF dependency
        # scanning alone cannot discover them. Graphics drivers remain supplied
        # by the host desktop.
        runtime_libraries = []
        for soname in ("libXcursor.so.1", "libXi.so.6", "libXrandr.so.2", "libXinerama.so.1",
                       "libxkbcommon-x11.so.0", "libxcb-cursor.so.0"):
            library = Path("/usr/lib/x86_64-linux-gnu") / soname
            if not library.is_file():
                raise ValueError(f"Missing Linux packaging runtime dependency: {soname}")
            runtime_libraries.extend(("--library", str(library)))
        subprocess.run([str(tools["linuxdeploy"]), "--appimage-extract-and-run", "--appdir", str(appdir),
                        "--executable", str(appdir / "usr/bin/gp200-gui"),
                        "--executable", str(appdir / "usr/bin/gp200-cli"),
                        "--desktop-file", str(desktop), "--icon-file", str(PACKAGING / "gp200-studio.svg"),
                        "--exclude-library", "libasound.so.2", "--exclude-library", "libstdc++.so.6",
                        "--exclude-library", "libgcc_s.so.1", *runtime_libraries], cwd=work, env=env, check=True)
        # linuxdeploy creates an AppRun symlink to the desktop executable.
        # Replace the link itself rather than overwriting our bin launcher.
        (appdir / "AppRun").unlink(missing_ok=True)
        shutil.copy2(PACKAGING / "AppRun", appdir / "AppRun")
        (appdir / "AppRun").chmod(0o755)
        # Retain full system license texts referenced by Ubuntu's deployed copyright notices.
        shutil.copytree("/usr/share/common-licenses", appdir / "usr/share/common-licenses", dirs_exist_ok=True)
        shutil.copytree(PACKAGING / "licenses/appimage-runtime", documentation / "licenses/appimage-runtime")
        include_runtime_source(work, documentation, tools, lock)
        (documentation / "appimage-tools.json").write_text(json.dumps(lock, indent=2) + "\n")
        app_manifest = dict(manifest)
        app_manifest.pop("files")
        app_manifest["packaging_tools"] = lock
        app_manifest["cli_runtime"] = dict(manifest["cli_runtime"])
        app_manifest["cli_runtime"]["original_compiled_sha256"] = manifest["cli_runtime"]["sha256"]
        app_manifest["cli_runtime"]["sha256"] = digest(appdir / "usr/bin/gp200-cli")
        app_manifest["file_paths_relative_to"] = "AppDir root"
        app_manifest["binary_build_provenance"] += "; AppImage ELF rpaths adjusted by pinned linuxdeploy"
        app_manifest["appimage_files"] = {str(path.relative_to(appdir)): {"sha256": digest(path), "size": path.stat().st_size}
                                         for path in sorted(appdir.rglob("*"))
                                         if path.is_file() and not path.is_symlink()
                                         and path != documentation / "release-manifest.json"}
        (documentation / "release-manifest.json").write_text(json.dumps(app_manifest, indent=2, sort_keys=True) + "\n")
        subprocess.run([str(tools["appimagetool"]), "--appimage-extract-and-run", "--runtime-file", str(tools["runtime"]),
                        "--no-appstream", "--comp", "zstd", str(appdir), str(appimage)],
                       cwd=work, env=env, check=True)
        portable.refresh_manifest(stage, manifest)
        portable.deterministic_archive(stage, archive, name, args.epoch)
    appimage.chmod(0o755)
    sums.write_text("".join(f"{digest(path)}  {path.name}\n" for path in sorted((appimage, archive))))
    # Retained for the existing isolated-prefix smoke tester.
    archive.with_suffix(archive.suffix + ".sha256").write_text(digest(archive) + "  " + archive.name + "\n")
    print(json.dumps({"appimage": str(appimage), "portable": str(archive), "checksums": str(sums)}))


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"GP200 Linux release: {error}", file=sys.stderr)
        sys.exit(1)
