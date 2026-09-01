#!/usr/bin/env python3
"""Capture the exact pre-strip input of the immutable Stasis v0.3.2 macOS build.

This utility is diagnostic-only.  It replaces the pinned Rust toolchain's
``rust-objcopy`` with a narrowly scoped forwarding wrapper, records the one
``--strip-all`` invocation plus its before/after bytes for Cargo's hashed
Stasis executable, and restores the original tool byte-for-byte after the
build.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
from typing import NoReturn


STATE_SCHEMA = "stasis-v0.3.2-macos-rust-objcopy-capture-state-v3"
RESULT_SCHEMA = "stasis-v0.3.2-macos-rust-objcopy-capture-v3"
# The pinned Darwin linker strips this hashed output before Cargo surfaces the
# sibling ``production-stripped/stasis`` executable.
HASHED_TARGET_PATTERN = re.compile(r"^stasis-[0-9a-f]{16}$")
RELEASE_BINARY_SHA256 = "42c30bacde31906457b11d0e64ddc4f57e20515016d4cae817d4e1cc8e016c1c"
RELEASE_BINARY_BYTES = 74_639_920


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def canonical(path: Path) -> Path:
    return Path(os.path.realpath(os.fspath(path)))


def command_output(argv: list[str]) -> str:
    completed = subprocess.run(
        argv,
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    return completed.stdout.strip()


def wrapper_text() -> str:
    return r"""#!/bin/bash
set -euo pipefail

: "${STASIS_EXPECTED_STRIP_DIRECTORY:?}"
: "${STASIS_PRESTRIP_CAPTURE:?}"
: "${STASIS_POSTSTRIP_CAPTURE:?}"
: "${STASIS_PRESTRIP_CAPTURE_LOCK:?}"
: "${STASIS_PRESTRIP_INVOCATION:?}"
: "${STASIS_REAL_RUST_OBJCOPY:?}"

target_argument_count=0
target_argument=''
target_canonical=''
for argument in "$@"; do
  if [[ -e "$argument" ]]; then
    candidate_dir=$(cd -P -- "$(dirname -- "$argument")" && pwd)
    candidate="$candidate_dir/$(basename -- "$argument")"
    candidate_name=$(basename -- "$candidate")
    if [[ "$candidate_dir" == "$STASIS_EXPECTED_STRIP_DIRECTORY" && \
          "$candidate_name" == stasis* ]]; then
      if [[ ! "$candidate_name" =~ ^stasis-[0-9a-f]{16}$ ]]; then
        echo 'unexpected Stasis rust-objcopy target in the hashed deps directory' >&2
        exit 64
      fi
      target_argument_count=$((target_argument_count + 1))
      target_argument="$argument"
      target_canonical="$candidate"
    fi
  fi
done

if [[ "$target_argument_count" -ne 0 ]]; then
  if [[ "$target_argument_count" -ne 1 || "$#" -ne 2 || \
        "$1" != "--strip-all" || "$2" != "$target_argument" ]]; then
    echo 'unexpected rust-objcopy invocation targeted the Stasis executable' >&2
    exit 64
  fi
  /bin/mkdir -- "$STASIS_PRESTRIP_CAPTURE_LOCK"
  temporary="${STASIS_PRESTRIP_CAPTURE}.tmp.$$"
  trap '/bin/rm -f -- "$temporary"' EXIT
  /bin/cp -p -- "$2" "$temporary"
  /bin/mv -- "$temporary" "$STASIS_PRESTRIP_CAPTURE"
  working_directory=$(pwd -P)
  printf '%s\0' "$working_directory" "$target_canonical" "$@" \
    > "$STASIS_PRESTRIP_INVOCATION"
  /usr/bin/shasum -a 256 "$STASIS_PRESTRIP_CAPTURE" \
    > "${STASIS_PRESTRIP_CAPTURE}.sha256"
  trap - EXIT
  "$STASIS_REAL_RUST_OBJCOPY" "$@"
  poststrip_temporary="${STASIS_POSTSTRIP_CAPTURE}.tmp.$$"
  trap '/bin/rm -f -- "$poststrip_temporary"' EXIT
  /bin/cp -p -- "$2" "$poststrip_temporary"
  /bin/mv -- "$poststrip_temporary" "$STASIS_POSTSTRIP_CAPTURE"
  /usr/bin/shasum -a 256 "$STASIS_POSTSTRIP_CAPTURE" \
    > "${STASIS_POSTSTRIP_CAPTURE}.sha256"
  trap - EXIT
  exit 0
fi

exec "$STASIS_REAL_RUST_OBJCOPY" "$@"
"""


def write_json(path: Path, value: object) -> None:
    temporary = path.with_name(f"{path.name}.tmp.{os.getpid()}")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def append_github_env(path: Path, values: dict[str, str]) -> None:
    for key, value in values.items():
        if "\n" in key or "\n" in value or "\r" in key or "\r" in value:
            fail(f"refusing newline in GitHub environment value {key!r}")
    with path.open("a", encoding="utf-8", newline="\n") as handle:
        for key, value in values.items():
            handle.write(f"{key}={value}\n")


def load_state(path: Path) -> dict[str, object]:
    try:
        state = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"could not read capture state {path}: {error}")
    if not isinstance(state, dict) or state.get("schema") != STATE_SCHEMA:
        fail(f"unexpected capture state schema in {path}")
    return state


def locate_objcopy(rustc: str) -> tuple[Path, Path, Path]:
    sysroot = canonical(Path(command_output([rustc, "--print", "sysroot"])))
    target_libdir = canonical(Path(command_output([rustc, "--print", "target-libdir"])))
    objcopy = canonical(target_libdir.parent / "bin" / "rust-objcopy")
    try:
        objcopy.relative_to(sysroot)
    except ValueError:
        fail(f"rust-objcopy escaped the pinned sysroot: {objcopy} not under {sysroot}")
    if not objcopy.is_file() or not os.access(objcopy, os.X_OK):
        fail(f"pinned rust-objcopy is not executable: {objcopy}")
    return sysroot, target_libdir, objcopy


def install(args: argparse.Namespace) -> None:
    expected_directory = canonical(args.expected_deps_directory)
    expected_cargo_output = canonical(args.expected_cargo_output)
    capture = canonical(args.capture)
    poststrip_capture = canonical(args.poststrip_capture)
    invocation = canonical(args.invocation_file)
    lock = canonical(args.lock)
    state_path = canonical(args.state)
    evidence = canonical(args.evidence)
    github_env = canonical(args.github_env)

    if not evidence.is_dir():
        fail(f"evidence directory does not exist: {evidence}")
    if not github_env.is_file():
        fail(f"GITHUB_ENV file does not exist: {github_env}")
    if expected_directory.name != "deps":
        fail(f"expected strip directory is not Cargo's deps directory: {expected_directory}")
    if expected_cargo_output.name != "stasis" or expected_cargo_output.parent != expected_directory.parent:
        fail("expected Cargo output is not the stasis sibling of the hashed deps directory")
    if expected_directory.exists() or expected_cargo_output.exists():
        fail("production-stripped outputs already exist before the exact build")
    for path in (capture, poststrip_capture, invocation, lock, state_path):
        if path.exists():
            fail(f"capture path already exists: {path}")
    capture.parent.mkdir(parents=True, exist_ok=True)
    poststrip_capture.parent.mkdir(parents=True, exist_ok=True)
    invocation.parent.mkdir(parents=True, exist_ok=True)

    sysroot, target_libdir, objcopy = locate_objcopy(args.rustc)
    run_id = os.environ.get("GITHUB_RUN_ID", "")
    run_attempt = os.environ.get("GITHUB_RUN_ATTEMPT", "")
    if not run_id.isdecimal() or not run_attempt.isdecimal():
        fail("GITHUB_RUN_ID and GITHUB_RUN_ATTEMPT must be decimal integers")
    real = objcopy.with_name(f"{objcopy.name}.stasis-real-{run_id}-{run_attempt}")
    if real.exists():
        fail(f"real rust-objcopy backup already exists: {real}")

    original_sha = sha256(objcopy)
    wrapper = wrapper_text().encode("utf-8")
    wrapper_sha = hashlib.sha256(wrapper).hexdigest()
    wrapper_copy = evidence / "rust-objcopy-wrapper.sh"
    if wrapper_copy.exists():
        fail(f"wrapper evidence already exists: {wrapper_copy}")
    wrapper_copy.write_bytes(wrapper)
    wrapper_copy.chmod(0o755)
    (evidence / "rust-objcopy.original.sha256").write_text(f"{original_sha}  {objcopy}\n", encoding="utf-8")
    (evidence / "rust-objcopy-wrapper.sha256").write_text(f"{wrapper_sha}  rust-objcopy-wrapper.sh\n", encoding="utf-8")

    os.replace(objcopy, real)
    try:
        temporary = objcopy.with_name(f"{objcopy.name}.tmp.{os.getpid()}")
        temporary.write_bytes(wrapper)
        temporary.chmod(
            stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR | stat.S_IRGRP | stat.S_IXGRP | stat.S_IROTH | stat.S_IXOTH
        )
        os.replace(temporary, objcopy)
        if sha256(objcopy) != wrapper_sha:
            fail("installed rust-objcopy wrapper hash does not match")
        state: dict[str, object] = {
            "schema": STATE_SCHEMA,
            "installed": True,
            "restored": False,
            "sysroot": os.fspath(sysroot),
            "targetLibdir": os.fspath(target_libdir),
            "objcopy": os.fspath(objcopy),
            "realObjcopy": os.fspath(real),
            "expectedStripDirectory": os.fspath(expected_directory),
            "expectedCargoOutput": os.fspath(expected_cargo_output),
            "hashedTargetPattern": HASHED_TARGET_PATTERN.pattern,
            "capture": os.fspath(capture),
            "poststripCapture": os.fspath(poststrip_capture),
            "captureInvocation": os.fspath(invocation),
            "captureLock": os.fspath(lock),
            "originalObjcopySha256": original_sha,
            "wrapperSha256": wrapper_sha,
            "githubRunId": int(run_id),
            "githubRunAttempt": int(run_attempt),
            "releaseGateAuthority": False,
        }
        write_json(state_path, state)
        append_github_env(
            github_env,
            {
                "STASIS_EXPECTED_STRIP_DIRECTORY": os.fspath(expected_directory),
                "STASIS_EXPECTED_CARGO_OUTPUT": os.fspath(expected_cargo_output),
                "STASIS_PRESTRIP_CAPTURE": os.fspath(capture),
                "STASIS_POSTSTRIP_CAPTURE": os.fspath(poststrip_capture),
                "STASIS_PRESTRIP_CAPTURE_LOCK": os.fspath(lock),
                "STASIS_PRESTRIP_INVOCATION": os.fspath(invocation),
                "STASIS_REAL_RUST_OBJCOPY": os.fspath(real),
                "STASIS_RUST_OBJCOPY_STATE": os.fspath(state_path),
            },
        )
    except BaseException:
        try:
            if objcopy.exists():
                objcopy.unlink()
            if real.exists():
                os.replace(real, objcopy)
        finally:
            raise


def restore(args: argparse.Namespace) -> None:
    state_path = canonical(args.state)
    state = load_state(state_path)
    objcopy = canonical(Path(str(state["objcopy"])))
    real = canonical(Path(str(state["realObjcopy"])))
    wrapper_sha = str(state["wrapperSha256"])
    original_sha = str(state["originalObjcopySha256"])
    if state.get("restored") is not False or state.get("installed") is not True:
        fail("capture state is not an installed, unrestored wrapper")
    if not objcopy.is_file() or sha256(objcopy) != wrapper_sha:
        fail("installed rust-objcopy wrapper is missing or changed")
    if not real.is_file() or sha256(real) != original_sha:
        fail("saved real rust-objcopy is missing or changed")
    objcopy.unlink()
    os.replace(real, objcopy)
    if not os.access(objcopy, os.X_OK) or sha256(objcopy) != original_sha:
        fail("restored rust-objcopy does not match the original executable")
    state["restored"] = True
    state["restoredObjcopySha256"] = sha256(objcopy)
    write_json(state_path, state)


def decode_nul_record(path: Path) -> list[str]:
    raw = path.read_bytes()
    if not raw.endswith(b"\0"):
        fail(f"strip invocation record is not NUL terminated: {path}")
    fields = raw[:-1].split(b"\0")
    try:
        return [field.decode("utf-8") for field in fields]
    except UnicodeDecodeError as error:
        fail(f"strip invocation record is not UTF-8: {error}")


def files_equal(left: Path, right: Path) -> bool:
    if left.stat().st_size != right.stat().st_size:
        return False
    with left.open("rb") as left_handle, right.open("rb") as right_handle:
        while True:
            left_chunk = left_handle.read(1024 * 1024)
            right_chunk = right_handle.read(1024 * 1024)
            if left_chunk != right_chunk:
                return False
            if not left_chunk:
                return True


def hashed_targets(directory: Path) -> list[Path]:
    if not directory.is_dir():
        fail(f"hashed target directory is missing: {directory}")
    matches: list[Path] = []
    for candidate in directory.iterdir():
        if HASHED_TARGET_PATTERN.fullmatch(candidate.name):
            if candidate.is_symlink() or not candidate.is_file():
                fail(f"hashed Stasis target is not a regular file: {candidate}")
            matches.append(canonical(candidate))
    return sorted(matches, key=os.fspath)


def resolve_recorded_argument(argument: str, working_directory: Path) -> Path:
    candidate = Path(argument)
    if not candidate.is_absolute():
        candidate = working_directory / candidate
    return canonical(candidate)


def verify(args: argparse.Namespace) -> None:
    state_path = canonical(args.state)
    result_path = canonical(args.output)
    state = load_state(state_path)
    if state.get("restored") is not True:
        fail("real rust-objcopy was not restored before capture verification")
    if state.get("releaseGateAuthority") is not False:
        fail("capture state incorrectly asserts release-gate authority")
    capture = canonical(Path(str(state["capture"])))
    poststrip_capture = canonical(Path(str(state["poststripCapture"])))
    invocation = canonical(Path(str(state["captureInvocation"])))
    lock = canonical(Path(str(state["captureLock"])))
    expected_directory = canonical(Path(str(state["expectedStripDirectory"])))
    expected_cargo_output = canonical(Path(str(state["expectedCargoOutput"])))
    built = canonical(args.built)
    release_binary = canonical(args.release_binary)
    if state.get("hashedTargetPattern") != HASHED_TARGET_PATTERN.pattern:
        fail("capture state does not bind the exact hashed Stasis target pattern")
    if built != expected_cargo_output or expected_directory != canonical(built.parent / "deps"):
        fail("verification Cargo output does not match the installed capture state")
    for path, label in ((capture, "pre-strip capture"), (poststrip_capture, "post-strip invocation capture")):
        if not path.is_file() or path.stat().st_size == 0:
            fail(f"{label} is missing or empty")
    if not invocation.is_file() or not lock.is_dir():
        fail("strip invocation record or atomic single-invocation lock is missing")
    matches = hashed_targets(expected_directory)
    if len(matches) != 1:
        fail(f"expected exactly one hashed Stasis target, got {matches!r}")
    actual_target = matches[0]
    recorded = decode_nul_record(invocation)
    if len(recorded) != 4:
        fail(f"unexpected rust-objcopy invocation record: {recorded!r}")
    working_directory = canonical(Path(recorded[0]))
    recorded_canonical_target = canonical(Path(recorded[1]))
    argv = recorded[2:]
    if (
        recorded[0] != os.fspath(working_directory)
        or recorded[1] != os.fspath(actual_target)
        or recorded_canonical_target != actual_target
        or argv[0] != "--strip-all"
        or resolve_recorded_argument(argv[1], working_directory) != actual_target
    ):
        fail(f"unexpected rust-objcopy invocation: {recorded!r}")
    capture_sha = sha256(capture)
    sidecar = Path(f"{capture}.sha256")
    if not sidecar.is_file():
        fail("pre-strip capture hash sidecar is missing")
    sidecar_fields = sidecar.read_text(encoding="utf-8").strip().split()
    if sidecar_fields != [capture_sha, os.fspath(capture)]:
        fail("pre-strip capture hash sidecar does not match")
    poststrip_capture_sha = sha256(poststrip_capture)
    poststrip_sidecar = Path(f"{poststrip_capture}.sha256")
    if not poststrip_sidecar.is_file():
        fail("post-strip invocation capture hash sidecar is missing")
    poststrip_sidecar_fields = poststrip_sidecar.read_text(encoding="utf-8").strip().split()
    if poststrip_sidecar_fields != [poststrip_capture_sha, os.fspath(poststrip_capture)]:
        fail("post-strip invocation capture hash sidecar does not match")
    for path, label in (
        (actual_target, "post-strip hashed target"),
        (built, "Cargo output"),
        (release_binary, "immutable release binary"),
    ):
        if not path.is_file() or path.stat().st_size == 0:
            fail(f"{label} is missing or empty: {path}")
    if release_binary.stat().st_size != RELEASE_BINARY_BYTES or sha256(release_binary) != RELEASE_BINARY_SHA256:
        fail("immutable release binary identity changed")
    if not files_equal(poststrip_capture, actual_target) or not files_equal(actual_target, built):
        fail("captured post-strip target is not byte-identical to the hashed target and Cargo output")
    poststrip_sha = sha256(actual_target)
    if capture_sha == poststrip_capture_sha or files_equal(capture, poststrip_capture):
        fail("pre-strip capture is not distinct from the post-strip executable")
    whole_file_matches_release = files_equal(poststrip_capture, release_binary)
    original_objcopy_sha = state.get("originalObjcopySha256")
    restored_objcopy_sha = state.get("restoredObjcopySha256")
    wrapper_sha = state.get("wrapperSha256")
    if (
        not isinstance(original_objcopy_sha, str)
        or re.fullmatch(r"[0-9a-f]{64}", original_objcopy_sha) is None
        or restored_objcopy_sha != original_objcopy_sha
        or not isinstance(wrapper_sha, str)
        or re.fullmatch(r"[0-9a-f]{64}", wrapper_sha) is None
    ):
        fail("restored rust-objcopy does not match the original executable")
    result = {
        "schema": RESULT_SCHEMA,
        "expectedStripDirectory": os.fspath(expected_directory),
        "expectedCargoOutput": os.fspath(expected_cargo_output),
        "hashedTargetPattern": HASHED_TARGET_PATTERN.pattern,
        "hashedTargetCount": 1,
        "hashedTargets": [os.fspath(actual_target)],
        "actualStripTarget": os.fspath(actual_target),
        "capture": os.fspath(capture),
        "captureBytes": capture.stat().st_size,
        "captureSha256": capture_sha,
        "poststripCapture": os.fspath(poststrip_capture),
        "poststripCaptureBytes": poststrip_capture.stat().st_size,
        "poststripCaptureSha256": poststrip_capture_sha,
        "rustObjcopyInvocation": {
            "workingDirectory": os.fspath(working_directory),
            "argv": argv,
            "canonicalTarget": os.fspath(recorded_canonical_target),
        },
        "singleTargetInvocation": True,
        "poststripHashedTargetBytes": actual_target.stat().st_size,
        "poststripHashedTargetSha256": poststrip_sha,
        "cargoOutputBytes": built.stat().st_size,
        "cargoOutputSha256": sha256(built),
        "immutableReleaseBytes": release_binary.stat().st_size,
        "immutableReleaseSha256": sha256(release_binary),
        "prestripDiffersFromPoststrip": True,
        "poststripCaptureMatchesHashedTarget": True,
        "poststripHashedTargetMatchesCargoOutput": True,
        "rebuiltPoststripWholeFileMatchesImmutableRelease": whole_file_matches_release,
        "originalObjcopySha256": original_objcopy_sha,
        "restoredObjcopySha256": restored_objcopy_sha,
        "wrapperSha256": wrapper_sha,
        "releaseGateAuthority": False,
    }
    write_json(result_path, result)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser()
    subparsers = result.add_subparsers(dest="command", required=True)

    install_parser = subparsers.add_parser("install")
    install_parser.add_argument("--rustc", default="rustc")
    install_parser.add_argument("--expected-deps-directory", type=Path, required=True)
    install_parser.add_argument("--expected-cargo-output", type=Path, required=True)
    install_parser.add_argument("--capture", type=Path, required=True)
    install_parser.add_argument("--poststrip-capture", type=Path, required=True)
    install_parser.add_argument("--invocation-file", type=Path, required=True)
    install_parser.add_argument("--lock", type=Path, required=True)
    install_parser.add_argument("--state", type=Path, required=True)
    install_parser.add_argument("--evidence", type=Path, required=True)
    install_parser.add_argument("--github-env", type=Path, required=True)
    install_parser.set_defaults(handler=install)

    restore_parser = subparsers.add_parser("restore")
    restore_parser.add_argument("--state", type=Path, required=True)
    restore_parser.set_defaults(handler=restore)

    verify_parser = subparsers.add_parser("verify")
    verify_parser.add_argument("--state", type=Path, required=True)
    verify_parser.add_argument("--built", type=Path, required=True)
    verify_parser.add_argument("--release-binary", type=Path, required=True)
    verify_parser.add_argument("--output", type=Path, required=True)
    verify_parser.set_defaults(handler=verify)
    return result


def main() -> None:
    args = parser().parse_args()
    args.handler(args)


if __name__ == "__main__":
    main()
