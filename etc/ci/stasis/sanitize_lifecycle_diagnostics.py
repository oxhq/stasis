#!/usr/bin/env python3
"""Extract bounded, fixed-vocabulary Stasis lifecycle diagnostics.

The input is always runner-private and may contain page data, URLs, environment values, or other
hostile text. The output contains only allow-listed structural records and compile-time lifecycle
phase tokens. In particular, this extractor recognizes phases both as direct stderr lines and
inside an SDK ``stderrTail`` whose newlines have been escaped.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import sys
import tempfile


MAX_INPUT_BYTES = 32 * 1024 * 1024
MAX_INPUT_LINES = 100_000
MAX_LINE_BYTES = 128 * 1024
MAX_OUTPUT_LINES = 4096
MAX_OUTPUT_BYTES = 256 * 1024
FOOTER_LINE_BUDGET = 6
FOOTER_BYTE_BUDGET = 512

LIFECYCLE_PHASES = frozenset(
    {
        "close_accepted",
        "engine_close_begin",
        "webview_drop_begin",
        "painter_drop_begin",
        "painter_webrender_shutdown_begin",
        "painter_webrender_shutdown_ack_observed",
        "painter_renderer_deinit_begin",
        "painter_renderer_deinit_end",
        "painter_drop_body_end",
        "webview_drop_end",
        "pre_shutdown_spin_begin",
        "pre_shutdown_spin_end",
        "servo_owner_drop_begin",
        "servo_inner_drop_begin",
        "constellation_exit_send_begin",
        "script_threads_join_begin",
        "script_threads_join_end",
        "script_threads_join_failed",
        "subsystems_shutdown_end",
        "constellation_run_end",
        "constellation_state_drop_begin",
        "constellation_state_drop_end",
        "shutdown_complete_send_begin",
        "shutdown_complete_observed",
        "constellation_join_begin",
        "constellation_join_end",
        "constellation_join_failed",
        "servo_inner_drop_body_end",
        "js_engine_drop_begin",
        "js_engine_drop_end",
        "servo_owner_drop_end",
        "engine_close_end",
        "engine_session_drop_begin",
        "engine_session_drop_end",
        "rendering_context_owner_drop_begin",
        "software_rendering_context_drop_begin",
        "software_rendering_context_drop_body_end",
        "surfman_rendering_context_drop_begin",
        "surfman_rendering_context_drop_body_end",
        "rendering_context_owner_drop_end",
        "close_response_written",
        "shell_run_end",
        "protocol_reader_join_begin",
        "protocol_reader_join_end",
        "protocol_reader_join_failed",
    }
)
PHASE_PATTERN = re.compile(rb"stasis_lifecycle_v1 phase=([a-z_]+)")


class BoundedOutput:
    def __init__(self) -> None:
        self.lines: list[str] = []
        self.encoded_bytes = 0

    def append(self, value: str, *, reserve_footer: bool = False) -> bool:
        encoded = value.encode("ascii", "strict")
        line_limit = MAX_OUTPUT_LINES - (FOOTER_LINE_BUDGET if reserve_footer else 0)
        byte_limit = MAX_OUTPUT_BYTES - (FOOTER_BYTE_BUDGET if reserve_footer else 0)
        if (
            len(self.lines) >= line_limit
            or self.encoded_bytes + len(encoded) + 1 > byte_limit
        ):
            return False
        self.lines.append(value)
        self.encoded_bytes += len(encoded) + 1
        return True


def structural_records(line: str, workspace: str) -> list[str]:
    line = line.replace(workspace, "<workspace>") if workspace else line
    records: list[str] = []

    match = re.fullmatch(
        r"session_north_star_(traced|untraced)_sample=([0-9]{3})/150", line
    )
    if match:
        return [f"north_star_lane={match.group(1)} sample={match.group(2)}/150"]

    match = re.fullmatch(
        r"session_north_star_lifecycle_sample=([0-9]{2,3})/(50|150)", line
    )
    if match:
        return [f"north_star_sample={match.group(1)}/{match.group(2)}"]

    match = re.fullmatch(
        r"StasisProcessError: Stasis exited from signal "
        r"(SIGABRT|SIGBUS|SIGILL|SIGSEGV)",
        line,
    )
    if match:
        records.append(f"stasis_process_error signal={match.group(1)}")

    if re.search(r"Redirecting call to abort\(\) to mozalloc_abort", line):
        records.append("stderr_marker=mozalloc_abort")

    match = re.fullmatch(
        r"lifecycle_fresh_process_sample=([0-9]{2,3})/(50|150) "
        r"test=ten_second_timeout_session_closes_cleanly_after_virtual_advance",
        line,
    )
    if match:
        records.append(
            f"lifecycle_sample={match.group(1)}/{match.group(2)} "
            "test=ten_second_timeout_session_closes_cleanly_after_virtual_advance"
        )

    match = re.fullmatch(
        r"\s*Finished `production-stripped` profile "
        r"\[(optimized|unoptimized)(?: \+ debuginfo)?\] target\(s\) in "
        r"([0-9]+(?:\.[0-9]+)?)s",
        line,
    )
    if match:
        records.append(
            "cargo_finished profile=production-stripped "
            f"mode={match.group(1)} seconds={match.group(2)}"
        )

    match = re.fullmatch(r"running ([0-9]+) tests?", line)
    if match:
        records.append(f"libtest_running count={match.group(1)}")

    match = re.fullmatch(
        r"test ([A-Za-z0-9_:]+) \.\.\. (ok|FAILED|ignored)"
        r"(?:, release gate: set STASIS_RELEASE_BINARY, STASIS_RELEASE_ARCHIVE, "
        r"and STASIS_RELEASE_REVISION)?",
        line,
    )
    if match and match.group(1) in {
        "release_gate_published_binary_completes_act_settle_inspect",
        "source_binary_single_close_lifecycle_is_owner_ordered",
        "ten_second_timeout_session_closes_cleanly_after_virtual_advance",
    }:
        records.append(f"test name={match.group(1)} status={match.group(2)}")

    match = re.fullmatch(
        r"test result: (ok|FAILED)\. ([0-9]+) passed; ([0-9]+) failed; "
        r"([0-9]+) ignored; ([0-9]+) measured; ([0-9]+) filtered out; "
        r"finished in ([0-9]+(?:\.[0-9]+)?)s",
        line,
    )
    if match:
        records.append(
            f"test_result status={match.group(1)} passed={match.group(2)} "
            f"failed={match.group(3)} ignored={match.group(4)} "
            f"measured={match.group(5)} filtered={match.group(6)} seconds={match.group(7)}"
        )

    return records


def sanitize(raw_path: Path, summary_path: Path, failed_gate: str, workspace: str) -> int:
    output = BoundedOutput()
    output.append("schema=2")
    output.append(f"failed_gate={failed_gate}")
    output.append("raw_log_uploaded=false")
    source_lines = 0
    omitted_lines = 0
    phase_count = 0
    input_truncated = False
    output_truncated = False
    source_status = "missing"

    try:
        size = raw_path.stat().st_size
        source_status = "available"
        start = max(0, size - MAX_INPUT_BYTES)
        input_truncated = start > 0
        with raw_path.open("rb") as source:
            source.seek(start)
            if start:
                source.readline()
            for raw_line in source:
                if source_lines >= MAX_INPUT_LINES:
                    input_truncated = True
                    break
                source_lines += 1
                if len(raw_line) > MAX_LINE_BYTES:
                    omitted_lines += 1
                    continue

                for phase_match in PHASE_PATTERN.finditer(raw_line):
                    phase = phase_match.group(1).decode("ascii", "strict")
                    if phase not in LIFECYCLE_PHASES:
                        omitted_lines += 1
                        continue
                    if output.append(
                        f"lifecycle_phase={phase}", reserve_footer=True
                    ):
                        phase_count += 1
                    else:
                        omitted_lines += 1
                        output_truncated = True

                line_bytes = raw_line.rstrip(b"\r\n")
                line = line_bytes.decode("utf-8", "replace")
                if any(ord(character) < 32 and character != "\t" for character in line):
                    omitted_lines += 1
                    continue
                for record in structural_records(line, workspace):
                    if not output.append(record, reserve_footer=True):
                        omitted_lines += 1
                        output_truncated = True
    except (OSError, ValueError, UnicodeError):
        source_status = "missing"

    output.append(f"lifecycle_phase_count={phase_count}")
    output.append(f"source_status={source_status}")
    output.append(f"source_lines={source_lines}")
    output.append(f"omitted_lines={omitted_lines}")
    output.append(f"input_truncated={'true' if input_truncated else 'false'}")
    output.append(f"output_truncated={'true' if output_truncated else 'false'}")
    summary_path.parent.mkdir(parents=True, exist_ok=True)
    with summary_path.open("w", encoding="ascii", newline="\n") as summary:
        for retained_line in output.lines:
            summary.write(retained_line + "\n")
    return 0 if source_status == "available" else 2


def self_test() -> None:
    direct = "stasis_lifecycle_v1 phase=close_accepted"
    escaped = (
        "stderrTail: 'HOSTILE_SECRET\\n"
        "stasis_lifecycle_v1 phase=painter_webrender_shutdown_ack_observed\\n"
        "stasis_lifecycle_v1 phase=painter_renderer_deinit_end\\n',"
    )
    hostile = "url=https://secret.invalid/?token=HOSTILE_SECRET env=HOSTILE_SECRET"
    unknown = "stasis_lifecycle_v1 phase=hostile_secret"
    compiler_mimic = "   Compiling HOSTILE_SECRET v1.0.0"
    signal_mimic = "StasisProcessError: Stasis exited from signal SIGHOSTILESECRET"
    test_mimic = "test HOSTILE_SECRET ... FAILED"
    oversized = "HOSTILE_SECRET" * (MAX_LINE_BYTES // len("HOSTILE_SECRET") + 2)
    payload = "\n".join(
        [
            "session_north_star_traced_sample=047/150",
            direct,
            escaped,
            hostile,
            unknown,
            compiler_mimic,
            signal_mimic,
            test_mimic,
            "Redirecting call to abort() to mozalloc_abort",
            oversized,
        ]
    ).encode("utf-8")

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        raw = root / "raw.log"
        summary = root / "summary.log"
        raw.write_bytes(payload)
        assert sanitize(raw, summary, "session_north_star_traced", str(root)) == 0
        sanitized = summary.read_text(encoding="ascii")

    expected_phases = [
        "lifecycle_phase=close_accepted",
        "lifecycle_phase=painter_webrender_shutdown_ack_observed",
        "lifecycle_phase=painter_renderer_deinit_end",
    ]
    positions = [sanitized.index(phase) for phase in expected_phases]
    assert positions == sorted(positions)
    assert "north_star_lane=traced sample=047/150" in sanitized
    assert "stderr_marker=mozalloc_abort" in sanitized
    assert "lifecycle_phase_count=3" in sanitized
    assert "HOSTILE_SECRET" not in sanitized
    assert "secret.invalid" not in sanitized
    assert "hostile_secret" not in sanitized
    assert len(sanitized.encode("ascii")) <= MAX_OUTPUT_BYTES

    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        raw = root / "raw.log"
        summary = root / "summary.log"
        raw.write_text(
            "\n".join([direct] * (MAX_OUTPUT_LINES * 2)), encoding="ascii"
        )
        assert sanitize(raw, summary, "session_north_star_traced", str(root)) == 0
        saturated = summary.read_text(encoding="ascii")

    for footer in (
        "lifecycle_phase_count=",
        "source_status=available",
        "source_lines=",
        "omitted_lines=",
        "input_truncated=false",
        "output_truncated=true",
    ):
        assert footer in saturated
    assert len(saturated.splitlines()) <= MAX_OUTPUT_LINES
    assert len(saturated.encode("ascii")) <= MAX_OUTPUT_BYTES


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    sanitize_parser = subparsers.add_parser("sanitize")
    sanitize_parser.add_argument("--input", required=True, type=Path)
    sanitize_parser.add_argument("--output", required=True, type=Path)
    sanitize_parser.add_argument(
        "--failed-gate",
        required=True,
        choices=(
            "session_north_star",
            "session_north_star_traced",
            "session_north_star_untraced",
            "controlled_mvp",
            "baseline_protocol",
            "release_artifact_gate",
            "sdk_registry_gate",
            "unknown",
        ),
    )
    sanitize_parser.add_argument("--workspace", default=os.environ.get("GITHUB_WORKSPACE", ""))
    subparsers.add_parser("self-test")
    return parser.parse_args(argv)


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    if args.command == "self-test":
        self_test()
        return 0
    return sanitize(args.input, args.output, args.failed_gate, args.workspace)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
