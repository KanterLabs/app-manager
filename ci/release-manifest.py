#!/usr/bin/env python3
"""Build and verify the app-manager release-tools schema-1 manifest.

This is deliberately a small contract adapter, rather than a copy of the
shared release publisher.  It owns local artifact inventory, checksums, and
evidence.  Publication and forge-specific receipt handling remain in the
infrastructure release-tools integration (INFRA164).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tarfile
from typing import Any, Iterable


SCHEMA = 1
SHA1_RE = re.compile(r"^[0-9a-f]{40}$")
SAFE_VERSION_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9.+_-]*$")
REPOSITORY_RE = re.compile(r"^[^/\s]+/[^/\s]+$")
FEDORA_BASE_DIGEST = "sha256:2651ad743908b60acca6099f7a06ccb3602eb63a1b08cbd00cfa441a4d4afbe0"
NATIVE_PACKAGE_NAMES = [
    "gcc-c++",
    "make",
    "pkgconf-pkg-config",
    "openssl-devel",
    "gtk3-devel",
    "webkit2gtk4.1-devel",
    "libsoup3-devel",
    "librsvg2-devel",
    "libappindicator-gtk3-devel",
    "dbus-daemon",
    "dbus-devel",
    "dbus-tools",
    "rpm-build",
    "openssl",
    "webkitgtk6.0",
    "python3-pyyaml",
    "git",
    "openssh-clients",
    "openssh-server",
    "xorg-x11-server-Xvfb",
]


class ManifestError(ValueError):
    """A user-facing contract or artifact error."""


def fail(message: str) -> "NoReturn":
    raise ManifestError(message)


def _safe_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        fail(f"{field} must be a non-empty string")
    if any(ord(char) < 32 or ord(char) == 127 for char in value):
        fail(f"{field} contains a control character")
    return value


def validate_project(value: Any) -> str:
    text = _safe_string(value, "project")
    if "/" in text or "\\" in text:
        fail("project must not contain a path separator")
    return text


def validate_repository(value: Any) -> str:
    text = _safe_string(value, "repository")
    if not REPOSITORY_RE.fullmatch(text):
        fail("repository must use owner/name form")
    owner, name = text.split("/", 1)
    if owner in {".", ".."} or name in {".", ".."}:
        fail("repository contains an invalid path component")
    return text


def validate_version(value: Any) -> str:
    text = _safe_string(value, "version")
    if not SAFE_VERSION_RE.fullmatch(text):
        fail("version must contain only safe release characters")
    return text


def validate_commit(value: Any) -> str:
    if not isinstance(value, str) or not SHA1_RE.fullmatch(value):
        fail("commit must be an exact lower-case 40-character Git SHA")
    return value


def validate_relative_path(value: Any, field: str = "path") -> str:
    text = _safe_string(value, field)
    path = PurePosixPath(text)
    if (
        "\\" in text
        or path.is_absolute()
        or not path.parts
        or any(part in {"", ".", ".."} for part in path.parts)
    ):
        fail(f"{field} must be a safe relative POSIX path")
    return "/".join(path.parts)


def validate_asset_name(value: Any, field: str = "name") -> str:
    text = _safe_string(value, field)
    if "/" in text or "\\" in text or text in {".", ".."}:
        fail(f"{field} must be a flat asset name")
    return text


def sha256_file(path: Path) -> tuple[int, str]:
    digest = hashlib.sha256()
    size = 0
    try:
        with path.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
                size += len(chunk)
    except OSError as exc:
        fail(f"could not read artifact {path}: {exc}")
    return size, digest.hexdigest()


def regular_files(root: Path) -> list[tuple[str, Path]]:
    """Return every regular artifact under root and reject symlink escapes."""

    if not root.is_dir():
        fail(f"artifact directory does not exist: {root}")
    files: list[tuple[str, Path]] = []
    for candidate in sorted(root.rglob("*")):
        relative = candidate.relative_to(root)
        relative_text = validate_relative_path(relative.as_posix(), "artifact path")
        if candidate.is_symlink():
            fail(f"symlink is not a release artifact: {relative_text}")
        if candidate.is_dir():
            continue
        if not candidate.is_file():
            fail(f"release artifact is not a regular file: {relative_text}")
        files.append((relative_text, candidate))
    if not files:
        fail(f"artifact directory is empty: {root}")
    return files


def target_key(path: str) -> str:
    """Derive a stable target identifier from a relative artifact path.

    The primary RPM target uses the shared ``linux-amd64``/``linux-arm64``
    naming convention so consumers can select it by host architecture. Other
    package formats retain a suffix while remaining deterministic across
    versioned filenames.
    """

    lower = path.lower()
    if "x86_64" in lower or "amd64" in lower or "x64" in lower:
        architecture = "amd64"
    elif "aarch64" in lower or "arm64" in lower:
        architecture = "arm64"
    else:
        architecture = "unknown"
    if lower.endswith(".rpm"):
        return f"linux-{architecture}"
    if lower.endswith(".deb"):
        return f"linux-{architecture}-deb"
    if lower.endswith(".appimage"):
        return f"linux-{architecture}-appimage"
    if lower.endswith((".tar.gz", ".tar.xz", ".tar.zst")):
        return f"linux-{architecture}-archive"
    stem = path
    for suffix in (".zip",):
        if stem.endswith(suffix):
            stem = stem[: -len(suffix)]
            break
    stem = re.sub(r"[^A-Za-z0-9]+", "-", stem).strip("-").lower()
    return stem or "artifact"


def inventory(artifact_dir: Path) -> list[dict[str, Any]]:
    files = regular_files(artifact_dir)
    seen_targets: set[str] = set()
    seen_names: set[str] = set()
    targets: list[dict[str, Any]] = []
    for path, absolute in files:
        name = validate_asset_name(PurePosixPath(path).name, "asset name")
        if name in seen_names:
            fail(f"duplicate release asset name: {name}")
        seen_names.add(name)
        target = "checksums" if name == "SHA256SUMS" else target_key(path)
        if target in seen_targets:
            # A second extension or nested path can otherwise collapse to the
            # same human-readable key. Keep the manifest unambiguous.
            suffix = 2
            base = target
            while f"{base}-{suffix}" in seen_targets:
                suffix += 1
            target = f"{base}-{suffix}"
        seen_targets.add(target)
        size, digest = sha256_file(absolute)
        targets.append(
            {
                "target": target,
                "path": path,
                "name": name,
                "size": size,
                "sha256": digest,
                "protected": name == "SHA256SUMS",
            }
        )
    return targets


def write_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    try:
        temporary.write_text(
            json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        temporary.replace(path)
    except OSError as exc:
        fail(f"could not write {path}: {exc}")


def _command_version(command: str, flag: str = "--version") -> str | None:
    try:
        result = subprocess.run(
            [command, flag],
            capture_output=True,
            check=False,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    output = (result.stdout or result.stderr).strip()
    return output.splitlines()[0] if output else None


def _os_release() -> dict[str, str]:
    path = Path("/etc/os-release")
    if not path.is_file():
        return {}
    values: dict[str, str] = {}
    for line in path.read_text(encoding="utf-8").splitlines():
        if "=" not in line or line.startswith("#"):
            continue
        key, value = line.split("=", 1)
        values[key] = value.strip().strip('"')
    return {
        key: values[key]
        for key in ("ID", "VERSION_ID", "PRETTY_NAME")
        if key in values
    }


def builder_command(args: argparse.Namespace) -> dict[str, Any]:
    toolchain: dict[str, str] = {}
    for command in ("rustc", "cargo", "rustup", "tauri-driver", "node", "npm"):
        version = _command_version(command)
        if version:
            toolchain[command] = version

    container: dict[str, str] = {}
    for env_name, field in (
        ("APP_MANAGER_BUILDER_IMAGE", "image"),
        ("APP_MANAGER_BUILDER_BASE_DIGEST", "base_image_digest"),
        ("CARGO_TARGET_DIR", "target_dir"),
        ("CARGO_BUILD_JOBS", "cargo_build_jobs"),
    ):
        value = os.environ.get(env_name)
        if value:
            container[field] = value

    value: dict[str, Any] = {
        "schema": SCHEMA,
        "kind": "app-manager.builder-evidence",
        "os": _os_release(),
        "toolchain": toolchain,
        "container": container,
        "builder_contract": {
            "base_image_digest": FEDORA_BASE_DIGEST,
            "node_version": "22.22.1",
            "rust_version": "1.96.0",
            "rustup_version": "1.29.0",
            "native_dependencies": {
                "names": NATIVE_PACKAGE_NAMES,
                "versions_pinned": False,
                "source": "ci/Dockerfile.fedora",
            },
            "runtime_smoke_tools": ["openssl", "WebKitWebDriver", "tauri-driver 2.1.0"],
        },
    }

    if args.archive:
        archive = Path(args.archive)
        if not archive.is_file():
            fail(f"portable archive does not exist: {archive}")
        try:
            with tarfile.open(archive, mode="r:*") as handle:
                members = sorted(
                    member.name
                    for member in handle.getmembers()
                    if member.isfile()
                )
        except (OSError, tarfile.TarError) as exc:
            fail(f"could not inspect portable archive {archive}: {exc}")
        for member in members:
            validate_relative_path(member, "portable archive member")
        required_members = {
            "app-manager-desktop",
            "appctl",
            "app-manager-broker",
            "LICENSE",
            "README.md",
        }
        archive_basenames = {PurePosixPath(member).name for member in members}
        if not required_members <= archive_basenames:
            missing = ", ".join(sorted(required_members - archive_basenames))
            fail(f"portable archive is missing required members: {missing}")
        value["portable_archive"] = {
            "name": archive.name,
            "members": members,
            "required_executables": [
                "app-manager-desktop",
                "appctl",
                "app-manager-broker",
            ],
            "required_documents": ["LICENSE", "README.md"],
        }

    write_json(Path(args.output), value)
    return value


def read_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except FileNotFoundError:
        fail(f"manifest does not exist: {path}")
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        fail(f"could not read manifest {path}: {exc}")
    if not isinstance(value, dict):
        fail("manifest root must be an object")
    return value


def manifest_command(args: argparse.Namespace) -> dict[str, Any]:
    artifact_dir = Path(args.artifacts).resolve()
    project = validate_project(args.project)
    repository = validate_repository(args.repository)
    version = validate_version(args.version)
    commit = validate_commit(args.commit)
    tag = args.tag or f"v{version}"
    tag = _safe_string(tag, "release.tag")
    if any(char in tag for char in "/\\{}") or tag != tag.strip():
        fail("release.tag contains an unsafe character")
    targets = inventory(artifact_dir)
    protected_assets = ["SHA256SUMS"] if any(item["name"] == "SHA256SUMS" for item in targets) else []
    if not protected_assets:
        fail("release artifact directory must contain SHA256SUMS")
    value: dict[str, Any] = {
        "schema": SCHEMA,
        "project": project,
        "repository": repository,
        "release": {"version": version, "tag": tag, "commit": commit},
        "protected_assets": protected_assets,
        "targets": targets,
    }
    write_json(Path(args.output), value)
    return value


def verify_manifest(manifest_path: Path, artifact_dir: Path) -> dict[str, Any]:
    value = read_json(manifest_path)
    if value.get("schema") != SCHEMA:
        fail("manifest schema must be 1")
    project = validate_project(value.get("project"))
    repository = validate_repository(value.get("repository"))
    release = value.get("release")
    if not isinstance(release, dict):
        fail("manifest release must be an object")
    version = validate_version(release.get("version"))
    tag = _safe_string(release.get("tag"), "release.tag")
    commit = validate_commit(release.get("commit"))
    protected = value.get("protected_assets")
    if not isinstance(protected, list) or any(not isinstance(item, str) for item in protected):
        fail("manifest protected_assets must be a list of strings")
    protected_names = {validate_asset_name(item, "protected asset") for item in protected}
    targets = value.get("targets")
    if not isinstance(targets, list) or not targets:
        fail("manifest targets must be a non-empty list")

    actual = {path: absolute for path, absolute in regular_files(artifact_dir)}
    listed_paths: set[str] = set()
    listed_names: set[str] = set()
    listed_targets: set[str] = set()
    normalized_targets: list[dict[str, Any]] = []
    for index, target in enumerate(targets):
        if not isinstance(target, dict):
            fail(f"manifest target {index} must be an object")
        target_name = _safe_string(target.get("target"), f"targets[{index}].target")
        path = validate_relative_path(target.get("path"), f"targets[{index}].path")
        name = validate_asset_name(target.get("name"), f"targets[{index}].name")
        if target_name in listed_targets or path in listed_paths or name in listed_names:
            fail(f"manifest target {index} duplicates a target, path, or name")
        listed_targets.add(target_name)
        listed_paths.add(path)
        listed_names.add(name)
        if path not in actual:
            fail(f"manifest target is missing from artifact directory: {path}")
        if name != PurePosixPath(path).name:
            fail(f"manifest target name does not match path basename: {path}")
        expected_size = target.get("size")
        expected_hash = target.get("sha256")
        if not isinstance(expected_size, int) or expected_size < 0:
            fail(f"manifest target has an invalid size: {name}")
        if not isinstance(expected_hash, str) or not re.fullmatch(r"[0-9a-f]{64}", expected_hash):
            fail(f"manifest target has an invalid sha256: {name}")
        size, digest = sha256_file(actual[path])
        if size != expected_size or digest != expected_hash:
            fail(f"manifest checksum mismatch: {name}")
        is_protected = target.get("protected")
        if not isinstance(is_protected, bool):
            fail(f"manifest target protected must be boolean: {name}")
        if is_protected != (name in protected_names):
            fail(f"manifest protected flag mismatch: {name}")
        normalized_targets.append(
            {
                "target": target_name,
                "path": path,
                "name": name,
                "size": size,
                "sha256": digest,
                "protected": is_protected,
            }
        )

    if protected_names - listed_names:
        fail("manifest protected asset is not listed in targets")
    unexpected = sorted(set(actual) - listed_paths)
    if unexpected:
        fail(f"artifact directory contains unlisted files: {', '.join(unexpected)}")
    return {
        "schema": SCHEMA,
        "project": project,
        "repository": repository,
        "release": {"version": version, "tag": tag, "commit": commit},
        "protected_assets": sorted(protected_names),
        "targets": normalized_targets,
    }


def evidence_command(args: argparse.Namespace) -> dict[str, Any]:
    manifest_path = Path(args.manifest)
    value = verify_manifest(manifest_path, Path(args.artifacts).resolve())
    _manifest_size, manifest_sha256 = sha256_file(manifest_path)
    evidence = {
        "schema": SCHEMA,
        "kind": "app-manager.package-evidence",
        "project": value["project"],
        "repository": value["repository"],
        "release": value["release"],
        "source_commit": value["release"]["commit"],
        "manifest_sha256": manifest_sha256,
        "manifest": manifest_path.name,
        "artifacts": value["targets"],
        "publication": {
            "assembled": True,
            "canonical_integration": "INFRA164",
            "published": False,
        },
    }
    if args.builder_metadata:
        builder = read_json(Path(args.builder_metadata))
        evidence["builder"] = builder
    write_json(Path(args.output), evidence)
    return evidence


def discover_version(root: Path) -> str:
    override = os.environ.get("RELEASE_VERSION") or os.environ.get("APP_VERSION")
    if override:
        return validate_version(override)
    cargo = root / "Cargo.toml"
    if cargo.is_file():
        in_workspace_package = False
        for line in cargo.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if stripped.startswith("["):
                in_workspace_package = stripped == "[workspace.package]"
                continue
            if in_workspace_package:
                match = re.match(r'version\s*=\s*"([^"]+)"', stripped)
                if match:
                    return validate_version(match.group(1))
    package = root / "package.json"
    if package.is_file():
        value = json.loads(package.read_text(encoding="utf-8"))
        if isinstance(value, dict) and value.get("version"):
            return validate_version(value["version"])
    fail("could not determine version; set RELEASE_VERSION")


def parser() -> argparse.ArgumentParser:
    command = argparse.ArgumentParser(description=__doc__)
    subparsers = command.add_subparsers(dest="command", required=True)

    manifest = subparsers.add_parser("manifest", help="write a schema-1 release manifest")
    manifest.add_argument("--artifacts", required=True)
    manifest.add_argument("--project", default="app-manager")
    manifest.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", "KanterLabs/app-manager"))
    manifest.add_argument("--version", required=True)
    manifest.add_argument("--commit", required=True)
    manifest.add_argument("--tag")
    manifest.add_argument("--output", required=True)

    verify = subparsers.add_parser("verify", help="verify a schema-1 manifest and its artifact directory")
    verify.add_argument("--manifest", required=True)
    verify.add_argument("--artifacts", required=True)

    evidence = subparsers.add_parser("evidence", help="write machine-readable package evidence")
    evidence.add_argument("--manifest", required=True)
    evidence.add_argument("--artifacts", required=True)
    evidence.add_argument("--builder-metadata")
    evidence.add_argument("--output", required=True)

    builder = subparsers.add_parser("builder", help="write machine-readable builder evidence")
    builder.add_argument("--archive")
    builder.add_argument("--output", required=True)

    version = subparsers.add_parser("version", help="print the workspace version")
    version.add_argument("--root", default=".")
    return command


def main(argv: Iterable[str] | None = None) -> int:
    args = parser().parse_args(list(argv) if argv is not None else None)
    try:
        if args.command == "manifest":
            value = manifest_command(args)
        elif args.command == "verify":
            value = verify_manifest(Path(args.manifest), Path(args.artifacts).resolve())
        elif args.command == "evidence":
            value = evidence_command(args)
        elif args.command == "builder":
            value = builder_command(args)
        else:
            print(discover_version(Path(args.root).resolve()))
            return 0
    except (ManifestError, OSError, UnicodeError, json.JSONDecodeError) as exc:
        print(json.dumps({"ok": False, "error": str(exc)}, sort_keys=True), file=sys.stderr)
        return 2
    if args.command in {"manifest", "verify", "evidence"}:
        print(json.dumps({"ok": True, "schema": SCHEMA, "release": value.get("release")}, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
