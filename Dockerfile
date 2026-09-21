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

RUN case "${TARGETARCH}" in \
      amd64) ORT_ARCH="x64" ;; \
      arm64) ORT_ARCH="aarch64" ;; \
      *) echo "Unsupported target architecture for onnxruntime: ${TARGETARCH}"; exit 1 ;; \
    esac && \
    mkdir -p /opt/onnxruntime && \
    curl -fsSL "https://github.com/microsoft/onnxruntime/releases/download/v${ONNXRUNTIME_VERSION}/onnxruntime-linux-${ORT_ARCH}-${ONNXRUNTIME_VERSION}.tgz" | \
    tar -xz --strip-components=2 -C /opt/onnxruntime "*/lib/libonnxruntime*.so*"

FROM gcr.io/distroless/cc:nonroot AS runtime

ARG TARGETARCH

# Bundled ONNX Runtime libraries for in-process inference
COPY --from=onnx-fetcher /opt/onnxruntime/ /usr/local/lib/

# Copy the pre-built binary for target architecture and example config.
COPY --chown=65532:65532 bin/${TARGETARCH}/openproxy /usr/local/bin/openproxy
COPY --chown=65532:65532 config.example.toml /etc/openproxy/config.example.toml

USER 65532:65532
WORKDIR /var/lib/openproxy

# openproxy binds 127.0.0.1:8787 by default (see config.example.toml).
# When running in Docker the operator overrides `server.bind` to `0.0.0.0:8787`.
EXPOSE 8787

# Persistent state: SQLite database, encryption key file, models, etc.
VOLUME ["/var/lib/openproxy"]

ENTRYPOINT ["/usr/local/bin/openproxy"]
CMD ["--config", "/etc/openproxy/config.toml"]
