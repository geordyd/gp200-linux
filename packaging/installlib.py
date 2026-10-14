"""Ownership-safe, non-root install of a verified portable package."""
from __future__ import annotations

import hashlib
import contextlib
import fcntl
import json
import os
from pathlib import Path, PurePosixPath
import shlex
import shutil
import subprocess
import tempfile

APP = "gp200-studio"
MANIFEST = Path("libexec/gp200-studio/installed-files.json")


class InstallError(Exception):
    pass


@contextlib.contextmanager
def prefix_lock(prefix: Path):
    prefix.mkdir(parents=True, exist_ok=True)
    path = safe_destination(prefix, Path(".gp200-studio-install.lock"))
    descriptor = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise InstallError("Another installer owns this prefix; finish it first") from error
        yield
    finally:
        os.close(descriptor)


def recover_interrupted(prefix: Path) -> list[str]:
    recovered = []
    for stage in sorted(prefix.glob(".gp200-install-*")):
        if stage.is_symlink() or not stage.is_dir():
            raise InstallError(f"Unexpected install staging entry: {stage}")
        journal_path = stage / "transaction.json"
        if not journal_path.exists():
            # Preflight/staging never changes installed files before this marker.
            shutil.rmtree(stage)
            recovered.append("discarded incomplete staging")
            continue
        journal = json.loads(journal_path.read_text())
        if journal.get("schema") != 1 or journal.get("prefix") != str(prefix):
            raise InstallError(f"Invalid interrupted install journal: {stage}")
        entries = journal["files"]
        for entry in entries:
            safe_destination(prefix, owned_path(entry["name"]))
        committed = all((prefix / entry["name"]).is_file() and
                        digest(prefix / entry["name"]) == entry["new_sha256"]
                        if entry["new_sha256"] is not None else
                        not (prefix / entry["name"]).exists() for entry in entries)
        if committed:
            shutil.rmtree(stage)
            recovered.append("finished committed install cleanup")
            continue
        # Validate all rollback candidates before changing any destination.
        for entry in entries:
            name = entry["name"]
            current = prefix / name
            backup = safe_destination(stage, Path("old") / owned_path(name))
            if current.exists() and (not current.is_file() or digest(current) not in {
                    entry["old_sha256"], entry["new_sha256"]}):
                raise InstallError(f"Interrupted destination was modified; preserve it first: {current}")
            if backup.exists() and (not backup.is_file() or digest(backup) != entry["old_sha256"]):
                raise InstallError(f"Interrupted rollback copy was modified: {backup}")
            if entry["old_sha256"] is not None and not backup.exists() and (
                    not current.is_file() or digest(current) != entry["old_sha256"]):
                raise InstallError(f"Interrupted install lacks its original rollback copy: {name}")
        for entry in reversed(entries):
            current = prefix / entry["name"]
            backup = stage / "old" / entry["name"]
            if backup.exists():
                if current.exists():
                    current.unlink()
                current.parent.mkdir(parents=True, exist_ok=True)
                os.replace(backup, current)
            elif entry["old_sha256"] is None and current.exists():
                current.unlink()
        shutil.rmtree(stage)
        recovered.append("restored interrupted install before retry")
    return recovered


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def relative_file(value: str) -> Path:
    path = PurePosixPath(value)
    if not value or path.is_absolute() or any(p in {"", ".", ".."} for p in value.split("/")):
        raise InstallError(f"Invalid package-relative path: {value!r}")
    return Path(*path.parts)


def owned_path(value: str) -> Path:
    path = relative_file(value)
    if value in {"bin/gp200-studio", "share/applications/gp200-studio.desktop",
                 "share/mime/packages/gp200-studio.xml",
                 "share/icons/hicolor/scalable/apps/gp200-studio.svg"}:
        return path
    if value.startswith(("libexec/gp200-studio/", "share/doc/gp200-studio/")):
        return path
    raise InstallError(f"Installed manifest contains an unowned path: {value!r}")


def safe_destination(prefix: Path, path: Path) -> Path:
    current = prefix
    for component in path.parts:
        current = current / component
        if current.is_symlink():
            raise InstallError(f"Refusing destination symlink: {current}")
        if current.exists() and current != prefix / path and not current.is_dir():
            raise InstallError(f"Destination parent is not a directory: {current}")
    return current


def selected_prefix(value: str | None) -> Path:
    if os.geteuid() == 0:
        raise InstallError("Run this user-prefix installer as your ordinary account, without sudo.")
    path = Path(value).expanduser() if value else Path.home() / ".local"
    if not path.is_absolute():
        raise InstallError("--prefix must be an absolute path")
    path = path.resolve()
    if "=" in str(path) or any(ord(c) < 32 for c in str(path)):
        raise InstallError("The desktop executable path cannot contain '=' or control characters")
    return path


def desktop_exec(path: Path) -> str:
    # Two escaping layers: desktop string values, then Exec argument quoting.
    value = str(path).replace("%", "%%")
    for character in ('\\', '"', '`', '$'):
        value = value.replace(character, "\\" + character)
    return '"' + value.replace("\\", "\\\\") + '"'


def read_owned(prefix: Path) -> dict:
    path = safe_destination(prefix, MANIFEST)
    if not path.exists():
        return {"schema": 1, "files": {}}
    if not path.is_file():
        raise InstallError(f"Install manifest is not a regular file: {path}")
    try:
        value = json.loads(path.read_text())
        if value["schema"] != 1 or not isinstance(value["files"], dict):
            raise ValueError("Unsupported schema")
        for name, info in value["files"].items():
            owned_path(name)
            if not isinstance(info["sha256"], str) or len(info["sha256"]) != 64:
                raise ValueError("Invalid file hash")
        return value
    except (ValueError, KeyError, TypeError) as error:
        raise InstallError(f"Invalid install manifest: {error}") from error


def verify_package(package: Path) -> dict:
    manifest = json.loads((package / "release-manifest.json").read_text())
    if manifest.get("schema") != 1 or not isinstance(manifest.get("files"), dict):
        raise InstallError("Unsupported release manifest")
    for name, info in manifest["files"].items():
        path = safe_destination(package, relative_file(name))
        if not path.is_file() or digest(path) != info["sha256"]:
            raise InstallError(f"Package integrity check failed: {name}")
    required = {"bin/gp200-gui", "bin/gp200-cli", "bin/gp200-studio", "gp200-studio.desktop.in",
                "gp200-studio-mime.xml", "gp200-studio.svg", "README.md"}
    if not required.issubset(manifest["files"]):
        raise InstallError("Package lacks required runtime/desktop files")
    return manifest


def cache_update(prefix: Path) -> list[str]:
    notices = []
    for binary, path in [("update-desktop-database", "share/applications"),
                         ("update-mime-database", "share/mime")]:
        executable = shutil.which(binary)
        if executable and (prefix / path).exists():
            try:
                result = subprocess.run([executable, str(prefix / path)], capture_output=True,
                                        text=True, timeout=20, check=False)
            except subprocess.TimeoutExpired:
                notices.append(f"{binary} timed out; refresh the desktop cache manually")
                continue
            if result.returncode:
                notices.append(f"{binary} returned {result.returncode}: {result.stderr.strip()}")
        elif not executable:
            notices.append(f"{binary} is unavailable; refresh the desktop cache after installing it")
    return notices


def install(package: Path, prefix: Path, refresh: bool = True) -> dict:
    verify_package(package)
    with prefix_lock(prefix):
        recovered = recover_interrupted(prefix)
        result = _install(package, prefix, refresh)
        result["recovered_transactions"] = recovered
        return result


def _install(package: Path, prefix: Path, refresh: bool) -> dict:
    release = verify_package(package)
    prefix.mkdir(parents=True, exist_ok=True)
    old = read_owned(prefix)
    source_files = {
        "libexec/gp200-studio/gp200-gui": package / "bin/gp200-gui",
        "libexec/gp200-studio/gp200-cli": package / "bin/gp200-cli",
        "libexec/gp200-studio/gp200-studio": package / "bin/gp200-studio",
        "share/mime/packages/gp200-studio.xml": package / "gp200-studio-mime.xml",
        "share/icons/hicolor/scalable/apps/gp200-studio.svg": package / "gp200-studio.svg",
    }
    for name in release["files"]:
        if name.startswith(("docs/", "licenses/")) or name in {
                "README.md", "dependency-licenses.json"}:
            source_files["share/doc/gp200-studio/" + name] = package / name
    source_files["share/doc/gp200-studio/release-manifest.json"] = package / "release-manifest.json"
    generated = {
        "bin/gp200-studio": ("#!/bin/sh\nexec " + shlex.quote(str(
            prefix / "libexec/gp200-studio/gp200-studio")) + ' "$@"\n').encode(),
        "share/applications/gp200-studio.desktop": (
            package / "gp200-studio.desktop.in").read_text().replace(
                "@EXEC@", desktop_exec(prefix / "bin/gp200-studio")).encode(),
    }
    names = set(source_files) | set(generated)
    # Preflight every replacement/removal before touching an installed file.
    for name in names | set(old["files"]):
        path = safe_destination(prefix, owned_path(name))
        if path.exists():
            if not path.is_file():
                raise InstallError(f"Refusing non-file destination: {path}")
            if name not in old["files"]:
                raise InstallError(f"Refusing to overwrite an unowned file: {path}")
            if digest(path) != old["files"][name]["sha256"]:
                raise InstallError(f"Installed file was modified; preserve or move it first: {path}")
    with tempfile.TemporaryDirectory(prefix=".gp200-install-", dir=prefix) as temporary:
        stage = Path(temporary)
        new = {"schema": 1, "version": release["version"], "files": {}}
        for name in sorted(names):
            path = stage / "new" / name
            path.parent.mkdir(parents=True, exist_ok=True)
            if name in source_files:
                shutil.copyfile(source_files[name], path)
            else:
                path.write_bytes(generated[name])
            mode = 0o755 if name.startswith(("bin/", "libexec/")) else 0o644
            path.chmod(mode)
            with path.open("rb") as source:
                os.fsync(source.fileno())
            new["files"][name] = {"sha256": digest(path), "mode": mode}
        manifest_stage = stage / "new" / MANIFEST
        manifest_stage.parent.mkdir(parents=True, exist_ok=True)
        manifest_stage.write_text(json.dumps(new, indent=2, sort_keys=True) + "\n")
        with manifest_stage.open("rb") as source:
            os.fsync(source.fileno())
        changes = sorted(names | set(old["files"])) + [str(MANIFEST)]
        journal = {"schema": 1, "prefix": str(prefix), "files": [
            {"name": name,
             "old_sha256": digest(prefix / name) if (prefix / name).is_file() else None,
             "new_sha256": digest(stage / "new" / name) if (stage / "new" / name).is_file() else None}
            for name in changes]}
        (stage / "transaction.json").write_text(json.dumps(journal, indent=2) + "\n")
        with (stage / "transaction.json").open("rb") as source:
            os.fsync(source.fileno())
        completed = []
        try:
            for name in changes:
                destination = safe_destination(prefix, owned_path(name))
                destination.parent.mkdir(parents=True, exist_ok=True)
                backup = stage / "old" / name
                if destination.exists():
                    backup.parent.mkdir(parents=True, exist_ok=True)
                    os.replace(destination, backup)
                completed.append(name)
                replacement = stage / "new" / name
                if replacement.exists():
                    os.replace(replacement, destination)
        except BaseException:
            for name in reversed(completed):
                destination = prefix / name
                if destination.exists():
                    destination.unlink()
                backup = stage / "old" / name
                if backup.exists():
                    os.replace(backup, destination)
            raise
    return {"prefix": str(prefix), "installed": len(names), "version": release["version"],
            "notices": cache_update(prefix) if refresh else []}


def uninstall(prefix: Path, refresh: bool = True) -> dict:
    with prefix_lock(prefix):
        recovered = recover_interrupted(prefix)
        result = _uninstall(prefix, refresh)
        result["recovered_transactions"] = recovered
        return result


def _uninstall(prefix: Path, refresh: bool) -> dict:
    owned = read_owned(prefix)
    removed, retained = [], []
    # Validate every path before deleting any file.
    for name in owned["files"]:
        safe_destination(prefix, owned_path(name))
    for name, info in owned["files"].items():
        path = safe_destination(prefix, owned_path(name))
        if path.exists():
            if not path.is_file() or digest(path) != info["sha256"]:
                retained.append(name)
                continue
            path.unlink()
        removed.append(name)
    manifest = safe_destination(prefix, MANIFEST)
    if retained:
        owned["files"] = {name: owned["files"][name] for name in retained}
        temporary = manifest.with_suffix(".json.tmp")
        if temporary.exists() or temporary.is_symlink():
            raise InstallError(f"Refusing an existing temporary manifest: {temporary}")
        temporary.write_text(json.dumps(owned, indent=2, sort_keys=True) + "\n")
        os.replace(temporary, manifest)
    elif manifest.exists():
        manifest.unlink()
    # Remove only empty parents of owned files. Never traverse application data.
    for name in removed:
        parent = (prefix / name).parent
        while parent != prefix:
            try:
                parent.rmdir()
            except OSError:
                break
            parent = parent.parent
    return {"prefix": str(prefix), "removed": len(removed), "modified_files_retained": retained,
            "userdata_preserved": True, "notices": cache_update(prefix) if refresh else []}
