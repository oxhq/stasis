#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest import mock


MODULE_PATH = Path(__file__).with_name("capture_v032_macos_prestrip.py")
SPEC = importlib.util.spec_from_file_location("capture_v032_macos_prestrip", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
capture = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(capture)


class CaptureV032MacosPrestripTests(unittest.TestCase):
    def test_wrapper_is_bash_syntax_valid_and_fail_closed(self) -> None:
        text = capture.wrapper_text()
        self.assertIn('"$1" != "--strip-all"', text)
        self.assertIn("^stasis-[0-9a-f]{16}$", text)
        self.assertIn('"$candidate_dir" == "$STASIS_EXPECTED_STRIP_DIRECTORY"', text)
        self.assertIn("unexpected rust-objcopy invocation targeted", text)
        self.assertIn('/bin/mkdir -- "$STASIS_PRESTRIP_CAPTURE_LOCK"', text)
        self.assertIn('"$working_directory" "$target_canonical" "$@"', text)
        self.assertIn('"$STASIS_REAL_RUST_OBJCOPY" "$@"', text)
        self.assertIn('"$2" "$poststrip_temporary"', text)
        self.assertIn('exec "$STASIS_REAL_RUST_OBJCOPY" "$@"', text)
        bash = shutil.which("bash")
        if bash is None or os.name == "nt":
            self.skipTest("native bash is unavailable")
        completed = subprocess.run([bash, "-n"], input=text, text=True, capture_output=True, check=False)
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_wrapper_captures_the_hashed_deps_target_and_records_actual_invocation(self) -> None:
        bash = shutil.which("bash")
        if bash is None or os.name == "nt":
            self.skipTest("native bash is unavailable")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            deps = root / "target" / "production-stripped" / "deps"
            deps.mkdir(parents=True)
            target = deps / "stasis-0123456789abcdef"
            target.write_bytes(b"pre-strip executable")
            wrapper = root / "rust-objcopy"
            wrapper.write_text(capture.wrapper_text(), encoding="utf-8")
            wrapper.chmod(0o755)
            real = root / "rust-objcopy-real"
            real.write_text(
                "#!/bin/bash\nset -euo pipefail\nprintf 'post-strip executable' > \"$2\"\n",
                encoding="utf-8",
            )
            real.chmod(0o755)
            captured = root / "captured"
            poststrip_captured = root / "poststrip-captured"
            invocation = root / "invocation0"
            lock = root / "lock"
            env = os.environ.copy()
            env.update(
                {
                    "STASIS_EXPECTED_STRIP_DIRECTORY": os.fspath(deps),
                    "STASIS_PRESTRIP_CAPTURE": os.fspath(captured),
                    "STASIS_POSTSTRIP_CAPTURE": os.fspath(poststrip_captured),
                    "STASIS_PRESTRIP_CAPTURE_LOCK": os.fspath(lock),
                    "STASIS_PRESTRIP_INVOCATION": os.fspath(invocation),
                    "STASIS_REAL_RUST_OBJCOPY": os.fspath(real),
                }
            )
            completed = subprocess.run(
                [bash, os.fspath(wrapper), "--strip-all", os.fspath(target)],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(captured.read_bytes(), b"pre-strip executable")
            self.assertEqual(poststrip_captured.read_bytes(), b"post-strip executable")
            self.assertEqual(
                capture.decode_nul_record(invocation),
                [os.fspath(root), os.fspath(target), "--strip-all", os.fspath(target)],
            )
            duplicate = subprocess.run(
                [bash, os.fspath(wrapper), "--strip-all", os.fspath(target)],
                cwd=root,
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertNotEqual(duplicate.returncode, 0)

    def test_nul_invocation_requires_termination_and_preserves_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "args0"
            path.write_bytes(b"--strip-all\0/tmp/stasis\0")
            self.assertEqual(capture.decode_nul_record(path), ["--strip-all", "/tmp/stasis"])

    def test_nul_invocation_rejects_truncation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "args0"
            path.write_bytes(b"--strip-all\0/tmp/stasis")
            with self.assertRaises(SystemExit):
                capture.decode_nul_record(path)

    def test_canonical_handles_a_not_yet_created_leaf(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            expected = Path(directory) / "target" / "production-stripped" / "stasis"
            self.assertTrue(str(capture.canonical(expected)).endswith("stasis"))

    def test_hashed_target_pattern_is_exactly_sixteen_lowercase_hex_digits(self) -> None:
        self.assertIsNotNone(capture.HASHED_TARGET_PATTERN.fullmatch("stasis-0123456789abcdef"))
        for value in (
            "stasis",
            "stasis-0123456789abcde",
            "stasis-0123456789abcdef0",
            "stasis-0123456789abcdeF",
            "stasis-0123456789abcdef.d",
        ):
            self.assertIsNone(capture.HASHED_TARGET_PATTERN.fullmatch(value), value)

    def make_verify_fixture(self, root: Path) -> SimpleNamespace:
        profile = root / "target" / "production-stripped"
        deps = profile / "deps"
        deps.mkdir(parents=True)
        poststrip = b"post-strip executable"
        actual = deps / "stasis-0123456789abcdef"
        built = profile / "stasis"
        release = root / "release" / "stasis"
        release.parent.mkdir()
        for path in (actual, built):
            path.write_bytes(poststrip)
        release.write_bytes(b"immutable release with expected whole-file differences")
        prestrip = root / "evidence" / "stasis.prestrip"
        prestrip.parent.mkdir()
        prestrip.write_bytes(b"pre-strip executable with local symbols")
        Path(f"{prestrip}.sha256").write_text(
            f"{capture.sha256(prestrip)}  {capture.canonical(prestrip)}\n",
            encoding="utf-8",
        )
        poststrip_capture = root / "evidence" / "stasis.poststrip-at-invocation"
        poststrip_capture.write_bytes(poststrip)
        Path(f"{poststrip_capture}.sha256").write_text(
            f"{capture.sha256(poststrip_capture)}  {capture.canonical(poststrip_capture)}\n",
            encoding="utf-8",
        )
        invocation = root / "evidence" / "rust-objcopy.invocation0"
        working_directory = capture.canonical(root)
        actual = capture.canonical(actual)
        invocation.write_bytes(
            b"".join(
                os.fsencode(field) + b"\0"
                for field in (
                    working_directory,
                    actual,
                    "--strip-all",
                    actual,
                )
            )
        )
        lock = root / "capture.lock"
        lock.mkdir()
        state_path = root / "evidence" / "state.json"
        capture.write_json(
            state_path,
            {
                "schema": capture.STATE_SCHEMA,
                "installed": True,
                "restored": True,
                "expectedStripDirectory": os.fspath(capture.canonical(deps)),
                "expectedCargoOutput": os.fspath(capture.canonical(built)),
                "hashedTargetPattern": capture.HASHED_TARGET_PATTERN.pattern,
                "capture": os.fspath(capture.canonical(prestrip)),
                "poststripCapture": os.fspath(capture.canonical(poststrip_capture)),
                "captureInvocation": os.fspath(capture.canonical(invocation)),
                "captureLock": os.fspath(capture.canonical(lock)),
                "originalObjcopySha256": "a" * 64,
                "restoredObjcopySha256": "a" * 64,
                "wrapperSha256": "b" * 64,
                "releaseGateAuthority": False,
            },
        )
        args = SimpleNamespace(
            state=state_path,
            built=built,
            release_binary=release,
            output=root / "evidence" / "capture-result.json",
        )
        return args

    def test_verify_binds_one_hashed_target_and_accepts_release_whole_file_difference(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            args = self.make_verify_fixture(Path(directory).resolve())
            with (
                mock.patch.object(capture, "RELEASE_BINARY_SHA256", capture.sha256(args.release_binary)),
                mock.patch.object(capture, "RELEASE_BINARY_BYTES", args.release_binary.stat().st_size),
            ):
                capture.verify(args)
            result = json.loads(args.output.read_text(encoding="utf-8"))
            self.assertEqual(result["schema"], capture.RESULT_SCHEMA)
            self.assertEqual(result["hashedTargetCount"], 1)
            self.assertEqual(result["rustObjcopyInvocation"]["argv"][0], "--strip-all")
            self.assertTrue(result["poststripCaptureMatchesHashedTarget"])
            self.assertTrue(result["poststripHashedTargetMatchesCargoOutput"])
            self.assertFalse(result["rebuiltPoststripWholeFileMatchesImmutableRelease"])

    def test_verify_rejects_a_second_hashed_stasis_target(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            args = self.make_verify_fixture(root)
            (root / "target" / "production-stripped" / "deps" / "stasis-fedcba9876543210").write_bytes(
                b"post-strip executable"
            )
            with (
                mock.patch.object(capture, "RELEASE_BINARY_SHA256", capture.sha256(args.release_binary)),
                mock.patch.object(capture, "RELEASE_BINARY_BYTES", args.release_binary.stat().st_size),
                self.assertRaises(SystemExit),
            ):
                capture.verify(args)

    def test_verify_rejects_the_old_top_level_cargo_target_invocation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            args = self.make_verify_fixture(root)
            state = json.loads(args.state.read_text(encoding="utf-8"))
            invocation = Path(state["captureInvocation"])
            built = capture.canonical(args.built)
            invocation.write_bytes(
                b"".join(
                    os.fsencode(field) + b"\0"
                    for field in (
                        capture.canonical(root),
                        built,
                        "--strip-all",
                        built,
                    )
                )
            )
            with (
                mock.patch.object(capture, "RELEASE_BINARY_SHA256", capture.sha256(args.release_binary)),
                mock.patch.object(capture, "RELEASE_BINARY_BYTES", args.release_binary.stat().st_size),
                self.assertRaises(SystemExit),
            ):
                capture.verify(args)

    def test_verify_rejects_poststrip_bytes_that_differ_from_cargo_output(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            args = self.make_verify_fixture(root)
            target = root / "target" / "production-stripped" / "deps" / "stasis-0123456789abcdef"
            target.write_bytes(b"different post-strip bytes")
            with (
                mock.patch.object(capture, "RELEASE_BINARY_SHA256", capture.sha256(args.release_binary)),
                mock.patch.object(capture, "RELEASE_BINARY_BYTES", args.release_binary.stat().st_size),
                self.assertRaises(SystemExit),
            ):
                capture.verify(args)

    def test_verify_rejects_a_poststrip_capture_that_is_not_the_hashed_target(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory).resolve()
            args = self.make_verify_fixture(root)
            state = json.loads(args.state.read_text(encoding="utf-8"))
            poststrip_capture = Path(state["poststripCapture"])
            poststrip_capture.write_bytes(b"different captured post-strip bytes")
            Path(f"{poststrip_capture}.sha256").write_text(
                f"{capture.sha256(poststrip_capture)}  {capture.canonical(poststrip_capture)}\n",
                encoding="utf-8",
            )
            with (
                mock.patch.object(capture, "RELEASE_BINARY_SHA256", capture.sha256(args.release_binary)),
                mock.patch.object(capture, "RELEASE_BINARY_BYTES", args.release_binary.stat().st_size),
                self.assertRaises(SystemExit),
            ):
                capture.verify(args)


if __name__ == "__main__":
    unittest.main()
