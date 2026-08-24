#!/usr/bin/env python3
"""Bind a packed Stasis SDK to its real native act-settle-inspect gate."""

from __future__ import annotations

import argparse
import base64
import gzip
import hashlib
import io
import json
import math
import posixpath
import re
import tarfile
import tempfile
import zlib
from pathlib import Path


VERSION_RE = re.compile(
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)\."
    r"(?:0|[1-9][0-9]*)"
    r"(?:-alpha\.(?:0|[1-9][0-9]*))?"
)
REVISION_RE = re.compile(r"[0-9a-f]{40}")
SHA256_RE = re.compile(r"[0-9a-f]{64}")
POSITIVE_INTEGER_RE = re.compile(r"[1-9][0-9]*")
PACKAGE_NAME = "@oxhq/stasis"
REPOSITORY = "https://github.com/oxhq/stasis.git"
GATE_NAME = "sdk-act-settle-inspect"
MAX_TARBALL_BYTES = 32 * 1024 * 1024
MAX_MEMBER_BYTES = 16 * 1024 * 1024
MAX_TOTAL_MEMBER_BYTES = 64 * 1024 * 1024
MAX_TAR_MEMBERS = 256
MAX_EXPANDED_TAR_BYTES = MAX_TOTAL_MEMBER_BYTES + 4 * 1024 * 1024
STREAM_CHUNK_BYTES = 1024 * 1024
INSTALL_LIFECYCLE_SCRIPTS = {
    "preinstall",
    "install",
    "postinstall",
    "prepublish",
    "preprepare",
    "prepare",
    "postprepare",
}
RUNTIME_DEPENDENCY_FIELDS = {
    "dependencies",
    "optionalDependencies",
    "peerDependencies",
    "bundledDependencies",
    "bundleDependencies",
}
UPSTREAM_IDENTITIES = {
    "servo_repository": "https://github.com/servo/servo.git",
    "servo_revision": "0d579bd5aab6df3764fad805427254751632a6e4",
    "pliego_repository": "https://github.com/oxhq/pliego.git",
    "pliego_revision": "556c774242b272b11bc60999449c5debff1ad20f",
    "pliego_servo_merge_base": "313b6d5ecc113b08010ce434140db3ca5abcc71c",
}


class NpmReleaseError(RuntimeError):
    pass


class StrictJsonError(ValueError):
    pass


def _strict_json_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    value: dict[str, object] = {}
    for key, item in pairs:
        if key in value:
            raise StrictJsonError(f"duplicate object key {key!r}")
        value[key] = item
    return value


def _strict_json_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed):
        raise StrictJsonError(f"non-finite number {value!r}")
    return parsed


def _strict_json_constant(value: str) -> object:
    raise StrictJsonError(f"non-finite number {value!r}")


def strict_json_loads(source: str | bytes, context: str) -> object:
    if isinstance(source, bytes):
        try:
            source = source.decode("utf-8")
        except UnicodeError as error:
            raise NpmReleaseError(f"{context} is not UTF-8: {error}") from error
    try:
        return json.loads(
            source,
            object_pairs_hook=_strict_json_object,
            parse_float=_strict_json_float,
            parse_constant=_strict_json_constant,
        )
    except (ValueError, RecursionError) as error:
        raise NpmReleaseError(f"{context} is invalid JSON: {error}") from error


def strict_json_dumps(value: object, **kwargs: object) -> str:
    return json.dumps(value, allow_nan=False, **kwargs)


def require_json_string(value: object, field: str) -> str:
    if type(value) is not str:
        raise NpmReleaseError(f"invalid {field}: expected a JSON string")
    return value


def fullmatch(pattern: re.Pattern[str], value: object, field: str) -> str:
    value = require_json_string(value, field)
    if pattern.fullmatch(value) is None:
        raise NpmReleaseError(f"invalid {field}: {value!r}")
    return value


def expected_source_identities(revision: str) -> dict[str, str]:
    fullmatch(REVISION_RE, revision, "revision")
    return {
        **UPSTREAM_IDENTITIES,
        "stasis_repository": REPOSITORY,
        "stasis_revision": revision,
    }


def publish_tag(version: str) -> str:
    """Return the one permitted npm dist-tag for a supported release version."""
    fullmatch(VERSION_RE, version, "version")
    return "alpha" if "-alpha." in version else "latest"


def tarball_name(version: str) -> str:
    fullmatch(VERSION_RE, version, "version")
    return f"oxhq-stasis-{version}.tgz"


def proof_name(version: str) -> str:
    fullmatch(VERSION_RE, version, "version")
    return f"stasis-{version}-typescript-act-settle-inspect.json"


def hash_file(filename: Path, algorithm: str) -> str:
    digest = hashlib.new(algorithm)
    with filename.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def sha512_integrity(filename: Path) -> str:
    digest = hashlib.sha512()
    with filename.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return "sha512-" + base64.b64encode(digest.digest()).decode("ascii")


def tarball_binding(package: Path) -> dict[str, str]:
    return {
        "name": package.name,
        "sha256": hash_file(package, "sha256"),
        "integrity": sha512_integrity(package),
    }


def require_tarball_binding(
    value: object, expected: dict[str, str], context: str
) -> dict[str, object]:
    fields = {"name", "sha256", "integrity"}
    if type(value) is not dict or set(value) != fields:
        raise NpmReleaseError(f"{context} has an unexpected schema")
    for field in fields:
        require_json_string(value[field], f"{context} {field}")
    if value != expected:
        raise NpmReleaseError(f"{context} does not match the selected package tarball")
    return value


def require_source_identities(
    value: object, revision: str, context: str
) -> dict[str, object]:
    expected = expected_source_identities(revision)
    if type(value) is not dict or set(value) != set(expected):
        raise NpmReleaseError(f"{context} has an unexpected schema")
    for field in expected:
        require_json_string(value[field], f"{context} {field}")
    if value != expected:
        raise NpmReleaseError(f"{context} does not identify the exact Stasis and upstream sources")
    return value


WINDOWS_RESERVED_COMPONENTS = {
    "con",
    "prn",
    "aux",
    "nul",
    *(f"com{number}" for number in range(1, 10)),
    *(f"lpt{number}" for number in range(1, 10)),
}
WINDOWS_FORBIDDEN_CHARACTERS = set('<>:"\\|?*')


def canonical_member_path(member: tarfile.TarInfo) -> tuple[str, str]:
    name = member.name
    if not name or not name.isascii():
        raise NpmReleaseError(f"SDK tarball contains a non-ASCII or empty member: {name!r}")
    if member.isdir() and name.endswith("/"):
        canonical = name[:-1]
        if not canonical or canonical.endswith("/"):
            raise NpmReleaseError(f"SDK tarball contains a non-canonical member: {name!r}")
    else:
        canonical = name
    if (
        not canonical
        or canonical.startswith("/")
        or (name != canonical and name != canonical + "/")
        or "\\" in canonical
        or posixpath.normpath(canonical) != canonical
    ):
        raise NpmReleaseError(f"SDK tarball contains a non-canonical member: {name!r}")
    components = canonical.split("/")
    for component in components:
        if (
            not component
            or component in {".", ".."}
            or component != component.strip()
            or component.endswith(".")
            or any(ord(character) < 32 or ord(character) == 127 for character in component)
            or WINDOWS_FORBIDDEN_CHARACTERS.intersection(component)
            or component.split(".", 1)[0].lower() in WINDOWS_RESERVED_COMPONENTS
        ):
            raise NpmReleaseError(
                f"SDK tarball member is not portable across supported filesystems: {name!r}"
            )
    return canonical, "/".join(component.lower() for component in components)


def copy_bounded_gzip(package: Path, destination: io.BufferedIOBase) -> None:
    try:
        with package.open("rb") as source:
            if source.read(3) != b"\x1f\x8b\x08":
                raise NpmReleaseError("SDK tarball is not a gzip stream")
            source.seek(0)
            decompressor = zlib.decompressobj(wbits=16 + zlib.MAX_WBITS)
            expanded_bytes = 0
            while chunk := source.read(STREAM_CHUNK_BYTES):
                pending = chunk
                while pending:
                    remaining = MAX_EXPANDED_TAR_BYTES - expanded_bytes
                    decompressed = decompressor.decompress(
                        pending, min(STREAM_CHUNK_BYTES, remaining + 1)
                    )
                    expanded_bytes += len(decompressed)
                    if expanded_bytes > MAX_EXPANDED_TAR_BYTES:
                        raise NpmReleaseError(
                            "SDK tarball exceeds the maximum expanded tar-stream size"
                        )
                    destination.write(decompressed)
                    pending = decompressor.unconsumed_tail
                    if decompressor.eof:
                        break
                if decompressor.eof:
                    if decompressor.unused_data or source.read(1):
                        raise NpmReleaseError(
                            "SDK tarball has trailing data or another gzip member"
                        )
                    break
            flushed = decompressor.flush(
                min(STREAM_CHUNK_BYTES, MAX_EXPANDED_TAR_BYTES - expanded_bytes + 1)
            )
            expanded_bytes += len(flushed)
            if expanded_bytes > MAX_EXPANDED_TAR_BYTES:
                raise NpmReleaseError(
                    "SDK tarball exceeds the maximum expanded tar-stream size"
                )
            destination.write(flushed)
            if not decompressor.eof:
                raise NpmReleaseError("SDK tarball gzip stream ended before its validated trailer")
    except NpmReleaseError:
        raise
    except (EOFError, OSError, gzip.BadGzipFile, zlib.error) as error:
        raise NpmReleaseError(f"SDK tarball is not a valid bounded gzip stream: {error}") from error
    if expanded_bytes == 0:
        raise NpmReleaseError("SDK tarball expands to an empty tar stream")
    destination.seek(0)


def verify_tarball(package: Path, version: str) -> None:
    expected_name = tarball_name(version)
    if package.is_symlink() or not package.is_file() or package.name != expected_name:
        raise NpmReleaseError(f"SDK tarball must be the regular file {expected_name}")
    package_size = package.stat().st_size
    if package_size <= 0 or package_size > MAX_TARBALL_BYTES:
        raise NpmReleaseError(
            f"SDK tarball size {package_size} is outside the allowed range "
            f"1..{MAX_TARBALL_BYTES} bytes"
        )
    file_names: set[str] = set()
    directory_names: set[str] = set()
    paths: dict[str, tuple[str, str]] = {}
    metadata_bytes: bytes | None = None
    total_member_bytes = 0
    member_count = 0
    with tempfile.SpooledTemporaryFile(max_size=8 * 1024 * 1024, mode="w+b") as expanded:
        copy_bounded_gzip(package, expanded)
        try:
            expanded.seek(0, io.SEEK_END)
            expanded_size = expanded.tell()
            if expanded_size < 1024 or expanded_size % 512 != 0:
                raise NpmReleaseError(
                    "SDK tarball expanded stream must be at least 1024 bytes and 512-byte aligned"
                )
            expanded.seek(0)
            with tarfile.open(fileobj=expanded, mode="r:") as archive:
                for member in archive:
                    member_count += 1
                    if member_count > MAX_TAR_MEMBERS:
                        raise NpmReleaseError("SDK tarball exceeds the maximum member count")
                    canonical, collision_key = canonical_member_path(member)
                    if collision_key in paths:
                        previous = paths[collision_key][0]
                        raise NpmReleaseError(
                            "SDK tarball contains a cross-platform path collision: "
                            f"{previous!r} and {member.name!r}"
                        )
                    if not member.isfile() and not member.isdir():
                        raise NpmReleaseError(
                            f"SDK tarball contains an unsupported entry: {member.name!r}"
                        )
                    kind = "file" if member.isfile() else "directory"
                    paths[collision_key] = (canonical, kind)
                    if member.size < 0 or member.size > MAX_MEMBER_BYTES:
                        raise NpmReleaseError(
                            f"SDK tarball member size is outside the allowed range: {member.name!r}"
                        )
                    if member.isdir():
                        if member.size != 0:
                            raise NpmReleaseError(
                                f"SDK tarball directory has a nonzero size: {member.name!r}"
                            )
                        directory_names.add(canonical)
                        continue
                    total_member_bytes += member.size
                    if total_member_bytes > MAX_TOTAL_MEMBER_BYTES:
                        raise NpmReleaseError(
                            "SDK tarball exceeds the maximum total uncompressed member size"
                        )
                    member_source = archive.extractfile(member)
                    if member_source is None:
                        raise NpmReleaseError(f"SDK tarball member cannot be read: {member.name!r}")
                    captured = bytearray() if canonical == "package/package.json" else None
                    actual_size = 0
                    while True:
                        chunk = member_source.read(STREAM_CHUNK_BYTES)
                        if not chunk:
                            break
                        actual_size += len(chunk)
                        if actual_size > member.size or actual_size > MAX_MEMBER_BYTES:
                            raise NpmReleaseError(
                                f"SDK tarball member exceeds its declared size: {member.name!r}"
                            )
                        if captured is not None:
                            captured.extend(chunk)
                    if actual_size != member.size:
                        raise NpmReleaseError(
                            f"SDK tarball member is truncated: {member.name!r}"
                        )
                    if captured is not None:
                        metadata_bytes = bytes(captured)
                    file_names.add(canonical)
                parsed_tar_end = archive.offset
            padding_size = expanded_size - parsed_tar_end
            if (
                parsed_tar_end < 0
                or parsed_tar_end % 512 != 0
                or padding_size < 1024
                or padding_size % 512 != 0
            ):
                raise NpmReleaseError(
                    "SDK tarball does not end with canonical 512-byte zero padding"
                )
            expanded.seek(parsed_tar_end)
            while padding := expanded.read(STREAM_CHUNK_BYTES):
                if any(padding):
                    raise NpmReleaseError(
                        "SDK tarball contains nonzero data after the parsed tar archive"
                    )
        except NpmReleaseError:
            raise
        except (EOFError, OSError, tarfile.TarError) as error:
            raise NpmReleaseError(f"SDK tarball is not a valid bounded tar stream: {error}") from error
    for collision_key in paths:
        components = collision_key.split("/")
        for index in range(1, len(components)):
            parent = "/".join(components[:index])
            if parent in paths and paths[parent][1] == "file":
                raise NpmReleaseError(
                    f"SDK tarball file is also a parent directory: {paths[parent][0]!r}"
                )
    allowed_directories = {"package", "package/dist"}
    unknown_directories = directory_names - allowed_directories
    if unknown_directories:
        raise NpmReleaseError(
            f"SDK tarball contains unknown directories: {sorted(unknown_directories)}"
        )
    required = {
        "package/package.json",
        "package/README.md",
        "package/LICENSE",
        "package/dist/index.js",
        "package/dist/index.d.ts",
    }
    if not required.issubset(file_names):
        raise NpmReleaseError(f"SDK tarball is missing required files: {sorted(required - file_names)}")
    for name in file_names:
        if not name.startswith("package/"):
            raise NpmReleaseError(f"SDK tarball contains an unsafe member: {name!r}")
        if name not in required and not name.startswith("package/dist/"):
            raise NpmReleaseError(
                f"SDK tarball contains a development-only or unknown file: {name!r}"
            )
    if metadata_bytes is None:
        raise NpmReleaseError("SDK package metadata cannot be read")
    metadata = strict_json_loads(metadata_bytes, "SDK package metadata")
    if not isinstance(metadata, dict):
        raise NpmReleaseError("SDK package metadata is not an object")
    if "tag" in metadata:
        raise NpmReleaseError("SDK package metadata must not override the publish dist-tag")
    metadata_name = require_json_string(metadata.get("name"), "SDK package name")
    metadata_version = require_json_string(metadata.get("version"), "SDK package version")
    if metadata_name != PACKAGE_NAME or metadata_version != version:
        raise NpmReleaseError("SDK package name/version differs from the selected release")
    expected_entrypoints = {
        "type": "module",
        "main": "./dist/index.js",
        "types": "./dist/index.d.ts",
        "exports": {".": {"types": "./dist/index.d.ts", "import": "./dist/index.js"}},
    }
    if any(metadata.get(key) != value for key, value in expected_entrypoints.items()):
        raise NpmReleaseError("SDK package public entrypoint metadata is not exact")
    repository = metadata.get("repository")
    if repository != {"type": "git", "url": REPOSITORY}:
        raise NpmReleaseError("SDK package repository does not identify oxhq/stasis")
    publish = metadata.get("publishConfig")
    if (
        type(publish) is not dict
        or set(publish) != {"access", "tag", "provenance"}
        or require_json_string(publish.get("access"), "SDK publishConfig access") != "public"
        or require_json_string(publish.get("tag"), "SDK publishConfig tag")
        != publish_tag(version)
        or publish.get("provenance") is not True
    ):
        raise NpmReleaseError(
            "SDK publishConfig is not the exact public provenance and dist-tag policy"
        )
    scripts = metadata.get("scripts", {})
    if not isinstance(scripts, dict) or INSTALL_LIFECYCLE_SCRIPTS.intersection(scripts):
        raise NpmReleaseError("SDK package must not define install lifecycle scripts")
    populated_runtime_dependencies = {
        field
        for field in RUNTIME_DEPENDENCY_FIELDS
        if metadata.get(field) not in (None, {}, [])
    }
    if populated_runtime_dependencies:
        raise NpmReleaseError(
            "SDK package must not add runtime dependency fields: "
            f"{sorted(populated_runtime_dependencies)}"
        )


def parse_gate_log(
    gate_log: Path, package: Path, version: str, revision: str
) -> dict[str, object]:
    matches: list[dict[str, object]] = []
    try:
        lines = gate_log.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise NpmReleaseError(f"cannot read SDK gate log: {error}") from error
    for line_number, line in enumerate(lines, start=1):
        try:
            value = strict_json_loads(line, f"SDK gate log line {line_number}")
        except NpmReleaseError:
            if line.lstrip().startswith(("{", "[")):
                raise
            continue
        if isinstance(value, dict) and value.get("gate") == GATE_NAME:
            matches.append(value)
    if len(matches) != 1:
        raise NpmReleaseError("SDK gate log must contain exactly one structured success proof")
    value = matches[0]
    required = {
        "gate",
        "package",
        "revision",
        "source",
        "tarball",
        "binary",
        "binarySha256",
        "virtualElapsedNs",
        "wallElapsedMs",
        "closeResponseAndEof",
    }
    if set(value) != required:
        raise NpmReleaseError("SDK gate success has an unexpected schema")
    for field in ("gate", "package", "revision", "binary", "binarySha256", "virtualElapsedNs"):
        require_json_string(value.get(field), f"SDK gate {field}")
    if value["package"] != f"{PACKAGE_NAME}@{version}" or value["revision"] != revision:
        raise NpmReleaseError("SDK gate did not run the selected package/revision")
    require_source_identities(value.get("source"), revision, "SDK gate source identities")
    require_tarball_binding(
        value.get("tarball"), tarball_binding(package), "SDK gate tarball binding"
    )
    if value.get("virtualElapsedNs") != "10000000000" or value.get("closeResponseAndEof") is not True:
        raise NpmReleaseError("SDK gate did not prove exact virtual time and graceful EOF")
    wall_elapsed = value.get("wallElapsedMs")
    if (
        not isinstance(wall_elapsed, (int, float))
        or isinstance(wall_elapsed, bool)
        or not math.isfinite(wall_elapsed)
    ):
        raise NpmReleaseError("SDK gate wall duration is not numeric")
    if wall_elapsed < 0 or wall_elapsed >= 8_000:
        raise NpmReleaseError("SDK gate did not meet the eight-second wall guard")
    if not value["binary"]:
        raise NpmReleaseError("SDK gate did not identify the tested native executable")
    fullmatch(SHA256_RE, value["binarySha256"], "tested native binary SHA-256")
    return value


def create_proof(
    *,
    package: Path,
    gate_log: Path,
    output: Path,
    version: str,
    revision: str,
    native_binary_sha256: str,
    run_id: str,
    run_attempt: str,
) -> dict[str, object]:
    fullmatch(REVISION_RE, revision, "revision")
    fullmatch(SHA256_RE, native_binary_sha256, "native binary SHA-256")
    fullmatch(POSITIVE_INTEGER_RE, run_id, "workflow run ID")
    fullmatch(POSITIVE_INTEGER_RE, run_attempt, "workflow run attempt")
    verify_tarball(package, version)
    gate = parse_gate_log(gate_log, package, version, revision)
    if gate["binarySha256"] != native_binary_sha256:
        raise NpmReleaseError("SDK gate tested a different native binary digest")
    document: dict[str, object] = {
        "schema": 2,
        "gate": GATE_NAME,
        "package": f"{PACKAGE_NAME}@{version}",
        "revision": revision,
        "workflowRunId": run_id,
        "workflowRunAttempt": run_attempt,
        "source": gate["source"],
        "tarball": gate["tarball"],
        "nativeBinarySha256": native_binary_sha256,
        "gateLogSha256": hash_file(gate_log, "sha256"),
    }
    if output.exists():
        raise NpmReleaseError(f"refusing to overwrite SDK gate proof: {output}")
    output.write_text(
        strict_json_dumps(document, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )
    return document


def verify_directory(directory: Path, version: str) -> tuple[Path, Path]:
    expected = {tarball_name(version), proof_name(version)}
    if not directory.is_dir():
        raise NpmReleaseError(f"SDK artifact directory does not exist: {directory}")
    entries = list(directory.iterdir())
    actual = {entry.name for entry in entries}
    if any(entry.is_symlink() or not entry.is_file() for entry in entries) or actual != expected:
        raise NpmReleaseError(
            f"SDK artifact inventory differs: missing={sorted(expected - actual)} extra={sorted(actual - expected)}"
        )
    return directory / tarball_name(version), directory / proof_name(version)


def verify_proof(
    *,
    directory: Path,
    version: str,
    revision: str,
    native_binary_sha256: str,
    run_id: str,
    run_attempt: str,
) -> dict[str, object]:
    fullmatch(REVISION_RE, revision, "revision")
    fullmatch(SHA256_RE, native_binary_sha256, "native binary SHA-256")
    fullmatch(POSITIVE_INTEGER_RE, run_id, "workflow run ID")
    fullmatch(POSITIVE_INTEGER_RE, run_attempt, "workflow run attempt")
    package, proof = verify_directory(directory, version)
    verify_tarball(package, version)
    try:
        proof_text = proof.read_text(encoding="utf-8")
        document = strict_json_loads(proof_text, "SDK gate proof")
    except (OSError, UnicodeError, NpmReleaseError) as error:
        raise NpmReleaseError(f"cannot parse SDK gate proof: {error}") from error
    expected_keys = {
        "schema",
        "gate",
        "package",
        "revision",
        "workflowRunId",
        "workflowRunAttempt",
        "source",
        "tarball",
        "nativeBinarySha256",
        "gateLogSha256",
    }
    if not isinstance(document, dict) or set(document) != expected_keys:
        raise NpmReleaseError("SDK gate proof has an unexpected schema")
    if type(document.get("schema")) is not int:
        raise NpmReleaseError("SDK gate proof schema must be a JSON integer")
    for field in (
        "gate",
        "package",
        "revision",
        "workflowRunId",
        "workflowRunAttempt",
        "nativeBinarySha256",
        "gateLogSha256",
    ):
        require_json_string(document.get(field), f"SDK gate proof {field}")
    require_source_identities(
        document.get("source"), revision, "SDK gate proof source identities"
    )
    require_tarball_binding(
        document.get("tarball"), tarball_binding(package), "SDK gate proof tarball binding"
    )
    expected = {
        "schema": 2,
        "gate": GATE_NAME,
        "package": f"{PACKAGE_NAME}@{version}",
        "revision": revision,
        "workflowRunId": run_id,
        "workflowRunAttempt": run_attempt,
        "source": expected_source_identities(revision),
        "tarball": tarball_binding(package),
        "nativeBinarySha256": native_binary_sha256,
    }
    for key, value in expected.items():
        if document.get(key) != value:
            raise NpmReleaseError(f"SDK gate proof field does not match: {key}")
    fullmatch(SHA256_RE, document["gateLogSha256"], "SDK gate log SHA-256")
    return document


def self_test() -> None:
    version = "0.2.0"
    revision = "2" * 40
    binary_digest = "3" * 64

    def expect_error(label: str, operation: object) -> None:
        try:
            operation()  # type: ignore[operator]
        except NpmReleaseError:
            return
        raise AssertionError(f"self-test did not reject {label}")

    with tempfile.TemporaryDirectory(prefix="stasis-npm-release-self-test-") as temporary:
        root = Path(temporary)
        metadata = {
            "name": PACKAGE_NAME,
            "version": version,
            "type": "module",
            "main": "./dist/index.js",
            "types": "./dist/index.d.ts",
            "exports": {".": {"types": "./dist/index.d.ts", "import": "./dist/index.js"}},
            "repository": {"type": "git", "url": REPOSITORY},
            "publishConfig": {"access": "public", "tag": "latest", "provenance": True},
            "scripts": {"prepack": "tsc -p tsconfig.build.json"},
        }

        def write_package(
            destination: Path,
            *,
            package_metadata: dict[str, object] | None = None,
            metadata_source: bytes | None = None,
            extra_files: dict[str, bytes] | None = None,
            mode: str = "w:gz",
        ) -> None:
            destination.parent.mkdir(parents=True, exist_ok=True)
            selected_metadata = metadata if package_metadata is None else package_metadata
            files = {
                "package/package.json": metadata_source
                if metadata_source is not None
                else strict_json_dumps(selected_metadata, separators=(",", ":")).encode("utf-8"),
                "package/README.md": b"fixture\n",
                "package/LICENSE": b"fixture\n",
                "package/dist/index.js": b"export {};\n",
                "package/dist/index.d.ts": b"export {};\n",
                **(extra_files or {}),
            }
            with tarfile.open(destination, mode=mode) as archive:
                for name, content in files.items():
                    info = tarfile.TarInfo(name)
                    info.size = len(content)
                    archive.addfile(info, io.BytesIO(content))

        artifact = root / "artifact"
        package = artifact / tarball_name(version)
        write_package(package)
        gate_record = {
            "gate": GATE_NAME,
            "package": f"{PACKAGE_NAME}@{version}",
            "revision": revision,
            "source": expected_source_identities(revision),
            "tarball": tarball_binding(package),
            "binary": "/tmp/stasis",
            "binarySha256": binary_digest,
            "virtualElapsedNs": "10000000000",
            "wallElapsedMs": 5.0,
            "closeResponseAndEof": True,
        }
        gate_log = root / "gate.log"
        gate_log.write_text(
            strict_json_dumps(gate_record, separators=(",", ":")) + "\n",
            encoding="utf-8",
        )
        proof = artifact / proof_name(version)
        document = create_proof(
            package=package,
            gate_log=gate_log,
            output=proof,
            version=version,
            revision=revision,
            native_binary_sha256=binary_digest,
            run_id="123",
            run_attempt="1",
        )
        assert document["schema"] == 2
        assert document["tarball"] == gate_record["tarball"]
        verify_proof(
            directory=artifact,
            version=version,
            revision=revision,
            native_binary_sha256=binary_digest,
            run_id="123",
            run_attempt="1",
        )

        for hook in sorted(INSTALL_LIFECYCLE_SCRIPTS):
            hook_metadata = {**metadata, "scripts": {hook: "exit 1", "prepack": "tsc"}}
            hook_package = root / f"hook-{hook}" / tarball_name(version)
            write_package(hook_package, package_metadata=hook_metadata)
            expect_error(hook, lambda package=hook_package: verify_tarball(package, version))

        numeric_provenance_metadata = {
            **metadata,
            "publishConfig": {"access": "public", "tag": "latest", "provenance": 1},
        }
        numeric_provenance_package = root / "numeric-provenance" / tarball_name(version)
        write_package(
            numeric_provenance_package,
            package_metadata=numeric_provenance_metadata,
        )
        expect_error(
            "numeric package provenance",
            lambda: verify_tarball(numeric_provenance_package, version),
        )

        wrong_stable_tag_package = root / "wrong-stable-tag" / tarball_name(version)
        write_package(
            wrong_stable_tag_package,
            package_metadata={
                **metadata,
                "publishConfig": {
                    "access": "public",
                    "tag": "alpha",
                    "provenance": True,
                },
            },
        )
        expect_error(
            "stable package using the alpha dist-tag",
            lambda: verify_tarball(wrong_stable_tag_package, version),
        )

        alpha_version = "0.1.0-alpha.0"
        alpha_metadata = {
            **metadata,
            "version": alpha_version,
            "publishConfig": {
                "access": "public",
                "tag": "alpha",
                "provenance": True,
            },
        }
        alpha_package = root / "alpha" / tarball_name(alpha_version)
        write_package(alpha_package, package_metadata=alpha_metadata)
        verify_tarball(alpha_package, alpha_version)
        assert publish_tag(alpha_version) == "alpha"
        assert publish_tag(version) == "latest"

        expect_error("beta prerelease", lambda: tarball_name("0.2.0-beta.1"))
        expect_error("leading-zero version", lambda: tarball_name("00.2.0"))

        top_level_tag_package = root / "top-level-tag" / tarball_name(version)
        write_package(
            top_level_tag_package,
            package_metadata={**metadata, "tag": "latest"},
        )
        expect_error(
            "top-level package dist-tag override",
            lambda: verify_tarball(top_level_tag_package, version),
        )

        collision_cases = {
            "case-folded collision": {"package/dist/INDEX.js": b"export {};\n"},
            "non-ASCII path": {"package/dist/caf\N{LATIN SMALL LETTER E WITH ACUTE}.js": b""},
            "Windows reserved path": {"package/dist/AUX.js": b""},
            "non-canonical path": {"package/dist/trailing.": b""},
        }
        for index, (label, additions) in enumerate(collision_cases.items()):
            bad_package = root / f"path-{index}" / tarball_name(version)
            write_package(bad_package, extra_files=additions)
            expect_error(label, lambda package=bad_package: verify_tarball(package, version))

        valid_metadata = strict_json_dumps(metadata, separators=(",", ":"))
        duplicate_metadata = ('{"name":"@oxhq/stasis",' + valid_metadata[1:]).encode("utf-8")
        duplicate_package = root / "duplicate-json" / tarball_name(version)
        write_package(duplicate_package, metadata_source=duplicate_metadata)
        expect_error(
            "duplicate package metadata key",
            lambda: verify_tarball(duplicate_package, version),
        )
        nonfinite_metadata = valid_metadata.replace('"provenance":true', '"provenance":1e400')
        nonfinite_package = root / "nonfinite-json" / tarball_name(version)
        write_package(nonfinite_package, metadata_source=nonfinite_metadata.encode("utf-8"))
        expect_error(
            "non-finite package metadata number",
            lambda: verify_tarball(nonfinite_package, version),
        )

        compact_gate = strict_json_dumps(gate_record, separators=(",", ":"))
        duplicate_gate_log = root / "duplicate-gate.log"
        duplicate_gate_log.write_text(
            '{"gate":"sdk-act-settle-inspect",' + compact_gate[1:] + "\n",
            encoding="utf-8",
        )
        expect_error(
            "duplicate gate-log key",
            lambda: parse_gate_log(duplicate_gate_log, package, version, revision),
        )
        nonfinite_gate_log = root / "nonfinite-gate.log"
        nonfinite_gate_log.write_text(
            compact_gate.replace('"wallElapsedMs":5.0', '"wallElapsedMs":1e400') + "\n",
            encoding="utf-8",
        )
        expect_error(
            "non-finite gate-log number",
            lambda: parse_gate_log(nonfinite_gate_log, package, version, revision),
        )
        numeric_digest_gate_log = root / "numeric-digest-gate.log"
        numeric_digest_gate_log.write_text(
            strict_json_dumps(
                {**gate_record, "binarySha256": int("3" * 64)},
                separators=(",", ":"),
            )
            + "\n",
            encoding="utf-8",
        )
        expect_error(
            "numeric native digest in gate log",
            lambda: parse_gate_log(numeric_digest_gate_log, package, version, revision),
        )

        proof_text = proof.read_text(encoding="utf-8")
        proof.write_text('{"schema":2,' + proof_text.lstrip()[1:], encoding="utf-8")
        expect_error(
            "duplicate proof key",
            lambda: verify_proof(
                directory=artifact,
                version=version,
                revision=revision,
                native_binary_sha256=binary_digest,
                run_id="123",
                run_attempt="1",
            ),
        )
        proof.write_text(
            proof_text.replace('"schema": 2', '"schema": 1e400'), encoding="utf-8"
        )
        expect_error(
            "non-finite proof number",
            lambda: verify_proof(
                directory=artifact,
                version=version,
                revision=revision,
                native_binary_sha256=binary_digest,
                run_id="123",
                run_attempt="1",
            ),
        )
        proof.write_text(
            proof_text.replace('"schema": 2', '"schema": 2.0'), encoding="utf-8"
        )
        expect_error(
            "floating-point proof schema",
            lambda: verify_proof(
                directory=artifact,
                version=version,
                revision=revision,
                native_binary_sha256=binary_digest,
                run_id="123",
                run_attempt="1",
            ),
        )
        numeric_digest_proof = strict_json_loads(proof_text, "self-test SDK gate proof")
        assert isinstance(numeric_digest_proof, dict)
        numeric_digest_proof["gateLogSha256"] = int("1" * 64)
        proof.write_text(
            strict_json_dumps(numeric_digest_proof, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        expect_error(
            "numeric 64-digit proof digest",
            lambda: verify_proof(
                directory=artifact,
                version=version,
                revision=revision,
                native_binary_sha256=binary_digest,
                run_id="123",
                run_attempt="1",
            ),
        )
        proof.write_text(proof_text, encoding="utf-8")

        replay_package = root / "replay" / tarball_name(version)
        write_package(replay_package, extra_files={"package/README.md": b"replayed\n"})
        expect_error(
            "gate-log replay against another tarball",
            lambda: create_proof(
                package=replay_package,
                gate_log=gate_log,
                output=root / "replay-proof.json",
                version=version,
                revision=revision,
                native_binary_sha256=binary_digest,
                run_id="123",
                run_attempt="1",
            ),
        )
        write_package(package, extra_files={"package/README.md": b"substituted\n"})
        expect_error(
            "proof replay against another tarball",
            lambda: verify_proof(
                directory=artifact,
                version=version,
                revision=revision,
                native_binary_sha256=binary_digest,
                run_id="123",
                run_attempt="1",
            ),
        )

        plain_tar = root / "plain-tar" / tarball_name(version)
        write_package(plain_tar, mode="w")
        expect_error("plain tar disguised as tgz", lambda: verify_tarball(plain_tar, version))

        gzip_source = root / "gzip-source" / tarball_name(version)
        write_package(gzip_source)
        gzip_bytes = gzip_source.read_bytes()
        trailing_gzip = root / "trailing-gzip" / tarball_name(version)
        trailing_gzip.parent.mkdir()
        trailing_gzip.write_bytes(gzip_bytes + b"untrusted trailing bytes")
        expect_error("trailing gzip data", lambda: verify_tarball(trailing_gzip, version))
        concatenated_gzip = root / "concatenated-gzip" / tarball_name(version)
        concatenated_gzip.parent.mkdir()
        concatenated_gzip.write_bytes(gzip_bytes + gzip_bytes)
        expect_error(
            "concatenated gzip members", lambda: verify_tarball(concatenated_gzip, version)
        )

        first_plain_tar = root / "hidden-tar" / "first.tar"
        second_plain_tar = root / "hidden-tar" / "second.tar"
        write_package(first_plain_tar, mode="w")
        write_package(second_plain_tar, mode="w")
        hidden_second_tar = root / "hidden-tar" / tarball_name(version)
        with gzip.open(hidden_second_tar, mode="wb") as compressed:
            compressed.write(first_plain_tar.read_bytes())
            compressed.write(second_plain_tar.read_bytes())
        expect_error(
            "valid tar followed by a hidden second tar in one gzip member",
            lambda: verify_tarball(hidden_second_tar, version),
        )

        member_flood = root / "member-flood" / tarball_name(version)
        member_flood.parent.mkdir()
        with tarfile.open(member_flood, mode="w:gz") as archive:
            for index in range(MAX_TAR_MEMBERS + 1):
                info = tarfile.TarInfo(f"package/dist/flood-{index}.js")
                info.size = 0
                archive.addfile(info)
        expect_error("tar member flood", lambda: verify_tarball(member_flood, version))

        gzip_bomb = root / "gzip-bomb" / tarball_name(version)
        gzip_bomb.parent.mkdir()
        remaining = MAX_EXPANDED_TAR_BYTES + 1
        zeroes = b"\0" * STREAM_CHUNK_BYTES
        with gzip.open(gzip_bomb, mode="wb", compresslevel=9) as compressed:
            while remaining:
                chunk_size = min(remaining, len(zeroes))
                compressed.write(zeroes[:chunk_size])
                remaining -= chunk_size
        expect_error("gzip expansion bomb", lambda: verify_tarball(gzip_bomb, version))
    print("stasis npm release self-test: ok")


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser(description=__doc__)
    commands = result.add_subparsers(dest="command", required=True)
    create = commands.add_parser("create-proof")
    create.add_argument("--package", type=Path, required=True)
    create.add_argument("--gate-log", type=Path, required=True)
    create.add_argument("--output", type=Path, required=True)
    create.add_argument("--version", required=True)
    create.add_argument("--revision", required=True)
    create.add_argument("--native-binary-sha256", required=True)
    create.add_argument("--run-id", required=True)
    create.add_argument("--run-attempt", required=True)
    verify = commands.add_parser("verify-proof")
    verify.add_argument("--directory", type=Path, required=True)
    verify.add_argument("--version", required=True)
    verify.add_argument("--revision", required=True)
    verify.add_argument("--native-binary-sha256", required=True)
    verify.add_argument("--run-id", required=True)
    verify.add_argument("--run-attempt", required=True)
    commands.add_parser("self-test")
    return result


def main() -> None:
    arguments = parser().parse_args()
    if arguments.command == "create-proof":
        result = create_proof(
            package=arguments.package,
            gate_log=arguments.gate_log,
            output=arguments.output,
            version=arguments.version,
            revision=arguments.revision,
            native_binary_sha256=arguments.native_binary_sha256,
            run_id=arguments.run_id,
            run_attempt=arguments.run_attempt,
        )
    elif arguments.command == "verify-proof":
        result = verify_proof(
            directory=arguments.directory,
            version=arguments.version,
            revision=arguments.revision,
            native_binary_sha256=arguments.native_binary_sha256,
            run_id=arguments.run_id,
            run_attempt=arguments.run_attempt,
        )
    else:
        self_test()
        return
    print(strict_json_dumps(result, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except NpmReleaseError as error:
        raise SystemExit(f"error: {error}") from error
