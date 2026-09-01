#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


MODULE_PATH = Path(__file__).with_name("capture_v032_macos_prestrip.py")
SPEC = importlib.util.spec_from_file_location("capture_v032_macos_prestrip", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
capture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(capture)


class CaptureV032MacosPrestripTests(unittest.TestCase):
    def test_wrapper_is_bash_syntax_valid_and_fail_closed(self) -> None:
        text = capture.wrapper_text()
        self.assertIn('"$1" != "--strip-all"', text)
        self.assertIn("unexpected rust-objcopy invocation targeted", text)
        self.assertIn('/bin/mkdir -- "$STASIS_PRESTRIP_CAPTURE_LOCK"', text)
        self.assertIn('exec "$STASIS_REAL_RUST_OBJCOPY" "$@"', text)
        bash = shutil.which("bash")
        if bash is None or os.name == "nt":
            self.skipTest("native bash is unavailable")
        completed = subprocess.run([bash, "-n"], input=text, text=True, capture_output=True, check=False)
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_nul_invocation_requires_termination_and_preserves_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "args0"
            path.write_bytes(b"--strip-all\0/tmp/stasis\0")
            self.assertEqual(capture.decode_invocation(path), ["--strip-all", "/tmp/stasis"])

    def test_nul_invocation_rejects_truncation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "args0"
            path.write_bytes(b"--strip-all\0/tmp/stasis")
            with self.assertRaises(SystemExit):
                capture.decode_invocation(path)

    def test_canonical_handles_a_not_yet_created_leaf(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            expected = Path(directory) / "target" / "production-stripped" / "stasis"
            self.assertTrue(str(capture.canonical(expected)).endswith("stasis"))


if __name__ == "__main__":
    unittest.main()
