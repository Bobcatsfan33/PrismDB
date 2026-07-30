"""Bounded Unix-socket identity gateway for a local embedding runtime.

The gateway deliberately does not load model code. It verifies the immutable
artifact bytes mounted into the pod, then proxies bounded batches to a
loopback-only Text Embeddings Inference (TEI) process. The database never
shares an address space or a network listener with the model runtime.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import socket
import struct
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from pathlib import Path, PurePosixPath
from typing import Any

PROTOCOL_VERSION = 1
MAX_CONFIG_BYTES = 1024 * 1024
MAX_REGISTRY_BYTES = 1024 * 1024
MAX_MODELS = 128
MAX_BATCH_ITEMS = 256
MAX_TEXT_BYTES = 32 * 1024
MAX_BATCH_INPUT_BYTES = 4 * 1024 * 1024
MAX_REQUEST_BYTES = 32 * 1024 * 1024
MAX_RESPONSE_BYTES = 64 * 1024 * 1024
MAX_ARTIFACT_FILES = 4096
COPY_CHUNK_BYTES = 1024 * 1024
REVISION_KEYS = ("model_sha256", "tokenizer_sha256", "preprocessing_sha256")


class PrismModelServiceError(RuntimeError):
    """A named, operator-actionable refusal at the model boundary."""


def _read_bounded(path: Path, limit: int, label: str) -> bytes:
    with path.open("rb") as source:
        value = source.read(limit + 1)
    if len(value) > limit:
        raise PrismModelServiceError(f"{label} exceeds {limit} bytes")
    return value


def _json_object(path: Path, limit: int, label: str) -> dict[str, Any]:
    try:
        value = _strict_json(_read_bounded(path, limit, label))
    except (OSError, json.JSONDecodeError, PrismModelServiceError) as error:
        raise PrismModelServiceError(f"cannot load {label} {path}: {error}") from error
    if not isinstance(value, dict):
        raise PrismModelServiceError(f"{label} must be a JSON object")
    return value


def _strict_json(value: bytes) -> Any:
    def object_without_duplicates(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, item in pairs:
            if key in result:
                raise PrismModelServiceError(f"JSON object repeats field {key!r}")
            result[key] = item
        return result

    def reject_constant(value: str) -> None:
        raise PrismModelServiceError(f"JSON contains non-finite number {value}")

    return json.loads(
        value,
        object_pairs_hook=object_without_duplicates,
        parse_constant=reject_constant,
    )


def _only_keys(value: dict[str, Any], allowed: set[str], label: str) -> None:
    unknown = sorted(set(value) - allowed)
    if unknown:
        raise PrismModelServiceError(
            f"{label} has unknown fields: {', '.join(unknown)}"
        )


def _bounded_int(value: Any, low: int, high: int, label: str) -> int:
    if (
        isinstance(value, bool)
        or not isinstance(value, int)
        or not low <= value <= high
    ):
        raise PrismModelServiceError(f"{label} must be an integer in {low}..={high}")
    return value


def _digest(value: Any, label: str) -> str:
    if (
        not isinstance(value, str)
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise PrismModelServiceError(
            f"{label} must be a lowercase 64-character SHA-256 digest"
        )
    return value


def _absolute_path(value: Any, label: str) -> Path:
    if not isinstance(value, str) or not value:
        raise PrismModelServiceError(f"{label} must be a non-empty absolute path")
    path = Path(value)
    if not path.is_absolute():
        raise PrismModelServiceError(f"{label} must be an absolute path")
    return path


def artifact_revision(artifacts: dict[str, str]) -> str:
    """Match ``ModelArtifacts::revision`` byte-for-byte."""

    canonical = (
        f"model={artifacts['model_sha256']}\n"
        f"tokenizer={artifacts['tokenizer_sha256']}\n"
        f"preprocessing={artifacts['preprocessing_sha256']}\n"
    )
    return hashlib.sha256(canonical.encode("ascii")).hexdigest()


def _relative_artifact_path(raw: Any) -> PurePosixPath:
    if not isinstance(raw, str) or not raw or len(raw.encode("utf-8")) > 4096:
        raise PrismModelServiceError("artifact path must contain 1..=4096 UTF-8 bytes")
    value = PurePosixPath(raw)
    if value.is_absolute() or any(part in ("", ".", "..") for part in value.parts):
        raise PrismModelServiceError(
            f"artifact path must be normalized and relative: {raw!r}"
        )
    return value


def artifact_digest(root: Path, raw_paths: list[Any]) -> str:
    """Content-address an explicitly enumerated, ordered artifact file set.

    Paths are sorted and framed with their UTF-8 path length and byte length,
    so neither concatenation ambiguity nor directory traversal can change the
    meaning of a digest.
    """

    if not isinstance(raw_paths, list) or not 1 <= len(raw_paths) <= MAX_ARTIFACT_FILES:
        raise PrismModelServiceError(
            f"artifact set must contain 1..={MAX_ARTIFACT_FILES} files"
        )
    paths = sorted({_relative_artifact_path(raw) for raw in raw_paths}, key=str)
    if len(paths) != len(raw_paths):
        raise PrismModelServiceError("artifact set contains duplicate paths")
    if root.is_symlink():
        raise PrismModelServiceError("model root must not be a symlink")
    try:
        root = root.resolve(strict=True)
    except OSError as error:
        raise PrismModelServiceError(f"model root is unavailable: {root}") from error
    digest = hashlib.sha256()
    for relative in paths:
        candidate = root.joinpath(*relative.parts)
        try:
            if candidate.is_symlink():
                raise PrismModelServiceError(
                    f"artifact must not be a symlink: {relative}"
                )
            resolved = candidate.resolve(strict=True)
            resolved.relative_to(root)
            stat = resolved.stat()
        except (OSError, ValueError) as error:
            raise PrismModelServiceError(
                f"artifact escapes or is absent from model root: {relative}"
            ) from error
        if not resolved.is_file():
            raise PrismModelServiceError(f"artifact is not a regular file: {relative}")
        encoded = str(relative).encode("utf-8")
        digest.update(struct.pack(">Q", len(encoded)))
        digest.update(encoded)
        digest.update(struct.pack(">Q", stat.st_size))
        with resolved.open("rb") as source:
            while chunk := source.read(COPY_CHUNK_BYTES):
                digest.update(chunk)
    return digest.hexdigest()


@dataclass(frozen=True)
class RegisteredModel:
    model_id: str
    model_version: str
    dim: int
    artifacts: dict[str, str]


@dataclass(frozen=True)
class ModelDeployment:
    registered: RegisteredModel
    model_root: Path
    weights: tuple[str, ...]
    tokenizer: tuple[str, ...]
    preprocessing: tuple[str, ...]
    backend_url: str


@dataclass(frozen=True)
class GatewayConfig:
    socket_path: Path
    socket_mode: int
    allowed_peer_uids: frozenset[int]
    request_timeout_seconds: float
    max_concurrency: int
    models: dict[tuple[str, str], ModelDeployment]


def _load_registry(path: Path) -> dict[tuple[str, str], RegisteredModel]:
    raw = _json_object(path, MAX_REGISTRY_BYTES, "model registry")
    _only_keys(
        raw,
        {"default_model_id", "default_model_version", "models"},
        "model registry",
    )
    models = raw.get("models")
    if not isinstance(models, list) or not 1 <= len(models) <= MAX_MODELS:
        raise PrismModelServiceError(
            f"model registry must contain 1..={MAX_MODELS} models"
        )
    result: dict[tuple[str, str], RegisteredModel] = {}
    for index, entry in enumerate(models):
        label = f"model registry models[{index}]"
        if not isinstance(entry, dict):
            raise PrismModelServiceError(f"{label} must be an object")
        _only_keys(entry, {"model_id", "model_version", "dim", "artifacts"}, label)
        model_id = entry.get("model_id")
        version = entry.get("model_version")
        if not isinstance(model_id, str) or not 1 <= len(model_id) <= 256:
            raise PrismModelServiceError(
                f"{label}.model_id must contain 1..=256 characters"
            )
        if not isinstance(version, str):
            raise PrismModelServiceError(f"{label}.model_version must be a string")
        dim = _bounded_int(entry.get("dim"), 1, 65_536, f"{label}.dim")
        artifacts = entry.get("artifacts")
        if not isinstance(artifacts, dict):
            raise PrismModelServiceError(f"{label}.artifacts must be an object")
        _only_keys(artifacts, set(REVISION_KEYS), f"{label}.artifacts")
        checked = {
            key: _digest(artifacts.get(key), f"{label}.{key}") for key in REVISION_KEYS
        }
        expected_version = artifact_revision(checked)
        if version != expected_version:
            raise PrismModelServiceError(
                f"{label} uses version {version!r}; immutable artifacts revise to "
                f"{expected_version!r}"
            )
        key = (model_id, version)
        if key in result:
            raise PrismModelServiceError(
                f"duplicate registered model {model_id}:{version}"
            )
        result[key] = RegisteredModel(model_id, version, dim, checked)
    default = (raw.get("default_model_id"), raw.get("default_model_version"))
    if default not in result:
        raise PrismModelServiceError("model registry default is not a registered model")
    return result


def _loopback_backend(raw: Any, label: str) -> str:
    if not isinstance(raw, str):
        raise PrismModelServiceError(f"{label} must be a URL")
    parsed = urllib.parse.urlsplit(raw)
    if (
        parsed.scheme != "http"
        or parsed.hostname not in {"127.0.0.1", "::1", "localhost"}
        or parsed.port is None
        or parsed.username is not None
        or parsed.password is not None
        or parsed.query
        or parsed.fragment
        or parsed.path not in ("", "/")
    ):
        raise PrismModelServiceError(
            f"{label} must be an explicit loopback-only http://host:port URL"
        )
    return raw.rstrip("/")


def load_gateway_config(config_path: Path) -> GatewayConfig:
    raw = _json_object(config_path, MAX_CONFIG_BYTES, "model-service config")
    _only_keys(
        raw,
        {
            "registry_path",
            "socket_path",
            "socket_mode",
            "allowed_peer_uids",
            "request_timeout_ms",
            "max_concurrency",
            "models",
        },
        "model-service config",
    )
    registry_path = _absolute_path(raw.get("registry_path"), "registry_path")
    socket_path = _absolute_path(raw.get("socket_path"), "socket_path")
    registry = _load_registry(registry_path)
    mode_raw = raw.get("socket_mode", "0660")
    try:
        mode = int(mode_raw, 8)
    except (TypeError, ValueError) as error:
        raise PrismModelServiceError("socket_mode must be an octal string") from error
    if not 0o600 <= mode <= 0o770 or mode & 0o007:
        raise PrismModelServiceError(
            "socket_mode must be 0600..0770 and grant no access to other users"
        )
    uids_raw = raw.get("allowed_peer_uids")
    if not isinstance(uids_raw, list) or not 1 <= len(uids_raw) <= 32:
        raise PrismModelServiceError("allowed_peer_uids must contain 1..=32 UIDs")
    uids = frozenset(
        _bounded_int(value, 1, 2**31 - 1, "allowed peer UID") for value in uids_raw
    )
    timeout_ms = _bounded_int(
        raw.get("request_timeout_ms"), 10, 60_000, "request_timeout_ms"
    )
    concurrency = _bounded_int(raw.get("max_concurrency"), 1, 256, "max_concurrency")
    deployments = raw.get("models")
    if not isinstance(deployments, list) or len(deployments) != len(registry):
        raise PrismModelServiceError(
            "model-service config must deploy every model in the registry exactly once"
        )
    result: dict[tuple[str, str], ModelDeployment] = {}
    for index, entry in enumerate(deployments):
        label = f"model-service models[{index}]"
        if not isinstance(entry, dict):
            raise PrismModelServiceError(f"{label} must be an object")
        _only_keys(
            entry,
            {
                "model_id",
                "model_version",
                "model_root",
                "weights",
                "tokenizer",
                "preprocessing",
                "backend_url",
            },
            label,
        )
        key = (entry.get("model_id"), entry.get("model_version"))
        registered = registry.get(key)
        if registered is None:
            raise PrismModelServiceError(
                f"{label} is not present in the model registry"
            )
        root = _absolute_path(entry.get("model_root"), f"{label}.model_root")
        groups: dict[str, tuple[str, ...]] = {}
        for group in ("weights", "tokenizer", "preprocessing"):
            paths = entry.get(group)
            if not isinstance(paths, list) or any(
                not isinstance(path, str) for path in paths
            ):
                raise PrismModelServiceError(
                    f"{label}.{group} must be an array of strings"
                )
            groups[group] = tuple(paths)
        deployment = ModelDeployment(
            registered=registered,
            model_root=root,
            weights=groups["weights"],
            tokenizer=groups["tokenizer"],
            preprocessing=groups["preprocessing"],
            backend_url=_loopback_backend(
                entry.get("backend_url"), f"{label}.backend_url"
            ),
        )
        if key in result:
            raise PrismModelServiceError(f"duplicate deployed model {key[0]}:{key[1]}")
        result[key] = deployment
    if set(result) != set(registry):
        raise PrismModelServiceError(
            "model-service deployment set differs from registry"
        )
    return GatewayConfig(
        socket_path=socket_path,
        socket_mode=mode,
        allowed_peer_uids=uids,
        request_timeout_seconds=timeout_ms / 1000.0,
        max_concurrency=concurrency,
        models=result,
    )


class TeiBackend:
    """Dependency-free adapter to a loopback-only TEI process."""

    def __init__(self, base_url: str, timeout_seconds: float):
        self._embed_url = f"{base_url}/embed"
        self._health_url = f"{base_url}/health"
        self._timeout = timeout_seconds
        self._opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def health(self) -> None:
        request = urllib.request.Request(self._health_url, method="GET")
        self._read(request, 64 * 1024)

    def embed(self, texts: list[str]) -> list[list[float]]:
        payload = json.dumps(
            {"inputs": texts, "truncate": True}, separators=(",", ":")
        ).encode("utf-8")
        request = urllib.request.Request(
            self._embed_url,
            data=payload,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        raw = self._read(request, MAX_RESPONSE_BYTES)
        try:
            value = _strict_json(raw)
        except json.JSONDecodeError as error:
            raise PrismModelServiceError(
                f"TEI returned invalid JSON: {error}"
            ) from error
        if not isinstance(value, list) or any(
            not isinstance(vector, list) for vector in value
        ):
            raise PrismModelServiceError("TEI response must be an array of vectors")
        return value

    def _read(self, request: urllib.request.Request, limit: int) -> bytes:
        try:
            with self._opener.open(request, timeout=self._timeout) as response:
                value = response.read(limit + 1)
        except (urllib.error.URLError, TimeoutError, OSError) as error:
            raise PrismModelServiceError(f"TEI backend unavailable: {error}") from error
        if len(value) > limit:
            raise PrismModelServiceError(f"TEI response exceeds {limit} bytes")
        return value


class Gateway:
    def __init__(
        self,
        config: GatewayConfig,
        backends: dict[tuple[str, str], Any] | None = None,
    ):
        self.config = config
        self.backends = (
            {
                key: TeiBackend(deployment.backend_url, config.request_timeout_seconds)
                for key, deployment in config.models.items()
            }
            if backends is None
            else backends
        )
        if set(self.backends) != set(config.models):
            raise PrismModelServiceError("backend set differs from deployment set")
        self._stopping = threading.Event()
        self._socket: socket.socket | None = None
        self._executor: ThreadPoolExecutor | None = None
        self._slots = threading.BoundedSemaphore(config.max_concurrency)

    def verify_and_warm(self) -> None:
        for key, deployment in self.config.models.items():
            registered = deployment.registered
            _validate_preprocessing(deployment)
            actual = {
                "model_sha256": artifact_digest(
                    deployment.model_root, list(deployment.weights)
                ),
                "tokenizer_sha256": artifact_digest(
                    deployment.model_root, list(deployment.tokenizer)
                ),
                "preprocessing_sha256": artifact_digest(
                    deployment.model_root, list(deployment.preprocessing)
                ),
            }
            if actual != registered.artifacts:
                mismatches = [
                    name
                    for name in REVISION_KEYS
                    if actual[name] != registered.artifacts[name]
                ]
                raise PrismModelServiceError(
                    f"artifact verification failed for {key[0]}:{key[1]}: "
                    f"{', '.join(mismatches)}"
                )
            backend = self.backends[key]
            backend.health()
            vectors = backend.embed(["PrismDB model-service startup warmup"])
            self._validated_vectors(registered, vectors, 1)

    def serve_forever(self) -> None:
        self.verify_and_warm()
        path = self.config.socket_path
        path.parent.mkdir(mode=0o750, parents=True, exist_ok=True)
        if path.exists() or path.is_symlink():
            if not path.is_socket():
                raise PrismModelServiceError(
                    f"refusing to replace non-socket path {path}"
                )
            path.unlink()
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            listener.bind(str(path))
            os.chmod(path, self.config.socket_mode)
            listener.listen(self.config.max_concurrency)
            listener.settimeout(0.25)
            self._socket = listener
            self._executor = ThreadPoolExecutor(
                max_workers=self.config.max_concurrency,
                thread_name_prefix="prism-model",
            )
            _log("ready", models=len(self.config.models), socket=str(path))
            while not self._stopping.is_set():
                self._slots.acquire()
                try:
                    connection, _ = listener.accept()
                except TimeoutError:
                    self._slots.release()
                    continue
                except OSError:
                    self._slots.release()
                    if self._stopping.is_set():
                        break
                    raise
                self._executor.submit(self._serve_connection, connection)
        finally:
            self.stop()
            if self._executor is not None:
                self._executor.shutdown(wait=True, cancel_futures=True)
            if path.is_socket():
                path.unlink()

    def stop(self) -> None:
        self._stopping.set()
        if self._socket is not None:
            try:
                self._socket.close()
            except OSError:
                pass

    def _serve_connection(self, connection: socket.socket) -> None:
        started = time.monotonic()
        outcome = "error"
        try:
            connection.settimeout(self.config.request_timeout_seconds)
            self._check_peer(connection)
            raw = _read_line(connection, MAX_REQUEST_BYTES)
            request = _strict_json(raw)
            response = self.handle(request)
            encoded = json.dumps(
                response, separators=(",", ":"), allow_nan=False
            ).encode("utf-8")
            if len(encoded) + 1 > MAX_RESPONSE_BYTES:
                raise PrismModelServiceError(
                    f"encoded model response exceeds {MAX_RESPONSE_BYTES} bytes"
                )
            connection.sendall(encoded + b"\n")
            outcome = "ok"
        except (
            PrismModelServiceError,
            json.JSONDecodeError,
            OSError,
            TimeoutError,
            ValueError,
        ) as error:
            _log("request_refused", error=str(error))
        finally:
            try:
                connection.close()
            finally:
                self._slots.release()
                _log(
                    "request_complete",
                    outcome=outcome,
                    duration_ms=round((time.monotonic() - started) * 1000, 3),
                )

    def _check_peer(self, connection: socket.socket) -> None:
        if not hasattr(socket, "SO_PEERCRED"):
            raise PrismModelServiceError(
                "peer credentials are unavailable on this platform; production service is Linux-only"
            )
        credentials = connection.getsockopt(
            socket.SOL_SOCKET, socket.SO_PEERCRED, struct.calcsize("3i")
        )
        _, uid, _ = struct.unpack("3i", credentials)
        if uid not in self.config.allowed_peer_uids:
            raise PrismModelServiceError(f"peer UID {uid} is not authorized")

    def handle(self, request: Any) -> dict[str, Any]:
        if not isinstance(request, dict):
            raise PrismModelServiceError("request must be a JSON object")
        operation = request.get("operation", "infer")
        if operation == "health":
            _only_keys(request, {"protocol_version", "operation"}, "health request")
            if request.get("protocol_version") != PROTOCOL_VERSION:
                raise PrismModelServiceError("unsupported health protocol version")
            return {
                "protocol_version": PROTOCOL_VERSION,
                "status": "ok",
                "loaded_models": len(self.config.models),
            }
        if operation != "infer":
            raise PrismModelServiceError("unsupported operation")
        _only_keys(
            request,
            {
                "protocol_version",
                "model_id",
                "model_version",
                "artifacts",
                "texts",
                "operation",
            },
            "inference request",
        )
        if request.get("protocol_version") != PROTOCOL_VERSION:
            raise PrismModelServiceError("unsupported inference protocol version")
        key = (request.get("model_id"), request.get("model_version"))
        deployment = self.config.models.get(key)
        if deployment is None:
            raise PrismModelServiceError("requested model identity is not deployed")
        registered = deployment.registered
        if request.get("artifacts") != registered.artifacts:
            raise PrismModelServiceError(
                "request artifact identity differs from registry"
            )
        texts = request.get("texts")
        if not isinstance(texts, list) or not 1 <= len(texts) <= MAX_BATCH_ITEMS:
            raise PrismModelServiceError(
                f"texts must contain 1..={MAX_BATCH_ITEMS} strings"
            )
        total = 0
        for text in texts:
            if not isinstance(text, str):
                raise PrismModelServiceError("every input must be a string")
            size = len(text.encode("utf-8"))
            if size > MAX_TEXT_BYTES:
                raise PrismModelServiceError(
                    f"individual input exceeds {MAX_TEXT_BYTES} UTF-8 bytes"
                )
            total += size
        if total > MAX_BATCH_INPUT_BYTES:
            raise PrismModelServiceError(
                f"batch input exceeds {MAX_BATCH_INPUT_BYTES} UTF-8 bytes"
            )
        try:
            raw_vectors = self.backends[key].embed(texts)
            vectors = self._validated_vectors(registered, raw_vectors, len(texts))
            outputs = [{"status": "ok", "vector": vector} for vector in vectors]
        except PrismModelServiceError as error:
            outputs = [{"status": "error", "error": str(error)} for _ in texts]
        return {
            "protocol_version": PROTOCOL_VERSION,
            "model_id": registered.model_id,
            "model_version": registered.model_version,
            "artifacts": registered.artifacts,
            "outputs": outputs,
        }

    @staticmethod
    def _validated_vectors(
        model: RegisteredModel, raw_vectors: Any, expected: int
    ) -> list[list[float]]:
        if not isinstance(raw_vectors, list) or len(raw_vectors) != expected:
            raise PrismModelServiceError(
                f"TEI returned {len(raw_vectors) if isinstance(raw_vectors, list) else 'invalid'} "
                f"vectors for {expected} inputs"
            )
        result: list[list[float]] = []
        for raw in raw_vectors:
            if not isinstance(raw, list) or len(raw) != model.dim:
                raise PrismModelServiceError(
                    f"TEI vector dimension differs from registered {model.dim}"
                )
            vector: list[float] = []
            norm_sq = 0.0
            for value in raw:
                if isinstance(value, bool) or not isinstance(value, (int, float)):
                    raise PrismModelServiceError("TEI vector contains a non-number")
                number = float(value)
                if not math.isfinite(number):
                    raise PrismModelServiceError(
                        "TEI vector contains a non-finite value"
                    )
                vector.append(number)
                norm_sq += number * number
            norm = math.sqrt(norm_sq)
            if not math.isfinite(norm) or norm <= 1e-12:
                raise PrismModelServiceError("TEI vector has an invalid norm")
            result.append([value / norm for value in vector])
        return result


def _validate_preprocessing(deployment: ModelDeployment) -> None:
    if len(deployment.preprocessing) != 1:
        raise PrismModelServiceError(
            "preprocessing artifact set must contain exactly one pipeline manifest"
        )
    relative = _relative_artifact_path(deployment.preprocessing[0])
    path = deployment.model_root.joinpath(*relative.parts)
    raw = _json_object(path, 64 * 1024, "preprocessing pipeline")
    _only_keys(
        raw,
        {
            "format_version",
            "backend",
            "truncate",
            "normalize",
            "max_input_bytes",
            "prompt",
        },
        "preprocessing pipeline",
    )
    expected = {
        "format_version": 1,
        "backend": "text-embeddings-inference",
        "truncate": True,
        "normalize": True,
        "max_input_bytes": MAX_TEXT_BYTES,
        "prompt": None,
    }
    if raw != expected:
        raise PrismModelServiceError(
            "preprocessing pipeline must exactly match the supported fail-closed "
            f"contract: {json.dumps(expected, separators=(',', ':'))}"
        )


def _read_line(connection: socket.socket, limit: int) -> bytes:
    value = bytearray()
    while len(value) <= limit:
        chunk = connection.recv(min(64 * 1024, limit + 1 - len(value)))
        if not chunk:
            break
        newline = chunk.find(b"\n")
        if newline >= 0:
            value.extend(chunk[:newline])
            break
        value.extend(chunk)
    if len(value) > limit:
        raise PrismModelServiceError(f"encoded model request exceeds {limit} bytes")
    if not value:
        raise PrismModelServiceError("connection closed without a request")
    return bytes(value)


def _log(event: str, **fields: Any) -> None:
    record = {"event": event, **fields}
    print(json.dumps(record, separators=(",", ":"), sort_keys=True), flush=True)


def healthcheck(socket_path: Path, timeout_seconds: float = 2.0) -> None:
    request = json.dumps(
        {"protocol_version": PROTOCOL_VERSION, "operation": "health"},
        separators=(",", ":"),
    ).encode("utf-8")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(timeout_seconds)
        connection.connect(str(socket_path))
        connection.sendall(request + b"\n")
        connection.shutdown(socket.SHUT_WR)
        response = _strict_json(_read_line(connection, 64 * 1024))
    if (
        response.get("protocol_version") != PROTOCOL_VERSION
        or response.get("status") != "ok"
    ):
        raise PrismModelServiceError("model-service health response is not ready")
