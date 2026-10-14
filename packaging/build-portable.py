#!/usr/bin/env python3
"""Build the public Linux x86_64 portable package from an explicit file allowlist."""
from __future__ import annotations

import argparse
import gzip
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib

from installlib import digest

TARGET = "x86_64-unknown-linux-gnu"
BASELINE_GLIBC = "2.39"
BASELINE_CXX_VERSIONS = {"GLIBCXX": "3.4.32", "CXXABI": "1.3.14"}
FEATURES = "gp200-gui/firmware-linux-completion"
SEMVER_PATTERN = (r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
                  r"(?:-(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)"
                  r"(?:\.(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*)?"
                  r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?")

ROOT = Path(__file__).resolve().parent.parent
PACKAGING = ROOT / "packaging"


def binary_sources() -> dict[str, str]:
    paths = {ROOT / "Cargo.toml", ROOT / "Cargo.lock", ROOT / "rust-toolchain.toml"}
    paths.update((ROOT / "crates").glob("*/Cargo.toml"))
    paths.update((ROOT / "crates").glob("*/build.rs"))
    paths.update((ROOT / "crates").glob("*/src/**/*.rs"))
    paths.update((ROOT / "crates/gp200-cli/src").glob("*.txt"))
    paths.update(p for p in (ROOT / "crates").glob("*/native/**/*") if p.is_file())
    for directory in ["assets/catalog", "assets/demo", "assets/ui/fonts", "assets/firmware", "tests/fixtures/sim",
                      "tests/fixtures/globals", "tests/fixtures/device-assets"]:
        paths.update(p for p in (ROOT / directory).rglob("*") if p.is_file())
    return {str(p.relative_to(ROOT)): digest(p) for p in sorted(paths) if p.is_file()}


def command(args: list[str], **kwargs) -> str:
    return subprocess.check_output(args, cwd=ROOT, text=True, **kwargs).strip()


def cargo_metadata(target: str) -> dict:
    return json.loads(command(["cargo", "metadata", "--locked", "--offline",
                               "--format-version", "1", "--filter-platform", target]))


def copy_notices(metadata: dict, destination: Path) -> list[dict]:
    inventory = []
    upstream = json.loads((PACKAGING / "licenses/upstream-index.json").read_text())["files"]
    for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
        if package["id"] in metadata["workspace_members"]:
            continue
        source = Path(package["manifest_path"]).parent
        notices = []
        for path in sorted(source.rglob("*")):
            if not path.is_file() or path.is_symlink():
                continue
            name = path.name.lower()
            if not (any(marker in name for marker in ("license", "notice", "copying", "copyright"))
                    or name in {"ofl.txt", "ufl.txt", "hack-regular.txt"}):
                continue
            if path.stat().st_size > 1024 * 1024:
                raise ValueError(f"Unusually large notice requires review: {path}")
            relative = path.relative_to(source)
            target = destination / "crates" / f'{package["name"]}-{package["version"]}' / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, target)
            notices.append({"path": str(target.relative_to(destination.parent)),
                            "sha256": digest(target)})
        if package.get("license_file"):
            path = source / package["license_file"]
            if path.is_file() and not any(n["sha256"] == digest(path) for n in notices):
                target = destination / "crates" / f'{package["name"]}-{package["version"]}' / "LICENSE-declared"
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(path, target)
                notices.append({"path": str(target.relative_to(destination.parent)), "sha256": digest(target)})
        if not notices:
            vcs_file = source / ".cargo_vcs_info.json"
            vcs = json.loads(vcs_file.read_text()) if vcs_file.exists() else {}
            repository = (package.get("repository") or "").removeprefix("https://github.com/")
            repository = "/".join(repository.split("/")[:2])
            if repository == "brendanzab/gl-rs":
                repository = "rust-windowing/gl-rs"  # Verified same repository ID7655767.
            for evidence in upstream:
                if evidence["repository"] != repository or evidence["revision"] != vcs.get("git", {}).get("sha1"):
                    continue
                path = PACKAGING / evidence["local_file"]
                if digest(path) != evidence["sha256"]:
                    raise ValueError(f"Pinned upstream notice was modified: {path}")
                target = destination / "crates" / f'{package["name"]}-{package["version"]}' / evidence["path"]
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(path, target)
                notices.append({"path": str(target.relative_to(destination.parent)),
                                "sha256": digest(target), "provenance": evidence})
            if not notices:
                raise ValueError(f'No shipped or pinned upstream license notice found for {package["name"]} {package["version"]}')
        inventory.append({"name": package["name"], "version": package["version"],
                          "spdx": package.get("license"), "notices": notices})
    return inventory


def deterministic_archive(stage: Path, archive: Path, name: str, epoch: int) -> None:
    with archive.open("wb") as output:
        with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.PAX_FORMAT) as tar:
                for path in [stage] + sorted(stage.rglob("*")):
                    relative = path.relative_to(stage)
                    info = tar.gettarinfo(str(path), arcname=str(Path(name) / relative))
                    info.uid = info.gid = 0
                    info.uname = info.gname = ""
                    info.mtime = epoch
                    info.mode = 0o755 if path.is_dir() or os.access(path, os.X_OK) else 0o644
                    info.pax_headers = {}
                    if path.is_file():
                        with path.open("rb") as source:
                            tar.addfile(info, source)
                    else:
                        tar.addfile(info)


def embedded_calibration_runtime(binary: Path) -> dict:
    info = json.loads(command([str(binary.resolve()), "--calibration-helper-info"]))
    if not info.get("available"):
        return {"available": False}
    expected = info.get("sha256")
    if not isinstance(expected, str) or not re.fullmatch(r"[0-9a-f]{64}", expected):
        raise ValueError("Invalid embedded calibration helper hash")
    target_directory = Path(cargo_metadata(TARGET)["target_directory"])
    candidates = list(target_directory.glob("**/build/gp200-assets-*/out/gp200-calibration"))
    helper = next((path for path in candidates if path.is_file() and digest(path) == expected), None)
    if helper is None:
        raise ValueError("Exact embedded helper ELF must be retained to verify its runtime dependencies")
    dependencies = command(["ldd", str(helper)])
    if "not found" in dependencies:
        raise ValueError("Calibration helper has an unresolved runtime dependency")
    version_info = command(["readelf", "--version-info", str(helper)])
    versions = {}
    for family in ("GLIBC", "GLIBCXX", "CXXABI", "GCC"):
        found = set(re.findall(rf"\b{family}_([0-9]+(?:\.[0-9]+)+)", version_info))
        versions[family] = max(found, key=lambda value: tuple(map(int, value.split(".")))) if found else None
    return {"available": True, "sha256": expected, "bytes": helper.stat().st_size,
            "direct_dynamic_dependencies": dependencies.splitlines(),
            "required_maximum_versions": versions,
            "provenance": "ELF bytes matched to the helper hash reported by the packaged application"}


def elf_runtime(binary: Path) -> dict:
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError(f"Executable is unavailable: {binary}")
    with binary.open("rb") as source:
        header = source.read(20)
    if (len(header) != 20 or header[:6] != b"\x7fELF\x02\x01"
            or int.from_bytes(header[18:20], "little") != 62):
        raise ValueError(f"Expected a Linux x86_64 ELF: {binary}")
    dependencies = command(["ldd", str(binary)])
    if "not found" in dependencies:
        raise ValueError(f"Unresolved runtime dependency: {binary}")
    versions = command(["readelf", "--version-info", str(binary)])
    glibc = set(re.findall(r"\bGLIBC_([0-9]+(?:\.[0-9]+)+)", versions))
    return {"sha256": digest(binary), "direct_dynamic_dependencies": dependencies.splitlines(),
            "minimum_glibc_symbol_version": max(glibc, key=version_tuple) if glibc else None}


def version_tuple(value: str) -> tuple[int, ...]:
    return tuple(map(int, value.split(".")))


def package_version() -> str:
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    if not re.fullmatch(SEMVER_PATTERN, version):
        raise ValueError("Package version must be a SemVer release or prerelease")
    return version


def build_binaries() -> tuple[Path, Path, dict]:
    flags = os.environ.get("RUSTFLAGS", "") + os.environ.get("CARGO_ENCODED_RUSTFLAGS", "")
    if "gp200_private_evidence" in flags:
        raise ValueError("Private evidence configuration cannot be used for public packages")
    inputs = binary_sources()
    subprocess.run(["cargo", "build", "--locked", "--release", "-p", "gp200-gui", "-p", "gp200-cli",
                    "--features", FEATURES], cwd=ROOT, check=True)
    if binary_sources() != inputs:
        raise ValueError("Build inputs changed during compilation; rebuild from stable sources")
    target = Path(cargo_metadata(TARGET)["target_directory"])
    return target / "release/gp200-gui", target / "release/gp200-cli", inputs


def stage_package(binary: Path, cli_binary: Path, stage: Path, epoch: int,
                  build_inputs: dict | None = None) -> dict:
    if stage.exists():
        raise ValueError(f"Package staging directory already exists: {stage}")
    gui_runtime, cli_runtime = elf_runtime(binary), elf_runtime(cli_binary)
    calibration = embedded_calibration_runtime(binary)
    cli_calibration = embedded_calibration_runtime(cli_binary)
    if not calibration.get("available") or calibration.get("sha256") != cli_calibration.get("sha256"):
        raise ValueError("Both binaries must contain the same Linux calibration helper")
    requirements = [gui_runtime["minimum_glibc_symbol_version"], cli_runtime["minimum_glibc_symbol_version"],
                    calibration["required_maximum_versions"].get("GLIBC")]
    minimum_glibc = max((value for value in requirements if value), key=version_tuple)
    if version_tuple(minimum_glibc) > version_tuple(BASELINE_GLIBC):
        raise ValueError(f"ELF requires glibc {minimum_glibc}; release baseline is {BASELINE_GLIBC}. Build on Ubuntu 24.04.")
    for family, baseline in BASELINE_CXX_VERSIONS.items():
        required = calibration["required_maximum_versions"].get(family)
        if required and version_tuple(required) > version_tuple(baseline):
            raise ValueError(f"Calibration helper requires {family}_{required}; baseline permits {family}_{baseline}. Build with G++ 13.")
    metadata = cargo_metadata(TARGET)
    stage.mkdir(parents=True)
    (stage / "bin").mkdir()
    for source, destination in [(binary, "bin/gp200-gui"), (cli_binary, "bin/gp200-cli"),
                                (PACKAGING / "gp200-studio", "bin/gp200-studio")]:
        shutil.copyfile(source, stage / destination)
        (stage / destination).chmod(0o755)
    for name in ["install-user.py", "uninstall-user.py", "installlib.py", "gp200-studio.desktop.in",
                 "gp200-studio-mime.xml", "gp200-studio.svg"]:
        shutil.copyfile(PACKAGING / name, stage / name)
    shutil.copyfile(PACKAGING / "README-portable.md", stage / "README.md")
    # Only public instructions are shipped. Never copy the private investigation tree.
    (stage / "docs").mkdir()
    shutil.copyfile(ROOT / "docs/releasing.md", stage / "docs/releasing.md")
    (stage / "docs/firmware.md").write_text((ROOT / "docs/firmware.md").read_text().replace(
        "../assets/firmware/", "firmware/"))
    (stage / "docs/firmware").mkdir()
    for name in ["protocol-contract.json", "loss.md", "recovery.md", "vendor-guidance.md"]:
        shutil.copyfile(ROOT / "assets/firmware" / name, stage / "docs/firmware" / name)
    (stage / "licenses").mkdir()
    notices = [(ROOT / "LICENSE", "GP200-Linux-MIT.txt"),
               (ROOT / "assets/ui/fonts/Cantarell-LICENSE", "Cantarell-OFL.txt"),
               (ROOT / "assets/licenses/neural-amp-modeler-core-MIT.txt", "NAM-core-MIT.txt"),
               (ROOT / "assets/licenses/nam-to-clo-MIT.txt", "NAM-to-CLO-MIT.txt"),
               (ROOT / "assets/licenses/r8brain-MIT.txt", "r8brain-MIT.txt"),
               (ROOT / "assets/licenses/ooura-fft.txt", "Ooura-FFT.txt")]
    for source, name in notices:
        shutil.copyfile(source, stage / "licenses" / name)
    inventory = copy_notices(metadata, stage / "licenses")
    (stage / "dependency-licenses.json").write_text(json.dumps({"schema": 1, "target": TARGET,
        "scope": "locked platform-filtered cargo graph including build dependencies",
        "license_choice": "permissive branch where dual licensed; retain all supplied notices",
        "packages": inventory}, indent=2, sort_keys=True) + "\n")
    manifest = {
        "schema": 1, "application": "GP200 Linux", "version": package_version(),
        "release_label": "public-preview", "target": TARGET, "profile": "release",
        "features": [FEATURES], "source_date_epoch": epoch,
        "revision": command(["git", "rev-parse", "HEAD"]),
        "working_tree_dirty": bool(command(["git", "status", "--porcelain", "--untracked-files=no"])),
        "cargo_lock_sha256": digest(ROOT / "Cargo.lock"),
        "binary_source_inputs": build_inputs if build_inputs is not None else {},
        "binary_build_provenance": "locked release build; source hashes stable before/after" if build_inputs is not None else "existing executables; compile-source correspondence not asserted",
        "toolchain": command(["rustc", "-Vv"]), "build_host_os_release": Path("/etc/os-release").read_text(),
        "supported_baseline": "Ubuntu 24.04 x86_64; glibc 2.39; host ALSA, C++ runtime, Mesa/OpenGL, Wayland or X11",
        "baseline_maximum_cxx_symbol_versions": BASELINE_CXX_VERSIONS,
        "minimum_glibc_symbol_version": minimum_glibc,
        "direct_dynamic_dependencies": gui_runtime["direct_dynamic_dependencies"],
        "cli_runtime": cli_runtime, "embedded_calibration_runtime": calibration,
        "runtime_dlopen_dependencies": ["Mesa/EGL/OpenGL", "Wayland or X11", "D-Bus desktop portal"],
        "firmware_support_resources_included": True,
        "official_firmware_images_included": False,
        "firmware_scope": "Updater and public support resources are embedded. Firmware images are user-supplied; device writes require the app's explicit approval and identity checks.",
        "hardware_validation": "CI is hardware-free. A private development build exercised GP-200 V1.6.3 to V1.8.2 with full-image recovery after an interrupted attempt. These package bytes have not been validated on hardware.",
    }
    refresh_manifest(stage, manifest)
    return manifest


def refresh_manifest(stage: Path, manifest: dict) -> None:
    manifest["files"] = {str(path.relative_to(stage)): {"sha256": digest(path), "size": path.stat().st_size}
                         for path in sorted(stage.rglob("*"))
                         if path.is_file() and path != stage / "release-manifest.json"}
    (stage / "release-manifest.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, help="explicit existing release GUI; provenance is not asserted")
    parser.add_argument("--cli-binary", type=Path)
    parser.add_argument("--output", type=Path, default=ROOT / "target/dist")
    parser.add_argument("--epoch", type=int, default=int(os.environ.get("SOURCE_DATE_EPOCH", "0")))
    args = parser.parse_args()
    if args.epoch < 0 or (args.binary is None) != (args.cli_binary is None):
        parser.error("Use a nonnegative epoch and supply both existing binaries or neither")
    binary, cli, inputs = (args.binary, args.cli_binary, None) if args.binary else build_binaries()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    name = f"GP200-Linux-{package_version()}-x86_64"
    archive = output / f"{name}.tar.gz"
    if archive.exists():
        raise ValueError(f"Refusing to overwrite {archive}")
    with tempfile.TemporaryDirectory(prefix="gp200-package-") as temporary:
        stage = Path(temporary) / name
        stage_package(binary.resolve(), cli.resolve(), stage, args.epoch, inputs)
        deterministic_archive(stage, archive, name, args.epoch)
    archive.with_suffix(archive.suffix + ".sha256").write_text(digest(archive) + "  " + archive.name + "\n")
    print(archive)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"GP200 Linux package: {error}", file=sys.stderr)
        sys.exit(1)
