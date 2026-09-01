#!/usr/bin/env python3
"""Capture the exact pre-strip input of the immutable Stasis v0.3.2 macOS build.

This utility is diagnostic-only.  It replaces the pinned Rust toolchain's
``rust-objcopy`` with a narrowly scoped forwarding wrapper, records the input
to the one expected ``--strip-all`` invocation, and restores the original
tool byte-for-byte after the build.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
from typing import NoReturn


STATE_SCHEMA = "stasis-v0.3.2-macos-rust-objcopy-capture-state-v1"
RESULT_SCHEMA = "stasis-v0.3.2-macos-rust-objcopy-capture-v1"


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

: "${STASIS_EXPECTED_STRIP_TARGET:?}"
: "${STASIS_PRESTRIP_CAPTURE:?}"
: "${STASIS_PRESTRIP_CAPTURE_LOCK:?}"
: "${STASIS_PRESTRIP_ARGS:?}"
: "${STASIS_REAL_RUST_OBJCOPY:?}"

target_argument_count=0
target_argument=''
for argument in "$@"; do
  if [[ -e "$argument" ]]; then
    candidate_dir=$(cd -P -- "$(dirname -- "$argument")" && pwd)
    candidate="$candidate_dir/$(basename -- "$argument")"
    if [[ "$candidate" == "$STASIS_EXPECTED_STRIP_TARGET" ]]; then
      target_argument_count=$((target_argument_count + 1))
      target_argument="$argument"
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
  printf '%s\0' "$@" > "$STASIS_PRESTRIP_ARGS"
  /usr/bin/shasum -a 256 "$STASIS_PRESTRIP_CAPTURE" \
    > "${STASIS_PRESTRIP_CAPTURE}.sha256"
  trap - EXIT
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
    expected = canonical(args.expected)
    capture = canonical(args.capture)
    invocation = canonical(args.args_file)
    lock = canonical(args.lock)
    state_path = canonical(args.state)
    evidence = canonical(args.evidence)
    github_env = canonical(args.github_env)

    if not evidence.is_dir():
        fail(f"evidence directory does not exist: {evidence}")
    if not github_env.is_file():
        fail(f"GITHUB_ENV file does not exist: {github_env}")
    for path in (capture, invocation, lock, state_path):
        if path.exists():
            fail(f"capture path already exists: {path}")
    capture.parent.mkdir(parents=True, exist_ok=True)
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
            "expectedTarget": os.fspath(expected),
            "capture": os.fspath(capture),
            "captureArgs": os.fspath(invocation),
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
                "STASIS_EXPECTED_STRIP_TARGET": os.fspath(expected),
                "STASIS_PRESTRIP_CAPTURE": os.fspath(capture),
                "STASIS_PRESTRIP_CAPTURE_LOCK": os.fspath(lock),
                "STASIS_PRESTRIP_ARGS": os.fspath(invocation),
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


def decode_invocation(path: Path) -> list[str]:
    raw = path.read_bytes()
    if not raw.endswith(b"\0"):
        fail(f"strip invocation record is not NUL terminated: {path}")
    fields = raw[:-1].split(b"\0")
    try:
        return [field.decode("utf-8") for field in fields]
    except UnicodeDecodeError as error:
        fail(f"strip invocation record is not UTF-8: {error}")


def verify(args: argparse.Namespace) -> None:
    state_path = canonical(args.state)
    result_path = canonical(args.output)
    state = load_state(state_path)
    if state.get("restored") is not True:
        fail("real rust-objcopy was not restored before capture verification")
    capture = canonical(Path(str(state["capture"])))
    invocation = canonical(Path(str(state["captureArgs"])))
    lock = canonical(Path(str(state["captureLock"])))
    expected = canonical(Path(str(state["expectedTarget"])))
    if not capture.is_file() or capture.stat().st_size == 0:
        fail("pre-strip capture is missing or empty")
    if not invocation.is_file() or not lock.is_dir():
        fail("strip invocation record or atomic single-invocation lock is missing")
    recorded = decode_invocation(invocation)
    if recorded != ["--strip-all", os.fspath(expected)]:
        fail(f"unexpected rust-objcopy invocation: {recorded!r}")
    capture_sha = sha256(capture)
    sidecar = Path(f"{capture}.sha256")
    if not sidecar.is_file():
        fail("pre-strip capture hash sidecar is missing")
    sidecar_fields = sidecar.read_text(encoding="utf-8").strip().split()
    if sidecar_fields != [capture_sha, os.fspath(capture)]:
        fail("pre-strip capture hash sidecar does not match")
    result = {
        "schema": RESULT_SCHEMA,
        "expectedTarget": os.fspath(expected),
        "capture": os.fspath(capture),
        "captureBytes": capture.stat().st_size,
        "captureSha256": capture_sha,
        "rustObjcopyArgs": recorded,
        "singleTargetInvocation": True,
        "originalObjcopySha256": state["originalObjcopySha256"],
        "restoredObjcopySha256": state["restoredObjcopySha256"],
        "wrapperSha256": state["wrapperSha256"],
        "releaseGateAuthority": False,
    }
    write_json(result_path, result)


def parser() -> argparse.ArgumentParser:
    result = argparse.ArgumentParser()
    subparsers = result.add_subparsers(dest="command", required=True)

    install_parser = subparsers.add_parser("install")
    install_parser.add_argument("--rustc", default="rustc")
    install_parser.add_argument("--expected", type=Path, required=True)
    install_parser.add_argument("--capture", type=Path, required=True)
    install_parser.add_argument("--args-file", type=Path, required=True)
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
    verify_parser.add_argument("--output", type=Path, required=True)
    verify_parser.set_defaults(handler=verify)
    return result


def main() -> None:
    args = parser().parse_args()
    args.handler(args)


if __name__ == "__main__":
    main()
