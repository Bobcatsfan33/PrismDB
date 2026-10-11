ARG PYTHON_IMAGE=python:3.12.15-slim-bookworm@sha256:34386ef0cb081344d7ec1c103ba398e6e9f64e9ab3a1509accc92a4e24a07258
FROM ${PYTHON_IMAGE}

ARG PYTHON_IMAGE
LABEL org.opencontainers.image.base.name="${PYTHON_IMAGE}"

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PYTHONPATH=/opt/prism-model-service

RUN groupadd --gid 65532 prism-model \
    && useradd --uid 65532 --gid 65532 --no-create-home --shell /usr/sbin/nologin prism-model

WORKDIR /opt/prism-model-service
COPY --chown=root:root prism_model_service ./prism_model_service

USER 65532:65532
ENTRYPOINT ["python", "-m", "prism_model_service"]
CMD ["serve", "--config", "/etc/prism/model-service.json"]
