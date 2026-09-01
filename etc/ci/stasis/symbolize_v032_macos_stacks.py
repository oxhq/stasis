#!/usr/bin/env python3
"""Bind and symbolize the nine immutable Stasis v0.3.2 macOS stack reports.

The script deliberately has no release authority.  It fails closed unless the
rebuilt post-strip executable is byte-identical to the immutable release and
the captured pre-strip executable has the same Mach-O UUID.
"""

from __future__ import annotations

import argparse
import bisect
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
from typing import Iterable, NoReturn


SCHEMA = "stasis-v0.3.2-macos-exact-symbolization-v1"
RELEASE_REVISION = "b3d1ac949d341dc6bbe1244162441d9bb8adb00a"
DIAGNOSTIC_REVISION = "9bdf49f360089cd91ae257f0078b2dc3dde4e70e"
RELEASE_TAG = "v0.3.2"
RELEASE_ID = 380180986
RELEASE_ASSET_ID = 538843353
RELEASE_ARCHIVE_SHA256 = "1bf3d640223f9352d773ee71a8333a22b5955a9230d66e2fde23f7d0e21e8c9b"
RELEASE_ARCHIVE_BYTES = 30_547_326
RELEASE_BINARY_SHA256 = "42c30bacde31906457b11d0e64ddc4f57e20515016d4cae817d4e1cc8e016c1c"
RELEASE_BINARY_BYTES = 74_639_920
RELEASE_UUID = "65B2AB64-84D0-3A7D-A121-D8055D51651D"
DIAGNOSTIC_RUN_ID = 33_466_775_208
DIAGNOSTIC_RUN_ATTEMPT = 1
DIAGNOSTIC_ARTIFACT_ID = 9_785_268_258
DIAGNOSTIC_ARTIFACT_NAME = "stasis-v0.3.2-macos-release-event-diagnostic-33466775208-attempt-1"
DIAGNOSTIC_ARTIFACT_DIGEST = "sha256:fb7014ed36f1ecb4e7a81ed1b1d323f389c9eed1fa9a1819e27b0540c6a61a47"
DIAGNOSTIC_ARTIFACT_BYTES = 466_977
DIAGNOSTIC_SUMMARY_SHA256 = "444f60a27dbb446459fff0ebe09fbb8958f20670140f8f9bf5598c651ca35ca1"
SAMPLES = (1, 3, 5, 9, 11, 12, 13, 18, 20)
MAIN_STABLE_OFFSETS = (0x7C064, 0xF608, 0x3C098, 0x65904, 0x1320018)
SCRIPT_STABLE_OFFSETS = (
    0x1320590,
    0xA45670,
    0xA472D4,
    0xA57560,
    0xA64458,
    0xA6821C,
    0x11A2708,
    0x1329F50,
)


def fail(message: str) -> NoReturn:
    raise SystemExit(message)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load_json(path: Path) -> object:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        fail(f"could not read JSON {path}: {error}")


def require_dict(value: object, label: str) -> dict[str, object]:
    if not isinstance(value, dict):
        fail(f"{label} is not a JSON object")
    return value


def run_command(argv: list[str], *, allow_failure: bool = False) -> subprocess.CompletedProcess[str]:
    completed = subprocess.run(
        argv,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        check=False,
    )
    if completed.returncode != 0 and not allow_failure:
        fail(
            f"command failed ({completed.returncode}): {argv!r}\n"
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        )
    return completed


def write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


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


def mach_uuid(path: Path, dwarfdump: str) -> str:
    completed = run_command([dwarfdump, "--uuid", os.fspath(path)])
    matches = re.findall(
        r"^UUID:\s+([0-9A-Fa-f-]{36})\s+\(arm64\)\s+.+$",
        completed.stdout,
        re.MULTILINE,
    )
    if len(matches) != 1:
        fail(f"expected one arm64 Mach-O UUID for {path}, got {matches!r}")
    return matches[0].upper()


@dataclass(frozen=True)
class Frame:
    offset: int
    absolute: int
    thread: str
    source_line: str


@dataclass(frozen=True)
class ParsedSample:
    sample: int
    path: Path
    load_address: int
    frames: tuple[Frame, ...]


EXPLICIT_FRAME = re.compile(
    r"\(in stasis\)\s+load address\s+(0x[0-9a-fA-F]+)\s+"
    r"\+\s+(0x[0-9a-fA-F]+)\s+\[(0x[0-9a-fA-F]+)\]"
)
GENERIC_STASIS_FRAME = re.compile(r"\(in stasis\).*\[(0x[0-9a-fA-F]+)\]")
THREAD_HEADER = re.compile(r"^\s*\d+\s+Thread_\d+")


def parse_sample_report(path: Path, sample: int, *, expected_pid: int | None = None) -> ParsedSample:
    text = path.read_text(encoding="utf-8")
    process_matches = re.findall(r"^Process:\s+stasis\s+\[(\d+)\]\s*$", text, re.MULTILINE)
    if len(process_matches) != 1:
        fail(f"sample {sample} does not contain one Stasis process identity")
    if expected_pid is not None and int(process_matches[0]) != expected_pid:
        fail(f"sample {sample} process PID does not match its phase evidence")
    load_matches = re.findall(r"^Load Address:\s+(0x[0-9a-fA-F]+)\s*$", text, re.MULTILINE)
    if len(load_matches) != 1:
        fail(f"sample {sample} does not contain one Load Address")
    load = int(load_matches[0], 16)
    if text.count("Code Type:       ARM64") != 1:
        fail(f"sample {sample} is not one ARM64 report")
    image_matches = re.findall(
        r"^\s*(0x[0-9a-fA-F]+)\s+-\s+(0x[0-9a-fA-F]+)\s+\+stasis\s+"
        r"\(0\)\s+<([0-9A-Fa-f-]{36})>\s+.+$",
        text,
        re.MULTILINE,
    )
    if len(image_matches) != 1:
        fail(f"sample {sample} does not contain one Stasis binary image")
    image_start, image_end, image_uuid = image_matches[0]
    if int(image_start, 16) != load or image_uuid.upper() != RELEASE_UUID:
        fail(f"sample {sample} Stasis image identity does not match")
    image_end_value = int(image_end, 16)

    try:
        graph = text.split("Call graph:\n", 1)[1].split("Total number in stack", 1)[0]
    except IndexError:
        fail(f"sample {sample} call graph delimiters are missing")
    current_thread = "unclassified"
    frames: list[Frame] = []
    stasis_line_count = 0
    for line in graph.splitlines():
        if THREAD_HEADER.match(line):
            if "com.apple.main-thread" in line:
                current_thread = "main"
            elif re.search(r":\s+Script#1\s*$", line):
                current_thread = "script"
            else:
                current_thread = "other"
        if "(in stasis)" not in line:
            continue
        generic = GENERIC_STASIS_FRAME.search(line)
        if generic is None:
            fail(f"sample {sample} has an unparseable Stasis frame: {line}")
        stasis_line_count += 1
        explicit = EXPLICIT_FRAME.search(line)
        if explicit is not None:
            frame_load = int(explicit.group(1), 16)
            offset = int(explicit.group(2), 16)
            absolute = int(explicit.group(3), 16)
            if frame_load != load or absolute != load + offset:
                fail(f"sample {sample} frame address arithmetic failed: {line}")
        else:
            absolute = int(generic.group(1), 16)
            if absolute < load:
                fail(f"sample {sample} Stasis frame precedes the image load: {line}")
            offset = absolute - load
        if absolute > image_end_value:
            fail(f"sample {sample} frame falls outside the Stasis image: {line}")
        frames.append(Frame(offset, absolute, current_thread, line.strip()))
    if not frames or len(frames) != stasis_line_count:
        fail(f"sample {sample} did not preserve every Stasis call-graph frame")

    main_offsets = {frame.offset for frame in frames if frame.thread == "main"}
    script_offsets = {frame.offset for frame in frames if frame.thread == "script"}
    missing_main = set(MAIN_STABLE_OFFSETS) - main_offsets
    missing_script = set(SCRIPT_STABLE_OFFSETS) - script_offsets
    if missing_main or missing_script:
        fail(
            f"sample {sample} is missing stable frames: main={sorted(missing_main)!r} script={sorted(missing_script)!r}"
        )
    return ParsedSample(sample, path, load, tuple(frames))


def validate_summary(input_dir: Path) -> dict[int, dict[str, object]]:
    identity = require_dict(load_json(input_dir / "identity.json"), "diagnostic identity")
    if (
        identity.get("schema") != "stasis-v0.3.2-macos-release-event-diagnostic-identity-v2"
        or identity.get("diagnosticHarnessRevision") != DIAGNOSTIC_REVISION
        or identity.get("releaseRevision") != RELEASE_REVISION
        or identity.get("releaseTag") != RELEASE_TAG
        or identity.get("archiveSha256") != RELEASE_ARCHIVE_SHA256
        or identity.get("binarySha256") != RELEASE_BINARY_SHA256
        or identity.get("sampleCount") != 20
        or identity.get("commandDeadlineMs") != 30_000
        or identity.get("sdkCommandTimeoutOverride") is not False
        or identity.get("closeTimeoutMs") != 30_000
        or identity.get("exactVerifierTimeoutsPreserved") is not True
        or identity.get("nativeLifecycleTrace") is not True
        or identity.get("releaseGateAuthority") is not False
    ):
        fail("diagnostic artifact identity changed")
    summary_path = input_dir / "summary.json"
    if sha256(summary_path) != DIAGNOSTIC_SUMMARY_SHA256:
        fail("diagnostic summary SHA-256 does not match the immutable artifact")
    summary = require_dict(load_json(summary_path), "diagnostic summary")
    if summary.get("schema") != "stasis-v0.3.2-macos-release-event-diagnostic-summary-v2":
        fail("unexpected diagnostic summary schema")
    if summary.get("releaseGateAuthority") is not False:
        fail("diagnostic summary incorrectly claims release authority")
    if summary.get("predeclaredSampleCount") != 20:
        fail("diagnostic summary sample census is not 20")
    if summary.get("counts") != {
        "completed": 11,
        "cssRequest5Timeout": 0,
        "cookieRequest5Timeout": 9,
        "otherFailure": 0,
    }:
        fail("diagnostic summary classification counts changed")
    if summary.get("stackSampleRecords") != {"begin": 9, "end": 9, "error": 0}:
        fail("diagnostic stack-record counts changed")
    if summary.get("validatedTimeoutStackCaptures") != 9:
        fail("diagnostic summary does not validate exactly nine captures")
    samples_value = summary.get("samples")
    if not isinstance(samples_value, list) or len(samples_value) != 20:
        fail("diagnostic summary does not contain exactly 20 sample records")
    by_sample: dict[int, dict[str, object]] = {}
    for value in samples_value:
        record = require_dict(value, "diagnostic sample")
        number = record.get("sample")
        if not isinstance(number, int) or number in by_sample:
            fail("diagnostic summary contains an invalid or duplicate sample")
        by_sample[number] = record
    if set(by_sample) != set(range(1, 21)):
        fail("diagnostic summary sample identities are not 1 through 20")
    if {
        number for number, record in by_sample.items() if record.get("classification") == "cookie_request_5_timeout"
    } != set(SAMPLES):
        fail("diagnostic timeout sample identities changed")
    for number, record in by_sample.items():
        phase_records = record.get("phaseRecords")
        if not isinstance(phase_records, list):
            fail(f"sample {number} phase records are invalid")
        if number not in SAMPLES:
            if record.get("exitCode") != 0 or record.get("classification") != "completed":
                fail(f"sample {number} no longer classifies as completed")
            if phase_records:
                fail(f"completed sample {number} unexpectedly contains phase records")
            continue
        if record.get("exitCode") != 1 or len(phase_records) != 3:
            fail(f"timeout sample {number} has an unexpected exit or phase count")
        begin, end, error = (require_dict(item, "phase record") for item in phase_records)
        if [begin.get("kind"), end.get("kind"), error.get("kind")] != [
            "stack-sample-begin",
            "stack-sample-end",
            "error",
        ]:
            fail(f"timeout sample {number} phase sequence changed")
        for item in (begin, end, error):
            if (
                item.get("phase") != "cookie-post-submit-settle"
                or item.get("operation") != "runtime.settle"
                or item.get("processOrdinal") != 4
                or not isinstance(item.get("processPid"), int)
            ):
                fail(f"timeout sample {number} phase identity changed")
        if (
            begin.get("processPid") != end.get("processPid")
            or begin.get("processPid") != error.get("processPid")
            or end.get("samplerExitCode") != 0
            or error.get("requestId") != "5"
            or error.get("expectedRequestId") != "5"
            or error.get("method") != "runtime.settle"
            or error.get("expectedMethod") != "runtime.settle"
            or error.get("reasonName") != "TimeoutError"
            or error.get("fatal") is not True
        ):
            fail(f"timeout sample {number} attribution changed")
    return by_sample


def validate_artifact_files(input_dir: Path, summary: dict[int, dict[str, object]]) -> list[ParsedSample]:
    reports = sorted(input_dir.glob("*.sample.txt"))
    if len(reports) != 9:
        fail(f"diagnostic artifact contains {len(reports)} stack reports, not nine")
    parsed: list[ParsedSample] = []
    seen: set[int] = set()
    for path in reports:
        match = re.fullmatch(
            r"sample-(\d{3})-process-004-cookie-post-submit-settle\.sample\.txt",
            path.name,
        )
        if match is None:
            fail(f"unexpected stack report name: {path.name}")
        sample = int(match.group(1))
        if sample not in SAMPLES or sample in seen:
            fail(f"unexpected or duplicate stack sample: {sample}")
        seen.add(sample)
        phase_records = summary[sample]["phaseRecords"]
        if not isinstance(phase_records, list):
            fail(f"sample {sample} phase records changed after summary validation")
        end = require_dict(phase_records[1], "stack-sample-end")
        metadata_name = str(end.get("stackMetadataArtifact"))
        if end.get("stackArtifact") != path.name:
            fail(f"sample {sample} report name does not match its phase record")
        if end.get("stackArtifactSha256") != sha256(path):
            fail(f"sample {sample} report hash does not match its phase record")
        if end.get("stackArtifactBytes") != path.stat().st_size:
            fail(f"sample {sample} report size does not match its phase record")
        metadata_path = input_dir / metadata_name
        metadata = require_dict(load_json(metadata_path), "stack metadata")
        if (
            metadata.get("schema") != "stasis-v0.3.2-macos-stack-sample-v1"
            or metadata.get("sample") != str(sample)
            or metadata.get("phase") != "cookie-post-submit-settle"
            or metadata.get("processOrdinal") != 4
            or metadata.get("processPid") != end.get("processPid")
            or metadata.get("samplerExitCode") != 0
            or metadata.get("stackArtifact") != path.name
            or metadata.get("stackArtifactBytes") != path.stat().st_size
            or metadata.get("stackArtifactSha256") != sha256(path)
            or metadata.get("releaseGateAuthority") is not False
        ):
            fail(f"sample {sample} stack metadata changed")
        parsed.append(parse_sample_report(path, sample, expected_pid=int(end["processPid"])))
    if seen != set(SAMPLES):
        fail("diagnostic artifact does not contain the expected nine sample identities")
    return sorted(parsed, key=lambda item: item.sample)


def validate_hosted_metadata(args: argparse.Namespace) -> None:
    tag = require_dict(load_json(args.tag_metadata), "tag metadata")
    tag_object = require_dict(tag.get("object"), "tag object")
    if (
        tag.get("ref") != f"refs/tags/{RELEASE_TAG}"
        or tag_object.get("type") != "commit"
        or tag_object.get("sha") != RELEASE_REVISION
    ):
        fail("immutable v0.3.2 tag identity changed")

    release = require_dict(load_json(args.release_metadata), "release metadata")
    if (
        release.get("id") != RELEASE_ID
        or release.get("tag_name") != RELEASE_TAG
        or release.get("target_commitish") != RELEASE_REVISION
        or release.get("draft") is not False
        or release.get("prerelease") is not False
        or release.get("immutable") is not True
    ):
        fail("immutable v0.3.2 release identity changed")
    assets = release.get("assets")
    if not isinstance(assets, list):
        fail("release assets are missing")
    asset_matches = [
        require_dict(asset, "release asset")
        for asset in assets
        if isinstance(asset, dict) and asset.get("id") == RELEASE_ASSET_ID
    ]
    if len(asset_matches) != 1:
        fail("immutable macOS release asset is not unique")
    release_asset = asset_matches[0]
    if (
        release_asset.get("name") != "stasis-0.3.2-macos-aarch64.tar.gz"
        or release_asset.get("size") != RELEASE_ARCHIVE_BYTES
        or release_asset.get("digest") != f"sha256:{RELEASE_ARCHIVE_SHA256}"
        or release_asset.get("state") != "uploaded"
    ):
        fail("immutable macOS release asset metadata changed")

    run = require_dict(load_json(args.run_metadata), "diagnostic run metadata")
    if (
        run.get("id") != DIAGNOSTIC_RUN_ID
        or run.get("run_attempt") != DIAGNOSTIC_RUN_ATTEMPT
        or run.get("head_sha") != DIAGNOSTIC_REVISION
        or run.get("head_branch") != "main"
        or run.get("path") != ".github/workflows/stasis-v0.3-macos-public-diagnostic.yml"
        or run.get("event") != "workflow_dispatch"
        or run.get("status") != "completed"
        or run.get("conclusion") != "failure"
    ):
        fail("diagnostic run identity changed")

    artifact = require_dict(load_json(args.artifact_metadata), "diagnostic artifact metadata")
    workflow_run = require_dict(artifact.get("workflow_run"), "artifact workflow run")
    if (
        artifact.get("id") != DIAGNOSTIC_ARTIFACT_ID
        or artifact.get("name") != DIAGNOSTIC_ARTIFACT_NAME
        or artifact.get("size_in_bytes") != DIAGNOSTIC_ARTIFACT_BYTES
        or artifact.get("digest") != DIAGNOSTIC_ARTIFACT_DIGEST
        or artifact.get("expired") is not False
        or workflow_run.get("id") != DIAGNOSTIC_RUN_ID
        or workflow_run.get("head_sha") != DIAGNOSTIC_REVISION
    ):
        fail("diagnostic artifact identity changed")
    if sha256(args.artifact_zip) != DIAGNOSTIC_ARTIFACT_DIGEST.removeprefix("sha256:"):
        fail("downloaded diagnostic artifact bytes do not match its digest")


def parse_nm(output: str) -> list[tuple[int, str, str]]:
    symbols: list[tuple[int, str, str]] = []
    for line in output.splitlines():
        match = re.match(r"^([0-9A-Fa-f]+)\s+([A-Za-z?])\s+(.+)$", line)
        if match is None:
            continue
        symbols.append((int(match.group(1), 16), match.group(2), match.group(3)))
    return symbols


def text_vmaddr(otool_output: str) -> int:
    lines = otool_output.splitlines()
    for index, line in enumerate(lines):
        if line.strip() != "segname __TEXT":
            continue
        for candidate in lines[index + 1 : index + 12]:
            match = re.match(r"\s*vmaddr\s+(0x[0-9A-Fa-f]+)\s*$", candidate)
            if match is not None:
                return int(match.group(1), 16)
    fail("could not locate the Mach-O __TEXT vmaddr")


def nm_resolution(
    offset: int,
    symbols: list[tuple[int, str, str]],
    text_base: int,
    addresses: list[int] | None = None,
) -> str:
    if addresses is None:
        addresses = [address for address, _kind, _name in symbols]
    target = text_base + offset
    index = bisect.bisect_right(addresses, target) - 1
    if index < 0:
        return ""
    address, _kind, name = symbols[index]
    delta = target - address
    return name if delta == 0 else f"{name} + 0x{delta:x}"


def useful_atos(symbol: str) -> bool:
    stripped = symbol.strip()
    return bool(stripped) and not (
        stripped.startswith("0x") or "???" in stripped or stripped.startswith("_ZN") or stripped.startswith("__ZN")
    )


def symbolize_sample(
    sample: ParsedSample,
    capture: Path,
    atos: str,
    symbols: list[tuple[int, str, str]],
    text_base: int,
    output_dir: Path,
) -> dict[str, object]:
    by_offset: dict[int, int] = {}
    for frame in sample.frames:
        existing = by_offset.setdefault(frame.offset, frame.absolute)
        if existing != frame.absolute:
            fail(f"sample {sample.sample} maps one offset to multiple addresses")
    ordered = sorted(by_offset.items())
    symbol_addresses = [address for address, _kind, _name in symbols]
    atos_by_offset: dict[int, str] = {}
    for start in range(0, len(ordered), 100):
        chunk = ordered[start : start + 100]
        argv = [
            atos,
            "-o",
            os.fspath(capture),
            "-arch",
            "arm64",
            "-l",
            f"0x{sample.load_address:x}",
            *[f"0x{absolute:x}" for _offset, absolute in chunk],
        ]
        completed = run_command(argv)
        if completed.stderr.strip():
            fail(f"atos emitted diagnostics for sample {sample.sample}: {completed.stderr.strip()}")
        lines = completed.stdout.splitlines()
        if len(lines) != len(chunk):
            fail(f"atos returned {len(lines)} lines for {len(chunk)} addresses in sample {sample.sample}")
        for (offset, _absolute), symbol in zip(chunk, lines, strict=True):
            atos_by_offset[offset] = symbol.strip()

    rows: list[str] = []
    resolutions: dict[int, dict[str, str]] = {}
    for offset, absolute in ordered:
        atos_name = atos_by_offset[offset]
        nm_name = nm_resolution(offset, symbols, text_base, symbol_addresses)
        resolved = atos_name if useful_atos(atos_name) else nm_name
        if not resolved:
            fail(f"sample {sample.sample} offset 0x{offset:x} has no atos or llvm-nm symbol")
        resolutions[offset] = {"atos": atos_name, "llvmNm": nm_name, "resolved": resolved}
        rows.append(f"0x{offset:x}\t0x{absolute:x}\t{atos_name}\t{nm_name}\t{resolved}")
    (output_dir / f"sample-{sample.sample:03d}.atos.tsv").write_text(
        "offset\tabsolute\tatos\tllvm_nm\tresolved\n" + "\n".join(rows) + "\n",
        encoding="utf-8",
    )

    def stable_frames(offsets: Iterable[int]) -> list[dict[str, str]]:
        result: list[dict[str, str]] = []
        for offset in offsets:
            absolute = by_offset[offset]
            names = resolutions[offset]
            result.append(
                {
                    "offset": f"0x{offset:x}",
                    "absolute": f"0x{absolute:x}",
                    **names,
                }
            )
        return result

    return {
        "sample": sample.sample,
        "source": sample.path.name,
        "sourceSha256": sha256(sample.path),
        "loadAddress": f"0x{sample.load_address:x}",
        "imageUuid": RELEASE_UUID,
        "stasisFrameRecords": len(sample.frames),
        "uniqueStasisAddresses": len(ordered),
        "addressArithmeticValidated": True,
        "mainFrames": stable_frames(MAIN_STABLE_OFFSETS),
        "scriptFrames": stable_frames(SCRIPT_STABLE_OFFSETS),
    }


def validate_runner_and_toolchain(args: argparse.Namespace) -> tuple[dict[str, object], dict[str, object]]:
    runner = require_dict(load_json(args.runner_metadata), "runner metadata")
    producer_environment = require_dict(runner.get("producerEnvironment"), "producer environment")
    if (
        runner.get("kernel") != "Darwin"
        or runner.get("architecture") != "arm64"
        or runner.get("arm64") != "1"
        or runner.get("macosVersion") != "15.7.7"
        or runner.get("macosBuild") != "24G720"
        or runner.get("xcodeVersion") != "Xcode 16.4"
        or runner.get("xcodeBuild") != "Build version 16F6"
        or runner.get("macosSdkVersion") != "15.5"
        or runner.get("macosDeploymentTarget") != "13.0"
        or not isinstance(runner.get("harnessRevision"), str)
        or re.fullmatch(r"[0-9a-f]{40}", str(runner.get("harnessRevision"))) is None
        or runner.get("imageVersion") != "20260727.0256.1"
        or producer_environment
        != {
            "cargoIncremental": "0",
            "rustBacktrace": "1",
            "stasisRevision": RELEASE_REVISION,
            "stasisPlatform": "macos-aarch64",
            "stasisReleaseVersion": "0.3.2",
        }
    ):
        fail("runner metadata does not match the immutable producer environment")
    toolchain = require_dict(load_json(args.toolchain_metadata), "toolchain metadata")
    if (
        toolchain.get("rustcRelease") != "1.97.1"
        or toolchain.get("rustcCommit") != "8bab26f4f68e0e26f0bb7960be334d5b520ea452"
        or toolchain.get("llvmVersion") != "22.1.6"
        or toolchain.get("cargoRelease") != "1.97.1"
        or toolchain.get("cargoCommit") != "c980f4866141969fab6254a680546a277789d6f0"
    ):
        fail("Rust/Cargo/LLVM toolchain identity changed")
    return runner, toolchain


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--input", type=Path, required=True)
    parser.add_argument("--artifact-zip", type=Path, required=True)
    parser.add_argument("--prestrip", type=Path, required=True)
    parser.add_argument("--built", type=Path, required=True)
    parser.add_argument("--release-binary", type=Path, required=True)
    parser.add_argument("--release-archive", type=Path, required=True)
    parser.add_argument("--capture-result", type=Path, required=True)
    parser.add_argument("--tag-metadata", type=Path, required=True)
    parser.add_argument("--release-metadata", type=Path, required=True)
    parser.add_argument("--run-metadata", type=Path, required=True)
    parser.add_argument("--artifact-metadata", type=Path, required=True)
    parser.add_argument("--runner-metadata", type=Path, required=True)
    parser.add_argument("--toolchain-metadata", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--atos", default="/usr/bin/atos")
    parser.add_argument("--dwarfdump", default="dwarfdump")
    parser.add_argument("--llvm-nm", required=True)
    parser.add_argument("--otool", default="otool")
    args = parser.parse_args()

    args.output.mkdir(parents=True, exist_ok=False)
    validate_hosted_metadata(args)
    runner, toolchain = validate_runner_and_toolchain(args)
    if sha256(args.release_archive) != RELEASE_ARCHIVE_SHA256:
        fail("release archive SHA-256 changed")
    if args.release_archive.stat().st_size != RELEASE_ARCHIVE_BYTES:
        fail("release archive size changed")
    for path, label in (
        (args.prestrip, "pre-strip capture"),
        (args.built, "rebuilt post-strip binary"),
        (args.release_binary, "release binary"),
    ):
        if not path.is_file() or path.stat().st_size == 0:
            fail(f"{label} is missing or empty: {path}")
    if (
        args.built.stat().st_size != RELEASE_BINARY_BYTES
        or args.release_binary.stat().st_size != RELEASE_BINARY_BYTES
        or sha256(args.built) != RELEASE_BINARY_SHA256
        or sha256(args.release_binary) != RELEASE_BINARY_SHA256
        or not files_equal(args.built, args.release_binary)
    ):
        fail("rebuilt post-strip executable is not byte-identical to the release")
    uuids = {
        "prestrip": mach_uuid(args.prestrip, args.dwarfdump),
        "built": mach_uuid(args.built, args.dwarfdump),
        "release": mach_uuid(args.release_binary, args.dwarfdump),
    }
    if set(uuids.values()) != {RELEASE_UUID}:
        fail(f"Mach-O UUIDs do not bind to the release: {uuids!r}")

    capture_result = require_dict(load_json(args.capture_result), "capture result")
    if (
        capture_result.get("schema") != "stasis-v0.3.2-macos-rust-objcopy-capture-v1"
        or capture_result.get("captureSha256") != sha256(args.prestrip)
        or capture_result.get("captureBytes") != args.prestrip.stat().st_size
        or capture_result.get("rustObjcopyArgs") != ["--strip-all", os.fspath(args.built.resolve())]
        or capture_result.get("singleTargetInvocation") is not True
        or capture_result.get("originalObjcopySha256") != capture_result.get("restoredObjcopySha256")
        or capture_result.get("releaseGateAuthority") is not False
    ):
        fail("rust-objcopy capture result does not bind the exact build target")

    nm_prestrip = run_command(
        [args.llvm_nm, "--numeric-sort", "--demangle", "--defined-only", os.fspath(args.prestrip)]
    )
    if nm_prestrip.stderr.strip():
        fail(f"llvm-nm emitted diagnostics for the pre-strip capture: {nm_prestrip.stderr.strip()}")
    symbols = parse_nm(nm_prestrip.stdout)
    local_symbols = [item for item in symbols if item[1].islower()]
    if not symbols or not local_symbols:
        fail("pre-strip capture does not contain defined local symbols")
    (args.output / "llvm-nm.demangled.txt").write_text(nm_prestrip.stdout, encoding="utf-8")
    nm_release = run_command(
        [args.llvm_nm, "--numeric-sort", "--demangle", "--defined-only", os.fspath(args.release_binary)],
        allow_failure=True,
    )
    if nm_release.returncode not in (0, 1):
        fail(f"llvm-nm could not inspect the immutable release: {nm_release.stderr.strip()}")
    if nm_release.returncode == 1 and "no symbols" not in (nm_release.stdout + nm_release.stderr).lower():
        fail(f"llvm-nm failed unexpectedly for the immutable release: {nm_release.stderr.strip()}")
    release_symbols = parse_nm(nm_release.stdout)
    release_local_symbols = [item for item in release_symbols if item[1].islower()]
    if release_local_symbols:
        fail("immutable release unexpectedly contains local symbols")
    (args.output / "release-llvm-nm.txt").write_text(nm_release.stdout + nm_release.stderr, encoding="utf-8")
    otool = run_command([args.otool, "-l", os.fspath(args.prestrip)])
    (args.output / "prestrip-otool-load-commands.txt").write_text(otool.stdout, encoding="utf-8")
    text_base = text_vmaddr(otool.stdout)

    summary_records = validate_summary(args.input)
    samples = validate_artifact_files(args.input, summary_records)
    symbolized = [
        symbolize_sample(
            sample,
            args.prestrip,
            args.atos,
            symbols,
            text_base,
            args.output,
        )
        for sample in samples
    ]

    def mapping(record: dict[str, object], key: str) -> tuple[tuple[str, str], ...]:
        frames = record[key]
        if not isinstance(frames, list):
            fail(f"symbolized {key} is not a frame list")
        return tuple((str(frame["offset"]), str(frame["resolved"])) for frame in frames if isinstance(frame, dict))

    main_mappings = [mapping(record, "mainFrames") for record in symbolized]
    script_mappings = [mapping(record, "scriptFrames") for record in symbolized]
    if len(set(main_mappings)) != 1 or len(set(script_mappings)) != 1:
        fail("stable offset symbol mappings differ across the nine reports")

    def symbol_text(record: dict[str, object], key: str) -> str:
        frames = record[key]
        if not isinstance(frames, list):
            fail(f"symbolized {key} is not a frame list")
        values: list[str] = []
        for frame in frames:
            if not isinstance(frame, dict):
                fail(f"symbolized {key} contains a non-object frame")
            values.extend(str(frame.get(name, "")) for name in ("atos", "llvmNm", "resolved"))
        return "\n".join(values).lower()

    main_names = symbol_text(symbolized[0], "mainFrames")
    script_names = symbol_text(symbolized[0], "scriptFrames")
    shell_wait_bound = "shell::run" in main_names and "wait_for_change_checked" in main_names
    script_authority_wait_bound = (
        "await_initial_pipeline_activation_authority" in script_names
        and "recv_document_control_timeout" in script_names
    )

    source_hashes = {record["source"]: record["sourceSha256"] for record in symbolized}
    write_json(args.output / "sample-source-hashes.json", source_hashes)
    result = {
        "schema": SCHEMA,
        "releaseGateAuthority": False,
        "release": {
            "revision": RELEASE_REVISION,
            "tag": RELEASE_TAG,
            "releaseId": RELEASE_ID,
            "assetId": RELEASE_ASSET_ID,
            "archiveSha256": RELEASE_ARCHIVE_SHA256,
            "archiveBytes": RELEASE_ARCHIVE_BYTES,
            "binarySha256": RELEASE_BINARY_SHA256,
            "binaryBytes": RELEASE_BINARY_BYTES,
            "uuid": RELEASE_UUID,
        },
        "build": {
            "runner": runner,
            "toolchain": toolchain,
            "rustObjcopy": capture_result,
            "prestripSha256": sha256(args.prestrip),
            "prestripBytes": args.prestrip.stat().st_size,
            "poststripSha256": sha256(args.built),
            "poststripMatchesRelease": True,
            "uuids": uuids,
            "definedSymbolCount": len(symbols),
            "localSymbolCount": len(local_symbols),
            "releaseLocalSymbolCount": len(release_local_symbols),
            "textVmaddr": f"0x{text_base:x}",
        },
        "diagnostic": {
            "runId": DIAGNOSTIC_RUN_ID,
            "attempt": DIAGNOSTIC_RUN_ATTEMPT,
            "artifactId": DIAGNOSTIC_ARTIFACT_ID,
            "artifactName": DIAGNOSTIC_ARTIFACT_NAME,
            "artifactDigest": DIAGNOSTIC_ARTIFACT_DIGEST,
            "summarySha256": DIAGNOSTIC_SUMMARY_SHA256,
        },
        "samples": symbolized,
        "stableMappings": {
            "main": dict(main_mappings[0]),
            "script": dict(script_mappings[0]),
        },
        "claims": {
            "exactReleaseAddressAuthority": True,
            "poststripWholeFileEquality": True,
            "allNineStableMappingsEqual": True,
            "allStasisFrameArithmeticValidated": True,
            "shellWaitBound": shell_wait_bound,
            "scriptAuthorityWaitBound": script_authority_wait_bound,
        },
        "observedClassification": (
            "shell-and-script-activation-authority-waits"
            if shell_wait_bound and script_authority_wait_bound
            else "exact-symbols-recovered-unexpected-stable-wait-family"
        ),
    }
    write_json(args.output / "summary.json", result)
    print(json.dumps(result, sort_keys=True))
    if not shell_wait_bound or not script_authority_wait_bound:
        fail("exact stable symbols did not bind the predeclared wait families")


if __name__ == "__main__":
    main()
