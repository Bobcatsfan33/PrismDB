ARG PYTHON_IMAGE=python:3.12.11-slim-bookworm
FROM ${PYTHON_IMAGE}

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
