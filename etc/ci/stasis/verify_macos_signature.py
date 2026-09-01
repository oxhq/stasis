#!/usr/bin/env python3
"""Verify Stasis's exact macOS ad hoc linker-signature release boundary."""

from __future__ import annotations

import argparse
import json
import platform
import struct
import subprocess
import sys
from dataclasses import asdict, dataclass
from pathlib import Path


MH_MAGIC_64 = 0xFEEDFACF
MH_EXECUTE = 0x2
CPU_TYPE_ARM64 = 0x0100000C
LC_CODE_SIGNATURE = 0x1D

CSMAGIC_EMBEDDED_SIGNATURE = 0xFADE0CC0
CSMAGIC_CODEDIRECTORY = 0xFADE0C02
CSMAGIC_BLOBWRAPPER = 0xFADE0B01
CSSLOT_CODEDIRECTORY = 0
CSSLOT_SIGNATURESLOT = 0x10000

CS_ADHOC = 0x2
CS_RUNTIME = 0x10000
CS_LINKER_SIGNED = 0x20000
EXPECTED_CODE_DIRECTORY_FLAGS = CS_ADHOC | CS_LINKER_SIGNED

CODE_DIRECTORY_SUPPORTS_TEAM_ID = 0x20200
EXPECTED_CODE_DIRECTORY_VERSION = 0x20400
EXPECTED_CODE_DIRECTORY_HEADER_SIZE = 88
CODE_DIRECTORY_TEAM_OFFSET = 48


class SignatureBoundaryError(ValueError):
    """The binary does not satisfy the declared macOS release boundary."""


@dataclass(frozen=True)
class SignatureBoundary:
    code_directory_flags: str
    code_directory_version: str
    code_limit: int
    identifier: str
    signature_data_offset: int
    signature_data_size: int
    superblob_slots: tuple[int, ...]
    team_offset: int


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SignatureBoundaryError(message)


def unpack_from(fmt: str, data: bytes, offset: int, label: str) -> tuple[int, ...]:
    size = struct.calcsize(fmt)
    require(offset >= 0 and offset + size <= len(data), f"truncated {label}")
    return struct.unpack_from(fmt, data, offset)


def parse_code_directory(
    blob: bytes,
    *,
    signature_data_offset: int,
) -> tuple[int, int, int, int, str]:
    magic, length, version, flags = unpack_from(">IIII", blob, 0, "CodeDirectory header")
    require(magic == CSMAGIC_CODEDIRECTORY, "code-signature slot is not a CodeDirectory")
    require(length == len(blob), "CodeDirectory length does not match its slot")
    require(version >= CODE_DIRECTORY_SUPPORTS_TEAM_ID, "CodeDirectory cannot carry an explicit team offset")
    require(version == EXPECTED_CODE_DIRECTORY_VERSION, "CodeDirectory version changed from the qualified boundary")
    require(length >= EXPECTED_CODE_DIRECTORY_HEADER_SIZE, "CodeDirectory omits qualified v0x20400 fields")

    (
        _magic,
        _length,
        _version,
        _flags,
        hash_offset,
        identifier_offset,
        special_slot_count,
        code_slot_count,
        code_limit,
    ) = unpack_from(">IIIIIIIII", blob, 0, "CodeDirectory fixed fields")
    hash_size, hash_type, code_platform, page_size = unpack_from(">BBBB", blob, 36, "CodeDirectory hash fields")
    (team_offset,) = unpack_from(">I", blob, CODE_DIRECTORY_TEAM_OFFSET, "CodeDirectory team offset")
    (code_limit_64,) = unpack_from(">Q", blob, 56, "CodeDirectory 64-bit code limit")

    require(flags == EXPECTED_CODE_DIRECTORY_FLAGS, "CodeDirectory flags are not exactly CS_ADHOC | CS_LINKER_SIGNED")
    require(flags & CS_RUNTIME == 0, "CodeDirectory unexpectedly enables the hardened runtime flag")
    require(team_offset == 0, "ad hoc CodeDirectory unexpectedly names a team")
    require(code_limit == signature_data_offset, "CodeDirectory code limit does not meet LC_CODE_SIGNATURE")
    require(code_limit_64 == 0, "CodeDirectory unexpectedly uses a 64-bit code limit")
    require(special_slot_count == 0, "CodeDirectory unexpectedly contains special hash slots")
    require(code_slot_count > 0, "CodeDirectory has no executable code slots")
    require(hash_size == 32 and hash_type == 2, "CodeDirectory is not SHA-256-only")
    require(code_platform == 0, "CodeDirectory unexpectedly names a platform byte")
    require(page_size == 12, "CodeDirectory page-size exponent is not 4 KiB")
    require(identifier_offset == EXPECTED_CODE_DIRECTORY_HEADER_SIZE, "CodeDirectory identifier offset changed")
    require(identifier_offset < length, "CodeDirectory identifier offset is out of range")
    identifier_end = blob.find(b"\0", identifier_offset)
    require(identifier_end > identifier_offset, "CodeDirectory identifier is empty or unterminated")
    try:
        identifier = blob[identifier_offset:identifier_end].decode("ascii")
    except UnicodeDecodeError as error:
        raise SignatureBoundaryError("CodeDirectory identifier is not ASCII") from error
    require(
        all(0x21 <= ord(character) <= 0x7E for character in identifier), "CodeDirectory identifier is not printable"
    )

    require(hash_offset >= identifier_end + 1, "CodeDirectory hashes overlap its identifier")
    require(
        all(byte == 0 for byte in blob[identifier_end + 1 : hash_offset]), "CodeDirectory identifier padding is nonzero"
    )
    require(
        hash_offset + code_slot_count * hash_size == length,
        "CodeDirectory code-slot hashes do not exactly close the slot",
    )
    return version, flags, code_limit, team_offset, identifier


def parse_signature_superblob(
    blob: bytes,
    *,
    signature_data_offset: int,
) -> tuple[tuple[int, ...], tuple[int, int, int, int, str]]:
    magic, declared_length, count = unpack_from(">III", blob, 0, "embedded-signature header")
    require(magic == CSMAGIC_EMBEDDED_SIGNATURE, "LC_CODE_SIGNATURE is not an embedded-signature SuperBlob")
    require(12 <= declared_length <= len(blob), "embedded-signature length is invalid")
    require(all(byte == 0 for byte in blob[declared_length:]), "LC_CODE_SIGNATURE has nonzero trailing bytes")

    index_end = 12 + count * 8
    require(count > 0 and index_end <= declared_length, "embedded-signature index is invalid")
    indexed: list[tuple[int, int, int]] = []
    seen_slots: set[int] = set()
    for index in range(count):
        slot, offset = unpack_from(">II", blob, 12 + index * 8, f"embedded-signature index {index}")
        require(slot not in seen_slots, "embedded-signature contains a duplicate slot")
        seen_slots.add(slot)
        require(offset >= index_end and offset + 8 <= declared_length, "embedded-signature slot offset is invalid")
        nested_magic, nested_length = unpack_from(">II", blob, offset, f"embedded-signature slot {slot}")
        require(
            nested_length >= 8 and offset + nested_length <= declared_length,
            "embedded-signature slot length is invalid",
        )
        indexed.append((slot, offset, nested_length))
        if slot == CSSLOT_SIGNATURESLOT or nested_magic == CSMAGIC_BLOBWRAPPER:
            raise SignatureBoundaryError("embedded signature unexpectedly contains a CMS signature")

    require(seen_slots == {CSSLOT_CODEDIRECTORY}, "embedded signature is not a lone CodeDirectory slot")
    ordered_ranges = sorted((offset, offset + length) for _slot, offset, length in indexed)
    for previous, current in zip(ordered_ranges, ordered_ranges[1:]):
        require(previous[1] <= current[0], "embedded-signature slots overlap")

    _slot, code_directory_offset, code_directory_length = indexed[0]
    require(
        all(byte == 0 for byte in blob[index_end:code_directory_offset]),
        "embedded signature has nonzero padding before its CodeDirectory",
    )
    code_directory_end = code_directory_offset + code_directory_length
    require(
        all(byte == 0 for byte in blob[code_directory_end:declared_length]),
        "embedded signature contains undeclared nonzero payload",
    )
    code_directory = blob[code_directory_offset:code_directory_end]
    return tuple(sorted(seen_slots)), parse_code_directory(
        code_directory,
        signature_data_offset=signature_data_offset,
    )


def inspect_binary_bytes(data: bytes) -> SignatureBoundary:
    require(len(data) >= 32, "binary is shorter than a Mach-O 64-bit header")
    magic, cpu_type, _cpu_subtype, file_type, command_count, command_bytes, _flags, _reserved = unpack_from(
        "<IiiIIIII", data, 0, "Mach-O header"
    )
    require(magic == MH_MAGIC_64, "binary is not a thin little-endian Mach-O 64-bit file")
    require(cpu_type == CPU_TYPE_ARM64, "Mach-O binary is not arm64")
    require(file_type == MH_EXECUTE, "Mach-O file is not an executable")
    require(command_count > 0, "Mach-O binary has no load commands")

    command_offset = 32
    command_end = command_offset + command_bytes
    require(command_end <= len(data), "Mach-O load-command region is truncated")
    signatures: list[tuple[int, int]] = []
    for index in range(command_count):
        command, command_size = unpack_from("<II", data, command_offset, f"Mach-O load command {index}")
        require(command_size >= 8 and command_size % 4 == 0, "Mach-O load-command size is invalid")
        require(command_offset + command_size <= command_end, "Mach-O load command exceeds the declared region")
        if command == LC_CODE_SIGNATURE:
            require(command_size == 16, "LC_CODE_SIGNATURE has an unexpected size")
            _command, _size, data_offset, data_size = unpack_from("<IIII", data, command_offset, "LC_CODE_SIGNATURE")
            signatures.append((data_offset, data_size))
        command_offset += command_size
    require(command_offset == command_end, "Mach-O load-command count and byte size disagree")
    require(len(signatures) == 1, "Mach-O must contain exactly one LC_CODE_SIGNATURE")

    signature_data_offset, signature_data_size = signatures[0]
    require(signature_data_offset >= command_end, "LC_CODE_SIGNATURE overlaps Mach-O load commands")
    require(signature_data_size > 0, "LC_CODE_SIGNATURE is empty")
    require(
        signature_data_offset + signature_data_size == len(data),
        "LC_CODE_SIGNATURE is not the final Mach-O payload",
    )
    signature_blob = data[signature_data_offset:]
    slots, parsed = parse_signature_superblob(
        signature_blob,
        signature_data_offset=signature_data_offset,
    )
    version, flags, code_limit, team_offset, identifier = parsed
    return SignatureBoundary(
        code_directory_flags=f"0x{flags:08x}",
        code_directory_version=f"0x{version:08x}",
        code_limit=code_limit,
        identifier=identifier,
        signature_data_offset=signature_data_offset,
        signature_data_size=signature_data_size,
        superblob_slots=slots,
        team_offset=team_offset,
    )


def inspect_binary(path: Path) -> SignatureBoundary:
    require(not path.is_symlink(), "macOS release binary must not be a symlink")
    require(path.is_file(), "macOS release binary is not a regular file")
    return inspect_binary_bytes(path.read_bytes())


def validate_codesign_display(output: str) -> None:
    lines = output.replace("\r\n", "\n").splitlines()
    require(lines.count("Signature=adhoc") == 1, "codesign did not report exactly one ad hoc signature")
    require(lines.count("TeamIdentifier=not set") == 1, "codesign did not report an absent team identifier")
    require(
        sum(line.startswith("CodeDirectory ") and "flags=0x20002(adhoc,linker-signed)" in line for line in lines) == 1,
        "codesign did not report the exact ad hoc linker-signature flags",
    )
    require(
        not any(line.startswith(("Authority=", "Timestamp=")) for line in lines),
        "codesign unexpectedly reported an identity or timestamp",
    )


def run_codesign(path: Path) -> None:
    display = subprocess.run(
        ["codesign", "--display", "--verbose=4", str(path)],
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="strict",
    )
    require(display.returncode == 0, f"codesign display failed with status {display.returncode}")
    require(not display.stdout, "codesign display unexpectedly wrote to stdout")
    validate_codesign_display(display.stderr)

    verification = subprocess.run(
        ["codesign", "--verify", "--strict", "--verbose=4", str(path)],
        check=False,
        capture_output=True,
        text=True,
        encoding="utf-8",
        errors="strict",
    )
    require(verification.returncode == 0, f"codesign verification failed with status {verification.returncode}")
    require(not verification.stdout, "codesign verification unexpectedly wrote to stdout")


def make_test_binary(
    *,
    flags: int = EXPECTED_CODE_DIRECTORY_FLAGS,
    team_offset: int = 0,
    code_limit_delta: int = 0,
    include_cms: bool = False,
) -> bytes:
    identifier = b"stasis-self-test\0"
    code_directory_header_size = 88
    hash_size = 32
    code_directory_length = code_directory_header_size + len(identifier) + hash_size
    code_directory = bytearray(code_directory_length)
    data_offset = 4096
    struct.pack_into(
        ">IIIIIIIII",
        code_directory,
        0,
        CSMAGIC_CODEDIRECTORY,
        code_directory_length,
        0x20400,
        flags,
        code_directory_header_size + len(identifier),
        code_directory_header_size,
        0,
        1,
        data_offset + code_limit_delta,
    )
    struct.pack_into(">BBBB", code_directory, 36, hash_size, 2, 0, 12)
    struct.pack_into(">I", code_directory, 40, 0)
    struct.pack_into(">I", code_directory, 44, 0)
    struct.pack_into(">I", code_directory, CODE_DIRECTORY_TEAM_OFFSET, team_offset)
    code_directory[code_directory_header_size : code_directory_header_size + len(identifier)] = identifier

    slots: list[tuple[int, bytes]] = [(CSSLOT_CODEDIRECTORY, bytes(code_directory))]
    if include_cms:
        slots.append((CSSLOT_SIGNATURESLOT, struct.pack(">II", CSMAGIC_BLOBWRAPPER, 8)))
    index_end = 12 + len(slots) * 8
    first_slot_offset = (index_end + 7) & ~7
    offsets: list[tuple[int, int]] = []
    cursor = first_slot_offset
    for slot, nested in slots:
        offsets.append((slot, cursor))
        cursor += len(nested)
    superblob = bytearray(cursor)
    struct.pack_into(">III", superblob, 0, CSMAGIC_EMBEDDED_SIGNATURE, len(superblob), len(slots))
    for index, ((slot, nested), (_indexed_slot, offset)) in enumerate(zip(slots, offsets)):
        struct.pack_into(">II", superblob, 12 + index * 8, slot, offset)
        superblob[offset : offset + len(nested)] = nested

    header = struct.pack(
        "<IiiIIIII",
        MH_MAGIC_64,
        CPU_TYPE_ARM64,
        0,
        MH_EXECUTE,
        1,
        16,
        0,
        0,
    )
    command = struct.pack("<IIII", LC_CODE_SIGNATURE, 16, data_offset, len(superblob))
    return header + command + bytes(data_offset - len(header) - len(command)) + bytes(superblob)


def expect_rejection(data: bytes, fragment: str) -> None:
    try:
        inspect_binary_bytes(data)
    except SignatureBoundaryError as error:
        require(fragment in str(error), f"self-test rejection changed: {error}")
    else:
        raise SignatureBoundaryError(f"self-test accepted invalid binary expecting {fragment!r}")


def self_test() -> None:
    valid = make_test_binary()
    boundary = inspect_binary_bytes(valid)
    require(boundary.code_directory_flags == "0x00020002", "self-test flags changed")
    require(boundary.code_limit == boundary.signature_data_offset == 4096, "self-test code limit changed")
    require(boundary.identifier == "stasis-self-test", "self-test identifier changed")
    require(boundary.superblob_slots == (CSSLOT_CODEDIRECTORY,), "self-test slot inventory changed")
    require(boundary.team_offset == 0, "self-test team offset changed")

    expect_rejection(make_test_binary(flags=CS_LINKER_SIGNED), "flags")
    expect_rejection(make_test_binary(flags=EXPECTED_CODE_DIRECTORY_FLAGS | CS_RUNTIME), "flags")
    expect_rejection(make_test_binary(team_offset=1), "team")
    expect_rejection(make_test_binary(code_limit_delta=1), "code limit")
    expect_rejection(make_test_binary(include_cms=True), "CMS")
    expect_rejection(valid[:-1], "final Mach-O payload")

    valid_display = "\n".join(
        (
            "Executable=/tmp/stasis",
            "CodeDirectory v=20400 size=123 flags=0x20002(adhoc,linker-signed) hashes=1+0 location=embedded",
            "Signature=adhoc",
            "TeamIdentifier=not set",
        )
    )
    validate_codesign_display(valid_display)
    for invalid_display, fragment in (
        (valid_display.replace("Signature=adhoc\n", ""), "ad hoc signature"),
        (valid_display.replace("TeamIdentifier=not set", ""), "team identifier"),
        (valid_display + "\nAuthority=Developer ID Application: example", "identity or timestamp"),
    ):
        try:
            validate_codesign_display(invalid_display)
        except SignatureBoundaryError as error:
            require(fragment in str(error), f"self-test codesign rejection changed: {error}")
        else:
            raise SignatureBoundaryError("self-test accepted invalid codesign output")
    print("stasis macOS signature verifier self-test: ok")


def verify(path: Path) -> None:
    require(sys.platform == "darwin", "macOS signature verification must run on Darwin")
    require(platform.machine() == "arm64", "macOS signature verification must run on native arm64")
    boundary = inspect_binary(path)
    run_codesign(path)
    print(json.dumps({"schema": 1, "status": "verified", **asdict(boundary)}, sort_keys=True))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)
    subparsers.add_parser("self-test", help="run deterministic parser and policy regressions")
    verify_parser = subparsers.add_parser("verify", help="verify one native macOS arm64 executable")
    verify_parser.add_argument("--binary", required=True, type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        if args.command == "self-test":
            self_test()
        elif args.command == "verify":
            verify(args.binary)
        else:
            raise SignatureBoundaryError(f"unknown command {args.command!r}")
    except (OSError, SignatureBoundaryError, struct.error) as error:
        print(f"macOS signature verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
