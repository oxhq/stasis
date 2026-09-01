#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import unittest


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


def synthetic_capture_result(root: Path) -> tuple[Path, Path, Path, dict[str, object]]:
    profile = root / "target" / "production-stripped"
    deps = profile / "deps"
    deps.mkdir(parents=True)
    actual = deps / "stasis-0123456789abcdef"
    built = profile / "stasis"
    release = root / "release" / "stasis"
    release.parent.mkdir()
    poststrip = b"post-strip executable"
    for path in (actual, built, release):
        path.write_bytes(poststrip)
    prestrip = root / "stasis.prestrip"
    prestrip.write_bytes(b"pre-strip executable with local symbols")
    root = symbolize.canonical(root)
    actual = symbolize.canonical(actual)
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
        "poststripHashedTargetMatchesCargoOutput": True,
        "poststripHashedTargetMatchesImmutableRelease": True,
        "originalObjcopySha256": "a" * 64,
        "restoredObjcopySha256": "a" * 64,
        "wrapperSha256": "b" * 64,
        "releaseGateAuthority": False,
    }
    return prestrip, built, release, result


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
            prestrip, built, release, result = synthetic_capture_result(Path(directory).resolve())
            validated = symbolize.validate_capture_result(result, prestrip, built, release)
            self.assertEqual(validated["actualStripTarget"], result["actualStripTarget"])

    def test_capture_result_rejects_a_second_hashed_target(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            prestrip, built, release, result = synthetic_capture_result(root)
            (root / "target" / "production-stripped" / "deps" / "stasis-fedcba9876543210").write_bytes(
                b"post-strip executable"
            )
            with self.assertRaises(SystemExit):
                symbolize.validate_capture_result(result, prestrip, built, release)

    def test_capture_result_rejects_the_old_top_level_cargo_target(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            prestrip, built, release, result = synthetic_capture_result(Path(directory).resolve())
            result["actualStripTarget"] = os.fspath(built)
            with self.assertRaises(SystemExit):
                symbolize.validate_capture_result(result, prestrip, built, release)

    def test_capture_result_rejects_poststrip_bytes_not_matching_the_release(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            prestrip, built, release, result = synthetic_capture_result(Path(directory).resolve())
            actual = Path(str(result["actualStripTarget"]))
            actual.write_bytes(b"different post-strip executable")
            with self.assertRaises(SystemExit):
                symbolize.validate_capture_result(result, prestrip, built, release)


if __name__ == "__main__":
    unittest.main()
