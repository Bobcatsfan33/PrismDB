ARG RUST_IMAGE=rust:1.75.0-bookworm@sha256:87f3b2f93b82995443a1a558c234212dafe79cfdc3af956539610560369ddcd0
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:fccdbb0a547c14e23fcf4ce8ad62ca5d43b4faae8d22cd292f490fef9946c96e

FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --locked --release -p prism-service \
    && mkdir /runtime-libs \
    && cp /usr/lib/*/libssl.so.3 /usr/lib/*/libcrypto.so.3 /runtime-libs/

FROM ${RUNTIME_IMAGE}
ARG RUST_IMAGE
ARG RUNTIME_IMAGE
LABEL org.opencontainers.image.base.name="${RUNTIME_IMAGE}" \
      org.opencontainers.image.build.base.name="${RUST_IMAGE}" \
      org.opencontainers.image.source="https://github.com/Bobcatsfan33/PrismDB"
COPY --from=build --chown=65532:65532 /src/target/release/prismd /usr/local/bin/prismd
COPY --from=build /runtime-libs/ /usr/local/lib/
ENV LD_LIBRARY_PATH=/usr/local/lib
USER 65532:65532
EXPOSE 8443
ENTRYPOINT ["/usr/local/bin/prismd"]
CMD ["version"]
