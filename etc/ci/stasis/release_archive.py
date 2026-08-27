#!/usr/bin/env python3
"""Build and verify the exact Stasis 0.3.0 native release archives."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import re
import shutil
import stat
import tarfile
import tempfile
import tomllib
import zlib
from pathlib import Path
from typing import BinaryIO, Iterable


RELEASE_VERSION = "0.3.0"
VERSION_RE = re.compile(re.escape(RELEASE_VERSION))
REVISION_RE = re.compile(r"[0-9a-f]{40}")
SHA256_RE = re.compile(r"[0-9a-f]{64}")
RUN_ID_RE = re.compile(r"[1-9][0-9]*")
FROZEN_V1_PROFILE = Path("profiles/controlled-webapp-v1.json")
FROZEN_V1_PROFILE_SHA256 = "6e262edf0f8be11a1cece28f68f00d59fdac68b79b0a670ac891d36998720100"
FROZEN_V2_PROFILE = Path("profiles/controlled-web-session-v1.json")
FROZEN_V2_PROFILE_SHA256 = "9b62b9245b2c6a6f9620b117da6787a18df9298be1115cbce2e6c3d5439cc41a"
CANDIDATE_V2_PROFILE = Path("profiles/controlled-web-session-v2.json")
CANDIDATE_V2_PROFILE_SHA256 = "d9855ee01844e0fae796a9925c87a936281d47fc480b9d047da74d4a3afcd989"
CANDIDATE_V2_CONTRACT = Path("docs/stasis/session-v0.3-candidate.md")
CANDIDATE_V2_CONTRACT_SHA256 = "47116a08cd3917466ca57f2a51bc74f0b4052688af878f0ee493e9a893929cc2"
PUBLIC_TOP_LEVEL_README = Path("README.md")
PUBLIC_STASIS_BOUNDARY = Path("STASIS.md")
PUBLIC_PROFILE_README = Path("profiles/README.md")
PUBLIC_TYPESCRIPT_SDK_README = Path("sdk/typescript/README.md")
PUBLIC_RELEASE_RUNBOOK = Path("docs/stasis/releases.md")
PUBLIC_RELEASE_WORKFLOW = Path(".github/workflows/stasis-package.yml")
PUBLIC_NPM_PUBLISH_WORKFLOW = Path(".github/workflows/stasis-publish-npm.yml")
MESSAGE_CHANNEL_LIMITS_SOURCE = Path("components/script/dom/globalscope/globalscope.rs")
MESSAGE_CHANNEL_BASELINE_TEST_SOURCE = Path("ports/stasis/tests/baseline_protocol.rs")
MESSAGE_CHANNEL_MULTI_PAIR_FIXTURE = Path("ports/stasis/tests/fixtures/message_channel_multi_pair.html")
MESSAGE_CHANNEL_SOURCE = Path("components/script/dom/globalscope/messagechannel.rs")
STRUCTURED_CLONE_SOURCE = Path("components/script/dom/bindings/structuredclone.rs")
MESSAGE_PORT_SOURCE = Path("components/script/dom/globalscope/messageport.rs")
INPUT_METHOD_CONTROL_SOURCE = Path("components/script/dom/document/document_embedder_controls.rs")
INPUT_METHOD_INPUT_SOURCE = Path("components/script/dom/html/form_controls/htmlinputelement.rs")
INPUT_METHOD_TEXTAREA_SOURCE = Path("components/script/dom/html/form_controls/htmltextareaelement.rs")
EVENT_SOURCE = Path("components/script/dom/event/event.rs")
FOCUS_EVENT_SOURCE = Path("components/script/dom/event/focusevent.rs")
CONTROLLED_AUTOMATION_SOURCE = Path("components/script/automation.rs")
CONTROLLED_AUTOMATION_EVENT_TARGET_SOURCE = Path("components/script/dom/event/eventtarget.rs")
CONTROLLED_AUTOMATION_INPUT_EVENT_SOURCE = Path("components/script/dom/event/inputevent.rs")
CONTROLLED_AUTOMATION_POINTER_EVENT_SOURCE = Path("components/script/dom/event/pointerevent.rs")
CONTROLLED_AUTOMATION_SUBMIT_EVENT_SOURCE = Path("components/script/dom/event/submitevent.rs")
CONTROLLED_AUTOMATION_FORM_DATA_EVENT_SOURCE = Path("components/script/dom/event/formdataevent.rs")
CONTROLLED_AUTOMATION_EVENT_FIXTURE = Path("ports/stasis/tests/fixtures/controlled_v2_form_event_timestamp.html")
CONTROLLED_CSS_ANIMATION_SOURCE = Path("components/script/animations.rs")
CONTROLLED_CSS_ANIMATION_EVENT_SOURCE = Path("components/script/dom/event/animationevent.rs")
CONTROLLED_CSS_TRANSITION_EVENT_SOURCE = Path("components/script/dom/event/transitionevent.rs")
CONTROLLED_CSS_DOCUMENT_SOURCE = Path("components/script/dom/document/document.rs")
CONTROLLED_CSS_ANIMATION_EVENT_FIXTURE = Path(
    "ports/stasis/tests/fixtures/controlled_v2_css_animation_event_timestamp.html"
)
CONTROLLED_RENDERING_SETTLEMENT_SOURCE = Path("ports/stasis/src/settle.rs")
CONTROLLED_SESSION_SHELL_SOURCE = Path("ports/stasis/src/main.rs")
CONTROLLED_IMAGE_ELEMENT_SOURCE = Path("components/script/dom/html/embedded_content/htmlimageelement.rs")
CONTROLLED_IMAGE_WINDOW_SOURCE = Path("components/script/dom/window/window.rs")
CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE = Path("components/script/event_loop/script_thread.rs")
CONTROLLED_IMAGE_MESSAGING_SOURCE = Path("components/script/messaging.rs")
CONTROLLED_IMAGE_PRODUCER_FENCE_SOURCE = Path("components/script/producer_fence.rs")
CONTROLLED_IMAGE_CACHE_SOURCE = Path("components/net/image_cache.rs")
CONTROLLED_PROFILE_WIRE_SOURCE = Path("ports/stasis/src/wire.rs")
CONTROLLED_HTTP_IMAGE_FIXTURE = Path("ports/stasis/tests/fixtures/controlled_v2_image_http.html")
CONTROLLED_HTTP_IMAGE_MULTIPART_FIXTURE = Path("ports/stasis/tests/fixtures/controlled_v2_image_http_multipart.html")
CONTROLLED_INLINE_SVG_SOURCE = Path("components/script/dom/svg/svgsvgelement.rs")
CONTROLLED_INLINE_SVG_FIXTURE = Path("ports/stasis/tests/fixtures/controlled_v2_inline_svg.html")
CONTROLLED_INLINE_SVG_ADVANCED_FIXTURE = Path("ports/stasis/tests/fixtures/controlled_v2_inline_svg_advanced.html")
EXECUTION_LIMITS_SOURCE = Path("components/timers/lib.rs")
CONTROLLED_INPUT_METHOD_EMBEDDER_SUMMARY = (
    "controlledTopLevelSingleLineTextInputMethodPresentationWithoutVirtualKeyboard"
)
CONTROLLED_INPUT_METHOD_PRODUCT_SURFACE = (
    "controlled_top_level_single_line_text_input_method_presentation_suppression_without_virtual_keyboard"
)
CONTROLLED_FOCUS_EVENT_TIMESTAMP_PRODUCT_SURFACE = (
    "controlled_top_level_engine_focus_event_timestamp_from_document_clock"
)
CONTROLLED_AUTOMATION_EVENT_TIMESTAMP_PRODUCT_SURFACE = (
    "controlled_top_level_synchronous_public_automation_event_timestamps_from_document_clock"
)
CONTROLLED_CSS_ANIMATION_EVENT_TIMESTAMP_PRODUCT_SURFACE = (
    "controlled_public_non_auxiliary_top_level_internal_CSS_animation_event_timestamps_from_document_clock"
)
CONTROLLED_IMAGE_ELEMENT_PRODUCT_SURFACE = "bounded_controlled_top_level_direct_data_svg_HTMLImageElement_completion"
CONTROLLED_HTTP_IMAGE_ELEMENT_PRODUCT_SURFACE = (
    "initial_url_and_retained_ownership_bounded_controlled_top_level_direct_http_https_HTMLImageElement_completion"
)
CONTROLLED_INLINE_SVG_PRODUCT_SURFACE = "bounded_controlled_top_level_internal_serialized_data_svg_inline_rendering"
CONTROLLED_COOKIE_EXPIRY_PRODUCT_SURFACE = (
    "controlled_in_memory_persistent_cookie_expiry_with_explicit_v2_state_portability"
)
CONTROLLED_COOKIE_SAME_SITE_PRODUCT_SURFACE = "bounded_schemeful_SameSite_request_cookie_selection"
CONTROLLED_COOKIE_SAME_SITE_RESPONSE_PRODUCT_SURFACE = "bounded_schemeful_SameSite_response_cookie_storage"

BINARY_NAME = "stasis"
THIRD_PARTY_NAME = "THIRD_PARTY_LICENSES.html"
PLATFORM_CONTRACTS: dict[str, dict[str, str]] = {
    "linux-x86_64": {
        "display_name": "Linux x86_64",
        "operating_system": "Linux",
        "architecture": "x86_64",
        "abi": "GNU/Linux with glibc 2.35 or newer",
        "install_note": (
            "This executable targets x86_64 GNU/Linux with glibc 2.35 or newer (the Ubuntu 22.04 compatibility floor)."
        ),
        "dependency_note": (
            "External runtime baseline: Ubuntu 22.04-compatible x86_64 userspace with "
            "glibc 2.35 or newer; system shared libraries are resolved by the GNU/Linux "
            "dynamic loader."
        ),
    },
    "macos-aarch64": {
        "display_name": "macOS Apple Silicon",
        "operating_system": "macOS",
        "architecture": "arm64",
        "abi": "macOS arm64",
        "install_note": ("This macOS arm64 executable is unsigned and is not Apple-notarized."),
        "dependency_note": ("External runtime dependencies: Apple system libraries and frameworks supplied by macOS."),
    },
}
SOURCE_ASSETS = {
    "controlled-web-session-v2.json": CANDIDATE_V2_PROFILE,
    "LICENSE": Path("LICENSE"),
    "LICENSE_WHATWG_SPECS": Path("LICENSE_WHATWG_SPECS"),
    "STASIS_UPSTREAM.toml": Path("STASIS_UPSTREAM.toml"),
    THIRD_PARTY_NAME: Path("resources/resource_protocol/license.html"),
    "session-v0.3-candidate.md": CANDIDATE_V2_CONTRACT,
}
GENERATED_ASSETS = {
    "INSTALL.txt",
    "NATIVE-LIBRARIES.txt",
    "README.md",
    "SOURCE.txt",
    "VERSION.txt",
}
EXPECTED_FILES = {BINARY_NAME, *SOURCE_ASSETS, *GENERATED_ASSETS}
MAX_COMPRESSED_ARCHIVE_BYTES = 512 * 1024 * 1024
MAX_BINARY_MEMBER_BYTES = 512 * 1024 * 1024
MAX_TEXT_MEMBER_BYTES = 8 * 1024 * 1024
MAX_UNCOMPRESSED_ARCHIVE_BYTES = 600 * 1024 * 1024
DECOMPRESSION_CHUNK_BYTES = 4 * 1024 * 1024
GATE_TEST = "release_gate_published_binary_completes_act_settle_inspect"
GATE_SUCCESS = "test result: ok. 1 passed; 0 failed; 0 ignored;"
GATE_NAME = "act-settle-inspect"
GATE_PROOF_SCHEMA = 2
GATE_RECORD_PREFIX = "[RELEASE ARTIFACT] "
STASIS_REPOSITORY_IDENTITY = "https://github.com/oxhq/stasis.git"
UPSTREAM_IDENTITIES = {
    "servo_repository": "https://github.com/servo/servo.git",
    "servo_revision": "0d579bd5aab6df3764fad805427254751632a6e4",
    "pliego_repository": "https://github.com/oxhq/pliego.git",
    "pliego_revision": "556c774242b272b11bc60999449c5debff1ad20f",
    "pliego_servo_merge_base": "313b6d5ecc113b08010ce434140db3ca5abcc71c",
}


class ReleaseError(RuntimeError):
    pass


def reject_duplicate_json_members(pairs: list[tuple[str, object]]) -> dict[str, object]:
    result: dict[str, object] = {}
    for name, value in pairs:
        if name in result:
            raise ValueError(f"duplicate JSON object member {name!r}")
        result[name] = value
    return result


def reject_nonfinite_json_number(value: str) -> None:
    raise ValueError(f"non-finite JSON number {value!r}")


def strict_json_loads(document: str, description: str) -> object:
    try:
        return json.loads(
            document,
            object_pairs_hook=reject_duplicate_json_members,
            parse_constant=reject_nonfinite_json_number,
        )
    except (json.JSONDecodeError, ValueError) as error:
        raise ReleaseError(f"{description} is invalid strict JSON: {error}") from error


def require_fullmatch(pattern: re.Pattern[str], value: str, field: str) -> str:
    if pattern.fullmatch(value) is None:
        raise ReleaseError(f"invalid {field}: {value!r}")
    return value


def validate_identity(version: str, platform: str, revision: str, repository: str) -> None:
    require_fullmatch(VERSION_RE, version, "version")
    if platform not in PLATFORM_CONTRACTS:
        raise ReleaseError(f"unsupported platform {platform!r}; expected one of {sorted(PLATFORM_CONTRACTS)}")
    require_fullmatch(REVISION_RE, revision, "revision")
    if not repository.startswith("https://") or repository.endswith("/"):
        raise ReleaseError(f"repository must be an https URL without a trailing slash: {repository!r}")


def platform_contract(platform: str) -> dict[str, str]:
    contract = PLATFORM_CONTRACTS.get(platform)
    if contract is None:
        raise ReleaseError(f"unsupported platform {platform!r}; expected one of {sorted(PLATFORM_CONTRACTS)}")
    return contract


def expected_source_identities(revision: str) -> dict[str, str]:
    require_fullmatch(REVISION_RE, revision, "revision")
    return {
        **UPSTREAM_IDENTITIES,
        "stasis_repository": STASIS_REPOSITORY_IDENTITY,
        "stasis_revision": revision,
    }


def verify_upstream_manifest(filename: Path) -> None:
    try:
        manifest = tomllib.loads(filename.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as error:
        raise ReleaseError(f"cannot parse Stasis upstream manifest: {error}") from error
    if manifest != UPSTREAM_IDENTITIES:
        raise ReleaseError("STASIS_UPSTREAM.toml does not contain the exact release source identities")


def bundle_name(version: str, platform: str) -> str:
    return f"stasis-{version}-{platform}"


def release_asset_names(version: str, platform: str) -> dict[str, str]:
    bundle = bundle_name(version, platform)
    archive = f"{bundle}.tar.gz"
    return {
        "archive": archive,
        "archive_sha256": f"{archive}.sha256",
        "binary_sha256": f"{bundle}.binary.sha256",
        "gate_proof": f"{bundle}-act-settle-inspect.json",
    }


def source_text(repository: str, revision: str) -> str:
    return f"Repository: {repository}\nSource: {repository}/commit/{revision}\nRevision: {revision}\n"


def version_text(version: str, platform: str, revision: str) -> str:
    return f"stasis {version}\nplatform {platform}\nrevision {revision}\n"


def install_text(version: str, platform: str) -> str:
    names = release_asset_names(version, platform)
    contract = platform_contract(platform)
    return (
        f"1. Verify {names['archive_sha256']} before extracting this archive.\n"
        f"2. Extract {names['archive']} and put the stasis executable on PATH.\n"
        f"3. Optionally verify the extracted executable with {names['binary_sha256']}.\n"
        "4. Read README.md and NATIVE-LIBRARIES.txt for the exact platform contract.\n"
        f"5. {contract['install_note']}\n"
    )


def readme_text(version: str, platform: str, repository: str) -> str:
    contract = platform_contract(platform)
    return (
        f"# Stasis {version}\n\n"
        "Stasis is a controlled Servo runtime for deterministic web-application "
        "automation within its declared support profile.\n\n"
        f"- Artifact: `{platform}`\n"
        f"- Platform: {contract['display_name']}\n"
        f"- Operating system: {contract['operating_system']}\n"
        f"- Architecture: {contract['architecture']}\n"
        f"- ABI: {contract['abi']}\n"
        f"- Source: {repository}\n\n"
        "Start with INSTALL.txt. NATIVE-LIBRARIES.txt records the native runtime "
        "dependency boundary for this exact artifact. "
        "controlled-web-session-v2.json and session-v0.3-candidate.md record the "
        "stable execution-profile boundary shipped with these bytes.\n"
    )


def native_libraries_text(version: str, platform: str) -> str:
    contract = platform_contract(platform)
    return (
        f"Artifact: {bundle_name(version, platform)}\n"
        f"Operating system: {contract['operating_system']}\n"
        f"Architecture: {contract['architecture']}\n"
        f"ABI: {contract['abi']}\n"
        "Separate bundled native-library files: none\n"
        f"{contract['dependency_note']}\n"
    )


def sha256_file(filename: Path) -> str:
    digest = hashlib.sha256()
    with filename.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def files_equal(first: Path, second: Path) -> bool:
    if first.stat().st_size != second.stat().st_size:
        return False
    with first.open("rb") as first_handle, second.open("rb") as second_handle:
        while True:
            first_chunk = first_handle.read(1024 * 1024)
            second_chunk = second_handle.read(1024 * 1024)
            if first_chunk != second_chunk:
                return False
            if not first_chunk:
                return True


def write_sidecar(filename: Path, digest: str, label: str) -> None:
    require_fullmatch(SHA256_RE, digest, "SHA-256")
    filename.write_text(f"{digest}  {label}\n", encoding="ascii", newline="\n")


def parse_sidecar(filename: Path, expected_label: str) -> str:
    try:
        content = filename.read_text(encoding="ascii")
    except (OSError, UnicodeError) as error:
        raise ReleaseError(f"cannot read checksum sidecar {filename}: {error}") from error
    match = re.fullmatch(r"([0-9a-f]{64})  ([^\r\n]+)\n", content)
    if match is None or match.group(2) != expected_label:
        raise ReleaseError(f"checksum sidecar {filename.name} must contain one canonical entry for {expected_label}")
    return match.group(1)


def require_regular_file(filename: Path, description: str) -> None:
    if filename.is_symlink() or not filename.is_file():
        raise ReleaseError(f"{description} is not a regular file: {filename}")


def verify_frozen_v1_profile(source_root: Path) -> dict[str, str]:
    filename = source_root / FROZEN_V1_PROFILE
    require_regular_file(filename, "frozen controlled-webapp-v1 profile")
    actual = sha256_file(filename)
    if actual != FROZEN_V1_PROFILE_SHA256:
        raise ReleaseError(
            f"frozen controlled-webapp-v1 profile SHA-256 differs: expected {FROZEN_V1_PROFILE_SHA256}, got {actual}"
        )
    return {"path": FROZEN_V1_PROFILE.as_posix(), "sha256": actual}


def verify_frozen_v2_profile(source_root: Path) -> dict[str, str]:
    filename = source_root / FROZEN_V2_PROFILE
    require_regular_file(filename, "frozen controlled-web-session-v1 profile")
    actual = sha256_file(filename)
    if actual != FROZEN_V2_PROFILE_SHA256:
        raise ReleaseError(
            "frozen controlled-web-session-v1 profile SHA-256 differs: "
            f"expected {FROZEN_V2_PROFILE_SHA256}, got {actual}"
        )
    return {"path": FROZEN_V2_PROFILE.as_posix(), "sha256": actual}


def require_json_object(value: object, description: str) -> dict[str, object]:
    if type(value) is not dict:
        raise ReleaseError(f"{description} must be a JSON object")
    return value


def require_expected_fields(
    document: dict[str, object],
    expected: dict[str, object],
    description: str,
) -> None:
    for field, expected_value in expected.items():
        if document.get(field) != expected_value:
            raise ReleaseError(f"{description} {field} must be {expected_value!r}")


def require_exact_fields(
    document: dict[str, object],
    expected: dict[str, object],
    description: str,
) -> None:
    if set(document) != set(expected):
        raise ReleaseError(f"{description} must contain exactly {sorted(expected)!r}")
    require_expected_fields(document, expected, description)


def require_source_fragments_in_order(
    source: str,
    fragments: Iterable[str],
    description: str,
) -> None:
    cursor = 0
    for fragment in fragments:
        position = source.find(fragment, cursor)
        if position < 0:
            raise ReleaseError(f"cannot locate ordered {description} source fragment {fragment!r}")
        cursor = position + len(fragment)


def verify_message_port_router_source(source: str) -> None:
    capacity_start = source.find("fn controlled_local_channel_capacity_admitted(")
    retention_start = source.find("fn controlled_local_port_retains_native_capacity(", capacity_start)
    retention_end = source.find("enum MessagePortRouteDisposition", retention_start)
    if min(capacity_start, retention_start, retention_end) < 0:
        raise ReleaseError("cannot locate controlled-local retained-entry capacity boundary")
    require_source_fragments_in_order(
        source[capacity_start:retention_start],
        (
            "retained_ports",
            ".checked_add(2)",
            "count <= MAX_CONTROLLED_LOCAL_MESSAGE_PORTS",
        ),
        "controlled-local two-entry pair admission",
    )
    require_source_fragments_in_order(
        source[retention_start:retention_end],
        (
            "_explicitly_closed: bool",
            "provenance == MessagePortProvenance::ControlledLocal",
        ),
        "controlled-local one-ended terminal entry retention",
    )
    require_source_fragments_in_order(
        source,
        (
            "Self::ControlledLocalOnly => None",
            "Self::External(router_id) => Some(router_id)",
        ),
        "controlled-local router ownership",
    )
    registration_start = source.find("if let MessagePortState::UnManaged = &*current_state")
    registration_end = source.find(
        "if let MessagePortState::Managed(ownership, message_ports)",
        registration_start,
    )
    if registration_start < 0 or registration_end < 0:
        raise ReleaseError("cannot locate MessagePort registration ownership block")
    registration = source[registration_start:registration_end]
    local_start = registration.find("MessagePortProvenance::ControlledLocal => {")
    external_start = registration.find("MessagePortProvenance::ExternalCapable => {")
    if local_start < 0 or external_start < 0 or local_start >= external_start:
        raise ReleaseError("cannot separate controlled-local and external router registration")
    local_registration = registration[local_start:external_start]
    external_registration = registration[external_start:]
    if "MessagePortRouterOwnership::ControlledLocalOnly" not in local_registration:
        raise ReleaseError("controlled-local MessagePort registration lost typed ownership")
    if "NewMessagePortRouter" in local_registration:
        raise ReleaseError("controlled-local MessagePort registration creates a router")
    if "NewMessagePortRouter" not in external_registration:
        raise ReleaseError("external MessagePort registration lost its inherited router")
    require_source_fragments_in_order(
        source[registration_end:],
        (
            "if !ownership.admits(provenance)",
            "self.latch_controlled_local_rejection();",
        ),
        "mixed MessagePort provenance rejection",
    )


def verify_controlled_local_pending_projection_source(source: str) -> None:
    facts_start = source.find("struct ControlledLocalPortPendingFacts")
    pair_helper_start = source.find("fn controlled_local_port_pending_source(")
    pair_helper_end = source.find("fn add_controlled_local_queued_message(", pair_helper_start)
    if facts_start < 0 or pair_helper_start < 0 or pair_helper_end < 0:
        raise ReleaseError("cannot locate controlled-local pair projection helper")
    require_source_fragments_in_order(
        source[facts_start:pair_helper_start],
        (
            "has_buffered_messages: bool",
            "has_queued_messages: bool",
            "!self.has_buffered_messages",
            "!self.has_queued_messages",
        ),
        "controlled-local exact work-presence facts",
    )
    require_source_fragments_in_order(
        source[pair_helper_start:pair_helper_end],
        (
            "if controlled_local_port_is_quiescent(id, port, peer)",
            "return None;",
            "let Some((peer_id, peer)) = peer else",
            "return Some(id);",
            "let is_live_reciprocal_pair =",
            "port.dom_peer == Some(peer_id)",
            "port.implementation_peer == Some(peer_id)",
            "peer.dom_peer == Some(id)",
            "peer.implementation_peer == Some(id)",
            "if !is_live_reciprocal_pair",
            "return Some(id);",
            "let pair_identity = std::cmp::min(id, peer_id);",
            "(id == pair_identity).then_some(pair_identity)",
        ),
        "controlled-local reciprocal-pair pending projection",
    )

    association_start = pair_helper_end
    association_end = source.find("#[cfg(test)]", association_start)
    if association_end < 0:
        raise ReleaseError("cannot locate controlled-local queued-association helpers")
    require_source_fragments_in_order(
        source[association_start:association_end],
        (
            "fn add_controlled_local_queued_message(",
            "queued_messages.entry(port_id)",
            "Entry::Vacant(entry)",
            "entry.insert(1)",
            "Entry::Occupied(mut entry)",
            "entry.get().checked_add(1)",
            "fn take_controlled_local_queued_message(",
            "let Entry::Occupied(mut entry) = queued_messages.entry(port_id)",
            "0 =>",
            "entry.remove()",
            "false",
            "1 =>",
            "entry.remove()",
            "true",
            "*entry.get_mut() = count - 1",
            "fn controlled_local_message_accounting_total(",
            ".chain(buffered_counts)",
            ".try_fold(0usize, usize::checked_add)",
            "fn controlled_local_message_accounting_reconciles(",
            "== Some(retained_messages)",
        ),
        "controlled-local exact destination association and reconciliation",
    )

    unit_test_start = source.find("fn queued_message_associations_are_exact_and_reconcile_with_native_buffers()")
    unit_test_end = source.find("\n}\n\nimpl Drop for AutoCloseWorker", unit_test_start)
    if unit_test_start < 0 or unit_test_end < 0:
        raise ReleaseError("cannot locate controlled-local accounting unit proof")
    require_source_fragments_in_order(
        source[unit_test_start:unit_test_end],
        (
            "add_controlled_local_queued_message(&mut queued, second)",
            "add_controlled_local_queued_message(&mut queued, first)",
            "assert_eq!(queued.get(&first), Some(&1))",
            "assert_eq!(queued.get(&second), Some(&2))",
            "controlled_local_message_accounting_reconciles(",
            "assert!(!controlled_local_message_accounting_reconciles(",
            "take_controlled_local_queued_message(&mut queued, second)",
            "assert!(!queued.contains_key(&second))",
            "assert!(queued.is_empty())",
            "[usize::MAX].into_iter()",
            "None",
        ),
        "controlled-local exact accounting unit proof",
    )

    require_source_fragments_in_order(
        source,
        (
            "controlled_local_queued_message_counts: RefCell<FxHashMap<MessagePortId, usize>>",
            "controlled_local_queued_message_counts: RefCell::new(FxHashMap::default())",
            "fn associate_controlled_local_queued_message(&self, port_id: MessagePortId)",
            "add_controlled_local_queued_message(",
            "fn move_controlled_local_queued_message_to_buffer(&self, port_id: MessagePortId)",
            "take_controlled_local_queued_message(",
            "fn finish_controlled_local_queued_message(&self, port_id: MessagePortId)",
            "self.release_controlled_local_messages(1)",
            "self.controlled_local_queued_message_counts",
            ".borrow_mut()",
            ".clear()",
        ),
        "controlled-local queued-association lifecycle",
    )

    inventory_start = source.find("pub(crate) fn pending_persistent_sources(")
    inventory_end = source.find("pub(crate) fn track_event_source(", inventory_start)
    if inventory_start < 0 or inventory_end < 0:
        raise ReleaseError("cannot locate controlled persistent-source inventory")
    require_source_fragments_in_order(
        source[inventory_start:inventory_end],
        (
            "let retained_controlled_local_messages =",
            "self.controlled_local_retained_message_count.get()",
            "let message_port_state = self.message_port_state.borrow()",
            "let queued_controlled_local_messages =",
            "self.controlled_local_queued_message_counts.borrow()",
            "let controlled_local_accounting = (||",
            "let MessagePortState::Managed(ownership, message_ports)",
            "retained_controlled_local_messages == 0",
            "queued_controlled_local_messages.is_empty()",
            "!ownership.admits(port.provenance)",
            "queued_controlled_local_messages",
            ".any(|(id, count)|",
            "*count == 0",
            "message_ports",
            ".get(id)",
            "port.provenance != MessagePortProvenance::ControlledLocal",
            "buffered_counts.push(port_impl.buffered_message_count())",
            "controlled_local_message_accounting_reconciles(",
            "retained_controlled_local_messages",
            "queued_controlled_local_messages.values().copied()",
            "buffered_counts.into_iter()",
            "if controlled_local_accounting.is_none()",
            "self.latch_controlled_local_rejection()",
            "PendingPersistentSourceObservationError::ControlledLocalMessageAccountingMismatch",
            "if let MessagePortState::Managed(_, message_ports)",
            "let queued_message_count = queued_controlled_local_messages",
            ".get(id)",
            "let pending_source = match port.provenance",
            "has_queued_messages: queued_message_count > 0",
            "has_queued_messages: queued_controlled_local_messages",
            ".get(&peer_id)",
            "controlled_local_port_pending_source(*id, facts, peer)",
            "if let Some(pending_id) = pending_source",
            "PendingPersistentSourceStableId::MessagePort(pending_id)",
        ),
        "controlled-local exact retained-message source inventory",
    )
    gc_start = source.find("pub(crate) fn perform_a_message_port_garbage_collection_checkpoint(&self)")
    gc_end = source.find("/// Remove broadcast-channels that are closed.", gc_start)
    if gc_start < 0 or gc_end < 0:
        raise ReleaseError("cannot locate controlled-local MessagePort GC checkpoint")
    require_source_fragments_in_order(
        source[gc_start:gc_end],
        (
            "let retained_controlled_local_messages =",
            "let retain_closed_controlled_local_tombstones =",
            "MessagePortRouterOwnership::ControlledLocalOnly",
            "retained_controlled_local_messages > 0",
            "if !retain_closed_controlled_local_tombstones",
            "message_ports.remove(&id)",
            "self.remove_message_ports_router()",
        ),
        "controlled-local closed-port tombstone retention",
    )


def verify_controlled_local_fifo_source(source: str) -> None:
    post_start = source.find("pub(crate) fn post_messageport_msg(")
    queue_start = source.find("fn queue_message_port_route(", post_start)
    if post_start < 0 or queue_start < 0:
        raise ReleaseError("cannot locate controlled-local MessagePort post routing")
    require_source_fragments_in_order(
        source[post_start:queue_start],
        (
            "self.reserve_controlled_local_message()?",
            "let incoming = if let MessagePortState::Managed",
            ".get_mut(&entangled_id)",
            ".handle_controlled_local_incoming(task)",
            "Some(MessagePortIncomingResult::Dispatch(task))",
            "if !self.associate_controlled_local_queued_message(entangled_id)",
            "self.release_controlled_local_messages(1)",
            "return Err(self.controlled_local_rejection(",
            "self.queue_message_port_route(",
            "Some(MessagePortIncomingResult::Buffered) => {}",
            "Some(MessagePortIncomingResult::Dropped)",
            "self.release_controlled_local_messages(1)",
            "return Err(self.controlled_local_rejection(",
            "return Ok(())",
        ),
        "controlled-local exact-destination admission before task queueing",
    )
    start_begin = source.find("pub(crate) fn start_message_port(")
    start_end = source.find("pub(crate) fn close_message_port(", start_begin)
    if start_begin < 0 or start_end < 0:
        raise ReleaseError("cannot locate controlled-local MessagePort start routing")
    controlled_start = source.find("MessagePortProvenance::ControlledLocal => {", start_begin, start_end)
    if controlled_start < 0:
        raise ReleaseError("cannot locate controlled-local MessagePort start branch")
    require_source_fragments_in_order(
        source[controlled_start:start_end],
        (
            "for task in message_buffer",
            "if self.associate_controlled_local_queued_message(*port_id)",
            "self.queue_message_port_route(",
            "MessagePortTaskIngress::ControlledLocal",
            "self.release_controlled_local_messages(1)",
        ),
        "controlled-local native-buffer FIFO drain",
    )

    route_start = source.find("fn route_task_to_port(")
    route_end = source.find("pub(crate) fn maybe_add_pending_ports(", route_start)
    if route_start < 0 or route_end < 0:
        raise ReleaseError("cannot locate controlled-local MessagePort route task")
    require_source_fragments_in_order(
        source[route_start:route_end],
        (
            "if ingress == MessagePortTaskIngress::ControlledLocal",
            "&& !self.has_controlled_local_queued_message(port_id)",
            "self.latch_controlled_local_rejection()",
            "MessagePortRouteDisposition::Accept",
            "self.finish_controlled_local_queued_message(port_id)",
            "MessagePortRouteDisposition::DropClosedControlledLocal",
            "self.finish_controlled_local_queued_message(port_id)",
            "MessagePortRouteDisposition::RejectControlledLocalEscape",
            "self.finish_controlled_local_queued_message(port_id)",
            "let (should_dispatch, retains_controlled_reservation)",
            "MessagePortIncomingResult::Buffered => (None, true)",
            "MessagePortIncomingResult::Dropped => (None, false)",
            "if retains_controlled_reservation",
            "self.move_controlled_local_queued_message_to_buffer(port_id)",
            "self.finish_controlled_local_queued_message(port_id)",
        ),
        "controlled-local exact queued terminal and move-to-buffer transitions",
    )


def verify_controlled_local_multi_pair_proof_source(
    baseline_source: str,
    fixture_source: str,
) -> None:
    require_source_fragments_in_order(
        fixture_source,
        (
            "const queued = new MessageChannel()",
            "const buffered = new MessageChannel()",
            "queued.port1.onmessage =",
            'buffered.port1.addEventListener("message"',
            'queued.port2.postMessage("one")',
            'buffered.port2.postMessage("two")',
            "globalThis.multiPairChannels.buffered.port1.start()",
        ),
        "controlled-local queued-plus-buffered multi-pair fixture",
    )
    test_start = baseline_source.find("fn controlled_multi_pair_pending_distinguishes_queued_and_buffered_owners()")
    test_end = baseline_source.find(
        "fn controlled_local_message_channel_recursion_uses_the_shared_control_turn_budget()",
        test_start,
    )
    if test_start < 0 or test_end < 0:
        raise ReleaseError("cannot locate controlled-local multi-pair native proof")
    require_source_fragments_in_order(
        baseline_source[test_start:test_end],
        (
            '"profile": "controlled-web-session-v2"',
            "MULTI_PAIR_MESSAGE_CHANNEL_FIXTURE",
            '"id": "prime-multi-pair-message-channel"',
            '"method": "runtime.pending"',
            'source["openEnded"]["reason"] == "message_port"',
            "assert_eq!(\n        message_port_sources, 2",
            "one queued pair plus one buffered pair must project two distinct MessagePort owners",
            '"id": "start-buffered-multi-pair-message-channel"',
            '"method": "runtime.settle"',
            'settled["result"]["outcome"], "quiescent"',
            'settled["result"]["snapshot"]["sources"]',
            '"queued:one>buffered:two"',
        ),
        "controlled-local two-owner multi-pair native proof",
    )


def verify_port_backed_transfer_preflight_source(source: str) -> None:
    admission_start = source.find("fn require_port_backed_transfer_admission(")
    write_start = source.find("pub(crate) fn write(", admission_start)
    if admission_start < 0 or write_start < 0:
        raise ReleaseError("cannot locate the port-backed transfer preflight implementation")
    admission = source[admission_start:write_start]
    require_source_fragments_in_order(
        admission,
        (
            "root_from_object::<MessagePort>",
            "controlled_local_execution_profile_selected()",
            "root_from_object::<ReadableStream>",
            "controlled_local_execution_profile_selected()",
            "root_from_object::<WritableStream>",
            "controlled_local_execution_profile_selected()",
            "root_from_object::<TransformStream>",
            "controlled_local_execution_profile_selected()",
        ),
        "port-backed transfer interface",
    )
    read_start = source.find("pub(crate) fn read(", write_start)
    if read_start < 0:
        raise ReleaseError("cannot locate structured-clone read after write")
    require_source_fragments_in_order(
        source[write_start:read_start],
        (
            "preflight_transfer_entries",
            "require_port_backed_transfer_admission(cx, object)",
            "transfer.safe_to_jsval",
            "JS_WriteStructuredClone",
        ),
        "complete-list transfer preflight",
    )


def verify_message_port_post_transfer_source(source: str) -> None:
    post_start = source.find("fn post_message_impl(")
    post_end = source.find("pub(crate) fn cross_realm_transform_send_error", post_start)
    if post_start < 0 or post_end < 0:
        raise ReleaseError("cannot locate MessagePort postMessage implementation")
    require_source_fragments_in_order(
        source[post_start:post_end],
        (
            "if self.detached.get()",
            "return Ok(())",
            "let incumbent = GlobalScope::incumbent()",
            ".require_message_port_post(",
            "self.message_port_id",
            "transfer.is_empty()",
            "incumbent.as_deref()",
            "let ports = transfer",
            "structuredclone::write(cx, message, Some(transfer))?",
            ".require_message_port_payload(self.message_port_id, &data)?",
            ".post_messageport_msg(*self.message_port_id(), task)?",
        ),
        "MessagePort postMessage transfer-list admission before detachment or dispatch",
    )


def verify_message_channel_incumbent_authority_source(
    global_scope_source: str,
    message_channel_source: str,
) -> None:
    profile_start = global_scope_source.find("fn controlled_local_profile_enabled(&self)")
    constructor_start = global_scope_source.find("pub(crate) fn admit_message_channel_constructor(", profile_start)
    post_start = global_scope_source.find("pub(crate) fn require_message_port_post(", constructor_start)
    payload_start = global_scope_source.find("pub(crate) fn require_message_port_payload(", post_start)
    if min(profile_start, constructor_start, post_start, payload_start) < 0:
        raise ReleaseError("cannot locate controlled-local incumbent authority implementation")
    require_source_fragments_in_order(
        global_scope_source[profile_start:constructor_start],
        (
            "controlled_local_execution_profile_selected()",
            ".downcast::<Window>()",
            ".is_some_and(ScriptThread::current_controlled_top_level_target_matches)",
            "fn controlled_local_caller_matches_owner(&self, caller: &GlobalScope)",
            "std::ptr::from_ref(self)",
            "self.pipeline_id()",
            "self.webview_id()",
            "std::ptr::from_ref(caller)",
            "caller.pipeline_id()",
            "caller.webview_id()",
        ),
        "controlled-local exact target and caller identity",
    )
    require_source_fragments_in_order(
        global_scope_source[constructor_start:post_start],
        (
            "incumbent: Option<&GlobalScope>",
            "if !self.controlled_local_profile_enabled()",
            "self.require_external_subscription()?",
            "incumbent.is_none_or(|caller| !self.controlled_local_caller_matches_owner(caller))",
            "self.controlled_local_rejection(",
            '"MessageChannel construction crosses the controlled-local global boundary"',
            "controlled_local_channel_capacity_admitted(retained_local_ports)",
            "Ok(true)",
        ),
        "MessageChannel exact incumbent admission before publication",
    )
    require_source_fragments_in_order(
        global_scope_source[post_start:payload_start],
        (
            "incumbent: Option<&GlobalScope>",
            "self.controlled_local_profile_enabled()",
            "incumbent.is_some_and(|caller| self.controlled_local_caller_matches_owner(caller))",
            "transfer_list_is_empty",
            "self.controlled_local_pair_is_well_formed(port_id)",
            "self.controlled_local_port_is_proven_terminal(port_id)",
            '"MessagePort post crosses the controlled-local boundary"',
        ),
        "MessagePort exact incumbent pre-clone admission",
    )

    constructor = message_channel_source.find("fn Constructor(")
    port1 = message_channel_source.find("fn Port1(", constructor)
    if constructor < 0 or port1 < 0:
        raise ReleaseError("cannot locate MessageChannel WebIDL constructor")
    require_source_fragments_in_order(
        message_channel_source[constructor:port1],
        (
            "let incumbent = GlobalScope::incumbent()",
            "global.admit_message_channel_constructor(incumbent.as_deref())?",
            "MessageChannel::new(cx, global, proto, controlled_local)",
        ),
        "MessageChannel incumbent resolution before pair construction",
    )


def verify_controlled_animation_scheduler_liveness_source(source: str) -> None:
    ready_start = source.find("fn rendering_ready(pending: &RawPendingSnapshot)")
    demand_start = source.find("fn has_finite_rendering_demand(", ready_start)
    unsupported_start = source.find("fn unsupported_rendering(", demand_start)
    if min(ready_start, demand_start, unsupported_start) < 0:
        raise ReleaseError("cannot locate controlled rendering scheduler classification")
    require_source_fragments_in_order(
        source[ready_start:demand_start],
        (
            "let rendering_opportunity_is_unscheduled =",
            "pending.rendering.scheduled_opportunity.is_none()",
            "rendering.animated_images.update_ready",
            "rendering_opportunity_is_unscheduled",
            "rendering.pending_animation_events != 0",
            "rendering.runnable_animation_frame_callbacks != 0",
            "rendering.document_update_required",
        ),
        "scheduled pending animation-event Drive exclusion",
    )
    require_source_fragments_in_order(
        source[demand_start:unsupported_start],
        (
            "rendering.runnable_animation_frame_callbacks != 0",
            "rendering.document_update_required",
            "rendering.pending_animation_events != 0",
            "rendering.finite_animations != 0",
        ),
        "pending animation-event finite rendering demand",
    )
    test_start = source.find("fn scheduled_pending_animation_events_advance_instead_of_spinning()")
    test_end = source.find(
        "fn exact_now_rendering_opportunity_advances_instead_of_spinning()",
        test_start,
    )
    if test_start < 0 or test_end < 0:
        raise ReleaseError("cannot locate scheduled pending animation-event liveness proof")
    require_source_fragments_in_order(
        source[test_start:test_end],
        (
            "for delay in [Duration::ZERO, Duration::from_nanos(20)]",
            "rendering.pending_animation_events = 1",
            "Some(advance_token)",
            "SettleProgress::Command(DocumentControlCommand::AdvanceTo(_))",
        ),
        "scheduled pending animation-event exact-now and future-head proof",
    )


INPUT_METHOD_REQUEST_INITIALIZER_RE = re.compile(r"\bInputMethodRequest\s*\{")
HANDLE_FOCUS_EVENT_RE = re.compile(
    r"\bfn\s+handle_focus_event\s*\(\s*&self\s*,\s*"
    r"event\s*:\s*&FocusEvent\s*\)\s*\{"
)


def rust_braced_block_end(source: str, opening_brace: int, description: str) -> int:
    if opening_brace < 0 or opening_brace >= len(source) or source[opening_brace] != "{":
        raise ReleaseError(f"cannot locate opening brace for {description}")
    depth = 0
    cursor = opening_brace
    while cursor < len(source):
        if source.startswith("//", cursor):
            newline = source.find("\n", cursor + 2)
            cursor = len(source) if newline < 0 else newline + 1
            continue
        if source.startswith("/*", cursor):
            comment_depth = 1
            cursor += 2
            while cursor < len(source) and comment_depth:
                if source.startswith("/*", cursor):
                    comment_depth += 1
                    cursor += 2
                elif source.startswith("*/", cursor):
                    comment_depth -= 1
                    cursor += 2
                else:
                    cursor += 1
            if comment_depth:
                raise ReleaseError(f"unterminated block comment in {description}")
            continue
        if source[cursor] == '"':
            cursor += 1
            while cursor < len(source):
                if source[cursor] == "\\":
                    cursor += 2
                elif source[cursor] == '"':
                    cursor += 1
                    break
                else:
                    cursor += 1
            else:
                raise ReleaseError(f"unterminated string literal in {description}")
            continue
        if source[cursor] == "{":
            depth += 1
        elif source[cursor] == "}":
            depth -= 1
            if depth == 0:
                return cursor + 1
            if depth < 0:
                break
        cursor += 1
    raise ReleaseError(f"unterminated braced block for {description}")


def input_method_initializer_spans(source: str, description: str) -> list[tuple[int, int]]:
    spans = []
    for match in INPUT_METHOD_REQUEST_INITIALIZER_RE.finditer(source):
        opening_brace = source.find("{", match.start(), match.end())
        spans.append(
            (
                match.start(),
                rust_braced_block_end(source, opening_brace, description),
            )
        )
    return spans


def verify_input_method_request_inventory(source_root: Path) -> None:
    script_root = source_root / "components/script"
    if not script_root.is_dir():
        raise ReleaseError("cannot locate components/script InputMethod producer root")
    actual: dict[str, int] = {}
    try:
        for filename in sorted(script_root.rglob("*.rs")):
            source = filename.read_text(encoding="utf-8")
            count = len(
                input_method_initializer_spans(
                    source,
                    f"InputMethodRequest initializer inventory in {filename}",
                )
            )
            if count:
                actual[filename.relative_to(source_root).as_posix()] = count
    except (OSError, UnicodeError) as error:
        raise ReleaseError(f"cannot inventory InputMethodRequest producers: {error}") from error
    expected = {
        INPUT_METHOD_CONTROL_SOURCE.as_posix(): 1,
        INPUT_METHOD_INPUT_SOURCE.as_posix(): 1,
        INPUT_METHOD_TEXTAREA_SOURCE.as_posix(): 1,
    }
    if actual != expected:
        raise ReleaseError(
            f"components/script InputMethodRequest initializer inventory changed: expected {expected!r}, got {actual!r}"
        )


def verify_input_method_focus_producer(
    source: str,
    source_name: Path,
    expected_input_method_type: str,
    expected_multiline: bool,
) -> None:
    focus_functions = list(HANDLE_FOCUS_EVENT_RE.finditer(source))
    if len(focus_functions) != 1:
        raise ReleaseError(f"{source_name.as_posix()} must contain exactly one handle_focus_event producer")
    focus_match = focus_functions[0]
    opening_brace = source.find("{", focus_match.start(), focus_match.end())
    focus_end = rust_braced_block_end(
        source,
        opening_brace,
        f"{source_name.as_posix()} handle_focus_event producer",
    )
    initializer_spans = input_method_initializer_spans(source, f"{source_name.as_posix()} InputMethodRequest producer")
    if len(initializer_spans) != 1:
        raise ReleaseError(f"{source_name.as_posix()} must contain exactly one InputMethodRequest initializer")
    initializer_start, initializer_end = initializer_spans[0]
    if not (focus_match.start() <= initializer_start and initializer_end <= focus_end):
        raise ReleaseError(f"{source_name.as_posix()} InputMethodRequest must be owned by handle_focus_event")
    focus_source = source[focus_match.start() : focus_end]
    initializer_source = source[initializer_start:initializer_end]
    require_source_fragments_in_order(
        focus_source,
        (
            'if *event_type == *"blur"',
            'else if *event_type == *"focus"',
            ".show_embedder_control(",
            "ControlElement::Ime(Dom::from_ref(self.upcast()))",
            "EmbedderControlRequest::InputMethod(InputMethodRequest {",
        ),
        f"{source_name.as_posix()} InputMethod focus producer",
    )
    multiline = str(expected_multiline).lower()
    require_source_fragments_in_order(
        initializer_source,
        (
            expected_input_method_type,
            "text: String::from(self.Value())",
            "insertion_point: self.GetSelectionEnd()",
            f"multiline: {multiline}",
            "allow_virtual_keyboard: self.owner_window().has_sticky_activation()",
        ),
        f"{source_name.as_posix()} InputMethod request provenance",
    )


def verify_controlled_input_method_source(
    source_root: Path,
    control_source: str,
    input_source: str,
    textarea_source: str,
) -> None:
    verify_input_method_request_inventory(source_root)
    control_initializers = input_method_initializer_spans(
        control_source, "controlled InputMethod test-helper initializer"
    )
    test_module_start = control_source.find("#[cfg(test)]\nmod input_method_embedder_control_tests")
    test_helper_start = control_source.find("fn input_method_request(", test_module_start)
    test_helper_opening_brace = control_source.find("{", test_helper_start)
    test_helper_end = (
        rust_braced_block_end(
            control_source,
            test_helper_opening_brace,
            "controlled InputMethod cfg(test) request helper",
        )
        if test_helper_start >= 0
        else -1
    )
    if (
        len(control_initializers) != 1
        or test_module_start < 0
        or test_helper_start < 0
        or not (test_helper_start <= control_initializers[0][0] and control_initializers[0][1] <= test_helper_end)
    ):
        raise ReleaseError(
            "the sole document-control InputMethodRequest initializer must remain in "
            "the cfg(test) input_method_request helper"
        )
    verify_input_method_focus_producer(
        input_source,
        INPUT_METHOD_INPUT_SOURCE,
        "input_method_type,",
        False,
    )
    verify_input_method_focus_producer(
        textarea_source,
        INPUT_METHOD_TEXTAREA_SOURCE,
        "input_method_type: InputMethodType::Text",
        True,
    )
    helper_start = control_source.find("fn suppress_input_method_embedder_control(")
    implementation_start = control_source.find("impl DocumentEmbedderControls", helper_start)
    if helper_start < 0 or implementation_start < 0:
        raise ReleaseError("cannot locate controlled InputMethod suppression helper")
    expected_helper = """
        fn suppress_input_method_embedder_control(
            semantic_automation_focus_active: bool,
            controlled_clock: bool,
            control_profile: DocumentControlProfile,
            execution_profile: DocumentExecutionProfile,
            top_level: bool,
            request: &EmbedderControlRequest,
        ) -> bool {
            let EmbedderControlRequest::InputMethod(input_method) = request else {
                return false;
            };
            if semantic_automation_focus_active {
                return true;
            }

            let controlled_session_v2 = controlled_clock
                && control_profile == DocumentControlProfile::TopLevelSession
                && execution_profile == DocumentExecutionProfile::ControlledWebSessionV2
                && top_level;

            controlled_session_v2
                && input_method.input_method_type == InputMethodType::Text
                && !input_method.multiline
                && !input_method.allow_virtual_keyboard
        }
    """
    actual_helper = control_source[helper_start:implementation_start]
    if " ".join(actual_helper.split()) != " ".join(expected_helper.split()):
        raise ReleaseError(
            "controlled InputMethod suppression helper must match the exact "
            "whitespace-normalized single-line Text/nonmultiline/no-virtual-keyboard body"
        )
    show_start = control_source.find("pub(crate) fn show_embedder_control(", implementation_start)
    send_start = control_source.find("fn send_embedder_control_request(", show_start)
    if show_start < 0 or send_start < 0:
        raise ReleaseError("cannot locate controlled InputMethod presentation boundary")
    require_source_fragments_in_order(
        control_source[show_start:send_start],
        (
            "if suppress_input_method_embedder_control(",
            "self.window",
            ".as_global_scope()",
            ".document_clock()",
            ".is_controlled()",
            "ScriptThread::current_document_control_profile()",
            "ScriptThread::current_document_execution_profile()",
            "self.window.is_top_level()",
            "return None;",
            ".require_embedder_control()",
            ".insert(id.index.into(), element)",
            "self.send_embedder_control_request(request, id, rect)",
        ),
        "controlled InputMethod suppression before external publication",
    )


def verify_controlled_focus_event_timestamp_source(event_source: str, focus_event_source: str) -> None:
    require_source_fragments_in_order(
        event_source,
        (
            "time_stamp: Cell<PerformanceEntryTime>",
            "Cell::new(PerformanceEntryTime::Host(CrossProcessInstant::now()))",
            "pub(crate) fn set_creation_time_stamp(",
            "self.time_stamp.set(time_stamp)",
            "entry_time_to_dom_high_res_time_stamp(self.time_stamp.get())",
        ),
        "provenance-aware Event timestamp storage",
    )
    uninitialized_start = focus_event_source.find("pub(crate) fn new_uninitialized_with_proto(")
    engine_new_start = focus_event_source.find("pub(crate) fn new(", uninitialized_start)
    new_with_proto_start = focus_event_source.find("fn new_with_proto(", engine_new_start)
    constructor_start = focus_event_source.find("fn Constructor(", new_with_proto_start)
    if min(uninitialized_start, engine_new_start, new_with_proto_start, constructor_start) < 0:
        raise ReleaseError("cannot locate FocusEvent construction boundaries")
    if "set_creation_time_stamp" in focus_event_source[uninitialized_start:engine_new_start]:
        raise ReleaseError("script-created FocusEvent unexpectedly receives controlled timestamp")
    engine_new_end = focus_event_source.rfind(
        "#[expect(clippy::too_many_arguments)]", engine_new_start, new_with_proto_start
    )
    if engine_new_end < 0:
        raise ReleaseError("cannot isolate the engine-generated FocusEvent constructor")
    expected_engine_new = """
        pub(crate) fn new(
            cx: &mut JSContext,
            window: &Window,
            event_type: Atom,
            can_bubble: EventBubbles,
            cancelable: EventCancelable,
            view: Option<&Window>,
            detail: i32,
            related_target: Option<&EventTarget>,
        ) -> DomRoot<FocusEvent> {
            let event = Self::new_with_proto(
                cx,
                window,
                None,
                event_type,
                can_bubble,
                cancelable,
                view,
                detail,
                related_target,
            );
            if ScriptThread::current_document_control_profile()
                == DocumentControlProfile::TopLevelSession
                && ScriptThread::current_document_execution_profile()
                    == DocumentExecutionProfile::ControlledWebSessionV2
                && window.is_top_level()
                && let Ok(time_stamp @ PerformanceEntryTime::Document(_)) =
                    window.Performance(cx).current_entry_time()
            {
                // Engine-generated focus transitions are same-event-loop work in the controlled
                // top-level document. Stamp them at creation from that document clock so React's
                // Event.timeStamp observation cannot import a host monotonic instant. Script-created
                // FocusEvent objects and every predecessor profile retain the explicit host boundary.
                event
                    .upcast::<Event>()
                    .set_creation_time_stamp(time_stamp);
            }
            event
        }
    """
    actual_engine_new = focus_event_source[engine_new_start:engine_new_end]
    if " ".join(actual_engine_new.split()) != " ".join(expected_engine_new.split()):
        raise ReleaseError(
            "engine-generated controlled FocusEvent constructor must match the exact "
            "whitespace-normalized v2/top-level/document-clock body"
        )
    constructor = focus_event_source[constructor_start:]
    if "FocusEvent::new_with_proto(" not in constructor or "FocusEvent::new(" in constructor:
        raise ReleaseError("FocusEvent WebIDL constructor must retain the host-timestamp construction path")


def verify_controlled_automation_event_timestamp_source(
    automation_source: str,
    script_thread_source: str,
    window_source: str,
    event_source: str,
    event_target_source: str,
    input_event_source: str,
    pointer_event_source: str,
    submit_event_source: str,
    form_data_event_source: str,
    protocol_source: str,
    fixture_source: str,
) -> None:
    sample_start = script_thread_source.find("let synchronous_automation_event_time = if operation.is_mutating()")
    sample_end = script_thread_source.find("let capture_synchronous_navigation =", sample_start)
    if sample_start < 0 or sample_end < 0:
        raise ReleaseError("cannot locate synchronous automation timestamp admission")
    require_source_fragments_in_order(
        script_thread_source[sample_start:sample_end],
        (
            "operation.is_mutating()",
            "self.document_control_profile == DocumentControlProfile::TopLevelSession",
            "self.document_execution_profile == DocumentExecutionProfile::ControlledWebSessionV2",
            "document",
            ".window()",
            ".sample_controlled_v2_document_performance_time()",
            "Ok(sampled) => Some(sampled)",
            "Err(error) =>",
            "reject(DocumentControlError::Clock(error))",
            "return;",
            "None",
        ),
        "controlled synchronous automation timestamp admission before mutation",
    )
    scope_start = script_thread_source.find("let execution = {", sample_end)
    scope_end = script_thread_source.find("let synchronous_navigation_emitted =", scope_start)
    if scope_start < 0 or scope_end < 0:
        raise ReleaseError("cannot locate synchronous automation timestamp RAII scope")
    require_source_fragments_in_order(
        script_thread_source[scope_start:scope_end],
        (
            "let _event_time_scope = synchronous_automation_event_time",
            "document",
            ".window()",
            ".begin_synchronous_automation_event_time(sampled)",
            "enter_auto_realm(cx, &*document)",
            "execute_prevalidated_document_automation(cx, &document, &request)",
        ),
        "controlled synchronous automation timestamp RAII scope",
    )

    sampler_start = window_source.find("pub(crate) fn sample_controlled_v2_document_performance_time(")
    sampler_end = window_source.find("fn baseline_image_cache_transport(", sampler_start)
    if sampler_start < 0 or sampler_end < 0:
        raise ReleaseError("cannot locate controlled-v2 document Performance sampler")
    require_source_fragments_in_order(
        window_source[sampler_start:sampler_end],
        (
            "let clock = self.as_global_scope().document_clock()",
            "if !clock.is_controlled()",
            "DocumentClockError::RealtimeClock",
            "clock.terminal_error()",
            "clock.require_surface(DocumentTimeSurface::Performance)?",
            "clock.try_now()?",
            "clock.duration_since_for_surface(",
            "self.document_time_origin.get()",
            "PerformanceEntryTime::Document(relative)",
            "clock.latch_terminal_error(error)",
        ),
        "controlled-v2 document Performance timestamp sampler",
    )

    require_source_fragments_in_order(
        window_source,
        (
            "pub(crate) struct SynchronousAutomationEventTimeGuard",
            "impl Drop for SynchronousAutomationEventTimeGuard",
            "self.time.set(self.previous)",
            "fn begin_synchronous_automation_event_time(",
            "let previous = time.replace(Some(sampled))",
            "mod synchronous_automation_event_time_tests",
            "fn scope_restores_an_enclosing_sample()",
            "synchronous_automation_event_time: Cell<Option<PerformanceEntryTime>>",
            "pub(crate) fn begin_synchronous_automation_event_time(",
            "pub(crate) fn synchronous_automation_event_time(",
            "self.synchronous_automation_event_time.get()",
            "synchronous_automation_event_time: Default::default()",
        ),
        "document-owned synchronous automation timestamp scope and restoration",
    )

    fill_event_start = automation_source.find("let event = InputEvent::new(")
    fill_start = automation_source.rfind("fn fill(", 0, fill_event_start)
    fill_end = automation_source.find("fn activate(", fill_event_start)
    if fill_event_start < 0 or fill_start < 0 or fill_end < 0:
        raise ReleaseError("cannot locate automation fill event timestamp seam")
    require_source_fragments_in_order(
        automation_source[fill_start:fill_end],
        (
            "let event = InputEvent::new(",
            "window",
            ".synchronous_automation_event_time()",
            "event.set_creation_time_stamp(time_stamp)",
            "event.set_composed(true)",
            "event.fire(self.cx, element.upcast::<EventTarget>())",
        ),
        "automation fill InputEvent timestamp seam",
    )

    fire_start = event_target_source.find("pub(crate) fn fire_event_with_params(")
    fire_end = event_target_source.find("pub(crate) fn add_event_listener(", fire_start)
    if fire_start < 0 or fire_end < 0:
        raise ReleaseError("cannot locate simple automation event timestamp seam")
    require_source_fragments_in_order(
        event_target_source[fire_start:fire_end],
        (
            "let global = self.global()",
            "let event = Event::new(cx, &global, name, bubbles, cancelable)",
            "global.downcast::<Window>()",
            ".synchronous_automation_event_time()",
            "event.set_creation_time_stamp(time_stamp)",
            "event.set_composed(composed.into())",
            "event.fire(cx, self)",
        ),
        "simple automation input/change/invalid event timestamp seam",
    )

    for source, description in (
        (pointer_event_source, "PointerEvent"),
        (submit_event_source, "SubmitEvent"),
        (form_data_event_source, "FormDataEvent"),
    ):
        engine_start = source.find("pub(crate) fn new(")
        with_proto_start = source.find("fn new_with_proto(", engine_start)
        constructor_start = source.find("fn Constructor(", with_proto_start)
        if min(engine_start, with_proto_start, constructor_start) < 0:
            raise ReleaseError(f"cannot locate internal and WebIDL {description} constructors")
        require_source_fragments_in_order(
            source[engine_start:with_proto_start],
            (
                "Self::new_with_proto(",
                ".synchronous_automation_event_time()",
                "set_creation_time_stamp(time_stamp)",
            ),
            f"internal {description} automation timestamp seam",
        )
        constructor = source[constructor_start:]
        if "synchronous_automation_event_time" in constructor:
            raise ReleaseError(f"script-created {description} unexpectedly consults the automation timestamp scope")
        if f"{description}::new_with_proto(" not in constructor:
            raise ReleaseError(f"script-created {description} must retain its WebIDL construction path")

    input_constructor_start = input_event_source.find("fn Constructor(")
    if input_constructor_start < 0:
        raise ReleaseError("cannot locate script-created InputEvent constructor")
    input_constructor = input_event_source[input_constructor_start:]
    if (
        "let event = InputEvent::new(" not in input_constructor
        or "synchronous_automation_event_time" in input_constructor
    ):
        raise ReleaseError("script-created InputEvent must retain its host-timestamp construction path")
    inherited_start = event_source.find("pub(crate) fn new_inherited() -> Event")
    inherited_end = event_source.find("pub(crate) fn new(", inherited_start)
    if inherited_start < 0 or inherited_end < 0:
        raise ReleaseError("cannot isolate generic Event host-timestamp construction")
    inherited = event_source[inherited_start:inherited_end]
    if (
        "PerformanceEntryTime::Host(CrossProcessInstant::now())" not in inherited
        or "synchronous_automation_event_time" in inherited
    ):
        raise ReleaseError("generic Event construction no longer preserves host authority")

    require_source_fragments_in_order(
        protocol_source,
        (
            'include_bytes!("fixtures/controlled_v2_form_event_timestamp.html")',
            "fn controlled_session_v2_form_automation_events_share_the_advanced_document_time()",
            '"action.fill"',
            '"action.activate"',
            '"action.check"',
            '"action.select"',
            '"action.submit"',
            '"5|fill:input:5>activate:click:5>reset:reset:5>check:click:5>check:input:5>check:change:5>select:input:5>select:change:5>invalid:invalid:5>submit:submit:5>submit:formdata:5|not-read|0"',
            '"5|fill:input:5>activate:click:5>reset:reset:5>check:click:5>check:input:5>check:change:5>select:input:5>select:change:5>invalid:invalid:5>submit:submit:5>submit:formdata:5>script-trigger:click:5|0,0,0,0,0|0"',
            'rejected["result"]["failure"]["code"]',
            '"unsupported_clock_surface"',
            '"timeSurface": "host_timestamp"',
        ),
        "controlled automation timestamp native protocol proof",
    )
    require_source_fragments_in_order(
        fixture_source,
        (
            "scheduledAt = String(performance.now())",
            'setTimeout(() => document.body.dataset.advanced = "yes", 5)',
            'addEventListener("input", record("fill"))',
            'addEventListener("click", record("activate"))',
            'addEventListener("reset", record("reset"))',
            'for (const type of ["click", "input", "change"])',
            'for (const type of ["input", "change"])',
            'addEventListener("invalid", record("invalid"))',
            'addEventListener("submit", record("submit"))',
            'addEventListener("formdata", event =>',
            'new Event("plain")',
            'new InputEvent("input")',
            'new PointerEvent("click")',
            'new SubmitEvent("submit")',
            'new FormDataEvent("formdata", { formData: new FormData() })',
        ),
        "controlled automation timestamp browser and script-created fixture",
    )


def verify_controlled_css_animation_event_timestamp_source(
    animations_source: str,
    animation_event_source: str,
    transition_event_source: str,
    document_source: str,
    script_thread_source: str,
    protocol_source: str,
    fixture_source: str,
) -> None:
    dispatch_start = animations_source.find("pub(crate) fn send_pending_events(&self, window: &Window")
    dispatch_end = animations_source.find("/// The type of transition event to trigger", dispatch_start)
    if dispatch_start < 0 or dispatch_end < 0:
        raise ReleaseError("cannot isolate the CSS pending-event dispatch seam")
    dispatch = animations_source[dispatch_start:dispatch_end]
    queue_take = dispatch.find("let events = std::mem::take(&mut *self.pending_events.safe_borrow_mut(cx.no_gc()))")
    if queue_take < 0:
        raise ReleaseError("cannot locate the CSS pending-event queue take")
    before_queue_take = dispatch[:queue_take]
    require_source_fragments_in_order(
        before_queue_take,
        (
            "if self.pending_events.borrow().is_empty()",
            "return;",
            "let controlled_v2_batch_time =",
            "ScriptThread::current_controlled_top_level_target_matches(window)",
            "let document = window.Document()",
            "!document.is_fully_active()",
            "!std::ptr::eq(self, document.animations())",
            "PerformanceEntryTime::Document(_)",
            "window.sample_controlled_v2_document_performance_time()",
            "else",
            "return;",
            "Some(time_stamp)",
            "None",
        ),
        "controlled CSS pending-event batch timestamp admission before queue take",
    )
    if dispatch.count("sample_controlled_v2_document_performance_time()") != 1:
        raise ReleaseError("a nonempty CSS pending-event dispatch batch must sample document time exactly once")
    if dispatch.count("current_controlled_top_level_target_matches") != 2:
        raise ReleaseError(
            "CSS pending-event timestamps must check the exact public target before sampling "
            "and again for each retained record"
        )

    require_source_fragments_in_order(
        dispatch[queue_take:],
        (
            'TransitionOrAnimationEventType::AnimationEnd => atom!("animationend")',
            'TransitionOrAnimationEventType::AnimationStart => atom!("animationstart")',
            'TransitionOrAnimationEventType::AnimationCancel => atom!("animationcancel")',
            'TransitionOrAnimationEventType::AnimationIteration => atom!("animationiteration")',
            'TransitionOrAnimationEventType::TransitionCancel => atom!("transitioncancel")',
            'TransitionOrAnimationEventType::TransitionEnd => atom!("transitionend")',
            'TransitionOrAnimationEventType::TransitionRun => atom!("transitionrun")',
            'TransitionOrAnimationEventType::TransitionStart => atom!("transitionstart")',
            "let owner_window = node.owner_window()",
            "let controlled_v2_event_time = controlled_v2_batch_time.filter(|_|",
            "let owner_document = node.owner_document()",
            "ScriptThread::current_controlled_top_level_target_matches(&owner_window)",
            "event.pipeline_id == window.pipeline_id()",
            "owner_window.pipeline_id() == window.pipeline_id()",
            "owner_document.is_fully_active()",
            "owner_window.is_top_level()",
            "std::ptr::eq(owner_document.window(), window)",
            "std::ptr::eq(&*owner_window, window)",
            "std::ptr::eq(self, owner_document.animations())",
            "let event = TransitionEvent::new(",
            "if let Some(time_stamp) = controlled_v2_event_time",
            "event.upcast::<Event>().set_creation_time_stamp(time_stamp)",
            "event.upcast::<Event>().fire(cx, node.upcast())",
            "let event = AnimationEvent::new(",
            "if let Some(time_stamp) = controlled_v2_event_time",
            "event.upcast::<Event>().set_creation_time_stamp(time_stamp)",
            "event.upcast::<Event>().fire(cx, node.upcast())",
        ),
        "exact-owner internal CSS animation and transition event timestamp dispatch",
    )
    if dispatch.count("set_creation_time_stamp(time_stamp)") != 2:
        raise ReleaseError("CSS pending-event dispatch must stamp exactly the two internal event classes")

    for source, description in (
        (animation_event_source, "AnimationEvent"),
        (transition_event_source, "TransitionEvent"),
    ):
        inherited_start = source.find("fn new_inherited(")
        engine_start = source.find("pub(crate) fn new(", inherited_start)
        with_proto_start = source.find("fn new_with_proto(", engine_start)
        constructor_start = source.find("fn Constructor(", with_proto_start)
        if min(inherited_start, engine_start, with_proto_start, constructor_start) < 0:
            raise ReleaseError(f"cannot isolate internal and WebIDL {description} constructors")
        if (
            "set_creation_time_stamp" in source
            or "sample_controlled_v2_document_performance_time" in source
            or "controlled_v2_batch_time" in source
        ):
            raise ReleaseError(f"{description} constructor source must not acquire controlled timestamp authority")
        constructor = source[constructor_start:]
        if f"{description}::new_with_proto(" not in constructor:
            raise ReleaseError(f"script-created {description} must retain its WebIDL host-timestamp path")

    require_source_fragments_in_order(
        animations_source,
        (
            "pub(crate) struct CssAnimationPendingObservation",
            "pub(crate) pending_event_count: usize",
            "pub(crate) finite_pending_or_running: usize",
            "pub(crate) infinite_pending_or_running: usize",
            "pub(crate) unsupported_pending_or_running: CssAnimationUnsupportedCounts",
            "pub(crate) fn pending_observation(&self) -> CssAnimationPendingObservation",
            "observation.pending_event_count = self.pending_events.borrow().len()",
        ),
        "retained CSS animation and pending-event observation",
    )
    require_source_fragments_in_order(
        document_source,
        (
            "pub(crate) fn pending_rendering_observation(",
            "let css_animations = self.animations.pending_observation()",
            "DocumentRenderingPendingObservation",
            "css_animations",
        ),
        "document-owned CSS animation rendering observation",
    )
    rendering_start = script_thread_source.find("fn capture_controlled_rendering(")
    rendering_end = script_thread_source.find("fn capture_controlled_pending(", rendering_start)
    if rendering_start < 0 or rendering_end < 0:
        raise ReleaseError("cannot isolate controlled rendering pending capture")
    require_source_fragments_in_order(
        script_thread_source[rendering_start:rendering_end],
        (
            "let css_animations = rendering.css_animations",
            "unsupported_pending_or_running",
            "pending_animation_events: checked_pending_count(",
            "css_animations.pending_event_count",
            "finite_animations: checked_pending_count(css_animations.finite_pending_or_running)",
            "infinite_animations: checked_pending_count(",
            "css_animations.infinite_pending_or_running",
            "unsupported_animations: checked_pending_count(unsupported_animations)",
        ),
        "controlled CSS animation settlement authority",
    )

    target_match_start = script_thread_source.find(
        "pub(crate) fn current_controlled_top_level_target_matches(window: &Window)"
    )
    target_match_end = script_thread_source.find(
        "pub(crate) fn admit_controlled_session_history_change()", target_match_start
    )
    if target_match_start < 0 or target_match_end < 0:
        raise ReleaseError("cannot isolate the exact public controlled top-level target matcher")
    require_source_fragments_in_order(
        script_thread_source[target_match_start:target_match_end],
        (
            "script_thread.document_control_profile != DocumentControlProfile::TopLevelSession",
            "script_thread.document_execution_profile !=",
            "DocumentExecutionProfile::ControlledWebSessionV2",
            "script_thread.controlled_input.is_none()",
            "!window.is_top_level()",
            "let Some(window_proxy) = window.undiscarded_window_proxy()",
            "window_proxy.is_auxiliary()",
            "let webview_id = window.webview_id()",
            "let Some(state) = &script_thread.document_control_state",
            "state.borrow().pending.owner_snapshot(webview_id).is_err()",
            "script_thread",
            ".incomplete_loads",
            ".any(|load| load.webview_id != webview_id)",
            "let documents = script_thread.documents.borrow()",
            "let mut documents = documents.iter()",
            "let Some((pipeline_id, document)) = documents.next()",
            "documents.next().is_none()",
            "pipeline_id == window.pipeline_id()",
            "document.webview_id() == webview_id",
            "document.is_fully_active()",
            "std::ptr::eq(document.window(), window)",
        ),
        "non-auxiliary exact public controlled top-level target membership",
    )
    require_source_fragments_in_order(
        protocol_source,
        (
            'include_bytes!("fixtures/controlled_v2_css_animation_event_timestamp.html")',
            "fn controlled_session_v2_css_animation_events_use_owned_dispatch_time()",
            '"armed:5|animationstart:trusted:20:20:owned>animationend:trusted:20:20:owned"',
            'exercise_css_animation_event_profile("controlled-web-session-v1"',
            "None",
            "fn controlled_session_v2_script_created_animation_and_transition_events_remain_host_stamped()",
            "fn exercise_css_animation_event_profile(",
            '"timeSurface": "host_timestamp"',
            "fn exercise_script_created_css_event_timestamp_boundary()",
            '"armed:-1|script:0,0"',
            '"unsupported_clock_surface"',
            '"timeSurface": "host_timestamp"',
        ),
        "controlled CSS animation timestamp native protocol proof",
    )
    require_source_fragments_in_order(
        fixture_source,
        (
            "const eventTime = event.timeStamp",
            "const now = performance.now()",
            'eventTime === now && eventTime >= armedAt ? "owned" : "wrong"',
            '"animationstart"',
            '"animationend"',
            'target.classList.add("running")',
            'new AnimationEvent("animationstart"',
            'new TransitionEvent("transitionrun"',
            "setTimeout(() => {}, 5)",
        ),
        "controlled CSS animation timestamp browser/internal and script-created fixture",
    )


def verify_controlled_image_element_source(source: str) -> None:
    require_source_fragments_in_order(
        source,
        (
            "enum ImageRequestProvenance",
            "ControlledV2DirectDataSvg",
            "ControlledV2DirectHttpImage",
            "fn is_controlled_v2(self) -> bool",
            "Self::ControlledV2DirectDataSvg | Self::ControlledV2DirectHttpImage",
        ),
        "controlled direct image request provenance classes",
    )
    url_policy_start = source.find("fn controlled_v2_direct_image_url_provenance(")
    url_policy_end = source.find("#[cfg(test)]", url_policy_start)
    if url_policy_start < 0 or url_policy_end < 0:
        raise ReleaseError("cannot locate controlled direct image URL policy")
    require_source_fragments_in_order(
        source[url_policy_start:url_policy_end],
        (
            'matches!(image_url.scheme(), "http" | "https")',
            "serialized_url.len() <= CONTROLLED_V2_DIRECT_HTTP_IMAGE_URL_LIMIT",
            "ImageRequestProvenance::ControlledV2DirectHttpImage",
            "serialized_url.len() > CONTROLLED_V2_DIRECT_DATA_SVG_URL_LIMIT",
            "DataUrl::process(serialized_url)",
            'mime_type.type_ == "image"',
            'mime_type.subtype == "svg+xml"',
            "ImageRequestProvenance::ControlledV2DirectDataSvg",
        ),
        "controlled direct HTTP(S) and data-SVG URL selection policy",
    )
    require_source_fragments_in_order(
        source,
        (
            "#[test]\n    fn bounded_http_and_https_urls_are_admitted_as_owned_image_streams()",
            "#[test]\n    fn non_network_urls_do_not_gain_the_http_image_authority()",
            "#[test]\n    fn oversized_http_url_remains_unowned()",
        ),
        "controlled direct HTTP(S) image URL policy unit proofs",
    )
    selection_start = source.find("fn selected_request_provenance(")
    selection_end = source.find("fn active_request_provenance(", selection_start)
    if selection_start < 0 or selection_end < 0:
        raise ReleaseError("cannot locate controlled HTMLImageElement selection boundary")
    require_source_fragments_in_order(
        source[selection_start:selection_end],
        (
            "!document.is_active()",
            "!ScriptThread::current_controlled_top_level_target_matches(window)",
            "self.uses_srcset_or_picture()",
            "return ImageRequestProvenance::Baseline",
            'get_string_attribute(&local_name!("src"))',
            "direct_src.str().as_ref() != selected_source.as_ref()",
            "controlled_v2_direct_image_url_provenance(image_url)",
            ".unwrap_or(ImageRequestProvenance::Baseline)",
        ),
        "controlled direct image DOM and exact-target selection",
    )

    prepare_start = source.find("fn prepare_image_request(")
    prepare_end = source.find("fn update_the_image_data(", prepare_start)
    if prepare_start < 0 or prepare_end < 0:
        raise ReleaseError("cannot locate HTMLImageElement request-provenance retention")
    prepare_source = source[prepare_start:prepare_end]
    require_source_fragments_in_order(
        prepare_source,
        (
            "let provenance = self.selected_request_provenance(selected_source, image_url);",
            "self.init_image_request(",
            "provenance,",
        ),
        "controlled image request provenance",
    )
    if "request.provenance = provenance;" not in source:
        raise ReleaseError("controlled image request provenance is not stored on ImageRequest")

    image_request_start = source.find("struct ImageRequest {")
    image_request_end = source.find("#[dom_struct]", image_request_start)
    if image_request_start < 0 or image_request_end < 0:
        raise ReleaseError("cannot locate HTMLImageElement request authority fields")
    require_source_fragments_in_order(
        source[image_request_start:image_request_end],
        (
            "provenance: ImageRequestProvenance",
            "controlled_cache_id: Option<PendingImageId>",
        ),
        "controlled image request authority fields",
    )
    if source.count("controlled_cache_id: None,") != 2:
        raise ReleaseError("both HTMLImageElement request slots must begin without controlled cache authority")

    cached_identity_start = source.find("fn record_active_controlled_cache_id(")
    cached_identity_end = source.find("fn synchronous_image_delivery(", cached_identity_start)
    if cached_identity_start < 0 or cached_identity_end < 0:
        raise ReleaseError("cannot locate controlled cached-vector identity boundary")
    require_source_fragments_in_order(
        source[cached_identity_start:cached_identity_end],
        (
            "ImageRequestPhase::Current => {",
            "self.current_request.borrow_mut().controlled_cache_id = id",
            "ImageRequestPhase::Pending => {",
            "self.pending_request.borrow_mut().controlled_cache_id = id",
            "fn owns_controlled_cache_id(",
            "request.provenance.is_controlled_v2()",
            "request.controlled_cache_id == Some(id)",
            "fn prepare_cached_vector_identity(",
            "let Image::Vector(vector) = image",
            "ImageRequestProvenance::Baseline",
            "window.downgrade_cached_vector_identity_to_baseline(vector.id)",
            "ImageRequestProvenance::ControlledV2DirectDataSvg",
            "ImageRequestProvenance::ControlledV2DirectHttpImage",
            ".retain_controlled_v2_cached_vector_identity(vector.id, self.upcast::<Node>())",
            "fn release_cached_vector_identity(",
            ".release_controlled_v2_cached_vector_identity(id, self.upcast::<Node>())",
            "fn release_cached_vector_identity_if_unowned(",
            "if !self.owns_controlled_cache_id(id)",
            "self.release_cached_vector_identity(id)",
        ),
        "controlled HTMLImageElement cached-vector identity",
    )
    if source.count(".prepare_cached_vector_identity(&image, provenance)") != 2:
        raise ReleaseError(
            "controlled HTMLImageElement must retain or downgrade cached-vector identity "
            "on both synchronous cache-hit paths"
        )

    fetch_cache_start = source.find("fn fetch_image(")
    fetch_cache_end = source.find("fn queue_image_cache_response(", fetch_cache_start)
    if fetch_cache_start < 0 or fetch_cache_end < 0:
        raise ReleaseError("cannot locate ordinary HTMLImageElement cache-hit boundary")
    fetch_cache_source = source[fetch_cache_start:fetch_cache_end]
    require_source_fragments_in_order(
        fetch_cache_source,
        (
            "self.synchronous_image_delivery(provenance)",
            ".prepare_cached_vector_identity(&image, provenance)",
            "let controlled_cache_id = provenance",
            ".is_controlled_v2()",
            "Self::vector_image_id(Some(&image))",
            "self.record_active_controlled_cache_id(controlled_cache_id)",
            "ImageRequestProvenance::Baseline => {",
            "self.process_image_response(",
            "ImageResponse::Loaded(image, url)",
            "ImageRequestProvenance::ControlledV2DirectDataSvg |",
            "ImageRequestProvenance::ControlledV2DirectHttpImage => {",
            "self.install_loaded_image_response(image, url, cx)",
            "self.queue_controlled_v2_cache_hit_load(delivery)",
        ),
        "status-race cache-hit retained identity and controlled queued delivery",
    )
    if "fire_image_event(" in fetch_cache_source or "fire_image_completion_events(" in fetch_cache_source:
        raise ReleaseError("HTMLImageElement cache-status dispatch must not fire completion events inline")

    ready_start = source.find("ImageCacheResult::ReadyForRequest(id) => {")
    ready_end = source.find("ImageCacheResult::FailedToLoadOrDecode", ready_start)
    if ready_start < 0 or ready_end < 0:
        raise ReleaseError("cannot locate controlled image request-start ordering")
    require_source_fragments_in_order(
        source[ready_start:ready_end],
        (
            "ImageRequestProvenance::ControlledV2DirectDataSvg |",
            "ImageRequestProvenance::ControlledV2DirectHttpImage => {",
            "if self.register_image_cache_callback(id, ChangeType::Element, provenance) {",
            "self.fetch_request(img_url, id, provenance);",
        ),
        "controlled image registration before request start",
    )

    callback_start = source.find("fn register_image_cache_callback(")
    callback_end = source.find("fn process_image_response(", callback_start)
    if callback_start < 0 or callback_end < 0:
        raise ReleaseError("cannot locate controlled HTMLImageElement callback registration")
    require_source_fragments_in_order(
        source[callback_start:callback_end],
        (
            "ImageRequestProvenance::ControlledV2DirectDataSvg |",
            "ImageRequestProvenance::ControlledV2DirectHttpImage => {",
            "window.register_controlled_v2_image_cache_listener(",
            "self.upcast::<Node>()",
            "Self::queue_image_cache_response(",
            "delivery,",
            "return false;",
            "image_cache().add_listener(",
            "if provenance.is_controlled_v2()",
            "self.record_active_controlled_cache_id(Some(id))",
            "true",
        ),
        "controlled HTMLImageElement callback registration",
    )
    fetch_request_start = source.find("fn fetch_request(", callback_start)
    fetch_request_end = source.find("fn process_image_response(", fetch_request_start)
    if fetch_request_start < 0 or fetch_request_end < 0:
        raise ReleaseError("cannot locate controlled image request provenance handoff")
    require_source_fragments_in_order(
        source[fetch_request_start:fetch_request_end],
        (
            "provenance: ImageRequestProvenance",
            "let context = ImageContext",
            "provenance,",
        ),
        "controlled image request provenance handoff into ImageContext",
    )

    multipart_start = source.find("if mime.type_() == mime::MULTIPART")
    multipart_end = source.find("// The HTTP status code is ignored here", multipart_start)
    if multipart_start < 0 or multipart_end < 0:
        raise ReleaseError("cannot locate controlled multipart image retirement boundary")
    require_source_fragments_in_order(
        source[multipart_start:multipart_end],
        (
            'mime.subtype().as_str() == "x-mixed-replace"',
            "self.aborted = true",
            "self.provenance.is_controlled_v2()",
            ".mark_controlled_v2_image_cache_id_unsupported(self.id)",
            "FetchResponseMsg::ProcessResponseEOF(",
            "Err(NetworkError::ResourceLoadError(",
            "ResourceTimingType::Error",
            "return;",
        ),
        "controlled HTTP multipart typed retirement",
    )

    queue_start = source.find("fn queue_image_cache_response(")
    queue_end = source.find("fn register_image_cache_callback(", queue_start)
    if queue_start < 0 or queue_end < 0:
        raise ReleaseError("cannot locate controlled image task handoff")
    require_source_fragments_in_order(
        source[queue_start:queue_end],
        (
            ".networking_task_source()",
            ".queue(task!(process_image_response:",
            "if generation != element.generation_id()",
            "ImageCallbackDelivery::ControlledV2Fenced",
            "!element.owns_controlled_cache_id(response_id)",
            "element.release_cached_vector_identity(response_id)",
            "element.process_image_response(response.response, delivery, cx)",
        ),
        "controlled image task and generation handoff",
    )
    cache_hit_start = source.find("// Step 7.4. If the list of available images")
    cache_hit_end = source.find("// Step 7.4.8.", cache_hit_start)
    if cache_hit_start < 0 or cache_hit_end < 0:
        raise ReleaseError("cannot locate controlled image synchronous cache-hit authority")
    require_source_fragments_in_order(
        source[cache_hit_start:cache_hit_end],
        (
            "self.synchronous_image_delivery(provenance)",
            ".prepare_cached_vector_identity(&image, provenance)",
            "let controlled_cache_id =",
            "current_request.provenance = provenance",
            "current_request.controlled_cache_id = controlled_cache_id",
            "let generation = self.generation_id()",
            "if generation != this.generation_id()",
        ),
        "step-7.4 cache-hit retained authority",
    )

    environment_start = source.find("pub(crate) fn react_to_environment_changes(")
    environment_end = source.find("fn react_to_decode_image_sync_steps(", environment_start)
    if environment_start < 0 or environment_end < 0:
        raise ReleaseError("cannot locate baseline environment-change image boundary")
    require_source_fragments_in_order(
        source[environment_start:environment_end],
        (
            "ImageRequestProvenance::Baseline",
            "ImageCacheResult::Available(ImageOrMetadataAvailable::ImageAvailable",
            "self.prepare_cached_vector_identity(",
            "&image",
            "ImageRequestProvenance::Baseline",
        ),
        "unadmitted environment-change vector downgrade",
    )

    release_boundaries = (
        (
            "fn handle_loaded_image(",
            "fn promote_pending_request_authority(",
            (
                "let new_vector_id = Self::vector_image_id(Some(&image))",
                "let old_controlled_id = current_request.controlled_cache_id",
                "current_request.controlled_cache_id == new_vector_id",
                "current_request.controlled_cache_id = retained_new_id",
                "old_controlled_id.filter(|old_id| Some(*old_id) != retained_new_id)",
                "self.release_cached_vector_identity_if_unowned(id)",
            ),
            "loaded-image vector replacement",
        ),
        (
            "fn promote_pending_request_authority(",
            "fn process_image_response(",
            (
                "pending.controlled_cache_id.take()",
                "pending.provenance = ImageRequestProvenance::Baseline",
                "let old_id = current.controlled_cache_id",
                "current.provenance = provenance",
                "current.controlled_cache_id = controlled_cache_id",
                "old_id.filter(|old_id| Some(*old_id) != controlled_cache_id)",
                "self.release_cached_vector_identity_if_unowned(id)",
            ),
            "pending-to-current authority promotion",
        ),
        (
            "fn abort_request(",
            "fn init_image_request(",
            (
                "request.controlled_cache_id.take()",
                "request.image = None",
                "self.release_cached_vector_identity_if_unowned(id)",
            ),
            "aborted image request",
        ),
        (
            "fn init_image_request(",
            "fn prepare_image_request(",
            (
                "request.controlled_cache_id.take()",
                "request.image = None",
                "self.release_cached_vector_identity_if_unowned(id)",
            ),
            "replaced image request",
        ),
    )
    for start_marker, end_marker, fragments, description in release_boundaries:
        start = source.find(start_marker)
        end = source.find(end_marker, start)
        if start < 0 or end < 0:
            raise ReleaseError(f"cannot locate controlled cached-vector {description} boundary")
        require_source_fragments_in_order(
            source[start:end],
            fragments,
            f"controlled cached-vector {description} release",
        )

    install_start = source.find("fn install_loaded_image_response(")
    response_start = source.find("fn process_image_response(", install_start)
    if install_start < 0 or response_start < 0:
        raise ReleaseError("cannot locate pending controlled image authority promotion")
    require_source_fragments_in_order(
        source[install_start:response_start],
        (
            "ImageRequestPhase::Current => self.handle_loaded_image(image, url, cx)",
            "ImageRequestPhase::Pending => {",
            "self.promote_pending_request_authority(cx)",
            "self.image_request.set(ImageRequestPhase::Current)",
            "self.handle_loaded_image(image, url, cx)",
        ),
        "shared loaded-image installation and pending authority promotion",
    )
    response_end = source.find("fn process_image_response_for_environment_change(", response_start)
    if response_end < 0:
        raise ReleaseError("cannot locate shared loaded-image response consumer")
    require_source_fragments_in_order(
        source[response_start:response_end],
        (
            "ImageResponse::Loaded(image, url)",
            "ImageRequestPhase::Current | ImageRequestPhase::Pending",
            "self.install_loaded_image_response(image, url, cx)",
        ),
        "shared loaded-image response consumer",
    )


def verify_controlled_image_timestamp_source(source: str) -> None:
    synchronous_start = source.find("fn synchronous_image_delivery(")
    event_start = source.find("fn fire_image_event(", synchronous_start)
    completion_start = source.find("fn fire_image_completion_events(", event_start)
    status_race_queue_start = source.find("fn queue_controlled_v2_cache_hit_load(", completion_start)
    fetch_start = source.find("fn fetch_image(", status_race_queue_start)
    if (
        min(
            synchronous_start,
            event_start,
            completion_start,
            status_race_queue_start,
            fetch_start,
        )
        < 0
    ):
        raise ReleaseError("cannot locate controlled image timestamp boundaries")
    require_source_fragments_in_order(
        source[synchronous_start:event_start],
        (
            "ImageRequestProvenance::ControlledV2DirectDataSvg",
            ".sample_controlled_v2_document_performance_time()",
            "ImageCallbackDelivery::ControlledV2Fenced",
            "completion_time",
        ),
        "controlled synchronous image timestamp sample",
    )
    require_source_fragments_in_order(
        source[event_start:completion_start],
        (
            "let event = Event::new(",
            "completion_time: time_stamp @ PerformanceEntryTime::Document(_)",
            "event.set_creation_time_stamp(time_stamp)",
            "ImageCallbackDelivery::ControlledV2Fenced { .. }",
            "return;",
            "event.fire(cx, self.upcast::<EventTarget>())",
        ),
        "controlled HTMLImageElement event timestamp locality",
    )
    require_source_fragments_in_order(
        source[completion_start:status_race_queue_start],
        (
            "self.fire_image_event(cx, primary_event, delivery);",
            'self.fire_image_event(cx, atom!("loadend"), delivery);',
        ),
        "shared controlled image completion timestamp",
    )
    status_race_queue_source = source[status_race_queue_start:fetch_start]
    require_source_fragments_in_order(
        status_race_queue_source,
        (
            "let ImageCallbackDelivery::ControlledV2Fenced { .. } = delivery else",
            "let generation = self.generation_id()",
            ".dom_manipulation_task_source()",
            ".queue(task!(controlled_v2_image_cache_hit_load:",
            "if generation != this.generation_id()",
            'this.fire_image_event(cx, atom!("load"), delivery);',
        ),
        "controlled image status-race cache-hit queued timestamp handoff",
    )
    if 'atom!("loadend")' in status_race_queue_source:
        raise ReleaseError("controlled image status-race cache hit must emit queued load only")

    cache_hit_start = source.find("// Step 7.4. If the list of available images")
    cache_hit_end = source.find("// Step 7.4.8.", cache_hit_start)
    if cache_hit_start < 0 or cache_hit_end < 0:
        raise ReleaseError("cannot locate controlled image synchronous cache-hit path")
    require_source_fragments_in_order(
        source[cache_hit_start:cache_hit_end],
        (
            "self.selected_request_provenance(&selected_source, &image_url)",
            "self.synchronous_image_delivery(provenance)",
            "current_request.provenance = provenance",
            "let generation = self.generation_id();",
            "if generation != this.generation_id()",
            'this.fire_image_event(cx, atom!("load"), delivery);',
        ),
        "controlled image cache-hit provenance and timestamp handoff",
    )


def verify_controlled_http_image_protocol_proof_source(
    baseline_source: str,
    http_fixture_source: str,
    multipart_fixture_source: str,
) -> None:
    require_source_fragments_in_order(
        baseline_source,
        (
            "#[test]\nfn controlled_session_v2_http_image_success_and_failure_are_owned_without_v1_promotion()",
            'exercise_controlled_http_image_profile("controlled-web-session-v2"',
            'exercise_controlled_http_image_profile("controlled-web-session-v1"',
            "#[test]\nfn controlled_session_v2_http_multipart_finite_response_retires_to_typed_image_load_unsupported()",
            '"unsupported_rendering"',
            '"image_load"',
            "#[test]\nfn controlled_session_v2_http_same_id_aba_ignores_stale_generations()",
            '"/probe"',
            "b_flushed",
            'format!("A>B>A|load:0>loadend:0|{a_url}|now:0")',
            "#[test]\nfn controlled_session_v2_http_redirect_completion_is_owned()",
            '"load:0>loadend:0|now:0"',
            "#[test]\nfn controlled_session_v2_inflight_http_image_blocks_document_replacement()",
            '"blocked_on_external_io"',
            "Some(70)",
        ),
        "controlled HTTP image native protocol proofs",
    )
    require_source_fragments_in_order(
        http_fixture_source,
        (
            'const assets = "https://controlled-image-assets.example.test"',
            'for (const type of ["load", "error", "loadend"])',
            "const cached = new Image()",
            'record("cached", event)',
            "cached.src = `${assets}/controlled-v2-http-image.svg`",
            'document.querySelector("#loaded").src',
            'document.querySelector("#failed").src',
        ),
        "controlled cross-origin HTTP success decode-error and cache-hit fixture",
    )
    require_source_fragments_in_order(
        multipart_fixture_source,
        (
            "controlled v2 multipart HTTP image boundary",
            'src="https://controlled-image-assets.example.test/controlled-v2-http-image.multipart"',
        ),
        "controlled cross-origin HTTP multipart fixture",
    )


def verify_controlled_http_image_replacement_authority_source(
    shell_source: str,
    settlement_source: str,
) -> None:
    require_source_fragments_in_order(
        shell_source,
        (
            "source_external_io_active_at_authorization:\n"
            "                        controlled_network_blocks_document_replacement(\n"
            "                            active_profile,\n"
            "                            controlled_network_active_operations,\n"
            "                        ),",
            "fn controlled_network_blocks_document_replacement(",
            "profile == Some(SessionProfile::ControlledWebSessionV2) && active_operations != 0",
            "coordinator.latch_additional_foreground_external_io_active(\n"
            "                    *source_external_io_active_at_authorization,\n"
            "                );",
            "fn active_controlled_network_latches_v2_document_replacement_only()",
        ),
        "v2 source-document external-I/O replacement latch",
    )
    require_source_fragments_in_order(
        settlement_source,
        (
            "latched_additional_foreground_external_io_active: bool,",
            "pub fn latch_additional_foreground_external_io_active(&mut self, active: bool)",
            "self.latched_additional_foreground_external_io_active |= active;",
            "if self.latched_additional_foreground_external_io_active\n"
            "            || self.additional_foreground_external_io_active",
            "fn latched_additional_foreground_io_survives_refresh_and_fails_closed()",
        ),
        "monotonic source-document external-I/O settlement gate",
    )


def verify_controlled_image_per_pipeline_cache_source(source: str) -> None:
    create_start = source.find("impl ImageCacheFactory for ImageCacheFactoryImpl")
    create_end = source.find("pub struct ImageCacheImpl", create_start)
    if create_start < 0 or create_end < 0:
        raise ReleaseError("cannot locate per-pipeline ImageCache factory boundary")
    require_source_fragments_in_order(
        source[create_start:create_end],
        (
            "fn create(",
            "webview_id: WebViewId",
            "pipeline_id: PipelineId",
            "Arc::new(ImageCacheImpl",
            "store: Arc::new(Mutex::new(ImageCacheStore",
            "pending_loads: AllPendingLoads::new()",
            "completed_loads: HashMap::new()",
            "vector_images: FxHashMap::default()",
            "rasterized_vector_images: FxHashMap::default()",
            "pipeline_id,",
            "webview_id,",
            "svg_id_image_id_map: Arc::new(Mutex::new(FxHashMap::default()))",
            "thread_pool: self.thread_pool.clone()",
        ),
        "fresh per-pipeline ImageCache store with only decode infrastructure shared",
    )


def verify_controlled_profile_wire_source(source: str) -> None:
    require_source_fragments_in_order(
        source,
        (
            "    #[test]\n    fn controlled_web_session_v2_profile_is_an_explicit_bounded_surface_expansion()",
            'include_bytes!("../../../profiles/controlled-web-session-v2.json")',
            "Sha256::digest(profile_bytes)",
            f'"{CANDIDATE_V2_PROFILE_SHA256}"',
            'assert_eq!(profile["releaseStatus"], "stable_contract");',
            'assert_eq!(profile["targetRelease"], "0.3.0");',
            'profile["execution"]["controlledImageElement"]',
            '"controlled_top_level_direct_data_svg_and_initial_url_retained_ownership_bounded_http_https"',
            '"maximumInitialSelectedCanonicalUrlBytes": 65536',
            '"multipartMixedReplace": "post_metadata_explicit_unsupported_provenance_retires_controlled_Image_producer_and_reports_unsupported_rendering_image_load_after_finite_resource_IO_drains_while_endless_resource_IO_remains_external"',
            '"inflightHttpDocumentReplacement": "fatal_blocked_on_external_io_before_cross_document_successor_authority"',
            '"unsupportedReservationReconciliation": "explicit_Unsupported_records_retain_exact_logical_ID_without_controlled_capacity_reservations"',
        ),
        "enabled exact controlled-web-session-v2 wire profile assertion",
    )


def verify_controlled_image_transport_source(
    messaging_source: str,
    producer_fence_source: str,
    window_source: str,
    script_thread_source: str,
    timers_source: str,
) -> None:
    delivery_match_start = window_source.find("fn retained_image_provenance_accepts_delivery(")
    delivery_match_end = window_source.find("struct PendingImageCallback", delivery_match_start)
    if delivery_match_start < 0 or delivery_match_end < 0:
        raise ReleaseError("cannot locate retained image provenance delivery matcher")
    require_source_fragments_in_order(
        window_source[delivery_match_start:delivery_match_end],
        (
            "retained != PendingImageProvenance::Unsupported",
            "retained == delivery",
        ),
        "explicit Unsupported image provenance no-delivery boundary",
    )
    unsupported_retirement_start = window_source.find("pub(crate) fn mark_controlled_v2_image_cache_id_unsupported(")
    unsupported_retirement_end = window_source.find(
        "pub(crate) fn sample_controlled_v2_document_performance_time(",
        unsupported_retirement_start,
    )
    if unsupported_retirement_start < 0 or unsupported_retirement_end < 0:
        raise ReleaseError("cannot locate controlled image Unsupported retirement boundary")
    raster_retirement_start = window_source.find(
        "fn mark_unsupported(&mut self)",
    )
    raster_retirement_end = window_source.find("}\n}", raster_retirement_start)
    if raster_retirement_start < 0 or raster_retirement_end < 0:
        raise ReleaseError("cannot locate controlled raster Unsupported retirement boundary")
    require_source_fragments_in_order(
        window_source[raster_retirement_start:raster_retirement_end],
        (
            "self.provenance = PendingImageProvenance::Unsupported",
            "self.reservation = None",
        ),
        "controlled raster Unsupported provenance and reservation retirement",
    )
    require_source_fragments_in_order(
        window_source[unsupported_retirement_start:unsupported_retirement_end],
        (
            "self.controlled_image_identities.borrow_mut().remove(&id)",
            "self.pending_image_callbacks.borrow_mut().get_mut(&id)",
            "callback.provenance = PendingImageProvenance::Unsupported",
            "callback._reservation = None",
            "self.pending_layout_images.borrow_mut().get_mut(&id)",
            "owner.provenance = PendingImageProvenance::Unsupported",
            "if *candidate_id == id",
            "entry.mark_unsupported()",
        ),
        "controlled multipart image Unsupported provenance and reservation retirement",
    )
    require_source_fragments_in_order(
        window_source,
        (
            "#[test]\n    fn explicitly_unsupported_shared_id_is_exact_inventory_and_matches_no_delivery_set()",
            "PendingImageProvenance::Unsupported",
            "assert_eq!(observation.controlled_work_items, Some(0))",
            "assert_eq!(observation.unsupported_work_items, Some(1))",
            "retained_image_provenance_accepts_delivery(",
            "PendingImageProvenance::Baseline",
            "PendingImageProvenance::ControlledV2Fenced",
        ),
        "explicit Unsupported image inventory and no-delivery unit proof",
    )
    require_source_fragments_in_order(
        messaging_source,
        (
            "pub(crate) enum ImageCacheMessage",
            "Baseline(ImageCacheResponseMessage)",
            "ControlledV2(DocumentProducerEnvelope<ImageCacheResponseMessage>)",
        ),
        "typed image-cache transport",
    )
    require_source_fragments_in_order(
        producer_fence_source,
        (
            "pub(crate) fn fence_image_callback(",
            "fence_image_callback_with_admission(",
            "DocumentProducerKind::Image",
            "fn image_response_is_terminal(",
            "ImageCacheResponseMessage::VectorImageRasterizationComplete",
            "ImageResponse::Loaded(..) | ImageResponse::FailedToLoadOrDecode",
            "let message_guard = match (state.admit_message)()",
            "DocumentProducerEnvelope::new(message, Some(message_guard))",
            "observer_fence.notify_observer_after_commit()",
        ),
        "producer-fenced image callback lifecycle",
    )
    owned_cancellation_start = producer_fence_source.find("impl Drop for ImageStreamProducer")
    callback_state_start = producer_fence_source.find("struct ImageCallbackState", owned_cancellation_start)
    if owned_cancellation_start < 0 or callback_state_start < 0:
        raise ReleaseError("cannot locate image callback owned-cancellation boundary")
    owned_cancellation = producer_fence_source[owned_cancellation_start:callback_state_start]
    require_source_fragments_in_order(
        owned_cancellation,
        (
            "fn drop(&mut self)",
            "Dropping the cache callback is the cache's cancellation boundary",
            "Treat that owned cancellation as",
            "an ordinary completion",
            "Actual transport loss, admission failure, and unwind paths call",
            "self.complete()",
        ),
        "image callback owned cancellation",
    )
    if "self.abandon()" in owned_cancellation:
        raise ReleaseError("cache-owned image callback retirement must not latch producer abandonment")
    owned_cancellation_test = producer_fence_source.find(
        "fn dropping_an_image_listener_before_terminal_is_owned_cancellation()"
    )
    next_image_test = producer_fence_source.find(
        "fn metadata_keeps_the_stream_live_and_terminal_hands_off_a_distinct_queue_lease()",
        owned_cancellation_test,
    )
    if owned_cancellation_test < 0 or next_image_test < 0:
        raise ReleaseError("cannot locate image callback owned-cancellation proof")
    require_source_fragments_in_order(
        producer_fence_source[owned_cancellation_test:next_image_test],
        (
            "drop(callback)",
            "assert!(snapshot.is_empty())",
            "assert_eq!(snapshot.terminal_error(), None)",
            ".for_kind(DocumentProducerKind::Image)",
            ".completed()",
        ),
        "image callback owned-cancellation proof",
    )
    require_source_fragments_in_order(
        timers_source,
        (
            "AdmissionLimitExceeded {",
            "kind: DocumentProducerKind",
            "limit: u64",
            "observed: u64",
            "pub fn latch_admission_limit_exceeded(",
            "DocumentProducerFenceError::AdmissionLimitExceeded",
            "state.terminal_error = Some(error)",
        ),
        "bounded image registration terminal",
    )
    finish_lease_start = timers_source.find("fn finish_lease(")
    notify_start = timers_source.find("fn notify_state_change(&self)", finish_lease_start)
    if finish_lease_start < 0 or notify_start < 0:
        raise ReleaseError("cannot locate producer lease completion invariant")
    require_source_fragments_in_order(
        timers_source[finish_lease_start:notify_start],
        (
            "if lease_id.fence_id != self.fence_id",
            "DocumentProducerFenceError::UnknownLease(lease_id)",
            "state.active_leases.get(&lease_id.sequence) != Some(&lease_id.kind)",
            "DocumentProducerFenceError::UnknownLease(lease_id)",
            "if state.terminal_error.is_none()",
            "state.terminal_error = terminal_error",
            "state.active_leases.remove(&lease_id.sequence)",
            "let index = lease_id.kind.index()",
        ),
        "producer completion and abandonment exact fence-sequence-class match",
    )

    registration_start = window_source.find("pub(crate) fn register_controlled_v2_image_cache_listener(")
    sample_start = window_source.find(
        "pub(crate) fn sample_controlled_v2_document_performance_time(", registration_start
    )
    baseline_transport_start = window_source.find("fn baseline_image_cache_transport(", sample_start)
    controlled_transport_end = window_source.find("fn retained_image_provenance(", baseline_transport_start)
    if (
        min(
            registration_start,
            sample_start,
            baseline_transport_start,
            controlled_transport_end,
        )
        < 0
    ):
        raise ReleaseError("cannot locate controlled Window image transport")
    require_source_fragments_in_order(
        window_source[registration_start:sample_start],
        (
            "DocumentControlProfile::TopLevelSession",
            "DocumentExecutionProfile::ControlledWebSessionV2",
            "ScriptThread::current_controlled_top_level_target_matches(self)",
            "let identity_owner_is_new = !self",
            ".any(|candidate| *candidate.owner == *owner)",
            "let identity_owner_reservation = identity_owner_is_new",
            "ControlledImageReservation::try_new(",
            "let callback_reservation = ControlledImageReservation::try_new(",
            "self.controlled_v2_image_cache_transport(&producer_fence)?",
            "ControlledImageIdentityOwner",
            "_reservation: identity_owner_reservation",
            ".push(PendingImageCallback",
            "_reservation: Some(callback_reservation)",
            "provenance: PendingImageProvenance::ControlledV2Fenced",
        ),
        "controlled Window image registration",
    )
    if window_source[registration_start:sample_start].count("_reservation: identity_owner_reservation") != 2:
        raise ReleaseError(
            "controlled image callback registration must retain the exact identity-owner "
            "reservation on both occupied and vacant cache-ID paths"
        )
    require_source_fragments_in_order(
        window_source[sample_start:baseline_transport_start],
        (
            "if !clock.is_controlled()",
            "clock.require_surface(DocumentTimeSurface::Performance)?",
            "clock.try_now()?",
            "PerformanceEntryTime::Document(relative)",
            "clock.latch_terminal_error(error)",
        ),
        "controlled image document-clock sample",
    )
    require_source_fragments_in_order(
        window_source[baseline_transport_start:controlled_transport_end],
        (
            "ImageCacheMessage::Baseline(message)",
            "fn controlled_v2_image_cache_transport(",
            "fence_image_callback(producer_fence",
            ".send(ImageCacheMessage::ControlledV2(envelope))",
            "ImageCacheMessage::ControlledV2(rejected) => rejected",
        ),
        "baseline-separated fenced image transport",
    )

    require_source_fragments_in_order(
        script_thread_source,
        (
            "enum ControlledImageDeliveryTarget",
            "Live",
            "Retired",
            "Unknown",
            "fn controlled_image_delivery_target(",
            "match (window_present, pipeline_tombstoned)",
            "(true, false) => ControlledImageDeliveryTarget::Live",
            "(false, true) => ControlledImageDeliveryTarget::Retired",
            "(true, true) | (false, false) => ControlledImageDeliveryTarget::Unknown",
        ),
        "controlled image live-retired-unknown target classification",
    )
    message_completion_start = script_thread_source.find("struct ControlledImageMessageCompletion {")
    message_completion_end = script_thread_source.find("struct InitialPipelineBootstrapFacts", message_completion_start)
    if message_completion_start < 0 or message_completion_end < 0:
        raise ReleaseError("cannot locate controlled image message completion guard")
    message_completion_source = script_thread_source[message_completion_start:message_completion_end]
    require_source_fragments_in_order(
        message_completion_source,
        (
            "guard: Option<DocumentProducerGuard>",
            "fn new(guard: Option<DocumentProducerGuard>) -> Self",
            "impl Drop for ControlledImageMessageCompletion",
            "let Some(guard) = self.guard.take()",
            "if std::thread::panicking()",
            "let _ = guard.abandon();",
            "else",
            "drop(guard);",
        ),
        "controlled image normal-return versus unwind completion guard",
    )
    exit_start = script_thread_source.find("fn handle_exit_pipeline_msg(")
    exit_end = script_thread_source.find("fn handle_exit_script_thread_msg(", exit_start)
    if exit_start < 0 or exit_end < 0:
        raise ReleaseError("cannot locate controlled image pipeline tombstone ordering")
    require_source_fragments_in_order(
        script_thread_source[exit_start:exit_end],
        (
            "if self.document_producer_fence.is_some()",
            "self.closed_pipelines.borrow_mut().insert(pipeline_id)",
            "self.task_queue.discard_pipeline(pipeline_id)",
            "let document = self.documents.borrow_mut().remove(pipeline_id)",
        ),
        "controlled image tombstone-before-Window-retirement ordering",
    )
    require_source_fragments_in_order(
        script_thread_source,
        (
            "fn image_delivery_completes_only_live_or_proven_retired_targets()",
            "controlled_image_delivery_target(true, false)",
            "ControlledImageDeliveryTarget::Live",
            "controlled_image_delivery_target(false, true)",
            "ControlledImageDeliveryTarget::Retired",
            "controlled_image_delivery_target(false, false)",
            "ControlledImageDeliveryTarget::Unknown",
            "controlled_image_delivery_target(true, true)",
            "ControlledImageDeliveryTarget::Unknown",
        ),
        "controlled image delivery target truth-table proof",
    )

    handle_start = script_thread_source.find("fn handle_msg_from_image_cache(")
    handle_end = script_thread_source.find("fn handle_webdriver_msg(", handle_start)
    if handle_start < 0 or handle_end < 0:
        raise ReleaseError("cannot locate ScriptThread image-cache handoff")
    image_handoff = script_thread_source[handle_start:handle_end]
    require_source_fragments_in_order(
        image_handoff,
        (
            "ImageCacheMessage::Baseline(response)",
            "ImageCacheMessage::ControlledV2(envelope)",
            "envelope.into_parts()",
            'guard.expect("controlled image transport requires an Image guard")',
            "find_window(pipeline_id)",
            "self.closed_pipelines.borrow().contains(&pipeline_id)",
            "controlled_image_delivery_target(window.is_some(), pipeline_tombstoned)",
            "ControlledImageDeliveryTarget::Live",
            "ControlledImageDeliveryTarget::Retired",
            "drop(guard);",
            "return;",
            "ControlledImageDeliveryTarget::Unknown",
            "let _ = guard.abandon();",
            "return;",
            "!= DocumentControlProfile::TopLevelSession",
            "self.document_execution_profile !=",
            "DocumentExecutionProfile::ControlledWebSessionV2",
            "!Self::current_controlled_top_level_target_matches(&window)",
            "let _ = guard.abandon();",
            "window.sample_controlled_v2_document_performance_time()",
            "let _ = guard.abandon();",
            "ImageCallbackDelivery::ControlledV2Fenced { completion_time }",
            "ControlledImageMessageCompletion::new(message_guard)",
            "let _retained_pending_state = match response",
            "window.pending_image_notification(",
            "window.handle_image_rasterization_complete_notification(",
        ),
        "guarded ScriptThread image delivery",
    )
    if image_handoff.count("drop(guard);") != 1:
        raise ReleaseError("retired controlled image target must complete exactly one message guard")
    if image_handoff.count("let _ = guard.abandon();") != 3:
        raise ReleaseError(
            "controlled image delivery must abandon exactly the unknown-target, pre-handler "
            "authority, and clock failure paths"
        )
    if (
        "if handled.is_err()" in image_handoff
        or "message_guard.abandon()" in image_handoff
        or "drop(message_guard)" in image_handoff
    ):
        raise ReleaseError(
            "a retained image handler rejection must remain under scoped normal-return/unwind completion"
        )


def verify_controlled_image_pending_and_teardown_source(window_source: str, script_thread_source: str) -> None:
    require_source_fragments_in_order(
        window_source,
        (
            "const CONTROLLED_V2_IMAGE_RETAINED_RECORD_LIMIT: usize = 512;",
            "struct ControlledImageReservation",
            "observed > CONTROLLED_V2_IMAGE_RETAINED_RECORD_LIMIT",
            ".latch_admission_limit_exceeded(",
            "DocumentProducerKind::Image",
            "count.set(observed)",
            "impl Drop for ControlledImageReservation",
            ".checked_sub(1)",
        ),
        "controlled image capacity reservation",
    )
    require_source_fragments_in_order(
        window_source,
        (
            "fn observe_pending_nonanimated_images(",
            "layout_registrations: impl IntoIterator<Item = (PendingImageId, PendingImageProvenance)>",
            "let mut callback_provenances",
            "let mut layout_provenances",
            "for (id, provenance) in layout_registrations",
            "let layout_image_ids: HashSet<_> = layout_provenances.keys().copied().collect()",
            "let mut unique_image_ids = callback_ids",
            "unique_image_ids.extend(layout_image_ids)",
            "Some((0, controlled)) if *controlled != 0",
            "!matches!(layout_provenances.get(id), Some((baseline, _)) if *baseline != 0)",
            "controlled_unique_image_ids.checked_add(controlled_rasterization_keys)",
            "retained_controlled_identity_owners",
            "controlled_retained_record_inventory_matches: retained_controlled_registrations ==",
            "Some(observed_controlled_retained_records)",
        ),
        "controlled image pending-owner reconciliation",
    )
    layout_owner_start = window_source.find("struct PendingLayoutImageAncillaryData {")
    layout_observation_start = window_source.find(
        "pub(crate) fn pending_nonanimated_image_observation(", layout_owner_start
    )
    controlled_registration_start = window_source.find(
        "pub(crate) fn register_image_cache_listener(", layout_observation_start
    )
    if (
        min(
            layout_owner_start,
            layout_observation_start,
            controlled_registration_start,
        )
        < 0
    ):
        raise ReleaseError("cannot locate retained layout-owner provenance boundary")
    require_source_fragments_in_order(
        window_source[layout_owner_start:layout_observation_start],
        (
            "struct PendingLayoutImageAncillaryData",
            "node: Dom<Node>",
            "destination: LayoutImageDestination",
            "provenance: PendingImageProvenance",
        ),
        "retained layout-owner provenance field",
    )
    require_source_fragments_in_order(
        window_source[layout_observation_start:controlled_registration_start],
        (
            "let layout_images = self.pending_layout_images.borrow()",
            ".iter()",
            ".flat_map(|(id, owners)| owners.iter().map(move |owner| (*id, owner.provenance)))",
        ),
        "per-owner layout provenance pending observation",
    )
    mixed_layout_test_start = window_source.find(
        "fn baseline_layout_owner_prevents_controlled_classification_for_shared_id()"
    )
    owner_without_callback_test_start = window_source.find(
        "fn controlled_layout_owner_without_a_callback_is_not_owned_work()",
        mixed_layout_test_start,
    )
    owner_record_test_start = window_source.find(
        "fn every_exact_cached_vector_owner_consumes_a_retained_record()",
        owner_without_callback_test_start,
    )
    if (
        min(
            mixed_layout_test_start,
            owner_without_callback_test_start,
            owner_record_test_start,
        )
        < 0
    ):
        raise ReleaseError("cannot locate retained layout-owner pending regression proofs")
    require_source_fragments_in_order(
        window_source[mixed_layout_test_start:owner_without_callback_test_start],
        (
            "&[(1, PendingImageProvenance::ControlledV2Fenced)]",
            "(1, PendingImageProvenance::ControlledV2Fenced)",
            "(1, PendingImageProvenance::Baseline)",
            "assert_eq!(observation.controlled_work_items, Some(0))",
            "assert_eq!(observation.unsupported_work_items, Some(1))",
        ),
        "mixed baseline/controlled layout-owner regression proof",
    )
    require_source_fragments_in_order(
        window_source[owner_without_callback_test_start:owner_record_test_start],
        (
            "&[]",
            "&[(1, PendingImageProvenance::ControlledV2Fenced)]",
            "assert_eq!(observation.controlled_work_items, Some(0))",
            "assert_eq!(observation.unsupported_work_items, Some(1))",
        ),
        "layout owner without callback regression proof",
    )
    require_source_fragments_in_order(
        window_source,
        (
            "pub(crate) fn register_image_cache_listener(",
            "self.downgrade_cached_vector_identity_to_baseline(id)",
            "pub(crate) fn register_controlled_v2_image_cache_listener(",
            "pub(crate) fn retain_controlled_v2_cached_vector_identity(",
            "DocumentControlProfile::TopLevelSession",
            "DocumentExecutionProfile::ControlledWebSessionV2",
            "self.is_top_level()",
            "producer_fence.snapshot().terminal_error()",
            "return Err(error)",
            ".any(|candidate| *candidate.owner == *owner)",
            "return Ok(())",
            "ControlledImageReservation::try_new(",
            ".push(ControlledImageIdentityOwner",
            "_reservation: reservation",
            "pub(crate) fn release_controlled_v2_cached_vector_identity(",
            ".retain(|candidate| *candidate.owner != *owner)",
            "pub(crate) fn downgrade_cached_vector_identity_to_baseline(",
            "self.controlled_image_identities.borrow_mut().remove(&id)",
            "for ((candidate_id, _), entry)",
            "if *candidate_id == id",
            "entry.downgrade_to_baseline()",
        ),
        "controlled cached-vector identity lifecycle",
    )
    require_source_fragments_in_order(
        window_source,
        (
            "fn retained_image_provenance(",
            ".any(|candidate| *candidate.owner == *owner)",
            "fn new_image_rasterization_entry(",
            "ControlledImageReservation::try_new(",
            "register_baseline_image_rasterization_listener(",
            "downgrade_cached_vector_identity_to_baseline(image_id)",
            "entry.downgrade_to_baseline()",
        ),
        "controlled image layout and raster provenance",
    )
    raster_start = window_source.find("fn new_image_rasterization_entry(")
    raster_end = window_source.find("pub(crate) fn register_baseline_image_rasterization_listener(", raster_start)
    if raster_start < 0 or raster_end < 0:
        raise ReleaseError("cannot locate controlled vector-raster retained record")
    require_source_fragments_in_order(
        window_source[raster_start:raster_end],
        (
            "PendingImageProvenance::ControlledV2Fenced =>",
            "let reservation = ControlledImageReservation::try_new(",
            "let transport = self.controlled_v2_image_cache_transport(&producer_fence)?",
            "(Some(reservation), transport)",
            "PendingImageRasterizationEntry",
            "reservation,",
        ),
        "controlled vector-raster retained record",
    )
    reflow_start = window_source.find("let Some(reflow_result) = self.layout.borrow_mut().reflow(reflow)")
    post_reflow_handler_start = window_source.find("fn handle_pending_images_post_reflow(", reflow_start)
    if reflow_start < 0 or post_reflow_handler_start < 0:
        raise ReleaseError("cannot locate post-reflow controlled raster admission boundary")
    require_source_fragments_in_order(
        window_source[reflow_start:post_reflow_handler_start],
        (
            "let Some(reflow_result) = self.layout.borrow_mut().reflow(reflow)",
            "self.handle_pending_images_post_reflow(",
            "reflow_result.pending_images",
            "reflow_result.pending_rasterization_images",
        ),
        "controlled reflow raster handoff",
    )
    raster_handoff_start = window_source.find("for image in pending_rasterization_images", post_reflow_handler_start)
    raster_handoff_end = window_source.find("for node in pending_svg_element_for_serialization", raster_handoff_start)
    if raster_handoff_start < 0 or raster_handoff_end < 0:
        raise ReleaseError("cannot locate exact post-reflow raster key handoff")
    require_source_fragments_in_order(
        window_source[raster_handoff_start:raster_handoff_end],
        (
            "let key = (image.id, image.size)",
            "let mut provenance = self.retained_image_provenance(image.id, &node)",
            "let needs_listener = !self",
            "let Ok((entry, callback)) = self.new_image_rasterization_entry(provenance)",
            "continue;",
            ".insert(key, entry)",
            "image_cache.add_rasterization_complete_listener(",
            "image.id",
            "image.size",
            "callback",
        ),
        "post-reflow exact raster reservation and fenced-listener installation",
    )
    layout_handoff_start = window_source.find("for image in pending_images", post_reflow_handler_start)
    layout_handoff_end = window_source.find("for image in pending_rasterization_images", layout_handoff_start)
    if layout_handoff_start < 0 or layout_handoff_end < 0:
        raise ReleaseError("cannot locate post-reflow layout-owner provenance handoff")
    require_source_fragments_in_order(
        window_source[layout_handoff_start:layout_handoff_end],
        (
            "let is_new_owner = !self",
            ".pending_layout_images",
            ".any(|owner| *owner.node == *node)",
            "let needs_listener = !self.pending_layout_images.borrow().contains_key(&id)",
            "if !is_new_owner",
            "continue;",
            "let provenance = self.retained_image_provenance(id, &node)",
            "if provenance == PendingImageProvenance::Baseline",
            "self.downgrade_cached_vector_identity_to_baseline(id)",
            "if needs_listener",
            "nodes.push(PendingLayoutImageAncillaryData",
            "provenance,",
        ),
        "post-reflow exact layout-owner provenance retention and downgrade",
    )

    delivery_start = window_source.find("pub(crate) fn pending_image_notification(")
    delivery_end = window_source.find("pub(crate) fn handle_image_notification_pending", delivery_start)
    if delivery_start < 0:
        raise ReleaseError("cannot locate retained layout-owner delivery rejection")
    if delivery_end < 0:
        delivery_end = window_source.find("pub(crate) fn", delivery_start + 1)
    if delivery_end < 0:
        raise ReleaseError("cannot bound retained layout-owner delivery rejection")
    delivery_source = window_source[delivery_start:delivery_end]
    require_source_fragments_in_order(
        delivery_source,
        (
            "let delivery_provenance = delivery.provenance()",
            ".pending_layout_images",
            ".get(&response.id)",
            "!retained_image_provenance_accepts_delivery(",
            "owner.provenance",
            "delivery_provenance",
            "return Err(())",
            "std::mem::take(&mut *self.pending_image_callbacks.borrow_mut())",
            "!retained_image_provenance_accepts_delivery(",
            "callback.provenance",
            "delivery_provenance",
            "return Err(())",
            "for callback in callbacks.get()",
            "(callback.callback)(response.clone(), delivery, cx)",
        ),
        "mixed layout-owner delivery rejection before callbacks",
    )
    retained_layout_start = delivery_source.find("        if self\n            .pending_layout_images")
    retained_layout_end = delivery_source.find("        // We take the images here")
    if retained_layout_start < 0 or retained_layout_end < 0:
        raise ReleaseError("mixed layout-owner rejection must leave Window pending collections retained")
    retained_layout_rejection = delivery_source[retained_layout_start:retained_layout_end]
    if (
        "return Err(());" not in retained_layout_rejection
        or ".borrow_mut()" in retained_layout_rejection
        or ".remove(" in retained_layout_rejection
    ):
        raise ReleaseError("mixed layout-owner rejection must fail before mutating retained Window state")
    retained_callback_rejection = (
        "        {\n"
        "            let _ = std::mem::replace(&mut *self.pending_image_callbacks.borrow_mut(), images);\n"
        "            return Err(());\n"
        "        }\n\n"
        "        for callback in callbacks.get()"
    )
    if retained_callback_rejection not in delivery_source:
        raise ReleaseError("mixed callback rejection must restore Window pending callback state")

    raster_delivery_start = window_source.find("pub(crate) fn handle_image_rasterization_complete_notification(")
    if raster_delivery_start < 0 or raster_delivery_start >= delivery_start:
        raise ReleaseError("cannot locate retained raster delivery rejection")
    raster_delivery_source = window_source[raster_delivery_start:delivery_start]
    retained_raster_provenance_rejection = (
        "        if entry.provenance != delivery.provenance() {\n"
        "            self.pending_images_for_rasterization\n"
        "                .borrow_mut()\n"
        "                .insert(key, entry);\n"
        "            return Err(());\n"
        "        }"
    )
    retained_raster_callback_rejection = (
        "        if !callbacks_complete {\n"
        "            self.pending_images_for_rasterization\n"
        "                .borrow_mut()\n"
        "                .insert(key, entry);\n"
        "            return Err(());\n"
        "        }"
    )
    if retained_raster_provenance_rejection not in raster_delivery_source:
        raise ReleaseError("raster provenance rejection must restore the exact pending key")
    if retained_raster_callback_rejection not in raster_delivery_source:
        raise ReleaseError("incomplete raster callback delivery must restore the exact pending key")
    require_source_fragments_in_order(
        window_source,
        (
            "pub(crate) fn clear_js_runtime_for_script_deallocation(",
            "self.clear_retained_image_work();",
            "pub(crate) fn clear_js_runtime(&self)",
            "self.clear_retained_image_work();",
            "fn clear_retained_image_work(&self)",
            "self.pending_image_callbacks.borrow_mut().clear();",
            "self.pending_layout_images.borrow_mut().0.clear();",
            "self.pending_images_for_rasterization",
            ".clear();",
            "self.controlled_image_identities.borrow_mut().0.clear();",
            "debug_assert_eq!(self.controlled_image_retained_record_count.get(), 0)",
        ),
        "controlled image teardown",
    )

    rendering_start = script_thread_source.find("fn capture_controlled_rendering(")
    rendering_end = script_thread_source.find("fn capture_controlled_parsers(", rendering_start)
    if rendering_start < 0 or rendering_end < 0:
        raise ReleaseError("cannot locate controlled rendering pending observation")
    rendering_capture = script_thread_source[rendering_start:rendering_end]
    require_source_fragments_in_order(
        rendering_capture,
        (
            "pending_nonanimated_image_observation()",
            ".retained_work_items",
            ".controlled_work_items",
            ".unsupported_work_items",
            "!nonanimated_images.controlled_retained_record_inventory_matches",
            "controlled_image_work.checked_add(unsupported_image_work)",
            "self.document_execution_profile ==",
            "DocumentExecutionProfile::ControlledWebSessionV2",
            "unsupported_image_work",
            "retained_image_work",
            "Ok((rendering, image_terminals, controlled_image_work_items))",
        ),
        "controlled image rendering-count capture",
    )
    if "fence.snapshot()" in rendering_capture:
        raise ReleaseError("controlled rendering capture re-enters the producer fence")

    pending_start = script_thread_source.find("fn capture_controlled_pending(")
    pending_end = script_thread_source.find("fn handle_controlled_advance(", pending_start)
    if pending_start < 0 or pending_end < 0:
        raise ReleaseError("cannot locate controlled pending producer qualification")
    pending_capture = script_thread_source[pending_start:pending_end]
    require_source_fragments_in_order(
        pending_capture,
        (
            "let (rendering, image_terminals, controlled_image_work_items) =",
            "self.capture_controlled_rendering(target, &scheduler, no_gc)?",
            "let producers = match producer_capture",
            "ProducerCapture::Exact(observation) => observation",
            "if self.document_execution_profile == DocumentExecutionProfile::ControlledWebSessionV2",
            "producers.snapshot.terminal_error().is_none()",
            "producers",
            ".snapshot",
            ".for_kind(DocumentProducerKind::Image)",
            ".pending() <",
            "u64::try_from(controlled_image_work_items)",
        ),
        "controlled image qualified producer reconciliation",
    )
    reconciliation_start = pending_capture.find(
        "if self.document_execution_profile == DocumentExecutionProfile::ControlledWebSessionV2"
    )
    reconciliation_end = pending_capture.find("let owner =", reconciliation_start)
    if reconciliation_start < 0 or reconciliation_end < 0:
        raise ReleaseError("cannot bound controlled image producer reconciliation")
    if "fence.snapshot()" in pending_capture[reconciliation_start:reconciliation_end]:
        raise ReleaseError("controlled image reconciliation ignored the qualified producer observation")

    advance_start = pending_end
    advance_end = script_thread_source.find("fn send_document_control_response(", advance_start)
    if advance_start < 0 or advance_end < 0:
        raise ReleaseError("cannot locate guarded controlled advance")
    advance_source = script_thread_source[advance_start:advance_end]
    guarded_start = advance_source.find("let guarded = fence.with_matching_snapshot(token.producers().snapshot, || {")
    guarded_end = advance_source.find("let detached: DetachedTimerEvent", guarded_start)
    if guarded_start < 0 or guarded_end < 0:
        raise ReleaseError("cannot locate exact producer-fenced advance capture")
    guarded_capture = advance_source[guarded_start:guarded_end]
    require_source_fragments_in_order(
        guarded_capture,
        (
            "fence.with_matching_snapshot(token.producers().snapshot, || {",
            "self.capture_controlled_pending(",
            "ProducerCapture::Exact(token.producers())",
            "token",
            ".validate_against(&pending)",
            ".validate_advance_and_detach(token.now(), token.deadline())",
        ),
        "exact producer-fenced advance capture",
    )
    if "fence.snapshot()" in guarded_capture:
        raise ReleaseError("guarded controlled advance re-enters the producer fence")


def verify_controlled_inline_svg_rendering_source(
    svg_source: str,
    window_source: str,
    protocol_source: str,
    fixture_source: str,
    advanced_fixture_source: str,
) -> None:
    url_gate_start = svg_source.find("fn is_bounded_data_svg_url(")
    element_start = svg_source.find("#[dom_struct]", url_gate_start)
    if url_gate_start < 0 or element_start < 0:
        raise ReleaseError("cannot locate controlled inline SVG URL gate")
    require_source_fragments_in_order(
        svg_source[url_gate_start:element_start],
        (
            "serialized_url.len() > CONTROLLED_V2_INLINE_DATA_SVG_URL_LIMIT",
            "DataUrl::process(serialized_url)",
            'mime_type.type_ == "image"',
            'mime_type.subtype == "svg+xml"',
        ),
        "controlled inline SVG bounded canonical data-URL gate",
    )

    cached_gate_start = svg_source.find("pub(crate) fn controlled_v2_cached_serialized_data_url(")
    process_use_start = svg_source.find("fn process_use_elements(", cached_gate_start)
    if cached_gate_start < 0 or process_use_start < 0:
        raise ReleaseError("cannot locate controlled inline SVG cached-URL admission")
    cached_gate = svg_source[cached_gate_start:process_use_start]
    require_source_fragments_in_order(
        cached_gate,
        (
            "let document = self.owner_document()",
            "let window = document.window()",
            "!document.is_active()",
            "!ScriptThread::current_controlled_top_level_target_matches(window)",
            "cached_serialized_data_url.borrow()",
            "is_bounded_data_svg_url(url)",
            "pub(crate) fn admits_controlled_v2_serialized_data_url(",
            "is_internal_request == InternalRequest::Yes",
            "self",
            ".controlled_v2_cached_serialized_data_url()",
            ".is_some_and(|cached| cached == *candidate)",
            "pub(crate) fn record_controlled_v2_cached_vector_id(",
            "controlled_v2_cached_vector_id.replace(Some(id))",
            "previous != id",
            ".release_controlled_v2_cached_vector_identity(previous, self.upcast::<Node>())",
            "pub(crate) fn release_controlled_v2_cached_vector_id(",
            "self.controlled_v2_cached_vector_id.get() != Some(id)",
            "self.controlled_v2_cached_vector_id.set(None)",
            ".release_controlled_v2_cached_vector_identity(id, self.upcast::<Node>())",
        ),
        "controlled inline SVG exact cached-URL and identity lifecycle",
    )
    invalidate_start = svg_source.find("fn invalidate_cached_serialized_subtree_and_rasterization_result(")
    layout_impl_start = svg_source.find("impl<'dom> LayoutDom", invalidate_start)
    if invalidate_start < 0 or layout_impl_start < 0:
        raise ReleaseError("cannot locate controlled inline SVG invalidation release")
    require_source_fragments_in_order(
        svg_source[invalidate_start:layout_impl_start],
        (
            "self.controlled_v2_cached_vector_id.take()",
            "owner_window.release_controlled_v2_cached_vector_identity(",
            "id,",
            "self.upcast::<Node>()",
            "evict_rasterized_image(&self.uuid)",
            "cached_serialized_data_url.borrow()",
            "evict_completed_image(",
            "*self.cached_serialized_data_url.borrow_mut() = None",
        ),
        "controlled inline SVG generation invalidation",
    )

    admission_start = window_source.find("fn image_id_has_explicitly_unsupported_retained_work(")
    baseline_admission_start = window_source.find("fn image_id_has_baseline_retained_work(", admission_start)
    inline_svg_decode_start = window_source.find("fn admits_controlled_v2_inline_svg_decode(", baseline_admission_start)
    raster_entry_start = window_source.find("fn new_image_rasterization_entry(", admission_start)
    if (
        min(
            admission_start,
            baseline_admission_start,
            inline_svg_decode_start,
            raster_entry_start,
        )
        < 0
    ):
        raise ReleaseError("cannot locate controlled inline SVG Window admission helpers")
    require_source_fragments_in_order(
        window_source[admission_start:baseline_admission_start],
        (
            "fn image_id_has_explicitly_unsupported_retained_work(",
            "pending_image_callbacks",
            ".get(&id)",
            "callback.provenance == PendingImageProvenance::Unsupported",
            "pending_layout_images",
            ".get(&id)",
            "owner.provenance == PendingImageProvenance::Unsupported",
            "pending_images_for_rasterization",
            "*candidate_id == id",
            "entry.provenance == PendingImageProvenance::Unsupported",
        ),
        "explicit Unsupported exact cache-ID classifier",
    )
    require_source_fragments_in_order(
        window_source[baseline_admission_start:inline_svg_decode_start],
        (
            "fn image_id_has_baseline_retained_work(",
            "pending_image_callbacks",
            ".get(&id)",
            "callback.provenance != PendingImageProvenance::ControlledV2Fenced",
            "pending_layout_images",
            ".get(&id)",
            "owner.provenance != PendingImageProvenance::ControlledV2Fenced",
            "pending_images_for_rasterization",
            "*candidate_id == id",
            "entry.provenance != PendingImageProvenance::ControlledV2Fenced",
        ),
        "unadmitted exact cache-ID classifier",
    )
    require_source_fragments_in_order(
        window_source[inline_svg_decode_start:raster_entry_start],
        (
            "fn admits_controlled_v2_inline_svg_decode(",
            "std::ptr::eq(self, node.owner_document().window())",
            "node.downcast::<SVGSVGElement>()",
            "svg.admits_controlled_v2_serialized_data_url(url, is_internal_request)",
            "fn admits_controlled_v2_inline_svg_raster(",
            "std::ptr::eq(self, node.owner_document().window())",
            "svg.controlled_v2_cached_serialized_data_url()",
            "image_cache.get_image(url, self.origin().immutable().clone(), None)",
            "Some(Image::Vector(vector)) if vector.id == id",
        ),
        "controlled inline SVG same-Window decode and exact raster cache-ID admission",
    )

    post_reflow_start = window_source.find("fn handle_pending_images_post_reflow(")
    post_reflow_end = window_source.find(
        "/// <https://html.spec.whatwg.org/multipage/#sticky-activation>",
        post_reflow_start,
    )
    if post_reflow_start < 0 or post_reflow_end < 0:
        raise ReleaseError("cannot locate controlled inline SVG post-reflow ownership")
    post_reflow = window_source[post_reflow_start:post_reflow_end]
    controlled_decode_start = post_reflow.find("let mut controlled_inline_svg_fetches_started = HashSet::new()")
    baseline_decode_start = post_reflow.find(
        "// Preserve the predecessor fetch-before-listener ordering",
        controlled_decode_start,
    )
    raster_start = post_reflow.find("for image in pending_rasterization_images", baseline_decode_start)
    serialization_start = post_reflow.find("for node in pending_svg_element_for_serialization", raster_start)
    if (
        min(
            controlled_decode_start,
            baseline_decode_start,
            raster_start,
            serialization_start,
        )
        < 0
    ):
        raise ReleaseError("cannot separate controlled inline SVG post-reflow phases")
    controlled_decode = post_reflow[controlled_decode_start:baseline_decode_start]
    require_source_fragments_in_order(
        controlled_decode,
        (
            "controlled_inline_svg_fetches_started = HashSet::new()",
            "let controlled_transport_is_exact = if needs_listener",
            "!self.pending_image_callbacks.borrow().contains_key(&id)",
            "controlled_inline_svg_fetches_started.contains(&id)",
            "self.admits_controlled_v2_inline_svg_decode(",
            "!self.image_id_has_baseline_retained_work(id)",
            "let Some(svg) = node.downcast::<SVGSVGElement>()",
            "self.retained_image_provenance(id, &node)",
            ".retain_controlled_v2_cached_vector_identity(id, &node)",
            "self.register_controlled_v2_image_cache_listener(",
            "self.release_controlled_v2_cached_vector_identity(id, &node)",
            "PendingImageProvenance::ControlledV2Fenced",
            "svg.record_controlled_v2_cached_vector_id(id)",
            "image_cache.add_listener(ImageLoadListener::new(sender, pipeline_id, id))",
            "fetch_image_for_layout(",
            "controlled_inline_svg_fetches_started.insert(id)",
            "continue;",
        ),
        "controlled inline SVG identity/listener/fetch ordering",
    )
    if controlled_decode.count("fetch_image_for_layout(") != 1:
        raise ReleaseError("controlled inline SVG decode must start exactly one fetch after fenced admission")
    if any(
        event_fragment in controlled_decode for event_fragment in ("Event::new(", "fire_image_event(", 'atom!("load")')
    ):
        raise ReleaseError("controlled inline SVG decode invents a DOM completion event")

    baseline_decode = post_reflow[baseline_decode_start:raster_start]
    require_source_fragments_in_order(
        baseline_decode,
        (
            "if let Some(url) = unrequested_url",
            "fetch_image_for_layout(",
            "let provenance = self.retained_image_provenance(id, &node)",
            "self.downgrade_cached_vector_identity_to_baseline(id)",
            "self.register_image_cache_listener(",
        ),
        "unchanged baseline inline image fetch-before-listener authority",
    )

    raster = post_reflow[raster_start:serialization_start]
    require_source_fragments_in_order(
        raster,
        (
            "let mut provenance = self.retained_image_provenance(image.id, &node)",
            "!self.image_id_has_baseline_retained_work(image.id)",
            "self.admits_controlled_v2_inline_svg_raster(",
            "let Some(svg) = node.downcast::<SVGSVGElement>()",
            ".retain_controlled_v2_cached_vector_identity(image.id, &node)",
            "provenance = PendingImageProvenance::ControlledV2Fenced",
            "svg.record_controlled_v2_cached_vector_id(image.id)",
            "self.downgrade_cached_vector_identity_to_baseline(image.id)",
            "self.new_image_rasterization_entry(provenance)",
            "svg.release_controlled_v2_cached_vector_id(image.id)",
            ".insert(key, entry)",
            "image_cache.add_rasterization_complete_listener(",
        ),
        "controlled inline SVG synchronous cache-hit raster ownership",
    )

    require_source_fragments_in_order(
        svg_source,
        (
            "fn bounded_data_svg_gate_rejects_other_schemes_mime_types_and_oversize_urls()",
            'ServoUrl::parse("data:image/svg+xml;base64,PHN2Zy8+")',
            'ServoUrl::parse("data:image/png;base64,AA==")',
            'ServoUrl::parse("https://example.test/image.svg")',
            "CONTROLLED_V2_INLINE_DATA_SVG_URL_LIMIT",
            "assert!(!is_bounded_data_svg_url(&oversize))",
        ),
        "controlled inline SVG native rejection unit proof",
    )
    require_source_fragments_in_order(
        protocol_source,
        (
            'include_bytes!("fixtures/controlled_v2_inline_svg.html")',
            'include_bytes!("fixtures/controlled_v2_inline_svg_advanced.html")',
            "fn controlled_session_v2_direct_data_svg_is_owned_without_v1_promotion()",
            '"controlled-web-session-v2"',
            'ControlledImageProfileExpectation::Owned("load:0>loadend:0|now:0")',
            '"controlled-web-session-v1"',
            "ControlledImageProfileExpectation::Unsupported",
            "fn controlled_session_v2_inline_svg_rendering_is_owned_without_v1_promotion()",
            '"controlled-web-session-v2"',
            'ControlledImageProfileExpectation::Owned("inline-svg:4x3|events:0|now:0")',
            '"controlled-web-session-v1"',
            "CONTROLLED_V2_INLINE_SVG_FIXTURE",
            "ControlledImageProfileExpectation::PredecessorMayQuiesce(",
            '"inline-svg:4x3|events:0|now:0"',
            "fn controlled_session_v2_inline_svg_raster_completes_after_advanced_document_time()",
            "fn exercise_controlled_inline_svg_advanced()",
            '"5000000"',
            'settled["result"]["snapshot"]["producers"]["pending"]',
            'settled["result"]["snapshot"]["rendering"]["updateRequired"]',
            '"inline-svg:5|load-events:0"',
            "enum ControlledImageProfileExpectation<'a>",
            "PredecessorMayQuiesce(&'a str)",
            'if opened["error"]["code"] == "unsupported_work"',
            "ControlledImageProfileExpectation::Unsupported |",
            "ControlledImageProfileExpectation::PredecessorMayQuiesce(_)",
            "v2-owned image work must not fall back to predecessor rejection",
            "ControlledImageProfileExpectation::Owned(expected_text) |",
            "ControlledImageProfileExpectation::PredecessorMayQuiesce(expected_text)",
            "predecessor image work must remain typed unsupported",
            'opened["result"]["boundary"], "controlled_ready"',
            "successful predecessor retirement must still cross the exact controlled-ready boundary",
            'settled["result"]["unsupportedWork"]',
            'settled["result"]["externalIo"]',
            'settled["result"]["snapshot"]["producers"]["pending"]',
            'settled["result"]["snapshot"]["producers"]["terminal"]',
            'settled["result"]["snapshot"]["rendering"]["pendingImages"]',
            'settled["result"]["snapshot"]["rendering"]["updateRequired"]',
            'settled["result"]["snapshot"]["rendering"]["nextOpportunityNs"]',
            'settled["result"]["snapshot"]["runtimeFailures"]',
            'text["result"]["value"], expected_text',
        ),
        "controlled direct and inline SVG owned, predecessor, and advanced protocol proof",
    )
    require_source_fragments_in_order(
        fixture_source,
        (
            '<svg id="inline-svg" width="4" height="3"',
            '"inline-svg:4x3"',
            "`events:${completionEvents.length}`",
            "`now:${performance.now()}`",
            'for (const type of ["load", "error", "loadend"])',
            "svg.addEventListener(type",
            "svg.getBoundingClientRect()",
        ),
        "controlled inline SVG zero-event fixture",
    )
    require_source_fragments_in_order(
        advanced_fixture_source,
        (
            'document.querySelector("#start").onclick',
            "setTimeout(() =>",
            'svg.setAttribute("width", "119")',
            'svg.setAttribute("height", "48")',
            'svg.addEventListener("load"',
            "document.body.append(svg)",
            "`inline-svg:${performance.now()}|load-events:${loadEvents}`",
            ", 5)",
        ),
        "controlled inline SVG advanced clock and no-event fixture",
    )


def rust_usize_constant(source: str, name: str, description: str) -> int:
    match = re.search(
        rf"(?m)^const {re.escape(name)}: usize = ([0-9_]+(?:\s*\*\s*[0-9_]+)*);$",
        source,
    )
    if match is None:
        raise ReleaseError(f"cannot locate {description} native constant {name}")
    result = 1
    for factor in match.group(1).split("*"):
        result *= int(factor.strip().replace("_", ""))
    return result


def controlled_webapp_ordinary_task_limit(source: str) -> int:
    block = re.search(
        r"pub const CONTROLLED_WEBAPP_V1: Self = Self \{(?P<body>.*?)\n\s*\};",
        source,
        flags=re.DOTALL,
    )
    if block is None:
        raise ReleaseError("cannot locate the native controlled execution-limit defaults")
    ordinary_tasks = re.search(r"ordinary_tasks:\s*([0-9_]+),", block.group("body"))
    if ordinary_tasks is None:
        raise ReleaseError("cannot locate the native ordinary-task budget")
    return int(ordinary_tasks.group(1).replace("_", ""))


def controlled_webapp_rendering_opportunity_limit(source: str) -> int:
    block = re.search(
        r"pub const CONTROLLED_WEBAPP_V1: Self = Self \{(?P<body>.*?)\n\s*\};",
        source,
        flags=re.DOTALL,
    )
    if block is None:
        raise ReleaseError("cannot locate the native controlled execution-limit defaults")
    rendering_opportunities = re.search(r"rendering_opportunities:\s*([0-9_]+),", block.group("body"))
    if rendering_opportunities is None:
        raise ReleaseError("cannot locate the native rendering-opportunity budget")
    return int(rendering_opportunities.group(1).replace("_", ""))


def require_public_surface_markers(
    source: str,
    description: str,
    markers: tuple[str, ...],
    *,
    forbidden: tuple[str, ...] = (),
) -> None:
    for marker in markers:
        if marker not in source:
            raise ReleaseError(f"{description} is missing the canonical public marker {marker!r}")
    for marker in forbidden:
        if marker in source:
            raise ReleaseError(f"{description} retains the contradictory public marker {marker!r}")


def credential_free_v2_automation_verifier_block(source: str, description: str) -> str:
    start = 'automation_record = document.get("v2AutomationEventTimestamps")'
    end = 'css_record = document.get("v2CssAnimationEventTimestamps")'
    if source.count(start) != 1 or source.count(end) != 1:
        raise ReleaseError(f"{description} does not contain one v2 automation verifier block")
    start_index = source.index(start)
    end_index = source.index(end, start_index)
    block = source[start_index:end_index]
    require_public_surface_markers(
        block,
        description,
        (
            'automation_dynamic_fields = set(automation_time_fields) | {"controlledTrace"}',
            'r"(?:0|[1-9][0-9]*)", automation_record[field]',
            "max_u128 = (1 << 128) - 1",
            "virtual_time_ns > max_u128",
            "!= initial_virtual_time_ns + 5_000_000",
            "dispatched_virtual_time_ns != advanced_virtual_time_ns",
            'controlled_trace_parts = controlled_trace.split("|")',
            'controlled_trace_parts[2] != "not-read"',
            "controlled_event_time_ms != controlled_baseline_time_ms + 5",
            "controlled_baseline_time_ms * 1_000_000",
            ">= initial_virtual_time_ns",
            "controlled_trace != expected_controlled_trace",
            '"fill:input"',
            '"submit:formdata"',
        ),
        forbidden=(
            '"initialVirtualTimeNs": "140000000"',
            '"advancedVirtualTimeNs": "145000000"',
            '"dispatchedVirtualTimeNs": "145000000"',
            '"initialVirtualTimeNs": "180000000"',
            '"advancedVirtualTimeNs": "185000000"',
            '"dispatchedVirtualTimeNs": "185000000"',
            '"controlledTrace": (',
        ),
    )
    return block


def verify_candidate_v2_profile(source_root: Path) -> dict[str, object]:
    profile_filename = source_root / CANDIDATE_V2_PROFILE
    contract_filename = source_root / CANDIDATE_V2_CONTRACT
    public_top_level_readme_filename = source_root / PUBLIC_TOP_LEVEL_README
    public_stasis_boundary_filename = source_root / PUBLIC_STASIS_BOUNDARY
    public_profile_readme_filename = source_root / PUBLIC_PROFILE_README
    public_typescript_sdk_readme_filename = source_root / PUBLIC_TYPESCRIPT_SDK_README
    public_release_runbook_filename = source_root / PUBLIC_RELEASE_RUNBOOK
    public_release_workflow_filename = source_root / PUBLIC_RELEASE_WORKFLOW
    public_npm_publish_workflow_filename = source_root / PUBLIC_NPM_PUBLISH_WORKFLOW
    message_limits_filename = source_root / MESSAGE_CHANNEL_LIMITS_SOURCE
    message_baseline_test_filename = source_root / MESSAGE_CHANNEL_BASELINE_TEST_SOURCE
    message_multi_pair_fixture_filename = source_root / MESSAGE_CHANNEL_MULTI_PAIR_FIXTURE
    message_channel_filename = source_root / MESSAGE_CHANNEL_SOURCE
    structured_clone_filename = source_root / STRUCTURED_CLONE_SOURCE
    message_port_filename = source_root / MESSAGE_PORT_SOURCE
    input_method_control_filename = source_root / INPUT_METHOD_CONTROL_SOURCE
    input_method_input_filename = source_root / INPUT_METHOD_INPUT_SOURCE
    input_method_textarea_filename = source_root / INPUT_METHOD_TEXTAREA_SOURCE
    event_filename = source_root / EVENT_SOURCE
    focus_event_filename = source_root / FOCUS_EVENT_SOURCE
    controlled_automation_filename = source_root / CONTROLLED_AUTOMATION_SOURCE
    controlled_automation_event_target_filename = source_root / CONTROLLED_AUTOMATION_EVENT_TARGET_SOURCE
    controlled_automation_input_event_filename = source_root / CONTROLLED_AUTOMATION_INPUT_EVENT_SOURCE
    controlled_automation_pointer_event_filename = source_root / CONTROLLED_AUTOMATION_POINTER_EVENT_SOURCE
    controlled_automation_submit_event_filename = source_root / CONTROLLED_AUTOMATION_SUBMIT_EVENT_SOURCE
    controlled_automation_form_data_event_filename = source_root / CONTROLLED_AUTOMATION_FORM_DATA_EVENT_SOURCE
    controlled_automation_event_fixture_filename = source_root / CONTROLLED_AUTOMATION_EVENT_FIXTURE
    controlled_css_animation_filename = source_root / CONTROLLED_CSS_ANIMATION_SOURCE
    controlled_css_animation_event_filename = source_root / CONTROLLED_CSS_ANIMATION_EVENT_SOURCE
    controlled_css_transition_event_filename = source_root / CONTROLLED_CSS_TRANSITION_EVENT_SOURCE
    controlled_css_document_filename = source_root / CONTROLLED_CSS_DOCUMENT_SOURCE
    controlled_css_animation_event_fixture_filename = source_root / CONTROLLED_CSS_ANIMATION_EVENT_FIXTURE
    controlled_rendering_settlement_filename = source_root / CONTROLLED_RENDERING_SETTLEMENT_SOURCE
    controlled_session_shell_filename = source_root / CONTROLLED_SESSION_SHELL_SOURCE
    controlled_image_element_filename = source_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
    controlled_image_window_filename = source_root / CONTROLLED_IMAGE_WINDOW_SOURCE
    controlled_image_script_thread_filename = source_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
    controlled_image_messaging_filename = source_root / CONTROLLED_IMAGE_MESSAGING_SOURCE
    controlled_image_producer_fence_filename = source_root / CONTROLLED_IMAGE_PRODUCER_FENCE_SOURCE
    controlled_image_cache_filename = source_root / CONTROLLED_IMAGE_CACHE_SOURCE
    controlled_profile_wire_filename = source_root / CONTROLLED_PROFILE_WIRE_SOURCE
    controlled_http_image_fixture_filename = source_root / CONTROLLED_HTTP_IMAGE_FIXTURE
    controlled_http_image_multipart_fixture_filename = source_root / CONTROLLED_HTTP_IMAGE_MULTIPART_FIXTURE
    controlled_inline_svg_filename = source_root / CONTROLLED_INLINE_SVG_SOURCE
    controlled_inline_svg_fixture_filename = source_root / CONTROLLED_INLINE_SVG_FIXTURE
    controlled_inline_svg_advanced_fixture_filename = source_root / CONTROLLED_INLINE_SVG_ADVANCED_FIXTURE
    execution_limits_filename = source_root / EXECUTION_LIMITS_SOURCE
    require_regular_file(profile_filename, "controlled-web-session-v2 candidate profile")
    require_regular_file(contract_filename, "controlled-web-session-v2 candidate contract")
    require_regular_file(public_top_level_readme_filename, "top-level public README")
    require_regular_file(public_stasis_boundary_filename, "public Stasis product boundary")
    require_regular_file(public_profile_readme_filename, "public profile README")
    require_regular_file(public_typescript_sdk_readme_filename, "public TypeScript SDK README")
    require_regular_file(public_release_runbook_filename, "public release runbook")
    require_regular_file(public_release_workflow_filename, "public release-note workflow")
    require_regular_file(public_npm_publish_workflow_filename, "public npm publish workflow")
    require_regular_file(message_limits_filename, "MessageChannel native limit source")
    require_regular_file(message_baseline_test_filename, "MessageChannel native baseline proof source")
    require_regular_file(message_multi_pair_fixture_filename, "MessageChannel multi-pair fixture")
    require_regular_file(message_channel_filename, "MessageChannel constructor source")
    require_regular_file(structured_clone_filename, "structured-clone transfer source")
    require_regular_file(message_port_filename, "MessagePort postMessage source")
    require_regular_file(input_method_control_filename, "controlled InputMethod source")
    require_regular_file(input_method_input_filename, "HTMLInput InputMethod producer")
    require_regular_file(input_method_textarea_filename, "HTMLTextArea InputMethod producer")
    require_regular_file(event_filename, "controlled Event timestamp source")
    require_regular_file(focus_event_filename, "controlled FocusEvent timestamp source")
    require_regular_file(controlled_automation_filename, "controlled automation event source")
    require_regular_file(
        controlled_automation_event_target_filename,
        "controlled simple automation event source",
    )
    require_regular_file(
        controlled_automation_input_event_filename,
        "controlled automation InputEvent source",
    )
    require_regular_file(
        controlled_automation_pointer_event_filename,
        "controlled automation PointerEvent source",
    )
    require_regular_file(
        controlled_automation_submit_event_filename,
        "controlled automation SubmitEvent source",
    )
    require_regular_file(
        controlled_automation_form_data_event_filename,
        "controlled automation FormDataEvent source",
    )
    require_regular_file(
        controlled_automation_event_fixture_filename,
        "controlled automation event timestamp fixture",
    )
    require_regular_file(controlled_css_animation_filename, "controlled CSS animation dispatch source")
    require_regular_file(
        controlled_css_animation_event_filename,
        "controlled internal AnimationEvent source",
    )
    require_regular_file(
        controlled_css_transition_event_filename,
        "controlled internal TransitionEvent source",
    )
    require_regular_file(controlled_css_document_filename, "controlled CSS rendering observation source")
    require_regular_file(
        controlled_css_animation_event_fixture_filename,
        "controlled CSS animation event timestamp fixture",
    )
    require_regular_file(
        controlled_rendering_settlement_filename,
        "controlled rendering settlement source",
    )
    require_regular_file(
        controlled_session_shell_filename,
        "controlled session shell source",
    )
    require_regular_file(controlled_image_element_filename, "controlled HTMLImageElement source")
    require_regular_file(controlled_image_window_filename, "controlled image Window source")
    require_regular_file(
        controlled_image_script_thread_filename,
        "controlled image ScriptThread source",
    )
    require_regular_file(controlled_image_messaging_filename, "controlled image transport source")
    require_regular_file(
        controlled_image_producer_fence_filename,
        "controlled image producer-fence source",
    )
    require_regular_file(
        controlled_image_cache_filename,
        "controlled per-pipeline image-cache source",
    )
    require_regular_file(
        controlled_profile_wire_filename,
        "controlled exact profile wire assertion source",
    )
    require_regular_file(
        controlled_http_image_fixture_filename,
        "controlled HTTP image protocol fixture",
    )
    require_regular_file(
        controlled_http_image_multipart_fixture_filename,
        "controlled HTTP multipart image protocol fixture",
    )
    require_regular_file(controlled_inline_svg_filename, "controlled inline SVG source")
    require_regular_file(
        controlled_inline_svg_fixture_filename,
        "controlled inline SVG protocol fixture",
    )
    require_regular_file(
        controlled_inline_svg_advanced_fixture_filename,
        "controlled inline SVG advanced protocol fixture",
    )
    require_regular_file(execution_limits_filename, "controlled execution-limit source")
    if contract_filename.stat().st_size <= 0 or contract_filename.stat().st_size > MAX_TEXT_MEMBER_BYTES:
        raise ReleaseError("controlled-web-session-v2 candidate contract has an invalid size")
    try:
        profile_text = profile_filename.read_text(encoding="utf-8")
        contract_text = contract_filename.read_text(encoding="utf-8")
        public_top_level_readme = public_top_level_readme_filename.read_text(encoding="utf-8")
        public_stasis_boundary = public_stasis_boundary_filename.read_text(encoding="utf-8")
        public_profile_readme = public_profile_readme_filename.read_text(encoding="utf-8")
        public_typescript_sdk_readme = public_typescript_sdk_readme_filename.read_text(encoding="utf-8")
        public_release_runbook = public_release_runbook_filename.read_text(encoding="utf-8")
        public_release_workflow = public_release_workflow_filename.read_text(encoding="utf-8")
        public_npm_publish_workflow = public_npm_publish_workflow_filename.read_text(encoding="utf-8")
        message_limits_source = message_limits_filename.read_text(encoding="utf-8")
        message_baseline_test_source = message_baseline_test_filename.read_text(encoding="utf-8")
        message_multi_pair_fixture_source = message_multi_pair_fixture_filename.read_text(encoding="utf-8")
        message_channel_source = message_channel_filename.read_text(encoding="utf-8")
        structured_clone_source = structured_clone_filename.read_text(encoding="utf-8")
        message_port_source = message_port_filename.read_text(encoding="utf-8")
        input_method_control_source = input_method_control_filename.read_text(encoding="utf-8")
        input_method_input_source = input_method_input_filename.read_text(encoding="utf-8")
        input_method_textarea_source = input_method_textarea_filename.read_text(encoding="utf-8")
        event_source = event_filename.read_text(encoding="utf-8")
        focus_event_source = focus_event_filename.read_text(encoding="utf-8")
        controlled_automation_source = controlled_automation_filename.read_text(encoding="utf-8")
        controlled_automation_event_target_source = controlled_automation_event_target_filename.read_text(
            encoding="utf-8"
        )
        controlled_automation_input_event_source = controlled_automation_input_event_filename.read_text(
            encoding="utf-8"
        )
        controlled_automation_pointer_event_source = controlled_automation_pointer_event_filename.read_text(
            encoding="utf-8"
        )
        controlled_automation_submit_event_source = controlled_automation_submit_event_filename.read_text(
            encoding="utf-8"
        )
        controlled_automation_form_data_event_source = controlled_automation_form_data_event_filename.read_text(
            encoding="utf-8"
        )
        controlled_automation_event_fixture_source = controlled_automation_event_fixture_filename.read_text(
            encoding="utf-8"
        )
        controlled_css_animation_source = controlled_css_animation_filename.read_text(encoding="utf-8")
        controlled_css_animation_event_source = controlled_css_animation_event_filename.read_text(encoding="utf-8")
        controlled_css_transition_event_source = controlled_css_transition_event_filename.read_text(encoding="utf-8")
        controlled_css_document_source = controlled_css_document_filename.read_text(encoding="utf-8")
        controlled_css_animation_event_fixture_source = controlled_css_animation_event_fixture_filename.read_text(
            encoding="utf-8"
        )
        controlled_rendering_settlement_source = controlled_rendering_settlement_filename.read_text(encoding="utf-8")
        controlled_session_shell_source = controlled_session_shell_filename.read_text(encoding="utf-8")
        controlled_image_element_source = controlled_image_element_filename.read_text(encoding="utf-8")
        controlled_image_window_source = controlled_image_window_filename.read_text(encoding="utf-8")
        controlled_image_script_thread_source = controlled_image_script_thread_filename.read_text(encoding="utf-8")
        controlled_image_messaging_source = controlled_image_messaging_filename.read_text(encoding="utf-8")
        controlled_image_producer_fence_source = controlled_image_producer_fence_filename.read_text(encoding="utf-8")
        controlled_image_cache_source = controlled_image_cache_filename.read_text(encoding="utf-8")
        controlled_profile_wire_source = controlled_profile_wire_filename.read_text(encoding="utf-8")
        controlled_http_image_fixture_source = controlled_http_image_fixture_filename.read_text(encoding="utf-8")
        controlled_http_image_multipart_fixture_source = controlled_http_image_multipart_fixture_filename.read_text(
            encoding="utf-8"
        )
        controlled_inline_svg_source = controlled_inline_svg_filename.read_text(encoding="utf-8")
        controlled_inline_svg_fixture_source = controlled_inline_svg_fixture_filename.read_text(encoding="utf-8")
        controlled_inline_svg_advanced_fixture_source = controlled_inline_svg_advanced_fixture_filename.read_text(
            encoding="utf-8"
        )
        execution_limits_source = execution_limits_filename.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise ReleaseError(f"cannot read the controlled-web-session-v2 authority inputs: {error}") from error
    profile_sha256 = sha256_file(profile_filename)
    if profile_sha256 != CANDIDATE_V2_PROFILE_SHA256:
        raise ReleaseError(
            "controlled-web-session-v2 candidate profile changed: "
            f"expected {CANDIDATE_V2_PROFILE_SHA256}, got {profile_sha256}"
        )
    contract_sha256 = sha256_file(contract_filename)
    if contract_sha256 != CANDIDATE_V2_CONTRACT_SHA256:
        raise ReleaseError(
            "controlled-web-session-v2 candidate contract changed: "
            f"expected {CANDIDATE_V2_CONTRACT_SHA256}, got {contract_sha256}"
        )
    profile = require_json_object(
        strict_json_loads(profile_text, "controlled-web-session-v2 candidate profile"),
        "controlled-web-session-v2 candidate profile",
    )
    compatibility = require_json_object(profile.get("compatibility"), "controlled-web-session-v2 compatibility")
    session_state = require_json_object(profile.get("sessionState"), "controlled-web-session-v2 sessionState")
    session_cookies = require_json_object(session_state.get("cookies"), "controlled-web-session-v2 session cookies")
    cookie_persistence = require_json_object(
        session_cookies.get("persistence"),
        "controlled-web-session-v2 cookie persistence",
    )
    cookie_time_range = require_json_object(
        session_cookies.get("timeRange"),
        "controlled-web-session-v2 cookie time range",
    )
    cookie_post_open_time_range = require_json_object(
        cookie_time_range.get("postOpenNetworkRequestAboveMaximum"),
        "controlled-web-session-v2 post-open cookie time-range boundary",
    )
    cookie_same_site = require_json_object(
        session_cookies.get("sameSite"),
        "controlled-web-session-v2 SameSite policy",
    )
    cookie_same_site_context = require_json_object(
        cookie_same_site.get("context"),
        "controlled-web-session-v2 SameSite request context",
    )
    cookie_same_site_response_storage = require_json_object(
        cookie_same_site.get("responseStorage"),
        "controlled-web-session-v2 SameSite response-storage policy",
    )
    cookie_unknown_or_opaque_context = require_json_object(
        cookie_same_site_context.get("unknownOrOpaque"),
        "controlled-web-session-v2 opaque SameSite context boundary",
    )
    cookie_set_cookie = require_json_object(
        session_cookies.get("setCookie"),
        "controlled-web-session-v2 Set-Cookie policy",
    )
    cookie_page_api = require_json_object(
        session_cookies.get("pageApi"),
        "controlled-web-session-v2 page cookie API",
    )
    cookie_store = require_json_object(
        cookie_page_api.get("cookieStore"),
        "controlled-web-session-v2 Cookie Store boundary",
    )
    execution = require_json_object(profile.get("execution"), "controlled-web-session-v2 execution")
    unsupported_classes = require_json_object(
        profile.get("unsupportedClasses"),
        "controlled-web-session-v2 unsupported classes",
    )
    embedder_controls = require_json_object(
        unsupported_classes.get("embedderControls"),
        "controlled-web-session-v2 embedder-control boundary",
    )
    transfer_preflight = require_json_object(
        execution.get("portBackedStructuredCloneTransferPreflight"),
        "controlled-web-session-v2 port-backed transfer preflight",
    )
    controlled_input_method = require_json_object(
        execution.get("controlledInputMethodFocus"),
        "controlled-web-session-v2 InputMethod focus boundary",
    )
    controlled_focus_event_timestamp = require_json_object(
        execution.get("controlledFocusEventTimestamp"),
        "controlled-web-session-v2 FocusEvent timestamp boundary",
    )
    controlled_automation_event_timestamps = require_json_object(
        execution.get("controlledAutomationEventTimestamps"),
        "controlled-web-session-v2 automation event timestamp boundary",
    )
    controlled_automation_event_seams = require_json_object(
        controlled_automation_event_timestamps.get("implementationSeams"),
        "controlled-web-session-v2 automation event constructor seams",
    )
    controlled_css_animation_event_timestamps = require_json_object(
        execution.get("controlledCssAnimationEventTimestamps"),
        "controlled-web-session-v2 CSS animation event timestamp boundary",
    )
    controlled_css_settlement_scheduling = require_json_object(
        controlled_css_animation_event_timestamps.get("settlementScheduling"),
        "controlled-web-session-v2 CSS pending-event settlement scheduling",
    )
    controlled_image_element = require_json_object(
        execution.get("controlledImageElement"),
        "controlled-web-session-v2 HTMLImageElement boundary",
    )
    controlled_image_selection = require_json_object(
        controlled_image_element.get("selection"),
        "controlled-web-session-v2 HTMLImageElement selection",
    )
    controlled_image_retention = require_json_object(
        controlled_image_element.get("retention"),
        "controlled-web-session-v2 HTMLImageElement retention",
    )
    controlled_image_completion = require_json_object(
        controlled_image_element.get("completion"),
        "controlled-web-session-v2 HTMLImageElement completion",
    )
    controlled_image_pending = require_json_object(
        controlled_image_element.get("pending"),
        "controlled-web-session-v2 HTMLImageElement pending authority",
    )
    controlled_image_event_timestamp = require_json_object(
        controlled_image_element.get("eventTimestamp"),
        "controlled-web-session-v2 HTMLImageElement event timestamp",
    )
    controlled_image_unsupported = require_json_object(
        controlled_image_element.get("unsupported"),
        "controlled-web-session-v2 HTMLImageElement unsupported boundary",
    )
    controlled_inline_svg = require_json_object(
        execution.get("controlledInlineSvgRendering"),
        "controlled-web-session-v2 inline SVG rendering boundary",
    )
    controlled_inline_svg_admission = require_json_object(
        controlled_inline_svg.get("admission"),
        "controlled-web-session-v2 inline SVG admission",
    )
    controlled_inline_svg_ownership = require_json_object(
        controlled_inline_svg.get("ownership"),
        "controlled-web-session-v2 inline SVG ownership",
    )
    controlled_inline_svg_completion = require_json_object(
        controlled_inline_svg.get("completion"),
        "controlled-web-session-v2 inline SVG completion",
    )
    controlled_inline_svg_unsupported = require_json_object(
        controlled_inline_svg.get("unsupported"),
        "controlled-web-session-v2 inline SVG unsupported boundary",
    )
    host_timestamp_boundary = require_json_object(
        unsupported_classes.get("hostTimestamp"),
        "controlled-web-session-v2 host-timestamp boundary",
    )
    image_element_unsupported_class = require_json_object(
        unsupported_classes.get("imageElement"),
        "controlled-web-session-v2 image-element unsupported class",
    )
    supported_product_surface = profile.get("supportedProductSurface")
    if type(supported_product_surface) is not list or any(
        type(entry) is not str for entry in supported_product_surface
    ):
        raise ReleaseError("controlled-web-session-v2 supportedProductSurface must be an array of strings")
    message_channel = require_json_object(execution.get("messageChannel"), "controlled-web-session-v2 messageChannel")
    construction = require_json_object(
        message_channel.get("construction"), "controlled-web-session-v2 MessageChannel construction"
    )
    post_message = require_json_object(
        message_channel.get("postMessage"), "controlled-web-session-v2 MessageChannel postMessage"
    )
    delivery = require_json_object(message_channel.get("delivery"), "controlled-web-session-v2 MessageChannel delivery")
    retained_work_projection = require_json_object(
        delivery.get("retainedWorkProjection"),
        "controlled-web-session-v2 retained MessagePort work projection",
    )
    unsupported = require_json_object(
        message_channel.get("unsupported"),
        "controlled-web-session-v2 MessageChannel unsupported boundary",
    )
    execution_limits = require_json_object(profile.get("executionLimits"), "controlled-web-session-v2 executionLimits")
    expected_identity = {
        "id": "controlled-web-session-v2",
        "releaseStatus": "stable_contract",
        "targetRelease": RELEASE_VERSION,
    }
    for field, expected in expected_identity.items():
        if profile.get(field) != expected:
            raise ReleaseError(f"controlled-web-session-v2 {field} must be {expected!r}")
    if compatibility.get("predecessor") != "controlled-web-session-v1":
        raise ReleaseError("controlled-web-session-v2 predecessor must be controlled-web-session-v1")
    if compatibility.get("predecessorProfileSha256") != FROZEN_V2_PROFILE_SHA256:
        raise ReleaseError(
            "controlled-web-session-v2 predecessorProfileSha256 must pin the frozen controlled-web-session-v1 profile"
        )
    if compatibility.get("predecessorContractUnchanged") is not True:
        raise ReleaseError("controlled-web-session-v2 predecessorContractUnchanged must remain true")
    if compatibility.get("profileExpansion") != (
        "execution_headless_presentation_and_controlled_cookie_state_surfaces"
    ):
        raise ReleaseError("controlled-web-session-v2 profile expansion identity changed")
    if compatibility.get("stateArtifactProfile") != ("selected_controlled_session_profile_exact"):
        raise ReleaseError("controlled-web-session-v2 state artifact selection identity changed")
    require_exact_fields(
        cookie_persistence,
        {
            "mode": "controlled_session_owned_expiry",
            "storage": "memory_only_for_session_lifetime",
            "diskOrHostPersistence": False,
            "portablePersistence": ("explicit_profile_v2_state_export_and_initial_import_only"),
            "clock": "controlled_unix_time_ns_with_origin_zero",
            "maxAgePrecedence": "last_valid_Max-Age_over_Expires",
            "maximumLifetimeSeconds": 34_560_000,
            "clamp": "now_plus_400_days",
            "purge": ("lazily_before_cookie_observation_request_selection_and_state_export"),
            "expiresAtOrBeforeNow": "delete_instead_of_retain",
        },
        "controlled-web-session-v2 cookie persistence",
    )
    require_exact_fields(
        cookie_post_open_time_range,
        {
            "code": "unsupported_cookie_time_range",
            "boundary": "before_network_start_and_cookie_header_construction",
            "fatal": False,
            "stateEffect": "partial",
            "processEffect": "continue",
        },
        "controlled-web-session-v2 post-open cookie time-range boundary",
    )
    require_exact_fields(
        cookie_time_range,
        {
            "maximumControlledUnixTimeNsInclusive": "18446744073709551615",
            "postOpenNetworkRequestAboveMaximum": cookie_post_open_time_range,
            "controlledOpenFailurePrecedence": ("same_code_hardened_to_fatal_fail_stop"),
            "persistentExpiryWithoutU64Headroom": "unsupported_cookie_time_range",
            "pageApiDomException": "NotSupportedError",
        },
        "controlled-web-session-v2 cookie time range",
    )
    require_exact_fields(
        cookie_unknown_or_opaque_context,
        {
            "code": "unsupported_cookie_same_site_context",
            "boundary": "before_network_start_and_cookie_header_construction",
        },
        "controlled-web-session-v2 opaque SameSite context boundary",
    )
    require_exact_fields(
        cookie_same_site_context,
        {
            "requestClient": "captured_at_request_creation",
            "siteForCookies": "captured_from_request_client",
            "requestMethod": "current_redirect_hop_method",
            "topLevelNavigation": "captured_request_boolean",
            "unknownOrOpaque": cookie_unknown_or_opaque_context,
        },
        "controlled-web-session-v2 SameSite request context",
    )
    require_exact_fields(
        cookie_same_site_response_storage,
        {
            "sameSiteOrTopLevelNavigation": ("all_valid_unpartitioned_response_cookies_eligible"),
            "crossSiteSubresource": (
                "only_secure_SameSite_None_eligible_Strict_Lax_and_unspecified_ignored_without_terminal"
            ),
            "requestMethod": "not_an_admission_input",
        },
        "controlled-web-session-v2 SameSite response-storage policy",
    )
    require_exact_fields(
        cookie_same_site,
        {
            "metadataRoundTrip": True,
            "siteModel": "schemeful_site",
            "context": cookie_same_site_context,
            "none": "requires_secure",
            "strict": "included_only_for_schemeful_same_site_requests",
            "lax": ("included_for_schemeful_same_site_or_cross_site_top_level_safe_method"),
            "unspecified": "same_request_filter_as_lax",
            "safeMethods": ["GET", "HEAD", "OPTIONS", "TRACE"],
            "noneSelection": "included_cross_site_when_secure",
            "ineligibleCookie": ("filtered_before_cookie_header_construction_without_terminal"),
            "responseStorage": cookie_same_site_response_storage,
        },
        "controlled-web-session-v2 SameSite policy",
    )
    require_exact_fields(
        cookie_set_cookie,
        {
            "expiresOrMaxAge": "supported_by_controlled_expiry_policy",
            "partitioned": "unsupported_partitioned_cookie",
            "invalid": "invalid_controlled_cookie",
            "boundary": "before_cookie_jar_mutation",
        },
        "controlled-web-session-v2 Set-Cookie policy",
    )
    require_exact_fields(
        cookie_store,
        {
            "set": {
                "supported": True,
                "policy": "supported_atomic_controlled_policy",
                "expiry": "supported_by_controlled_expiry_policy",
                "partitionedError": "unsupported_partitioned_cookie",
                "invalidError": "invalid_controlled_cookie",
            },
            "get": {
                "supported": False,
                "code": "controlled_cookie_store_read_delete_unsupported",
            },
            "getAll": {
                "supported": False,
                "code": "controlled_cookie_store_read_delete_unsupported",
            },
            "delete": {
                "supported": False,
                "code": "controlled_cookie_store_read_delete_unsupported",
            },
        },
        "controlled-web-session-v2 Cookie Store boundary",
    )
    require_expected_fields(
        session_cookies,
        {
            "supported": True,
            "persistence": cookie_persistence,
            "timeRange": cookie_time_range,
            "partitioning": "unpartitioned_only",
            "sameSite": cookie_same_site,
            "setCookie": cookie_set_cookie,
            "pageApi": cookie_page_api,
        },
        "controlled-web-session-v2 session cookies",
    )
    require_expected_fields(
        session_state,
        {
            "artifactProfile": "controlled-web-session-v2",
            "compatibleSelectedProfiles": ["controlled-web-session-v2"],
        },
        "controlled-web-session-v2 session state",
    )
    require_exact_fields(
        controlled_input_method,
        {
            "scope": ("exact_public_controlled_non_auxiliary_top_level_WebView_document_global"),
            "request": ("page_driven_InputMethod_Text_nonmultiline_allowVirtualKeyboard_false_only"),
            "trigger": ("page_driven_programmatic_DOM_focus_including_React_autoFocus"),
            "semanticAutomation": ("preexisting_profile_independent_suppression_unchanged"),
            "domSemantics": "focus_events_value_and_selection_preserved",
            "embedderPresentation": "suppressed_before_time_surface_admission",
            "visibleOwner": "not_published",
            "embedderRequest": "not_sent",
            "callback": "not_created_or_awaited",
            "pendingAuthority": "no_external_work_created",
            "otherEmbedderControls": "unchanged_unsupported_boundaries",
            "predecessorBehavior": "controlled_web_session_v1_unchanged",
        },
        "controlled-web-session-v2 InputMethod focus boundary",
    )
    require_exact_fields(
        controlled_focus_event_timestamp,
        {
            "scope": ("exact_public_controlled_non_auxiliary_top_level_WebView_document_global"),
            "events": ["focus", "blur", "focusin", "focusout"],
            "creation": "engine_generated_document_focus_transition_only",
            "clock": "document_performance_clock_sampled_at_event_creation",
            "observableValue": ("Event_timeStamp_equals_document_relative_performance_time"),
            "hostValue": ("sampled_implementation_value_is_overwritten_and_not_observable"),
            "scriptCreatedFocusEvent": "host_timestamp",
            "otherEventsOutsideControlledAutomationScope": "host_timestamp",
            "predecessorBehavior": "controlled_web_session_v1_unchanged",
            "realtimeBehavior": "unchanged",
        },
        "controlled-web-session-v2 FocusEvent timestamp boundary",
    )
    require_exact_fields(
        controlled_automation_event_timestamps,
        {
            "scope": "active_controlled_top_level_document_global",
            "automationScope": "synchronous_public_mutating_automation_action_only",
            "clock": "document_performance_clock_sampled_once_before_mutation",
            "lifetime": "RAII_scope_restored_before_action_response",
            "coverage": ("every_browser_created_event_constructed_synchronously_during_the_admitted_action"),
            "implementationSeams": controlled_automation_event_seams,
            "representativeProofEvents": (
                "fill_input_activate_click_reset_check_click_input_change_select_input_change_invalid_submit_formdata"
            ),
            "observableValue": (
                "all_browser_created_events_synchronously_constructed_during_one_"
                "admitted_action_share_its_document_relative_timestamp"
            ),
            "samplingFailure": ("reject_action_before_mutation_without_host_fallback"),
            "scriptCreatedConstructors": (
                "Event_InputEvent_PointerEvent_SubmitEvent_FormDataEvent_remain_host_timestamp"
            ),
            "genericEventConstructor": "Event_new_inherited_unchanged",
            "predecessorBehavior": "controlled_web_session_v1_unchanged",
            "nestedAndRealtimeBehavior": "unchanged",
        },
        "controlled-web-session-v2 automation event timestamp boundary",
    )
    require_exact_fields(
        controlled_automation_event_seams,
        {
            "fillInputEvent": "explicit_internal_fill_InputEvent_stamp",
            "internalPointerEvent": "internal_PointerEvent_new_stamp",
            "genericEventTargetFire": "browser_created_simple_Event_stamp",
            "internalSubmitEvent": "internal_SubmitEvent_new_stamp",
            "internalFormDataEvent": "internal_FormDataEvent_new_stamp",
        },
        "controlled-web-session-v2 automation event constructor seams",
    )
    require_exact_fields(
        controlled_css_animation_event_timestamps,
        {
            "scope": ("exact_public_controlled_non_auxiliary_top_level_WebView_document_global"),
            "source": (
                "nonempty_Animations_pending_event_dispatch_batch_already_retained_by_document_rendering_authority"
            ),
            "eventKinds": [
                "animationstart",
                "animationiteration",
                "animationend",
                "animationcancel",
                "transitionrun",
                "transitionstart",
                "transitionend",
                "transitioncancel",
            ],
            "clock": ("document_performance_clock_sampled_once_before_pending_queue_take"),
            "targetAdmission": (
                "ScriptThread_current_controlled_top_level_target_matches_conservative_"
                "singleton_reconstruction_with_undiscarded_non_auxiliary_WindowProxy"
            ),
            "recordAdmission": (
                "queued_pipeline_and_rooted_node_owner_match_exact_public_controlled_"
                "non_auxiliary_top_level_target_and_fully_active_Document"
            ),
            "construction": ("internal_AnimationEvent_and_TransitionEvent_timestamp_overwrite_immediately_before_fire"),
            "observableValue": (
                "every_admitted_internal_event_in_one_nonempty_dispatch_batch_shares_its_document_relative_timestamp"
            ),
            "samplingFailure": ("latch_controlled_clock_terminal_and_leave_batch_undispatched_without_host_fallback"),
            "pendingAuthority": (
                "existing_pending_event_and_finite_infinite_unsupported_animation_rendering_facts_unchanged"
            ),
            "settlementScheduling": controlled_css_settlement_scheduling,
            "executionLimit": "existing_10000_rendering_opportunity_limit",
            "representativeExecutableProof": ("instant_finite_animationstart_and_animationend_only"),
            "transitionSettlementCompatibility": (
                "not_claimed_timestamp_adapter_applies_only_if_an_existing_owned_"
                "transition_record_reaches_pending_dispatch"
            ),
            "scriptCreatedConstructors": ("AnimationEvent_and_TransitionEvent_remain_host_timestamp"),
            "auxiliaryStaleMismatchedNestedAndRealtime": ("host_timestamp_predecessor_behavior"),
            "semanticBoundary": (
                "timestamp_only_event_order_cardinality_elapsedTime_and_CSS_animation_semantics_unchanged"
            ),
            "predecessorBehavior": "controlled_web_session_v1_unchanged",
        },
        "controlled-web-session-v2 CSS animation event timestamp boundary",
    )
    require_exact_fields(
        controlled_css_settlement_scheduling,
        {
            "scheduledPendingEventBatch": (
                "finite_rendering_demand_advanced_to_exact_retained_scheduler_head_including_deadline_equal_to_now"
            ),
            "driveReadiness": ("pending_animation_events_are_Drive_ready_only_without_a_live_scheduled_opportunity"),
            "reason": "Drive_cannot_detach_a_controlled_scheduler_entry",
            "surfaceEffect": ("liveness_correction_only_no_new_producer_task_source_or_execution_limit"),
        },
        "controlled-web-session-v2 CSS pending-event settlement scheduling",
    )
    require_exact_fields(
        controlled_image_element,
        {
            "mode": ("controlled_top_level_direct_data_svg_and_initial_url_retained_ownership_bounded_http_https"),
            "selection": controlled_image_selection,
            "retention": controlled_image_retention,
            "completion": controlled_image_completion,
            "pending": controlled_image_pending,
            "eventTimestamp": controlled_image_event_timestamp,
            "unsupported": controlled_image_unsupported,
        },
        "controlled-web-session-v2 HTMLImageElement boundary",
    )
    require_exact_fields(
        controlled_image_selection,
        {
            "interface": "HTMLImageElement",
            "scope": ("exact_public_controlled_non_auxiliary_top_level_WebView_document_global"),
            "source": ("direct_src_selected_without_srcset_picture_or_environment_change"),
            "urlSchemes": ["http", "https", "data"],
            "httpHttps": ("content_format_agnostic_at_initial_selection_with_resource_IO_separately_owned"),
            "httpHttpsOrigin": (
                "same_and_cross_origin_source_ownership_response_determinism_governed_by_session_network_policy"
            ),
            "httpHttpsRedirects": ("resource_owned_final_URL_not_rechecked_against_initial_selected_URL_limit"),
            "dataUrlParser": "canonical_DataUrl",
            "dataUrlMimeType": "image/svg+xml",
            "maximumInitialSelectedCanonicalUrlBytes": 65536,
            "requestProvenance": ("captured_at_selection_and_carried_with_request_generation"),
            "retainedVectorAuthority": (
                "controlled_cache_id_stored_on_request_only_after_successful_"
                "registration_or_synchronous_exact_owner_retain"
            ),
            "executionDomain": "same_ScriptThread_and_per_pipeline_ImageCacheStore",
            "cacheReuseProofBoundary": ("same_pipeline_store_under_immutable_session_fixture_routes"),
        },
        "controlled-web-session-v2 HTMLImageElement selection",
    )
    require_exact_fields(
        controlled_image_retention,
        {
            "maximumRetainedControlledOwnershipRecordsPerWindow": 512,
            "recordKinds": [
                "pending_callback",
                "exact_cache_id_DOM_owner_identity",
                "vector_rasterization_key",
            ],
            "reservationUnit": (
                "one_record_per_controlled_pending_callback_exact_cache_id_DOM_owner_"
                "identity_or_vector_rasterization_key"
            ),
            "overflow": ("sticky_Image_producer_admission_limit_terminal_without_baseline_fallback"),
            "decodeRequestAdmission": (
                "ReadyForRequest_callback_and_identity_reservations_succeed_before_cache_request_issue"
            ),
            "teardown": ("callback_identity_layout_and_raster_collections_cleared_together_releasing_all_records"),
        },
        "controlled-web-session-v2 HTMLImageElement retention",
    )
    require_exact_fields(
        controlled_image_completion,
        {
            "synchronousCacheHit": ("admitted_provenance_bound_current_turn_without_async_producer_lease"),
            "asyncCacheDecode": "Image_producer_fenced_through_ScriptThread_handoff",
            "callbackRetirement": (
                "cache_owned_callback_drop_before_protocol_terminal_is_owned_"
                "cancellation_completed_without_producer_terminal"
            ),
            "retiredTargetDelivery": (
                "dequeued_after_navigation_with_closed_pipeline_tombstone_and_without_"
                "live_Window_is_owned_"
                "cancellation_completed_without_producer_terminal"
            ),
            "retainedHandlerRejection": (
                "normal_handler_Err_preserves_rejected_key_or_owner_in_Window_pending_"
                "collections_completes_scoped_message_guard_and_settles_as_unsupported_"
                "rendering_image_load"
            ),
            "handlerUnwind": (
                "ControlledImageMessageCompletion_abandons_during_unwind_and_completes_every_normal_handler_return"
            ),
            "explicitAbandonment": (
                "message_admission_failure_enqueue_rejection_producer_callback_panic_"
                "ScriptThread_handler_unwind_missing_untombstoned_or_live_tombstoned_"
                "target_prehandler_profile_or_exact_public_target_mismatch_clock_"
                "sampling_failure_or_guarded_"
                "transport_loss_latches_sticky_Image_producer_terminal"
            ),
            "leaseClassMatch": (
                "completion_and_abandonment_require_exact_fence_sequence_and_registered_"
                "Image_kind_before_terminal_or_watermark_mutation"
            ),
            "vectorRasterization": ("fenced_only_when_joined_from_a_retained_exact_cache_id_DOM_owner_identity"),
            "vectorRasterizationStart": ("may_begin_in_layout_before_post_reflow_exact_key_reservation"),
            "vectorRasterizationAdmission": (
                "post_reflow_exact_key_reservation_and_fenced_listener_install_before_"
                "next_ScriptThread_pending_snapshot_publish_or_observe"
            ),
            "vectorRasterizationCapacityFailure": (
                "sticky_Image_producer_terminal_without_baseline_fallback_even_if_task_already_started"
            ),
            "terminalResponses": [
                "loaded",
                "failed_to_load_or_decode",
                "vector_rasterization_complete",
            ],
            "queuedDomCallback": ("ordinary_task_after_guarded_handoff_with_request_generation_check"),
            "preHandlerMismatchOrAbandonment": ("sticky_producer_terminal_without_baseline_fallback"),
            "requestAuthorityLifecycle": (
                "pending_to_current_move_preserves_exact_cache_id_and_abort_replace_"
                "or_different_id_releases_exact_owner"
            ),
            "sameIdAbaProtection": ("stale_generation_releases_only_when_neither_request_slot_owns_exact_cache_id"),
            "multipartMixedReplace": (
                "post_metadata_explicit_unsupported_provenance_retires_controlled_"
                "Image_producer_and_reports_unsupported_rendering_image_load_after_"
                "finite_resource_IO_drains_while_endless_resource_IO_remains_external"
            ),
            "inflightHttpDocumentReplacement": (
                "fatal_blocked_on_external_io_before_cross_document_successor_authority"
            ),
            "decoderResourceBudget": ("not_claimed_existing_wall_task_and_rendering_limits_only"),
        },
        "controlled-web-session-v2 HTMLImageElement completion",
    )
    require_exact_fields(
        controlled_image_pending,
        {
            "logicalIdentity": (
                "union_of_callback_and_layout_PendingImageId_plus_exact_image_id_size_rasterization_keys"
            ),
            "layoutOwnerProvenance": ("captured_per_exact_cache_id_DOM_owner_at_first_post_reflow_retention"),
            "controlledClassification": (
                "image_id_controlled_only_when_every_retained_callback_is_controlled_"
                "and_no_retained_owner_is_baseline_or_explicit_Unsupported"
            ),
            "mixedLayoutOwnership": (
                "baseline_layout_owner_globally_downgrades_cache_id_and_live_raster_"
                "keys_and_delivery_mismatch_rejects_before_any_callback_while_retained_"
                "as_unsupported_rendering"
            ),
            "mixedMissingOrBaseline": (
                "missing_baseline_or_explicit_Unsupported_is_unsupported_pending_rendering_image_load"
            ),
            "controlledProjection": ("Image_producer_fence_not_pending_rendering_image_load"),
            "reservationReconciliation": (
                "live_controlled_records_equal_retained_controlled_callbacks_plus_"
                "exact_cache_id_DOM_owner_identities_plus_controlled_rasterization_keys"
            ),
            "unsupportedReservationReconciliation": (
                "explicit_Unsupported_records_retain_exact_logical_ID_without_controlled_capacity_reservations"
            ),
            "producerReconciliation": (
                "pending_Image_producers_greater_than_or_equal_to_controlled_logical_work_absent_terminal"
            ),
        },
        "controlled-web-session-v2 HTMLImageElement pending authority",
    )
    require_exact_fields(
        controlled_image_event_timestamp,
        {
            "events": ["load", "error", "loadend"],
            "creation": "engine_generated_HTMLImageElement_completion_only",
            "clock": "document_performance_clock_sampled_once_per_completion",
            "observableValue": ("every_event_emitted_for_one_completion_shares_the_document_relative_timestamp"),
            "ordinaryTerminalCardinality": "load_then_loadend_or_error_then_loadend",
            "existingCacheHitCardinality": "load_only",
            "cacheHit": ("sampled_before_queued_DOM_manipulation_task_and_carried_with_request_generation"),
            "async": ("sampled_at_guarded_ScriptThread_delivery_and_carried_through_queued_callback"),
            "hostFallback": "forbidden_for_admitted_work",
            "predecessorBehavior": "controlled_web_session_v1_unchanged",
        },
        "controlled-web-session-v2 HTMLImageElement event timestamp",
    )
    require_exact_fields(
        controlled_image_unsupported,
        {
            "blobFileAndNonSvgDataUrls": "not_admitted_baseline_image_authorities_unchanged",
            "oversizeUrl": "not_admitted_baseline_image_authorities_unchanged",
            "srcsetPictureAndEnvironmentChange": ("not_admitted_baseline_image_authorities_unchanged"),
            "cssBackgroundListStyleAndContent": (
                "not_admitted_unless_joining_a_retained_exact_cache_id_DOM_owner_identity"
            ),
            "faviconAndVideoPoster": ("not_admitted_baseline_image_authorities_unchanged"),
            "imageBitmapAndCanvasUpload": ("not_admitted_baseline_image_authorities_unchanged"),
            "animatedImages": ("not_admitted_by_this_slice_existing_rendering_authority_unchanged"),
            "multipartMixedReplace": (
                "post_metadata_typed_unsupported_rendering_image_load_after_finite_"
                "resource_IO_drains_without_streaming_semantics"
            ),
            "iframeWorkerWorkletAndCrossLoop": ("not_admitted_existing_context_boundaries_unchanged"),
            "unadmittedSharedVectorCacheIdentity": (
                "remove_all_controlled_owners_and_downgrade_live_raster_keys_to_baseline"
            ),
            "nestedOrExternalSvgResources": ("not_content_inspected_not_proven_by_this_slice"),
        },
        "controlled-web-session-v2 HTMLImageElement unsupported boundary",
    )
    require_exact_fields(
        controlled_inline_svg,
        {
            "mode": "controlled_top_level_internal_serialized_data_svg",
            "admission": controlled_inline_svg_admission,
            "ownership": controlled_inline_svg_ownership,
            "completion": controlled_inline_svg_completion,
            "unsupported": controlled_inline_svg_unsupported,
        },
        "controlled-web-session-v2 inline SVG rendering boundary",
    )
    require_exact_fields(
        controlled_inline_svg_admission,
        {
            "interface": "SVGSVGElement",
            "scope": ("exact_public_controlled_non_auxiliary_top_level_WebView_document_global"),
            "source": "internally_serialized_inline_svg_subtree_only",
            "requestKind": "InternalRequest_Yes",
            "cachedUrlIdentity": ("candidate_exactly_equals_element_cached_serialized_data_url"),
            "parser": "canonical_DataUrl",
            "mimeType": "image/svg+xml",
            "maximumSerializedUrlBytes": 65536,
            "executionDomain": "same_ScriptThread_and_ImageCache",
        },
        "controlled-web-session-v2 inline SVG admission",
    )
    require_exact_fields(
        controlled_inline_svg_ownership,
        {
            "cacheIdJoin": "exact_PendingImageId_DOM_owner_identity_required",
            "retentionBudget": "shared_512_record_controlled_image_ownership_limit",
            "mixedOwnership": ("baseline_owner_globally_downgrades_shared_cache_id_and_live_raster_keys"),
            "hostFallback": "forbidden_for_admitted_work",
        },
        "controlled-web-session-v2 inline SVG ownership",
    )
    require_exact_fields(
        controlled_inline_svg_completion,
        {
            "asyncCacheDecode": "Image_producer_fenced_through_ScriptThread_handoff",
            "vectorRasterization": ("fenced_only_from_exact_retained_inline_svg_cache_id_DOM_owner_identity"),
            "pendingProjection": "Image_producer_fence_not_pending_rendering_image_load",
            "domLoadEvent": ("not_emitted_by_internal_inline_svg_rendering_completion"),
        },
        "controlled-web-session-v2 inline SVG completion",
    )
    require_exact_fields(
        controlled_inline_svg_unsupported,
        {
            "baselineAndV1": "unchanged",
            "generalSvgRendering": "not_admitted_by_this_slice",
            "nestedOrExternalResources": ("not_admitted_or_proven_existing_resource_authority_unchanged"),
            "nonInternalOrMismatchedUrl": "baseline_image_authorities_unchanged",
            "iframeWorkerWorkletAndCrossLoop": ("not_admitted_existing_context_boundaries_unchanged"),
        },
        "controlled-web-session-v2 inline SVG unsupported boundary",
    )
    require_exact_fields(
        host_timestamp_boundary,
        {
            "controlledTopLevelEngineGeneratedFocusTransitionInV2": ("document_clock_timestamp"),
            "controlledTopLevelAdmittedImageCompletionEventsInV2": ("shared_document_clock_timestamp"),
            "controlledTopLevelSynchronousPublicAutomationEventsInV2": (
                "one_document_clock_timestamp_per_mutating_action"
            ),
            "controlledPublicNonAuxiliaryTopLevelInternalCssAnimationAndTransitionEventsInV2": (
                "one_document_clock_timestamp_per_nonempty_pending_event_dispatch_batch"
            ),
            "scriptCreatedEventConstructorsAndAllUnlistedHostTimestampSurfaces": ("host_timestamp"),
        },
        "controlled-web-session-v2 host-timestamp boundary",
    )
    require_exact_fields(
        image_element_unsupported_class,
        {
            "admittedDirectDataSvg": "owned_bounded_Image_producer_work",
            "admittedDirectHttpHttps": (
                "owned_Image_producer_work_with_initial_URL_and_retained_ownership_"
                "bounds_and_resource_IO_separately_owned"
            ),
            "baselineMixedOrUnownedRetainedWork": ("unsupported_rendering_image_load"),
            "excludedSynchronousCacheHit": ("predecessor_behavior_no_universal_new_typed_rejection"),
            "nestedOrExternalSvgResources": "not_proven",
        },
        "controlled-web-session-v2 image-element unsupported class",
    )
    require_exact_fields(
        embedder_controls,
        {
            CONTROLLED_INPUT_METHOD_EMBEDDER_SUMMARY: ("suppressed_without_external_work"),
            "selectElementColorPickerFilePickerContextMenuAndOtherControls": ("embedder_control"),
        },
        "controlled-web-session-v2 embedder-control boundary",
    )
    input_method_product_surfaces = [entry for entry in supported_product_surface if "input_method" in entry.lower()]
    if input_method_product_surfaces != [CONTROLLED_INPUT_METHOD_PRODUCT_SURFACE]:
        raise ReleaseError(
            "controlled-web-session-v2 supportedProductSurface must contain exactly the "
            "single-line Text/nonmultiline/no-virtual-keyboard InputMethod summary"
        )
    focus_timestamp_product_surfaces = [
        entry for entry in supported_product_surface if "focus_event_timestamp" in entry.lower()
    ]
    if focus_timestamp_product_surfaces != [CONTROLLED_FOCUS_EVENT_TIMESTAMP_PRODUCT_SURFACE]:
        raise ReleaseError(
            "controlled-web-session-v2 supportedProductSurface must contain exactly the "
            "engine-generated top-level FocusEvent document-clock timestamp summary"
        )
    automation_timestamp_product_surfaces = [
        entry
        for entry in supported_product_surface
        if "synchronous_public_automation_event_timestamps" in entry.lower()
    ]
    if automation_timestamp_product_surfaces != [CONTROLLED_AUTOMATION_EVENT_TIMESTAMP_PRODUCT_SURFACE]:
        raise ReleaseError(
            "controlled-web-session-v2 supportedProductSurface must contain exactly the "
            "synchronous public automation event document-clock timestamp summary"
        )
    css_animation_timestamp_product_surfaces = [
        entry for entry in supported_product_surface if "internal_css_animation_event_timestamps" in entry.lower()
    ]
    if css_animation_timestamp_product_surfaces != [CONTROLLED_CSS_ANIMATION_EVENT_TIMESTAMP_PRODUCT_SURFACE]:
        raise ReleaseError(
            "controlled-web-session-v2 supportedProductSurface must contain exactly the "
            "internal CSS animation event document-clock timestamp summary"
        )
    image_product_surfaces = [
        entry for entry in supported_product_surface if "htmlimageelement_completion" in entry.lower()
    ]
    if image_product_surfaces != [
        CONTROLLED_IMAGE_ELEMENT_PRODUCT_SURFACE,
        CONTROLLED_HTTP_IMAGE_ELEMENT_PRODUCT_SURFACE,
    ]:
        raise ReleaseError(
            "controlled-web-session-v2 supportedProductSurface must contain exactly the "
            "bounded top-level direct data-SVG and initial-URL/retained-ownership-bounded "
            "direct HTTP(S) HTMLImageElement completion summaries"
        )
    inline_svg_product_surfaces = [
        entry for entry in supported_product_surface if "serialized_data_svg_inline_rendering" in entry.lower()
    ]
    if inline_svg_product_surfaces != [CONTROLLED_INLINE_SVG_PRODUCT_SURFACE]:
        raise ReleaseError(
            "controlled-web-session-v2 supportedProductSurface must contain exactly the "
            "bounded top-level internal serialized data-SVG inline-rendering summary"
        )
    cookie_expiry_product_surfaces = [
        entry for entry in supported_product_surface if "persistent_cookie_expiry" in entry.lower()
    ]
    if cookie_expiry_product_surfaces != [CONTROLLED_COOKIE_EXPIRY_PRODUCT_SURFACE]:
        raise ReleaseError(
            "controlled-web-session-v2 supportedProductSurface must contain exactly the "
            "controlled in-memory persistent-cookie expiry summary"
        )
    cookie_same_site_product_surfaces = [
        entry for entry in supported_product_surface if "samesite_request_cookie_selection" in entry.lower()
    ]
    if cookie_same_site_product_surfaces != [CONTROLLED_COOKIE_SAME_SITE_PRODUCT_SURFACE]:
        raise ReleaseError(
            "controlled-web-session-v2 supportedProductSurface must contain exactly the "
            "bounded schemeful SameSite request-cookie selection summary"
        )
    cookie_same_site_response_product_surfaces = [
        entry for entry in supported_product_surface if "samesite_response_cookie_storage" in entry.lower()
    ]
    if cookie_same_site_response_product_surfaces != [CONTROLLED_COOKIE_SAME_SITE_RESPONSE_PRODUCT_SURFACE]:
        raise ReleaseError(
            "controlled-web-session-v2 supportedProductSurface must contain exactly the "
            "bounded schemeful SameSite response-cookie storage summary"
        )
    require_expected_fields(
        message_channel,
        {"mode": "controlled_same_global_untransferred"},
        "controlled-web-session-v2 MessageChannel",
    )
    require_exact_fields(
        construction,
        {
            "interface": "MessageChannel",
            "scope": "active_controlled_top_level_document_global",
            "provenance": "constructor_created_controlled_local_pair",
            "targetAdmission": ("ScriptThread_current_controlled_top_level_target_matches_before_pair_publication"),
            "incumbentAdmission": (
                "incumbent_global_required_and_exact_owner_global_pipeline_WebView_identity_before_pair_publication"
            ),
            "borrowedOrMissingIncumbentFailure": (
                "synchronous_NotSupportedError_and_sticky_external_subscription_before_pair_publication"
            ),
            "routerOwnership": ("controlled_local_only_no_router_id_or_constellation_registration"),
            "mixedProvenance": ("rejected_and_sticky_external_subscription_never_cohabits_controlled_managed_map"),
            "maximumRetainedNativePortEntriesPerGlobal": 32,
            "capacityUnit": "retained_native_port_entry",
            "completePairCapacityFromEmptyGlobal": 16,
            "completePairCapacityCondition": ("no_one_ended_terminal_identities_retained"),
            "oneEndedTerminalIdentityCapacity": (
                "each_retained_identity_consumes_one_of_32_entries_and_reduces_available_complete_pair_capacity"
            ),
            "closedEntryCapacity": (
                "retained_until_dom_garbage_collection_checkpoint_pruning_and_"
                "while_any_controlled_local_message_reservation_remains"
            ),
            "overflow": ("synchronous_NotSupportedError_and_sticky_external_subscription_before_pair_publication"),
        },
        "controlled-web-session-v2 MessageChannel construction",
    )
    require_expected_fields(
        post_message,
        {
            "transferList": "must_be_empty",
            "incumbentAdmission": (
                "resolved_before_structured_clone_and_exact_owner_global_pipeline_WebView_identity_required"
            ),
            "incumbentFailure": (
                "synchronous_NotSupportedError_and_sticky_external_subscription_before_"
                "serialization_detachment_reservation_or_dispatch"
            ),
            "nonemptyTransferListFailure": (
                "synchronous_NotSupportedError_and_sticky_external_subscription_before_serialization_detach_or_dispatch"
            ),
        },
        "controlled-web-session-v2 MessageChannel postMessage",
    )
    require_expected_fields(
        unsupported,
        {
            "constellationOrExternallyRoutedPort": "external_subscription",
            "otherwiseValidPortTransferAttempt": "external_subscription",
            "detachedPortTransferAttempt": ("platform_DataCloneError_before_stasis_boundary"),
            "detachedPortPostMessage": "platform_noop_before_stasis_boundary",
            "missingOrBorrowedIncumbentConstructor": ("external_subscription_before_pair_publication"),
            "missingOrBorrowedIncumbentPostMessage": ("external_subscription_before_structured_clone_or_detachment"),
            "replacedDiscardedOrAuxiliaryOwner": (
                "external_subscription_before_controlled_local_authority_is_borrowed"
            ),
            "otherwiseReachedCrossGlobalPortIncludingNestedWindow": ("external_subscription"),
            "nestedWindowCreation": "same_event_loop_iframe",
            "crossEventLoopIframeClassification": (
                "conservative_alternate_ingress_fence_not_ordinary_child_creation_outcome"
            ),
            "otherwiseReachedCrossEventLoopPort": "external_subscription",
            "workerCreation": "worker_before_worker_global_or_port_exists",
            "workerGlobalMessageChannel": ("unreachable_not_independently_admitted_or_evidenced"),
            "unexpectedAsyncIngress": (
                "impossible_router_or_external_ingress_backstop_sticky_external_subscription_drop_without_dispatch"
            ),
        },
        "controlled-web-session-v2 MessageChannel unsupported boundary",
    )
    require_expected_fields(
        delivery,
        {
            "publicRetainedPresence": (
                "queued_or_buffered_work_sources_kind_tracked_presence_openEnded_reason_message_port"
            ),
        },
        "controlled-web-session-v2 MessageChannel delivery",
    )
    require_expected_fields(
        retained_work_projection,
        {
            "reservationIdentity": (
                "exact_destination_port_before_retention_in_ordinary_task_queue_or_native_disabled_port_buffer"
            ),
            "accountingReconciliation": ("global_retained_equals_sum_per_destination_queued_plus_sum_native_buffered"),
            "reciprocalPairWithOwnedWork": ("one_deterministic_minimum_port_identity_per_pair"),
            "zeroRetainedMessages": "does_not_make_idle_open_pair_pending",
            "invalidMissingOrZeroDestinationAssociation": ("pending_observation_failure"),
        },
        "controlled-web-session-v2 retained MessagePort work projection",
    )
    expected_transfer_preflight = {
        "scope": "complete_transfer_list_in_selected_controlled_global",
        "ordering": ("transfer_list_order_first_failure_stops_before_any_javascript_transfer_step"),
        "interfaces": [
            "MessagePort",
            "ReadableStream",
            "WritableStream",
            "TransformStream",
        ],
        "otherwiseValidEntryFailure": (
            "synchronous_NotSupportedError_and_sticky_external_subscription_before_any_javascript_transfer_step"
        ),
        "platformValidationPrecedence": (
            "per_checked_entry_detached_MessagePort_or_locked_stream_DataCloneError_before_stasis_boundary"
        ),
        "earlierTransferEntriesOnBoundaryRejection": "remain_undetached",
        "notClaimed": [
            "transferable_stream_support",
            "general_transactional_rollback_for_other_transfer_failures",
        ],
    }
    require_expected_fields(
        transfer_preflight,
        expected_transfer_preflight,
        "controlled-web-session-v2 port-backed transfer preflight",
    )
    contract_markers = (
        "# Controlled session v2 contract",
        "**Status:** Versioned contract for Stasis 0.3.0",
        "Checked-in source is not a publication or",
        "native-availability claim; verify the immutable tag, release, registry package, and provenance",
        "A non-empty `MessagePort.postMessage()` transfer list",
        "owns no MessagePort router identifier and never registers a",
        "`MessagePort`, `ReadableStream`, `WritableStream`, or",
        "This is a typed rejection boundary",
        "transferable-stream support or general transactional rollback",
        "An already-detached port instead preserves the",
        "Construction is admitted only while `ScriptThread` can reconstruct the receiver",
        "incumbent global exists and matches the",
        "Those checks happen before either port is\n  published",
        "`MessagePort.postMessage()` resolves and validates that same exact incumbent/owner identity",
        "before serialization, transfer detachment,\n  retained-message reservation, or dispatch",
        "rejects nested-window/iframe creation earlier with",
        "Worker creation likewise latches `worker` before a worker global",
        "not a hostile or forged renderer-IPC security claim",
        "returns before time-surface admission, visible-owner publication",
        "No external work is created or hidden from pending authority",
        "new page-driven exception is limited to the exact",
        "Other InputMethod types, multiline or virtual-keyboard requests",
        "frozen v1 profiles retain",
        "retained queued or buffered port-message work appears",
        "Every admitted reservation acquires its exact destination-port identity",
        "global retained count equals the sum of exact",
        "there is no global nonzero fallback identity",
        "independent pairs\nwith work remain independently visible",
        "A zero retained count does not make an otherwise-idle open pair pending",
        "At most 32 controlled-local native port entries",
        "no one-ended terminal identities",
        "Each retained one-ended terminal identity consumes one of the 32 entries",
        "React's `autoFocus` mount behavior",
        "Literal HTML\n`autofocus` candidate processing is not claimed",
        "exact public non-auxiliary\ncontrolled top-level WebView/document",
        "For an engine-generated `focus`, `blur`, `focusin`, or `focusout`",
        "no\nhost-derived timestamp value is observable",
        "Script-created\n`new FocusEvent(...)` objects",
        "Immediately before executing one public mutating automation action",
        "samples the document Performance clock exactly once",
        "Every browser-created event constructed synchronously during that admitted action",
        "This is not an event-name allowlist",
        "internal fill `InputEvent`, internal `PointerEvent`, generic browser-created `Event`",
        "representative observations of the generic rule",
        "`Event::new_inherited` remains unchanged",
        "`unsupported_clock_surface` settlement outcome",
        "not\ncreated synchronously inside the public action scope",
        "existing rendering authority already retains each document's CSS animation",
        "scheduled opportunity with pending animation events is\nfinite rendering demand",
        "including when that deadline equals `now`",
        "only an event batch with no live scheduled opportunity is\n`Drive`-ready",
        "timestamp adapter at the owning `Animations::send_pending_events` seam",
        "`ScriptThread::current_controlled_top_level_target_matches(window)`",
        "conservatively reconstruct the dispatch Window as the sole retained fully-active public",
        "WindowProxy must be undiscarded and non-auxiliary",
        "not a\nclaim that Stasis retains a separate Constellation target identity",
        "samples that document's\nPerformance clock exactly once before taking the queue",
        "leaves the batch undispatched; it never falls through to a host timestamp",
        "queued pipeline and rooted node owner both\nmatch that dispatch Window and document",
        "`animationstart`, `animationiteration`, `animationend`",
        "`transitionrun`, `transitionstart`, `transitionend`, and `transitioncancel`",
        "executable compatibility proof is deliberately narrower than the adapter mapping",
        "owned `animationstart` and `animationend` events",
        "`TransitionEvent` mapping applies only if the existing rendering pipeline",
        "does not claim general\nCSS transition execution or settlement compatibility",
        "WebIDL `new_with_proto` paths remain unchanged",
        "script-created `new AnimationEvent(...)` and\n`new TransitionEvent(...)` objects",
        "Auxiliary top-level WebViews are deliberately excluded",
        "does not expand CSS/Web Animations API\nsemantics",
        "canonical `DataUrl` parser accepts it",
        "a direct `http:` or `https:` URL is admitted by scheme before response",
        "A final redirect URL is not rechecked against the initial 65,536-byte",
        "redirected fetch remains separately owned Resource I/O and the immutable",
        "session network policy remains authoritative",
        "active retains the existing fatal `blocked_on_external_io` boundary",
        "never reconstructed later from the element's current",
        "requires\nno invented asynchronous producer lease",
        "`multipart/x-mixed-replace` is discovered only after HTTP response metadata",
        "becomes observable after the separately owned\nResource I/O drains",
        "an endless response remains blocked on that external I/O",
        "Neither baseline nor\ncontrolled callback delivery can invoke the retained unsupported callback",
        "Message\nadmission failure, enqueue rejection, producer callback panic",
        "missing untombstoned target, a live tombstoned target, a live\nWindow",
        "exact public top-level target does not match",
        "Admitted work never retries via the baseline image sender",
        "image cache owns the callback's lifetime",
        "completes the stream lease as owned cancellation without a producer terminal",
        "completes normally as retired only\nwhen pipeline teardown independently installed its permanent tombstone",
        "Window, a handler `Err` likewise completes the scoped message guard normally",
        "stays in the Window's pending collections",
        "settlement reports it as typed\n`unsupported_rendering` / `image_load`",
        "`ControlledImageMessageCompletion`\nspans the live handler call",
        "every normal return, including that retained-state `Err`, completes",
        "Rust unwind abandons before propagation",
        "Explicit abandonment is reserved for",
        "registered\n`DocumentProducerKind::Image` class before either a terminal",
        "class-mismatched lease is rejected without consuming another producer's work",
        "retained exact `(PendingImageId, DOM owner)` identity",
        "synchronous vector cache hit retains that identity",
        "layout listener may likewise join only for its exact",
        "Each layout owner captures its provenance",
        "every callback is controlled and no\nretained layout owner is baseline",
        "controlled cache ID only after its asynchronous callback",
        "`Image::Vector`; it is not a promise",
        "Layout may initiate the vector-raster task",
        "before ScriptThread can publish or observe another pending",
        "does not claim that the raster task had not already started",
        "Pending-to-current promotion moves the retained cache identity",
        "neither the current nor pending request slot",
        "removes all controlled owners for its shared cache ID",
        "globally downgrades that cache ID and its live raster",
        "rejected before\nimage callbacks or layout invalidation run",
        "owner or raster key remains retained",
        "pending observation classifies that work as\n`unsupported_rendering` / `image_load`",
        "At most 512 controlled image ownership records",
        "Multiple DOM owners sharing a\ncache ID therefore consume distinct records",
        "Rejection of record 513 latches",
        "HTML decode\n`ReadyForRequest` path",
        "after layout initiated the task",
        "checks that terminal before even an idempotent success",
        "union of callback and per-owner layout `PendingImageId` values",
        "Image producer's pending count must be at least",
        "Controlled items are represented by the producer fence",
        "`load` and\n`loadend`, or `error` and `loadend`",
        "synchronous-cache-hit path emits only `load`",
        "this is not a universal eager-rejection promise",
        "Nested or external SVG resource\nsemantics are not proven",
        "they do not claim a separate deterministic CPU or allocation",
        "The second image slice is independent of `HTMLImageElement`",
        "request must be marked `InternalRequest::Yes`",
        "request URL must exactly equal the cached\nserialized URL for that same DOM owner",
        "delivery-time provenance mismatch remains retained typed unsupported work",
        "not converted into producer abandonment",
        "missing identity, mismatched cached URL, non-internal request, capacity",
        "completion creates no DOM `load`, `error`, or `loadend` event",
        "V2 owns persistent cookies in memory for the lifetime of its single controlled session",
        "Persistence\nacross processes is available only through an explicit `controlled-web-session-v2` state export",
        "its literal `state.profile`\nis `controlled-web-session-v2`; a v1 artifact is not silently migrated",
        "A valid\n`Max-Age` takes precedence over `Expires`",
        "retained expiry is clamped to at most 400 days",
        "expired\nrecords are lazily purged before cookie observation, request selection, and state export",
        "schemeful site-for-cookies captured from the request client",
        "current redirect-hop method",
        "cross-site\ntop-level `GET`, `HEAD`, `OPTIONS`, or `TRACE`",
        "Ineligible cookies are filtered before Cookie header construction",
        "`unsupported_cookie_same_site_context` before network start",
        "post-open request observes controlled Unix time above\n`18446744073709551615`",
        "`unsupported_cookie_time_range` with partial request\nstate effect before network start",
        "initial controlled open,\nthe shell hardens that same code to a fatal fail-stop",
        "cross-site subresource response, only a valid Secure `SameSite=None` cookie is eligible",
        "redirect-hop method is not an\ninput to this response-storage admission rule",
        "Partitioned cookies remain `unsupported_partitioned_cookie`",
        "`delete()` retain `controlled_cookie_store_read_delete_unsupported`",
        FROZEN_V2_PROFILE_SHA256,
    )
    for marker in contract_markers:
        if marker not in contract_text:
            raise ReleaseError(
                f"controlled-web-session-v2 candidate contract is missing the canonical boundary marker {marker!r}"
            )
    require_public_surface_markers(
        public_top_level_readme,
        "top-level public README",
        (
            "target the 0.3.0 train",
            "`controlled-web-session-v1` still the default",
            "[`controlled-web-session-v2` contract](docs/stasis/session-v0.3-candidate.md)",
            "Source version and package CI are not publication proof",
            "are the immutable predecessor; 0.3.0 is the stable successor only when its",
            "Verify those public artifacts rather than inferring release status from this checkout.",
        ),
        forbidden=(
            "The stable v0.2 release extends",
            "current immutable public stable\nboundary remains",
        ),
    )
    require_public_surface_markers(
        public_stasis_boundary,
        "public Stasis product boundary",
        (
            "Source version `0.3.0` is not a publication claim",
            "canonical HTTP(S) URL no larger than 65,536 bytes",
            "without inventing an asynchronous `Image` producer lease",
            "Finite\n  asynchronous cache/decode completion is fenced by an `Image` producer",
            "HTTP(S) Resource I/O remains separately owned",
            "A final redirect URL is not rechecked",
            "against the initial 65,536-byte selection bound",
            "pipeline's image-cache store under immutable fixture routes",
            "after its separately owned Resource I/O drains",
            "an endless response remains blocked on external I/O",
            "Public document replacement while HTTP image Resource I/O is active remains fatal",
            "decoder-resource-budget",
        ),
    )
    for public_readme, description in (
        (public_profile_readme, "public profile README"),
        (public_typescript_sdk_readme, "public TypeScript SDK README"),
    ):
        require_public_surface_markers(
            public_readme,
            description,
            (
                "An admitted synchronous cache hit is owned in",
                "current Script turn and queues its existing ordinary DOM callback without inventing an",
                "`Image` producer lease. Finite asynchronous cache/decode completion is producer-fenced",
            ),
            forbidden=(
                "Cache hits and finite asynchronous completion\nremain producer-fenced",
                "cache-hit or finite asynchronous completion is\nproducer-fenced",
            ),
        )
    require_public_surface_markers(
        public_profile_readme,
        "public profile README HTTP image authority",
        (
            "with resource I/O separately owned and",
            "Public document replacement while HTTP image resource I/O is active",
            "retains fatal `blocked_on_external_io`",
        ),
    )
    require_public_surface_markers(
        public_typescript_sdk_readme,
        "public TypeScript SDK publication and HTTP image authority",
        (
            "is not a publication claim: the native runtime must advertise v2",
            "while HTTP image resource I/O remains active retains fatal `blocked_on_external_io`",
        ),
    )
    require_public_surface_markers(
        public_release_runbook,
        "public release runbook",
        (
            "Source version is not\na publication claim",
            "A final redirect URL is not rechecked against that initial limit",
            "separately owned Resource I/O and the immutable session network policy remains authoritative",
            "Cache\nreuse proof is limited to one pipeline's image-cache store under immutable fixture routes",
        ),
    )
    require_public_surface_markers(
        public_release_workflow,
        "public GitHub release-note template",
        (
            "An admitted synchronous cache hit is owned in the current",
            "without an invented asynchronous Image producer lease; finite asynchronous",
            "A final\n          redirect URL is not rechecked against that initial selected-URL limit",
            "Resource I/O and the\n          immutable session network policy remain authoritative",
            "one pipeline's image-cache store under immutable fixture routes",
            "Public document replacement while HTTP image resource I/O remains active keeps",
            "the fatal blocked_on_external_io boundary",
        ),
    )
    package_v2_automation_verifier = credential_free_v2_automation_verifier_block(
        public_release_workflow,
        "credential-free package v2 automation verifier",
    )
    publish_v2_automation_verifier = credential_free_v2_automation_verifier_block(
        public_npm_publish_workflow,
        "credential-free npm-publish v2 automation verifier",
    )
    if package_v2_automation_verifier != publish_v2_automation_verifier:
        raise ReleaseError("credential-free package and npm-publish v2 automation verifiers diverged")
    verify_message_port_router_source(message_limits_source)
    verify_controlled_local_pending_projection_source(message_limits_source)
    verify_controlled_local_fifo_source(message_limits_source)
    verify_controlled_local_multi_pair_proof_source(
        message_baseline_test_source,
        message_multi_pair_fixture_source,
    )
    verify_port_backed_transfer_preflight_source(structured_clone_source)
    verify_message_port_post_transfer_source(message_port_source)
    verify_message_channel_incumbent_authority_source(
        message_limits_source,
        message_channel_source,
    )
    verify_controlled_input_method_source(
        source_root,
        input_method_control_source,
        input_method_input_source,
        input_method_textarea_source,
    )
    verify_controlled_focus_event_timestamp_source(event_source, focus_event_source)
    verify_controlled_automation_event_timestamp_source(
        controlled_automation_source,
        controlled_image_script_thread_source,
        controlled_image_window_source,
        event_source,
        controlled_automation_event_target_source,
        controlled_automation_input_event_source,
        controlled_automation_pointer_event_source,
        controlled_automation_submit_event_source,
        controlled_automation_form_data_event_source,
        message_baseline_test_source,
        controlled_automation_event_fixture_source,
    )
    verify_controlled_css_animation_event_timestamp_source(
        controlled_css_animation_source,
        controlled_css_animation_event_source,
        controlled_css_transition_event_source,
        controlled_css_document_source,
        controlled_image_script_thread_source,
        message_baseline_test_source,
        controlled_css_animation_event_fixture_source,
    )
    verify_controlled_animation_scheduler_liveness_source(controlled_rendering_settlement_source)
    verify_controlled_image_element_source(controlled_image_element_source)
    verify_controlled_image_timestamp_source(controlled_image_element_source)
    verify_controlled_http_image_protocol_proof_source(
        message_baseline_test_source,
        controlled_http_image_fixture_source,
        controlled_http_image_multipart_fixture_source,
    )
    verify_controlled_http_image_replacement_authority_source(
        controlled_session_shell_source,
        controlled_rendering_settlement_source,
    )
    verify_controlled_image_per_pipeline_cache_source(controlled_image_cache_source)
    verify_controlled_profile_wire_source(controlled_profile_wire_source)
    verify_controlled_image_transport_source(
        controlled_image_messaging_source,
        controlled_image_producer_fence_source,
        controlled_image_window_source,
        controlled_image_script_thread_source,
        execution_limits_source,
    )
    verify_controlled_image_pending_and_teardown_source(
        controlled_image_window_source,
        controlled_image_script_thread_source,
    )
    verify_controlled_inline_svg_rendering_source(
        controlled_inline_svg_source,
        controlled_image_window_source,
        message_baseline_test_source,
        controlled_inline_svg_fixture_source,
        controlled_inline_svg_advanced_fixture_source,
    )
    native_bounds = {
        "maximumRetainedNativePortEntriesPerGlobal": rust_usize_constant(
            message_limits_source,
            "MAX_CONTROLLED_LOCAL_MESSAGE_PORTS",
            "MessageChannel retained-port",
        ),
        "maximumRetainedMessagesPerGlobal": rust_usize_constant(
            message_limits_source,
            "MAX_CONTROLLED_LOCAL_RETAINED_MESSAGES",
            "MessageChannel retained-message",
        ),
        "maximumSerializedPayloadBytes": rust_usize_constant(
            message_limits_source,
            "MAX_CONTROLLED_LOCAL_SERIALIZED_BYTES",
            "MessageChannel serialized-payload",
        ),
        "ordinaryTasks": controlled_webapp_ordinary_task_limit(execution_limits_source),
        "renderingOpportunities": controlled_webapp_rendering_opportunity_limit(execution_limits_source),
        "maximumControlledDataSvgImageUrlBytes": rust_usize_constant(
            controlled_image_element_source,
            "CONTROLLED_V2_DIRECT_DATA_SVG_URL_LIMIT",
            "controlled data-SVG image serialized-URL",
        ),
        "maximumControlledHttpImageInitialUrlBytes": rust_usize_constant(
            controlled_image_element_source,
            "CONTROLLED_V2_DIRECT_HTTP_IMAGE_URL_LIMIT",
            "controlled HTTP(S) image initial serialized-URL",
        ),
        "maximumControlledInlineSvgUrlBytes": rust_usize_constant(
            controlled_inline_svg_source,
            "CONTROLLED_V2_INLINE_DATA_SVG_URL_LIMIT",
            "controlled inline SVG serialized-URL",
        ),
        "maximumRetainedControlledImageOwnershipRecordsPerWindow": rust_usize_constant(
            controlled_image_window_source,
            "CONTROLLED_V2_IMAGE_RETAINED_RECORD_LIMIT",
            "controlled image retained ownership-record",
        ),
    }
    profile_bounds = {
        "maximumRetainedNativePortEntriesPerGlobal": construction.get("maximumRetainedNativePortEntriesPerGlobal"),
        "maximumRetainedMessagesPerGlobal": post_message.get("maximumRetainedMessagesPerGlobal"),
        "maximumSerializedPayloadBytes": post_message.get("maximumSerializedPayloadBytes"),
        "ordinaryTasks": execution_limits.get("ordinaryTasks"),
        "renderingOpportunities": execution_limits.get("renderingOpportunities"),
        "maximumControlledDataSvgImageUrlBytes": controlled_image_selection.get(
            "maximumInitialSelectedCanonicalUrlBytes"
        ),
        "maximumControlledHttpImageInitialUrlBytes": controlled_image_selection.get(
            "maximumInitialSelectedCanonicalUrlBytes"
        ),
        "maximumControlledInlineSvgUrlBytes": controlled_inline_svg_admission.get("maximumSerializedUrlBytes"),
        "maximumRetainedControlledImageOwnershipRecordsPerWindow": (
            controlled_image_retention.get("maximumRetainedControlledOwnershipRecordsPerWindow")
        ),
    }
    if profile_bounds != native_bounds:
        raise ReleaseError(
            "controlled-web-session-v2 bounds differ from native constants: "
            f"profile={profile_bounds}, native={native_bounds}"
        )
    if delivery.get("ordinaryTaskAccounting") != (
        "one_dispatched_message_event_consumes_one_executionLimits.ordinaryTasks"
    ):
        raise ReleaseError("controlled-web-session-v2 must account each delivered message against ordinaryTasks")
    return {
        "path": CANDIDATE_V2_PROFILE.as_posix(),
        "sha256": profile_sha256,
        "contractPath": CANDIDATE_V2_CONTRACT.as_posix(),
        "contractSha256": contract_sha256,
        "identity": expected_identity,
        "predecessor": compatibility["predecessor"],
        "bounds": native_bounds,
        "routerOwnership": construction["routerOwnership"],
        "controlledInputMethodFocus": controlled_input_method,
        "controlledFocusEventTimestamp": controlled_focus_event_timestamp,
        "controlledAutomationEventTimestamps": controlled_automation_event_timestamps,
        "controlledCssAnimationEventTimestamps": controlled_css_animation_event_timestamps,
        "controlledImageElement": controlled_image_element,
        "controlledInlineSvgRendering": controlled_inline_svg,
        "controlledCookiePersistence": cookie_persistence,
        "controlledCookieTimeRange": cookie_time_range,
        "controlledCookieSameSite": cookie_same_site,
        "embedderControls": embedder_controls,
        "portBackedTransferInterfaces": transfer_preflight["interfaces"],
        "retainedWorkProjection": retained_work_projection,
    }


def normalized_tar_info(name: str, *, directory: bool, executable: bool = False) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.type = tarfile.DIRTYPE if directory else tarfile.REGTYPE
    info.mode = 0o755 if directory or executable else 0o644
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    info.mtime = 0
    return info


def add_normalized_tar_members(package: tarfile.TarFile, bundle_directory: Path) -> None:
    members = sorted(bundle_directory.iterdir(), key=lambda item: item.name)
    package.addfile(normalized_tar_info(bundle_directory.name, directory=True))
    for member in members:
        require_regular_file(member, "bundle member")
        info = normalized_tar_info(
            f"{bundle_directory.name}/{member.name}",
            directory=False,
            executable=member.name == BINARY_NAME,
        )
        info.size = member.stat().st_size
        with member.open("rb") as source:
            package.addfile(info, source)


def create_deterministic_tar(bundle_directory: Path, archive: Path) -> None:
    with tarfile.open(archive, mode="w", format=tarfile.USTAR_FORMAT) as package:
        add_normalized_tar_members(package, bundle_directory)


def create_deterministic_archive(bundle_directory: Path, archive: Path) -> None:
    with archive.open("wb") as raw_archive:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw_archive, compresslevel=9, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as package:
                add_normalized_tar_members(package, bundle_directory)


def decompress_canonical_gzip(
    archive: Path,
    destination: Path,
    *,
    max_output_bytes: int = MAX_UNCOMPRESSED_ARCHIVE_BYTES,
) -> None:
    if max_output_bytes <= 0:
        raise ReleaseError("maximum decompressed archive size must be positive")
    expected_header = bytes.fromhex("1f8b08000000000002ff")
    with archive.open("rb") as source:
        header = source.read(len(expected_header))
        if header != expected_header:
            raise ReleaseError("archive does not use the canonical Stasis gzip header")
        source.seek(0)
        decompressor = zlib.decompressobj(wbits=16 + zlib.MAX_WBITS)
        output_bytes = 0
        with destination.open("wb") as output:
            while chunk := source.read(1024 * 1024):
                pending = chunk
                while pending:
                    remaining = max_output_bytes - output_bytes
                    decompressed = decompressor.decompress(
                        pending,
                        min(DECOMPRESSION_CHUNK_BYTES, remaining + 1),
                    )
                    output_bytes += len(decompressed)
                    if output_bytes > max_output_bytes:
                        raise ReleaseError(f"archive exceeds the maximum decompressed size of {max_output_bytes} bytes")
                    output.write(decompressed)
                    pending = decompressor.unconsumed_tail
                    if decompressor.eof:
                        break
                if decompressor.eof:
                    if decompressor.unused_data or source.read(1):
                        raise ReleaseError("archive has data or another gzip member after canonical EOF")
                    break
            flushed = decompressor.flush(min(DECOMPRESSION_CHUNK_BYTES, max_output_bytes - output_bytes + 1))
            output_bytes += len(flushed)
            if output_bytes > max_output_bytes:
                raise ReleaseError(f"archive exceeds the maximum decompressed size of {max_output_bytes} bytes")
            output.write(flushed)
        if not decompressor.eof:
            raise ReleaseError("archive gzip stream ended before its validated trailer")


def expected_generated_assets(version: str, platform: str, revision: str, repository: str) -> dict[str, bytes]:
    return {
        "INSTALL.txt": install_text(version, platform).encode("utf-8"),
        "NATIVE-LIBRARIES.txt": native_libraries_text(version, platform).encode("utf-8"),
        "README.md": readme_text(version, platform, repository).encode("utf-8"),
        "SOURCE.txt": source_text(repository, revision).encode("utf-8"),
        "VERSION.txt": version_text(version, platform, revision).encode("utf-8"),
    }


def member_size_limit(name: str) -> int:
    return MAX_BINARY_MEMBER_BYTES if name == BINARY_NAME else MAX_TEXT_MEMBER_BYTES


def require_bounded_nonempty_file(filename: Path, name: str) -> None:
    require_regular_file(filename, f"bundle member {name}")
    size = filename.stat().st_size
    limit = member_size_limit(name)
    if size <= 0 or size > limit:
        raise ReleaseError(f"bundle member {name} has invalid size {size}; allowed range is 1..{limit} bytes")


def require_executable_mode(
    filename: Path,
    description: str,
    *,
    self_test_allow_unrepresentable_windows_mode: bool = False,
) -> None:
    if stat.S_IMODE(filename.stat().st_mode) & 0o111 != 0:
        return
    if self_test_allow_unrepresentable_windows_mode and os.name == "nt":
        return
    raise ReleaseError(f"{description} is not executable: {filename}")


def validate_bundle_directory(
    directory: Path,
    *,
    version: str,
    platform: str,
    revision: str,
    repository: str,
    source_root: Path,
    _self_test_allow_unrepresentable_windows_mode: bool = False,
) -> None:
    actual = {item.name for item in directory.iterdir()}
    if actual != EXPECTED_FILES:
        raise ReleaseError(
            f"bundle inventory differs: missing={sorted(EXPECTED_FILES - actual)} "
            f"extra={sorted(actual - EXPECTED_FILES)}"
        )
    for name in EXPECTED_FILES:
        require_bounded_nonempty_file(directory / name, name)
    require_executable_mode(
        directory / BINARY_NAME,
        "packaged stasis executable",
        self_test_allow_unrepresentable_windows_mode=(_self_test_allow_unrepresentable_windows_mode),
    )
    for packaged_name, source_name in SOURCE_ASSETS.items():
        source = source_root / source_name
        require_regular_file(source, f"source asset {source_name}")
        if (directory / packaged_name).read_bytes() != source.read_bytes():
            raise ReleaseError(f"packaged source asset differs: {packaged_name}")
    verify_upstream_manifest(directory / "STASIS_UPSTREAM.toml")
    for name, expected in expected_generated_assets(version, platform, revision, repository).items():
        if (directory / name).read_bytes() != expected:
            raise ReleaseError(f"generated release asset differs: {name}")


def create_release(
    *,
    binary: Path,
    dist: Path,
    version: str,
    platform: str,
    revision: str,
    repository: str,
    source_root: Path,
    _self_test_allow_unrepresentable_windows_mode: bool = False,
) -> dict[str, str]:
    validate_identity(version, platform, revision, repository)
    verify_candidate_v2_profile(source_root)
    require_bounded_nonempty_file(binary, BINARY_NAME)
    require_executable_mode(
        binary,
        "Stasis binary",
        self_test_allow_unrepresentable_windows_mode=(_self_test_allow_unrepresentable_windows_mode),
    )
    for packaged_name, source_name in SOURCE_ASSETS.items():
        source = source_root / source_name
        require_regular_file(source, f"source asset for {packaged_name}")
        size = source.stat().st_size
        if size <= 0 or size > MAX_TEXT_MEMBER_BYTES:
            raise ReleaseError(
                f"source asset {source_name} has invalid size {size}; allowed range is 1..{MAX_TEXT_MEMBER_BYTES} bytes"
            )

    dist.mkdir(parents=True, exist_ok=True)
    names = release_asset_names(version, platform)
    expected_outputs = {names["archive"], names["archive_sha256"], names["binary_sha256"]}
    existing_outputs = {name for name in expected_outputs if (dist / name).exists()}
    if existing_outputs:
        raise ReleaseError(f"refusing to overwrite release outputs: {sorted(existing_outputs)}")

    with tempfile.TemporaryDirectory(prefix=".stasis-package-", dir=dist) as temporary:
        bundle = Path(temporary) / bundle_name(version, platform)
        bundle.mkdir(mode=0o755)
        shutil.copyfile(binary, bundle / BINARY_NAME)
        (bundle / BINARY_NAME).chmod(0o755)
        for packaged_name, source_name in SOURCE_ASSETS.items():
            shutil.copyfile(source_root / source_name, bundle / packaged_name)
            (bundle / packaged_name).chmod(0o644)
        generated = expected_generated_assets(version, platform, revision, repository)
        for name, content in generated.items():
            (bundle / name).write_bytes(content)
            (bundle / name).chmod(0o644)
        validate_bundle_directory(
            bundle,
            version=version,
            platform=platform,
            revision=revision,
            repository=repository,
            source_root=source_root,
            _self_test_allow_unrepresentable_windows_mode=(_self_test_allow_unrepresentable_windows_mode),
        )
        create_deterministic_archive(bundle, dist / names["archive"])

    archive_digest = sha256_file(dist / names["archive"])
    binary_digest = sha256_file(binary)
    write_sidecar(dist / names["archive_sha256"], archive_digest, names["archive"])
    write_sidecar(
        dist / names["binary_sha256"],
        binary_digest,
        f"{bundle_name(version, platform)}/{BINARY_NAME}",
    )
    return {
        "archive": str(dist / names["archive"]),
        "archiveSha256": archive_digest,
        "binarySha256": binary_digest,
    }


def verify_asset_directory(directory: Path, expected: Iterable[str]) -> None:
    if not directory.is_dir():
        raise ReleaseError(f"release asset directory does not exist: {directory}")
    expected_set = set(expected)
    entries = list(directory.iterdir())
    actual = {entry.name for entry in entries}
    if any(entry.is_symlink() or not entry.is_file() for entry in entries) or actual != expected_set:
        raise ReleaseError(
            f"release assets differ: missing={sorted(expected_set - actual)} extra={sorted(actual - expected_set)}"
        )


def verify_tar_metadata(package: tarfile.TarFile, bundle: str) -> dict[str, tarfile.TarInfo]:
    expected_member_count = len(EXPECTED_FILES) + 1
    members: list[tarfile.TarInfo] = []
    for member in package:
        if len(members) == expected_member_count:
            raise ReleaseError(f"archive contains more than the exact {expected_member_count}-member limit")
        members.append(member)
    names = [member.name for member in members]
    expected_names = {bundle, *(f"{bundle}/{name}" for name in EXPECTED_FILES)}
    if len(names) != len(set(names)) or set(names) != expected_names:
        raise ReleaseError(
            f"archive inventory differs: missing={sorted(expected_names - set(names))} "
            f"extra={sorted(set(names) - expected_names)}"
        )
    by_name = {member.name: member for member in members}
    root = by_name[bundle]
    if not root.isdir() or root.mode != 0o755:
        raise ReleaseError("archive root is not the normalized bundle directory")
    for member in members:
        if member.uid != 0 or member.gid != 0 or member.uname or member.gname or member.mtime != 0:
            raise ReleaseError(f"archive member metadata is not normalized: {member.name}")
        if member.name == bundle:
            continue
        relative = member.name.removeprefix(f"{bundle}/")
        expected_mode = 0o755 if relative == BINARY_NAME else 0o644
        if not member.isfile() or member.mode != expected_mode:
            raise ReleaseError(f"archive member type or mode is invalid: {member.name}")
        member_limit = member_size_limit(relative)
        if member.size <= 0 or member.size > member_limit:
            raise ReleaseError(
                f"archive member {member.name} has invalid size {member.size}; allowed range is 1..{member_limit} bytes"
            )
    total_member_bytes = sum(member.size for member in members if member.isfile())
    if total_member_bytes > MAX_UNCOMPRESSED_ARCHIVE_BYTES:
        raise ReleaseError(
            f"archive members exceed the maximum total uncompressed size of {MAX_UNCOMPRESSED_ARCHIVE_BYTES} bytes"
        )
    return by_name


def copy_and_hash(source: BinaryIO, destination: BinaryIO | None) -> str:
    digest = hashlib.sha256()
    for chunk in iter(lambda: source.read(1024 * 1024), b""):
        digest.update(chunk)
        if destination is not None:
            destination.write(chunk)
    return digest.hexdigest()


def verify_release(
    *,
    asset_directory: Path,
    version: str,
    platform: str,
    revision: str,
    repository: str,
    source_root: Path,
    extract_to: Path | None,
) -> dict[str, str]:
    validate_identity(version, platform, revision, repository)
    verify_candidate_v2_profile(source_root)
    verify_upstream_manifest(source_root / SOURCE_ASSETS["STASIS_UPSTREAM.toml"])
    names = release_asset_names(version, platform)
    verify_asset_directory(
        asset_directory,
        [names["archive"], names["archive_sha256"], names["binary_sha256"]],
    )
    archive = asset_directory / names["archive"]
    archive_size = archive.stat().st_size
    if archive_size <= 0 or archive_size > MAX_COMPRESSED_ARCHIVE_BYTES:
        raise ReleaseError(
            f"archive size {archive_size} is outside the allowed range 1..{MAX_COMPRESSED_ARCHIVE_BYTES} bytes"
        )
    archive_digest = sha256_file(archive)
    if parse_sidecar(asset_directory / names["archive_sha256"], names["archive"]) != archive_digest:
        raise ReleaseError("archive SHA-256 does not match its sidecar")
    expected_binary_digest = parse_sidecar(
        asset_directory / names["binary_sha256"],
        f"{bundle_name(version, platform)}/{BINARY_NAME}",
    )

    stage: Path | None = None
    bundle = bundle_name(version, platform)
    if extract_to is not None:
        if extract_to.exists():
            raise ReleaseError(f"refusing to overwrite extraction destination: {extract_to}")
        extract_to.parent.mkdir(parents=True, exist_ok=True)
        stage = Path(tempfile.mkdtemp(prefix=f".{extract_to.name}.", dir=extract_to.parent))
    else:
        stage = Path(tempfile.mkdtemp(prefix=".stasis-verify-"))

    try:
        downloaded_tar = stage / ".downloaded.tar"
        decompress_canonical_gzip(archive, downloaded_tar)
        with tarfile.open(downloaded_tar, mode="r:") as package:
            by_name = verify_tar_metadata(package, bundle)
            generated = expected_generated_assets(version, platform, revision, repository)
            for name in sorted(EXPECTED_FILES):
                member = by_name[f"{bundle}/{name}"]
                source = package.extractfile(member)
                if source is None:
                    raise ReleaseError(f"cannot read archive member: {member.name}")
                destination_path = stage / bundle / name
                destination_path.parent.mkdir(parents=True, exist_ok=True)
                destination_handle: BinaryIO | None = destination_path.open("wb")
                try:
                    if name == BINARY_NAME:
                        binary_digest = copy_and_hash(source, destination_handle)
                    else:
                        content = source.read()
                        if destination_handle is not None:
                            destination_handle.write(content)
                        if name in SOURCE_ASSETS:
                            source_filename = source_root / SOURCE_ASSETS[name]
                            require_regular_file(source_filename, f"source asset {SOURCE_ASSETS[name]}")
                            expected = source_filename.read_bytes()
                        else:
                            expected = generated[name]
                        if content != expected:
                            raise ReleaseError(f"archive member content differs: {name}")
                finally:
                    if destination_handle is not None:
                        destination_handle.close()
                destination_path.chmod(0o755 if name == BINARY_NAME else 0o644)
        if binary_digest != expected_binary_digest:
            raise ReleaseError("packaged binary SHA-256 does not match its sidecar")
        canonical_tar = stage / ".canonical.tar"
        create_deterministic_tar(stage / bundle, canonical_tar)
        if not files_equal(downloaded_tar, canonical_tar):
            raise ReleaseError("archive tar bytes are not the canonical normalized Stasis serialization")
        canonical_tar.unlink()
        downloaded_tar.unlink()
        if extract_to is not None:
            stage.rename(extract_to)
            stage = None
    finally:
        if stage is not None:
            shutil.rmtree(stage)

    result = {
        "archive": str(archive),
        "archiveSha256": archive_digest,
        "binarySha256": expected_binary_digest,
    }
    if extract_to is not None:
        result["binary"] = str(extract_to / bundle / BINARY_NAME)
    return result


def parse_native_gate_identity(
    log_text: str,
    *,
    version: str,
    revision: str,
    archive_name: str,
    archive_digest: str,
    binary_digest: str,
) -> dict[str, str]:
    records = [
        strict_json_loads(line.removeprefix(GATE_RECORD_PREFIX), "gate log release record")
        for line in log_text.splitlines()
        if line.startswith(GATE_RECORD_PREFIX)
    ]
    if len(records) != 1:
        raise ReleaseError("gate log must contain exactly one structured release-artifact record")
    record = records[0]
    expected_keys = {"schema", "gate", "test", "version", "archive", "binary", "source"}
    if not isinstance(record, dict) or set(record) != expected_keys:
        raise ReleaseError("gate log release-artifact record has an unexpected schema")
    if type(record["schema"]) is not int or record["schema"] != GATE_PROOF_SCHEMA:
        raise ReleaseError("gate log release-artifact record schema does not match")
    if record["gate"] != GATE_NAME or record["test"] != GATE_TEST or record["version"] != version:
        raise ReleaseError("gate log release-artifact gate or version does not match")
    if record["archive"] != {"name": archive_name, "sha256": archive_digest}:
        raise ReleaseError("gate log release-artifact archive does not match the selected archive")
    binary = record["binary"]
    if (
        not isinstance(binary, dict)
        or set(binary) != {"path", "sha256"}
        or not isinstance(binary["path"], str)
        or not binary["path"]
        or binary["sha256"] != binary_digest
    ):
        raise ReleaseError("gate log release-artifact binary does not match")
    sources = record["source"]
    expected = expected_source_identities(revision)
    if sources != expected:
        raise ReleaseError("gate log does not report the exact Stasis and upstream source identities")
    return expected


def create_gate_proof(
    *,
    proof: Path,
    gate_log: Path,
    asset_directory: Path,
    version: str,
    platform: str,
    revision: str,
    run_id: str,
    run_attempt: str,
) -> dict[str, object]:
    validate_identity(version, platform, revision, "https://github.com/oxhq/stasis")
    require_fullmatch(RUN_ID_RE, run_id, "workflow run ID")
    require_fullmatch(RUN_ID_RE, run_attempt, "workflow run attempt")
    log_text = gate_log.read_text(encoding="utf-8", errors="strict")
    if log_text.count(GATE_SUCCESS) != 1 or log_text.count(f"test {GATE_TEST} ... ok") != 1:
        raise ReleaseError("gate log does not prove the exact one-test act-settle-inspect gate passed")
    names = release_asset_names(version, platform)
    archive = asset_directory / names["archive"]
    archive_digest = parse_sidecar(asset_directory / names["archive_sha256"], names["archive"])
    if archive_digest != sha256_file(archive):
        raise ReleaseError("cannot create gate proof for an archive with an invalid sidecar")
    binary_label = f"{bundle_name(version, platform)}/{BINARY_NAME}"
    binary_digest = parse_sidecar(asset_directory / names["binary_sha256"], binary_label)
    sources = parse_native_gate_identity(
        log_text,
        version=version,
        revision=revision,
        archive_name=names["archive"],
        archive_digest=archive_digest,
        binary_digest=binary_digest,
    )
    document: dict[str, object] = {
        "schema": GATE_PROOF_SCHEMA,
        "gate": GATE_NAME,
        "test": GATE_TEST,
        "version": version,
        "platform": platform,
        "revision": revision,
        "workflowRunId": run_id,
        "workflowRunAttempt": run_attempt,
        "archive": {"name": names["archive"], "sha256": archive_digest},
        "binary": {"path": binary_label, "sha256": binary_digest},
        "source": sources,
        "gateLogSha256": sha256_file(gate_log),
    }
    if proof.exists():
        raise ReleaseError(f"refusing to overwrite gate proof: {proof}")
    proof.write_text(
        json.dumps(document, allow_nan=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
        newline="\n",
    )
    return document


def verify_gate_proof(
    *,
    proof_directory: Path,
    asset_directory: Path,
    version: str,
    platform: str,
    revision: str,
    run_id: str,
    run_attempt: str,
) -> dict[str, object]:
    validate_identity(version, platform, revision, "https://github.com/oxhq/stasis")
    require_fullmatch(RUN_ID_RE, run_id, "workflow run ID")
    require_fullmatch(RUN_ID_RE, run_attempt, "workflow run attempt")
    names = release_asset_names(version, platform)
    verify_asset_directory(proof_directory, [names["gate_proof"]])
    verify_asset_directory(
        asset_directory,
        [names["archive"], names["archive_sha256"], names["binary_sha256"]],
    )
    proof_path = proof_directory / names["gate_proof"]
    try:
        proof_text = proof_path.read_text(encoding="utf-8")
    except (OSError, UnicodeError) as error:
        raise ReleaseError(f"cannot read gate proof: {error}") from error
    document = strict_json_loads(proof_text, "gate proof")
    expected_top_level = {
        "schema",
        "gate",
        "test",
        "version",
        "platform",
        "revision",
        "workflowRunId",
        "workflowRunAttempt",
        "archive",
        "binary",
        "source",
        "gateLogSha256",
    }
    if not isinstance(document, dict) or set(document) != expected_top_level:
        raise ReleaseError("gate proof has an unexpected top-level schema")
    archive_digest = parse_sidecar(asset_directory / names["archive_sha256"], names["archive"])
    if sha256_file(asset_directory / names["archive"]) != archive_digest:
        raise ReleaseError("gate proof archive sidecar does not match the selected archive")
    binary_label = f"{bundle_name(version, platform)}/{BINARY_NAME}"
    binary_digest = parse_sidecar(asset_directory / names["binary_sha256"], binary_label)
    expected_values = {
        "schema": GATE_PROOF_SCHEMA,
        "gate": GATE_NAME,
        "test": GATE_TEST,
        "version": version,
        "platform": platform,
        "revision": revision,
        "workflowRunId": run_id,
        "workflowRunAttempt": run_attempt,
        "archive": {"name": names["archive"], "sha256": archive_digest},
        "binary": {"path": binary_label, "sha256": binary_digest},
        "source": expected_source_identities(revision),
    }
    for key, expected in expected_values.items():
        if document.get(key) != expected:
            raise ReleaseError(f"gate proof field {key} does not match the selected package run")
    if type(document["schema"]) is not int:
        raise ReleaseError("gate proof schema must be an integer")
    gate_log_digest = document["gateLogSha256"]
    if not isinstance(gate_log_digest, str):
        raise ReleaseError("gate proof log SHA-256 must be a string")
    require_fullmatch(SHA256_RE, gate_log_digest, "gate log SHA-256")
    return document


def self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="stasis-release-self-test-") as temporary:
        root = Path(temporary)
        repository_root = Path(__file__).resolve().parents[3]
        verify_frozen_v1_profile(repository_root)
        verify_frozen_v2_profile(repository_root)
        verify_candidate_v2_profile(repository_root)
        generated_readme = readme_text(
            RELEASE_VERSION,
            "linux-x86_64",
            "servo/stasis",
        )
        if "stable execution-profile boundary" not in generated_readme:
            raise ReleaseError("self-test archive README omits the stable v2 profile boundary")
        if "candidate execution-profile boundary" in generated_readme:
            raise ReleaseError("self-test archive README regressed to candidate v2 wording")
        candidate_profile_root = root / "candidate-profile-source"
        candidate_authority_sources = (
            CANDIDATE_V2_PROFILE,
            CANDIDATE_V2_CONTRACT,
            PUBLIC_TOP_LEVEL_README,
            PUBLIC_STASIS_BOUNDARY,
            PUBLIC_PROFILE_README,
            PUBLIC_TYPESCRIPT_SDK_README,
            PUBLIC_RELEASE_RUNBOOK,
            PUBLIC_RELEASE_WORKFLOW,
            PUBLIC_NPM_PUBLISH_WORKFLOW,
            MESSAGE_CHANNEL_LIMITS_SOURCE,
            MESSAGE_CHANNEL_BASELINE_TEST_SOURCE,
            MESSAGE_CHANNEL_MULTI_PAIR_FIXTURE,
            MESSAGE_CHANNEL_SOURCE,
            STRUCTURED_CLONE_SOURCE,
            MESSAGE_PORT_SOURCE,
            INPUT_METHOD_CONTROL_SOURCE,
            INPUT_METHOD_INPUT_SOURCE,
            INPUT_METHOD_TEXTAREA_SOURCE,
            EVENT_SOURCE,
            FOCUS_EVENT_SOURCE,
            CONTROLLED_AUTOMATION_SOURCE,
            CONTROLLED_AUTOMATION_EVENT_TARGET_SOURCE,
            CONTROLLED_AUTOMATION_INPUT_EVENT_SOURCE,
            CONTROLLED_AUTOMATION_POINTER_EVENT_SOURCE,
            CONTROLLED_AUTOMATION_SUBMIT_EVENT_SOURCE,
            CONTROLLED_AUTOMATION_FORM_DATA_EVENT_SOURCE,
            CONTROLLED_AUTOMATION_EVENT_FIXTURE,
            CONTROLLED_CSS_ANIMATION_SOURCE,
            CONTROLLED_CSS_ANIMATION_EVENT_SOURCE,
            CONTROLLED_CSS_TRANSITION_EVENT_SOURCE,
            CONTROLLED_CSS_DOCUMENT_SOURCE,
            CONTROLLED_CSS_ANIMATION_EVENT_FIXTURE,
            CONTROLLED_RENDERING_SETTLEMENT_SOURCE,
            CONTROLLED_SESSION_SHELL_SOURCE,
            CONTROLLED_IMAGE_ELEMENT_SOURCE,
            CONTROLLED_IMAGE_WINDOW_SOURCE,
            CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE,
            CONTROLLED_IMAGE_MESSAGING_SOURCE,
            CONTROLLED_IMAGE_PRODUCER_FENCE_SOURCE,
            CONTROLLED_IMAGE_CACHE_SOURCE,
            CONTROLLED_PROFILE_WIRE_SOURCE,
            CONTROLLED_HTTP_IMAGE_FIXTURE,
            CONTROLLED_HTTP_IMAGE_MULTIPART_FIXTURE,
            CONTROLLED_INLINE_SVG_SOURCE,
            CONTROLLED_INLINE_SVG_FIXTURE,
            CONTROLLED_INLINE_SVG_ADVANCED_FIXTURE,
            EXECUTION_LIMITS_SOURCE,
        )

        def reset_candidate_authority_sources() -> None:
            for source_name in candidate_authority_sources:
                destination = candidate_profile_root / source_name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(repository_root / source_name, destination)

        def require_candidate_mutation_rejected(description: str) -> None:
            try:
                verify_candidate_v2_profile(candidate_profile_root)
            except ReleaseError:
                pass
            else:
                raise ReleaseError(f"self-test accepted changed candidate {description}")

        def require_public_marker_mutation_rejected(
            source_name: Path,
            marker: str,
            replacement: str,
            description: str,
        ) -> None:
            reset_candidate_authority_sources()
            filename = candidate_profile_root / source_name
            source = filename.read_text(encoding="utf-8")
            if source.count(marker) != 1:
                raise ReleaseError(f"self-test cannot uniquely locate {description} public marker")
            filename.write_text(source.replace(marker, replacement, 1), encoding="utf-8")
            require_candidate_mutation_rejected(description)

        reset_candidate_authority_sources()
        verify_candidate_v2_profile(candidate_profile_root)

        public_surface_mutations = (
            (
                PUBLIC_TOP_LEVEL_README,
                "Source version and package CI are not publication proof.",
                "Source version and package CI prove publication.",
                "top-level source-versus-publication boundary",
            ),
            (
                PUBLIC_TOP_LEVEL_README,
                "0.3.0 is the stable successor only when its",
                "0.3.0 is the stable successor even before its",
                "top-level immutable public-successor evidence boundary",
            ),
            (
                PUBLIC_TOP_LEVEL_README,
                "`controlled-web-session-v1` still the default",
                "`controlled-web-session-v2` is now the default",
                "top-level frozen v1 default",
            ),
            (
                PUBLIC_STASIS_BOUNDARY,
                "Source version `0.3.0` is not a publication claim",
                "Source version `0.3.0` is a publication claim",
                "STASIS source-versus-publication boundary",
            ),
            (
                PUBLIC_TYPESCRIPT_SDK_README,
                "is not a publication claim: the native runtime must advertise v2",
                "is a publication claim: the native runtime must advertise v2",
                "SDK README source-versus-publication boundary",
            ),
            (
                PUBLIC_RELEASE_RUNBOOK,
                "Source version is not\na publication claim",
                "Source version is\na publication claim",
                "release-runbook source-versus-publication boundary",
            ),
            (
                PUBLIC_PROFILE_README,
                "without inventing an\nasynchronous `Image` producer lease",
                "by inventing an\nasynchronous `Image` producer lease",
                "profile README synchronous cache-hit no-lease boundary",
            ),
            (
                PUBLIC_PROFILE_README,
                "Finite asynchronous cache/decode completion is producer-fenced",
                "Finite asynchronous cache/decode completion is not producer-fenced",
                "profile README asynchronous image producer fence",
            ),
            (
                PUBLIC_PROFILE_README,
                "with resource I/O separately owned and",
                "with resource I/O hidden inside image callback work and",
                "profile README HTTP image Resource-I/O ownership",
            ),
            (
                PUBLIC_TYPESCRIPT_SDK_README,
                "without inventing an asynchronous\n`Image` producer lease",
                "by inventing an asynchronous\n`Image` producer lease",
                "SDK README synchronous cache-hit no-lease boundary",
            ),
            (
                PUBLIC_TYPESCRIPT_SDK_README,
                "Finite asynchronous cache/decode completion is producer-fenced",
                "Finite asynchronous cache/decode completion is not producer-fenced",
                "SDK README asynchronous image producer fence",
            ),
            (
                PUBLIC_TYPESCRIPT_SDK_README,
                "while HTTP image resource I/O remains active retains fatal `blocked_on_external_io`",
                "while HTTP image resource I/O remains active succeeds without `blocked_on_external_io`",
                "SDK README active-image replacement boundary",
            ),
            (
                PUBLIC_STASIS_BOUNDARY,
                "canonical HTTP(S) URL no larger than 65,536 bytes",
                "canonical HTTP(S) URL no larger than 65,537 bytes",
                "STASIS HTTP image initial-URL bound",
            ),
            (
                CANDIDATE_V2_CONTRACT,
                "A final redirect URL is not rechecked against the initial 65,536-byte",
                "A final redirect URL is rechecked against the initial 65,536-byte",
                "v2 contract HTTP image final-redirect boundary",
            ),
            (
                PUBLIC_STASIS_BOUNDARY,
                "HTTP(S) Resource I/O remains separately owned.",
                "HTTP(S) Resource I/O is hidden in image callback work.",
                "STASIS HTTP image Resource authority",
            ),
            (
                PUBLIC_STASIS_BOUNDARY,
                "A final redirect URL is not rechecked",
                "A final redirect URL is rechecked",
                "STASIS HTTP image final-redirect boundary",
            ),
            (
                PUBLIC_STASIS_BOUNDARY,
                "pipeline's image-cache store under immutable fixture routes",
                "global image-cache store under live routes",
                "STASIS HTTP image cache-provenance boundary",
            ),
            (
                PUBLIC_STASIS_BOUNDARY,
                "after its separately owned Resource I/O drains",
                "before its separately owned Resource I/O drains",
                "STASIS finite multipart retirement boundary",
            ),
            (
                PUBLIC_STASIS_BOUNDARY,
                "an endless response remains blocked on external I/O",
                "an endless response becomes quiescent",
                "STASIS endless multipart external-I/O boundary",
            ),
            (
                PUBLIC_STASIS_BOUNDARY,
                "Public document replacement while HTTP image Resource I/O is active remains fatal",
                "Public document replacement while HTTP image Resource I/O is active succeeds",
                "STASIS active-image replacement boundary",
            ),
            (
                PUBLIC_RELEASE_RUNBOOK,
                "A final redirect URL is not rechecked against that initial limit",
                "A final redirect URL is rechecked against that initial limit",
                "release-runbook HTTP image final-redirect boundary",
            ),
            (
                PUBLIC_RELEASE_RUNBOOK,
                "one pipeline's image-cache store under immutable fixture routes",
                "a global image-cache store under live routes",
                "release-runbook HTTP image cache-provenance boundary",
            ),
            (
                PUBLIC_RELEASE_WORKFLOW,
                "Public document replacement while HTTP image resource I/O remains active keeps",
                "Public document replacement while HTTP image resource I/O remains active succeeds and removes",
                "release-note active-image replacement boundary",
            ),
            (
                PUBLIC_RELEASE_WORKFLOW,
                "without an invented asynchronous Image producer lease",
                "with an invented asynchronous Image producer lease",
                "release-note synchronous cache-hit no-lease boundary",
            ),
            (
                PUBLIC_RELEASE_WORKFLOW,
                "redirect URL is not rechecked against that initial selected-URL limit",
                "redirect URL is rechecked against that initial selected-URL limit",
                "release-note HTTP image final-redirect boundary",
            ),
            (
                PUBLIC_RELEASE_WORKFLOW,
                "one pipeline's image-cache store under immutable fixture routes",
                "a global image-cache store under live routes",
                "release-note HTTP image cache-provenance boundary",
            ),
            (
                PUBLIC_RELEASE_WORKFLOW,
                'automation_dynamic_fields = set(automation_time_fields) | {"controlledTrace"}',
                "automation_dynamic_fields = set(automation_time_fields)",
                "package attestation dynamic automation timestamp evidence",
            ),
            (
                PUBLIC_RELEASE_WORKFLOW,
                "max_u128 = (1 << 128) - 1",
                "max_u128 = (1 << 64) - 1",
                "package attestation u128 automation timestamp bound",
            ),
            (
                PUBLIC_NPM_PUBLISH_WORKFLOW,
                "controlled_trace != expected_controlled_trace",
                "controlled_trace == expected_controlled_trace",
                "npm-publish attestation exact automation trace reconstruction",
            ),
            (
                PUBLIC_NPM_PUBLISH_WORKFLOW,
                "dispatched_virtual_time_ns != advanced_virtual_time_ns",
                "dispatched_virtual_time_ns == advanced_virtual_time_ns",
                "npm-publish attestation automation dispatch-time equality",
            ),
        )
        for source_name, marker, replacement, description in public_surface_mutations:
            require_public_marker_mutation_rejected(
                source_name,
                marker,
                replacement,
                description,
            )

        reset_candidate_authority_sources()
        wire_filename = candidate_profile_root / CONTROLLED_PROFILE_WIRE_SOURCE
        wire_source = wire_filename.read_text(encoding="utf-8")
        enabled_wire_test = (
            "    #[test]\n    fn controlled_web_session_v2_profile_is_an_explicit_bounded_surface_expansion()"
        )
        if wire_source.count(enabled_wire_test) != 1:
            raise ReleaseError("self-test cannot uniquely locate enabled v2 wire profile proof")
        wire_filename.write_text(
            wire_source.replace(
                enabled_wire_test,
                "    #[allow(dead_code)]\n"
                "    fn controlled_web_session_v2_profile_is_an_explicit_bounded_surface_expansion()",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("disabled v2 wire profile proof")

        reset_candidate_authority_sources()
        wire_source = wire_filename.read_text(encoding="utf-8")
        pinned_wire_profile_sha = f'"{CANDIDATE_V2_PROFILE_SHA256}"'
        if wire_source.count(pinned_wire_profile_sha) != 1:
            raise ReleaseError("self-test cannot uniquely locate v2 wire profile hash")
        wire_filename.write_text(
            wire_source.replace(
                pinned_wire_profile_sha,
                '"0000000000000000000000000000000000000000000000000000000000000000"',
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("stale v2 wire profile hash")

        reset_candidate_authority_sources()
        wire_source = wire_filename.read_text(encoding="utf-8")
        stable_wire_status = (
            'assert_eq!(profile["releaseStatus"], "stable_contract");\n'
            '        assert_eq!(profile["targetRelease"], "0.3.0");'
        )
        if wire_source.count(stable_wire_status) != 1:
            raise ReleaseError("self-test cannot uniquely locate v2 wire release status")
        wire_filename.write_text(
            wire_source.replace(
                stable_wire_status,
                'assert_eq!(profile["releaseStatus"], "candidate_contract");\n'
                '        assert_eq!(profile["targetRelease"], "0.3.0");',
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("stale v2 wire release status")

        reset_candidate_authority_sources()
        wire_source = wire_filename.read_text(encoding="utf-8")
        wire_replacement_boundary = (
            '"inflightHttpDocumentReplacement": '
            '"fatal_blocked_on_external_io_before_cross_document_successor_authority"'
        )
        if wire_source.count(wire_replacement_boundary) != 1:
            raise ReleaseError("self-test cannot uniquely locate v2 wire HTTP replacement assertion")
        wire_filename.write_text(
            wire_source.replace(
                wire_replacement_boundary,
                '"inflightHttpDocumentReplacement": "quiescent"',
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("v2 wire HTTP replacement assertion")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test candidate profile",
        )
        if type(candidate_profile) is not dict:
            raise ReleaseError("self-test candidate profile root changed")
        candidate_profile["execution"]["messageChannel"]["construction"][
            "maximumRetainedNativePortEntriesPerGlobal"
        ] = 31
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("bounds that differ from native constants")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test candidate cookie time-range boundary",
        )
        candidate_profile["sessionState"]["cookies"]["timeRange"]["postOpenNetworkRequestAboveMaximum"]["fatal"] = True
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("post-open cookie time-range fatality")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test candidate SameSite response storage",
        )
        candidate_profile["sessionState"]["cookies"]["sameSite"]["responseStorage"]["crossSiteSubresource"] = (
            "all_valid_cookies_eligible"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("cross-site response-cookie storage")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test candidate profile router ownership",
        )
        candidate_profile["execution"]["messageChannel"]["construction"]["routerOwnership"] = "external_router"
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("router ownership")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test candidate profile transfer interfaces",
        )
        candidate_profile["execution"]["portBackedStructuredCloneTransferPreflight"]["interfaces"].remove(
            "TransformStream"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("port-backed transfer interfaces")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test closed candidate InputMethod focus boundary",
        )
        candidate_profile["execution"]["controlledInputMethodFocus"]["additionalInputMethodShape"] = "suppressed"
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("open InputMethod focus boundary")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test closed candidate embedder-control boundary",
        )
        candidate_profile["unsupportedClasses"]["embedderControls"]["additionalInputMethodShape"] = (
            "suppressed_without_external_work"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("open embedder-control boundary")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test candidate InputMethod product surface",
        )
        candidate_profile["supportedProductSurface"].append(
            "controlled_top_level_multiline_input_method_presentation_suppression"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("additional InputMethod product surface")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test predecessor contract identity",
        )
        candidate_profile["compatibility"]["predecessorContractUnchanged"] = False
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("predecessor contract identity")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test controlled cookie lifetime",
        )
        candidate_profile["sessionState"]["cookies"]["persistence"]["maximumLifetimeSeconds"] = 34_560_001
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("controlled cookie lifetime expansion")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test v2 state artifact compatibility",
        )
        candidate_profile["sessionState"]["compatibleSelectedProfiles"].append("controlled-web-session-v1")
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("implicit v1-to-v2 state migration")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test inherited candidate session scope",
        )
        candidate_profile["sessionScope"]["childBrowsingContexts"] = "supported"
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("inherited iframe session scope")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test exact MessageChannel candidate contract",
        )
        candidate_profile["execution"]["messageChannel"]["construction"]["completePairCapacityFromEmptyGlobal"] = 999
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("unqualified MessageChannel pair capacity")

        reset_candidate_authority_sources()
        message_channel_filename = candidate_profile_root / MESSAGE_CHANNEL_SOURCE
        message_channel_source = message_channel_filename.read_text(encoding="utf-8")
        incumbent_resolution = "let incumbent = GlobalScope::incumbent();"
        if message_channel_source.count(incumbent_resolution) != 1:
            raise ReleaseError("self-test cannot uniquely locate MessageChannel incumbent resolution")
        message_channel_filename.write_text(
            message_channel_source.replace(
                incumbent_resolution,
                "let incumbent = Some(DomRoot::from_ref(global));",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("MessageChannel constructor incumbent resolution")

        reset_candidate_authority_sources()
        message_limits_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_limits_source = message_limits_filename.read_text(encoding="utf-8")
        exact_target_gate = ".is_some_and(ScriptThread::current_controlled_top_level_target_matches)"
        if message_limits_source.count(exact_target_gate) != 1:
            raise ReleaseError("self-test cannot uniquely locate MessageChannel exact target gate")
        message_limits_filename.write_text(
            message_limits_source.replace(exact_target_gate, ".is_some()", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("MessageChannel exact target admission")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test closed candidate supported product surface",
        )
        candidate_profile["supportedProductSurface"].append("controlled_iframe_context_tree")
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("arbitrary supported product surface")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test closed FocusEvent timestamp boundary",
        )
        candidate_profile["execution"]["controlledFocusEventTimestamp"]["events"].append("click")
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("expanded FocusEvent timestamp boundary")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test generic automation event timestamp scope",
        )
        candidate_profile["execution"]["controlledAutomationEventTimestamps"]["coverage"] = (
            "only_representative_proof_event_names"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("automation event-name allowlist substituted for causal scope")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test script-created automation event boundary",
        )
        candidate_profile["execution"]["controlledAutomationEventTimestamps"]["scriptCreatedConstructors"] = (
            "document_clock_timestamp"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("script-created events admitted to automation timestamp scope")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test automation event timestamp product surface",
        )
        candidate_profile["supportedProductSurface"].append("controlled_script_created_automation_event_timestamp")
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("additional automation event timestamp product surface")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test closed CSS animation event-kind boundary",
        )
        candidate_profile["execution"]["controlledCssAnimationEventTimestamps"]["eventKinds"].append("animationframe")
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("expanded CSS animation event timestamp kinds")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test script-created CSS event boundary",
        )
        candidate_profile["execution"]["controlledCssAnimationEventTimestamps"]["scriptCreatedConstructors"] = (
            "document_clock_timestamp"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("script-created CSS event constructors admitted")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test exact CSS pending-event record admission",
        )
        candidate_profile["execution"]["controlledCssAnimationEventTimestamps"]["recordAdmission"] = (
            "every_AnimationEvent_and_TransitionEvent"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("CSS event timestamp record provenance bypass")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test CSS public non-auxiliary target admission",
        )
        candidate_profile["execution"]["controlledCssAnimationEventTimestamps"]["targetAdmission"] = (
            "any_top_level_Window"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("CSS auxiliary top-level target promotion")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test CSS transition settlement compatibility boundary",
        )
        candidate_profile["execution"]["controlledCssAnimationEventTimestamps"]["transitionSettlementCompatibility"] = (
            "general_controlled_transition_settlement"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("general CSS transition settlement promotion")

        reset_candidate_authority_sources()
        settlement_filename = candidate_profile_root / CONTROLLED_RENDERING_SETTLEMENT_SOURCE
        settlement_source = settlement_filename.read_text(encoding="utf-8")
        pending_event_demand = (
            "            || rendering.document_update_required\n"
            "            || rendering.pending_animation_events != 0\n"
            "            || rendering.finite_animations != 0"
        )
        if settlement_source.count(pending_event_demand) != 1:
            raise ReleaseError("self-test cannot uniquely locate pending animation-event finite demand")
        settlement_filename.write_text(
            settlement_source.replace(
                pending_event_demand,
                "            || rendering.document_update_required\n            || rendering.finite_animations != 0",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("scheduled pending animation-event finite demand")

        reset_candidate_authority_sources()
        shell_filename = candidate_profile_root / CONTROLLED_SESSION_SHELL_SOURCE
        shell_source = shell_filename.read_text(encoding="utf-8")
        v2_replacement_profile_fence = (
            "profile == Some(SessionProfile::ControlledWebSessionV2) && active_operations != 0"
        )
        if shell_source.count(v2_replacement_profile_fence) != 1:
            raise ReleaseError("self-test cannot uniquely locate the v2 replacement I/O profile fence")
        shell_filename.write_text(
            shell_source.replace(
                v2_replacement_profile_fence,
                "profile.is_some_and(SessionProfile::supports_session_api) && active_operations != 0",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("v1 replacement external-I/O promotion")

        reset_candidate_authority_sources()
        shell_filename = candidate_profile_root / CONTROLLED_SESSION_SHELL_SOURCE
        shell_source = shell_filename.read_text(encoding="utf-8")
        replacement_authorization_call = (
            "source_external_io_active_at_authorization:\n"
            "                        controlled_network_blocks_document_replacement(\n"
            "                            active_profile,\n"
            "                            controlled_network_active_operations,\n"
            "                        ),"
        )
        if shell_source.count(replacement_authorization_call) != 1:
            raise ReleaseError("self-test cannot uniquely locate the replacement I/O authorization call")
        shell_filename.write_text(
            shell_source.replace(
                replacement_authorization_call,
                "source_external_io_active_at_authorization:\n"
                "                        controlled_network_blocks_document_replacement(\n"
                "                            active_profile,\n"
                "                            0,\n"
                "                        ),",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("hardcoded-inactive replacement authorization")

        reset_candidate_authority_sources()
        shell_filename = candidate_profile_root / CONTROLLED_SESSION_SHELL_SOURCE
        shell_source = shell_filename.read_text(encoding="utf-8")
        replacement_latch_handoff = (
            "coordinator.latch_additional_foreground_external_io_active(\n"
            "                    *source_external_io_active_at_authorization,\n"
            "                );"
        )
        if shell_source.count(replacement_latch_handoff) != 1:
            raise ReleaseError("self-test cannot uniquely locate the replacement I/O latch handoff")
        shell_filename.write_text(
            shell_source.replace(
                replacement_latch_handoff,
                "coordinator.latch_additional_foreground_external_io_active(\n"
                "                    false,\n"
                "                );",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("hardcoded-false replacement I/O latch handoff")

        reset_candidate_authority_sources()
        settlement_filename = candidate_profile_root / CONTROLLED_RENDERING_SETTLEMENT_SOURCE
        settlement_source = settlement_filename.read_text(encoding="utf-8")
        monotonic_replacement_latch = "self.latched_additional_foreground_external_io_active |= active;"
        if settlement_source.count(monotonic_replacement_latch) != 1:
            raise ReleaseError("self-test cannot uniquely locate the monotonic replacement I/O latch")
        settlement_filename.write_text(
            settlement_source.replace(
                monotonic_replacement_latch,
                "self.latched_additional_foreground_external_io_active = active;",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("reversible replacement external-I/O latch")

        reset_candidate_authority_sources()
        settlement_filename = candidate_profile_root / CONTROLLED_RENDERING_SETTLEMENT_SOURCE
        settlement_source = settlement_filename.read_text(encoding="utf-8")
        latched_replacement_decision = (
            "if self.latched_additional_foreground_external_io_active\n"
            "            || self.additional_foreground_external_io_active"
        )
        if settlement_source.count(latched_replacement_decision) != 1:
            raise ReleaseError("self-test cannot uniquely locate the replacement I/O decision latch")
        settlement_filename.write_text(
            settlement_source.replace(
                latched_replacement_decision,
                "if self.additional_foreground_external_io_active",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("ignored replacement external-I/O latch")

        reset_candidate_authority_sources()
        settlement_filename = candidate_profile_root / CONTROLLED_RENDERING_SETTLEMENT_SOURCE
        settlement_source = settlement_filename.read_text(encoding="utf-8")
        latched_replacement_decision = (
            "if self.latched_additional_foreground_external_io_active\n"
            "            || self.additional_foreground_external_io_active"
        )
        if settlement_source.count(latched_replacement_decision) != 1:
            raise ReleaseError("self-test cannot uniquely locate the replacement I/O decision latch")
        settlement_filename.write_text(
            settlement_source.replace(
                latched_replacement_decision,
                "if self.latched_additional_foreground_external_io_active && false\n"
                "            || self.additional_foreground_external_io_active",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("logically neutralized replacement external-I/O latch")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test closed host-timestamp boundary",
        )
        candidate_profile["unsupportedClasses"]["hostTimestamp"]["otherControlledEvents"] = "document_clock_timestamp"
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("open host-timestamp boundary")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test FocusEvent timestamp product surface",
        )
        candidate_profile["supportedProductSurface"].append("controlled_script_created_focus_event_timestamp")
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("additional FocusEvent timestamp product surface")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test controlled image selection boundary",
        )
        candidate_profile["execution"]["controlledImageElement"]["selection"]["dataUrlParser"] = "string_prefix"
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("noncanonical image URL parser")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test controlled HTTP image initial URL bound",
        )
        candidate_profile["execution"]["controlledImageElement"]["selection"][
            "maximumInitialSelectedCanonicalUrlBytes"
        ] = 65_537
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("controlled HTTP image initial URL bound")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test controlled HTTP image origin and redirect boundary",
        )
        candidate_profile["execution"]["controlledImageElement"]["selection"]["httpHttpsOrigin"] = "same_origin_only"
        candidate_profile["execution"]["controlledImageElement"]["selection"]["httpHttpsRedirects"] = (
            "final_URL_rejected"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("controlled HTTP image origin or redirect boundary")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test controlled HTTP image cache and multipart boundary",
        )
        candidate_profile["execution"]["controlledImageElement"]["selection"]["cacheReuseProofBoundary"] = (
            "cross_pipeline_global_cache"
        )
        candidate_profile["execution"]["controlledImageElement"]["completion"]["multipartMixedReplace"] = (
            "owned_quiescent_decode_failure"
        )
        candidate_profile["execution"]["controlledImageElement"]["completion"]["inflightHttpDocumentReplacement"] = (
            "controlled_ready"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("controlled HTTP image cache, multipart, or replacement boundary")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test controlled image Unsupported provenance reconciliation",
        )
        candidate_profile["execution"]["controlledImageElement"]["pending"]["unsupportedReservationReconciliation"] = (
            "unsupported_records_retain_controlled_reservations"
        )
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("controlled image Unsupported provenance reconciliation")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test controlled image bounds",
        )
        candidate_profile["execution"]["controlledImageElement"]["retention"][
            "maximumRetainedControlledOwnershipRecordsPerWindow"
        ] = 513
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("controlled image registration bound")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test controlled image unsupported boundary",
        )
        candidate_profile["execution"]["controlledImageElement"]["unsupported"]["blobFileAndNonSvgDataUrls"] = "owned"
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("expanded controlled image source surface")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test frozen predecessor hash identity",
        )
        candidate_profile["compatibility"]["predecessorProfileSha256"] = "0" * 64
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("frozen predecessor profile hash")

        reset_candidate_authority_sources()
        candidate_contract_filename = candidate_profile_root / CANDIDATE_V2_CONTRACT
        candidate_contract_filename.write_bytes(candidate_contract_filename.read_bytes() + b"\n")
        require_candidate_mutation_rejected("candidate contract hash")

        reset_candidate_authority_sources()
        input_method_source_filename = candidate_profile_root / INPUT_METHOD_CONTROL_SOURCE
        input_method_source = input_method_source_filename.read_text(encoding="utf-8")
        input_method_source_filename.write_text(
            input_method_source.replace(
                "execution_profile == DocumentExecutionProfile::ControlledWebSessionV2",
                "execution_profile == DocumentExecutionProfile::Baseline",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled InputMethod execution-profile fence")

        reset_candidate_authority_sources()
        input_method_source_filename = candidate_profile_root / INPUT_METHOD_CONTROL_SOURCE
        input_method_source = input_method_source_filename.read_text(encoding="utf-8")
        semantic_guard = "if semantic_automation_focus_active {\n        return true;\n    }"
        changed_semantic_guard = semantic_guard.replace("return true;", "return false;")
        mutated_input_method_source = input_method_source.replace(semantic_guard, changed_semantic_guard, 1)
        if mutated_input_method_source == input_method_source:
            raise ReleaseError("self-test cannot mutate the semantic focus guard")
        input_method_source_filename.write_text(
            mutated_input_method_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("false semantic focus guard")

        reset_candidate_authority_sources()
        input_method_source_filename = candidate_profile_root / INPUT_METHOD_CONTROL_SOURCE
        input_method_source = input_method_source_filename.read_text(encoding="utf-8")
        mutated_input_method_source = input_method_source.replace(
            "&& !input_method.multiline",
            "&& (!input_method.multiline || input_method.multiline)",
            1,
        )
        if mutated_input_method_source == input_method_source:
            raise ReleaseError("self-test cannot mutate the multiline admission fence")
        input_method_source_filename.write_text(
            mutated_input_method_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("tautological multiline admission fence")

        reset_candidate_authority_sources()
        input_method_source_filename = candidate_profile_root / INPUT_METHOD_CONTROL_SOURCE
        input_method_source = input_method_source_filename.read_text(encoding="utf-8")
        virtual_keyboard_predicate = "&& !input_method.allow_virtual_keyboard"
        if input_method_source.count(virtual_keyboard_predicate) != 1:
            raise ReleaseError("self-test cannot uniquely locate the virtual-keyboard admission fence")
        input_method_source_filename.write_text(
            input_method_source.replace(
                virtual_keyboard_predicate,
                "&& true",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("virtual-keyboard admission fence")

        reset_candidate_authority_sources()
        textarea_source_filename = candidate_profile_root / INPUT_METHOD_TEXTAREA_SOURCE
        textarea_source = textarea_source_filename.read_text(encoding="utf-8")
        textarea_multiline = "multiline: true,"
        if textarea_source.count(textarea_multiline) != 1:
            raise ReleaseError("self-test cannot uniquely locate the textarea multiline provenance")
        textarea_source_filename.write_text(
            textarea_source.replace(textarea_multiline, "multiline: false,", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("textarea multiline producer provenance")

        reset_candidate_authority_sources()
        unreviewed_producer = candidate_profile_root / "components/script/dom/unreviewed_input_method_producer.rs"
        unreviewed_producer.parent.mkdir(parents=True, exist_ok=True)
        unreviewed_producer.write_text(
            """
fn unreviewed_input_method_producer() -> InputMethodRequest {
    InputMethodRequest {
        input_method_type: InputMethodType::Text,
        text: String::new(),
        insertion_point: None,
        multiline: false,
        allow_virtual_keyboard: false,
    }
}
""",
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("unreviewed InputMethod producer")
        unreviewed_producer.unlink()

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test retained MessagePort work projection",
        )
        candidate_profile["execution"]["messageChannel"]["delivery"]["retainedWorkProjection"][
            "zeroRetainedMessages"
        ] = "makes_idle_pair_pending"
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("retained MessagePort work projection")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test reciprocal MessagePort pair work projection",
        )
        candidate_profile["execution"]["messageChannel"]["delivery"]["retainedWorkProjection"][
            "reciprocalPairWithOwnedWork"
        ] = "one_deterministic_maximum_port_identity_per_pair"
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("reciprocal MessagePort pair work projection contract")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test exact MessagePort reservation identity",
        )
        candidate_profile["execution"]["messageChannel"]["delivery"]["retainedWorkProjection"][
            "reservationIdentity"
        ] = "global_unattributed_reservation"
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("unattributed MessagePort reservation identity")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test exact MessagePort accounting reconciliation",
        )
        candidate_profile["execution"]["messageChannel"]["delivery"]["retainedWorkProjection"][
            "accountingReconciliation"
        ] = "global_retained_at_least_observed_work"
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("inexact MessagePort accounting reconciliation")

        reset_candidate_authority_sources()
        candidate_profile = strict_json_loads(
            (candidate_profile_root / CANDIDATE_V2_PROFILE).read_text(encoding="utf-8"),
            "self-test fail-closed MessagePort association boundary",
        )
        candidate_profile["execution"]["messageChannel"]["delivery"]["retainedWorkProjection"][
            "invalidMissingOrZeroDestinationAssociation"
        ] = "ignore"
        (candidate_profile_root / CANDIDATE_V2_PROFILE).write_text(
            json.dumps(candidate_profile, allow_nan=False), encoding="utf-8"
        )
        require_candidate_mutation_rejected("open MessagePort association boundary")

        reset_candidate_authority_sources()
        structured_clone_filename = candidate_profile_root / STRUCTURED_CLONE_SOURCE
        structured_clone_source = structured_clone_filename.read_text(encoding="utf-8")
        structured_clone_filename.write_text(
            structured_clone_source.replace(
                "root_from_object::<TransformStream>",
                "root_from_object::<UnboundTransformStream>",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("structured-clone source preflight")

        reset_candidate_authority_sources()
        structured_clone_filename = candidate_profile_root / STRUCTURED_CLONE_SOURCE
        structured_clone_source = structured_clone_filename.read_text(encoding="utf-8")
        mutated_structured_clone_source = structured_clone_source.replace(
            "controlled_local_execution_profile_selected()",
            "true",
            1,
        )
        if mutated_structured_clone_source == structured_clone_source:
            raise ReleaseError("self-test cannot mutate the v2 transfer-preflight fence")
        structured_clone_filename.write_text(
            mutated_structured_clone_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("v2-only transfer-preflight fence")

        reset_candidate_authority_sources()
        message_port_post_filename = candidate_profile_root / MESSAGE_PORT_SOURCE
        message_port_post_source = message_port_post_filename.read_text(encoding="utf-8")
        mutated_message_port_post_source = message_port_post_source.replace(
            "            transfer.is_empty(),\n",
            "            true,\n",
            1,
        )
        if mutated_message_port_post_source == message_port_post_source:
            raise ReleaseError("self-test cannot mutate postMessage transfer admission")
        message_port_post_filename.write_text(
            mutated_message_port_post_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("postMessage transfer admission ordering")

        reset_candidate_authority_sources()
        message_port_post_filename = candidate_profile_root / MESSAGE_PORT_SOURCE
        message_port_post_source = message_port_post_filename.read_text(encoding="utf-8")
        mutated_message_port_post_source = message_port_post_source.replace(
            "            incumbent.as_deref(),\n",
            "            None,\n",
            1,
        )
        if mutated_message_port_post_source == message_port_post_source:
            raise ReleaseError("self-test cannot mutate postMessage incumbent admission")
        message_port_post_filename.write_text(
            mutated_message_port_post_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("postMessage exact-incumbent admission")

        reset_candidate_authority_sources()
        focus_event_source_filename = candidate_profile_root / FOCUS_EVENT_SOURCE
        focus_event_source = focus_event_source_filename.read_text(encoding="utf-8")
        mutated_focus_event_source = focus_event_source.replace(
            "== DocumentControlProfile::TopLevelSession",
            "!= DocumentControlProfile::TopLevelSession",
            1,
        )
        if mutated_focus_event_source == focus_event_source:
            raise ReleaseError("self-test cannot invert the FocusEvent control-profile fence")
        focus_event_source_filename.write_text(
            mutated_focus_event_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("inverted FocusEvent control-profile fence")

        reset_candidate_authority_sources()
        focus_event_source_filename = candidate_profile_root / FOCUS_EVENT_SOURCE
        focus_event_source = focus_event_source_filename.read_text(encoding="utf-8")
        mutated_focus_event_source = focus_event_source.replace(
            "== DocumentExecutionProfile::ControlledWebSessionV2",
            "!= DocumentExecutionProfile::ControlledWebSessionV2",
            1,
        )
        if mutated_focus_event_source == focus_event_source:
            raise ReleaseError("self-test cannot invert the FocusEvent execution-profile fence")
        focus_event_source_filename.write_text(
            mutated_focus_event_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("inverted FocusEvent execution-profile fence")

        reset_candidate_authority_sources()
        focus_event_source_filename = candidate_profile_root / FOCUS_EVENT_SOURCE
        focus_event_source = focus_event_source_filename.read_text(encoding="utf-8")
        mutated_focus_event_source = focus_event_source.replace(
            "&& window.is_top_level()",
            "&& true",
            1,
        )
        if mutated_focus_event_source == focus_event_source:
            raise ReleaseError("self-test cannot mutate the top-level FocusEvent fence")
        focus_event_source_filename.write_text(
            mutated_focus_event_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("top-level FocusEvent timestamp fence")

        reset_candidate_authority_sources()
        automation_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        automation_script_thread_source = automation_script_thread_filename.read_text(encoding="utf-8")
        automation_scope_start = automation_script_thread_source.find(
            "let synchronous_automation_event_time = if operation.is_mutating()"
        )
        automation_scope_end = automation_script_thread_source.find(
            "let capture_synchronous_navigation =", automation_scope_start
        )
        if automation_scope_start < 0 or automation_scope_end < 0:
            raise ReleaseError("self-test cannot isolate automation timestamp admission")
        automation_scope = automation_script_thread_source[automation_scope_start:automation_scope_end]
        automation_script_thread_filename.write_text(
            automation_script_thread_source[:automation_scope_start]
            + automation_scope.replace("operation.is_mutating()", "true", 1)
            + automation_script_thread_source[automation_scope_end:],
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("non-mutating automation timestamp scope admission")

        reset_candidate_authority_sources()
        automation_control_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        automation_control_source = automation_control_filename.read_text(encoding="utf-8")
        automation_restore = "self.time.set(self.previous);"
        if automation_control_source.count(automation_restore) != 1:
            raise ReleaseError("self-test cannot uniquely locate automation timestamp scope restoration")
        automation_control_filename.write_text(
            automation_control_source.replace(automation_restore, "let _ = self.previous;", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("automation timestamp RAII restoration")

        reset_candidate_authority_sources()
        automation_event_target_filename = candidate_profile_root / CONTROLLED_AUTOMATION_EVENT_TARGET_SOURCE
        automation_event_target_source = automation_event_target_filename.read_text(encoding="utf-8")
        generic_event_stamp = "event.set_creation_time_stamp(time_stamp);"
        if automation_event_target_source.count(generic_event_stamp) != 1:
            raise ReleaseError("self-test cannot uniquely locate generic automation event timestamp stamp")
        automation_event_target_filename.write_text(
            automation_event_target_source.replace(generic_event_stamp, "let _ = time_stamp;", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("generic synchronous browser-event timestamp locality")

        reset_candidate_authority_sources()
        automation_input_event_filename = candidate_profile_root / CONTROLLED_AUTOMATION_INPUT_EVENT_SOURCE
        automation_input_event_source = automation_input_event_filename.read_text(encoding="utf-8")
        input_constructor = "fn Constructor("
        if automation_input_event_source.count(input_constructor) != 1:
            raise ReleaseError("self-test cannot uniquely locate script-created InputEvent constructor")
        automation_input_event_filename.write_text(
            automation_input_event_source.replace(
                input_constructor,
                "fn Constructor(/* synchronous_automation_event_time */",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("script-created InputEvent automation-scope bypass")

        reset_candidate_authority_sources()
        automation_protocol_filename = candidate_profile_root / MESSAGE_CHANNEL_BASELINE_TEST_SOURCE
        automation_protocol_source = automation_protocol_filename.read_text(encoding="utf-8")
        representative_reset_stamp = "reset:reset:5"
        if automation_protocol_source.count(representative_reset_stamp) != 2:
            raise ReleaseError("self-test cannot locate both representative reset timestamp traces")
        automation_protocol_filename.write_text(
            automation_protocol_source.replace(representative_reset_stamp, "reset:reset:0", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("representative derived reset-event timestamp proof")

        reset_candidate_authority_sources()
        css_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        css_script_thread_source = css_script_thread_filename.read_text(encoding="utf-8")
        auxiliary_gate = "if window_proxy.is_auxiliary()"
        if css_script_thread_source.count(auxiliary_gate) != 1:
            raise ReleaseError("self-test cannot uniquely locate the CSS timestamp auxiliary gate")
        css_script_thread_filename.write_text(
            css_script_thread_source.replace(auxiliary_gate, "if false", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("auxiliary top-level CSS timestamp target admission")

        reset_candidate_authority_sources()
        css_animations_filename = candidate_profile_root / CONTROLLED_CSS_ANIMATION_SOURCE
        css_animations_source = css_animations_filename.read_text(encoding="utf-8")
        css_sample = "window.sample_controlled_v2_document_performance_time()"
        if css_animations_source.count(css_sample) != 1:
            raise ReleaseError("self-test cannot uniquely locate the CSS dispatch-batch time sample")
        css_animations_filename.write_text(
            css_animations_source.replace(
                css_sample,
                "Ok(PerformanceEntryTime::Host(CrossProcessInstant::now()))",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("CSS dispatch batch document-time sample before queue take")

        reset_candidate_authority_sources()
        css_animations_filename = candidate_profile_root / CONTROLLED_CSS_ANIMATION_SOURCE
        css_animations_source = css_animations_filename.read_text(encoding="utf-8")
        css_record_match = "ScriptThread::current_controlled_top_level_target_matches(&owner_window)"
        if css_animations_source.count(css_record_match) != 1:
            raise ReleaseError("self-test cannot uniquely locate CSS retained-record target matching")
        css_animations_filename.write_text(
            css_animations_source.replace(css_record_match, "true", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("CSS retained-record target membership bypass")

        reset_candidate_authority_sources()
        css_animations_filename = candidate_profile_root / CONTROLLED_CSS_ANIMATION_SOURCE
        css_animations_source = css_animations_filename.read_text(encoding="utf-8")
        css_event_stamp = "event.upcast::<Event>().set_creation_time_stamp(time_stamp);"
        if css_animations_source.count(css_event_stamp) != 2:
            raise ReleaseError("self-test cannot locate both internal CSS event timestamp stamps")
        css_animations_filename.write_text(
            css_animations_source.replace(css_event_stamp, "let _ = time_stamp;", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("internal TransitionEvent timestamp locality")

        reset_candidate_authority_sources()
        css_animation_event_filename = candidate_profile_root / CONTROLLED_CSS_ANIMATION_EVENT_SOURCE
        css_animation_event_source = css_animation_event_filename.read_text(encoding="utf-8")
        css_constructor = "fn Constructor("
        if css_animation_event_source.count(css_constructor) != 1:
            raise ReleaseError("self-test cannot uniquely locate script-created AnimationEvent constructor")
        css_animation_event_filename.write_text(
            css_animation_event_source.replace(
                css_constructor,
                "fn Constructor(/* controlled_v2_batch_time */",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("script-created AnimationEvent controlled timestamp contamination")

        reset_candidate_authority_sources()
        css_animations_filename = candidate_profile_root / CONTROLLED_CSS_ANIMATION_SOURCE
        css_animations_source = css_animations_filename.read_text(encoding="utf-8")
        pending_event_count = "observation.pending_event_count = self.pending_events.borrow().len()"
        if css_animations_source.count(pending_event_count) != 1:
            raise ReleaseError("self-test cannot uniquely locate retained CSS pending-event accounting")
        css_animations_filename.write_text(
            css_animations_source.replace(
                pending_event_count,
                "observation.pending_event_count = 0",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("retained CSS pending-event settlement accounting")

        reset_candidate_authority_sources()
        css_protocol_filename = candidate_profile_root / MESSAGE_CHANNEL_BASELINE_TEST_SOURCE
        css_protocol_source = css_protocol_filename.read_text(encoding="utf-8")
        css_animation_start_proof = "animationstart:trusted:20:20:owned"
        if css_protocol_source.count(css_animation_start_proof) != 1:
            raise ReleaseError("self-test cannot uniquely locate native CSS animation timestamp proof")
        css_protocol_filename.write_text(
            css_protocol_source.replace(
                css_animation_start_proof,
                "animationstart:untrusted:20:20:owned",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("native CSS animation timestamp proof")

        reset_candidate_authority_sources()
        css_fixture_filename = candidate_profile_root / CONTROLLED_CSS_ANIMATION_EVENT_FIXTURE
        css_fixture_source = css_fixture_filename.read_text(encoding="utf-8")
        script_animation_constructor = 'new AnimationEvent("animationstart"'
        if css_fixture_source.count(script_animation_constructor) != 1:
            raise ReleaseError("self-test cannot uniquely locate script-created CSS event probe")
        css_fixture_filename.write_text(
            css_fixture_source.replace(
                script_animation_constructor,
                'new Event("animationstart"',
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("script-created AnimationEvent host timestamp fixture")

        reset_candidate_authority_sources()
        event_source_filename = candidate_profile_root / EVENT_SOURCE
        event_source = event_source_filename.read_text(encoding="utf-8")
        mutated_event_source = event_source.replace(
            "entry_time_to_dom_high_res_time_stamp(self.time_stamp.get())",
            "to_dom_high_res_time_stamp(CrossProcessInstant::now())",
            1,
        )
        if mutated_event_source == event_source:
            raise ReleaseError("self-test cannot mutate Event timestamp projection")
        event_source_filename.write_text(mutated_event_source, encoding="utf-8")
        require_candidate_mutation_rejected("Event timestamp projection")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        image_top_level_gate = "!ScriptThread::current_controlled_top_level_target_matches(window)"
        if image_element_source.count(image_top_level_gate) != 1:
            raise ReleaseError("self-test cannot uniquely locate exact image target gate")
        image_element_filename.write_text(
            image_element_source.replace(image_top_level_gate, "false", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image exact public target gate")

        reset_candidate_authority_sources()
        image_protocol_filename = candidate_profile_root / MESSAGE_CHANNEL_BASELINE_TEST_SOURCE
        image_protocol_source = image_protocol_filename.read_text(encoding="utf-8")
        multipart_proof_name = (
            "fn controlled_session_v2_http_multipart_finite_response_retires_to_typed_image_load_unsupported()"
        )
        if image_protocol_source.count(multipart_proof_name) != 1:
            raise ReleaseError("self-test cannot uniquely locate controlled HTTP multipart protocol proof")
        image_protocol_filename.write_text(
            image_protocol_source.replace(
                multipart_proof_name,
                "fn deleted_controlled_http_multipart_proof()",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled HTTP multipart native protocol proof")

        reset_candidate_authority_sources()
        http_image_fixture_filename = candidate_profile_root / CONTROLLED_HTTP_IMAGE_FIXTURE
        http_image_fixture_source = http_image_fixture_filename.read_text(encoding="utf-8")
        cross_origin_asset = 'const assets = "https://controlled-image-assets.example.test"'
        if http_image_fixture_source.count(cross_origin_asset) != 1:
            raise ReleaseError("self-test cannot uniquely locate controlled cross-origin HTTP image fixture")
        http_image_fixture_filename.write_text(
            http_image_fixture_source.replace(
                cross_origin_asset,
                "const assets = location.origin",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled cross-origin HTTP image fixture boundary")

        reset_candidate_authority_sources()
        image_cache_filename = candidate_profile_root / CONTROLLED_IMAGE_CACHE_SOURCE
        image_cache_source = image_cache_filename.read_text(encoding="utf-8")
        fresh_completed_loads = "completed_loads: HashMap::new(),"
        if image_cache_source.count(fresh_completed_loads) != 1:
            raise ReleaseError("self-test cannot uniquely locate fresh per-pipeline completed image cache")
        image_cache_filename.write_text(
            image_cache_source.replace(
                fresh_completed_loads,
                "completed_loads: shared_completed_loads.clone(),",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("fresh per-pipeline completed image cache store")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        canonical_data_parser = "DataUrl::process(serialized_url)"
        if image_element_source.count(canonical_data_parser) != 1:
            raise ReleaseError("self-test cannot uniquely locate canonical image DataUrl parser")
        image_element_filename.write_text(
            image_element_source.replace(
                canonical_data_parser,
                'DataUrl::process("data:image/svg+xml,")',
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image canonical DataUrl input")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        direct_src_gate = "direct_src.str().as_ref() != selected_source.as_ref()"
        if image_element_source.count(direct_src_gate) != 1:
            raise ReleaseError("self-test cannot uniquely locate direct image src gate")
        image_element_filename.write_text(
            image_element_source.replace(direct_src_gate, "false", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image direct-src identity gate")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        image_url_limit = "const CONTROLLED_V2_DIRECT_DATA_SVG_URL_LIMIT: usize = 65_536;"
        if image_element_source.count(image_url_limit) != 1:
            raise ReleaseError("self-test cannot uniquely locate controlled image URL bound")
        image_element_filename.write_text(
            image_element_source.replace(
                image_url_limit,
                "const CONTROLLED_V2_DIRECT_DATA_SVG_URL_LIMIT: usize = 65_537;",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image native URL bound")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        http_image_url_limit = "const CONTROLLED_V2_DIRECT_HTTP_IMAGE_URL_LIMIT: usize = 65_536;"
        if image_element_source.count(http_image_url_limit) != 1:
            raise ReleaseError("self-test cannot uniquely locate controlled HTTP image URL bound")
        image_element_filename.write_text(
            image_element_source.replace(
                http_image_url_limit,
                "const CONTROLLED_V2_DIRECT_HTTP_IMAGE_URL_LIMIT: usize = 65_537;",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled HTTP image native URL bound")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        http_scheme_gate = 'matches!(image_url.scheme(), "http" | "https")'
        if image_element_source.count(http_scheme_gate) != 1:
            raise ReleaseError("self-test cannot uniquely locate controlled HTTP scheme gate")
        image_element_filename.write_text(
            image_element_source.replace(
                http_scheme_gate,
                'image_url.scheme() == "https"',
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled HTTP image scheme gate")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        multipart_retirement = ".mark_controlled_v2_image_cache_id_unsupported(self.id);"
        if image_element_source.count(multipart_retirement) != 1:
            raise ReleaseError("self-test cannot uniquely locate controlled multipart image retirement")
        image_element_filename.write_text(
            image_element_source.replace(multipart_retirement, "", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled multipart image Unsupported retirement before terminal EOF")

        reset_candidate_authority_sources()
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        multipart_mime_gate = 'mime.type_() == mime::MULTIPART && mime.subtype().as_str() == "x-mixed-replace"'
        if image_element_source.count(multipart_mime_gate) != 1:
            raise ReleaseError("self-test cannot uniquely locate controlled multipart MIME subtype gate")
        image_element_filename.write_text(
            image_element_source.replace(
                multipart_mime_gate,
                "mime.type_() == mime::MULTIPART",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled multipart x-mixed-replace subtype gate")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        image_registration_limit = "const CONTROLLED_V2_IMAGE_RETAINED_RECORD_LIMIT: usize = 512;"
        if image_window_source.count(image_registration_limit) != 1:
            raise ReleaseError("self-test cannot uniquely locate controlled image capacity")
        image_window_filename.write_text(
            image_window_source.replace(
                image_registration_limit,
                "const CONTROLLED_V2_IMAGE_RETAINED_RECORD_LIMIT: usize = 513;",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image native registration bound")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        retained_reservation = "_reservation: Some(callback_reservation),"
        if image_window_source.count(retained_reservation) != 1:
            raise ReleaseError("self-test cannot locate retained controlled image reservation")
        image_window_filename.write_text(
            image_window_source.replace(retained_reservation, "_reservation: None,", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("retained controlled image reservation")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        unsupported_reservation_release = "callback._reservation = None;"
        if image_window_source.count(unsupported_reservation_release) != 1:
            raise ReleaseError("self-test cannot uniquely locate Unsupported image reservation release")
        image_window_filename.write_text(
            image_window_source.replace(unsupported_reservation_release, "", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("explicit Unsupported image controlled-reservation release")

        reset_candidate_authority_sources()
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        unsupported_callback_exact_id = "self.pending_image_callbacks.borrow_mut().get_mut(&id)"
        if image_window_source.count(unsupported_callback_exact_id) != 1:
            raise ReleaseError("self-test cannot uniquely locate Unsupported callback exact-ID selection")
        image_window_filename.write_text(
            image_window_source.replace(
                unsupported_callback_exact_id,
                "self.pending_image_callbacks.borrow_mut().values_mut().next()",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("Unsupported callback exact cache-ID selection")

        reset_candidate_authority_sources()
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        unsupported_layout_exact_id = "self.pending_layout_images.borrow_mut().get_mut(&id)"
        if image_window_source.count(unsupported_layout_exact_id) != 1:
            raise ReleaseError("self-test cannot uniquely locate Unsupported layout exact-ID selection")
        image_window_filename.write_text(
            image_window_source.replace(
                unsupported_layout_exact_id,
                "self.pending_layout_images.borrow_mut().values_mut().next()",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("Unsupported layout exact cache-ID selection")

        reset_candidate_authority_sources()
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        unsupported_raster_exact_id = "if *candidate_id == id {\n                entry.mark_unsupported();"
        if image_window_source.count(unsupported_raster_exact_id) != 1:
            raise ReleaseError("self-test cannot uniquely locate Unsupported raster exact-ID selection")
        image_window_filename.write_text(
            image_window_source.replace(
                unsupported_raster_exact_id,
                "if true {\n                entry.mark_unsupported();",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("Unsupported raster exact cache-ID selection")

        reset_candidate_authority_sources()
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        raster_unsupported_retirement = (
            "fn mark_unsupported(&mut self) {\n"
            "        self.provenance = PendingImageProvenance::Unsupported;\n"
            "        self.reservation = None;\n"
            "    }"
        )
        if image_window_source.count(raster_unsupported_retirement) != 1:
            raise ReleaseError("self-test cannot uniquely locate raster Unsupported retirement")
        image_window_filename.write_text(
            image_window_source.replace(
                raster_unsupported_retirement,
                raster_unsupported_retirement.replace(
                    "PendingImageProvenance::Unsupported",
                    "PendingImageProvenance::Baseline",
                    1,
                ),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("raster explicit Unsupported provenance retirement")

        reset_candidate_authority_sources()
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        image_window_filename.write_text(
            image_window_source.replace(
                raster_unsupported_retirement,
                raster_unsupported_retirement.replace(
                    "        self.reservation = None;\n",
                    "",
                    1,
                ),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("raster Unsupported controlled-reservation release")

        reset_candidate_authority_sources()
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        unsupported_classifier_start = image_window_source.find("fn image_id_has_explicitly_unsupported_retained_work(")
        unsupported_classifier_end = image_window_source.find(
            "fn image_id_has_baseline_retained_work(", unsupported_classifier_start
        )
        if unsupported_classifier_start < 0 or unsupported_classifier_end < 0:
            raise ReleaseError("self-test cannot locate exact Unsupported cache-ID classifier")
        unsupported_classifier = image_window_source[unsupported_classifier_start:unsupported_classifier_end]
        if unsupported_classifier.count(".get(&id)") != 2:
            raise ReleaseError("self-test cannot locate exact Unsupported callback/layout lookups")
        image_window_filename.write_text(
            image_window_source[:unsupported_classifier_start]
            + unsupported_classifier.replace(".get(&id)", ".values().next()", 1)
            + image_window_source[unsupported_classifier_end:],
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("Unsupported classifier callback exact cache-ID lookup")

        reset_candidate_authority_sources()
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        unsupported_classifier = image_window_source[unsupported_classifier_start:unsupported_classifier_end]
        unsupported_classifier_raster_id = "*candidate_id == id &&"
        if unsupported_classifier.count(unsupported_classifier_raster_id) != 1:
            raise ReleaseError("self-test cannot locate Unsupported classifier raster exact-ID predicate")
        image_window_filename.write_text(
            image_window_source[:unsupported_classifier_start]
            + unsupported_classifier.replace(
                unsupported_classifier_raster_id,
                "true &&",
                1,
            )
            + image_window_source[unsupported_classifier_end:],
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("Unsupported classifier raster exact cache-ID predicate")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        identity_reservation = "_reservation: identity_owner_reservation"
        if image_window_source.count(identity_reservation) < 1:
            raise ReleaseError("self-test cannot locate exact image identity reservation")
        image_window_filename.write_text(
            image_window_source.replace(identity_reservation, "_reservation: None", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("exact image identity-owner reservation")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        raster_reservation = "(Some(reservation), transport)"
        if image_window_source.count(raster_reservation) != 1:
            raise ReleaseError("self-test cannot uniquely locate vector-raster reservation")
        image_window_filename.write_text(
            image_window_source.replace(raster_reservation, "(None, transport)", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled vector-raster retained record")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        raster_listener_install = "image_cache.add_rasterization_complete_listener("
        if image_window_source.count(raster_listener_install) != 1:
            raise ReleaseError("self-test cannot uniquely locate post-reflow raster listener installation")
        image_window_filename.write_text(
            image_window_source.replace(
                raster_listener_install,
                "image_cache.ignore_rasterization_complete_listener(",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("post-reflow exact-key fenced raster listener installation")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        mixed_layout_classification = "!matches!(layout_provenances.get(id), Some((baseline, _)) if *baseline != 0)"
        if image_window_source.count(mixed_layout_classification) != 1:
            raise ReleaseError("self-test cannot uniquely locate mixed layout-owner classification")
        image_window_filename.write_text(
            image_window_source.replace(mixed_layout_classification, "true", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("baseline layout owner controlled-classification exclusion")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        layout_handoff_start = image_window_source.find("for image in pending_images")
        layout_handoff_end = image_window_source.find("for image in pending_rasterization_images", layout_handoff_start)
        if layout_handoff_start < 0 or layout_handoff_end < 0:
            raise ReleaseError("self-test cannot locate layout-owner post-reflow handoff")
        layout_handoff_source = image_window_source[layout_handoff_start:layout_handoff_end]
        baseline_layout_downgrade = "self.downgrade_cached_vector_identity_to_baseline(id);"
        if layout_handoff_source.count(baseline_layout_downgrade) != 1:
            raise ReleaseError("self-test cannot uniquely locate baseline layout-owner global downgrade")
        image_window_filename.write_text(
            image_window_source[:layout_handoff_start]
            + layout_handoff_source.replace(baseline_layout_downgrade, "", 1)
            + image_window_source[layout_handoff_end:],
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("baseline layout-owner global downgrade")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        unsupported_delivery_guard = "retained != PendingImageProvenance::Unsupported && retained == delivery"
        if image_window_source.count(unsupported_delivery_guard) != 1:
            raise ReleaseError("self-test cannot uniquely locate explicit Unsupported delivery guard")
        image_window_filename.write_text(
            image_window_source.replace(
                unsupported_delivery_guard,
                "retained == delivery",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("explicit Unsupported delivery rejection before callbacks")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        retained_mixed_layout_state = (
            "        {\n            return Err(());\n        }\n\n        // We take the images here"
        )
        if image_window_source.count(retained_mixed_layout_state) != 1:
            raise ReleaseError("self-test cannot uniquely locate retained mixed layout-owner state")
        image_window_filename.write_text(
            image_window_source.replace(
                retained_mixed_layout_state,
                retained_mixed_layout_state.replace(
                    "            return Err(());",
                    "            self.pending_layout_images.borrow_mut().remove(&response.id);\n"
                    "            return Err(());",
                ),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("retained mixed layout-owner pending state")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        image_teardown = "self.pending_image_callbacks.borrow_mut().clear();"
        if image_window_source.count(image_teardown) != 1:
            raise ReleaseError("self-test cannot uniquely locate controlled image teardown")
        image_window_filename.write_text(
            image_window_source.replace(image_teardown, "", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image callback teardown")

        reset_candidate_authority_sources()
        image_messaging_filename = candidate_profile_root / CONTROLLED_IMAGE_MESSAGING_SOURCE
        image_messaging_source = image_messaging_filename.read_text(encoding="utf-8")
        guarded_transport = "ControlledV2(DocumentProducerEnvelope<ImageCacheResponseMessage>)"
        if image_messaging_source.count(guarded_transport) != 1:
            raise ReleaseError("self-test cannot uniquely locate guarded image transport")
        image_messaging_filename.write_text(
            image_messaging_source.replace(
                guarded_transport,
                "ControlledV2(ImageCacheResponseMessage)",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("guard-bearing image transport")

        reset_candidate_authority_sources()
        producer_fence_filename = candidate_profile_root / CONTROLLED_IMAGE_PRODUCER_FENCE_SOURCE
        producer_fence_source = producer_fence_filename.read_text(encoding="utf-8")
        vector_terminal = "ImageCacheResponseMessage::VectorImageRasterizationComplete(..) => true"
        if producer_fence_source.count(vector_terminal) != 1:
            raise ReleaseError("self-test cannot uniquely locate vector image terminal")
        producer_fence_filename.write_text(
            producer_fence_source.replace(vector_terminal, "_ => false", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("vector image producer terminal")

        reset_candidate_authority_sources()
        producer_fence_filename = candidate_profile_root / CONTROLLED_IMAGE_PRODUCER_FENCE_SOURCE
        producer_fence_source = producer_fence_filename.read_text(encoding="utf-8")
        owned_cancellation_completion = "        self.complete();\n    }\n}\n\nstruct ImageCallbackState"
        if producer_fence_source.count(owned_cancellation_completion) != 1:
            raise ReleaseError("self-test cannot uniquely locate image callback owned cancellation")
        producer_fence_filename.write_text(
            producer_fence_source.replace(
                owned_cancellation_completion,
                "        self.abandon();\n    }\n}\n\nstruct ImageCallbackState",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("image callback owned cancellation")

        reset_candidate_authority_sources()
        producer_ledger_filename = candidate_profile_root / EXECUTION_LIMITS_SOURCE
        producer_ledger_source = producer_ledger_filename.read_text(encoding="utf-8")
        class_match = "if state.active_leases.get(&lease_id.sequence) != Some(&lease_id.kind) {"
        if producer_ledger_source.count(class_match) != 1:
            raise ReleaseError("self-test cannot uniquely locate producer lease class match")
        producer_ledger_filename.write_text(
            producer_ledger_source.replace(
                class_match,
                "if !state.active_leases.contains_key(&lease_id.sequence) {",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("producer completion and abandonment class match")

        reset_candidate_authority_sources()
        image_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        image_script_thread_source = image_script_thread_filename.read_text(encoding="utf-8")
        image_execution_profile_gate = (
            "                if self.document_control_profile != DocumentControlProfile::TopLevelSession ||\n"
            "                    self.document_execution_profile !=\n"
            "                        DocumentExecutionProfile::ControlledWebSessionV2 ||\n"
            "                    !Self::current_controlled_top_level_target_matches(&window)"
        )
        if image_script_thread_source.count(image_execution_profile_gate) < 1:
            raise ReleaseError("self-test cannot locate image delivery execution-profile gate")
        image_script_thread_filename.write_text(
            image_script_thread_source.replace(
                image_execution_profile_gate,
                image_execution_profile_gate.replace(
                    "self.document_execution_profile !=",
                    "self.document_execution_profile ==",
                    1,
                ),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image delivery profile gate")

        reset_candidate_authority_sources()
        image_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        image_script_thread_source = image_script_thread_filename.read_text(encoding="utf-8")
        retired_target_completion = (
            "                        ControlledImageDeliveryTarget::Retired => {\n"
            "                            // Pipeline teardown established the tombstone before removing this Window,\n"
            "                            // so the queued response has no remaining mutation target.\n"
            "                            drop(guard);\n"
            "                            return;\n"
            "                        },"
        )
        if image_script_thread_source.count(retired_target_completion) != 1:
            raise ReleaseError("self-test cannot uniquely locate retired image-target completion")
        image_script_thread_filename.write_text(
            image_script_thread_source.replace(
                retired_target_completion,
                retired_target_completion.replace("drop(guard);", "let _ = guard.abandon();"),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("retired image target owned cancellation")

        reset_candidate_authority_sources()
        image_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        image_script_thread_source = image_script_thread_filename.read_text(encoding="utf-8")
        unknown_target_abandonment = (
            "                        ControlledImageDeliveryTarget::Unknown => {\n"
            "                            // A missing untombstoned target or a live tombstoned target violates the\n"
            "                            // ScriptThread routing invariant and is not an owned cancellation.\n"
            "                            let _ = guard.abandon();\n"
            "                            return;\n"
            "                        },"
        )
        if image_script_thread_source.count(unknown_target_abandonment) != 1:
            raise ReleaseError("self-test cannot uniquely locate unknown image-target abandonment")
        image_script_thread_filename.write_text(
            image_script_thread_source.replace(
                unknown_target_abandonment,
                unknown_target_abandonment.replace("let _ = guard.abandon();", "drop(guard);"),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("unknown image target abandonment")

        reset_candidate_authority_sources()
        image_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        image_script_thread_source = image_script_thread_filename.read_text(encoding="utf-8")
        prehandler_authority_abandonment = (
            "                    !Self::current_controlled_top_level_target_matches(&window)\n"
            "                {\n"
            "                    let _ = guard.abandon();\n"
            "                    return;\n"
            "                }"
        )
        if image_script_thread_source.count(prehandler_authority_abandonment) != 1:
            raise ReleaseError("self-test cannot uniquely locate pre-handler image authority abandonment")
        image_script_thread_filename.write_text(
            image_script_thread_source.replace(
                prehandler_authority_abandonment,
                prehandler_authority_abandonment.replace("let _ = guard.abandon();", "drop(guard);"),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("pre-handler image authority abandonment")

        reset_candidate_authority_sources()
        image_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        image_script_thread_source = image_script_thread_filename.read_text(encoding="utf-8")
        prehandler_clock_abandonment = (
            "                let Ok(completion_time) = window.sample_controlled_v2_document_performance_time()\n"
            "                else {\n"
            "                    let _ = guard.abandon();\n"
            "                    return;\n"
            "                };"
        )
        if image_script_thread_source.count(prehandler_clock_abandonment) != 1:
            raise ReleaseError("self-test cannot uniquely locate pre-handler image clock abandonment")
        image_script_thread_filename.write_text(
            image_script_thread_source.replace(
                prehandler_clock_abandonment,
                prehandler_clock_abandonment.replace("let _ = guard.abandon();", "drop(guard);"),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("pre-handler image clock abandonment")

        reset_candidate_authority_sources()
        image_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        image_script_thread_source = image_script_thread_filename.read_text(encoding="utf-8")
        retained_handler_completion = (
            "        let _message_completion = ControlledImageMessageCompletion::new(message_guard);"
        )
        if image_script_thread_source.count(retained_handler_completion) != 1:
            raise ReleaseError("self-test cannot uniquely locate retained image-handler completion")
        image_script_thread_filename.write_text(
            image_script_thread_source.replace(
                retained_handler_completion,
                "        drop(message_guard);",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("scoped retained image-handler completion")

        reset_candidate_authority_sources()
        image_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        image_script_thread_source = image_script_thread_filename.read_text(encoding="utf-8")
        handler_unwind_abandonment = "        if std::thread::panicking() {"
        if image_script_thread_source.count(handler_unwind_abandonment) != 1:
            raise ReleaseError("self-test cannot uniquely locate image-handler unwind abandonment")
        image_script_thread_filename.write_text(
            image_script_thread_source.replace(
                handler_unwind_abandonment,
                "        if false {",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("image-handler unwind abandonment")

        reset_candidate_authority_sources()
        image_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        image_script_thread_source = image_script_thread_filename.read_text(encoding="utf-8")
        controlled_pending_projection = (
            "let pending_images = if self.document_execution_profile ==\n"
            "                DocumentExecutionProfile::ControlledWebSessionV2\n"
            "            {\n"
            "                unsupported_image_work"
        )
        if image_script_thread_source.count(controlled_pending_projection) != 1:
            raise ReleaseError("self-test cannot uniquely locate image pending projection")
        image_script_thread_filename.write_text(
            image_script_thread_source.replace(
                controlled_pending_projection,
                controlled_pending_projection.replace("unsupported_image_work", "retained_image_work"),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image pending projection")

        reset_candidate_authority_sources()
        image_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        image_script_thread_source = image_script_thread_filename.read_text(encoding="utf-8")
        guarded_advance_capture = "let guarded = fence.with_matching_snapshot(token.producers().snapshot, || {\n"
        if image_script_thread_source.count(guarded_advance_capture) != 1:
            raise ReleaseError("self-test cannot uniquely locate exact producer-fenced advance capture")
        image_script_thread_filename.write_text(
            image_script_thread_source.replace(
                guarded_advance_capture,
                guarded_advance_capture + "            let _ = fence.snapshot();\n",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("reentrant producer-fence read during controlled advance")

        reset_candidate_authority_sources()
        image_script_thread_filename = candidate_profile_root / CONTROLLED_IMAGE_SCRIPT_THREAD_SOURCE
        image_script_thread_source = image_script_thread_filename.read_text(encoding="utf-8")
        qualified_image_producer_observation = (
            "            producers\n                .snapshot\n                .for_kind(DocumentProducerKind::Image)"
        )
        if image_script_thread_source.count(qualified_image_producer_observation) != 1:
            raise ReleaseError("self-test cannot uniquely locate qualified image producer comparison")
        image_script_thread_filename.write_text(
            image_script_thread_source.replace(
                qualified_image_producer_observation,
                qualified_image_producer_observation.replace(
                    "            producers\n                .snapshot",
                    "            fence\n                .snapshot()",
                ),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image comparison bypasses qualified producer observation")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        image_timestamp_assignment = "event.set_creation_time_stamp(time_stamp)"
        if image_element_source.count(image_timestamp_assignment) != 1:
            raise ReleaseError("self-test cannot uniquely locate image timestamp assignment")
        image_element_filename.write_text(
            image_element_source.replace(image_timestamp_assignment, "return", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image timestamp locality")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        status_race_queue_handoff = "self.queue_controlled_v2_cache_hit_load(delivery);"
        if image_element_source.count(status_race_queue_handoff) != 1:
            raise ReleaseError("self-test cannot uniquely locate controlled image status-race queue handoff")
        image_element_filename.write_text(
            image_element_source.replace(
                status_race_queue_handoff,
                'self.fire_image_completion_events(cx, atom!("load"), delivery);',
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image status-race synchronous event bypass")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        image_generation_gate = "if generation != element.generation_id()"
        if image_element_source.count(image_generation_gate) != 1:
            raise ReleaseError("self-test cannot uniquely locate async image generation gate")
        image_element_filename.write_text(
            image_element_source.replace(image_generation_gate, "if false", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image request-generation fence")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        request_cache_authority = "controlled_cache_id: Option<PendingImageId>"
        if image_element_source.count(request_cache_authority) != 1:
            raise ReleaseError("self-test cannot uniquely locate image request cache authority")
        image_element_filename.write_text(
            image_element_source.replace(
                request_cache_authority,
                "controlled_cache_id: Option<()>",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("request-owned controlled image cache identity")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        successful_registration_authority = "self.record_active_controlled_cache_id(Some(id));"
        if image_element_source.count(successful_registration_authority) != 1:
            raise ReleaseError("self-test cannot uniquely locate successful image registration authority")
        image_element_filename.write_text(
            image_element_source.replace(
                successful_registration_authority,
                "self.record_active_controlled_cache_id(None);",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("successful image registration authority capture")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        pending_authority_move = "pending.controlled_cache_id.take(),"
        if image_element_source.count(pending_authority_move) != 1:
            raise ReleaseError("self-test cannot uniquely locate pending image authority move")
        image_element_filename.write_text(
            image_element_source.replace(pending_authority_move, "None,", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("pending-to-current image authority move")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        stale_same_id_guard = "!element.owns_controlled_cache_id(response_id)"
        if image_element_source.count(stale_same_id_guard) != 1:
            raise ReleaseError("self-test cannot uniquely locate stale same-ID image guard")
        image_element_filename.write_text(
            image_element_source.replace(stale_same_id_guard, "true", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("same-ID image ABA protection")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        cached_vector_join = ".prepare_cached_vector_identity(&image, provenance)"
        if image_element_source.count(cached_vector_join) != 2:
            raise ReleaseError("self-test cannot locate both cached-vector identity joins")
        image_element_filename.write_text(
            image_element_source.replace(
                cached_vector_join,
                ".prepare_cached_vector_identity(&image, ImageRequestProvenance::Baseline)",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled cached-vector identity join")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        baseline_identity_downgrade = "window.downgrade_cached_vector_identity_to_baseline(vector.id);"
        if image_element_source.count(baseline_identity_downgrade) != 1:
            raise ReleaseError("self-test cannot uniquely locate baseline shared-vector identity downgrade")
        image_element_filename.write_text(
            image_element_source.replace(
                baseline_identity_downgrade,
                "let _ = vector.id;",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("baseline shared-vector identity downgrade")

        reset_candidate_authority_sources()
        image_element_filename = candidate_profile_root / CONTROLLED_IMAGE_ELEMENT_SOURCE
        image_element_source = image_element_filename.read_text(encoding="utf-8")
        abort_start = image_element_source.find("fn abort_request(")
        init_start = image_element_source.find("fn init_image_request(", abort_start)
        if abort_start < 0 or init_start < 0:
            raise ReleaseError("self-test cannot locate cached-vector abort release")
        abort_source = image_element_source[abort_start:init_start]
        abort_release = "self.release_cached_vector_identity_if_unowned(id);"
        if abort_source.count(abort_release) != 1:
            raise ReleaseError("self-test cannot uniquely locate cached-vector abort release")
        image_element_filename.write_text(
            image_element_source[:abort_start]
            + abort_source.replace(abort_release, "", 1)
            + image_element_source[init_start:],
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled cached-vector abort release")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        identity_teardown = "self.controlled_image_identities.borrow_mut().0.clear();"
        if image_window_source.count(identity_teardown) != 1:
            raise ReleaseError("self-test cannot uniquely locate image identity teardown")
        image_window_filename.write_text(
            image_window_source.replace(identity_teardown, "", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled image identity teardown")

        reset_candidate_authority_sources()
        inline_svg_filename = candidate_profile_root / CONTROLLED_INLINE_SVG_SOURCE
        inline_svg_source = inline_svg_filename.read_text(encoding="utf-8")
        inline_internal_request_gate = "is_internal_request == InternalRequest::Yes"
        if inline_svg_source.count(inline_internal_request_gate) != 1:
            raise ReleaseError("self-test cannot uniquely locate inline SVG internal-request gate")
        inline_svg_filename.write_text(
            inline_svg_source.replace(inline_internal_request_gate, "true", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("inline SVG internal-request gate")

        reset_candidate_authority_sources()
        inline_svg_filename = candidate_profile_root / CONTROLLED_INLINE_SVG_SOURCE
        inline_svg_source = inline_svg_filename.read_text(encoding="utf-8")
        inline_exact_cached_url = ".is_some_and(|cached| cached == *candidate)"
        if inline_svg_source.count(inline_exact_cached_url) != 1:
            raise ReleaseError("self-test cannot uniquely locate inline SVG exact cached-URL gate")
        inline_svg_filename.write_text(
            inline_svg_source.replace(
                inline_exact_cached_url,
                ".is_some_and(|_| true)",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("inline SVG exact cached-URL identity")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        inline_controlled_branch = "if controlled_inline_svg {\n"
        if image_window_source.count(inline_controlled_branch) != 1:
            raise ReleaseError("self-test cannot uniquely locate controlled inline SVG decode branch")
        image_window_filename.write_text(
            image_window_source.replace(
                inline_controlled_branch,
                inline_controlled_branch + "                fetch_image_for_layout(\n",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("inline SVG fetch before exact owner/listener admission")

        reset_candidate_authority_sources()
        image_window_filename = candidate_profile_root / CONTROLLED_IMAGE_WINDOW_SOURCE
        image_window_source = image_window_filename.read_text(encoding="utf-8")
        inline_raster_id_gate = "Some(Image::Vector(vector)) if vector.id == id"
        if image_window_source.count(inline_raster_id_gate) != 1:
            raise ReleaseError("self-test cannot uniquely locate inline SVG raster cache-ID gate")
        image_window_filename.write_text(
            image_window_source.replace(
                inline_raster_id_gate,
                "Some(Image::Vector(_))",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("inline SVG exact raster cache-ID join")

        reset_candidate_authority_sources()
        inline_fixture_filename = candidate_profile_root / CONTROLLED_INLINE_SVG_FIXTURE
        inline_fixture_source = inline_fixture_filename.read_text(encoding="utf-8")
        inline_event_observer = "svg.addEventListener(type"
        if inline_fixture_source.count(inline_event_observer) != 1:
            raise ReleaseError("self-test cannot uniquely locate inline SVG no-event observer")
        inline_fixture_filename.write_text(
            inline_fixture_source.replace(inline_event_observer, "void(type", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("inline SVG zero-event native proof")

        reset_candidate_authority_sources()
        inline_protocol_filename = candidate_profile_root / MESSAGE_CHANNEL_BASELINE_TEST_SOURCE
        inline_protocol_source = inline_protocol_filename.read_text(encoding="utf-8")
        strict_direct_v1_expectation = (
            '"direct-v1",\n'
            "        CONTROLLED_V2_IMAGE_DATA_SVG_FIXTURE,\n"
            "        ControlledImageProfileExpectation::Unsupported,"
        )
        if inline_protocol_source.count(strict_direct_v1_expectation) != 1:
            raise ReleaseError("self-test cannot uniquely locate strict direct-image v1 expectation")
        inline_protocol_filename.write_text(
            inline_protocol_source.replace(
                strict_direct_v1_expectation,
                '"direct-v1",\n'
                "        CONTROLLED_V2_IMAGE_DATA_SVG_FIXTURE,\n"
                "        ControlledImageProfileExpectation::PredecessorMayQuiesce(\n"
                '            "load:0>loadend:0|now:0",\n'
                "        ),",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("direct image v1 quiescence promotion")

        reset_candidate_authority_sources()
        inline_protocol_filename = candidate_profile_root / MESSAGE_CHANNEL_BASELINE_TEST_SOURCE
        inline_protocol_source = inline_protocol_filename.read_text(encoding="utf-8")
        dual_inline_v1_expectation = (
            '"inline-svg-v1",\n'
            "        CONTROLLED_V2_INLINE_SVG_FIXTURE,\n"
            "        ControlledImageProfileExpectation::PredecessorMayQuiesce(\n"
            '            "inline-svg:4x3|events:0|now:0",\n'
            "        ),"
        )
        if inline_protocol_source.count(dual_inline_v1_expectation) != 1:
            raise ReleaseError("self-test cannot uniquely locate dual inline-SVG v1 expectation")
        inline_protocol_filename.write_text(
            inline_protocol_source.replace(
                dual_inline_v1_expectation,
                '"inline-svg-v1",\n'
                "        CONTROLLED_V2_INLINE_SVG_FIXTURE,\n"
                "        ControlledImageProfileExpectation::Unsupported,",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("inline SVG v1 transient-retirement false-red")

        reset_candidate_authority_sources()
        inline_protocol_filename = candidate_profile_root / MESSAGE_CHANNEL_BASELINE_TEST_SOURCE
        inline_protocol_source = inline_protocol_filename.read_text(encoding="utf-8")
        advanced_inline_no_event_trace = '"inline-svg:5|load-events:0"'
        if inline_protocol_source.count(advanced_inline_no_event_trace) != 1:
            raise ReleaseError("self-test cannot uniquely locate advanced inline SVG no-event trace")
        inline_protocol_filename.write_text(
            inline_protocol_source.replace(
                advanced_inline_no_event_trace,
                '"inline-svg:5|load-events:1"',
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("advanced inline SVG controlled-clock raster completion proof")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        capacity_start = message_port_source.find("fn controlled_local_channel_capacity_admitted(")
        capacity_end = message_port_source.find("fn next_controlled_local_retained_message_count(", capacity_start)
        if capacity_start < 0 or capacity_end < 0:
            raise ReleaseError("self-test cannot locate retained-port entry admission")
        capacity_source = message_port_source[capacity_start:capacity_end]
        pair_entry_cost = ".checked_add(2)"
        if capacity_source.count(pair_entry_cost) != 1:
            raise ReleaseError("self-test cannot uniquely locate two-entry pair cost")
        message_port_source_filename.write_text(
            message_port_source[:capacity_start]
            + capacity_source.replace(pair_entry_cost, ".checked_add(1)", 1)
            + message_port_source[capacity_end:],
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("two-entry MessageChannel pair admission")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        message_port_source_filename.write_text(
            message_port_source.replace(
                "Self::ControlledLocalOnly => None",
                "Self::ControlledLocalOnly => Some(MessagePortRouterId::new())",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled-local router source")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        reciprocal_projection = "(id == pair_identity).then_some(pair_identity)"
        if message_port_source.count(reciprocal_projection) != 1:
            raise ReleaseError("self-test cannot uniquely locate reciprocal MessagePort pair projection")
        mutated_message_port_source = message_port_source.replace(
            reciprocal_projection,
            "Some(id)",
            1,
        )
        message_port_source_filename.write_text(
            mutated_message_port_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("reciprocal MessagePort pair source coalescing")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        minimum_pair_identity = "let pair_identity = std::cmp::min(id, peer_id);"
        if message_port_source.count(minimum_pair_identity) != 1:
            raise ReleaseError("self-test cannot uniquely locate minimum MessagePort pair identity")
        mutated_message_port_source = message_port_source.replace(
            minimum_pair_identity,
            "let pair_identity = std::cmp::max(id, peer_id);",
            1,
        )
        message_port_source_filename.write_text(
            mutated_message_port_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("minimum reciprocal MessagePort pair source identity")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        queued_map_field = "controlled_local_queued_message_counts: RefCell<FxHashMap<MessagePortId, usize>>"
        if message_port_source.count(queued_map_field) != 1:
            raise ReleaseError("self-test cannot uniquely locate queued MessagePort map")
        message_port_source_filename.write_text(
            message_port_source.replace(
                queued_map_field,
                "controlled_local_queued_message_counts: Cell<usize>",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("per-destination queued MessagePort map")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        reconciliation_equality = "== Some(retained_messages)"
        if message_port_source.count(reconciliation_equality) != 1:
            raise ReleaseError("self-test cannot uniquely locate exact MessagePort reconciliation")
        message_port_source_filename.write_text(
            message_port_source.replace(
                reconciliation_equality,
                "<= Some(retained_messages)",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("inexact native MessagePort reconciliation")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        zero_association_guard = ".any(|(id, count)| {\n                *count == 0"
        if message_port_source.count(zero_association_guard) != 1:
            raise ReleaseError("self-test cannot uniquely locate zero MessagePort association rejection")
        message_port_source_filename.write_text(
            message_port_source.replace(
                zero_association_guard,
                (".any(|(id, count)| {\n                *count == usize::MAX"),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("zero MessagePort association rejection")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        queued_reconciliation_input = "queued_controlled_local_messages.values().copied()"
        if message_port_source.count(queued_reconciliation_input) != 1:
            raise ReleaseError("self-test cannot uniquely locate queued MessagePort reconciliation input")
        message_port_source_filename.write_text(
            message_port_source.replace(
                queued_reconciliation_input,
                "std::iter::empty()",
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("missing queued MessagePort reconciliation")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        mutated_message_port_source = message_port_source.replace(
            ".handle_controlled_local_incoming(task)",
            ".handle_incoming(task)",
            1,
        )
        if mutated_message_port_source == message_port_source:
            raise ReleaseError("self-test cannot bypass controlled-local FIFO admission")
        message_port_source_filename.write_text(
            mutated_message_port_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("controlled-local pre-start FIFO admission")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        queued_association = "if !self.associate_controlled_local_queued_message(entangled_id)"
        if message_port_source.count(queued_association) != 1:
            raise ReleaseError("self-test cannot uniquely locate pre-queue MessagePort association")
        message_port_source_filename.write_text(
            message_port_source.replace(queued_association, "if false", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("MessagePort destination association before queue")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        first_terminal_finish = "self.finish_controlled_local_queued_message(port_id);"
        if message_port_source.count(first_terminal_finish) < 3:
            raise ReleaseError("self-test cannot locate controlled-local terminal accounting transitions")
        message_port_source_filename.write_text(
            message_port_source.replace(first_terminal_finish, "false;", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("MessagePort terminal accounting transition")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        move_transition = (
            "if retains_controlled_reservation {\n"
            "                self.move_controlled_local_queued_message_to_buffer(port_id)"
        )
        if message_port_source.count(move_transition) != 1:
            raise ReleaseError("self-test cannot uniquely locate MessagePort move-to-buffer transition")
        message_port_source_filename.write_text(
            message_port_source.replace(
                move_transition,
                (
                    "if retains_controlled_reservation {\n"
                    "                self.finish_controlled_local_queued_message(port_id)"
                ),
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("MessagePort move-to-buffer accounting transition")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        mutated_message_port_source = message_port_source.replace(
            "retained_controlled_local_messages > 0",
            "retained_controlled_local_messages == 0",
            1,
        )
        if mutated_message_port_source == message_port_source:
            raise ReleaseError("self-test cannot mutate closed-port tombstone retention")
        message_port_source_filename.write_text(
            mutated_message_port_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("closed-port tombstone retention")

        reset_candidate_authority_sources()
        message_port_source_filename = candidate_profile_root / MESSAGE_CHANNEL_LIMITS_SOURCE
        message_port_source = message_port_source_filename.read_text(encoding="utf-8")
        unmanaged_empty_guard = "queued_controlled_local_messages.is_empty()"
        if message_port_source.count(unmanaged_empty_guard) != 1:
            raise ReleaseError("self-test cannot uniquely locate UnManaged queued-work rejection")
        mutated_message_port_source = message_port_source.replace(
            unmanaged_empty_guard,
            "true",
            1,
        )
        if mutated_message_port_source == message_port_source:
            raise ReleaseError("self-test cannot mutate UnManaged retained-work rejection")
        message_port_source_filename.write_text(
            mutated_message_port_source,
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("UnManaged retained-work rejection")

        reset_candidate_authority_sources()
        multi_pair_test_filename = candidate_profile_root / MESSAGE_CHANNEL_BASELINE_TEST_SOURCE
        multi_pair_test_source = multi_pair_test_filename.read_text(encoding="utf-8")
        two_owner_assertion = "message_port_sources, 2,"
        if multi_pair_test_source.count(two_owner_assertion) != 1:
            raise ReleaseError("self-test cannot uniquely locate the two-owner MessagePort assertion")
        multi_pair_test_filename.write_text(
            multi_pair_test_source.replace(two_owner_assertion, "message_port_sources, 1,", 1),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("multi-pair two-owner native proof")

        reset_candidate_authority_sources()
        multi_pair_fixture_filename = candidate_profile_root / MESSAGE_CHANNEL_MULTI_PAIR_FIXTURE
        multi_pair_fixture_source = multi_pair_fixture_filename.read_text(encoding="utf-8")
        buffered_listener = 'buffered.port1.addEventListener("message"'
        if multi_pair_fixture_source.count(buffered_listener) != 1:
            raise ReleaseError("self-test cannot uniquely locate the disabled-port multi-pair fixture")
        multi_pair_fixture_filename.write_text(
            multi_pair_fixture_source.replace(
                buffered_listener,
                'buffered.port1.onmessage = (event) => { /* "message" */',
                1,
            ),
            encoding="utf-8",
        )
        require_candidate_mutation_rejected("multi-pair native-buffer fixture")

        reset_candidate_authority_sources()
        verify_candidate_v2_profile(candidate_profile_root)
        frozen_profile_root = root / "frozen-profile-source"
        frozen_v1_profile = frozen_profile_root / FROZEN_V1_PROFILE
        frozen_v1_profile.parent.mkdir(parents=True)
        shutil.copyfile(repository_root / FROZEN_V1_PROFILE, frozen_v1_profile)
        verify_frozen_v1_profile(frozen_profile_root)
        frozen_v1_profile.write_bytes(frozen_v1_profile.read_bytes() + b"\n")
        try:
            verify_frozen_v1_profile(frozen_profile_root)
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test accepted a changed frozen v1 profile")
        frozen_v2_profile = frozen_profile_root / FROZEN_V2_PROFILE
        frozen_v2_profile.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(repository_root / FROZEN_V2_PROFILE, frozen_v2_profile)
        verify_frozen_v2_profile(frozen_profile_root)
        frozen_v2_profile.write_bytes(frozen_v2_profile.read_bytes() + b"\n")
        try:
            verify_frozen_v2_profile(frozen_profile_root)
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test accepted a changed frozen v2 profile")
        version = RELEASE_VERSION
        revision = "1" * 40
        repository = "https://github.com/oxhq/stasis"
        source_root = root / "source"
        source_root.mkdir()
        for source_name in SOURCE_ASSETS.values():
            filename = source_root / source_name
            filename.parent.mkdir(parents=True, exist_ok=True)
            filename.write_text(f"fixture for {source_name}\n", encoding="utf-8")
        for source_name in candidate_authority_sources:
            destination = source_root / source_name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(repository_root / source_name, destination)
        (source_root / SOURCE_ASSETS["STASIS_UPSTREAM.toml"]).write_text(
            "".join(f"{key} = {json.dumps(value, allow_nan=False)}\n" for key, value in UPSTREAM_IDENTITIES.items()),
            encoding="utf-8",
        )
        binary = root / BINARY_NAME
        binary.write_bytes(b"#!/bin/sh\nexit 0\n")
        binary.chmod(0o755)

        release_cases: dict[
            str,
            tuple[Path, dict[str, str], Path, Path, dict[str, object], str],
        ] = {}
        for index, platform in enumerate(sorted(PLATFORM_CONTRACTS), start=1):
            dist = root / f"dist-{platform}"
            result = create_release(
                binary=binary,
                dist=dist,
                version=version,
                platform=platform,
                revision=revision,
                repository=repository,
                source_root=source_root,
                _self_test_allow_unrepresentable_windows_mode=True,
            )
            extracted = root / f"extracted-{platform}"
            verified = verify_release(
                asset_directory=dist,
                version=version,
                platform=platform,
                revision=revision,
                repository=repository,
                source_root=source_root,
                extract_to=extracted,
            )
            if result["archiveSha256"] != verified["archiveSha256"]:
                raise ReleaseError(f"self-test {platform} archive digest changed during verification")

            names = release_asset_names(version, platform)
            archive = dist / names["archive"]
            bundle = bundle_name(version, platform)
            extracted_bundle = extracted / bundle
            actual_files = {entry.name for entry in extracted_bundle.iterdir()}
            if actual_files != EXPECTED_FILES:
                raise ReleaseError(f"self-test {platform} extracted inventory changed")
            generated = expected_generated_assets(version, platform, revision, repository)
            for name, expected in generated.items():
                if (extracted_bundle / name).read_bytes() != expected:
                    raise ReleaseError(f"self-test {platform} metadata changed: {name}")
            contract = platform_contract(platform)
            if contract["display_name"] not in generated["README.md"].decode("utf-8"):
                raise ReleaseError(f"self-test {platform} README lost its platform identity")
            if contract["dependency_note"] not in generated["NATIVE-LIBRARIES.txt"].decode("utf-8"):
                raise ReleaseError(f"self-test {platform} native dependency metadata changed")
            if contract["install_note"] not in generated["INSTALL.txt"].decode("utf-8"):
                raise ReleaseError(f"self-test {platform} install metadata changed")

            archive_digest = sha256_file(archive)
            binary_digest = parse_sidecar(
                dist / names["binary_sha256"],
                f"{bundle}/{BINARY_NAME}",
            )
            gate_record: dict[str, object] = {
                "schema": GATE_PROOF_SCHEMA,
                "gate": GATE_NAME,
                "test": GATE_TEST,
                "version": version,
                "archive": {"name": names["archive"], "sha256": archive_digest},
                "binary": {"path": "/tmp/stasis", "sha256": binary_digest},
                "source": expected_source_identities(revision),
            }
            log = root / f"gate-{platform}.log"
            log.write_text(
                f"{GATE_RECORD_PREFIX}"
                f"{json.dumps(gate_record, allow_nan=False, separators=(',', ':'), sort_keys=True)}\n"
                f"test {GATE_TEST} ... ok\n{GATE_SUCCESS} 0 measured; 0 filtered out\n",
                encoding="utf-8",
            )
            proof_dir = root / f"proof-{platform}"
            proof_dir.mkdir()
            proof = proof_dir / names["gate_proof"]
            run_id = str(122 + index)
            created_proof = create_gate_proof(
                proof=proof,
                gate_log=log,
                asset_directory=dist,
                version=version,
                platform=platform,
                revision=revision,
                run_id=run_id,
                run_attempt="1",
            )
            if created_proof["schema"] != GATE_PROOF_SCHEMA or created_proof["platform"] != platform:
                raise ReleaseError(f"self-test {platform} gate proof identity changed")
            verify_gate_proof(
                proof_directory=proof_dir,
                asset_directory=dist,
                version=version,
                platform=platform,
                revision=revision,
                run_id=run_id,
                run_attempt="1",
            )
            release_cases[platform] = (
                dist,
                names,
                archive,
                extracted_bundle,
                gate_record,
                binary_digest,
            )

        mac_generated = expected_generated_assets(version, "macos-aarch64", revision, repository)
        linux_generated = expected_generated_assets(version, "linux-x86_64", revision, repository)
        for name in ("INSTALL.txt", "NATIVE-LIBRARIES.txt", "README.md", "VERSION.txt"):
            if mac_generated[name] == linux_generated[name]:
                raise ReleaseError(f"self-test generated platform-neutral {name}")
        if b"glibc 2.35" not in linux_generated["NATIVE-LIBRARIES.txt"]:
            raise ReleaseError("self-test Linux metadata lost the glibc compatibility floor")
        if b"unsigned and is not Apple-notarized" not in mac_generated["INSTALL.txt"]:
            raise ReleaseError("self-test macOS metadata lost its signing boundary")

        linux_bundle = release_cases["linux-x86_64"][3]
        try:
            validate_bundle_directory(
                linux_bundle,
                version=version,
                platform="macos-aarch64",
                revision=revision,
                repository=repository,
                source_root=source_root,
                _self_test_allow_unrepresentable_windows_mode=True,
            )
        except ReleaseError as error:
            if "generated release asset differs" not in str(error):
                raise
        else:
            raise ReleaseError("self-test accepted Linux metadata as macOS metadata")

        for invalid_version in (
            "0.1.0-alpha.0",
            "0.2.0-alpha.0",
            "0.2.0",
            "0.2.1",
            "0.3.1",
            "v0.3.0",
        ):
            try:
                validate_identity(invalid_version, "macos-aarch64", revision, repository)
            except ReleaseError:
                pass
            else:
                raise ReleaseError(f"self-test accepted non-release version {invalid_version!r}")
        for invalid_platform in ("linux-aarch64", "macos-x86_64", "windows-x86_64"):
            try:
                validate_identity(version, invalid_platform, revision, repository)
            except ReleaseError:
                pass
            else:
                raise ReleaseError(f"self-test accepted unsupported platform {invalid_platform!r}")

        for invalid_json in (
            '{"outer":{"member":1,"member":2}}',
            '{"number":NaN}',
            '{"number":Infinity}',
            '{"number":-Infinity}',
        ):
            try:
                strict_json_loads(invalid_json, "self-test document")
            except ReleaseError:
                pass
            else:
                raise ReleaseError("self-test accepted duplicate-member or non-finite JSON")

        dist, names, archive, _, gate_record, binary_digest = release_cases["macos-aarch64"]
        archive_digest = sha256_file(archive)

        unrelated_record = dict(gate_record)
        unrelated_record["archive"] = {"name": names["archive"], "sha256": "2" * 64}
        unrelated_log = root / "unrelated-archive-gate.log"
        unrelated_log.write_text(
            f"{GATE_RECORD_PREFIX}"
            f"{json.dumps(unrelated_record, allow_nan=False, separators=(',', ':'), sort_keys=True)}\n"
            f"test {GATE_TEST} ... ok\n{GATE_SUCCESS} 0 measured; 0 filtered out\n",
            encoding="utf-8",
        )
        try:
            create_gate_proof(
                proof=root / "unrelated-archive-proof.json",
                gate_log=unrelated_log,
                asset_directory=dist,
                version=version,
                platform="macos-aarch64",
                revision=revision,
                run_id="123",
                run_attempt="1",
            )
        except ReleaseError as error:
            if "archive" not in str(error):
                raise
        else:
            raise ReleaseError("self-test attached an archive absent from the native gate record")

        overfull_tar = root / "overfull.tar"
        bundle = bundle_name(version, "macos-aarch64")
        with tarfile.open(overfull_tar, mode="w", format=tarfile.USTAR_FORMAT) as package:
            package.addfile(normalized_tar_info(bundle, directory=True))
            for name in sorted(EXPECTED_FILES):
                package.addfile(
                    normalized_tar_info(
                        f"{bundle}/{name}",
                        directory=False,
                        executable=name == BINARY_NAME,
                    )
                )
            package.addfile(normalized_tar_info(f"{bundle}/EXTRA", directory=False))
        with tarfile.open(overfull_tar, mode="r:") as package:
            try:
                verify_tar_metadata(package, bundle)
            except ReleaseError as error:
                expected_member_count = len(EXPECTED_FILES) + 1
                if f"exact {expected_member_count}-member limit" not in str(error):
                    raise
            else:
                raise ReleaseError("self-test accepted an archive beyond its member limit")

        empty_member_tar = root / "empty-member.tar"
        with tarfile.open(empty_member_tar, mode="w", format=tarfile.USTAR_FORMAT) as package:
            package.addfile(normalized_tar_info(bundle, directory=True))
            for name in sorted(EXPECTED_FILES):
                payload = b"" if name == "README.md" else b"x"
                member = normalized_tar_info(
                    f"{bundle}/{name}",
                    directory=False,
                    executable=name == BINARY_NAME,
                )
                member.size = len(payload)
                package.addfile(member, io.BytesIO(payload))
        with tarfile.open(empty_member_tar, mode="r:") as package:
            try:
                verify_tar_metadata(package, bundle)
            except ReleaseError as error:
                if "invalid size 0" not in str(error):
                    raise
            else:
                raise ReleaseError("self-test accepted an empty required archive member")

        sidecar = dist / names["archive_sha256"]
        try:
            decompress_canonical_gzip(
                archive,
                root / "deliberately-too-small.tar",
                max_output_bytes=16,
            )
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test decompressed an archive beyond its configured limit")
        canonical_archive = archive.read_bytes()
        archive.write_bytes(canonical_archive + b"UNVERIFIED-TRAILING-PAYLOAD")
        write_sidecar(sidecar, sha256_file(archive), names["archive"])
        try:
            verify_release(
                asset_directory=dist,
                version=version,
                platform="macos-aarch64",
                revision=revision,
                repository=repository,
                source_root=source_root,
                extract_to=None,
            )
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test accepted a validly checksummed archive with trailing bytes")
        archive.write_bytes(canonical_archive)
        write_sidecar(sidecar, sha256_file(archive), names["archive"])
        sidecar.write_text(sidecar.read_text(encoding="ascii").upper(), encoding="ascii")
        try:
            verify_release(
                asset_directory=dist,
                version=version,
                platform="macos-aarch64",
                revision=revision,
                repository=repository,
                source_root=source_root,
                extract_to=None,
            )
        except ReleaseError:
            pass
        else:
            raise ReleaseError("self-test accepted a noncanonical checksum")
    print("stasis release archive self-test: ok")


def parser() -> argparse.ArgumentParser:
    argument_parser = argparse.ArgumentParser(description=__doc__)
    subparsers = argument_parser.add_subparsers(dest="command", required=True)

    create = subparsers.add_parser("create")
    create.add_argument("--binary", type=Path, required=True)
    create.add_argument("--dist", type=Path, required=True)
    create.add_argument("--version", required=True)
    create.add_argument("--platform", required=True)
    create.add_argument("--revision", required=True)
    create.add_argument("--repository", required=True)
    create.add_argument("--source-root", type=Path, required=True)

    verify = subparsers.add_parser("verify")
    verify.add_argument("--asset-directory", type=Path, required=True)
    verify.add_argument("--version", required=True)
    verify.add_argument("--platform", required=True)
    verify.add_argument("--revision", required=True)
    verify.add_argument("--repository", required=True)
    verify.add_argument("--source-root", type=Path, required=True)
    verify.add_argument("--extract-to", type=Path)

    gate_proof = subparsers.add_parser("gate-proof")
    gate_proof.add_argument("--proof", type=Path, required=True)
    gate_proof.add_argument("--gate-log", type=Path, required=True)
    gate_proof.add_argument("--asset-directory", type=Path, required=True)
    gate_proof.add_argument("--version", required=True)
    gate_proof.add_argument("--platform", required=True)
    gate_proof.add_argument("--revision", required=True)
    gate_proof.add_argument("--run-id", required=True)
    gate_proof.add_argument("--run-attempt", required=True)

    verify_proof = subparsers.add_parser("verify-gate-proof")
    verify_proof.add_argument("--proof-directory", type=Path, required=True)
    verify_proof.add_argument("--asset-directory", type=Path, required=True)
    verify_proof.add_argument("--version", required=True)
    verify_proof.add_argument("--platform", required=True)
    verify_proof.add_argument("--revision", required=True)
    verify_proof.add_argument("--run-id", required=True)
    verify_proof.add_argument("--run-attempt", required=True)

    verify_frozen_profile = subparsers.add_parser("verify-frozen-v1-profile")
    verify_frozen_profile.add_argument("--source-root", type=Path, required=True)

    verify_frozen_v2 = subparsers.add_parser("verify-frozen-v2-profile")
    verify_frozen_v2.add_argument("--source-root", type=Path, required=True)

    verify_candidate_v2 = subparsers.add_parser("verify-candidate-v2-profile")
    verify_candidate_v2.add_argument("--source-root", type=Path, required=True)

    subparsers.add_parser("self-test")
    return argument_parser


def main() -> None:
    arguments = parser().parse_args()
    if arguments.command == "create":
        result = create_release(
            binary=arguments.binary,
            dist=arguments.dist,
            version=arguments.version,
            platform=arguments.platform,
            revision=arguments.revision,
            repository=arguments.repository,
            source_root=arguments.source_root,
        )
    elif arguments.command == "verify":
        result = verify_release(
            asset_directory=arguments.asset_directory,
            version=arguments.version,
            platform=arguments.platform,
            revision=arguments.revision,
            repository=arguments.repository,
            source_root=arguments.source_root,
            extract_to=arguments.extract_to,
        )
    elif arguments.command == "gate-proof":
        result = create_gate_proof(
            proof=arguments.proof,
            gate_log=arguments.gate_log,
            asset_directory=arguments.asset_directory,
            version=arguments.version,
            platform=arguments.platform,
            revision=arguments.revision,
            run_id=arguments.run_id,
            run_attempt=arguments.run_attempt,
        )
    elif arguments.command == "verify-gate-proof":
        result = verify_gate_proof(
            proof_directory=arguments.proof_directory,
            asset_directory=arguments.asset_directory,
            version=arguments.version,
            platform=arguments.platform,
            revision=arguments.revision,
            run_id=arguments.run_id,
            run_attempt=arguments.run_attempt,
        )
    elif arguments.command == "verify-frozen-v1-profile":
        result = verify_frozen_v1_profile(arguments.source_root)
    elif arguments.command == "verify-frozen-v2-profile":
        result = verify_frozen_v2_profile(arguments.source_root)
    elif arguments.command == "verify-candidate-v2-profile":
        result = verify_candidate_v2_profile(arguments.source_root)
    else:
        self_test()
        return
    print(json.dumps(result, allow_nan=False, sort_keys=True))


if __name__ == "__main__":
    try:
        main()
    except ReleaseError as error:
        raise SystemExit(f"error: {error}") from error
