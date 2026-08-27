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
CANONICAL_NONNEGATIVE_INTEGER_RE = re.compile(r"(?:0|[1-9][0-9]*)")
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
V2_AUTOMATION_CONTROLLED_EVENT_KINDS = (
    "fill:input",
    "activate:click",
    "reset:reset",
    "check:click",
    "check:input",
    "check:change",
    "select:input",
    "select:change",
    "invalid:invalid",
    "submit:submit",
    "submit:formdata",
)


def v2_automation_controlled_trace(
    event_time_ms: int, baseline_time_ms: int
) -> str:
    timestamp = str(event_time_ms)
    events = ">".join(
        f"{event_kind}:{timestamp}"
        for event_kind in V2_AUTOMATION_CONTROLLED_EVENT_KINDS
    )
    return f"{timestamp}|{events}|not-read|{baseline_time_ms}"


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


def require_v2_message_channel_proof(
    value: object, description: str
) -> dict[str, object]:
    expected_fields = {
        "profile",
        "idleOutcome",
        "idleMessagePortSources",
        "idleRuntimeFailures",
        "bufferActionRotatedStateToken",
        "pendingPreservedBufferedStateToken",
        "pendingMessagePortSources",
        "pendingRuntimeFailures",
        "startActionRotatedStateToken",
        "drainedOutcome",
        "drainedMessagePortSources",
        "drainedRuntimeFailures",
        "aggregateProcessedOrdinaryTasks",
        "trace",
        "evidenceProfile",
        "unsupportedWork",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    }
    if type(value) is not dict or set(value) != expected_fields:
        raise NpmReleaseError(f"{description} has an unexpected schema")
    for field in (
        "profile",
        "idleOutcome",
        "idleMessagePortSources",
        "idleRuntimeFailures",
        "pendingMessagePortSources",
        "pendingRuntimeFailures",
        "drainedOutcome",
        "drainedMessagePortSources",
        "drainedRuntimeFailures",
        "aggregateProcessedOrdinaryTasks",
        "trace",
        "evidenceProfile",
        "unsupportedWork",
    ):
        require_json_string(value.get(field), f"{description} {field}")
    expected_values = {
        "profile": "controlled-web-session-v2",
        "idleOutcome": "quiescent",
        "idleMessagePortSources": "0",
        "idleRuntimeFailures": "0",
        "pendingMessagePortSources": "1",
        "pendingRuntimeFailures": "0",
        "drainedOutcome": "quiescent",
        "drainedMessagePortSources": "0",
        "drainedRuntimeFailures": "0",
        "trace": "callback1>microtask1>callback2>microtask2",
        "evidenceProfile": "controlled-web-session-v2",
        "unsupportedWork": "0",
    }
    for field, expected in expected_values.items():
        if value[field] != expected:
            raise NpmReleaseError(f"{description} field does not match: {field}")
    for field in (
        "bufferActionRotatedStateToken",
        "pendingPreservedBufferedStateToken",
        "startActionRotatedStateToken",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    ):
        if value.get(field) is not True:
            raise NpmReleaseError(f"{description} field is not true: {field}")
    fullmatch(
        CANONICAL_NONNEGATIVE_INTEGER_RE,
        value["aggregateProcessedOrdinaryTasks"],
        f"{description} aggregate processed ordinary-task count",
    )
    if int(value["aggregateProcessedOrdinaryTasks"]) < 2:
        raise NpmReleaseError(f"{description} processed fewer than two ordinary tasks")
    return value


def require_v2_direct_data_svg_proof(
    value: object, description: str
) -> dict[str, object]:
    expected_fields = {
        "profile",
        "navigationBoundary",
        "outcome",
        "producerPending",
        "producerTerminal",
        "pendingImages",
        "runtimeFailures",
        "unsupportedWork",
        "externalIo",
        "completionTrace",
        "evidenceProfile",
        "httpNavigationBoundary",
        "httpOutcome",
        "httpProducerPending",
        "httpProducerTerminal",
        "httpPendingImages",
        "httpRuntimeFailures",
        "httpUnsupportedWork",
        "httpExternalIo",
        "httpCompletionTrace",
        "httpEvidenceProfile",
        "sameControlledSession",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    }
    if type(value) is not dict or set(value) != expected_fields:
        raise NpmReleaseError(f"{description} has an unexpected schema")
    expected_values = {
        "profile": "controlled-web-session-v2",
        "navigationBoundary": "controlled_ready",
        "outcome": "quiescent",
        "producerPending": "0",
        "pendingImages": "0",
        "runtimeFailures": "0",
        "unsupportedWork": "0",
        "externalIo": "0",
        "completionTrace": "load:0>loadend:0|now:0",
        "evidenceProfile": "controlled-web-session-v2",
        "httpNavigationBoundary": "controlled_ready",
        "httpOutcome": "quiescent",
        "httpProducerPending": "0",
        "httpPendingImages": "0",
        "httpRuntimeFailures": "0",
        "httpUnsupportedWork": "0",
        "httpExternalIo": "0",
        "httpCompletionTrace": (
            "loaded:load:0>loadend:0|failed:error:0>loadend:0|"
            "cached:load:0|now:0"
        ),
        "httpEvidenceProfile": "controlled-web-session-v2",
    }
    for field, expected in expected_values.items():
        if require_json_string(value.get(field), f"{description} {field}") != expected:
            raise NpmReleaseError(f"{description} field does not match: {field}")
    if value.get("producerTerminal") is not False:
        raise NpmReleaseError(f"{description} field is not false: producerTerminal")
    if value.get("httpProducerTerminal") is not False:
        raise NpmReleaseError(f"{description} field is not false: httpProducerTerminal")
    for field in (
        "sameControlledSession",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    ):
        if value.get(field) is not True:
            raise NpmReleaseError(f"{description} field is not true: {field}")
    return value


def require_v2_inline_svg_rendering_proof(
    value: object, description: str
) -> dict[str, object]:
    expected_fields = {
        "profile",
        "navigationBoundary",
        "outcome",
        "producerPending",
        "producerTerminal",
        "pendingImages",
        "runtimeFailures",
        "unsupportedWork",
        "externalIo",
        "fixtureTrace",
        "domCompletionEvents",
        "evidenceProfile",
        "sameControlledSession",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    }
    if type(value) is not dict or set(value) != expected_fields:
        raise NpmReleaseError(f"{description} has an unexpected schema")
    expected_values = {
        "profile": "controlled-web-session-v2",
        "navigationBoundary": "controlled_ready",
        "outcome": "quiescent",
        "producerPending": "0",
        "pendingImages": "0",
        "runtimeFailures": "0",
        "unsupportedWork": "0",
        "externalIo": "0",
        "fixtureTrace": "inline-svg:4x3|events:0|now:0",
        "domCompletionEvents": "0",
        "evidenceProfile": "controlled-web-session-v2",
    }
    for field, expected in expected_values.items():
        if require_json_string(value.get(field), f"{description} {field}") != expected:
            raise NpmReleaseError(f"{description} field does not match: {field}")
    if value.get("producerTerminal") is not False:
        raise NpmReleaseError(f"{description} field is not false: producerTerminal")
    for field in (
        "sameControlledSession",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    ):
        if value.get(field) is not True:
            raise NpmReleaseError(f"{description} field is not true: {field}")
    return value


def require_v2_input_method_focus_proof(
    value: object, description: str
) -> dict[str, object]:
    expected_fields = {
        "profile",
        "navigationBoundary",
        "outcome",
        "producerPending",
        "producerTerminal",
        "runtimeFailures",
        "unsupportedWork",
        "externalIo",
        "completionTrace",
        "evidenceProfile",
        "sameControlledSession",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    }
    if type(value) is not dict or set(value) != expected_fields:
        raise NpmReleaseError(f"{description} has an unexpected schema")
    expected_values = {
        "profile": "controlled-web-session-v2",
        "navigationBoundary": "controlled_ready",
        "outcome": "quiescent",
        "producerPending": "0",
        "runtimeFailures": "0",
        "unsupportedWork": "0",
        "externalIo": "0",
        "completionTrace": (
            "blurred|4|focus:trusted:0>focusin:trusted:0>blur:trusted:0>"
            "focusout:trusted:0|rwa-value|2:5"
        ),
        "evidenceProfile": "controlled-web-session-v2",
    }
    for field, expected in expected_values.items():
        if require_json_string(value.get(field), f"{description} {field}") != expected:
            raise NpmReleaseError(f"{description} field does not match: {field}")
    if value.get("producerTerminal") is not False:
        raise NpmReleaseError(f"{description} field is not false: producerTerminal")
    for field in (
        "sameControlledSession",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    ):
        if value.get(field) is not True:
            raise NpmReleaseError(f"{description} field is not true: {field}")
    return value


def require_v2_automation_event_timestamps_proof(
    value: object, description: str
) -> dict[str, object]:
    expected_fields = {
        "profile",
        "navigationBoundary",
        "initialOutcome",
        "initialVirtualTimeNs",
        "advancedVirtualTimeNs",
        "dispatchedVirtualTimeNs",
        "controlledEventCount",
        "controlledTrace",
        "browserEventCountAfterScriptProbe",
        "scriptCreatedConstructorCount",
        "scriptCreatedTrace",
        "rejectedOutcome",
        "failureCode",
        "unsupportedKind",
        "unsupportedCount",
        "unsupportedReason",
        "unsupportedTimeSurface",
        "evidenceProfile",
        "sameControlledSession",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    }
    if type(value) is not dict or set(value) != expected_fields:
        raise NpmReleaseError(f"{description} has an unexpected schema")
    expected_values = {
        "profile": "controlled-web-session-v2",
        "navigationBoundary": "controlled_ready",
        "initialOutcome": "quiescent",
        "controlledEventCount": "11",
        "browserEventCountAfterScriptProbe": "12",
        "scriptCreatedConstructorCount": "5",
        "scriptCreatedTrace": "0,0,0,0,0",
        "rejectedOutcome": "unsupported_work",
        "failureCode": "unsupported_clock_surface",
        "unsupportedKind": "other",
        "unsupportedCount": "1",
        "unsupportedReason": "time_surface",
        "unsupportedTimeSurface": "host_timestamp",
        "evidenceProfile": "controlled-web-session-v2",
    }
    for field, expected in expected_values.items():
        if require_json_string(value.get(field), f"{description} {field}") != expected:
            raise NpmReleaseError(f"{description} field does not match: {field}")
    for field in (
        "initialVirtualTimeNs",
        "advancedVirtualTimeNs",
        "dispatchedVirtualTimeNs",
    ):
        fullmatch(
            CANONICAL_NONNEGATIVE_INTEGER_RE,
            require_json_string(value.get(field), f"{description} {field}"),
            f"{description} {field}",
        )
    initial_virtual_time_ns = int(value["initialVirtualTimeNs"])
    advanced_virtual_time_ns = int(value["advancedVirtualTimeNs"])
    dispatched_virtual_time_ns = int(value["dispatchedVirtualTimeNs"])
    if advanced_virtual_time_ns != initial_virtual_time_ns + 5_000_000:
        raise NpmReleaseError(
            f"{description} did not advance exactly five milliseconds from its session baseline"
        )
    if dispatched_virtual_time_ns != advanced_virtual_time_ns:
        raise NpmReleaseError(
            f"{description} dispatch settle changed the advanced virtual time"
        )
    controlled_trace = require_json_string(
        value.get("controlledTrace"), f"{description} controlledTrace"
    )
    controlled_trace_parts = controlled_trace.split("|")
    if len(controlled_trace_parts) != 4 or controlled_trace_parts[2] != "not-read":
        raise NpmReleaseError(f"{description} controlled trace is not canonical")
    controlled_event_time_ms_raw = controlled_trace_parts[0]
    controlled_baseline_time_ms_raw = controlled_trace_parts[3]
    fullmatch(
        CANONICAL_NONNEGATIVE_INTEGER_RE,
        controlled_event_time_ms_raw,
        f"{description} controlled document-clock sample",
    )
    fullmatch(
        CANONICAL_NONNEGATIVE_INTEGER_RE,
        controlled_baseline_time_ms_raw,
        f"{description} controlled document-clock baseline",
    )
    controlled_event_time_ms = int(controlled_event_time_ms_raw)
    controlled_baseline_time_ms = int(controlled_baseline_time_ms_raw)
    if controlled_event_time_ms != controlled_baseline_time_ms + 5:
        raise NpmReleaseError(
            f"{description} controlled document clock did not advance exactly five milliseconds"
        )
    if controlled_baseline_time_ms * 1_000_000 >= initial_virtual_time_ns:
        raise NpmReleaseError(
            f"{description} reused document clock was conflated with the session-global clock"
        )
    if controlled_trace != v2_automation_controlled_trace(
        controlled_event_time_ms, controlled_baseline_time_ms
    ):
        raise NpmReleaseError(
            f"{description} controlled events do not match their document-clock sample"
        )
    for field in (
        "sameControlledSession",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    ):
        if value.get(field) is not True:
            raise NpmReleaseError(f"{description} field is not true: {field}")
    return value


def require_v2_css_animation_event_timestamps_proof(
    value: object, description: str
) -> dict[str, object]:
    expected_fields = {
        "profile",
        "initialOutcome",
        "settledVirtualTimeNs",
        "controlledOutcome",
        "controlledEventCount",
        "controlledEventKinds",
        "controlledOwnedEventCount",
        "controlledDispatchTimeCount",
        "controlledRuntimeFailures",
        "controlledUnsupportedWork",
        "controlledExternalIo",
        "pendingAnimationEvents",
        "finiteAnimations",
        "infiniteAnimations",
        "unsupportedAnimations",
        "producerPending",
        "producerTerminal",
        "processedRenderingOpportunities",
        "scriptCreatedConstructorCount",
        "scriptCreatedTrace",
        "rejectedOutcome",
        "failureCode",
        "unsupportedKind",
        "unsupportedCount",
        "unsupportedReason",
        "unsupportedTimeSurface",
        "evidenceProfile",
        "publicNonAuxiliaryControlledTarget",
        "sameControlledSession",
        "freshExactBinaryProcess",
        "managedRuntimeFallbackAccesses",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    }
    if type(value) is not dict or set(value) != expected_fields:
        raise NpmReleaseError(f"{description} has an unexpected schema")
    expected_values = {
        "profile": "controlled-web-session-v2",
        "initialOutcome": "quiescent",
        "settledVirtualTimeNs": "5000000",
        "controlledOutcome": "quiescent",
        "controlledEventCount": "2",
        "controlledEventKinds": "animationend,animationstart",
        "controlledOwnedEventCount": "2",
        "controlledRuntimeFailures": "0",
        "controlledUnsupportedWork": "0",
        "controlledExternalIo": "0",
        "pendingAnimationEvents": "0",
        "finiteAnimations": "0",
        "infiniteAnimations": "0",
        "unsupportedAnimations": "0",
        "producerPending": "0",
        "scriptCreatedConstructorCount": "2",
        "scriptCreatedTrace": "script:0,0",
        "rejectedOutcome": "unsupported_work",
        "failureCode": "unsupported_clock_surface",
        "unsupportedKind": "other",
        "unsupportedCount": "1",
        "unsupportedReason": "time_surface",
        "unsupportedTimeSurface": "host_timestamp",
        "evidenceProfile": "controlled-web-session-v2",
        "managedRuntimeFallbackAccesses": "0",
    }
    for field, expected in expected_values.items():
        if require_json_string(value.get(field), f"{description} {field}") != expected:
            raise NpmReleaseError(f"{description} field does not match: {field}")
    if value.get("producerTerminal") is not False:
        raise NpmReleaseError(f"{description} field is not false: producerTerminal")
    for field in (
        "publicNonAuxiliaryControlledTarget",
        "sameControlledSession",
        "freshExactBinaryProcess",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    ):
        if value.get(field) is not True:
            raise NpmReleaseError(f"{description} field is not true: {field}")
    dispatch_count = require_json_string(
        value.get("controlledDispatchTimeCount"),
        f"{description} controlledDispatchTimeCount",
    )
    fullmatch(
        POSITIVE_INTEGER_RE,
        dispatch_count,
        f"{description} controlled CSS dispatch-time count",
    )
    if int(dispatch_count) > 2:
        raise NpmReleaseError(f"{description} observed more dispatch times than events")
    rendering_opportunities = require_json_string(
        value.get("processedRenderingOpportunities"),
        f"{description} processedRenderingOpportunities",
    )
    fullmatch(
        POSITIVE_INTEGER_RE,
        rendering_opportunities,
        f"{description} processed rendering-opportunity count",
    )
    return value


def require_v2_cookie_session_proof(
    value: object, description: str
) -> dict[str, object]:
    expected_fields = {
        "profile",
        "stateSchemaVersion",
        "stateProfile",
        "responseCookieName",
        "responseCookieExpiryUnixTimeNs",
        "maxAgePrecedenceOverPastExpires",
        "restoredSameSiteCookieSent",
        "crossSiteResourceReachedServer",
        "crossSiteLaxCookieFiltered",
        "crossSiteRequestMethod",
        "crossSiteRequestPath",
        "evidenceProfile",
        "memoryOnlyExplicitStatePortability",
        "noImportControlCookieCount",
        "noImportControlRequestCookieHeaderEmpty",
        "noImportControlSameHostContext",
        "cookieTimeRangeFailureCode",
        "cookieTimeRangeFatal",
        "cookieTimeRangeStateEffect",
        "cookieTimeRangeRequestReachedServer",
        "credentialEnvironmentMode",
        "freshExactBinaryProcesses",
        "gracefulCookieSessionProcesses",
        "managedRuntimeFallbackAccesses",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    }
    if type(value) is not dict or set(value) != expected_fields:
        raise NpmReleaseError(f"{description} has an unexpected schema")
    expected_values = {
        "profile": "controlled-web-session-v2",
        "stateSchemaVersion": "1",
        "stateProfile": "controlled-web-session-v2",
        "responseCookieName": "remember_me",
        "responseCookieExpiryUnixTimeNs": "2592000000000000",
        "crossSiteRequestMethod": "GET",
        "crossSiteRequestPath": "/probe.js",
        "evidenceProfile": "controlled-web-session-v2",
        "noImportControlCookieCount": "0",
        "cookieTimeRangeFailureCode": "unsupported_cookie_time_range",
        "cookieTimeRangeStateEffect": "partial",
        "credentialEnvironmentMode": "explicit_allowlist",
        "freshExactBinaryProcesses": "4",
        "gracefulCookieSessionProcesses": "4",
        "managedRuntimeFallbackAccesses": "0",
    }
    for field, expected in expected_values.items():
        if require_json_string(value.get(field), f"{description} {field}") != expected:
            raise NpmReleaseError(f"{description} field does not match: {field}")
    fullmatch(
        POSITIVE_INTEGER_RE,
        value["responseCookieExpiryUnixTimeNs"],
        f"{description} response-cookie expiry",
    )
    for field in (
        "maxAgePrecedenceOverPastExpires",
        "restoredSameSiteCookieSent",
        "crossSiteResourceReachedServer",
        "crossSiteLaxCookieFiltered",
        "memoryOnlyExplicitStatePortability",
        "noImportControlRequestCookieHeaderEmpty",
        "noImportControlSameHostContext",
        "exactBinaryLaunch",
        "closeResponseAndEof",
    ):
        if value.get(field) is not True:
            raise NpmReleaseError(f"{description} field is not true: {field}")
    for field in ("cookieTimeRangeFatal", "cookieTimeRangeRequestReachedServer"):
        if value.get(field) is not False:
            raise NpmReleaseError(f"{description} field is not false: {field}")
    return value


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
        "v2MessageChannel",
        "v2DirectDataSvg",
        "v2InlineSvgRendering",
        "v2InputMethodFocus",
        "v2AutomationEventTimestamps",
        "v2CssAnimationEventTimestamps",
        "v2CookieSession",
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
    require_v2_message_channel_proof(
        value.get("v2MessageChannel"), "SDK gate v2 MessageChannel proof"
    )
    require_v2_direct_data_svg_proof(
        value.get("v2DirectDataSvg"), "SDK gate v2 direct data-SVG proof"
    )
    require_v2_inline_svg_rendering_proof(
        value.get("v2InlineSvgRendering"),
        "SDK gate v2 inline SVG rendering proof",
    )
    require_v2_input_method_focus_proof(
        value.get("v2InputMethodFocus"), "SDK gate v2 InputMethod focus proof"
    )
    require_v2_automation_event_timestamps_proof(
        value.get("v2AutomationEventTimestamps"),
        "SDK gate v2 automation event-timestamps proof",
    )
    require_v2_css_animation_event_timestamps_proof(
        value.get("v2CssAnimationEventTimestamps"),
        "SDK gate v2 CSS animation-event timestamps proof",
    )
    require_v2_cookie_session_proof(
        value.get("v2CookieSession"), "SDK gate v2 cookie/session proof"
    )
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
        "schema": 7,
        "gate": GATE_NAME,
        "package": f"{PACKAGE_NAME}@{version}",
        "revision": revision,
        "workflowRunId": run_id,
        "workflowRunAttempt": run_attempt,
        "source": gate["source"],
        "tarball": gate["tarball"],
        "nativeBinarySha256": native_binary_sha256,
        "gateLogSha256": hash_file(gate_log, "sha256"),
        "v2MessageChannel": gate["v2MessageChannel"],
        "v2DirectDataSvg": gate["v2DirectDataSvg"],
        "v2InlineSvgRendering": gate["v2InlineSvgRendering"],
        "v2InputMethodFocus": gate["v2InputMethodFocus"],
        "v2AutomationEventTimestamps": gate["v2AutomationEventTimestamps"],
        "v2CssAnimationEventTimestamps": gate["v2CssAnimationEventTimestamps"],
        "v2CookieSession": gate["v2CookieSession"],
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
        "v2MessageChannel",
        "v2DirectDataSvg",
        "v2InlineSvgRendering",
        "v2InputMethodFocus",
        "v2AutomationEventTimestamps",
        "v2CssAnimationEventTimestamps",
        "v2CookieSession",
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
    require_v2_message_channel_proof(
        document.get("v2MessageChannel"), "SDK gate proof v2 MessageChannel proof"
    )
    require_v2_direct_data_svg_proof(
        document.get("v2DirectDataSvg"), "SDK gate proof v2 direct data-SVG proof"
    )
    require_v2_inline_svg_rendering_proof(
        document.get("v2InlineSvgRendering"),
        "SDK gate proof v2 inline SVG rendering proof",
    )
    require_v2_input_method_focus_proof(
        document.get("v2InputMethodFocus"),
        "SDK gate proof v2 InputMethod focus proof",
    )
    require_v2_automation_event_timestamps_proof(
        document.get("v2AutomationEventTimestamps"),
        "SDK gate proof v2 automation event-timestamps proof",
    )
    require_v2_css_animation_event_timestamps_proof(
        document.get("v2CssAnimationEventTimestamps"),
        "SDK gate proof v2 CSS animation-event timestamps proof",
    )
    require_v2_cookie_session_proof(
        document.get("v2CookieSession"), "SDK gate proof v2 cookie/session proof"
    )
    expected = {
        "schema": 7,
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
            "v2MessageChannel": {
                "profile": "controlled-web-session-v2",
                "idleOutcome": "quiescent",
                "idleMessagePortSources": "0",
                "idleRuntimeFailures": "0",
                "bufferActionRotatedStateToken": True,
                "pendingPreservedBufferedStateToken": True,
                "pendingMessagePortSources": "1",
                "pendingRuntimeFailures": "0",
                "startActionRotatedStateToken": True,
                "drainedOutcome": "quiescent",
                "drainedMessagePortSources": "0",
                "drainedRuntimeFailures": "0",
                "aggregateProcessedOrdinaryTasks": "3",
                "trace": "callback1>microtask1>callback2>microtask2",
                "evidenceProfile": "controlled-web-session-v2",
                "unsupportedWork": "0",
                "exactBinaryLaunch": True,
                "closeResponseAndEof": True,
            },
            "v2DirectDataSvg": {
                "profile": "controlled-web-session-v2",
                "navigationBoundary": "controlled_ready",
                "outcome": "quiescent",
                "producerPending": "0",
                "producerTerminal": False,
                "pendingImages": "0",
                "runtimeFailures": "0",
                "unsupportedWork": "0",
                "externalIo": "0",
                "completionTrace": "load:0>loadend:0|now:0",
                "evidenceProfile": "controlled-web-session-v2",
                "httpNavigationBoundary": "controlled_ready",
                "httpOutcome": "quiescent",
                "httpProducerPending": "0",
                "httpProducerTerminal": False,
                "httpPendingImages": "0",
                "httpRuntimeFailures": "0",
                "httpUnsupportedWork": "0",
                "httpExternalIo": "0",
                "httpCompletionTrace": (
                    "loaded:load:0>loadend:0|failed:error:0>loadend:0|"
                    "cached:load:0|now:0"
                ),
                "httpEvidenceProfile": "controlled-web-session-v2",
                "sameControlledSession": True,
                "exactBinaryLaunch": True,
                "closeResponseAndEof": True,
            },
            "v2InlineSvgRendering": {
                "profile": "controlled-web-session-v2",
                "navigationBoundary": "controlled_ready",
                "outcome": "quiescent",
                "producerPending": "0",
                "producerTerminal": False,
                "pendingImages": "0",
                "runtimeFailures": "0",
                "unsupportedWork": "0",
                "externalIo": "0",
                "fixtureTrace": "inline-svg:4x3|events:0|now:0",
                "domCompletionEvents": "0",
                "evidenceProfile": "controlled-web-session-v2",
                "sameControlledSession": True,
                "exactBinaryLaunch": True,
                "closeResponseAndEof": True,
            },
            "v2InputMethodFocus": {
                "profile": "controlled-web-session-v2",
                "navigationBoundary": "controlled_ready",
                "outcome": "quiescent",
                "producerPending": "0",
                "producerTerminal": False,
                "runtimeFailures": "0",
                "unsupportedWork": "0",
                "externalIo": "0",
                "completionTrace": (
                    "blurred|4|focus:trusted:0>focusin:trusted:0>blur:trusted:0>"
                    "focusout:trusted:0|rwa-value|2:5"
                ),
                "evidenceProfile": "controlled-web-session-v2",
                "sameControlledSession": True,
                "exactBinaryLaunch": True,
                "closeResponseAndEof": True,
            },
            "v2AutomationEventTimestamps": {
                "profile": "controlled-web-session-v2",
                "navigationBoundary": "controlled_ready",
                "initialOutcome": "quiescent",
                "initialVirtualTimeNs": "140000000",
                "advancedVirtualTimeNs": "145000000",
                "dispatchedVirtualTimeNs": "145000000",
                "controlledEventCount": "11",
                "controlledTrace": v2_automation_controlled_trace(25, 20),
                "browserEventCountAfterScriptProbe": "12",
                "scriptCreatedConstructorCount": "5",
                "scriptCreatedTrace": "0,0,0,0,0",
                "rejectedOutcome": "unsupported_work",
                "failureCode": "unsupported_clock_surface",
                "unsupportedKind": "other",
                "unsupportedCount": "1",
                "unsupportedReason": "time_surface",
                "unsupportedTimeSurface": "host_timestamp",
                "evidenceProfile": "controlled-web-session-v2",
                "sameControlledSession": True,
                "exactBinaryLaunch": True,
                "closeResponseAndEof": True,
            },
            "v2CssAnimationEventTimestamps": {
                "profile": "controlled-web-session-v2",
                "initialOutcome": "quiescent",
                "settledVirtualTimeNs": "5000000",
                "controlledOutcome": "quiescent",
                "controlledEventCount": "2",
                "controlledEventKinds": "animationend,animationstart",
                "controlledOwnedEventCount": "2",
                "controlledDispatchTimeCount": "2",
                "controlledRuntimeFailures": "0",
                "controlledUnsupportedWork": "0",
                "controlledExternalIo": "0",
                "pendingAnimationEvents": "0",
                "finiteAnimations": "0",
                "infiniteAnimations": "0",
                "unsupportedAnimations": "0",
                "producerPending": "0",
                "producerTerminal": False,
                "processedRenderingOpportunities": "3",
                "scriptCreatedConstructorCount": "2",
                "scriptCreatedTrace": "script:0,0",
                "rejectedOutcome": "unsupported_work",
                "failureCode": "unsupported_clock_surface",
                "unsupportedKind": "other",
                "unsupportedCount": "1",
                "unsupportedReason": "time_surface",
                "unsupportedTimeSurface": "host_timestamp",
                "evidenceProfile": "controlled-web-session-v2",
                "publicNonAuxiliaryControlledTarget": True,
                "sameControlledSession": True,
                "freshExactBinaryProcess": True,
                "managedRuntimeFallbackAccesses": "0",
                "exactBinaryLaunch": True,
                "closeResponseAndEof": True,
            },
            "v2CookieSession": {
                "profile": "controlled-web-session-v2",
                "stateSchemaVersion": "1",
                "stateProfile": "controlled-web-session-v2",
                "responseCookieName": "remember_me",
                "responseCookieExpiryUnixTimeNs": "2592000000000000",
                "maxAgePrecedenceOverPastExpires": True,
                "restoredSameSiteCookieSent": True,
                "crossSiteResourceReachedServer": True,
                "crossSiteLaxCookieFiltered": True,
                "crossSiteRequestMethod": "GET",
                "crossSiteRequestPath": "/probe.js",
                "evidenceProfile": "controlled-web-session-v2",
                "memoryOnlyExplicitStatePortability": True,
                "noImportControlCookieCount": "0",
                "noImportControlRequestCookieHeaderEmpty": True,
                "noImportControlSameHostContext": True,
                "cookieTimeRangeFailureCode": "unsupported_cookie_time_range",
                "cookieTimeRangeFatal": False,
                "cookieTimeRangeStateEffect": "partial",
                "cookieTimeRangeRequestReachedServer": False,
                "credentialEnvironmentMode": "explicit_allowlist",
                "freshExactBinaryProcesses": "4",
                "gracefulCookieSessionProcesses": "4",
                "managedRuntimeFallbackAccesses": "0",
                "exactBinaryLaunch": True,
                "closeResponseAndEof": True,
            },
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
        assert document["schema"] == 7
        assert document["tarball"] == gate_record["tarball"]
        assert document["v2MessageChannel"] == gate_record["v2MessageChannel"]
        assert document["v2DirectDataSvg"] == gate_record["v2DirectDataSvg"]
        assert document["v2InlineSvgRendering"] == gate_record["v2InlineSvgRendering"]
        assert document["v2InputMethodFocus"] == gate_record["v2InputMethodFocus"]
        assert (
            document["v2AutomationEventTimestamps"]
            == gate_record["v2AutomationEventTimestamps"]
        )
        assert (
            document["v2CssAnimationEventTimestamps"]
            == gate_record["v2CssAnimationEventTimestamps"]
        )
        assert document["v2CookieSession"] == gate_record["v2CookieSession"]
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

        base_v2_message_channel = gate_record["v2MessageChannel"]
        assert isinstance(base_v2_message_channel, dict)
        v2_field_mutations = [
            ("wrong profile", "profile", "controlled-web-session-v1"),
            ("nonquiescent idle outcome", "idleOutcome", "pending"),
            ("nonzero idle MessagePort sources", "idleMessagePortSources", "1"),
            ("nonzero idle runtime failures", "idleRuntimeFailures", "1"),
            ("unrotated buffer action token", "bufferActionRotatedStateToken", False),
            (
                "unpreserved pending buffer token",
                "pendingPreservedBufferedStateToken",
                False,
            ),
            ("zero pending MessagePort sources", "pendingMessagePortSources", "0"),
            ("two pending MessagePort sources", "pendingMessagePortSources", "2"),
            ("numeric pending MessagePort sources", "pendingMessagePortSources", 1),
            ("nonzero pending runtime failures", "pendingRuntimeFailures", "1"),
            ("unrotated start action token", "startActionRotatedStateToken", False),
            ("nonquiescent drained outcome", "drainedOutcome", "pending"),
            ("nonzero drained MessagePort sources", "drainedMessagePortSources", "1"),
            ("nonzero drained runtime failures", "drainedRuntimeFailures", "1"),
            (
                "insufficient aggregate ordinary tasks",
                "aggregateProcessedOrdinaryTasks",
                "1",
            ),
            (
                "noncanonical aggregate ordinary tasks",
                "aggregateProcessedOrdinaryTasks",
                "02",
            ),
            (
                "numeric aggregate ordinary tasks",
                "aggregateProcessedOrdinaryTasks",
                2,
            ),
            (
                "wrong callback and microtask trace",
                "trace",
                "callback2>microtask2>callback1>microtask1",
            ),
            (
                "wrong evidence profile",
                "evidenceProfile",
                "controlled-web-session-v1",
            ),
            ("nonzero unsupported work", "unsupportedWork", "1"),
            ("false exact-binary launch", "exactBinaryLaunch", False),
            ("numeric exact-binary launch", "exactBinaryLaunch", 1),
            ("false close proof", "closeResponseAndEof", False),
            ("numeric close proof", "closeResponseAndEof", 1),
        ]
        v2_record_mutations = [
            (
                "missing lifecycle field",
                {
                    key: value
                    for key, value in base_v2_message_channel.items()
                    if key != "idleMessagePortSources"
                },
            ),
            (
                "extra lifecycle field",
                {**base_v2_message_channel, "iframeContextTree": "supported"},
            ),
            *[
                (label, {**base_v2_message_channel, field: value})
                for label, field, value in v2_field_mutations
            ],
        ]
        v2_gate_mutations = [
            (
                "missing v2 MessageChannel proof",
                {key: value for key, value in gate_record.items() if key != "v2MessageChannel"},
            ),
            *[
                (label, {**gate_record, "v2MessageChannel": mutation})
                for label, mutation in v2_record_mutations
            ],
        ]
        for index, (label, mutated_gate) in enumerate(v2_gate_mutations):
            mutated_gate_log = root / f"v2-gate-mutation-{index}.log"
            mutated_gate_log.write_text(
                strict_json_dumps(mutated_gate, separators=(",", ":")) + "\n",
                encoding="utf-8",
            )
            expect_error(
                label,
                lambda gate_log=mutated_gate_log: parse_gate_log(
                    gate_log, package, version, revision
                ),
            )

        base_v2_direct_data_svg = gate_record["v2DirectDataSvg"]
        assert isinstance(base_v2_direct_data_svg, dict)
        v2_direct_data_svg_record_mutations = [
            (
                "missing direct data-SVG lifecycle field",
                {
                    key: value
                    for key, value in base_v2_direct_data_svg.items()
                    if key != "pendingImages"
                },
            ),
            *[
                (
                    f"missing direct HTTP image lifecycle field {field}",
                    {
                        key: value
                        for key, value in base_v2_direct_data_svg.items()
                        if key != field
                    },
                )
                for field in (
                    "httpNavigationBoundary",
                    "httpOutcome",
                    "httpProducerPending",
                    "httpProducerTerminal",
                    "httpPendingImages",
                    "httpRuntimeFailures",
                    "httpUnsupportedWork",
                    "httpExternalIo",
                    "httpCompletionTrace",
                    "httpEvidenceProfile",
                )
            ],
            (
                "extra direct data-SVG lifecycle field",
                {**base_v2_direct_data_svg, "hostFallback": True},
            ),
            *[
                (label, {**base_v2_direct_data_svg, field: value})
                for label, field, value in (
                    ("wrong direct data-SVG profile", "profile", "controlled-webapp-v1"),
                    ("wrong image navigation boundary", "navigationBoundary", "unsupported"),
                    ("nonquiescent image outcome", "outcome", "unsupported_work"),
                    ("pending image producer", "producerPending", "1"),
                    ("numeric image producer count", "producerPending", 0),
                    ("terminal image producer", "producerTerminal", True),
                    ("numeric image producer terminal", "producerTerminal", 0),
                    ("pending rendering image", "pendingImages", "1"),
                    ("image runtime failure", "runtimeFailures", "1"),
                    ("image unsupported work", "unsupportedWork", "1"),
                    ("image external I/O", "externalIo", "1"),
                    ("wrong image completion trace", "completionTrace", "load:1"),
                    (
                        "wrong image evidence profile",
                        "evidenceProfile",
                        "controlled-webapp-v1",
                    ),
                    (
                        "wrong HTTP image navigation boundary",
                        "httpNavigationBoundary",
                        "unsupported",
                    ),
                    (
                        "numeric HTTP image navigation boundary",
                        "httpNavigationBoundary",
                        0,
                    ),
                    ("nonquiescent HTTP image outcome", "httpOutcome", "unsupported_work"),
                    ("numeric HTTP image outcome", "httpOutcome", 0),
                    ("pending HTTP image producer", "httpProducerPending", "1"),
                    ("numeric HTTP image producer count", "httpProducerPending", 0),
                    ("terminal HTTP image producer", "httpProducerTerminal", True),
                    ("numeric HTTP image producer terminal", "httpProducerTerminal", 0),
                    ("pending HTTP rendering image", "httpPendingImages", "1"),
                    ("numeric HTTP rendering image count", "httpPendingImages", 0),
                    ("HTTP image runtime failure", "httpRuntimeFailures", "1"),
                    ("numeric HTTP image runtime failures", "httpRuntimeFailures", 0),
                    ("HTTP image unsupported work", "httpUnsupportedWork", "1"),
                    ("numeric HTTP image unsupported work", "httpUnsupportedWork", 0),
                    ("HTTP image external I/O", "httpExternalIo", "1"),
                    ("numeric HTTP image external I/O", "httpExternalIo", 0),
                    (
                        "wrong HTTP image completion trace",
                        "httpCompletionTrace",
                        "loaded:load:1",
                    ),
                    ("numeric HTTP image completion trace", "httpCompletionTrace", 0),
                    (
                        "wrong HTTP image evidence profile",
                        "httpEvidenceProfile",
                        "controlled-web-session-v1",
                    ),
                    ("numeric HTTP image evidence profile", "httpEvidenceProfile", 0),
                    ("image not in same session", "sameControlledSession", False),
                    ("image not exact binary", "exactBinaryLaunch", False),
                    ("image missing close proof", "closeResponseAndEof", False),
                )
            ],
        ]

        base_v2_inline_svg_rendering = gate_record["v2InlineSvgRendering"]
        assert isinstance(base_v2_inline_svg_rendering, dict)
        v2_inline_svg_rendering_record_mutations = [
            (
                "missing inline SVG rendering lifecycle field",
                {
                    key: value
                    for key, value in base_v2_inline_svg_rendering.items()
                    if key != "domCompletionEvents"
                },
            ),
            (
                "extra inline SVG rendering lifecycle field",
                {**base_v2_inline_svg_rendering, "generalSvgSupported": True},
            ),
            *[
                (label, {**base_v2_inline_svg_rendering, field: value})
                for label, field, value in (
                    ("wrong inline SVG profile", "profile", "controlled-webapp-v1"),
                    (
                        "wrong inline SVG navigation boundary",
                        "navigationBoundary",
                        "unsupported",
                    ),
                    ("nonquiescent inline SVG outcome", "outcome", "unsupported_work"),
                    ("pending inline SVG producer", "producerPending", "1"),
                    ("numeric inline SVG producer count", "producerPending", 0),
                    ("terminal inline SVG producer", "producerTerminal", True),
                    ("numeric inline SVG producer terminal", "producerTerminal", 0),
                    ("pending inline SVG rendering image", "pendingImages", "1"),
                    ("inline SVG runtime failure", "runtimeFailures", "1"),
                    ("inline SVG unsupported work", "unsupportedWork", "1"),
                    ("inline SVG external I/O", "externalIo", "1"),
                    ("wrong inline SVG fixture trace", "fixtureTrace", "inline-svg"),
                    ("invented inline SVG DOM event", "domCompletionEvents", "1"),
                    (
                        "numeric inline SVG DOM event count",
                        "domCompletionEvents",
                        0,
                    ),
                    (
                        "wrong inline SVG evidence profile",
                        "evidenceProfile",
                        "controlled-webapp-v1",
                    ),
                    ("inline SVG not in same session", "sameControlledSession", False),
                    ("inline SVG not exact binary", "exactBinaryLaunch", False),
                    ("inline SVG missing close proof", "closeResponseAndEof", False),
                )
            ],
        ]

        base_v2_input_method_focus = gate_record["v2InputMethodFocus"]
        assert isinstance(base_v2_input_method_focus, dict)
        v2_input_method_focus_record_mutations = [
            (
                "missing InputMethod focus lifecycle field",
                {
                    key: value
                    for key, value in base_v2_input_method_focus.items()
                    if key != "completionTrace"
                },
            ),
            (
                "extra InputMethod focus lifecycle field",
                {**base_v2_input_method_focus, "hostTimestamp": True},
            ),
            *[
                (label, {**base_v2_input_method_focus, field: value})
                for label, field, value in (
                    ("wrong focus profile", "profile", "controlled-webapp-v1"),
                    ("wrong focus navigation boundary", "navigationBoundary", "unsupported"),
                    ("nonquiescent focus outcome", "outcome", "unsupported_work"),
                    ("pending focus producer", "producerPending", "1"),
                    ("numeric focus producer count", "producerPending", 0),
                    ("terminal focus producer", "producerTerminal", True),
                    ("numeric focus producer terminal", "producerTerminal", 0),
                    ("focus runtime failure", "runtimeFailures", "1"),
                    ("focus unsupported work", "unsupportedWork", "1"),
                    ("focus external I/O", "externalIo", "1"),
                    ("wrong focus completion trace", "completionTrace", "focused"),
                    (
                        "wrong focus evidence profile",
                        "evidenceProfile",
                        "controlled-webapp-v1",
                    ),
                    ("focus not in same session", "sameControlledSession", False),
                    ("focus not exact binary", "exactBinaryLaunch", False),
                    ("focus missing close proof", "closeResponseAndEof", False),
                )
            ],
        ]

        base_v2_automation_event_timestamps = gate_record[
            "v2AutomationEventTimestamps"
        ]
        assert isinstance(base_v2_automation_event_timestamps, dict)
        v2_automation_event_timestamps_record_mutations = [
            (
                "missing automation event-timestamps lifecycle field",
                {
                    key: value
                    for key, value in base_v2_automation_event_timestamps.items()
                    if key != "controlledEventCount"
                },
            ),
            (
                "extra automation event-timestamps lifecycle field",
                {
                    **base_v2_automation_event_timestamps,
                    "scriptCreatedEventsControlled": True,
                },
            ),
            *[
                (label, {**base_v2_automation_event_timestamps, field: value})
                for label, field, value in (
                    (
                        "wrong automation timestamp profile",
                        "profile",
                        "controlled-web-session-v1",
                    ),
                    (
                        "wrong automation navigation boundary",
                        "navigationBoundary",
                        "unsupported",
                    ),
                    ("nonquiescent automation initial outcome", "initialOutcome", "pending"),
                    (
                        "wrong automation initial virtual time",
                        "initialVirtualTimeNs",
                        "139999999",
                    ),
                    (
                        "numeric automation initial virtual time",
                        "initialVirtualTimeNs",
                        140000000,
                    ),
                    (
                        "noncanonical automation initial virtual time",
                        "initialVirtualTimeNs",
                        "0140000000",
                    ),
                    (
                        "negative automation initial virtual time",
                        "initialVirtualTimeNs",
                        "-1",
                    ),
                    (
                        "wrong automation advanced virtual time",
                        "advancedVirtualTimeNs",
                        "140000000",
                    ),
                    (
                        "numeric automation advanced virtual time",
                        "advancedVirtualTimeNs",
                        145000000,
                    ),
                    (
                        "noncanonical automation advanced virtual time",
                        "advancedVirtualTimeNs",
                        "0145000000",
                    ),
                    (
                        "wrong automation dispatch virtual time",
                        "dispatchedVirtualTimeNs",
                        "140000000",
                    ),
                    ("wrong controlled automation event count", "controlledEventCount", "10"),
                    ("numeric controlled automation event count", "controlledEventCount", 11),
                    ("wrong controlled automation trace", "controlledTrace", "5|not-owned"),
                    (
                        "controlled automation event escaped its document-clock sample",
                        "controlledTrace",
                        v2_automation_controlled_trace(25, 20).replace(
                            "fill:input:25", "fill:input:5", 1
                        ),
                    ),
                    (
                        "noncanonical controlled automation document-clock sample",
                        "controlledTrace",
                        v2_automation_controlled_trace(25, 20).replace(
                            "25|", "025|", 1
                        ),
                    ),
                    (
                        "noncanonical controlled automation document-clock baseline",
                        "controlledTrace",
                        v2_automation_controlled_trace(25, 20).replace(
                            "|20", "|020", 1
                        ),
                    ),
                    (
                        "wrong controlled automation document-clock delta",
                        "controlledTrace",
                        v2_automation_controlled_trace(26, 20),
                    ),
                    (
                        "controlled automation document clock conflated with session clock",
                        "controlledTrace",
                        v2_automation_controlled_trace(145, 140),
                    ),
                    (
                        "wrong browser event count after script probe",
                        "browserEventCountAfterScriptProbe",
                        "11",
                    ),
                    (
                        "wrong script-created constructor count",
                        "scriptCreatedConstructorCount",
                        "4",
                    ),
                    (
                        "wrong script-created timestamp trace",
                        "scriptCreatedTrace",
                        "5,5,5,5,5",
                    ),
                    (
                        "script-created events falsely settle",
                        "rejectedOutcome",
                        "quiescent",
                    ),
                    (
                        "wrong script-created failure code",
                        "failureCode",
                        "unsupported_work",
                    ),
                    ("wrong timestamp unsupported kind", "unsupportedKind", "timer"),
                    ("wrong timestamp unsupported count", "unsupportedCount", "0"),
                    ("numeric timestamp unsupported count", "unsupportedCount", 1),
                    (
                        "wrong timestamp unsupported reason",
                        "unsupportedReason",
                        "external_io",
                    ),
                    (
                        "wrong unsupported time surface",
                        "unsupportedTimeSurface",
                        "document_timestamp",
                    ),
                    (
                        "wrong automation evidence profile",
                        "evidenceProfile",
                        "controlled-web-session-v1",
                    ),
                    (
                        "automation timestamps not in same session",
                        "sameControlledSession",
                        False,
                    ),
                    (
                        "automation timestamps not exact binary",
                        "exactBinaryLaunch",
                        False,
                    ),
                    (
                        "automation timestamps missing close proof",
                        "closeResponseAndEof",
                        False,
                    ),
                )
            ],
        ]

        base_v2_css_animation_event_timestamps = gate_record[
            "v2CssAnimationEventTimestamps"
        ]
        assert isinstance(base_v2_css_animation_event_timestamps, dict)
        v2_css_animation_event_timestamps_record_mutations = [
            (
                "missing CSS animation-event timestamp lifecycle field",
                {
                    key: value
                    for key, value in base_v2_css_animation_event_timestamps.items()
                    if key != "controlledDispatchTimeCount"
                },
            ),
            (
                "extra CSS animation-event timestamp lifecycle field",
                {
                    **base_v2_css_animation_event_timestamps,
                    "generalAnimationEventConstructorsControlled": True,
                },
            ),
            *[
                (label, {**base_v2_css_animation_event_timestamps, field: value})
                for label, field, value in (
                    (
                        "wrong CSS animation timestamp profile",
                        "profile",
                        "controlled-web-session-v1",
                    ),
                    ("nonquiescent CSS initial outcome", "initialOutcome", "pending"),
                    ("wrong CSS settled virtual time", "settledVirtualTimeNs", "0"),
                    (
                        "numeric CSS settled virtual time",
                        "settledVirtualTimeNs",
                        5000000,
                    ),
                    ("nonquiescent CSS controlled outcome", "controlledOutcome", "pending"),
                    ("wrong CSS controlled event count", "controlledEventCount", "4"),
                    ("wrong CSS controlled event kinds", "controlledEventKinds", "animationstart"),
                    ("partly unowned CSS event set", "controlledOwnedEventCount", "4"),
                    ("zero CSS dispatch-time count", "controlledDispatchTimeCount", "0"),
                    ("too many CSS dispatch times", "controlledDispatchTimeCount", "6"),
                    (
                        "noncanonical CSS dispatch-time count",
                        "controlledDispatchTimeCount",
                        "02",
                    ),
                    ("numeric CSS dispatch-time count", "controlledDispatchTimeCount", 2),
                    ("CSS runtime failure", "controlledRuntimeFailures", "1"),
                    ("CSS unsupported work before script probe", "controlledUnsupportedWork", "1"),
                    ("CSS external I/O", "controlledExternalIo", "1"),
                    ("pending CSS events after settlement", "pendingAnimationEvents", "1"),
                    ("finite CSS animation after settlement", "finiteAnimations", "1"),
                    ("infinite CSS animation", "infiniteAnimations", "1"),
                    ("unsupported CSS animation", "unsupportedAnimations", "1"),
                    ("pending CSS producer", "producerPending", "1"),
                    ("terminal CSS producer", "producerTerminal", True),
                    ("numeric CSS producer terminal", "producerTerminal", 0),
                    (
                        "zero processed rendering opportunities",
                        "processedRenderingOpportunities",
                        "0",
                    ),
                    (
                        "noncanonical processed rendering opportunities",
                        "processedRenderingOpportunities",
                        "03",
                    ),
                    (
                        "numeric processed rendering opportunities",
                        "processedRenderingOpportunities",
                        3,
                    ),
                    ("wrong CSS constructor count", "scriptCreatedConstructorCount", "1"),
                    ("controlled script CSS constructors", "scriptCreatedTrace", "script:5,5"),
                    ("script CSS events falsely settle", "rejectedOutcome", "quiescent"),
                    ("wrong script CSS failure code", "failureCode", "unsupported_work"),
                    ("wrong CSS unsupported kind", "unsupportedKind", "rendering"),
                    ("wrong CSS unsupported count", "unsupportedCount", "0"),
                    ("wrong CSS unsupported reason", "unsupportedReason", "external_io"),
                    (
                        "wrong CSS unsupported time surface",
                        "unsupportedTimeSurface",
                        "document_timestamp",
                    ),
                    (
                        "wrong CSS evidence profile",
                        "evidenceProfile",
                        "controlled-web-session-v1",
                    ),
                    (
                        "auxiliary CSS target falsely accepted",
                        "publicNonAuxiliaryControlledTarget",
                        False,
                    ),
                    ("CSS proof not in one session", "sameControlledSession", False),
                    ("CSS proof reused a tainted process", "freshExactBinaryProcess", False),
                    (
                        "CSS proof used managed runtime fallback",
                        "managedRuntimeFallbackAccesses",
                        "1",
                    ),
                    ("CSS proof not exact binary", "exactBinaryLaunch", False),
                    ("CSS proof missing close and EOF", "closeResponseAndEof", False),
                )
            ],
        ]

        base_v2_cookie_session = gate_record["v2CookieSession"]
        assert isinstance(base_v2_cookie_session, dict)
        v2_cookie_session_record_mutations = [
            (
                "missing cookie/session proof field",
                {
                    key: value
                    for key, value in base_v2_cookie_session.items()
                    if key != "crossSiteLaxCookieFiltered"
                },
            ),
            (
                "extra cookie/session proof field",
                {**base_v2_cookie_session, "diskCookieJar": True},
            ),
            *[
                (label, {**base_v2_cookie_session, field: value})
                for label, field, value in (
                    ("wrong cookie state profile", "stateProfile", "controlled-web-session-v1"),
                    ("wrong cookie state schema", "stateSchemaVersion", "2"),
                    ("wrong persistent expiry", "responseCookieExpiryUnixTimeNs", "0"),
                    ("numeric persistent expiry", "responseCookieExpiryUnixTimeNs", 2592000000000000),
                    ("lost Max-Age precedence", "maxAgePrecedenceOverPastExpires", False),
                    ("restored cookie not sent", "restoredSameSiteCookieSent", False),
                    ("cross-site resource blocked", "crossSiteResourceReachedServer", False),
                    ("cross-site Lax cookie leaked", "crossSiteLaxCookieFiltered", False),
                    ("wrong cross-site method", "crossSiteRequestMethod", "POST"),
                    ("wrong cross-site path", "crossSiteRequestPath", "/other.js"),
                    ("wrong cookie evidence profile", "evidenceProfile", "controlled-web-session-v1"),
                    ("cookie proof claimed disk state", "memoryOnlyExplicitStatePortability", False),
                    ("no-import control retained a cookie", "noImportControlCookieCount", "1"),
                    ("no-import request sent a cookie", "noImportControlRequestCookieHeaderEmpty", False),
                    ("no-import control changed host context", "noImportControlSameHostContext", False),
                    ("wrong cookie time-range code", "cookieTimeRangeFailureCode", "other"),
                    ("cookie time-range failure became fatal", "cookieTimeRangeFatal", True),
                    ("wrong cookie time-range effect", "cookieTimeRangeStateEffect", "none"),
                    ("cookie time-range request reached server", "cookieTimeRangeRequestReachedServer", True),
                    ("cookie child environment not allowlisted", "credentialEnvironmentMode", "inherited"),
                    ("cookie proof reused a process", "freshExactBinaryProcesses", "3"),
                    ("cookie proof missed a graceful session close", "gracefulCookieSessionProcesses", "3"),
                    ("cookie proof used managed fallback", "managedRuntimeFallbackAccesses", "1"),
                    ("cookie proof not exact binary", "exactBinaryLaunch", False),
                    ("cookie proof missing close and EOF", "closeResponseAndEof", False),
                )
            ],
        ]

        v2_fixture_gate_mutations = [
            (
                "missing v2 direct data-SVG proof",
                {
                    key: value
                    for key, value in gate_record.items()
                    if key != "v2DirectDataSvg"
                },
            ),
            *[
                (label, {**gate_record, "v2DirectDataSvg": mutation})
                for label, mutation in v2_direct_data_svg_record_mutations
            ],
            (
                "missing v2 inline SVG rendering proof",
                {
                    key: value
                    for key, value in gate_record.items()
                    if key != "v2InlineSvgRendering"
                },
            ),
            *[
                (label, {**gate_record, "v2InlineSvgRendering": mutation})
                for label, mutation in v2_inline_svg_rendering_record_mutations
            ],
            (
                "missing v2 InputMethod focus proof",
                {
                    key: value
                    for key, value in gate_record.items()
                    if key != "v2InputMethodFocus"
                },
            ),
            *[
                (label, {**gate_record, "v2InputMethodFocus": mutation})
                for label, mutation in v2_input_method_focus_record_mutations
            ],
            (
                "missing v2 automation event-timestamps proof",
                {
                    key: value
                    for key, value in gate_record.items()
                    if key != "v2AutomationEventTimestamps"
                },
            ),
            *[
                (label, {**gate_record, "v2AutomationEventTimestamps": mutation})
                for label, mutation in v2_automation_event_timestamps_record_mutations
            ],
            (
                "missing v2 CSS animation-event timestamps proof",
                {
                    key: value
                    for key, value in gate_record.items()
                    if key != "v2CssAnimationEventTimestamps"
                },
            ),
            *[
                (label, {**gate_record, "v2CssAnimationEventTimestamps": mutation})
                for label, mutation in v2_css_animation_event_timestamps_record_mutations
            ],
            (
                "missing v2 cookie/session proof",
                {
                    key: value
                    for key, value in gate_record.items()
                    if key != "v2CookieSession"
                },
            ),
            *[
                (label, {**gate_record, "v2CookieSession": mutation})
                for label, mutation in v2_cookie_session_record_mutations
            ],
        ]
        for index, (label, mutated_gate) in enumerate(v2_fixture_gate_mutations):
            mutated_gate_log = root / f"v2-fixture-gate-mutation-{index}.log"
            mutated_gate_log.write_text(
                strict_json_dumps(mutated_gate, separators=(",", ":")) + "\n",
                encoding="utf-8",
            )
            expect_error(
                label,
                lambda gate_log=mutated_gate_log: parse_gate_log(
                    gate_log, package, version, revision
                ),
            )

        proof_text = proof.read_text(encoding="utf-8")
        proof_document = strict_json_loads(proof_text, "self-test SDK gate proof")
        assert isinstance(proof_document, dict)
        v2_proof_mutations = [
            (
                "missing durable v2 MessageChannel proof",
                {
                    key: value
                    for key, value in proof_document.items()
                    if key != "v2MessageChannel"
                },
            ),
            *[
                (label, {**proof_document, "v2MessageChannel": mutation})
                for label, mutation in v2_record_mutations
            ],
        ]
        for label, mutated_proof in v2_proof_mutations:
            proof.write_text(
                strict_json_dumps(mutated_proof, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )
            expect_error(
                f"durable proof {label}",
                lambda: verify_proof(
                    directory=artifact,
                    version=version,
                    revision=revision,
                    native_binary_sha256=binary_digest,
                    run_id="123",
                    run_attempt="1",
                ),
            )
        v2_fixture_proof_mutations = [
            (
                "missing durable v2 direct data-SVG proof",
                {
                    key: value
                    for key, value in proof_document.items()
                    if key != "v2DirectDataSvg"
                },
            ),
            *[
                (label, {**proof_document, "v2DirectDataSvg": mutation})
                for label, mutation in v2_direct_data_svg_record_mutations
            ],
            (
                "missing durable v2 inline SVG rendering proof",
                {
                    key: value
                    for key, value in proof_document.items()
                    if key != "v2InlineSvgRendering"
                },
            ),
            *[
                (label, {**proof_document, "v2InlineSvgRendering": mutation})
                for label, mutation in v2_inline_svg_rendering_record_mutations
            ],
            (
                "missing durable v2 InputMethod focus proof",
                {
                    key: value
                    for key, value in proof_document.items()
                    if key != "v2InputMethodFocus"
                },
            ),
            *[
                (label, {**proof_document, "v2InputMethodFocus": mutation})
                for label, mutation in v2_input_method_focus_record_mutations
            ],
            (
                "missing durable v2 automation event-timestamps proof",
                {
                    key: value
                    for key, value in proof_document.items()
                    if key != "v2AutomationEventTimestamps"
                },
            ),
            *[
                (
                    label,
                    {**proof_document, "v2AutomationEventTimestamps": mutation},
                )
                for label, mutation in v2_automation_event_timestamps_record_mutations
            ],
            (
                "missing durable v2 CSS animation-event timestamps proof",
                {
                    key: value
                    for key, value in proof_document.items()
                    if key != "v2CssAnimationEventTimestamps"
                },
            ),
            *[
                (
                    label,
                    {**proof_document, "v2CssAnimationEventTimestamps": mutation},
                )
                for label, mutation in v2_css_animation_event_timestamps_record_mutations
            ],
            (
                "missing durable v2 cookie/session proof",
                {
                    key: value
                    for key, value in proof_document.items()
                    if key != "v2CookieSession"
                },
            ),
            *[
                (label, {**proof_document, "v2CookieSession": mutation})
                for label, mutation in v2_cookie_session_record_mutations
            ],
        ]
        for label, mutated_proof in v2_fixture_proof_mutations:
            proof.write_text(
                strict_json_dumps(mutated_proof, indent=2, sort_keys=True) + "\n",
                encoding="utf-8",
            )
            expect_error(
                f"durable proof {label}",
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

        proof.write_text('{"schema":7,' + proof_text.lstrip()[1:], encoding="utf-8")
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
            proof_text.replace('"schema": 7', '"schema": 1e400'), encoding="utf-8"
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
            proof_text.replace('"schema": 7', '"schema": 7.0'), encoding="utf-8"
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
