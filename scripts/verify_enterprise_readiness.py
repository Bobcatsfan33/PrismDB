#!/usr/bin/env python3
"""Fail closed when the enterprise-readiness evidence index is missing, stale, or misleading."""

from __future__ import annotations

import datetime as dt
import json
import pathlib
import sys
from typing import Any

ROOT = pathlib.Path(__file__).resolve().parents[1]
DEFAULT_MANIFEST = ROOT / "docs" / "enterprise-readiness.json"
REQUIRED_CONTROLS = {
    "GOV-01",
    "SDLC-01",
    "APPSEC-01",
    "SUPPLY-01",
    "IAM-01",
    "CRYPTO-01",
    "DATA-01",
    "RES-01",
    "OBS-01",
    "IR-01",
    "VM-01",
    "AIRISK-01",
}
REQUIRED_FRAMEWORKS = {
    "nist-ssdf-1.1",
    "slsa-1.2",
    "owasp-asvs-5.0.0",
    "csa-ccm-caiq-4.1",
    "nist-ai-rmf-1.0",
    "csa-ai-caiq-1.0.2",
}
CONTROL_STATUSES = {"implemented", "partial", "not-applicable"}
DECISIONS = {"not-approved", "approved"}


def fail(message: str) -> None:
    raise ValueError(message)


def require_object(value: Any, label: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        fail(f"{label} must be an object")
    return value


def require_list(value: Any, label: str) -> list[Any]:
    if not isinstance(value, list):
        fail(f"{label} must be an array")
    return value


def require_text(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        fail(f"{label} must be a non-empty string")
    return value


def evidence_path(value: Any, label: str) -> str:
    relative = pathlib.PurePosixPath(require_text(value, label))
    if relative.is_absolute() or ".." in relative.parts:
        fail(f"{label} must be a repository-relative path without '..'")
    candidate = ROOT.joinpath(*relative.parts)
    if candidate.is_symlink() or not candidate.is_file():
        fail(f"{label} does not name a regular, non-symlink repository file: {relative}")
    return relative.as_posix()


def unique(items: list[str], label: str) -> None:
    if len(items) != len(set(items)):
        fail(f"{label} contains duplicates")


def parse_date(value: Any, label: str) -> dt.date:
    try:
        return dt.date.fromisoformat(require_text(value, label))
    except ValueError as error:
        fail(f"{label} must be an ISO-8601 date: {error}")


def verify(manifest: pathlib.Path = DEFAULT_MANIFEST) -> None:
    document = require_object(json.loads(manifest.read_text(encoding="utf-8")), "manifest")
    if document.get("schemaVersion") != 1:
        fail("schemaVersion must equal 1")
    require_text(document.get("product"), "product")
    require_text(document.get("repository"), "repository")

    assessment = require_object(document.get("assessment"), "assessment")
    as_of = parse_date(assessment.get("asOf"), "assessment.asOf")
    review_by = parse_date(assessment.get("reviewBy"), "assessment.reviewBy")
    if review_by < as_of:
        fail("assessment.reviewBy must not precede assessment.asOf")
    if dt.datetime.now(dt.timezone.utc).date() > review_by:
        fail(f"enterprise readiness evidence expired on {review_by.isoformat()}")
    decision = assessment.get("deploymentDecision")
    if decision not in DECISIONS:
        fail(f"assessment.deploymentDecision must be one of {sorted(DECISIONS)}")
    if not isinstance(assessment.get("softwareReleaseCandidate"), bool):
        fail("assessment.softwareReleaseCandidate must be boolean")
    require_text(assessment.get("decisionReason"), "assessment.decisionReason")

    frameworks = [
        require_object(item, f"frameworks[{index}]")
        for index, item in enumerate(require_list(document.get("frameworks"), "frameworks"))
    ]
    framework_ids = [require_text(item.get("id"), "framework.id") for item in frameworks]
    unique(framework_ids, "framework ids")
    if set(framework_ids) != REQUIRED_FRAMEWORKS:
        fail(f"framework ids must equal {sorted(REQUIRED_FRAMEWORKS)}")
    for item in frameworks:
        require_text(item.get("name"), f"framework {item['id']}.name")
        require_text(item.get("version"), f"framework {item['id']}.version")
        url = require_text(item.get("url"), f"framework {item['id']}.url")
        if not url.startswith("https://"):
            fail(f"framework {item['id']}.url must use HTTPS")

    controls = [
        require_object(item, f"controls[{index}]")
        for index, item in enumerate(require_list(document.get("controls"), "controls"))
    ]
    control_ids = [require_text(item.get("id"), "control.id") for item in controls]
    unique(control_ids, "control ids")
    if set(control_ids) != REQUIRED_CONTROLS:
        fail(f"control ids must equal {sorted(REQUIRED_CONTROLS)}")
    incomplete_controls = 0
    for control in controls:
        control_id = control["id"]
        require_text(control.get("title"), f"control {control_id}.title")
        status = control.get("status")
        if status not in CONTROL_STATUSES:
            fail(f"control {control_id}.status must be one of {sorted(CONTROL_STATUSES)}")
        mapped = [
            require_text(item, f"control {control_id}.frameworks")
            for item in require_list(control.get("frameworks"), f"control {control_id}.frameworks")
        ]
        unique(mapped, f"control {control_id}.frameworks")
        if not mapped or not set(mapped).issubset(REQUIRED_FRAMEWORKS):
            fail(f"control {control_id} must map only to declared frameworks")
        evidence = [
            evidence_path(item, f"control {control_id}.evidence")
            for item in require_list(control.get("evidence"), f"control {control_id}.evidence")
        ]
        unique(evidence, f"control {control_id}.evidence")
        gaps = [
            require_text(item, f"control {control_id}.gaps")
            for item in require_list(control.get("gaps"), f"control {control_id}.gaps")
        ]
        if status in {"implemented", "partial"} and not evidence:
            fail(f"control {control_id} requires evidence")
        if status == "implemented" and gaps:
            fail(f"implemented control {control_id} cannot contain gaps")
        if status == "partial" and not gaps:
            fail(f"partial control {control_id} must name its gaps")
        if status not in {"implemented", "not-applicable"}:
            incomplete_controls += 1

    gates = [
        require_object(item, f"externalGates[{index}]")
        for index, item in enumerate(require_list(document.get("externalGates"), "externalGates"))
    ]
    gate_ids = [require_text(item.get("id"), "external gate id") for item in gates]
    unique(gate_ids, "external gate ids")
    if not gates:
        fail("externalGates must not be empty")
    open_blocking = 0
    for gate in gates:
        gate_id = gate["id"]
        require_text(gate.get("title"), f"gate {gate_id}.title")
        if gate.get("status") not in {"open", "complete"}:
            fail(f"gate {gate_id}.status must be open or complete")
        if not isinstance(gate.get("blocking"), bool):
            fail(f"gate {gate_id}.blocking must be boolean")
        require_text(gate.get("ownerRole"), f"gate {gate_id}.ownerRole")
        require_text(gate.get("acceptanceCriteria"), f"gate {gate_id}.acceptanceCriteria")
        evidence = [
            evidence_path(item, f"gate {gate_id}.evidence")
            for item in require_list(gate.get("evidence"), f"gate {gate_id}.evidence")
        ]
        unique(evidence, f"gate {gate_id}.evidence")
        if gate["status"] == "complete" and not evidence:
            fail(f"complete gate {gate_id} requires retained evidence")
        if gate["status"] == "open" and gate["blocking"]:
            open_blocking += 1

    if decision == "approved" and (open_blocking or incomplete_controls):
        fail("deploymentDecision cannot be approved while controls or blocking gates remain open")
    if open_blocking and decision != "not-approved":
        fail("open blocking gates require deploymentDecision=not-approved")

    print(
        "enterprise readiness manifest valid: "
        f"{len(controls)} controls, {open_blocking} open blocking gates, "
        f"decision={decision}, reviewBy={review_by.isoformat()}"
    )


if __name__ == "__main__":
    try:
        if len(sys.argv) > 2:
            fail("usage: verify_enterprise_readiness.py [manifest.json]")
        selected = pathlib.Path(sys.argv[1]).resolve() if len(sys.argv) == 2 else DEFAULT_MANIFEST
        verify(selected)
    except (OSError, json.JSONDecodeError, ValueError) as error:
        print(f"enterprise readiness verification failed: {error}", file=sys.stderr)
        raise SystemExit(1)
