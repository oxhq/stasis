#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
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


if __name__ == "__main__":
    unittest.main()
