#!/usr/bin/env python3
"""Validate release-critical DuckDB community extension metadata."""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def read(path: str) -> str:
    return (ROOT / path).read_text(encoding="utf-8")


def first_match(pattern: str, text: str, label: str) -> str:
    match = re.search(pattern, text, re.MULTILINE)
    if match is None:
        raise ValueError(f"could not find {label}")
    return match.group(1)


def duckdb_crate_version(duckdb_version: str) -> str:
    match = re.fullmatch(r"v(\d+)\.(\d+)\.(\d+)", duckdb_version)
    if match is None:
        raise ValueError(f"invalid DuckDB version {duckdb_version!r}")
    _major, minor, patch = (int(part) for part in match.groups())
    return f"1.1{minor:02d}{patch:02d}.0"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--strict-community-ref",
        action="store_true",
        help="fail unless description.yml repo.ref is an immutable tag or commit hash",
    )
    parser.add_argument(
        "--description-path",
        default="description.yml",
        help="descriptor to validate, relative to the repository root",
    )
    args = parser.parse_args()

    failures: list[str] = []
    warnings: list[str] = []

    makefile = read("Makefile")
    workflow = read(".github/workflows/MainDistributionPipeline.yml")
    cargo = read("Cargo.toml")
    pyproject = read("pyproject.toml")
    # The repo's own description.yml, independent of --description-path: the
    # project-version-agreement check below is about the checked-in
    # development-placeholder trio, not whichever descriptor is being
    # validated (which may be a tag-synced render under build/).
    root_description = read("description.yml")
    description = read(args.description_path)

    try:
        make_duckdb = first_match(
            r"^TARGET_DUCKDB_VERSION=(v\d+\.\d+\.\d+)$",
            makefile,
            "Makefile TARGET_DUCKDB_VERSION",
        )
        cargo_duckdb = first_match(
            r'duckdb\s*=\s*\{\s*version\s*=\s*"=([0-9]+\.[0-9]+\.[0-9]+)"',
            cargo,
            "Cargo.toml duckdb crate pin",
        )
    except ValueError as exc:
        failures.append(str(exc))
    else:
        # The Makefile is the single DuckDB pin site. The distribution
        # workflow must NOT carry its own `duckdb_version:` literal: GitHub
        # refuses to let the Actions GITHUB_TOKEN push changes to files under
        # .github/workflows/, so an automated bump PR can never update such a
        # literal — it would silently go stale (and the bump bot's push would
        # fail outright). Instead the pipeline resolves the pin at run time.
        if re.search(r"^\s*duckdb_version:\s*v\d+\.\d+\.\d+\s*$", workflow, re.MULTILINE):
            failures.append(
                "stale pin: MainDistributionPipeline.yml has a literal "
                "`duckdb_version:` — the pipeline must resolve it from the "
                "Makefile instead (GITHUB_TOKEN cannot push workflow-file "
                "changes, so the drift bot can never update a literal here)"
            )
        if "needs.resolve-duckdb-version.outputs.duckdb_version" not in workflow:
            failures.append(
                "MainDistributionPipeline.yml does not resolve duckdb_version "
                "from the Makefile pin (expected "
                "`needs.resolve-duckdb-version.outputs.duckdb_version`)"
            )
        expected_crate = duckdb_crate_version(make_duckdb)
        if cargo_duckdb != expected_crate:
            failures.append(
                "DuckDB crate pin drift: "
                f"Cargo.toml has {cargo_duckdb}, expected {expected_crate} for {make_duckdb}"
            )

    # Cargo.toml / pyproject.toml / description.yml carry a development-
    # placeholder version (see docs/versioning.md) that is only ever rewritten
    # by scripts/sync_release_version.py at release-build time, never
    # committed. This does not check it against any git tag — it only
    # guards that the three checked-in placeholders can't silently drift
    # apart from each other between releases.
    try:
        project_versions = {
            "Cargo.toml": first_match(
                r'^version\s*=\s*"([^"]+)"', cargo, "Cargo.toml package version"
            ),
            "pyproject.toml": first_match(
                r'^version\s*=\s*"([^"]+)"', pyproject, "pyproject.toml project version"
            ),
            "description.yml": first_match(
                r"^\s*version:\s*([^\s]+)\s*$",
                root_description,
                "description.yml extension version",
            ),
        }
    except ValueError as exc:
        failures.append(str(exc))
    else:
        distinct_versions = set(project_versions.values())
        if len(distinct_versions) != 1:
            details = ", ".join(
                f"{path}={version}" for path, version in project_versions.items()
            )
            failures.append(f"project versions differ: {details}")

    descriptor_checks = {
        "extension.name": r"^\s*name:\s*zarr\s*$",
        "extension.language": r"^\s*language:\s*Rust\s*$",
        "extension.build": r"^\s*build:\s*cargo\s*$",
        "extension.requires_toolchains": r'^\s*requires_toolchains:\s*["\']rust;python3["\']\s*$',
        "extension.excluded_platforms": (
            r'^\s*excluded_platforms:\s*["\']'
            r"wasm_mvp;wasm_eh;wasm_threads;linux_amd64_musl"
            r'["\']\s*$'
        ),
    }
    for label, pattern in descriptor_checks.items():
        if re.search(pattern, description, re.MULTILINE) is None:
            failures.append(f"description.yml missing or incorrect {label}")

    ref = first_match(r"^\s*ref:\s*([^\s#]+)", description, "description.yml repo.ref")
    immutable_ref = bool(re.fullmatch(r"[0-9a-f]{40}", ref) or re.fullmatch(r"v\d+\.\d+\.\d+", ref))
    if not immutable_ref:
        message = (
            "description.yml repo.ref should be changed to a release tag or commit hash "
            f"before submitting to duckdb/community-extensions; current value is {ref!r}"
        )
        if args.strict_community_ref:
            failures.append(message)
        else:
            warnings.append(message)

    # ref_next is optional (points at the commit built for the upcoming DuckDB
    # version). When present it must be immutable too — the community rebuild
    # pins it exactly, so a branch name here would silently drift.
    ref_next_match = re.search(r"^\s*ref_next:\s*([^\s#]+)", description, re.MULTILINE)
    if ref_next_match is not None:
        ref_next = ref_next_match.group(1)
        if not (
            re.fullmatch(r"[0-9a-f]{40}", ref_next)
            or re.fullmatch(r"v\d+\.\d+\.\d+", ref_next)
        ):
            failures.append(
                "description.yml repo.ref_next must be a 40-character commit hash "
                f"or a vX.Y.Z tag; current value is {ref_next!r}"
            )

    if failures:
        print("release readiness check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1

    for warning in warnings:
        print(f"warning: {warning}")
    print("release readiness check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
