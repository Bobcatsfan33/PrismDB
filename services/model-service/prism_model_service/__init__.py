"""PrismDB's fail-closed identity gateway for embedding inference."""

from .service import (
    Gateway,
    GatewayConfig,
    ModelDeployment,
    PrismModelServiceError,
    artifact_digest,
    artifact_revision,
    load_gateway_config,
)

__all__ = [
    "Gateway",
    "GatewayConfig",
    "ModelDeployment",
    "PrismModelServiceError",
    "artifact_digest",
    "artifact_revision",
    "load_gateway_config",
]
