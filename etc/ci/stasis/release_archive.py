#!/usr/bin/env python3
"""Build and verify the exact Stasis 0.2.0 native release archives."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import re
import shutil
import stat
import tarfile
import tempfile
import tomllib
import zlib
from pathlib import Path
from typing import BinaryIO, Iterable


STABLE_VERSION = "0.2.0"
VERSION_RE = re.compile(re.escape(STABLE_VERSION))
REVISION_RE = re.compile(r"[0-9a-f]{40}")
SHA256_RE = re.compile(r"[0-9a-f]{64}")
RUN_ID_RE = re.compile(r"[1-9][0-9]*")
FROZEN_V1_PROFILE = Path("profiles/controlled-webapp-v1.json")
FROZEN_V1_PROFILE_SHA256 = (
    "6e262edf0f8be11a1cece28f68f00d59fdac68b79b0a670ac891d36998720100"
)
FROZEN_V2_PROFILE = Path("profiles/controlled-web-session-v1.json")
FROZEN_V2_PROFILE_SHA256 = (
    "9b62b9245b2c6a6f9620b117da6787a18df9298be1115cbce2e6c3d5439cc41a"
)

BINARY_NAME = "stasis"
THIRD_PARTY_NAME = "THIRD_PARTY_LICENSES.html"
PLATFORM_CONTRACTS: dict[str, dict[str, str]] = {
    "linux-x86_64": {
        "display_name": "Linux x86_64",
        "operating_system": "Linux",
        "architecture": "x86_64",
        "abi": "GNU/Linux with glibc 2.35 or newer",
        "install_note": (
            "This executable targets x86_64 GNU/Linux with glibc 2.35 or newer "
            "(the Ubuntu 22.04 compatibility floor)."
        ),
        "dependency_note": (
            "External runtime baseline: Ubuntu 22.04-compatible x86_64 userspace with "
            "glibc 2.35 or newer; system shared libraries are resolved by the GNU/Linux "
            "dynamic loader."
        ),
    },
    "macos-aarch64": {
        "display_name": "macOS Apple Silicon",
        "operating_system": "macOS",
        "architecture": "arm64",
        "abi": "macOS arm64",
        "install_note": (
            "This macOS arm64 executable is unsigned and is not Apple-notarized."
        ),
        "dependency_note": (
            "External runtime dependencies: Apple system libraries and frameworks "
            "supplied by macOS."
        ),
    },
}
SOURCE_ASSETS = {
    "LICENSE": Path("LICENSE"),
    "LICENSE_WHATWG_SPECS": Path("LICENSE_WHATWG_SPECS"),
    "STASIS_UPSTREAM.toml": Path("STASIS_UPSTREAM.toml"),
    THIRD_PARTY_NAME: Path("resources/resource_protocol/license.html"),
}
GENERATED_ASSETS = {
    "INSTALL.txt",
    "NATIVE-LIBRARIES.txt",
    "README.md",
    "SOURCE.txt",
    "VERSION.txt",
}
EXPECTED_FILES = {BINARY_NAME, *SOURCE_ASSETS, *GENERATED_ASSETS}
MAX_COMPRESSED_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_BINARY_MEMBER_BYTES = 512 * 1024 * 1024
MAX_TEXT_MEMBER_BYTES = 8 * 1024 * 1024
MAX_UNCOMPRESSED_ARCHIVE_BYTES = 600 * 1024 * 1024
DECOMPRESSION_CHUNK_BYTES = 4 * 1024 * 1024
GATE_TEST = "release_gate_published_binary_completes_act_settle_inspect"
GATE_SUCCESS = "test result: ok. 1 passed; 0 failed; 0 ignored;"
GATE_NAME = "act-settle-inspect"
GATE_PROOF_SCHEMA = 2
GATE_RECORD_PREFIX = "[RELEASE ARTIFACT] "
STASIS_REPOSITORY_IDENTITY = "https://github.com/oxhq/stasis.git"
UPSTREAM_IDENTITIES = {
    "servo_repository": "https://github.com/servo/servo.git",
    "servo_revision": "0d579bd5aab6df3764fad805427254751632a6e4",
    "pliego_repository": "https://github.com/oxhq/pliego.git",
    "pliego_revision": "556c774242b272b11bc60999449c5debff1ad20f",
    "pliego_servo_merge_base": "313b6d5ecc113b08010ce434140db3ca5abcc71c",
}


class ReleaseError(RuntimeError):
    pass


def reject_duplicate_json_members(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for name, value in pairs:
        if name in result:
            raise ValueError(f"duplicate JSON object member {name!r}")
        result[name] = value
    return result


def reject_nonfinite_json_number(value: str) -> None:
    raise ValueError(f"non-finite JSON number {value!r}")


def strict_json_loads(document: str, description: str) -> object:
    try:
        return json.loads(
            document,
            object_pairs_hook=reject_duplicate_json_members,
            parse_constant=reject_nonfinite_json_number,
        )
    except (json.JSONDecodeError, ValueError) as error:
        raise ReleaseError(f"{description} is invalid strict JSON: {error}") from error


def require_fullmatch(pattern: re.Pattern[str], value: str, field: str) -> str:
    if pattern.fullmatch(value) is None:
        raise ReleaseError(f"invalid {field}: {value!r}")
    return value


def validate_identity(version: str, platform: str, revision: str, repository: str) -> None:
    require_fullmatch(VERSION_RE, version, "version")
    if platform not in PLATFORM_CONTRACTS:
        raise ReleaseError(
            f"unsupported platform {platform!r}; expected one of {sorted(PLATFORM_CONTRACTS)}"
        )
    require_fullmatch(REVISION_RE, revision, "revision")
    if not repository.startswith("https://") or repository.endswith("/"):
        raise ReleaseError(f"repository must be an https URL without a trailing slash: {repository!r}")


def platform_contract(platform: str) -> dict[str, str]:
    contract = PLATFORM_CONTRACTS.get(platform)
    if contract is None:
        raise ReleaseError(
            f"unsupported platform {platform!r}; expected one of {sorted(PLATFORM_CONTRACTS)}"
        )
    return contract


def expected_source_identities(revision: str) -> dict[str, str]:
    require_fullmatch(REVISION_RE, revision, "revision")
    return {
        **UPSTREAM_IDENTITIES,
        "stasis_repository": STASIS_REPOSITORY_IDENTITY,
        "stasis_revision": revision,
    }


def verify_upstream_manifest(filename: Path) -> None:
    try:
        manifest = tomllib.loads(filename.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise ReleaseError(f"cannot parse Stasis upstream manifest: {error}") from error
    if manifest != UPSTREAM_IDENTITIES:
        raise ReleaseError("STASIS_UPSTREAM.toml does not contain the exact release source identities")


def bundle_name(version: str, platform: str) -> str:
    return f"stasis-{version}-{platform}"


def release_asset_names(version: str, platform: str) -> dict[str, str]:
    bundle = bundle_name(version, platform)
    archive = f"{bundle}.tar.gz"
    return {
        "archive": archive,
        "archive_sha256": f"{archive}.sha256",
        "binary_sha256": f"{bundle}.binary.sha256",
        "gate_proof": f"{bundle}-act-settle-inspect.json",
    }


def source_text(repository: str, revision: str) -> str:
    return (
        f"Repository: {repository}\n"
        f"Source: {repository}/commit/{revision}\n"
        f"Revision: {revision}\n"
    )


def version_text(version: str, platform: str, revision: str) -> str:
    return f"stasis {version}\nplatform {platform}\nrevision {revision}\n"


def install_text(version: str, platform: str) -> str:
    names = release_asset_names(version, platform)
    contract = platform_contract(platform)
    return (
        f"1. Verify {names['archive_sha256']} before extracting this archive.\n"
        f"2. Extract {names['archive']} and put the stasis executable on PATH.\n"
        f"3. Optionally verify the extracted executable with {names['binary_sha256']}.\n"
        "4. Read README.md and NATIVE-LIBRARIES.txt for the exact platform contract.\n"
        f"5. {contract['install_note']}\n"
    )


def readme_text(version: str, platform: str, repository: str) -> str:
    contract = platform_contract(platform)
    return (
        f"# Stasis {version}\n\n"
        "Stasis is a controlled Servo runtime for deterministic web-application "
        "automation within its declared support profile.\n\n"
        f"- Artifact: `{platform}`\n"
        f"- Platform: {contract['display_name']}\n"
        f"- Operating system: {contract['operating_system']}\n"
        f"- Architecture: {contract['architecture']}\n"
        f"- ABI: {contract['abi']}\n"
        f"- Source: {repository}\n\n"
        "Start with INSTALL.txt. NATIVE-LIBRARIES.txt records the native runtime "
        "dependency boundary for this exact artifact.\n"
    )


def native_libraries_text(version: str, platform: str) -> str:
    contract = platform_contract(platform)
    return (
        f"Artifact: {bundle_name(version, platform)}\n"
        f"Operating system: {contract['operating_system']}\n"
        f"Architecture: {contract['architecture']}\n"
        f"ABI: {contract['abi']}\n"
        "Separate bundled native-library files: none\n"
        f"{contract['dependency_note']}\n"
    )


def sha256_file(filename: Path) -> str:
    digest = hashlib.sha256()
    with filename.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def files_equal(first: Path, second: Path) -> bool:
    if first.stat().st_size != second.stat().st_size:
        return False
    with first.open("rb") as first_handle, second.open("rb") as second_handle:
        while True:
            first_chunk = first_handle.read(1024 * 1024)
            second_chunk = second_handle.read(1024 * 1024)
            if first_chunk != second_chunk:
                return False
            if not first_chunk:
                return True


def write_sidecar(filename: Path, digest: str, label: str) -> None:
    require_fullmatch(SHA256_RE, digest, "SHA-256")
    filename.write_text(f"{digest}  {label}\n", encoding="ascii", newline="\n")


def parse_sidecar(filename: Path, expected_label: str) -> str:
    try:
        content = filename.read_text(encoding="ascii")
    except (OSError, UnicodeError) as error:
        raise ReleaseError(f"cannot read checksum sidecar {filename}: {error}") from error
    match = re.fullmatch(r"([0-9a-f]{64})  ([^\r\n]+)\n", content)
    if match is None or match.group(2) != expected_label:
        raise ReleaseError(
            f"checksum sidecar {filename.name} must contain one canonical entry for {expected_label}"
        )
    return match.group(1)


def require_regular_file(filename: Path, description: str) -> None:
    if filename.is_symlink() or not filename.is_file():
        raise ReleaseError(f"{description} is not a regular file: {filename}")


def verify_frozen_v1_profile(source_root: Path) -> dict[str, str]:
    filename = source_root / FROZEN_V1_PROFILE
    require_regular_file(filename, "frozen controlled-webapp-v1 profile")
    actual = sha256_file(filename)
    if actual != FROZEN_V1_PROFILE_SHA256:
        raise ReleaseError(
            "frozen controlled-webapp-v1 profile SHA-256 differs: "
            f"expected {FROZEN_V1_PROFILE_SHA256}, got {actual}"
        )
    return {"path": FROZEN_V1_PROFILE.as_posix(), "sha256": actual}


def verify_frozen_v2_profile(source_root: Path) -> dict[str, str]:
    filename = source_root / FROZEN_V2_PROFILE
    require_regular_file(filename, "frozen controlled-web-session-v1 profile")
    actual = sha256_file(filename)
    if actual != FROZEN_V2_PROFILE_SHA256:
        raise ReleaseError(
            "frozen controlled-web-session-v1 profile SHA-256 differs: "
            f"expected {FROZEN_V2_PROFILE_SHA256}, got {actual}"
        )
    return {"path": FROZEN_V2_PROFILE.as_posix(), "sha256": actual}


def normalized_tar_info(name: str, *, directory: bool, executable: bool = False) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.type = tarfile.DIRTYPE if directory else tarfile.REGTYPE
    info.mode = 0o755 if directory or executable else 0o644
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.mtime = 0
    return info


def add_normalized_tar_members(package: tarfile.TarFile, bundle_directory: Path) -> None:
    members = sorted(bundle_directory.iterdir(), key=lambda item: item.name)
    package.addfile(normalized_tar_info(bundle_directory.name, directory=True))
    for member in members:
        require_regular_file(member, "bundle member")
        info = normalized_tar_info(
            f"{bundle_directory.name}/{member.name}",
            directory=False,
            executable=member.name == BINARY_NAME,
        )
        info.size = member.stat().st_size
        with member.open("rb") as source:
            package.addfile(info, source)


def create_deterministic_tar(bundle_directory: Path, archive: Path) -> None:
    with tarfile.open(archive, mode="w", format=tarfile.USTAR_FORMAT) as package:
        add_normalized_tar_members(package, bundle_directory)


def create_deterministic_archive(bundle_directory: Path, archive: Path) -> None:
    with archive.open("wb") as raw_archive:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw_archive, compresslevel=9, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as package:
                add_normalized_tar_members(package, bundle_directory)


def decompress_canonical_gzip(
    archive: Path,
    destination: Path,
    *,
    max_output_bytes: int = MAX_UNCOMPRESSED_ARCHIVE_BYTES,
) -> None:
    if max_output_bytes <= 0:
        raise ReleaseError("maximum decompressed archive size must be positive")
    expected_header = bytes.fromhex("1f8b08000000000002ff")
    with archive.open("rb") as source:
        header = source.read(len(expected_header))
        if header != expected_header:
            raise ReleaseError("archive does not use the canonical Stasis gzip header")
        source.seek(0)
        decompressor = zlib.decompressobj(wbits=16 + zlib.MAX_WBITS)
        output_bytes = 0
        with destination.open("wb") as output:
            while chunk := source.read(1024 * 1024):
                pending = chunk
                while pending:
                    remaining = max_output_bytes - output_bytes
                    decompressed = decompressor.decompress(
                        pending,
                        min(DECOMPRESSION_CHUNK_BYTES, remaining + 1),
                    )
                    output_bytes += len(decompressed)
                    if output_bytes > max_output_bytes:
                        raise ReleaseError(
                            "archive exceeds the maximum decompressed size "
                            f"of {max_output_bytes} bytes"
                        )
                    output.write(decompressed)
                    pending = decompressor.unconsumed_tail
                    if decompressor.eof:
                        break
                if decompressor.eof:
                    if decompressor.unused_data or source.read(1):
                        raise ReleaseError("archive has data or another gzip member after canonical EOF")
                    break
            flushed = decompressor.flush(
                min(DECOMPRESSION_CHUNK_BYTES, max_output_bytes - output_bytes + 1)
            )
            output_bytes += len(flushed)
            if output_bytes > max_output_bytes:
                raise ReleaseError(
                    "archive exceeds the maximum decompressed size "
                    f"of {max_output_bytes} bytes"
                )
            output.write(flushed)
        if not decompressor.eof:
            raise ReleaseError("archive gzip stream ended before its validated trailer")


def expected_generated_assets(
    version: str, platform: str, revision: str, repository: str
) -> dict[str, bytes]:
    return {
        "INSTALL.txt": install_text(version, platform).encode("utf-8"),
        "NATIVE-LIBRARIES.txt": native_libraries_text(version, platform).encode("utf-8"),
        "README.md": readme_text(version, platform, repository).encode("utf-8"),
        "SOURCE.txt": source_text(repository, revision).encode("utf-8"),
        "VERSION.txt": version_text(version, platform, revision).encode("utf-8"),
    }


def member_size_limit(name: str) -> int:
    return MAX_BINARY_MEMBER_BYTES if name == BINARY_NAME else MAX_TEXT_MEMBER_BYTES


def require_bounded_nonempty_file(filename: Path, name: str) -> None:
    require_regular_file(filename, f"bundle member {name}")
    size = filename.stat().st_size
    limit = member_size_limit(name)
    if size <= 0 or size > limit:
        raise ReleaseError(
            f"bundle member {name} has invalid size {size}; allowed range is 1..{limit} bytes"
        )


def validate_bundle_directory(
    directory: Path,
    *,
    version: str,
    platform: str,
    revision: str,
    repository: str,
    source_root: Path,
) -> None:
    actual = {item.name for item in directory.iterdir()}
    if actual != EXPECTED_FILES:
        raise ReleaseError(
            f"bundle inventory differs: missing={sorted(EXPECTED_FILES - actual)} "
            f"extra={sorted(actual - EXPECTED_FILES)}"
        )
    for name in EXPECTED_FILES:
        require_bounded_nonempty_file(directory / name, name)
    if stat.S_IMODE((directory / BINARY_NAME).stat().st_mode) & 0o111 == 0:
        raise ReleaseError("packaged stasis executable is not executable")
    for packaged_name, source_name in SOURCE_ASSETS.items():
        source = source_root / source_name
        require_regular_file(source, f"source asset {source_name}")
        if (directory / packaged_name).read_bytes() != source.read_bytes():
            raise ReleaseError(f"packaged source asset differs: {packaged_name}")
    verify_upstream_manifest(directory / "STASIS_UPSTREAM.toml")
    for name, expected in expected_generated_assets(version, platform, revision, repository).items():
        if (directory / name).read_bytes() != expected:
            raise ReleaseError(f"generated release asset differs: {name}")


def create_release(
    *,
    binary: Path,
    dist: Path,
    version: str,
    platform: str,
    revision: str,
    repository: str,
    source_root: Path,
) -> dict[str, str]:
    validate_identity(version, platform, revision, repository)
    require_bounded_nonempty_file(binary, BINARY_NAME)
    if stat.S_IMODE(binary.stat().st_mode) & 0o111 == 0:
        raise ReleaseError(f"Stasis binary is not executable: {binary}")
    for packaged_name, source_name in SOURCE_ASSETS.items():
        source = source_root / source_name
        require_regular_file(source, f"source asset for {packaged_name}")
        size = source.stat().st_size
        if size <= 0 or size > MAX_TEXT_MEMBER_BYTES:
            raise ReleaseError(
                f"source asset {source_name} has invalid size {size}; "
                f"allowed range is 1..{MAX_TEXT_MEMBER_BYTES} bytes"
            )

    dist.mkdir(parents=True, exist_ok=True)
    names = release_asset_names(version, platform)
    expected_outputs = {names["archive"], names["archive_sha256"], names["binary_sha256"]}
    existing_outputs = {name for name in expected_outputs if (dist / name).exists()}
    if existing_outputs:
        raise ReleaseError(f"refusing to overwrite release outputs: {sorted(existing_outputs)}")

    with tempfile.TemporaryDirectory(prefix=".stasis-package-", dir=dist) as temporary:
        bundle = Path(temporary) / bundle_name(version, platform)
        bundle.mkdir(mode=0o755)
        shutil.copyfile(binary, bundle / BINARY_NAME)
        (bundle / BINARY_NAME).chmod(0o755)
        for packaged_name, source_name in SOURCE_ASSETS.items():
            shutil.copyfile(source_root / source_name, bundle / packaged_name)
            (bundle / packaged_name).chmod(0o644)
        generated = expected_generated_assets(version, platform, revision, repository)
        for name, content in generated.items():
            (bundle / name).write_bytes(content)
            (bundle / name).chmod(0o644)
        validate_bundle_directory(
            bundle,
            version=version,
            platform=platform,
            revision=revision,
            repository=repository,
            source_root=source_root,
        )
        create_deterministic_archive(bundle, dist / names["archive"])

    archive_digest = sha256_file(dist / names["archive"])
    binary_digest = sha256_file(binary)
    write_sidecar(dist / names["archive_sha256"], archive_digest, names["archive"])
    write_sidecar(
        dist / names["binary_sha256"],
        binary_digest,
        f"{bundle_name(version, platform)}/{BINARY_NAME}",
    )
    return {
        "archive": str(dist / names["archive"]),
        "archiveSha256": archive_digest,
        "binarySha256": binary_digest,
    }


def verify_asset_directory(directory: Path, expected: Iterable[str]) -> None:
    if not directory.is_dir():
        raise ReleaseError(f"release asset directory does not exist: {directory}")
    expected_set = set(expected)
    entries = list(directory.iterdir())
    actual = {entry.name for entry in entries}
    if any(entry.is_symlink() or not entry.is_file() for entry in entries) or actual != expected_set:
        raise ReleaseError(
            f"release assets differ: missing={sorted(expected_set - actual)} "
            f"extra={sorted(actual - expected_set)}"
        )


def verify_tar_metadata(package: tarfile.TarFile, bundle: str) -> dict[str, tarfile.TarInfo]:
    expected_member_count = len(EXPECTED_FILES) + 1
    members: list[tarfile.TarInfo] = []
    for member in package:
        if len(members) == expected_member_count:
            raise ReleaseError(
                f"archive contains more than the exact {expected_member_count}-member limit"
            )
        members.append(member)
    names = [member.name for member in members]
    expected_names = {bundle, *(f"{bundle}/{name}" for name in EXPECTED_FILES)}
    if len(names) != len(set(names)) or set(names) != expected_names:
        raise ReleaseError(
            f"archive inventory differs: missing={sorted(expected_names - set(names))} "
            f"extra={sorted(set(names) - expected_names)}"
        )
    by_name = {member.name: member for member in members}
    root = by_name[bundle]
    if not root.isdir() or root.mode != 0o755:
        raise ReleaseError("archive root is not the normalized bundle directory")
    for member in members:
        if member.uid != 0 or member.gid != 0 or member.uname or member.gname or member.mtime != 0:
            raise ReleaseError(f"archive member metadata is not normalized: {member.name}")
        if member.name == bundle:
            continue
        relative = member.name.removeprefix(f"{bundle}/")
        expected_mode = 0o755 if relative == BINARY_NAME else 0o644
        if not member.isfile() or member.mode != expected_mode:
            raise ReleaseError(f"archive member type or mode is invalid: {member.name}")
        member_limit = member_size_limit(relative)
        if member.size <= 0 or member.size > member_limit:
            raise ReleaseError(
                f"archive member {member.name} has invalid size {member.size}; "
                f"allowed range is 1..{member_limit} bytes"
            )
    total_member_bytes = sum(member.size for member in members if member.isfile())
    if total_member_bytes > MAX_UNCOMPRESSED_ARCHIVE_BYTES:
        raise ReleaseError(
            "archive members exceed the maximum total uncompressed size "
            f"of {MAX_UNCOMPRESSED_ARCHIVE_BYTES} bytes"
        )
    return by_name


def copy_and_hash(source: BinaryIO, destination: BinaryIO | None) -> str:
    digest = hashlib.sha256()
    for chunk in iter(lambda: source.read(1024 * 1024), b""):
        digest.update(chunk)
        if destination is not None:
            destination.write(chunk)
    return digest.hexdigest()


def verify_release(
    *,
    asset_directory: Path,
    version: str,
    platform: str,
    revision: str,
    repository: str,
    source_root: Path,
    extract_to: Path | None,
) -> dict[str, str]:
    validate_identity(version, platform, revision, repository)
    verify_upstream_manifest(source_root / SOURCE_ASSETS["STASIS_UPSTREAM.toml"])
    names = release_asset_names(version, platform)
    verify_asset_directory(
        asset_directory,
        [names["archive"], names["archive_sha256"], names["binary_sha256"]],
    )
    archive = asset_directory / names["archive"]
    archive_size = archive.stat().st_size
    if archive_size <= 0 or archive_size > MAX_COMPRESSED_ARCHIVE_BYTES:
        raise ReleaseError(
            f"archive size {archive_size} is outside the allowed range "
            f"1..{MAX_COMPRESSED_ARCHIVE_BYTES} bytes"
        )
    archive_digest = sha256_file(archive)
    if parse_sidecar(asset_directory / names["archive_sha256"], names["archive"]) != archive_digest:
        raise ReleaseError("archive SHA-256 does not match its sidecar")
    expected_binary_digest = parse_sidecar(
        asset_directory / names["binary_sha256"],
        f"{bundle_name(version, platform)}/{BINARY_NAME}",
    )

    stage: Path | None = None
    bundle = bundle_name(version, platform)
    if extract_to is not None:
        if extract_to.exists():
            raise ReleaseError(f"refusing to overwrite extraction destination: {extract_to}")
        extract_to.parent.mkdir(parents=True, exist_ok=True)
        stage = Path(tempfile.mkdtemp(prefix=f".{extract_to.name}.", dir=extract_to.parent))
    else:
        stage = Path(tempfile.mkdtemp(prefix=".stasis-verify-"))

    try:
        downloaded_tar = stage / ".downloaded.tar"
        decompress_canonical_gzip(archive, downloaded_tar)
        with tarfile.open(downloaded_tar, mode="r:") as package:
            by_name = verify_tar_metadata(package, bundle)
            generated = expected_generated_assets(version, platform, revision, repository)
            for name in sorted(EXPECTED_FILES):
                member = by_name[f"{bundle}/{name}"]
                source = package.extractfile(member)
                if source is None:
                    raise ReleaseError(f"cannot read archive member: {member.name}")
                destination_path = stage / bundle / name
                destination_path.parent.mkdir(parents=True, exist_ok=True)
                destination_handle: BinaryIO | None = destination_path.open("wb")
                try:
                    if name == BINARY_NAME:
                        binary_digest = copy_and_hash(source, destination_handle)
                    else:
                        content = source.read()
                        if destination_handle is not None:
                            destination_handle.write(content)
                        if name in SOURCE_ASSETS:
                            source_filename = source_root / SOURCE_ASSETS[name]
                            require_regular_file(source_filename, f"source asset {SOURCE_ASSETS[name]}")
                            expected = source_filename.read_bytes()
                        else:
                            expected = generated[name]
                        if content != expected:
                            raise ReleaseError(f"archive member content differs: {name}")
                finally:
                    if destination_handle is not None:
                        destination_handle.close()
                destination_path.chmod(0o755 if name == BINARY_NAME else 0o644)
        if binary_digest != expected_binary_digest:
            raise ReleaseError("packaged binary SHA-256 does not match its sidecar")
        canonical_tar = stage / ".canonical.tar"
        create_deterministic_tar(stage / bundle, canonical_tar)
        if not files_equal(downloaded_tar, canonical_tar):
            raise ReleaseError(
                "archive tar bytes are not the canonical normalized Stasis serialization"
            )
        canonical_tar.unlink()
        downloaded_tar.unlink()
        if extract_to is not None:
            stage.rename(extract_to)
            stage = None
    finally:
        if stage is not None:
            shutil.rmtree(stage)

    result = {
        "archive": str(archive),
        "archiveSha256": archive_digest,
        "binarySha256": expected_binary_digest,
    }
    if extract_to is not None:
        result["binary"] = str(extract_to / bundle / BINARY_NAME)
    return result


def parse_native_gate_identity(
    log_text: str,
    *,
    version: str,
    revision: str,
    archive_name: str,
    archive_digest: str,
    binary_digest: str,
) -> dict[str, str]:
    records = [
        strict_json_loads(line.removeprefix(GATE_RECORD_PREFIX), "gate log release record")
        for line in log_text.splitlines()
        if line.startswith(GATE_RECORD_PREFIX)
    ]
    if len(records) != 1:
        raise ReleaseError("gate log must contain exactly one structured release-artifact record")
    record = records[0]
    expected_keys = {"schema", "gate", "test", "version", "archive", "binary", "source"}
    if not isinstance(record, dict) or set(record) != expected_keys:
        raise ReleaseError("gate log release-artifact record has an unexpected schema")
    if type(record["schema"]) is not int or record["schema"] != GATE_PROOF_SCHEMA:
        raise ReleaseError("gate log release-artifact record schema does not match")
    if (
        record["gate"] != GATE_NAME
        or record["test"] != GATE_TEST
        or record["version"] != version
    ):
        raise ReleaseError("gate log release-artifact gate or version does not match")
    if record["archive"] != {"name": archive_name, "sha256": archive_digest}:
        raise ReleaseError("gate log release-artifact archive does not match the selected archive")
    binary = record["binary"]
    if (
        not isinstance(binary, dict)
        or set(binary) != {"path", "sha256"}
        or not isinstance(binary["path"], str)
        or not binary["path"]
        or binary["sha256"] != binary_digest
    ):
        raise ReleaseError("gate log release-artifact binary does not match")
    sources = record["source"]
    expected = expected_source_identities(revision)
    if sources != expected:
        raise ReleaseError("gate log does not report the exact Stasis and upstream source identities")
    return expected


def create_gate_proof(
    *,
    proof: Path,
    gate_log: Path,
    asset_directory: Path,
    version: str,
    platform: str,
    revision: str,
    run_id: str,
    run_attempt: str,
) -> dict[str, object]:
    validate_identity(version, platform, revision, "https://github.com/oxhq/stasis")
    require_fullmatch(RUN_ID_RE, run_id, "workflow run ID")
    require_fullmatch(RUN_ID_RE, run_attempt, "workflow run attempt")
    log_text = gate_log.read_text(encoding="utf-8", errors="strict")
    if log_text.count(GATE_SUCCESS) != 1 or log_text.count(f"test {GATE_TEST} ... ok") != 1:
        raise ReleaseError("gate log does not prove the exact one-test act-settle-inspect gate passed")
    names = release_asset_names(version, platform)
    archive = asset_directory / names["archive"]
    archive_digest = parse_sidecar(asset_directory / names["archive_sha256"], names["archive"])
    if archive_digest != sha256_file(archive):
        raise ReleaseError("cannot create gate proof for an archive with an invalid sidecar")
    binary_label = f"{bundle_name(version, platform)}/{BINARY_NAME}"
    binary_digest = parse_sidecar(asset_directory / names["binary_sha256"], binary_label)
    sources = parse_native_gate_identity(
        log_text,
        version=version,
        revision=revision,
        archive_name=names["archive"],
        archive_digest=archive_digest,
        binary_digest=binary_digest,
    )
    document: dict[str, object] = {
        "schema": GATE_PROOF_SCHEMA,
        "gate": GATE_NAME,
        "test": GATE_TEST,
        "version": version,
        "platform": platform,
        "revision": revision,
        "workflowRunId": run_id,
        "workflowRunAttempt": run_attempt,
        "archive": {"name": names["archive"], "sha256": archive_digest},
        "binary": {"path": binary_label, "sha256": binary_digest},
        "source": sources,
        "gateLogSha256": sha256_file(gate_log),
    }
    if proof.exists():
        raise ReleaseError(f"refusing to overwrite gate proof: {proof}")
    proof.write_text(
        json.dumps(document, allow_nan=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )
    return document


def verify_gate_proof(
    *,
    proof_directory: Path,
    asset_directory: Path,
    version: str,
    platform: str,
    revision: str,
    run_id: str,
    run_attempt: str,
) -> dict[str, object]:
    validate_identity(version, platform, revision, "https://github.com/oxhq/stasis")
    require_fullmatch(RUN_ID_RE, run_id, "workflow run ID")
    require_fullmatch(RUN_ID_RE, run_attempt, "workflow run attempt")
    names = release_asset_names(version, platform)
    verify_asset_directory(proof_directory, [names["gate_proof"]])
    verify_asset_directory(
        asset_directory,
        [names["archive"], names["archive_sha256"], names["binary_sha256"]],
    )
    proof_path = proof_directory / names["gate_proof"]
    try:
        proof_text = proof_path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise ReleaseError(f"cannot read gate proof: {error}") from error
    document = strict_json_loads(proof_text, "gate proof")
    expected_top_level = {
        "schema",
        "gate",
        "test",
        "version",
        "platform",
        "revision",
        "workflowRunId",
        "workflowRunAttempt",
        "archive",
        "binary",
        "source",
        "gateLogSha256",
    }
    if not isinstance(document, dict) or set(document) != expected_top_level:
        raise ReleaseError("gate proof has an unexpected top-level schema")
    archive_digest = parse_sidecar(asset_directory / names["archive_sha256"], names["archive"])
    if sha256_file(asset_directory / names["archive"]) != archive_digest:
        raise ReleaseError("gate proof archive sidecar does not match the selected archive")
    binary_label = f"{bundle_name(version, platform)}/{BINARY_NAME}"
    binary_digest = parse_sidecar(asset_directory / names["binary_sha256"], binary_label)
    expected_values = {
        "schema": GATE_PROOF_SCHEMA,
        "gate": GATE_NAME,
        "test": GATE_TEST,
        "version": version,
        "platform": platform,
        "revision": revision,
        "workflowRunId": run_id,
        "workflowRunAttempt": run_attempt,
        "archive": {"name": names["archive"], "sha256": archive_digest},
        "binary": {"path": binary_label, "sha256": binary_digest},
        "source": expected_source_identities(revision),
    }
    for key, expected in expected_values.items():
        if document.get(key) != expected:
            raise ReleaseError(f"gate proof field {key} does not match the selected package run")
    if type(document["schema"]) is not int:
        raise ReleaseError("gate proof schema must be an integer")
    gate_log_digest = document["gateLogSha256"]
    if not isinstance(gate_log_digest, str):
        raise ReleaseError("gate proof log SHA-256 must be a string")
    require_fullmatch(SHA256_RE, gate_log_digest, "gate log SHA-256")
    return document


def self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="stasis-release-self-test-") as temporary:
        root = Path(temporary)
        repository_root = Path(__file__).resolve().parents[3]
        verify_frozen_v1_profile(repository_root)
        verify_frozen_v2_profile(repository_root)
        frozen_profile_root = root / "frozen-profile-source"
        frozen_v1_profile = frozen_profile_root / FROZEN_V1_PROFILE
        frozen_v1_profile.parent.mkdir(parents=True)
        shutil.copyfile(repository_root / FROZEN_V1_PROFILE, frozen_v1_profile)
        verify_frozen_v1_profile(frozen_profile_root)
        frozen_v1_profile.write_bytes(frozen_v1_profile.read_bytes() + b"\n")
        try:
            verify_frozen_v1_profile(frozen_profile_root)
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test accepted a changed frozen v1 profile")
        frozen_v2_profile = frozen_profile_root / FROZEN_V2_PROFILE
        frozen_v2_profile.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(repository_root / FROZEN_V2_PROFILE, frozen_v2_profile)
        verify_frozen_v2_profile(frozen_profile_root)
        frozen_v2_profile.write_bytes(frozen_v2_profile.read_bytes() + b"\n")
        try:
            verify_frozen_v2_profile(frozen_profile_root)
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test accepted a changed frozen v2 profile")
        version = STABLE_VERSION
        revision = "1" * 40
        repository = "https://github.com/oxhq/stasis"
        source_root = root / "source"
        source_root.mkdir()
        for source_name in SOURCE_ASSETS.values():
            filename = source_root / source_name
            filename.parent.mkdir(parents=True, exist_ok=True)
            filename.write_text(f"fixture for {source_name}\n", encoding="utf-8")
        (source_root / SOURCE_ASSETS["STASIS_UPSTREAM.toml"]).write_text(
            "".join(
                f'{key} = {json.dumps(value, allow_nan=False)}\n'
                for key, value in UPSTREAM_IDENTITIES.items()
            ),
            encoding="utf-8",
        )
        binary = root / BINARY_NAME
        binary.write_bytes(b"#!/bin/sh\nexit 0\n")
        binary.chmod(0o755)

        release_cases: dict[
            str,
            tuple[Path, dict[str, str], Path, Path, dict[str, object], str],
        ] = {}
        for index, platform in enumerate(sorted(PLATFORM_CONTRACTS), start=1):
            dist = root / f"dist-{platform}"
            result = create_release(
                binary=binary,
                dist=dist,
                version=version,
                platform=platform,
                revision=revision,
                repository=repository,
                source_root=source_root,
            )
            extracted = root / f"extracted-{platform}"
            verified = verify_release(
                asset_directory=dist,
                version=version,
                platform=platform,
                revision=revision,
                repository=repository,
                source_root=source_root,
                extract_to=extracted,
            )
            if result["archiveSha256"] != verified["archiveSha256"]:
                raise ReleaseError(
                    f"self-test {platform} archive digest changed during verification"
                )

            names = release_asset_names(version, platform)
            archive = dist / names["archive"]
            bundle = bundle_name(version, platform)
            extracted_bundle = extracted / bundle
            actual_files = {entry.name for entry in extracted_bundle.iterdir()}
            if actual_files != EXPECTED_FILES:
                raise ReleaseError(f"self-test {platform} extracted inventory changed")
            generated = expected_generated_assets(version, platform, revision, repository)
            for name, expected in generated.items():
                if (extracted_bundle / name).read_bytes() != expected:
                    raise ReleaseError(f"self-test {platform} metadata changed: {name}")
            contract = platform_contract(platform)
            if contract["display_name"] not in generated["README.md"].decode("utf-8"):
                raise ReleaseError(f"self-test {platform} README lost its platform identity")
            if contract["dependency_note"] not in generated["NATIVE-LIBRARIES.txt"].decode(
                "utf-8"
            ):
                raise ReleaseError(
                    f"self-test {platform} native dependency metadata changed"
                )
            if contract["install_note"] not in generated["INSTALL.txt"].decode("utf-8"):
                raise ReleaseError(f"self-test {platform} install metadata changed")

            archive_digest = sha256_file(archive)
            binary_digest = parse_sidecar(
                dist / names["binary_sha256"],
                f"{bundle}/{BINARY_NAME}",
            )
            gate_record: dict[str, object] = {
                "schema": GATE_PROOF_SCHEMA,
                "gate": GATE_NAME,
                "test": GATE_TEST,
                "version": version,
                "archive": {"name": names["archive"], "sha256": archive_digest},
                "binary": {"path": "/tmp/stasis", "sha256": binary_digest},
                "source": expected_source_identities(revision),
            }
            log = root / f"gate-{platform}.log"
            log.write_text(
                f"{GATE_RECORD_PREFIX}"
                f"{json.dumps(gate_record, allow_nan=False, separators=(',', ':'), sort_keys=True)}\n"
                f"test {GATE_TEST} ... ok\n{GATE_SUCCESS} 0 measured; 0 filtered out\n",
                encoding="utf-8",
            )
            proof_dir = root / f"proof-{platform}"
            proof_dir.mkdir()
            proof = proof_dir / names["gate_proof"]
            run_id = str(122 + index)
            created_proof = create_gate_proof(
                proof=proof,
                gate_log=log,
                asset_directory=dist,
                version=version,
                platform=platform,
                revision=revision,
                run_id=run_id,
                run_attempt="1",
            )
            if (
                created_proof["schema"] != GATE_PROOF_SCHEMA
                or created_proof["platform"] != platform
            ):
                raise ReleaseError(f"self-test {platform} gate proof identity changed")
            verify_gate_proof(
                proof_directory=proof_dir,
                asset_directory=dist,
                version=version,
                platform=platform,
                revision=revision,
                run_id=run_id,
                run_attempt="1",
            )
            release_cases[platform] = (
                dist,
                names,
                archive,
                extracted_bundle,
                gate_record,
                binary_digest,
            )

        mac_generated = expected_generated_assets(
            version, "macos-aarch64", revision, repository
        )
        linux_generated = expected_generated_assets(
            version, "linux-x86_64", revision, repository
        )
        for name in ("INSTALL.txt", "NATIVE-LIBRARIES.txt", "README.md", "VERSION.txt"):
            if mac_generated[name] == linux_generated[name]:
                raise ReleaseError(f"self-test generated platform-neutral {name}")
        if b"glibc 2.35" not in linux_generated["NATIVE-LIBRARIES.txt"]:
            raise ReleaseError("self-test Linux metadata lost the glibc compatibility floor")
        if b"unsigned and is not Apple-notarized" not in mac_generated["INSTALL.txt"]:
            raise ReleaseError("self-test macOS metadata lost its signing boundary")

        linux_bundle = release_cases["linux-x86_64"][3]
        try:
            validate_bundle_directory(
                linux_bundle,
                version=version,
                platform="macos-aarch64",
                revision=revision,
                repository=repository,
                source_root=source_root,
            )
        except ReleaseError as error:
            if "generated release asset differs" not in str(error):
                raise
        else:
            raise ReleaseError("self-test accepted Linux metadata as macOS metadata")

        for invalid_version in ("0.1.0-alpha.0", "0.2.0-alpha.0", "0.2.1", "v0.2.0"):
            try:
                validate_identity(invalid_version, "macos-aarch64", revision, repository)
            except ReleaseError:
                pass
            else:
                raise ReleaseError(f"self-test accepted non-release version {invalid_version!r}")
        for invalid_platform in ("linux-aarch64", "macos-x86_64", "windows-x86_64"):
            try:
                validate_identity(version, invalid_platform, revision, repository)
            except ReleaseError:
                pass
            else:
                raise ReleaseError(f"self-test accepted unsupported platform {invalid_platform!r}")

        for invalid_json in (
            '{"outer":{"member":1,"member":2}}',
            '{"number":NaN}',
            '{"number":Infinity}',
            '{"number":-Infinity}',
        ):
            try:
                strict_json_loads(invalid_json, "self-test document")
            except ReleaseError:
                pass
            else:
                raise ReleaseError("self-test accepted duplicate-member or non-finite JSON")

        dist, names, archive, _, gate_record, binary_digest = release_cases["macos-aarch64"]
        archive_digest = sha256_file(archive)

        unrelated_record = dict(gate_record)
        unrelated_record["archive"] = {"name": names["archive"], "sha256": "2" * 64}
        unrelated_log = root / "unrelated-archive-gate.log"
        unrelated_log.write_text(
            f"{GATE_RECORD_PREFIX}"
            f"{json.dumps(unrelated_record, allow_nan=False, separators=(',', ':'), sort_keys=True)}\n"
            f"test {GATE_TEST} ... ok\n{GATE_SUCCESS} 0 measured; 0 filtered out\n",
            encoding="utf-8",
        )
        try:
            create_gate_proof(
                proof=root / "unrelated-archive-proof.json",
                gate_log=unrelated_log,
                asset_directory=dist,
                version=version,
                platform="macos-aarch64",
                revision=revision,
                run_id="123",
                run_attempt="1",
            )
        except ReleaseError as error:
            if "archive" not in str(error):
                raise
        else:
            raise ReleaseError("self-test attached an archive absent from the native gate record")

        overfull_tar = root / "overfull.tar"
        bundle = bundle_name(version, "macos-aarch64")
        with tarfile.open(overfull_tar, mode="w", format=tarfile.USTAR_FORMAT) as package:
            package.addfile(normalized_tar_info(bundle, directory=True))
            for name in sorted(EXPECTED_FILES):
                package.addfile(
                    normalized_tar_info(
                        f"{bundle}/{name}",
                        directory=False,
                        executable=name == BINARY_NAME,
                    )
                )
            package.addfile(normalized_tar_info(f"{bundle}/EXTRA", directory=False))
        with tarfile.open(overfull_tar, mode="r:") as package:
            try:
                verify_tar_metadata(package, bundle)
            except ReleaseError as error:
                expected_member_count = len(EXPECTED_FILES) + 1
                if f"exact {expected_member_count}-member limit" not in str(error):
                    raise
            else:
                raise ReleaseError("self-test accepted an archive beyond its member limit")

        empty_member_tar = root / "empty-member.tar"
        with tarfile.open(empty_member_tar, mode="w", format=tarfile.USTAR_FORMAT) as package:
            package.addfile(normalized_tar_info(bundle, directory=True))
            for name in sorted(EXPECTED_FILES):
                payload = b"" if name == "README.md" else b"x"
                member = normalized_tar_info(
                    f"{bundle}/{name}",
                    directory=False,
                    executable=name == BINARY_NAME,
                )
                member.size = len(payload)
                package.addfile(member, io.BytesIO(payload))
        with tarfile.open(empty_member_tar, mode="r:") as package:
            try:
                verify_tar_metadata(package, bundle)
            except ReleaseError as error:
                if "invalid size 0" not in str(error):
                    raise
            else:
                raise ReleaseError("self-test accepted an empty required archive member")

        sidecar = dist / names["archive_sha256"]
        try:
            decompress_canonical_gzip(
                archive,
                root / "deliberately-too-small.tar",
                max_output_bytes=16,
            )
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test decompressed an archive beyond its configured limit")
        canonical_archive = archive.read_bytes()
        archive.write_bytes(canonical_archive + b"UNVERIFIED-TRAILING-PAYLOAD")
        write_sidecar(sidecar, sha256_file(archive), names["archive"])
        try:
            verify_release(
                asset_directory=dist,
                version=version,
                platform="macos-aarch64",
                revision=revision,
                repository=repository,
                source_root=source_root,
                extract_to=None,
            )
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test accepted a validly checksummed archive with trailing bytes")
        archive.write_bytes(canonical_archive)
        write_sidecar(sidecar, sha256_file(archive), names["archive"])
        sidecar.write_text(sidecar.read_text(encoding="ascii").upper(), encoding="ascii")
        try:
            verify_release(
                asset_directory=dist,
                version=version,
                platform="macos-aarch64",
                revision=revision,
                repository=repository,
                source_root=source_root,
                extract_to=None,
            )
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test accepted a noncanonical checksum")
    print("stasis release archive self-test: ok")


def parser() -> argparse.ArgumentParser:
    argument_parser = argparse.ArgumentParser(description=__doc__)
    subparsers = argument_parser.add_subparsers(dest="command", required=True)

    create = subparsers.add_parser("create")
    create.add_argument("--binary", type=Path, required=True)
    create.add_argument("--dist", type=Path, required=True)
    create.add_argument("--version", required=True)
    create.add_argument("--platform", required=True)
    create.add_argument("--revision", required=True)
    create.add_argument("--repository", required=True)
    create.add_argument("--source-root", type=Path, required=True)

    verify = subparsers.add_parser("verify")
    verify.add_argument("--asset-directory", type=Path, required=True)
    verify.add_argument("--version", required=True)
    verify.add_argument("--platform", required=True)
    verify.add_argument("--revision", required=True)
    verify.add_argument("--repository", required=True)
    verify.add_argument("--source-root", type=Path, required=True)
    verify.add_argument("--extract-to", type=Path)

    gate_proof = subparsers.add_parser("gate-proof")
    gate_proof.add_argument("--proof", type=Path, required=True)
    gate_proof.add_argument("--gate-log", type=Path, required=True)
    gate_proof.add_argument("--asset-directory", type=Path, required=True)
    gate_proof.add_argument("--version", required=True)
    gate_proof.add_argument("--platform", required=True)
    gate_proof.add_argument("--revision", required=True)
    gate_proof.add_argument("--run-id", required=True)
    gate_proof.add_argument("--run-attempt", required=True)

    verify_proof = subparsers.add_parser("verify-gate-proof")
    verify_proof.add_argument("--proof-directory", type=Path, required=True)
    verify_proof.add_argument("--asset-directory", type=Path, required=True)
    verify_proof.add_argument("--version", required=True)
    verify_proof.add_argument("--platform", required=True)
    verify_proof.add_argument("--revision", required=True)
    verify_proof.add_argument("--run-id", required=True)
    verify_proof.add_argument("--run-attempt", required=True)

    verify_frozen_profile = subparsers.add_parser("verify-frozen-v1-profile")
    verify_frozen_profile.add_argument("--source-root", type=Path, required=True)

    verify_frozen_v2 = subparsers.add_parser("verify-frozen-v2-profile")
    verify_frozen_v2.add_argument("--source-root", type=Path, required=True)

    subparsers.add_parser("self-test")
    return argument_parser


def main() -> None:
    arguments = parser().parse_args()
    if arguments.command == "create":
        result = create_release(
            binary=arguments.binary,
            dist=arguments.dist,
            version=arguments.version,
            platform=arguments.platform,
            revision=arguments.revision,
            repository=arguments.repository,
            source_root=arguments.source_root,
        )
    elif arguments.command == "verify":
        result = verify_release(
            asset_directory=arguments.asset_directory,
            version=arguments.version,
            platform=arguments.platform,
            revision=arguments.revision,
            repository=arguments.repository,
            source_root=arguments.source_root,
            extract_to=arguments.extract_to,
        )
    elif arguments.command == "gate-proof":
        result = create_gate_proof(
            proof=arguments.proof,
            gate_log=arguments.gate_log,
            asset_directory=arguments.asset_directory,
            version=arguments.version,
            platform=arguments.platform,
            revision=arguments.revision,
            run_id=arguments.run_id,
            run_attempt=arguments.run_attempt,
        )
    elif arguments.command == "verify-gate-proof":
        result = verify_gate_proof(
            proof_directory=arguments.proof_directory,
            asset_directory=arguments.asset_directory,
            version=arguments.version,
            platform=arguments.platform,
            revision=arguments.revision,
            run_id=arguments.run_id,
            run_attempt=arguments.run_attempt,
        )
    elif arguments.command == "verify-frozen-v1-profile":
        result = verify_frozen_v1_profile(arguments.source_root)
    elif arguments.command == "verify-frozen-v2-profile":
        result = verify_frozen_v2_profile(arguments.source_root)
    else:
        self_test()
        return
    print(json.dumps(result, allow_nan=False, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except ReleaseError as error:
        raise SystemExit(f"error: {error}") from error
