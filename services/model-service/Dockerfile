ARG PYTHON_IMAGE=python:3.12.13-slim-bookworm@sha256:d50fb7611f86d04a3b0471b46d7557818d88983fc3136726336b2a4c657aa30b
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
