#!/usr/bin/env python3
"""Validate a real-KMS custody receipt without treating the template as evidence."""

from __future__ import annotations

import datetime as dt
import json
import pathlib
import re
import sys

KEY_ARN = re.compile(r"^arn:aws:kms:([^:]+):[0-9]{12}:key/.+$")
DIGEST = re.compile(r"^sha256:[a-f0-9]{64}$")


def fail(message: str) -> None:
    raise ValueError(message)


def verify(path: pathlib.Path) -> None:
    receipt = json.loads(path.read_text(encoding="utf-8"))
    if receipt.get("schemaVersion") != 1 or receipt.get("status") != "complete":
        fail("receipt must be schemaVersion 1 and status complete")
    if receipt.get("backend") != "aws-kms":
        fail("receipt backend must be aws-kms")
    try:
        completed = dt.datetime.fromisoformat(receipt["completedAt"].replace("Z", "+00:00"))
    except (KeyError, AttributeError, ValueError) as error:
        fail(f"completedAt must be an ISO-8601 timestamp: {error}")
    if completed.tzinfo is None:
        fail("completedAt must include a timezone")
    if not DIGEST.fullmatch(receipt.get("releaseDigest", "")):
        fail("releaseDigest must be sha256:<64 lowercase hex>")
    if not DIGEST.fullmatch(receipt.get("evidenceBundleSha256", "")):
        fail("evidenceBundleSha256 must be sha256:<64 lowercase hex>")
    if not receipt.get("workflowRunUrl", "").startswith("https://"):
        fail("workflowRunUrl must use HTTPS")
    region = receipt.get("region")
    keys = receipt.get("keyArns")
    if not isinstance(keys, list) or len(keys) < 2 or len(keys) != len(set(keys)):
        fail("keyArns must contain at least two distinct immutable key ARNs")
    for key in keys:
        match = KEY_ARN.fullmatch(key)
        if not match or match.group(1) != region:
            fail("every key ARN must be immutable and match receipt.region")
    events = receipt.get("cloudTrailEventIds")
    if not isinstance(events, list) or len(events) < 4 or any(not event for event in events):
        fail("at least four CloudTrail event ids are required")
    topology = receipt.get("topology", {})
    if topology.get("hosts", 0) < 2:
        fail("the custody drill must use at least two hosts")
    for field in ("objectStore", "kmsEndpoint"):
        if not isinstance(topology.get(field), str) or not topology[field].strip():
            fail(f"topology.{field} is required")
    checks = receipt.get("checks")
    if not isinstance(checks, dict) or not checks or any(value is not True for value in checks.values()):
        fail("every declared live-KMS check must be true")
    required_checks = {
        "dekRoundTrip", "encryptedIngestAndRead", "multiPartRead",
        "expandActivateRewrap", "partBytesUnchanged", "retirementRefusal",
        "restoreThroughPreviousKey", "unreachableFailsClosedAndRetries",
        "deniedFailsClosedAndRetries", "throttledFailsClosedAndRetries",
        "revokedFailsClosedAndRetries", "replacementNodeDrill",
        "zeroAcknowledgedEventsLost", "multiRegionPostureRecorded",
        "separationOfDutiesApproved",
    }
    if set(checks) != required_checks:
        fail("checks must exactly match the EXT-KMS acceptance set")
    approved = receipt.get("approvedBy")
    if (
        not isinstance(approved, list)
        or any(not isinstance(name, str) or not name.strip() for name in approved)
        or len(set(approved)) < 2
    ):
        fail("approvedBy must name at least two distinct approvers")
    print(f"live KMS receipt valid: {len(keys)} keys, {topology['hosts']} hosts, {completed.isoformat()}")


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            fail("usage: verify_live_kms_receipt.py <receipt.json>")
        verify(pathlib.Path(sys.argv[1]))
    except (OSError, json.JSONDecodeError, TypeError, ValueError) as error:
        print(f"live KMS receipt verification failed: {error}", file=sys.stderr)
        raise SystemExit(1)
