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

FROM --platform=$BUILDPLATFORM alpine:latest AS onnx-fetcher

ARG TARGETARCH
ARG ONNXRUNTIME_VERSION=1.20.1

RUN apk add --no-cache curl tar

# Security (OP-04): ship a real /etc/openproxy/config.toml. Previously the
# CMD pointed at a file that did not exist in the image, so containers fell
# back to the binary defaults. The container default binds 0.0.0.0 (required
# for the port mapping to be reachable); the HOST-side exposure is now
# loopback-only via docker-compose.yml, and operators who need external
# access are expected to front the port with TLS.
RUN mkdir -p /container-config && printf '[server]\n# Container default: must bind all interfaces for the port mapping to work.\nbind = "0.0.0.0:8787"\nrequest_max_body_bytes = 10485760\n\n[storage]\ndatabase_path = "/var/lib/openproxy/data.db"\nencryption_key_source = "env"\n' > /container-config/config.toml

RUN case "${TARGETARCH}" in \
      amd64) ORT_ARCH="x64" ;; \
      arm64) ORT_ARCH="aarch64" ;; \
      *) echo "Unsupported target architecture for onnxruntime: ${TARGETARCH}"; exit 1 ;; \
    esac && \
    mkdir -p /opt/onnxruntime && \
    curl -fsSL "https://github.com/microsoft/onnxruntime/releases/download/v${ONNXRUNTIME_VERSION}/onnxruntime-linux-${ORT_ARCH}-${ONNXRUNTIME_VERSION}.tgz" | \
    tar -xz --wildcards --strip-components=2 -C /opt/onnxruntime "*/lib/libonnxruntime*.so*"

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

ENTRYPOINT ["/usr/local/bin/openproxy"]
CMD ["--config", "/etc/openproxy/config.toml"]
