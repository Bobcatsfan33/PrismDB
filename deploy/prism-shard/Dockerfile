ARG RUST_IMAGE=rust:1.75.0-bookworm@sha256:87f3b2f93b82995443a1a558c234212dafe79cfdc3af956539610560369ddcd0
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:fccdbb0a547c14e23fcf4ce8ad62ca5d43b4faae8d22cd292f490fef9946c96e

FROM ${RUST_IMAGE} AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# The shard node is exactly the `prism` binary that carries `shard-serve`; there is no separate
# server built from a different source path, so the image cannot drift from the tested command.
RUN cargo build --locked --release -p prism-cli \
    && mkdir /runtime-libs \
    && cp /usr/lib/*/libssl.so.3 /usr/lib/*/libcrypto.so.3 /runtime-libs/

FROM ${RUNTIME_IMAGE}
ARG RUST_IMAGE
ARG RUNTIME_IMAGE
LABEL org.opencontainers.image.base.name="${RUNTIME_IMAGE}" \
      org.opencontainers.image.build.base.name="${RUST_IMAGE}" \
      org.opencontainers.image.title="prism-shard" \
      org.opencontainers.image.source="https://github.com/Bobcatsfan33/PrismDB"
COPY --from=build --chown=65532:65532 /src/target/release/prism /usr/local/bin/prism
COPY --from=build /runtime-libs/ /usr/local/lib/
ENV LD_LIBRARY_PATH=/usr/local/lib
USER 65532:65532
EXPOSE 7443
ENTRYPOINT ["/usr/local/bin/prism"]
CMD ["--version"]
