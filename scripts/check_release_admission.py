#!/usr/bin/env python3
"""Admit a PrismDB release only when its identity AND its exact digest were approved.

Signed is not authorized. A Sigstore signature proves which workflow built which bytes; it says
nothing about whether those bytes were reviewed for production. A policy that admits "anything this
workflow signed" therefore admits every future release automatically, including one cut from a
compromised branch that the same workflow would sign just as happily.

This check is the deployment-side half of the release-assurance contract in
docs/RELEASE-ASSURANCE.md: a receipt is admitted only when the certificate identity is exactly the
approved workflow at the approved tag AND the image digest appears in the approved-digest list.
Either half missing is a refusal.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
from typing import Any

DIGEST = re.compile(r"^sha256:[a-f0-9]{64}$")
IDENTITY = re.compile(
    r"^https://github\.com/[A-Za-z0-9._-]+/[A-Za-z0-9._-]+"
    r"/\.github/workflows/[A-Za-z0-9._-]+\.yml@refs/tags/[A-Za-z0-9._-]+$"
)
OIDC_ISSUER = "https://token.actions.githubusercontent.com"


class Refused(Exception):
    """The release is not admitted."""


def _text(value: Any, label: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise Refused(f"{label} must be a non-empty string")
    return value


def _load(path: pathlib.Path, label: str) -> Any:
    if path.is_symlink() or not path.is_file():
        raise Refused(f"{label} {path} is not a regular, non-symlink file")
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        raise Refused(f"{label} {path} is not valid JSON: {error}") from error


def admit(receipt: dict[str, Any], approvals: dict[str, Any]) -> str:
    """Return the admitted digest, or raise `Refused` naming the first failing clause."""
    if receipt.get("schema_version") != 1:
        raise Refused("receipt schema_version must equal 1")

    image = _text(receipt.get("image"), "receipt.image")
    digest = _text(receipt.get("image_digest"), "receipt.image_digest")
    identity = _text(receipt.get("certificate_identity"), "receipt.certificate_identity")
    issuer = _text(receipt.get("certificate_oidc_issuer"), "receipt.certificate_oidc_issuer")

    if not DIGEST.match(digest):
        raise Refused(f"receipt.image_digest {digest!r} is not a sha256 digest")
    if not IDENTITY.match(identity):
        raise Refused(f"receipt.certificate_identity {identity!r} is not a workflow-at-tag identity")
    if issuer != OIDC_ISSUER:
        raise Refused(f"receipt.certificate_oidc_issuer must be {OIDC_ISSUER}")

    approved_image = _text(approvals.get("image"), "approvals.image")
    approved_identity = _text(approvals.get("certificate_identity"), "approvals.certificate_identity")
    approved_digests = approvals.get("approved_digests")
    if not isinstance(approved_digests, list) or not approved_digests:
        raise Refused("approvals.approved_digests must be a non-empty array")
    for candidate in approved_digests:
        if not DIGEST.match(_text(candidate, "approvals.approved_digests entry")):
            raise Refused(f"approved digest {candidate!r} is not a sha256 digest")

    if image != approved_image:
        raise Refused(f"image {image!r} is not the approved image {approved_image!r}")
    # Exact string comparison, deliberately: a prefix or repository-level match would admit any
    # workflow in the repository and any tag it was ever run against.
    if identity != approved_identity:
        raise Refused(
            f"certificate identity {identity!r} is not the approved identity {approved_identity!r}"
        )
    if digest not in approved_digests:
        raise Refused(
            f"digest {digest} is signed by an approved workflow but is not an approved digest; "
            "a valid signature is not a production authorization"
        )
    return digest


def _self_test() -> None:
    """Prove the three clauses that matter, including the two that must refuse."""
    identity = (
        "https://github.com/Bobcatsfan33/PrismDB"
        "/.github/workflows/prism-shard-release.yml@refs/tags/prism-shard-v0.1.0"
    )
    good = "sha256:" + "1" * 64
    other = "sha256:" + "2" * 64
    receipt = {
        "schema_version": 1,
        "image": "ghcr.io/bobcatsfan33/prism-shard",
        "image_digest": good,
        "certificate_identity": identity,
        "certificate_oidc_issuer": OIDC_ISSUER,
    }
    approvals = {
        "image": "ghcr.io/bobcatsfan33/prism-shard",
        "certificate_identity": identity,
        "approved_digests": [good],
    }

    assert admit(receipt, approvals) == good, "an approved digest and identity must be admitted"

    signed_but_unapproved = dict(receipt, image_digest=other)
    try:
        admit(signed_but_unapproved, approvals)
    except Refused as error:
        assert "not an approved digest" in str(error), error
    else:  # pragma: no cover - the assertion below is the failure report
        raise AssertionError("a signed but unapproved digest was admitted")

    wrong_workflow = dict(
        receipt,
        certificate_identity=identity.replace("prism-shard-release.yml", "prismd-release.yml"),
    )
    try:
        admit(wrong_workflow, approvals)
    except Refused as error:
        assert "not the approved identity" in str(error), error
    else:  # pragma: no cover
        raise AssertionError("a different workflow's signature was admitted")

    wrong_tag = dict(
        receipt,
        certificate_identity=identity.replace("prism-shard-v0.1.0", "prism-shard-v9.9.9"),
    )
    try:
        admit(wrong_tag, approvals)
    except Refused as error:
        assert "not the approved identity" in str(error), error
    else:  # pragma: no cover
        raise AssertionError("a different tag's signature was admitted")

    print("release admission self-test passed: approved admitted; unapproved digest, wrong "
          "workflow, and wrong tag all refused")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--receipt", type=pathlib.Path, help="release receipt JSON")
    parser.add_argument("--approvals", type=pathlib.Path, help="approved image/identity/digests JSON")
    parser.add_argument("--self-test", action="store_true", help="run the built-in policy gates")
    args = parser.parse_args(argv)

    if args.self_test:
        _self_test()
        return 0
    if not args.receipt or not args.approvals:
        parser.error("--receipt and --approvals are required unless --self-test is given")

    try:
        digest = admit(
            _load(args.receipt, "receipt"),
            _load(args.approvals, "approvals"),
        )
    except Refused as error:
        print(f"release admission refused: {error}", file=sys.stderr)
        return 1
    print(f"release admitted: {digest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
