#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import struct
import sys
import tempfile
import unittest
import uuid


MODULE_PATH = Path(__file__).with_name("symbolize_v032_macos_stacks.py")
SPEC = importlib.util.spec_from_file_location("symbolize_v032_macos_stacks", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
symbolize = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = symbolize
SPEC.loader.exec_module(symbolize)


def synthetic_report(*, bad_absolute: bool = False) -> str:
    load = 0x100000000
    lines = [
        "Analysis of sampling stasis (pid 1) every 1 millisecond",
        "Process:         stasis [1]",
        "Path:            /tmp/stasis",
        f"Load Address:    0x{load:x}",
        "Identifier:      stasis",
        "Version:         0",
        "Code Type:       ARM64",
        "Call graph:",
        "    1 Thread_1 DispatchQueue_1: com.apple.main-thread  (serial)",
    ]
    for index, offset in enumerate(symbolize.MAIN_STABLE_OFFSETS):
        absolute = load + offset + (1 if bad_absolute and index == 0 else 0)
        lines.append(f"    + 1 ???  (in stasis) load address 0x{load:x} + 0x{offset:x}  [0x{absolute:x}]")
    lines.append("    1 Thread_2: Script#1")
    for offset in symbolize.SCRIPT_STABLE_OFFSETS:
        lines.append(f"    + 1 ???  (in stasis) load address 0x{load:x} + 0x{offset:x}  [0x{load + offset:x}]")
    lines.extend(
        [
            "Total number in stack (recursive counted multiple, when >=5):",
            "Binary Images:",
            f"       0x100000000 -        0x1089bc697 +stasis (0) <{symbolize.RELEASE_UUID}> /tmp/stasis",
        ]
    )
    return "\n".join(lines) + "\n"


def synthetic_capture_result(root: Path) -> tuple[Path, Path, Path, Path, dict[str, object]]:
    profile = root / "target" / "production-stripped"
    deps = profile / "deps"
    deps.mkdir(parents=True)
    actual = deps / "stasis-0123456789abcdef"
    poststrip_capture = root / "stasis.poststrip-at-invocation"
    built = profile / "stasis"
    release = root / "release" / "stasis"
    release.parent.mkdir()
    poststrip = b"post-strip executable"
    for path in (actual, poststrip_capture, built):
        path.write_bytes(poststrip)
    release.write_bytes(b"immutable release with expected whole-file differences")
    prestrip = root / "stasis.prestrip"
    prestrip.write_bytes(b"pre-strip executable with local symbols")
    root = symbolize.canonical(root)
    actual = symbolize.canonical(actual)
    poststrip_capture = symbolize.canonical(poststrip_capture)
    built = symbolize.canonical(built)
    release = symbolize.canonical(release)
    prestrip = symbolize.canonical(prestrip)
    result: dict[str, object] = {
        "schema": symbolize.CAPTURE_SCHEMA,
        "expectedStripDirectory": os.fspath(symbolize.canonical(deps)),
        "expectedCargoOutput": os.fspath(built),
        "hashedTargetPattern": symbolize.HASHED_TARGET_PATTERN.pattern,
        "hashedTargetCount": 1,
        "hashedTargets": [os.fspath(actual)],
        "actualStripTarget": os.fspath(actual),
        "capture": os.fspath(prestrip),
        "captureBytes": prestrip.stat().st_size,
        "captureSha256": symbolize.sha256(prestrip),
        "poststripCapture": os.fspath(poststrip_capture),
        "poststripCaptureBytes": poststrip_capture.stat().st_size,
        "poststripCaptureSha256": symbolize.sha256(poststrip_capture),
        "rustObjcopyInvocation": {
            "workingDirectory": os.fspath(root),
            "argv": ["--strip-all", os.fspath(actual)],
            "canonicalTarget": os.fspath(actual),
        },
        "singleTargetInvocation": True,
        "poststripHashedTargetBytes": actual.stat().st_size,
        "poststripHashedTargetSha256": symbolize.sha256(actual),
        "cargoOutputBytes": built.stat().st_size,
        "cargoOutputSha256": symbolize.sha256(built),
        "immutableReleaseBytes": release.stat().st_size,
        "immutableReleaseSha256": symbolize.sha256(release),
        "prestripDiffersFromPoststrip": True,
        "poststripCaptureMatchesHashedTarget": True,
        "poststripHashedTargetMatchesCargoOutput": True,
        "rebuiltPoststripWholeFileMatchesImmutableRelease": False,
        "originalObjcopySha256": "a" * 64,
        "restoredObjcopySha256": "a" * 64,
        "wrapperSha256": "b" * 64,
        "releaseGateAuthority": False,
    }
    return prestrip, poststrip_capture, built, release, result


def encode_uleb(value: int) -> bytes:
    result = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            byte |= 0x80
        result.append(byte)
        if not value:
            return bytes(result)


def synthetic_macho(
    path: Path,
    *,
    uuid_value: str,
    text_bytes: bytes = b"0123456789abcdef",
    text_delta: int = 0x200,
) -> Path:
    if len(text_bytes) != 16:
        raise ValueError("synthetic __text must contain exactly 16 bytes")
    image_base = 0x100000000
    text_offset = 0x200
    function_data = encode_uleb(text_delta) + encode_uleb(8) + b"\0"
    function_data += b"\0" * (8 - len(function_data))
    function_offset = text_offset + len(text_bytes)
    segment = struct.pack(
        "<II16sQQQQiiII",
        symbolize.LC_SEGMENT_64,
        152,
        b"__TEXT\0".ljust(16, b"\0"),
        image_base,
        0x1000,
        0,
        function_offset + len(function_data),
        5,
        5,
        1,
        0,
    )
    section = struct.pack(
        "<16s16sQQ8I",
        b"__text\0".ljust(16, b"\0"),
        b"__TEXT\0".ljust(16, b"\0"),
        image_base + text_delta,
        len(text_bytes),
        text_offset,
        2,
        0,
        0,
        0x80000400,
        0,
        0,
        0,
    )
    uuid_command = struct.pack("<II16s", symbolize.LC_UUID, 24, uuid.UUID(uuid_value).bytes)
    function_command = struct.pack(
        "<4I",
        symbolize.LC_FUNCTION_STARTS,
        16,
        function_offset,
        len(function_data),
    )
    commands = segment + section + uuid_command + function_command
    header = struct.pack(
        "<8I",
        symbolize.MACHO_MAGIC_64,
        symbolize.CPU_TYPE_ARM64,
        0,
        symbolize.MH_EXECUTE,
        3,
        len(commands),
        0,
        0,
    )
    content = header + commands
    content += b"\0" * (text_offset - len(content))
    content += text_bytes + function_data
    path.write_bytes(content)
    return path


class SymbolizeV032MacosStacksTests(unittest.TestCase):
    def test_parses_all_stable_frames_and_validates_arithmetic(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sample.txt"
            path.write_text(synthetic_report(), encoding="utf-8")
            parsed = symbolize.parse_sample_report(path, 1)
            self.assertEqual(parsed.load_address, 0x100000000)
            self.assertEqual(
                len(parsed.frames),
                len(symbolize.MAIN_STABLE_OFFSETS) + len(symbolize.SCRIPT_STABLE_OFFSETS),
            )

    def test_rejects_explicit_address_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sample.txt"
            path.write_text(synthetic_report(bad_absolute=True), encoding="utf-8")
            with self.assertRaises(SystemExit):
                symbolize.parse_sample_report(path, 1)

    def test_nm_resolution_is_relative_to_text_vmaddr(self) -> None:
        symbols = [
            (0x100000100, "t", "first"),
            (0x100000200, "t", "second"),
        ]
        self.assertEqual(
            symbolize.nm_resolution(0x220, symbols, 0x100000000),
            "second + 0x20",
        )

    def test_text_vmaddr_selects_text_segment(self) -> None:
        output = """Load command 0
      segname __PAGEZERO
       vmaddr 0x0
Load command 1
      segname __TEXT
       vmaddr 0x100000000
"""
        self.assertEqual(symbolize.text_vmaddr(output), 0x100000000)

    def test_parser_rejects_unknown_stasis_frame_shape(self) -> None:
        report = synthetic_report().replace(
            "(in stasis) load address 0x100000000 + 0x7c064  [0x10007c064]",
            "(in stasis) without an address",
            1,
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sample.txt"
            path.write_text(report, encoding="utf-8")
            with self.assertRaises(SystemExit):
                symbolize.parse_sample_report(path, 1)

    def test_capture_result_binds_the_one_hashed_target_and_poststrip_bytes(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            prestrip, poststrip, built, release, result = synthetic_capture_result(Path(directory).resolve())
            validated = symbolize.validate_capture_result(result, prestrip, poststrip, built, release)
            self.assertEqual(validated["actualStripTarget"], result["actualStripTarget"])
            self.assertFalse(validated["rebuiltPoststripWholeFileMatchesImmutableRelease"])

    def test_capture_result_rejects_a_second_hashed_target(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            prestrip, poststrip, built, release, result = synthetic_capture_result(root)
            (root / "target" / "production-stripped" / "deps" / "stasis-fedcba9876543210").write_bytes(
                b"post-strip executable"
            )
            with self.assertRaises(SystemExit):
                symbolize.validate_capture_result(result, prestrip, poststrip, built, release)

    def test_capture_result_rejects_the_old_top_level_cargo_target(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            prestrip, poststrip, built, release, result = synthetic_capture_result(Path(directory).resolve())
            result["actualStripTarget"] = os.fspath(built)
            with self.assertRaises(SystemExit):
                symbolize.validate_capture_result(result, prestrip, poststrip, built, release)

    def test_capture_result_rejects_poststrip_bytes_not_matching_cargo(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            prestrip, poststrip, built, release, result = synthetic_capture_result(Path(directory).resolve())
            actual = Path(str(result["actualStripTarget"]))
            actual.write_bytes(b"different post-strip executable")
            with self.assertRaises(SystemExit):
                symbolize.validate_capture_result(result, prestrip, poststrip, built, release)

    def test_macho_parser_binds_uuid_text_and_function_starts(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = synthetic_macho(
                Path(directory) / "stasis",
                uuid_value=symbolize.RELEASE_UUID,
            )
            identity = symbolize.parse_macho_identity(path)
            self.assertEqual(identity.uuid, symbolize.RELEASE_UUID)
            self.assertEqual(identity.text.address, 0x100000200)
            self.assertEqual(identity.function_starts, (0x100000200, 0x100000208))

    def test_same_uuid_with_altered_text_bytes_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            left = symbolize.parse_macho_identity(synthetic_macho(root / "left", uuid_value=symbolize.RELEASE_UUID))
            right = symbolize.parse_macho_identity(
                synthetic_macho(
                    root / "right",
                    uuid_value=symbolize.RELEASE_UUID,
                    text_bytes=b"0123456789abcdeX",
                )
            )
            with self.assertRaises(SystemExit):
                symbolize.require_same_link_code(left, right)

    def test_same_uuid_with_altered_text_layout_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            left = symbolize.parse_macho_identity(synthetic_macho(root / "left", uuid_value=symbolize.RELEASE_UUID))
            right = symbolize.parse_macho_identity(
                synthetic_macho(
                    root / "right",
                    uuid_value=symbolize.RELEASE_UUID,
                    text_delta=0x204,
                )
            )
            with self.assertRaises(SystemExit):
                symbolize.require_same_link_code(left, right)

    def test_release_symbol_equivalence_accepts_expected_whole_file_differences(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rebuilt_text = struct.pack("<4I", 0x9001A560, 0x91025800, 0xD65F03C0, 0xD503201F)
            release_text = struct.pack("<4I", 0xF001A540, 0x913E5800, 0xD65F03C0, 0xD503201F)
            rebuilt_path = synthetic_macho(
                root / "rebuilt",
                uuid_value="04B66398-4FDD-39C3-BFD8-7646E34040E9",
                text_bytes=rebuilt_text,
            )
            release_path = synthetic_macho(
                root / "release",
                uuid_value=symbolize.RELEASE_UUID,
                text_bytes=release_text,
            )
            rebuilt = symbolize.parse_macho_identity(rebuilt_path)
            release = symbolize.parse_macho_identity(release_path)
            anchors = symbolize.release_symbol_layout_anchors(rebuilt, release, (0x204,))
            self.assertEqual(anchors[0]["functionStart"], "0x100000200")
            code = symbolize.release_target_code_equivalence(
                rebuilt_path,
                release_path,
                rebuilt,
                release,
                anchors,
            )
            self.assertFalse(code[0]["rawBytesEqual"])
            self.assertEqual(code[0]["allowedAddressImmediateDifferenceWords"], 2)

    def test_release_symbol_equivalence_rejects_changed_function_layout(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rebuilt = symbolize.parse_macho_identity(
                synthetic_macho(root / "rebuilt", uuid_value="04B66398-4FDD-39C3-BFD8-7646E34040E9")
            )
            release = symbolize.parse_macho_identity(
                synthetic_macho(root / "release", uuid_value=symbolize.RELEASE_UUID, text_delta=0x204)
            )
            with self.assertRaises(SystemExit):
                symbolize.release_symbol_layout_anchors(rebuilt, release, (0x204,))

    def test_same_uuid_with_altered_target_opcode_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rebuilt_path = synthetic_macho(
                root / "rebuilt",
                uuid_value=symbolize.RELEASE_UUID,
                text_bytes=struct.pack("<4I", 0x9001A560, 0x910EA821, 0xD65F03C0, 0xD503201F),
            )
            release_path = synthetic_macho(
                root / "release",
                uuid_value=symbolize.RELEASE_UUID,
                text_bytes=struct.pack("<4I", 0x9001A560, 0xD503201F, 0xD65F03C0, 0xD503201F),
            )
            rebuilt = symbolize.parse_macho_identity(rebuilt_path)
            release = symbolize.parse_macho_identity(release_path)
            anchors = symbolize.release_symbol_layout_anchors(rebuilt, release, (0x204,))
            with self.assertRaises(SystemExit):
                symbolize.release_target_code_equivalence(
                    rebuilt_path,
                    release_path,
                    rebuilt,
                    release,
                    anchors,
                )

    def test_altered_standalone_add_immediate_is_not_treated_as_an_address_relocation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rebuilt_path = synthetic_macho(
                root / "rebuilt",
                uuid_value=symbolize.RELEASE_UUID,
                text_bytes=struct.pack("<4I", 0x91000400, 0xD503201F, 0xD65F03C0, 0xD503201F),
            )
            release_path = synthetic_macho(
                root / "release",
                uuid_value=symbolize.RELEASE_UUID,
                text_bytes=struct.pack("<4I", 0x91000800, 0xD503201F, 0xD65F03C0, 0xD503201F),
            )
            rebuilt = symbolize.parse_macho_identity(rebuilt_path)
            release = symbolize.parse_macho_identity(release_path)
            anchors = symbolize.release_symbol_layout_anchors(rebuilt, release, (0x204,))
            with self.assertRaises(SystemExit):
                symbolize.release_target_code_equivalence(
                    rebuilt_path,
                    release_path,
                    rebuilt,
                    release,
                    anchors,
                )

    def test_function_start_decoder_rejects_uint64_uleb_overflow(self) -> None:
        overflowing = b"\x81" * 9 + b"\x02\x00"
        with self.assertRaises(SystemExit):
            symbolize.decode_function_starts(overflowing, 0, len(overflowing), 0)

    def test_release_layout_anchor_requires_one_exact_text_symbol(self) -> None:
        anchor = {
            "offset": "0x204",
            "absolute": "0x100000204",
            "functionStart": "0x100000200",
            "functionEnd": "0x100000208",
        }
        bound = symbolize.bind_layout_anchors_to_symbols(
            [anchor],
            [(0x100000200, "t", "stasis::bound_function")],
        )
        self.assertEqual(bound[0]["expectedLlvmNm"], "stasis::bound_function + 0x4")
        with self.assertRaises(SystemExit):
            symbolize.bind_layout_anchors_to_symbols([anchor], [])


if __name__ == "__main__":
    unittest.main()
