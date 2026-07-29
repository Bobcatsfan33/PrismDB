from __future__ import annotations

import argparse
import json
import signal
import sys
from pathlib import Path

from .service import (
    Gateway,
    PrismModelServiceError,
    artifact_digest,
    artifact_revision,
    healthcheck,
    load_gateway_config,
)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="prism-model-service")
    commands = parser.add_subparsers(dest="command", required=True)
    serve = commands.add_parser(
        "serve", help="verify artifacts, warm models, and serve"
    )
    serve.add_argument("--config", type=Path, required=True)
    health = commands.add_parser("health", help="probe a running Unix-socket gateway")
    health.add_argument("--socket", type=Path, required=True)
    health.add_argument("--timeout-seconds", type=float, default=2.0)
    digest = commands.add_parser("digest", help="content-address an artifact file set")
    digest.add_argument("--root", type=Path, required=True)
    digest.add_argument("--weights", nargs="+", required=True)
    digest.add_argument("--tokenizer", nargs="+", required=True)
    digest.add_argument("--preprocessing", nargs="+", required=True)
    return parser


def main() -> int:
    args = _parser().parse_args()
    try:
        if args.command == "health":
            healthcheck(args.socket, args.timeout_seconds)
            return 0
        if args.command == "digest":
            artifacts = {
                "model_sha256": artifact_digest(args.root, args.weights),
                "tokenizer_sha256": artifact_digest(args.root, args.tokenizer),
                "preprocessing_sha256": artifact_digest(args.root, args.preprocessing),
            }
            print(
                json.dumps(
                    {**artifacts, "model_version": artifact_revision(artifacts)},
                    indent=2,
                    sort_keys=True,
                )
            )
            return 0
        gateway = Gateway(load_gateway_config(args.config))
        signal.signal(signal.SIGTERM, lambda _signum, _frame: gateway.stop())
        signal.signal(signal.SIGINT, lambda _signum, _frame: gateway.stop())
        gateway.serve_forever()
        return 0
    except (PrismModelServiceError, OSError, ValueError) as error:
        print(f"prism-model-service: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
