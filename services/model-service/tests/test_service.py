from __future__ import annotations

import json
import os
import socket
import tempfile
import threading
import time
import unittest
from pathlib import Path

from prism_model_service.service import (
    Gateway,
    PrismModelServiceError,
    artifact_digest,
    artifact_revision,
    healthcheck,
    load_gateway_config,
)


class FakeBackend:
    def __init__(self, vectors: list[list[float]] | None = None):
        self.vectors = vectors
        self.health_calls = 0
        self.embed_calls: list[list[str]] = []

    def health(self) -> None:
        self.health_calls += 1

    def embed(self, texts: list[str]) -> list[list[float]]:
        self.embed_calls.append(texts)
        if self.vectors is not None:
            return self.vectors
        return [[3.0, 4.0] for _ in texts]


class ModelServiceTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.model_root = self.root / "model"
        self.model_root.mkdir()
        (self.model_root / "model.safetensors").write_bytes(b"approved weights")
        (self.model_root / "tokenizer.json").write_text(
            '{"tokenizer":"approved"}\n', encoding="utf-8"
        )
        (self.model_root / "preprocessing.json").write_text(
            json.dumps(
                {
                    "format_version": 1,
                    "backend": "text-embeddings-inference",
                    "truncate": True,
                    "normalize": True,
                    "max_input_bytes": 32768,
                    "prompt": None,
                }
            ),
            encoding="utf-8",
        )
        self.artifacts = {
            "model_sha256": artifact_digest(self.model_root, ["model.safetensors"]),
            "tokenizer_sha256": artifact_digest(self.model_root, ["tokenizer.json"]),
            "preprocessing_sha256": artifact_digest(
                self.model_root, ["preprocessing.json"]
            ),
        }
        self.version = artifact_revision(self.artifacts)
        self.registry_path = self.root / "registry.json"
        self.registry_path.write_text(
            json.dumps(
                {
                    "default_model_id": "approved",
                    "default_model_version": self.version,
                    "models": [
                        {
                            "model_id": "approved",
                            "model_version": self.version,
                            "dim": 2,
                            "artifacts": self.artifacts,
                        }
                    ],
                }
            ),
            encoding="utf-8",
        )
        self.socket_path = self.root / "run" / "model.sock"
        self.config_path = self.root / "service.json"
        self.write_config()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_config(self, **overrides: object) -> None:
        config: dict[str, object] = {
            "registry_path": str(self.registry_path),
            "socket_path": str(self.socket_path),
            "socket_mode": "0660",
            "allowed_peer_uids": [os.getuid()],
            "request_timeout_ms": 2_000,
            "max_concurrency": 2,
            "models": [
                {
                    "model_id": "approved",
                    "model_version": self.version,
                    "model_root": str(self.model_root),
                    "weights": ["model.safetensors"],
                    "tokenizer": ["tokenizer.json"],
                    "preprocessing": ["preprocessing.json"],
                    "backend_url": "http://127.0.0.1:8080",
                }
            ],
        }
        config.update(overrides)
        self.config_path.write_text(json.dumps(config), encoding="utf-8")

    def request(self, texts: list[str]) -> dict[str, object]:
        return {
            "protocol_version": 1,
            "model_id": "approved",
            "model_version": self.version,
            "artifacts": self.artifacts,
            "texts": texts,
        }

    def test_revision_matches_the_rust_contract_example(self) -> None:
        artifacts = {
            "model_sha256": "a" * 64,
            "tokenizer_sha256": "b" * 64,
            "preprocessing_sha256": "c" * 64,
        }
        self.assertEqual(
            artifact_revision(artifacts),
            "b5f4f9bcd7631a0cbb72e9a434602fd651edef027ac0dd595d36768ee01b7483",
        )

    def test_artifact_digest_is_order_independent_and_rejects_symlinks(self) -> None:
        (self.model_root / "other.bin").write_bytes(b"other")
        first = artifact_digest(self.model_root, ["other.bin", "model.safetensors"])
        second = artifact_digest(self.model_root, ["model.safetensors", "other.bin"])
        self.assertEqual(first, second)
        (self.model_root / "linked.bin").symlink_to("model.safetensors")
        with self.assertRaisesRegex(PrismModelServiceError, "symlink"):
            artifact_digest(self.model_root, ["linked.bin"])

    def test_config_refuses_a_non_loopback_backend(self) -> None:
        models = json.loads(self.config_path.read_text(encoding="utf-8"))["models"]
        models[0]["backend_url"] = "https://inference.example.com:443"
        self.write_config(models=models)
        with self.assertRaisesRegex(PrismModelServiceError, "loopback-only"):
            load_gateway_config(self.config_path)

    def test_preprocessing_contract_is_exact_and_content_addressed(self) -> None:
        config = load_gateway_config(self.config_path)
        gateway = Gateway(config, {("approved", self.version): FakeBackend()})
        (self.model_root / "preprocessing.json").write_text(
            '{"format_version":1,"backend":"text-embeddings-inference",'
            '"truncate":true,"normalize":true,"max_input_bytes":32768,'
            '"prompt":"query: "}',
            encoding="utf-8",
        )
        with self.assertRaisesRegex(PrismModelServiceError, "exactly match"):
            gateway.verify_and_warm()

    def test_startup_refuses_changed_artifact_bytes(self) -> None:
        config = load_gateway_config(self.config_path)
        gateway = Gateway(config, {("approved", self.version): FakeBackend()})
        (self.model_root / "model.safetensors").write_bytes(b"changed weights")
        with self.assertRaisesRegex(PrismModelServiceError, "model_sha256"):
            gateway.verify_and_warm()

    def test_warmup_checks_backend_and_vector_shape(self) -> None:
        config = load_gateway_config(self.config_path)
        backend = FakeBackend()
        Gateway(config, {("approved", self.version): backend}).verify_and_warm()
        self.assertEqual(backend.health_calls, 1)
        self.assertEqual(len(backend.embed_calls), 1)

        wrong = FakeBackend([[1.0, 0.0, 0.0]])
        with self.assertRaisesRegex(PrismModelServiceError, "dimension"):
            Gateway(config, {("approved", self.version): wrong}).verify_and_warm()

    def test_backend_failure_preserves_cardinality_and_fails_items_closed(self) -> None:
        config = load_gateway_config(self.config_path)
        wrong = FakeBackend([[float("nan"), 0.0], [1.0, 0.0]])
        gateway = Gateway(config, {("approved", self.version): wrong})
        response = gateway.handle(self.request(["one", "two"]))
        outputs = response["outputs"]
        self.assertEqual(len(outputs), 2)
        self.assertTrue(all(item["status"] == "error" for item in outputs))

    @unittest.skipUnless(
        hasattr(socket, "SO_PEERCRED"), "Linux peer credentials required"
    )
    def test_unix_service_round_trip_and_health(self) -> None:
        config = load_gateway_config(self.config_path)
        backend = FakeBackend()
        gateway = Gateway(config, {("approved", self.version): backend})
        thread = threading.Thread(target=gateway.serve_forever, daemon=True)
        thread.start()
        deadline = time.monotonic() + 5
        while not self.socket_path.exists() and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertTrue(self.socket_path.exists())
        healthcheck(self.socket_path)

        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.connect(str(self.socket_path))
            connection.sendall(
                json.dumps(self.request(["one", "two"])).encode("utf-8") + b"\n"
            )
            connection.shutdown(socket.SHUT_WR)
            response = json.loads(connection.makefile("rb").readline())
        self.assertEqual(response["model_version"], self.version)
        self.assertEqual(len(response["outputs"]), 2)
        self.assertEqual(response["outputs"][0]["vector"], [0.6, 0.8])

        gateway.stop()
        thread.join(timeout=5)
        self.assertFalse(thread.is_alive())


if __name__ == "__main__":
    unittest.main()
