#!/usr/bin/env python3
"""Fail closed when PrismDB's compatibility inventory drifts from executable code."""

from __future__ import annotations

import hashlib
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
CONTRACT = ROOT / "testing" / "compat" / "contract.json"


def fail(message: str) -> None:
    raise ValueError(message)


def integer(source: pathlib.Path, name: str, kind: str = "u32") -> int:
    text = source.read_text(encoding="utf-8")
    match = re.search(rf"pub const {re.escape(name)}: {kind} = (\d+);", text)
    if not match:
        fail(f"cannot find {name} in {source.relative_to(ROOT)}")
    return int(match.group(1))


def integer_array(source: pathlib.Path, name: str) -> list[int]:
    text = source.read_text(encoding="utf-8")
    match = re.search(
        rf"pub const {re.escape(name)}: &\[u32\] = &\[([^]]+)\];", text
    )
    if not match:
        fail(f"cannot find {name} in {source.relative_to(ROOT)}")
    return [int(value.strip()) for value in match.group(1).split(",")]


def string_constant(source: pathlib.Path, name: str) -> str:
    text = source.read_text(encoding="utf-8")
    match = re.search(rf'pub const {re.escape(name)}: &str = "([^"]+)";', text)
    if not match:
        fail(f"cannot find {name} in {source.relative_to(ROOT)}")
    return match.group(1)


def tree_digest(directory: pathlib.Path) -> tuple[int, str]:
    files = sorted(path for path in directory.rglob("*") if path.is_file())
    digest = hashlib.sha256()
    for path in files:
        relative = path.relative_to(directory).as_posix().encode()
        body = path.read_bytes()
        digest.update(len(relative).to_bytes(4, "big"))
        digest.update(relative)
        digest.update(len(body).to_bytes(8, "big"))
        digest.update(body)
    return len(files), digest.hexdigest()


def verify() -> None:
    document = json.loads(CONTRACT.read_text(encoding="utf-8"))
    if document.get("schemaVersion") != 1:
        fail("schemaVersion must equal 1")

    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    product = re.search(r'^version = "([^"]+)"$', cargo, re.MULTILINE)
    if not product or document.get("productVersion") != product.group(1):
        fail("productVersion does not match Cargo.toml")

    service = ROOT / "crates" / "prism-service" / "src" / "lib.rs"
    rpc = ROOT / "crates" / "prism-engine" / "src" / "shard_rpc.rs"
    store = ROOT / "crates" / "prism-part" / "src" / "store.rs"
    part = ROOT / "crates" / "prism-part" / "src" / "format.rs"
    expected = {
        "publicApiVersion": string_constant(service, "PUBLIC_API_VERSION"),
        "shardRpcProtocolVersion": integer(rpc, "SHARD_RPC_PROTOCOL_VERSION", "u16"),
    }
    for key, value in expected.items():
        if document.get(key) != value:
            fail(f"{key} is {document.get(key)!r}, executable code declares {value!r}")

    if document.get("store") != {
        "writeVersion": integer(store, "STORE_VERSION"),
        "readVersions": integer_array(store, "SUPPORTED_STORE_VERSIONS"),
    }:
        fail("store compatibility inventory does not match executable constants")
    part_versions = [integer(part, "LEGACY_FORMAT_VERSION")]
    part_versions.extend(integer_array(part, "SUPPORTED_BINARY_VERSIONS"))
    if document.get("part") != {
        "writeVersion": integer(part, "FORMAT_VERSION"),
        "readVersions": part_versions,
    }:
        fail("part compatibility inventory does not match executable constants")

    fixture_versions: list[int] = []
    for fixture in document.get("fixtures", []):
        version = fixture.get("version")
        fixture_versions.append(version)
        relative = pathlib.PurePosixPath(fixture.get("path", ""))
        if relative.is_absolute() or ".." in relative.parts:
            fail(f"fixture v{version} has an unsafe path")
        path = ROOT.joinpath(*relative.parts)
        if path.is_symlink() or not path.is_dir():
            fail(f"fixture v{version} path is not a regular directory")
        count, digest = tree_digest(path)
        if count != fixture.get("fileCount") or digest != fixture.get("treeSha256"):
            fail(
                f"fixture v{version} drifted: files={count}, treeSha256={digest}; "
                "released fixtures are immutable"
            )
    if fixture_versions != [1, 2, 3]:
        fail("frozen fixture inventory must contain released formats 1, 2, and 3")

    for raw in document.get("evidence", []):
        relative = pathlib.PurePosixPath(raw)
        path = ROOT.joinpath(*relative.parts)
        if relative.is_absolute() or ".." in relative.parts or path.is_symlink() or not path.is_file():
            fail(f"evidence path is missing or unsafe: {raw}")

    print(
        "compatibility contract valid: "
        f"product={document['productVersion']}, api={document['publicApiVersion']}, "
        f"rpc={document['shardRpcProtocolVersion']}, "
        f"store={document['store']['readVersions']}, part={document['part']['readVersions']}"
    )


if __name__ == "__main__":
    try:
        verify()
    except (OSError, json.JSONDecodeError, TypeError, ValueError) as error:
        print(f"compatibility contract verification failed: {error}", file=sys.stderr)
        raise SystemExit(1)

