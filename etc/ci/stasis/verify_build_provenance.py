#!/usr/bin/env python3
"""Verify one exact GitHub build-provenance statement and all of its subjects."""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import pathlib
import re
import tempfile
from typing import Any, NoReturn, Sequence


HEX_REVISION = re.compile(r"[0-9a-f]{40}")
POSITIVE_INTEGER = re.compile(r"[1-9][0-9]*")
PREDICATE_TYPE = "https://slsa.dev/provenance/v1"
STATEMENT_TYPE = "https://in-toto.io/Statement/v1"
BUILD_TYPE = "https://actions.github.io/buildtypes/workflow/v1"


class VerificationError(ValueError):
    """Raised when verified provenance does not match the release identity."""


def reject(message: str) -> NoReturn:
    raise VerificationError(message)


def strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            reject(f"verified provenance contains duplicate object key {key!r}")
        value[key] = item
    return value


def strict_float(value: str) -> float:
    parsed = float(value)
    if not math.isfinite(parsed):
        reject(f"verified provenance contains non-finite number {value!r}")
    return parsed


def strict_constant(value: str) -> NoReturn:
    reject(f"verified provenance contains non-finite number {value!r}")


def load_strict_json(filename: pathlib.Path) -> Any:
    try:
        return json.loads(
            filename.read_text(encoding="utf-8"),
            object_pairs_hook=strict_object,
            parse_float=strict_float,
            parse_constant=strict_constant,
        )
    except (OSError, UnicodeError, ValueError, RecursionError) as error:
        if isinstance(error, VerificationError):
            raise
        raise VerificationError(f"cannot read strict provenance JSON: {error}") from error


def sha256(filename: pathlib.Path) -> str:
    digest = hashlib.sha256()
    try:
        if filename.is_symlink() or not filename.is_file():
            reject(f"provenance subject is not a regular file: {filename}")
        with filename.open("rb") as source:
            for chunk in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(chunk)
    except OSError as error:
        raise VerificationError(f"cannot hash provenance subject {filename}: {error}") from error
    return digest.hexdigest()


def expected_subjects(subject_paths: Sequence[pathlib.Path], expected_count: int) -> dict[str, str]:
    if expected_count < 1 or len(subject_paths) != expected_count:
        reject(
            f"expected exactly {expected_count} provenance subject paths, got {len(subject_paths)}"
        )
    subjects: dict[str, str] = {}
    for filename in subject_paths:
        name = filename.name
        if not name or name in subjects:
            reject(f"provenance subject basename is empty or duplicated: {name!r}")
        subjects[name] = sha256(filename)
    return subjects


def nested(record: Any, *keys: str) -> Any:
    value = record
    for key in keys:
        if not isinstance(value, dict):
            return None
        value = value.get(key)
    return value


def verify_document(
    document: Any,
    *,
    repository: str,
    workflow: str,
    revision: str,
    source_ref: str,
    server_url: str,
    run_id: str,
    run_attempt: str,
    subjects: dict[str, str],
) -> None:
    if re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) is None:
        reject("repository is not an exact owner/name identity")
    if (
        not workflow.startswith(".github/workflows/")
        or workflow.endswith("/")
        or ".." in pathlib.PurePosixPath(workflow).parts
    ):
        reject("signer workflow path is invalid")
    if HEX_REVISION.fullmatch(revision) is None:
        reject("source revision is not a lowercase 40-character commit")
    if not source_ref.startswith("refs/heads/") or source_ref == "refs/heads/":
        reject("source ref is not an exact branch ref")
    if POSITIVE_INTEGER.fullmatch(run_id) is None or POSITIVE_INTEGER.fullmatch(run_attempt) is None:
        reject("provenance run identity is invalid")
    if not isinstance(document, list) or not document:
        reject("gh attestation verification did not return any verified statements")

    normalized_server = server_url.rstrip("/")
    if normalized_server not in {"https://github.com", "https://www.github.com"}:
        reject("unexpected GitHub server URL")
    normalized_server = "https://github.com"
    repository_uri = f"{normalized_server}/{repository}"
    signer_uri = f"{repository_uri}/{workflow}@{source_ref}"
    invocation_uri = (
        f"{repository_uri}/actions/runs/{run_id}/attempts/{run_attempt}"
    )

    candidates: list[dict[str, Any]] = []
    for record in document:
        if not isinstance(record, dict):
            reject("gh attestation verification returned a non-object record")
        certificate_invocation = nested(
            record,
            "verificationResult",
            "signature",
            "certificate",
            "runInvocationURI",
        )
        predicate_invocation = nested(
            record,
            "verificationResult",
            "statement",
            "predicate",
            "runDetails",
            "metadata",
            "invocationId",
        )
        if certificate_invocation == invocation_uri or predicate_invocation == invocation_uri:
            candidates.append(record)
    if len(candidates) != 1:
        reject(
            "expected exactly one verified attestation for "
            f"{invocation_uri}, got {len(candidates)}"
        )

    result = candidates[0].get("verificationResult")
    if not isinstance(result, dict):
        reject("selected verification result is missing")
    certificate = nested(result, "signature", "certificate")
    if not isinstance(certificate, dict):
        reject("selected verified certificate is missing")
    expected_certificate = {
        "subjectAlternativeName": signer_uri,
        "githubWorkflowSHA": revision,
        "githubWorkflowRepository": repository,
        "githubWorkflowRef": source_ref,
        "runnerEnvironment": "github-hosted",
        "sourceRepositoryURI": repository_uri,
        "sourceRepositoryDigest": revision,
        "sourceRepositoryRef": source_ref,
        "buildSignerURI": signer_uri,
        "runInvocationURI": invocation_uri,
    }
    for key, expected in expected_certificate.items():
        if certificate.get(key) != expected:
            reject(f"verified certificate field {key!r} changed")

    statement = result.get("statement")
    if not isinstance(statement, dict) or set(statement) != {
        "_type",
        "subject",
        "predicateType",
        "predicate",
    }:
        reject("verified provenance statement envelope changed")
    if statement.get("_type") != STATEMENT_TYPE or statement.get("predicateType") != PREDICATE_TYPE:
        reject("verified provenance statement type changed")

    statement_subjects = statement.get("subject")
    if not isinstance(statement_subjects, list) or len(statement_subjects) != len(subjects):
        reject("verified provenance subject count changed")
    actual_subjects: dict[str, str] = {}
    for subject in statement_subjects:
        if not isinstance(subject, dict) or set(subject) != {"name", "digest"}:
            reject("verified provenance subject shape changed")
        name = subject.get("name")
        digest = subject.get("digest")
        if (
            not isinstance(name, str)
            or not name
            or name in actual_subjects
            or not isinstance(digest, dict)
            or set(digest) != {"sha256"}
            or not isinstance(digest.get("sha256"), str)
            or re.fullmatch(r"[0-9a-f]{64}", digest["sha256"]) is None
        ):
            reject("verified provenance subject identity is malformed or duplicated")
        actual_subjects[name] = digest["sha256"]
    if actual_subjects != subjects:
        reject("verified provenance subjects or digests do not match the exact release files")

    predicate = statement.get("predicate")
    build_definition = nested(predicate, "buildDefinition")
    run_details = nested(predicate, "runDetails")
    if not isinstance(build_definition, dict) or not isinstance(run_details, dict):
        reject("verified SLSA build definition or run details are missing")
    if build_definition.get("buildType") != BUILD_TYPE:
        reject("verified SLSA build type changed")
    expected_external = {
        "workflow": {
            "path": workflow,
            "ref": source_ref,
            "repository": repository_uri,
        }
    }
    if build_definition.get("externalParameters") != expected_external:
        reject("verified SLSA workflow identity changed")
    expected_dependencies = [
        {
            "uri": f"git+{repository_uri}@{source_ref}",
            "digest": {"gitCommit": revision},
        }
    ]
    if build_definition.get("resolvedDependencies") != expected_dependencies:
        reject("verified SLSA source dependency changed")
    internal_github = nested(build_definition, "internalParameters", "github")
    if (
        not isinstance(internal_github, dict)
        or internal_github.get("event_name") != "push"
        or internal_github.get("runner_environment") != "github-hosted"
    ):
        reject("verified SLSA internal GitHub identity changed")
    if run_details.get("builder") != {"id": signer_uri}:
        reject("verified SLSA builder identity changed")
    if run_details.get("metadata") != {"invocationId": invocation_uri}:
        reject("verified SLSA invocation identity changed")


def verify_command(args: argparse.Namespace) -> None:
    paths = [pathlib.Path(value) for value in args.subject]
    subjects = expected_subjects(paths, args.expected_subject_count)
    document = load_strict_json(pathlib.Path(args.verification_json))
    verify_document(
        document,
        repository=args.repository,
        workflow=args.workflow,
        revision=args.revision,
        source_ref=args.source_ref,
        server_url=args.server_url,
        run_id=args.run_id,
        run_attempt=args.run_attempt,
        subjects=subjects,
    )
    print(
        "verified exact GitHub build provenance for "
        f"{len(subjects)} subjects from run {args.run_id} attempt {args.run_attempt}"
    )


def self_test() -> None:
    repository = "oxhq/stasis"
    workflow = ".github/workflows/stasis-package.yml"
    revision = "a" * 40
    source_ref = "refs/heads/main"
    run_id = "123"
    run_attempt = "4"
    repository_uri = f"https://github.com/{repository}"
    signer_uri = f"{repository_uri}/{workflow}@{source_ref}"
    invocation_uri = f"{repository_uri}/actions/runs/{run_id}/attempts/{run_attempt}"
    with tempfile.TemporaryDirectory() as directory:
        first = pathlib.Path(directory, "first.txt")
        second = pathlib.Path(directory, "second.txt")
        first.write_bytes(b"first\n")
        second.write_bytes(b"second\n")
        subjects = expected_subjects([first, second], 2)
        statement = {
            "_type": STATEMENT_TYPE,
            "subject": [
                {"name": name, "digest": {"sha256": digest}}
                for name, digest in subjects.items()
            ],
            "predicateType": PREDICATE_TYPE,
            "predicate": {
                "buildDefinition": {
                    "buildType": BUILD_TYPE,
                    "externalParameters": {
                        "workflow": {
                            "path": workflow,
                            "ref": source_ref,
                            "repository": repository_uri,
                        }
                    },
                    "internalParameters": {
                        "github": {
                            "event_name": "push",
                            "runner_environment": "github-hosted",
                        }
                    },
                    "resolvedDependencies": [
                        {
                            "uri": f"git+{repository_uri}@{source_ref}",
                            "digest": {"gitCommit": revision},
                        }
                    ],
                },
                "runDetails": {
                    "builder": {"id": signer_uri},
                    "metadata": {"invocationId": invocation_uri},
                },
            },
        }
        document = [
            {
                "attestation": {},
                "verificationResult": {
                    "signature": {
                        "certificate": {
                            "subjectAlternativeName": signer_uri,
                            "githubWorkflowSHA": revision,
                            "githubWorkflowRepository": repository,
                            "githubWorkflowRef": source_ref,
                            "runnerEnvironment": "github-hosted",
                            "sourceRepositoryURI": repository_uri,
                            "sourceRepositoryDigest": revision,
                            "sourceRepositoryRef": source_ref,
                            "buildSignerURI": signer_uri,
                            "runInvocationURI": invocation_uri,
                        }
                    },
                    "statement": statement,
                },
            }
        ]
        verify_document(
            document,
            repository=repository,
            workflow=workflow,
            revision=revision,
            source_ref=source_ref,
            server_url="https://github.com",
            run_id=run_id,
            run_attempt=run_attempt,
            subjects=subjects,
        )
        document[0]["verificationResult"]["signature"]["certificate"][
            "runInvocationURI"
        ] = f"{repository_uri}/actions/runs/{run_id}/attempts/5"
        try:
            verify_document(
                document,
                repository=repository,
                workflow=workflow,
                revision=revision,
                source_ref=source_ref,
                server_url="https://github.com",
                run_id=run_id,
                run_attempt=run_attempt,
                subjects=subjects,
            )
        except VerificationError:
            pass
        else:
            reject("self-test accepted mismatched signed invocation identity")
    print("stasis build provenance self-test: ok")


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser()
    commands = root.add_subparsers(dest="command", required=True)
    verify = commands.add_parser("verify")
    verify.add_argument("--verification-json", required=True)
    verify.add_argument("--repository", required=True)
    verify.add_argument("--workflow", required=True)
    verify.add_argument("--revision", required=True)
    verify.add_argument("--source-ref", required=True)
    verify.add_argument("--server-url", required=True)
    verify.add_argument("--run-id", required=True)
    verify.add_argument("--run-attempt", required=True)
    verify.add_argument("--expected-subject-count", required=True, type=int)
    verify.add_argument("--subject", required=True, action="append")
    commands.add_parser("self-test")
    return root


def main() -> None:
    args = parser().parse_args()
    try:
        if args.command == "verify":
            verify_command(args)
        else:
            self_test()
    except VerificationError as error:
        raise SystemExit(str(error)) from error


if __name__ == "__main__":
    main()
