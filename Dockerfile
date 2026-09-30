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
# Security (OP-09): expected SHA-256 of the onnxruntime release tarballs.
# The build FAILS if the downloaded artifact does not match — a compromised
# release or a MITM'd build can no longer inject a trojanized libonnxruntime
# (which the gateway dlopens into its own process).
ARG ONNXRUNTIME_SHA256_X64=67db4dc1561f1e3fd42e619575c82c601ef89849afc7ea85a003abbac1a1a105
ARG ONNXRUNTIME_SHA256_AARCH64=ae4fedbdc8c18d688c01306b4b50c63de3445cdf2dbd720e01a2fa3810b8106a

RUN apk add --no-cache curl tar

# Security (OP-04): ship a real /etc/openproxy/config.toml. Previously the
# CMD pointed at a file that did not exist in the image, so containers fell
# back to the binary defaults. The container default binds 0.0.0.0 (required
# for the port mapping to be reachable); the HOST-side exposure is now
# loopback-only via docker-compose.yml, and operators who need external
# access are expected to front the port with TLS.
RUN mkdir -p /container-config && printf '[server]\n# Container default: must bind all interfaces for the port mapping to work.\nbind = "0.0.0.0:8787"\nrequest_max_body_bytes = 10485760\n\n[storage]\ndatabase_path = "/var/lib/openproxy/data.db"\nencryption_key_source = "env"\n' > /container-config/config.toml

RUN case "${TARGETARCH}" in \
      amd64) ORT_ARCH="x64"; ORT_SHA256="${ONNXRUNTIME_SHA256_X64}" ;; \
      arm64) ORT_ARCH="aarch64"; ORT_SHA256="${ONNXRUNTIME_SHA256_AARCH64}" ;; \
      *) echo "Unsupported target architecture for onnxruntime: ${TARGETARCH}"; exit 1 ;; \
    esac && \
    mkdir -p /opt/onnxruntime /tmp/ort && \
    curl -fsSL "https://github.com/microsoft/onnxruntime/releases/download/v${ONNXRUNTIME_VERSION}/onnxruntime-linux-${ORT_ARCH}-${ONNXRUNTIME_VERSION}.tgz" -o /tmp/ort/ort.tgz && \
    echo "${ORT_SHA256}  /tmp/ort/ort.tgz" | sha256sum -c - && \
    tar -xzf /tmp/ort/ort.tgz --wildcards --strip-components=2 -C /opt/onnxruntime "*/lib/libonnxruntime*.so*"

FROM gcr.io/distroless/cc:nonroot AS runtime

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
# configured bind address and exits 0/1.
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD ["/usr/local/bin/openproxy", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/openproxy"]
CMD ["--config", "/etc/openproxy/config.toml"]
