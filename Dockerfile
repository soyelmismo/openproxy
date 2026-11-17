# syntax=docker/dockerfile:1.7
#
# Multi-arch Dockerfile for openproxy with bundled ONNX Runtime for native in-process inference.
#
# Expects pre-built binaries at:
#   bin/amd64/openproxy
#   bin/arm64/openproxy
#
# Usage (CI / pre-built):
#   docker buildx build --platform linux/amd64,linux/arm64 -t openproxy .
#

# Security (OP-09): base image pinned by digest (alpine:latest at 2026-09-30).
# Bump deliberately after reviewing the new image, not by accident on a rebuild.
FROM --platform=$BUILDPLATFORM alpine@sha256:294b683cb724975bec92580e1e685676bd4b50bda910ddb8c51d4cabeaec77e6 AS onnx-fetcher

ARG TARGETARCH
ARG ONNXRUNTIME_VERSION=1.20.1
# Security (OP-09): expected SHA-256 of the Microsoft.ML.OnnxRuntime 1.20.1
# NuGet package, which bundles libonnxruntime.so for linux-x64 AND linux-arm64.
# Source: https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/1.20.1/
# The upstream GitHub release tarballs for this version were removed from the
# microsoft/onnxruntime repo, so the NuGet package is the pinned artifact now.
# The build FAILS if the downloaded artifact does not match — a compromised
# artifact or a MITM'd build can no longer inject a trojanized libonnxruntime
# (which the gateway dlopens into its own process).
ARG ONNXRUNTIME_NUPKG_SHA256=9c32108e9a1c78b090a95c2a219f244dd5f067d4caf68fb0f967103f3f20839c

RUN apk add --no-cache curl unzip

# Security (OP-04): ship a real /etc/openproxy/config.toml. Previously the
# CMD pointed at a file that did not exist in the image, so containers fell
# back to the binary defaults. The container default binds 0.0.0.0 (required
# for the port mapping to be reachable); the HOST-side exposure is now
# loopback-only via docker-compose.yml, and operators who need external
# access are expected to front the port with TLS.
RUN mkdir -p /container-config && printf '[server]\n# Container default: must bind all interfaces for the port mapping to work.\nbind = "0.0.0.0:8787"\nrequest_max_body_bytes = 10485760\n\n[storage]\ndatabase_path = "/var/lib/openproxy/data.db"\nencryption_key_source = "env"\n' > /container-config/config.toml

RUN case "${TARGETARCH}" in \
      amd64) ORT_ARCH="x64" ;; \
      arm64) ORT_ARCH="arm64" ;; \
      *) echo "Unsupported target architecture for onnxruntime: ${TARGETARCH}"; exit 1 ;; \
    esac && \
    mkdir -p /opt/onnxruntime /tmp/ort && \
    curl -fsSL "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/${ONNXRUNTIME_VERSION}/microsoft.ml.onnxruntime.${ONNXRUNTIME_VERSION}.nupkg" -o /tmp/ort/ort.nupkg && \
    echo "${ONNXRUNTIME_NUPKG_SHA256}  /tmp/ort/ort.nupkg" | sha256sum -c - && \
    unzip -q /tmp/ort/ort.nupkg "runtimes/linux-${ORT_ARCH}/native/libonnxruntime*.so*" -d /tmp/ort && \
    cp /tmp/ort/runtimes/linux-${ORT_ARCH}/native/libonnxruntime*.so* /opt/onnxruntime/

# Security (OP-09, audit H-9): pin the runtime base by digest. The
# `nonroot` tag is mutable — a rebuilt upstream tag silently changes the
# runtime image every CI run. Digest verified against the registry
# (multi-arch index: amd64, arm/v7, arm64/v8, ppc64le, riscv64, s390x) on
# 2026-11-15. Bump deliberately after reviewing the new image, like the
# fetcher stage above.
FROM gcr.io/distroless/cc@sha256:cb87acc574abe719b6e5c547384defa2f7b1f24af9605d3f618cf852495d8be2 AS runtime

ARG TARGETARCH

# Bundled ONNX Runtime libraries for in-process inference
COPY --from=onnx-fetcher /opt/onnxruntime/ /usr/local/lib/

# Copy the pre-built binary for target architecture, the container config
# and the annotated example config for reference.
COPY --chown=65532:65532 bin/${TARGETARCH}/openproxy /usr/local/bin/openproxy
COPY --chown=65532:65532 --from=onnx-fetcher /container-config/config.toml /etc/openproxy/config.toml
COPY --chown=65532:65532 config.example.toml /etc/openproxy/config.example.toml

USER 65532:65532
WORKDIR /var/lib/openproxy

# The binary itself defaults to 127.0.0.1:8787 (OP-04); this image ships
# /etc/openproxy/config.toml with bind = "0.0.0.0:8787" so the published port
# is reachable inside the container. Host-side exposure is loopback-only in
# docker-compose.yml — put a TLS-terminating reverse proxy in front to expose.
EXPOSE 8787

# Persistent state: SQLite database, encryption key file, models, etc.
VOLUME ["/var/lib/openproxy"]

# Security (OP-23): real HEALTHCHECK. Distroless has no shell/wget/curl, so
# the binary checks itself: `openproxy --healthcheck` TCP-connects to the
# configured bind address and exits 0/1. The same --config flag as the CMD
# so the check targets the address the server actually binds.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/openproxy", "--healthcheck", "--config", "/etc/openproxy/config.toml"]

# Security/ops (audit H-20 + this audit): the binary reads OPENPROXY_CONFIG
# and, since the --config parsing fix, honors the CMD's --config flag as
# well (previously the flag was silently discarded, so a plain `docker run`
# fell back to ./config.toml defaults: loopback bind inside the container
# and a master-key crash loop). The ENV makes the config explicit for every
# invocation style; the CMD keeps the flag for operators who override it.
ENV OPENPROXY_CONFIG=/etc/openproxy/config.toml
ENTRYPOINT ["/usr/local/bin/openproxy"]
